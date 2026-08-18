//! Modes, prompts, and what every typed line does.
//!
//! # Two modes
//!
//! ```text
//! user@hostname>    operational     show, ping, traceroute, configure, shell
//! user@hostname#    configuration   set, delete, compare, commit, save, ...
//! ```
//!
//! The host name in the prompt is read from the running configuration before
//! every prompt, so committing a change to `system host-name` changes it. A
//! prompt that lies about which box you are on is a prompt that gets somebody
//! else's production firewall reconfigured.
//!
//! # No shell escapes
//!
//! There is no `!`, no backtick, no `$(...)`, and `|` is a set of functions in
//! [`crate::pipe`] rather than a pipeline. `ping` and `traceroute` exec their
//! binaries with an argv, and their argument is checked against the schema's
//! own host-name and address rules first -- so a host that starts with `-`, or
//! that is really a command substitution somebody hoped would be expanded,
//! never reaches them.
//!
//! The one way to a shell is [`crate::shell`], and it is gated by configd.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use nightshade_proto::message::{
    FailureKind, LoadSource, OpTarget, Report, Request, Response, SessionId,
};
use nightshade_schema::config::ConfigTree;
use nightshade_schema::model::Schema;
use nightshade_schema::path::Path;
use nightshade_schema::value::{ValueType, ValueSpec};
use reedline::{Color, Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus};

use crate::client::{Client, ClientError};
use crate::complete::{self, Context};
use crate::output::{self, Output, Style};
use crate::pipe::{self, Modifier};

/// What a command did, and what `ns` exits with because of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// A command that does not exist, or could not be run. Exit 1.
    CommandError,
    /// A configuration that was refused. Exit 2.
    ConfigError,
    /// Leave the session.
    Leave,
}

impl Outcome {
    pub fn code(self) -> u8 {
        match self {
            Outcome::Ok | Outcome::Leave => 0,
            Outcome::CommandError => 1,
            Outcome::ConfigError => 2,
        }
    }

    fn of(kind: FailureKind) -> Self {
        match kind {
            // A commit or a `set` that was refused is a configuration problem,
            // which a script wants to tell apart from a command it got wrong.
            FailureKind::Validation | FailureKind::Conflict => Outcome::ConfigError,
            FailureKind::Request | FailureKind::Internal => Outcome::CommandError,
        }
    }

}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Operational,
    Configuration,
}

/// The commands `?` lists and `<Tab>` completes.
///
/// One spelling each. The aliases -- `conf`, `config`, `sh` -- are accepted
/// but deliberately not listed: they are shorthands for a word already here,
/// and listing both spellings makes `<Tab>` ambiguous where it currently
/// fills the rest of the word in.
const OPERATIONAL_COMMANDS: &[&str] = &[
    "configure", "exit", "help", "logout", "ping", "quit", "shell", "show", "traceroute",
];

const CONFIGURATION_COMMANDS: &[&str] = &[
    "commit", "compare", "confirm", "delete", "discard", "edit", "exit", "help", "load",
    "rollback", "save", "set", "show", "top", "up",
];

pub struct Cli {
    client: Client,
    schema: &'static Schema,
    pub mode: Mode,
    session: Option<SessionId>,
    /// What `edit` put in front of everything typed.
    prefix: Path,
    user: String,
    hostname: String,
    pub context: Arc<Mutex<Context>>,
    colour: bool,
}

impl Cli {
    pub fn new(client: Client, schema: &'static Schema, colour: bool) -> Self {
        Self {
            client,
            schema,
            mode: Mode::Operational,
            session: None,
            prefix: Path::root(),
            user: current_user(),
            hostname: "nightshade".to_string(),
            context: Arc::new(Mutex::new(Context::default())),
            colour,
        }
    }

    pub fn colour(&self) -> bool {
        self.colour
    }

    /// `user@hostname>` or `user@hostname#`, with the path `edit` set.
    pub fn prompt(&self) -> String {
        let marker = match self.mode {
            Mode::Operational => '>',
            Mode::Configuration => '#',
        };
        if self.prefix.is_empty() {
            format!("{}@{}{marker} ", self.user, self.hostname)
        } else {
            format!("[edit {}]\n{}@{}{marker} ", self.prefix, self.user, self.hostname)
        }
    }

    /// Bring the completion context and the prompt up to date.
    ///
    /// Once per prompt rather than per keystroke: fast enough over a unix
    /// socket, and a completer that talks to a daemon mid-word can hang a
    /// terminal when the daemon is busy applying something.
    pub fn refresh(&mut self) {
        if let Ok(Response::Config { tree }) =
            self.client.call(Request::ShowRunning { path: Path::root() })
            && let Some(name) = tree
                .get(&Path::from_segments(["system", "host-name"]))
                .and_then(|node| node.value())
        {
            self.hostname = name.to_string();
        }

        let candidate = match &self.session {
            Some(session) => match self.client.call(Request::ShowCandidate {
                session: session.clone(),
                path: Path::root(),
            }) {
                Ok(Response::Config { tree }) => tree,
                _ => ConfigTree::new(),
            },
            None => ConfigTree::new(),
        };

        if let Ok(mut context) = self.context.lock() {
            context.candidate = candidate;
            context.prefix = self.prefix.clone();
            context.system_interfaces = output::system_interfaces();
            context.commands = match self.mode {
                Mode::Operational => OPERATIONAL_COMMANDS.to_vec(),
                Mode::Configuration => CONFIGURATION_COMMANDS.to_vec(),
            };
        }
    }

    /// Run one typed line and print what it produced.
    pub fn run(&mut self, line: &str) -> Outcome {
        let line = line.trim();
        if line.is_empty() {
            return Outcome::Ok;
        }

        // `?` asks what can go here, and is handled before anything else.
        // The prefix arrives with its trailing space intact, which is what
        // makes `set ?` a question about what follows `set`; see
        // [`complete::question`].
        if let Some(prefix) = complete::question(line) {
            print!("{}", self.help(prefix));
            return Outcome::Ok;
        }

        let (command, modifiers) = match pipe::split(line) {
            Ok(split) => split,
            Err(e) => return self.fail(e),
        };

        match self.dispatch(&command) {
            Ok((output, outcome)) => {
                self.emit(&output, &modifiers);
                outcome
            }
            Err(e) => self.fail(e),
        }
    }

    fn fail(&self, error: impl std::fmt::Display) -> Outcome {
        eprintln!("{error}");
        Outcome::CommandError
    }

    fn emit(&self, output: &Output, modifiers: &[Modifier]) {
        let style = Style {
            schema: self.schema,
            colour: self.colour,
            secrets: pipe::wants(modifiers, &Modifier::DisplaySecrets),
        };
        let text = if pipe::wants(modifiers, &Modifier::DisplayJson) {
            output.render_json(&style)
        } else {
            output.render(&style)
        };
        let text = pipe::apply(text, modifiers);
        if text.is_empty() {
            return;
        }
        page(&text, pipe::wants(modifiers, &Modifier::NoMore));
    }

    // -- dispatch -----------------------------------------------------------

    fn dispatch(&mut self, command: &str) -> Result<(Output, Outcome), ClientError> {
        let words = match Path::parse(command) {
            Ok(path) => path.segments().to_vec(),
            Err(e) => return Ok((Output::Text(format!("{e}\n")), Outcome::CommandError)),
        };
        let Some((verb, rest)) = words.split_first() else {
            return Ok((Output::Nothing, Outcome::Ok));
        };

        match self.mode {
            Mode::Operational => self.operational(verb, rest),
            Mode::Configuration => self.configuration(verb, rest),
        }
    }

    fn operational(&mut self, verb: &str, rest: &[String]) -> Result<(Output, Outcome), ClientError> {
        match verb {
            "show" | "sh" => self.op_show(rest), // not-a-shell: `sh` is short for `show`; `shell` is spelled out
            // Shorthands for the same command, written out. Named aliases
            // rather than prefix matching: `conf` means this and only this,
            // and it goes on meaning it when a command starting with `conf` is
            // added later. A CLI that resolves half a word by what else
            // happens to exist is a CLI whose meaning changes under an
            // operator who has already learnt it.
            "configure" | "config" | "conf" => {
                match self.client.call(Request::SessionOpen)? {
                    Response::Session { id } => {
                        self.session = Some(id);
                        self.mode = Mode::Configuration;
                        self.prefix = Path::root();
                        Ok((
                            Output::Text("entering configuration mode\n".into()),
                            Outcome::Ok,
                        ))
                    }
                    other => Ok(self.unexpected(other)),
                }
            }
            "shell" => self.shell(),
            "ping" => self.trace_or_ping("ping", rest),
            "traceroute" => self.trace_or_ping("traceroute", rest),
            "exit" | "quit" | "logout" => Ok((Output::Nothing, Outcome::Leave)),
            "help" => Ok((Output::Text(self.help("")), Outcome::Ok)),
            other => Ok(unknown(other, OPERATIONAL_COMMANDS)),
        }
    }

    fn op_show(&mut self, rest: &[String]) -> Result<(Output, Outcome), ClientError> {
        let words: Vec<&str> = rest.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["version"] => match self.client.call(Request::OpShow {
                target: OpTarget::Version,
            })? {
                Response::Operational {
                    report: Report::Version { version },
                } => Ok((
                    Output::Text(format!("Nightshade {version}\n")),
                    Outcome::Ok,
                )),
                other => Ok(self.unexpected(other)),
            },

            ["configuration"] | ["config"] | ["conf"] => {
                self.expect_config(Request::ShowSaved { path: Path::root() })
            }

            ["system", "commit-log"] => match self.client.call(Request::CommitLog)? {
                Response::Revisions { revisions } => Ok((Output::Revisions(revisions), Outcome::Ok)),
                other => Ok(self.unexpected(other)),
            },

            // `show interfaces`, `show interfaces detail`,
            // `show interfaces <type> <name> [detail]`.
            ["interfaces", tail @ ..] => {
                let (tail, detail) = match tail.split_last() {
                    Some((&"detail", head)) => (head, true),
                    _ => (tail, false),
                };
                let target = match tail {
                    [] => OpTarget::Interfaces,
                    [_kind, name] => OpTarget::Interface {
                        name: (*name).to_string(),
                    },
                    [name] => OpTarget::Interface {
                        name: (*name).to_string(),
                    },
                    _ => {
                        return Ok((
                            Output::Text(
                                "show interfaces [<type> <name>] [detail]\n".into(),
                            ),
                            Outcome::CommandError,
                        ));
                    }
                };
                match self.client.call(Request::OpShow { target })? {
                    Response::Operational {
                        report: Report::Interfaces { interfaces },
                    } => Ok((Output::Interfaces { interfaces, detail }, Outcome::Ok)),
                    other => Ok(self.unexpected(other)),
                }
            }

            _ => Ok((
                Output::Text(
                    "show version | interfaces | configuration | system commit-log\n".into(),
                ),
                Outcome::CommandError,
            )),
        }
    }

    fn shell(&mut self) -> Result<(Output, Outcome), ClientError> {
        // configd decides, and records it against the uid it read from the
        // socket rather than one this process claimed.
        match self.client.call(Request::ShellSession { entering: true })? {
            Response::Ok => {}
            Response::Failed { kind, message } => {
                return Ok((Output::Text(format!("{message}\n")), Outcome::of(kind)));
            }
            other => return Ok(self.unexpected(other)),
        }

        println!("entering a shell; type 'operational' or exit to return");
        let outcome = match crate::shell::run() {
            Ok(()) => Outcome::Ok,
            Err(e) => {
                eprintln!("{e}");
                Outcome::CommandError
            }
        };
        let _ = self.client.call(Request::ShellSession { entering: false })?;
        Ok((Output::Nothing, outcome))
    }

    /// `ping` and `traceroute`, with the destination checked first.
    ///
    /// Checked against the schema's own rules for an address or a host name,
    /// which is what stops `-oProxyCommand=...`, `$(id)` and every other thing
    /// somebody might hope gets expanded. Nothing here is expanded by
    /// anything: the destination is one element of an argv.
    fn trace_or_ping(
        &mut self,
        program: &str,
        rest: &[String],
    ) -> Result<(Output, Outcome), ClientError> {
        let [destination] = rest else {
            return Ok((
                Output::Text(format!("{program} <host>\n")),
                Outcome::CommandError,
            ));
        };

        if !is_destination(destination) {
            return Ok((
                Output::Text(format!(
                    "{destination:?} is not an address or a host name\n"
                )),
                Outcome::CommandError,
            ));
        }

        let mut command = std::process::Command::new(program);
        if program == "ping" {
            command.arg("-c").arg("5");
        }
        command.arg(destination);

        match command.status() {
            Ok(status) if status.success() => Ok((Output::Nothing, Outcome::Ok)),
            Ok(_) => Ok((Output::Nothing, Outcome::CommandError)),
            Err(e) => Ok((
                Output::Text(format!("{program} could not be run: {e}\n")),
                Outcome::CommandError,
            )),
        }
    }

    // -- configuration mode -------------------------------------------------

    fn configuration(
        &mut self,
        verb: &str,
        rest: &[String],
    ) -> Result<(Output, Outcome), ClientError> {
        let session = self.session.clone().expect("configuration mode has a session");

        match verb {
            "set" | "delete" => {
                let typed = self.with_prefix(rest);
                let (path, value) = self.schema.split_value(&typed);
                let request = if verb == "set" {
                    Request::Set {
                        session,
                        path,
                        value,
                    }
                } else {
                    Request::Delete {
                        session,
                        path,
                        value,
                    }
                };
                self.expect_ok(request)
            }

            "show" | "sh" => { // not-a-shell: `sh` is short for `show`; there is no shell from here at all
                let at = self.with_prefix(rest);
                let candidate = match self.client.call(Request::ShowCandidate {
                    session: session.clone(),
                    path: at.clone(),
                })? {
                    Response::Config { tree } => tree,
                    other => return Ok(self.unexpected(other)),
                };
                let running = match self.client.call(Request::ShowRunning { path: at })? {
                    Response::Config { tree } => tree,
                    other => return Ok(self.unexpected(other)),
                };
                Ok((Output::ConfigDiff { candidate, running }, Outcome::Ok))
            }

            "compare" => match self.client.call(Request::Compare { session })? {
                Response::Changes { changes } => {
                    if changes.is_empty() {
                        Ok((
                            Output::Text("no changes between the candidate and running\n".into()),
                            Outcome::Ok,
                        ))
                    } else {
                        Ok((Output::Changes(changes), Outcome::Ok))
                    }
                }
                other => Ok(self.unexpected(other)),
            },

            "commit" => self.commit(session, rest),

            "confirm" => self.expect_commit(Request::Confirm { session }),

            "discard" => self.expect_ok(Request::Discard { session }),

            "save" => self.expect_ok(Request::Save),

            "load" => self.expect_ok(Request::Load {
                session,
                source: LoadSource::Saved,
            }),

            "rollback" => match rest {
                [revision] => match revision.parse::<u64>() {
                    Ok(revision) => self.expect_ok(Request::Load {
                        session,
                        source: LoadSource::Archive { revision },
                    }),
                    Err(_) => Ok((
                        Output::Text(format!("{revision:?} is not a revision number\n")),
                        Outcome::CommandError,
                    )),
                },
                _ => Ok((
                    Output::Text("rollback <revision>\n".into()),
                    Outcome::CommandError,
                )),
            },

            "edit" => {
                let at = self.with_prefix(rest);
                if self.schema.resolve(&at).is_none() {
                    return Ok((
                        Output::Text(format!("`{at}` is not a configuration path\n")),
                        Outcome::ConfigError,
                    ));
                }
                self.prefix = at;
                Ok((Output::Nothing, Outcome::Ok))
            }

            "up" => {
                self.prefix = self.prefix.parent().unwrap_or_default();
                Ok((Output::Nothing, Outcome::Ok))
            }

            "top" => {
                self.prefix = Path::root();
                Ok((Output::Nothing, Outcome::Ok))
            }

            "exit" => self.leave_configuration(session, rest),

            "help" => Ok((Output::Text(self.help("")), Outcome::Ok)),

            "shell" => Ok((
                Output::Text(
                    "shell is not available from configuration mode; \
                     `exit` to operational mode first\n"
                        .into(),
                ),
                Outcome::CommandError,
            )),

            other => Ok(unknown(other, CONFIGURATION_COMMANDS)),
        }
    }

    fn commit(
        &mut self,
        session: SessionId,
        rest: &[String],
    ) -> Result<(Output, Outcome), ClientError> {
        let mut comment = None;
        let mut confirm_minutes = None;
        let mut words = rest.iter();

        while let Some(word) = words.next() {
            match word.as_str() {
                "confirm" => match words.next().map(|m| m.parse::<u16>()) {
                    Some(Ok(minutes)) if minutes > 0 => confirm_minutes = Some(minutes),
                    _ => {
                        return Ok((
                            Output::Text("commit confirm <minutes>\n".into()),
                            Outcome::CommandError,
                        ));
                    }
                },
                "comment" => match words.next() {
                    Some(text) => comment = Some(text.clone()),
                    None => {
                        return Ok((
                            Output::Text("commit comment \"...\"\n".into()),
                            Outcome::CommandError,
                        ));
                    }
                },
                other => {
                    return Ok((
                        Output::Text(format!(
                            "commit [confirm <minutes>] [comment \"...\"]; \
                             `{other}` is not one of those\n"
                        )),
                        Outcome::CommandError,
                    ));
                }
            }
        }

        self.expect_commit(Request::Commit {
            session,
            comment,
            confirm_minutes,
        })
    }

    /// `exit` from configuration mode, refusing to lose work.
    fn leave_configuration(
        &mut self,
        session: SessionId,
        rest: &[String],
    ) -> Result<(Output, Outcome), ClientError> {
        let discarding = rest.first().map(String::as_str) == Some("discard");

        if !discarding {
            match self.client.call(Request::Compare {
                session: session.clone(),
            })? {
                Response::Changes { changes } if !changes.is_empty() => {
                    return Ok((
                        Output::Text(format!(
                            "there are {} uncommitted changes.\n\
                             commit them, or `exit discard` to throw them away.\n",
                            changes.len()
                        )),
                        Outcome::ConfigError,
                    ));
                }
                Response::Changes { .. } => {}
                other => return Ok(self.unexpected(other)),
            }
        }

        let _ = self.client.call(Request::SessionClose { session })?;
        self.session = None;
        self.mode = Mode::Operational;
        self.prefix = Path::root();
        Ok((
            Output::Text("leaving configuration mode\n".into()),
            Outcome::Ok,
        ))
    }

    // -- helpers ------------------------------------------------------------

    fn with_prefix(&self, rest: &[String]) -> Path {
        let mut path = self.prefix.clone();
        for word in rest {
            path.push(word.clone());
        }
        path
    }

    fn expect_ok(&mut self, request: Request) -> Result<(Output, Outcome), ClientError> {
        match self.client.call(request)? {
            Response::Ok => Ok((Output::Nothing, Outcome::Ok)),
            Response::Failed { kind, message } => {
                Ok((Output::Text(format!("{message}\n")), Outcome::of(kind)))
            }
            other => Ok(self.unexpected(other)),
        }
    }

    fn expect_config(&mut self, request: Request) -> Result<(Output, Outcome), ClientError> {
        match self.client.call(request)? {
            Response::Config { tree } => Ok((Output::Config(tree), Outcome::Ok)),
            Response::Failed { kind, message } => {
                Ok((Output::Text(format!("{message}\n")), Outcome::of(kind)))
            }
            other => Ok(self.unexpected(other)),
        }
    }

    fn expect_commit(&mut self, request: Request) -> Result<(Output, Outcome), ClientError> {
        match self.client.call(request)? {
            Response::Committed {
                generation,
                changes,
                confirm_within,
            } => {
                let mut text = if changes.is_empty() {
                    "no configuration changes to commit\n".to_string()
                } else {
                    format!("committed {} changes as revision {generation}\n", changes.len())
                };
                if let Some(seconds) = confirm_within {
                    text.push_str(&format!(
                        "\nthis change will be ROLLED BACK in {} minutes {} seconds \
                         unless you type `confirm`\n",
                        seconds / 60,
                        seconds % 60
                    ));
                }
                Ok((Output::Text(text), Outcome::Ok))
            }
            Response::Failed { kind, message } => {
                Ok((Output::Text(format!("{message}\n")), Outcome::of(kind)))
            }
            other => Ok(self.unexpected(other)),
        }
    }

    /// configd answered something this command did not ask for. A bug on one
    /// side or the other, and not something to paper over.
    fn unexpected(&self, response: Response) -> (Output, Outcome) {
        (
            Output::Text(format!(
                "the configuration daemon sent an unexpected reply: {response:?}\n"
            )),
            Outcome::CommandError,
        )
    }

    /// What `?` prints, for everything typed in front of it.
    ///
    /// Whitespace at the end of `typed` is significant; see [`Cli::run`].
    pub fn help(&self, typed: &str) -> String {
        let context = match self.context.lock() {
            Ok(context) => context,
            Err(_) => return String::new(),
        };
        let (words, partial) = complete::words(typed);
        let entries = complete::candidates(self.schema, &context, &words, partial);

        let mut out = String::from("possible completions:\n");
        out.push_str(&output::help_lines(&entries));
        out
    }
}

fn unknown(verb: &str, known: &[&str]) -> (Output, Outcome) {
    let mut text = format!("`{verb}` is not a command here.\n");
    let close: Vec<&&str> = known
        .iter()
        .filter(|name| name.starts_with(verb.chars().next().unwrap_or(' ')))
        .collect();
    if !close.is_empty() {
        text.push_str(&format!(
            "did you mean: {}\n",
            close
                .iter()
                .map(|name| **name)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    (Output::Text(text), Outcome::CommandError)
}

/// An address or a host name, and nothing else.
///
/// Reuses the schema's rules rather than inventing a second answer to what a
/// host name is. This is what stops an argument that begins with `-` being
/// read as a flag by `ping`, and it is why nothing else has to.
fn is_destination(text: &str) -> bool {
    ValueSpec::new(ValueType::IpAddress).check(text).is_ok()
        || ValueSpec::new(ValueType::Hostname).check(text).is_ok()
}

fn current_user() -> String {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|user| user.name)
        .unwrap_or_else(|| "operator".to_string())
}

/// Print, a screenful at a time when there is a screen and more than one.
fn page(text: &str, no_more: bool) {
    use std::io::Write;

    let rows = output::terminal_rows();
    let lines: Vec<&str> = text.lines().collect();
    if no_more || lines.len() < rows {
        print!("{text}");
        let _ = std::io::stdout().flush();
        return;
    }

    let mut stdout = std::io::stdout();
    for (i, chunk) in lines.chunks(rows.saturating_sub(1)).enumerate() {
        if i > 0 {
            let _ = write!(stdout, "-- more -- ");
            let _ = stdout.flush();
            let mut discard = String::new();
            if std::io::stdin().read_line(&mut discard).unwrap_or(0) == 0 {
                let _ = writeln!(stdout);
                return;
            }
        }
        for line in chunk {
            let _ = writeln!(stdout, "{line}");
        }
    }
    let _ = stdout.flush();
}

/// The violet the rest of Nightshade is drawn in.
///
/// The same ANSI slot the installer's `VIOLET` uses -- palette 13, `SGR 95` --
/// and named rather than given as RGB for the same reason it is named there:
/// the Linux virtual console this is read on is a 16-colour terminal that
/// discards 24-bit escapes. Without it the prompt is reedline's default green,
/// which is nothing else on the box.
const VIOLET: Color = Color::LightMagenta;

/// The reedline prompt.
pub struct NsPrompt {
    pub text: String,
    /// Off when nobody is going to see it; see [`crate::output::use_colour`].
    pub colour: bool,
}

impl NsPrompt {
    pub fn new(text: String, colour: bool) -> Self {
        Self { text, colour }
    }
}

impl Prompt for NsPrompt {
    fn get_prompt_color(&self) -> Color {
        if self.colour { VIOLET } else { Color::Default }
    }

    /// The whole prompt is in [`Self::render_prompt_left`], so this colours
    /// nothing today. Set anyway, so that giving the indicator its own text
    /// later cannot reintroduce the green a word at a time.
    fn get_indicator_color(&self) -> Color {
        self.get_prompt_color()
    }

    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        // The whole prompt, marker and all, is in `text`.
        Cow::Borrowed("")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }
    fn render_prompt_history_search_indicator(
        &self,
        search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!("({prefix}search: {}) ", search.term))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_say_which_kind_of_wrong() {
        assert_eq!(Outcome::Ok.code(), 0);
        assert_eq!(Outcome::Leave.code(), 0);
        assert_eq!(Outcome::CommandError.code(), 1);
        assert_eq!(Outcome::ConfigError.code(), 2);

        assert_eq!(Outcome::of(FailureKind::Validation), Outcome::ConfigError);
        assert_eq!(Outcome::of(FailureKind::Conflict), Outcome::ConfigError);
        assert_eq!(Outcome::of(FailureKind::Request), Outcome::CommandError);
        assert_eq!(Outcome::of(FailureKind::Internal), Outcome::CommandError);
    }

    /// The check that keeps operator input away from anything that could read
    /// it as a flag or a substitution.
    #[test]
    fn only_addresses_and_host_names_can_be_pinged() {
        for good in ["10.0.0.1", "2001:db8::1", "example.com", "fw-01", "a"] {
            assert!(is_destination(good), "{good} should be pingable");
        }
        for bad in [
            "-oProxyCommand=id",
            "--help",
            "$(id)",
            "`id`",
            "a; rm -rf /",
            "a b",
            "",
            "host|other",
            "10.0.0.1 -c 100000",
        ] {
            assert!(!is_destination(bad), "{bad:?} should be refused");
        }
    }

    #[test]
    fn an_unknown_command_suggests_the_near_ones() {
        let (output, outcome) = unknown("comit", CONFIGURATION_COMMANDS);
        assert_eq!(outcome, Outcome::CommandError);
        let Output::Text(text) = output else {
            panic!("expected text");
        };
        assert!(text.contains("not a command here"), "{text}");
        assert!(text.contains("commit"), "{text}");
    }
}

