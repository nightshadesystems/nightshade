//! Turning a response into something to read.
//!
//! Every command produces an [`Output`], which knows how to render itself as
//! text and as JSON. That is what makes `| display json` a modifier rather
//! than a separate code path: the same value renders two ways, and the text
//! form is never parsed back to produce the JSON one.

use std::io::IsTerminal;

use nightshade_ifstate::Snapshot;
use nightshade_ifstate::query::View;
use nightshade_proto::message::RevisionInfo;
use nightshade_schema::config::{Body, ConfigTree, Node};
use nightshade_schema::diff::{Change, Op};
use nightshade_schema::model::{Location, Schema};
use nightshade_schema::path::Path;
use nightshade_schema::curly;

/// What was masked in place of a secret.
pub const MASK: &str = "****";

#[derive(Debug, Clone)]
pub enum Output {
    Nothing,
    /// Already-formatted text: a message, a help listing.
    Text(String),
    /// A configuration, rendered curly.
    Config(ConfigTree),
    /// A candidate marked up against running.
    ConfigDiff {
        candidate: ConfigTree,
        running: ConfigTree,
    },
    Changes(Vec<Change>),
    Revisions(Vec<RevisionInfo>),
    /// One `show interfaces ...`, and which of them it was.
    ///
    /// The snapshot is the same value the text renderer and the JSON
    /// serialiser both read, which is what makes `| display json` a modifier
    /// rather than a second implementation of the command.
    Interfaces {
        snapshot: Box<Snapshot>,
        view: View,
    },
}

/// How to render, decided once per command.
pub struct Style {
    pub schema: &'static Schema,
    pub colour: bool,
    pub secrets: bool,
}

impl Output {
    pub fn render(&self, style: &Style) -> String {
        match self {
            Output::Nothing => String::new(),
            Output::Text(text) => text.clone(),
            Output::Config(tree) => curly::render(&prepare(tree, style), style.schema),
            Output::ConfigDiff { candidate, running } => marked_up(candidate, running, style),
            Output::Changes(changes) => changes
                .iter()
                .map(|change| format!("{}\n", colour_change(change, style)))
                .collect(),
            Output::Revisions(revisions) => revisions_table(revisions),
            Output::Interfaces { snapshot, view } => {
                nightshade_ifstate::render(snapshot, view)
            }
        }
    }

    pub fn render_json(&self, style: &Style) -> String {
        let value = match self {
            Output::Nothing => serde_json::Value::Null,
            Output::Text(text) => serde_json::json!({ "message": text.trim_end() }),
            Output::Config(tree) => config_json(&prepare(tree, style)),
            Output::ConfigDiff { candidate, .. } => config_json(&prepare(candidate, style)),
            Output::Changes(changes) => serde_json::to_value(changes).unwrap_or_default(),
            Output::Revisions(revisions) => serde_json::to_value(revisions).unwrap_or_default(),
            Output::Interfaces { snapshot, .. } => {
                serde_json::to_value(snapshot).unwrap_or_default()
            }
        };
        let mut text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".into());
        text.push('\n');
        text
    }
}

fn prepare(tree: &ConfigTree, style: &Style) -> ConfigTree {
    if style.secrets {
        tree.clone()
    } else {
        mask_secrets(tree, style.schema)
    }
}

/// Replace the values of nodes the schema marks secret.
///
/// Masked by default and shown only when asked for, because `show` is what an
/// operator runs while somebody is looking over their shoulder or a session is
/// being recorded.
pub fn mask_secrets(tree: &ConfigTree, schema: &Schema) -> ConfigTree {
    fn walk(node: &Node, at: &Path, schema: &Schema) -> Node {
        let secret = matches!(
            schema.resolve(at),
            Some(Location::Node(schema_node)) if schema_node.secret
        );
        let body = match &node.body {
            Body::Values(values) if secret && !values.is_empty() => {
                Body::Values([MASK.to_string()].into_iter().collect())
            }
            Body::Values(values) => Body::Values(values.clone()),
            Body::Interior(children) => Body::Interior(
                children
                    .iter()
                    .map(|(name, child)| (name.clone(), walk(child, &at.child(name), schema)))
                    .collect(),
            ),
        };
        Node {
            comment: node.comment.clone(),
            body,
        }
    }

    let Some(children) = tree.root().children() else {
        return tree.clone();
    };
    ConfigTree::from_children(
        children
            .iter()
            .map(|(name, child)| {
                (
                    name.clone(),
                    walk(child, &Path::from_segments([name.clone()]), schema),
                )
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// the candidate, marked up against running
// ---------------------------------------------------------------------------

/// Render the candidate with `+`/`-` against running.
///
/// A line diff of the two renderings rather than a walk of the structured
/// diff. Both texts come from the same deterministic renderer, so the line
/// sequences line up, and the result is what an operator expects to see: the
/// configuration as it will be, with the changes marked in place.
fn marked_up(candidate: &ConfigTree, running: &ConfigTree, style: &Style) -> String {
    let before: Vec<String> = curly::render(&prepare(running, style), style.schema)
        .lines()
        .map(str::to_string)
        .collect();
    let after: Vec<String> = curly::render(&prepare(candidate, style), style.schema)
        .lines()
        .map(str::to_string)
        .collect();

    let mut out = String::new();
    for edit in line_diff(&before, &after) {
        let (marker, line, colour) = match edit {
            Edit::Same(line) => (' ', line, None),
            Edit::Added(line) => ('+', line, Some(GREEN)),
            Edit::Removed(line) => ('-', line, Some(RED)),
        };
        match (style.colour, colour) {
            (true, Some(code)) => out.push_str(&format!("{code}{marker}{line}{RESET}\n")),
            _ => out.push_str(&format!("{marker}{line}\n")),
        }
    }
    out
}

enum Edit<'a> {
    Same(&'a str),
    Added(&'a str),
    Removed(&'a str),
}

/// Longest common subsequence, then a walk back through it.
///
/// Quadratic, over a few hundred lines of configuration. A smarter diff would
/// be faster and would be the same answer.
fn line_diff<'a>(before: &'a [String], after: &'a [String]) -> Vec<Edit<'a>> {
    let (n, m) = (before.len(), after.len());
    let mut lengths = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lengths[i][j] = if before[i] == after[j] {
                lengths[i + 1][j + 1] + 1
            } else {
                lengths[i + 1][j].max(lengths[i][j + 1])
            };
        }
    }

    let mut edits = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if before[i] == after[j] {
            edits.push(Edit::Same(&after[j]));
            i += 1;
            j += 1;
        } else if lengths[i + 1][j] >= lengths[i][j + 1] {
            edits.push(Edit::Removed(&before[i]));
            i += 1;
        } else {
            edits.push(Edit::Added(&after[j]));
            j += 1;
        }
    }
    edits.extend(before[i..].iter().map(|line| Edit::Removed(line.as_str())));
    edits.extend(after[j..].iter().map(|line| Edit::Added(line.as_str())));
    edits
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// The config as JSON: objects for nodes, arrays for values.
///
/// Key-sorted, because the tree is. Distinct from the internal serialisation
/// configd writes under `/run`: this one is for people and for whatever reads
/// the API later, and drops the comments a config file carries.
fn config_json(tree: &ConfigTree) -> serde_json::Value {
    fn walk(node: &Node) -> serde_json::Value {
        match &node.body {
            Body::Values(values) => {
                serde_json::Value::Array(values.iter().map(|v| v.clone().into()).collect())
            }
            Body::Interior(children) => serde_json::Value::Object(
                children
                    .iter()
                    .map(|(name, child)| (name.clone(), walk(child)))
                    .collect(),
            ),
        }
    }
    walk(tree.root())
}

// ---------------------------------------------------------------------------
// tables
// ---------------------------------------------------------------------------

const RESET: &str = "\x1b[0m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";

fn colour_change(change: &Change, style: &Style) -> String {
    let text = change.to_string();
    if !style.colour {
        return text;
    }
    match change.op {
        Op::Add => format!("{GREEN}{text}{RESET}"),
        Op::Remove => format!("{RED}{text}{RESET}"),
    }
}

/// Column widths from the content, so nothing is truncated and nothing is
/// padded to a width somebody guessed.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }

    let mut out = String::new();
    let line = |out: &mut String, cells: &[String]| {
        let rendered: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let pad = widths[i].saturating_sub(cell.chars().count());
                format!("{cell}{}", " ".repeat(pad))
            })
            .collect();
        out.push_str(rendered.join("  ").trim_end());
        out.push('\n');
    };

    line(
        &mut out,
        &headers.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
    );
    line(
        &mut out,
        &widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>(),
    );
    for row in rows {
        line(&mut out, row);
    }
    out
}

fn revisions_table(revisions: &[RevisionInfo]) -> String {
    if revisions.is_empty() {
        return "no commits have been recorded on this system\n".to_string();
    }
    let rows: Vec<Vec<String>> = revisions
        .iter()
        .map(|revision| {
            vec![
                revision.revision.to_string(),
                readable(&revision.timestamp),
                revision.actor.clone(),
                revision.changes.len().to_string(),
                revision.comment.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    table(&["revision", "when", "by", "changes", "comment"], &rows)
}

/// `20260816T113045Z` is right for a filename and wrong for a person.
fn readable(stamp: &str) -> String {
    if stamp.len() != 16 {
        return stamp.to_string();
    }
    format!(
        "{}-{}-{} {}:{}:{} UTC",
        &stamp[0..4],
        &stamp[4..6],
        &stamp[6..8],
        &stamp[9..11],
        &stamp[11..13],
        &stamp[13..15],
    )
}

// ---------------------------------------------------------------------------
// terminal
// ---------------------------------------------------------------------------

/// Colour only when a person is going to see it.
///
/// `NO_COLOR` is honoured whatever else is true, because a variable somebody
/// set deliberately outranks a guess about their terminal.
pub fn use_colour() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Rows in the terminal, for paging. Twenty-four when there is no answer,
/// which is the size of the console this is most likely to be read on.
pub fn terminal_rows() -> usize {
    if !std::io::stdout().is_terminal() {
        return usize::MAX;
    }
    let mut size: nix::libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { nix::libc::ioctl(1, nix::libc::TIOCGWINSZ, &raw mut size) } == 0;
    if ok && size.ws_row > 2 {
        size.ws_row as usize
    } else {
        24
    }
}

/// Longest common prefix of a set of candidates, for tab completion.
pub fn common_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for candidate in &candidates[1..] {
        while !candidate.starts_with(&prefix) {
            prefix.pop();
            if prefix.is_empty() {
                return prefix;
            }
        }
    }
    prefix
}

/// Help lines for a set of completion candidates, as `?` prints them.
pub fn help_lines(entries: &[nightshade_schema::model::NodeInfo]) -> String {
    if entries.is_empty() {
        return "  <Enter>       run this command\n".to_string();
    }
    let width = entries
        .iter()
        .map(|entry| entry.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(12);

    let mut out = String::new();
    for entry in entries {
        let pad = width - entry.name.chars().count();
        out.push_str(&format!("  {}{}  {}", entry.name, " ".repeat(pad), entry.help));
        if let Some(default) = &entry.default {
            out.push_str(&format!(" (default: {default})"));
        }
        out.push('\n');
    }
    out
}

/// The values configured under a tag node, for completing an instance name.
pub fn configured(tree: &ConfigTree, at: &Path) -> Vec<String> {
    tree.get(at)
        .and_then(Node::children)
        .map(|children| children.keys().cloned().collect())
        .unwrap_or_default()
}

/// Interface names the kernel has, for completing a name that is not
/// configured yet.
pub fn system_interfaces() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    names
}


#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> Style {
        Style {
            schema: Schema::compiled(),
            colour: false,
            secrets: false,
        }
    }

    fn config(pairs: &[(&str, &str)]) -> ConfigTree {
        let schema = Schema::compiled();
        let mut tree = ConfigTree::new();
        for (path, value) in pairs {
            let path = Path::parse(path).unwrap();
            let value = (!value.is_empty()).then_some(*value);
            schema.apply_set(&mut tree, &path, value).unwrap();
        }
        tree
    }

    #[test]
    fn a_candidate_is_marked_up_against_running() {
        let running = config(&[
            ("system host-name", "before"),
            ("system time-zone", "UTC"),
        ]);
        let candidate = config(&[
            ("system host-name", "after"),
            ("system time-zone", "UTC"),
            ("system name-server", "1.1.1.1"),
        ]);

        let text = Output::ConfigDiff {
            candidate,
            running,
        }
        .render(&style());

        assert!(text.contains("-    host-name before"), "{text}");
        assert!(text.contains("+    host-name after"), "{text}");
        assert!(text.contains("+    name-server 1.1.1.1"), "{text}");
        // Unchanged lines are present and unmarked.
        assert!(text.contains("     time-zone UTC"), "{text}");
        assert!(text.contains(" system {"), "{text}");
    }

    #[test]
    fn an_unchanged_candidate_marks_nothing() {
        let tree = config(&[("system host-name", "fw")]);
        let text = Output::ConfigDiff {
            candidate: tree.clone(),
            running: tree,
        }
        .render(&style());
        // The marker is the first character of a line. `hostname` has a
        // hyphen in it and is not a change.
        for line in text.lines() {
            assert!(
                line.starts_with(' '),
                "{line:?} is marked as a change in an unchanged config"
            );
        }
    }

    #[test]
    fn json_is_objects_for_nodes_and_arrays_for_values() {
        let tree = config(&[
            ("system host-name", "fw"),
            ("system name-server", "1.1.1.1"),
            ("system name-server", "9.9.9.9"),
        ]);
        let json = Output::Config(tree).render_json(&style());
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["system"]["host-name"][0], "fw");
        assert_eq!(value["system"]["name-server"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn colour_is_off_when_it_is_off() {
        let running = config(&[("system host-name", "before")]);
        let candidate = config(&[("system host-name", "after")]);
        let plain = Output::ConfigDiff {
            candidate: candidate.clone(),
            running: running.clone(),
        }
        .render(&style());
        assert!(!plain.contains('\x1b'), "{plain:?}");

        let coloured = Output::ConfigDiff { candidate, running }.render(&Style {
            colour: true,
            ..style()
        });
        assert!(coloured.contains('\x1b'));
    }

    #[test]
    fn an_interface_report_renders_as_text_and_as_the_same_data_in_json() {
        use nightshade_ifstate::model::{Address, Interface, Kind, Oper};

        let mut eth0 = Interface::new("eth0", Kind::Ethernet);
        eth0.admin_up = true;
        eth0.oper = Oper::Up;
        eth0.link = nightshade_ifstate::model::Link::Connected;
        eth0.mac = Some("02:00:5e:10:00:01".into());
        eth0.mtu = Some(1500);
        eth0.description = Some("the uplink".into());
        eth0.addresses = vec![Address {
            prefix: "10.0.0.1/24".into(),
            broadcast: None,
        }];

        // The most useful row this command has: configured, and not there.
        let mut eth9 = Interface::new("eth9", Kind::Ethernet);
        eth9.present = false;
        eth9.oper = Oper::NotPresent;

        let output = Output::Interfaces {
            snapshot: Box::new(Snapshot {
                interfaces: vec![eth0, eth9],
                ..Snapshot::default()
            }),
            view: View::Detail,
        };

        let text = output.render(&style());
        assert!(text.contains("eth0 is up, line protocol is up (connected)"), "{text}");
        assert!(text.contains("Internet address is 10.0.0.1/24"), "{text}");
        assert!(text.contains("line protocol is notpresent"), "{text}");

        // The same value, structured. Not a second rendering of the text.
        let json: serde_json::Value =
            serde_json::from_str(&output.render_json(&style())).expect("valid JSON");
        assert_eq!(json["interfaces"][0]["name"], "eth0");
        assert_eq!(json["interfaces"][0]["mtu"], 1500);
        assert_eq!(json["interfaces"][1]["present"], false);
    }

    #[test]
    fn timestamps_are_rewritten_for_people() {
        assert_eq!(readable("20260816T113045Z"), "2026-08-16 11:30:45 UTC");
        assert_eq!(readable("nonsense"), "nonsense");
    }

    #[test]
    fn a_common_prefix_is_what_tab_fills_in() {
        assert_eq!(
            common_prefix(&["interfaces".into(), "interface-groups".into()]),
            "interface"
        );
        assert_eq!(common_prefix(&["system".into()]), "system");
        assert_eq!(common_prefix(&["a".into(), "b".into()]), "");
        assert_eq!(common_prefix(&[]), "");
    }
}
