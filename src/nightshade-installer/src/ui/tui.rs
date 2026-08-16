//! Full-screen frontend.
//!
//! Keyboard only — there is no mouse handling anywhere, because this has to
//! work on a bare TTY. If it cannot initialise (no terminal, a TERM the
//! library cannot drive, a serial line), `TuiFrontend::new` fails and main
//! falls back to the plain frontend running the very same engine.
//!
//! Everything is drawn with Paragraph, Block, Gauge and Layout. Stateful
//! widgets are deliberately avoided: the disk picker renders its own rows so
//! that multi-selection, the live-medium exclusion note and the per-disk
//! signature lines are all under direct control.

use std::io::{Stdout, stdout};
use std::path::Path;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};

use super::{FinalAction, Frontend, Progress, text, validate_selection};
use crate::config::{InstallConfig, Layout as PoolLayout};
use crate::disk::Disk;
use crate::error::{Error, IoContext, Result};
use crate::secret::SecretString;
use crate::validate;

// Named ANSI colours, not the GRUB theme's RGB values.
//
// The installer's home is tty1 on a machine that has just booted: the Linux
// virtual console is a 16-colour terminal and silently discards 24-bit colour
// escapes, which renders the entire interface in plain white. A serial console
// is no better. Named colours are the ones every terminal in the chain can
// actually show, so the violet becomes LightMagenta and the design survives
// contact with the hardware.
const BG: Color = Color::Black;
const TEXT: Color = Color::White;
const DIM: Color = Color::Gray;
const VIOLET: Color = Color::LightMagenta;
const DEEP: Color = Color::Magenta;
const WARN: Color = Color::Yellow;
const DANGER: Color = Color::LightRed;
const OK: Color = Color::LightGreen;

type Term = Terminal<CrosstermBackend<Stdout>>;

pub struct TuiFrontend {
    terminal: Term,
    /// Streamed install log, newest last.
    log: Vec<Line<'static>>,
    step: Option<(usize, usize, String)>,
    restored: bool,
}

impl TuiFrontend {
    /// Try to take over the terminal. An error here is not fatal to the
    /// install; main treats it as "use the plain frontend instead".
    pub fn new() -> Result<Self> {
        enable_raw_mode().ctx("could not put the terminal into raw mode")?;
        let mut out = stdout();
        if let Err(e) = execute!(out, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(Error::Io {
                context: "could not switch to the alternate screen".into(),
                source: e,
            });
        }
        let backend = CrosstermBackend::new(out);
        let terminal = match Terminal::new(backend) {
            Ok(t) => t,
            Err(e) => {
                let _ = execute!(stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(Error::Io {
                    context: "could not initialise the terminal".into(),
                    source: e,
                });
            }
        };

        Ok(TuiFrontend {
            terminal,
            log: Vec::new(),
            step: None,
            restored: false,
        })
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }

    fn draw<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        self.terminal.draw(f).ctx("could not draw the interface")?;
        Ok(())
    }

    /// Block for the next key press. Ctrl-C aborts from anywhere.
    fn key(&mut self) -> Result<KeyEvent> {
        loop {
            if !event::poll(Duration::from_millis(200)).ctx("terminal poll failed")? {
                continue;
            }
            match event::read().ctx("terminal read failed")? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                        return Err(Error::Aborted);
                    }
                    return Ok(k);
                }
                _ => continue,
            }
        }
    }
}

impl Drop for TuiFrontend {
    fn drop(&mut self) {
        // Leaving a terminal in raw mode with the alternate screen active makes
        // the console unusable, which on an appliance means a power cycle.
        self.restore();
    }
}

// --- chrome ---------------------------------------------------------------

/// Header, body, footer. Returns the body rect.
fn chrome(f: &mut ratatui::Frame, heading: &str, keys: &str) -> Rect {
    let area = f.area();
    f.render_widget(Clear, area);
    f.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "  Nightshade",
                Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  Installer", Style::default().fg(DIM)),
        ]),
        Line::from(Span::styled(
            format!("  {heading}"),
            Style::default().fg(TEXT),
        )),
    ]);
    f.render_widget(header, rows[0]);

    let footer = Paragraph::new(Line::from(Span::styled(
        format!("  {keys}"),
        Style::default().fg(DIM),
    )));
    f.render_widget(footer, rows[2]);

    // One column of breathing room on each side of the body.
    let inner = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .split(rows[1]);
    inner[1]
}

fn body(f: &mut ratatui::Frame, area: Rect, lines: Vec<Line<'static>>) {
    let p = Paragraph::new(Text::from(lines))
        .style(Style::default().fg(TEXT).bg(BG))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn plain(s: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(s.into(), Style::default().fg(TEXT)))
}

fn dim(s: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(s.into(), Style::default().fg(DIM)))
}

fn coloured(s: impl Into<String>, c: Color) -> Line<'static> {
    Line::from(Span::styled(s.into(), Style::default().fg(c)))
}

fn blank() -> Line<'static> {
    Line::from("")
}

/// A single-line text input rendered as `label [value_]`.
struct Field {
    value: String,
    masked: bool,
}

impl Field {
    fn new(masked: bool) -> Self {
        Field {
            value: String::new(),
            masked,
        }
    }

    fn render(&self, label: &str, active: bool) -> Line<'static> {
        let shown = if self.masked {
            "*".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        };
        let cursor = if active { "_" } else { " " };
        Line::from(vec![
            Span::styled(format!("  {label}"), Style::default().fg(DIM)),
            Span::styled(
                format!("{shown}{cursor}"),
                Style::default()
                    .fg(if active { VIOLET } else { TEXT })
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    }

    /// Apply a key press. Returns true if the key was consumed as text.
    fn handle(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(c) => {
                self.value.push(c);
                true
            }
            KeyCode::Backspace => {
                self.value.pop();
                true
            }
            _ => false,
        }
    }
}

// --- frontend -------------------------------------------------------------

impl Frontend for TuiFrontend {
    fn welcome(&mut self) -> Result<()> {
        loop {
            self.draw(|f| {
                let area = chrome(f, "Welcome", "Enter  Continue        Esc  Quit");
                let mut lines = vec![blank()];
                for l in text::WELCOME_WARNING.lines() {
                    lines.push(if l.contains("DESTRUCTIVE") {
                        coloured(format!("  {l}"), DANGER)
                    } else {
                        plain(format!("  {l}"))
                    });
                }
                body(f, area, lines);
            })?;

            match self.key()?.code {
                KeyCode::Enter => return Ok(()),
                KeyCode::Esc => return Err(Error::Aborted),
                _ => {}
            }
        }
    }

    fn select_disks(&mut self, disks: &[Disk]) -> Result<Vec<usize>> {
        let mut cursor = 0usize;
        let mut chosen: Vec<usize> = Vec::new();
        let mut error: Option<String> = None;

        loop {
            let rows: Vec<Line<'static>> = disks
                .iter()
                .enumerate()
                .flat_map(|(i, d)| {
                    let marker = if chosen.contains(&i) { "[x]" } else { "[ ]" };
                    let style = if i == cursor {
                        Style::default().fg(VIOLET).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(TEXT)
                    };
                    let mut out = vec![Line::from(Span::styled(
                        format!(
                            "  {marker} {:<12} {:>10}  {}",
                            d.path.display(),
                            d.size_human(),
                            d.model
                        ),
                        style,
                    ))];
                    out.push(dim(format!(
                        "        {}  serial {}",
                        d.media(),
                        d.serial
                    )));
                    if d.signatures.is_empty() {
                        out.push(dim("        no partition table detected"));
                    } else {
                        for s in &d.signatures {
                            out.push(dim(format!("        {s}")));
                        }
                    }
                    out.push(blank());
                    out
                })
                .collect();

            let err = error.clone();
            self.draw(|f| {
                let area = chrome(
                    f,
                    "Select installation target",
                    "Up/Down  Move    Space  Select    Enter  Confirm    Esc  Quit",
                );
                let mut lines = vec![blank()];
                lines.extend(rows.clone());
                for l in text::DISK_HELP.lines() {
                    lines.push(dim(format!("  {l}")));
                }
                if let Some(e) = &err {
                    lines.push(blank());
                    lines.push(coloured(format!("  {e}"), DANGER));
                }
                body(f, area, lines);
            })?;

            let key = self.key()?;
            error = None;
            match key.code {
                KeyCode::Esc => return Err(Error::Aborted),
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if cursor + 1 < disks.len() {
                        cursor += 1;
                    }
                }
                KeyCode::Char(' ') => {
                    if let Some(pos) = chosen.iter().position(|i| *i == cursor) {
                        chosen.remove(pos);
                    } else if chosen.len() < 2 {
                        chosen.push(cursor);
                    } else {
                        error = Some("Select one disk, or exactly two for a mirror.".into());
                    }
                }
                KeyCode::Enter => match validate_selection(&chosen, disks.len()) {
                    Ok(()) => return Ok(chosen),
                    Err(e) => error = Some(e.to_string()),
                },
                _ => {}
            }
        }
    }

    fn confirm_warning(&mut self, message: &str) -> Result<()> {
        let message = message.to_string();
        loop {
            let msg = message.clone();
            self.draw(|f| {
                let area = chrome(f, "Check this", "Enter  Continue anyway        Esc  Go back");
                let mut lines = vec![blank()];
                for l in msg.lines() {
                    lines.push(coloured(format!("  {l}"), WARN));
                }
                body(f, area, lines);
            })?;

            match self.key()?.code {
                KeyCode::Enter => return Ok(()),
                KeyCode::Esc => return Err(Error::Aborted),
                _ => {}
            }
        }
    }

    fn confirm_destruction(&mut self, disks: &[&Disk], layout: PoolLayout) -> Result<()> {
        let mut field = Field::new(false);
        let mut error: Option<String> = None;

        // Rendered once: this screen must show exactly what was decided, and
        // nothing about it changes while the operator types.
        let mut summary = vec![
            blank(),
            coloured("  These disks will be COMPLETELY ERASED:", DANGER),
            blank(),
        ];
        for d in disks {
            summary.push(plain(format!(
                "    {:<12} {:>10}  {}  ({})",
                d.path.display(),
                d.size_human(),
                d.model,
                d.serial
            )));
            for s in &d.signatures {
                summary.push(coloured(format!("        destroying: {s}"), DIM));
            }
        }
        summary.push(blank());
        summary.push(dim(format!("  Layout:    {}", layout.label())));
        summary.push(dim(format!(
            "  Boot pool: {} (2 GiB per disk)",
            crate::config::BPOOL
        )));
        summary.push(dim(format!(
            "  Root pool: {} (remaining space)",
            crate::config::RPOOL
        )));
        summary.push(blank());
        summary.push(coloured("  This cannot be undone.", DANGER));
        summary.push(blank());

        loop {
            let summary = summary.clone();
            let err = error.clone();
            let f_line = field.render(
                &format!("Type {} to continue:  ", validate::ERASE_CONFIRMATION),
                true,
            );
            self.draw(|f| {
                let area = chrome(f, "Confirm destruction", "Enter  Confirm        Esc  Cancel");
                let mut lines = summary;
                lines.push(f_line);
                if let Some(e) = &err {
                    lines.push(blank());
                    lines.push(coloured(format!("  {e}"), DANGER));
                }
                body(f, area, lines);
            })?;

            let key = self.key()?;
            match key.code {
                KeyCode::Esc => return Err(Error::Aborted),
                KeyCode::Enter => match validate::erase_confirmation(&field.value) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        error = Some(e.to_string());
                        field.value.clear();
                    }
                },
                _ => {
                    if field.handle(&key) {
                        error = None;
                    }
                }
            }
        }
    }

    fn collect_password(&mut self, user: &str) -> Result<SecretString> {
        let user = user.to_string();
        let mut first = Field::new(true);
        let mut second = Field::new(true);
        let mut active = 0usize;
        let mut error: Option<String> = None;

        loop {
            let err = error.clone();
            let u = user.clone();
            let l1 = first.render("Password:          ", active == 0);
            let l2 = second.render("Confirm password:  ", active == 1);
            self.draw(|f| {
                let area = chrome(
                    f,
                    "Administrator account",
                    "Tab  Switch field        Enter  Confirm        Esc  Quit",
                );
                let mut lines = vec![blank(), plain(format!("  Account: {u}")), blank()];
                for l in text::PASSWORD_HELP.lines() {
                    lines.push(dim(format!("  {l}")));
                }
                lines.push(blank());
                lines.push(dim(format!(
                    "  At least {} characters. Required - there is no way past this screen.",
                    validate::MIN_PASSWORD_LEN
                )));
                lines.push(blank());
                lines.push(l1);
                lines.push(l2);
                if let Some(e) = &err {
                    lines.push(blank());
                    lines.push(coloured(format!("  {e}"), DANGER));
                }
                body(f, area, lines);
            })?;

            let key = self.key()?;
            match key.code {
                KeyCode::Esc => return Err(Error::Aborted),
                KeyCode::Tab | KeyCode::Down => active = 1 - active,
                KeyCode::Up => active = 1 - active,
                KeyCode::Enter => {
                    if active == 0 {
                        active = 1;
                        continue;
                    }
                    let a = SecretString::new(first.value.clone());
                    let b = SecretString::new(second.value.clone());
                    match validate::password(&a, &b) {
                        Ok(()) => return Ok(a),
                        Err(e) => {
                            error = Some(e.to_string());
                            first.value.clear();
                            second.value.clear();
                            active = 0;
                        }
                    }
                }
                _ => {
                    let consumed = if active == 0 {
                        first.handle(&key)
                    } else {
                        second.handle(&key)
                    };
                    if consumed {
                        error = None;
                    }
                }
            }
        }
    }

    fn collect_hostname(&mut self, default: &str) -> Result<String> {
        let default = default.to_string();
        let mut field = Field::new(false);
        field.value = default.clone();
        let mut error: Option<String> = None;

        loop {
            let err = error.clone();
            let d = default.clone();
            let line = field.render("Hostname:  ", true);
            self.draw(|f| {
                let area = chrome(f, "Hostname", "Enter  Confirm        Esc  Quit");
                let mut lines = vec![
                    blank(),
                    dim(format!("  Default: {d}")),
                    dim("  Letters, digits and hyphens (RFC 1123)."),
                    blank(),
                    line,
                ];
                if let Some(e) = &err {
                    lines.push(blank());
                    lines.push(coloured(format!("  {e}"), DANGER));
                }
                body(f, area, lines);
            })?;

            let key = self.key()?;
            match key.code {
                KeyCode::Esc => return Err(Error::Aborted),
                KeyCode::Enter => match validate::hostname(field.value.trim()) {
                    Ok(()) => return Ok(field.value.trim().to_string()),
                    Err(e) => error = Some(e.to_string()),
                },
                _ => {
                    if field.handle(&key) {
                        error = None;
                    }
                }
            }
        }
    }

    fn confirm_summary(&mut self, config: &InstallConfig) -> Result<()> {
        let mut lines = vec![blank()];
        lines.push(plain(format!("  Layout:     {}", config.layout().label())));
        for (i, d) in config.disks.iter().enumerate() {
            lines.push(plain(format!(
                "  Disk {}:     {} ({}, {})",
                i + 1,
                d.path.display(),
                d.size_human(),
                d.serial
            )));
        }
        lines.push(blank());
        lines.push(dim(format!(
            "  Boot pool:  {} -> /boot",
            crate::config::BOOT_DATASET
        )));
        lines.push(dim(format!(
            "  Root pool:  {} -> /",
            crate::config::ROOT_DATASET
        )));
        lines.push(dim("              separate datasets for /var/log and /var/tmp"));
        lines.push(dim("  ESP:        512 MiB FAT32 on each disk"));
        lines.push(blank());
        lines.push(plain(format!("  Hostname:   {}", config.hostname)));
        lines.push(plain(format!(
            "  Account:    {} (sudo)",
            validate::DEFAULT_USER
        )));
        lines.push(plain("  Root login: locked"));
        lines.push(blank());
        lines.push(coloured(
            "  Pressing Enter starts writing to disk.",
            DANGER,
        ));

        loop {
            let lines = lines.clone();
            self.draw(|f| {
                let area = chrome(f, "Summary", "Enter  Install        Esc  Cancel");
                body(f, area, lines);
            })?;

            match self.key()?.code {
                KeyCode::Enter => return Ok(()),
                KeyCode::Esc => return Err(Error::Aborted),
                _ => {}
            }
        }
    }

    fn progress(&mut self, progress: Progress) {
        match progress {
            Progress::Step { index, total, name } => {
                self.log
                    .push(coloured(format!("  [{index}/{total}] {name}"), VIOLET));
                self.step = Some((index, total, name));
            }
            Progress::Detail(d) => self.log.push(dim(format!("        {d}"))),
            Progress::Warning(w) => self.log.push(coloured(format!("      ! {w}"), WARN)),
        }

        let step = self.step.clone();
        // Keep only what fits; this is a live view, and the full record is in
        // the log file.
        let start = self.log.len().saturating_sub(200);
        let lines: Vec<Line<'static>> = self.log[start..].to_vec();

        // A draw failure here must not abort an install that is otherwise fine.
        let _ = self.terminal.draw(|f| {
            // Plain hyphen, not an em dash: the Linux console font has no glyph
            // for U+2014 and draws a box instead.
            let area = chrome(f, "Installing", "Please wait - do not power off");
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(3)])
                .split(area);

            let (index, total, name) = step.unwrap_or((0, 1, String::new()));
            let percent = ((index as f64 / total as f64) * 100.0).round() as u16;
            let gauge = Gauge::default()
                .block(Block::default().borders(Borders::NONE))
                .gauge_style(Style::default().fg(DEEP).bg(Color::Rgb(0x24, 0x1C, 0x3C)))
                .label(Span::styled(
                    format!("{index}/{total}  {name}"),
                    Style::default().fg(TEXT),
                ))
                .percent(percent.min(100));
            f.render_widget(gauge, rows[0]);

            // Show the tail: the interesting part of a long log is the end.
            let height = rows[1].height as usize;
            let tail = lines.len().saturating_sub(height);
            let p = Paragraph::new(Text::from(lines[tail..].to_vec()))
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: false });
            f.render_widget(p, rows[1]);
        });
    }

    fn failed(&mut self, err: &Error, log_path: &Path) {
        let detail = err.detail();
        let summary = err.to_string();
        let log_path = log_path.display().to_string();

        // Scrollable: a failed grub-install can produce more output than fits.
        let mut offset = 0usize;
        loop {
            let detail = detail.clone();
            let summary = summary.clone();
            let log_path = log_path.clone();
            let drew = self.terminal.draw(|f| {
                let area = chrome(
                    f,
                    "Installation failed",
                    "Up/Down  Scroll        Enter  Continue",
                );
                let mut lines = vec![blank(), coloured(format!("  {summary}"), DANGER), blank()];
                for l in detail.lines().skip(offset) {
                    lines.push(dim(format!("  {l}")));
                }
                lines.push(blank());
                lines.push(plain(format!("  Full log: {log_path}")));
                lines.push(dim(
                    "  The disks are in whatever state the failing step left them.",
                ));
                lines.push(dim("  Re-running the installer starts again from scratch."));
                body(f, area, lines);
            });
            if drew.is_err() {
                return;
            }

            match self.key() {
                Ok(k) => match k.code {
                    KeyCode::Enter | KeyCode::Esc => return,
                    KeyCode::Down => offset = offset.saturating_add(1),
                    KeyCode::Up => offset = offset.saturating_sub(1),
                    _ => {}
                },
                Err(_) => return,
            }
        }
    }

    fn finished(&mut self, log_path: &Path) -> Result<FinalAction> {
        let log_path = log_path.display().to_string();
        let mut choice = 0usize;

        loop {
            let log_path = log_path.clone();
            self.draw(|f| {
                let area = chrome(
                    f,
                    "Installation complete",
                    "Left/Right  Choose        Enter  Confirm",
                );
                let mut lines = vec![
                    blank(),
                    coloured("  Nightshade has been installed.", OK),
                    blank(),
                    plain("  Remove the installation medium before rebooting."),
                    dim("  Log copied to /var/log/nightshade-install.log on the new system."),
                    dim(format!("  Log on this live system: {log_path}")),
                    blank(),
                ];

                let style_for = |active: bool| {
                    if active {
                        Style::default()
                            .fg(BG)
                            .bg(VIOLET)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(DIM)
                    }
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("  Reboot now  ", style_for(choice == 0)),
                    Span::raw("   "),
                    Span::styled("  Exit to a shell  ", style_for(choice == 1)),
                ]));
                body(f, area, lines);
            })?;

            match self.key()?.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => choice = 1 - choice,
                KeyCode::Enter => {
                    // Hand the terminal back before the engine reboots or execs
                    // a shell into it.
                    self.restore();
                    return Ok(if choice == 0 {
                        FinalAction::Reboot
                    } else {
                        FinalAction::Shell
                    });
                }
                _ => {}
            }
        }
    }
}
