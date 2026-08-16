//! Tab completion and `?` help.
//!
//! Both answer the same question -- what can go here -- so both come from the
//! same function. Three sources, merged:
//!
//! - the **schema**, for node names and for the `<interface>`-shaped hint
//!   where an operator invents the next word
//! - the **candidate configuration**, so `delete interfaces ethernet <Tab>`
//!   offers the interfaces that are actually configured
//! - **`/sys/class/net`**, so `set interfaces ethernet <Tab>` offers the ports
//!   the box has, which is what somebody configuring a new one needs
//!
//! Reading the schema here is metadata, not judgement. Nothing in this file
//! decides whether a value is legal; that answer comes from configd, once,
//! where it cannot be skipped by a client that is not this one.
//!
//! # Why the candidate is a snapshot
//!
//! It is fetched once per prompt rather than per keystroke. A round trip over
//! a unix socket is fast, but a completer that talks to a daemon while the
//! operator is mid-word is a completer that can hang the terminal if the
//! daemon is busy.

use std::sync::{Arc, Mutex};

use nightshade_schema::config::ConfigTree;
use nightshade_schema::model::{Location, NodeInfo, Schema};
use nightshade_schema::path::Path;
use reedline::{Completer, CompletionResult, Span, Suggestion};

/// Commands that take a configuration path, and so complete against the
/// schema. Everything else completes against its own word list.
const PATH_COMMANDS: &[&str] = &["set", "delete", "show", "edit"];

/// What the completer needs to know, refreshed by the REPL before each prompt.
#[derive(Default)]
pub struct Context {
    pub candidate: ConfigTree,
    pub system_interfaces: Vec<String>,
    /// The prefix `edit` put in front of everything typed.
    pub prefix: Path,
    pub commands: Vec<&'static str>,
}

pub struct NsCompleter {
    schema: &'static Schema,
    context: Arc<Mutex<Context>>,
}

impl NsCompleter {
    pub fn new(schema: &'static Schema, context: Arc<Mutex<Context>>) -> Self {
        Self { schema, context }
    }
}

/// What can follow the words already typed.
///
/// `typed` is the whole line split into words; `partial` says whether the last
/// word is still being typed (no trailing space) or finished.
pub fn candidates(
    schema: &'static Schema,
    context: &Context,
    typed: &[String],
    partial: bool,
) -> Vec<NodeInfo> {
    let (complete, being_typed) = if partial {
        typed.split_last().map_or((typed, ""), |(last, rest)| (rest, last.as_str()))
    } else {
        (typed, "")
    };

    // The first word is a command, not a path.
    let Some((command, path_words)) = complete.split_first() else {
        return commands(context, being_typed);
    };
    if !PATH_COMMANDS.contains(&command.as_str()) {
        return Vec::new();
    }

    let mut path = context.prefix.clone();
    for word in path_words {
        path.push(word.clone());
    }

    let mut entries = schema.children_of(&path);

    // Where the schema says the operator invents a word, replace the
    // placeholder with the names that actually exist.
    if entries.len() == 1
        && entries[0].placeholder
        && let Some(Location::Node(node)) = schema.resolve(&path)
        && node.is_tag()
    {
        let hint = entries.remove(0);
        entries = instance_names(context, &path, &hint);
    }

    entries
        .into_iter()
        .filter(|entry| entry.placeholder || entry.name.starts_with(being_typed))
        .collect()
}

/// Real instance names, plus the schema's hint when there are none to offer.
fn instance_names(context: &Context, at: &Path, hint: &NodeInfo) -> Vec<NodeInfo> {
    let mut names: Vec<String> = crate::output::configured(&context.candidate, at);

    // For an interface tag node, the ports the kernel has are worth offering
    // too -- somebody configuring a new one has not configured it yet, so the
    // candidate cannot help them.
    if at.starts_with(&Path::from_segments(["interfaces"])) {
        for name in &context.system_interfaces {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    names.sort();
    names.dedup();

    if names.is_empty() {
        return vec![hint.clone()];
    }
    names
        .into_iter()
        .map(|name| NodeInfo {
            name,
            help: hint.help.clone(),
            placeholder: false,
            value: None,
            multi: false,
            default: None,
            secret: false,
        })
        .collect()
}

fn commands(context: &Context, being_typed: &str) -> Vec<NodeInfo> {
    context
        .commands
        .iter()
        .filter(|name| name.starts_with(being_typed))
        .map(|name| NodeInfo {
            name: (*name).to_string(),
            help: String::new(),
            placeholder: false,
            value: None,
            multi: false,
            default: None,
            secret: false,
        })
        .collect()
}

/// Split a line into words, and say whether the last one is unfinished.
pub fn words(line: &str) -> (Vec<String>, bool) {
    let partial = !line.is_empty() && !line.ends_with(char::is_whitespace);
    let words = Path::parse(line)
        .map(|path| path.segments().to_vec())
        .unwrap_or_else(|_| line.split_whitespace().map(str::to_string).collect());
    (words, partial)
}

impl Completer for NsCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        CompletionResult::fresh(self.suggestions(line, pos))
    }
}

impl NsCompleter {
    fn suggestions(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let line = &line[..pos.min(line.len())];
        let (typed, partial) = words(line);

        let context = match self.context.lock() {
            Ok(context) => context,
            // A poisoned lock means another thread panicked. Offering no
            // completions is the right answer; taking the terminal down with
            // it is not.
            Err(_) => return Vec::new(),
        };

        // How much of the line the suggestion replaces.
        let start = if partial {
            line.rfind(char::is_whitespace).map_or(0, |i| i + 1)
        } else {
            line.len()
        };

        candidates(self.schema, &context, &typed, partial)
            .into_iter()
            // A placeholder describes what to type; it is not something to
            // type, so completing to it would insert `<interface>`.
            .filter(|entry| !entry.placeholder)
            .map(|entry| Suggestion {
                value: entry.name,
                display_override: None,
                description: (!entry.help.is_empty()).then_some(entry.help),
                style: None,
                extra: None,
                span: Span::new(start, line.len()),
                append_whitespace: true,
                match_indices: None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(pairs: &[(&str, &str)]) -> Context {
        let schema = Schema::compiled();
        let mut candidate = ConfigTree::new();
        for (path, value) in pairs {
            let path = Path::parse(path).unwrap();
            let value = (!value.is_empty()).then_some(*value);
            schema.apply_set(&mut candidate, &path, value).unwrap();
        }
        Context {
            candidate,
            system_interfaces: vec!["eth0".into(), "eth1".into(), "lo".into()],
            prefix: Path::root(),
            commands: vec!["set", "delete", "show", "commit", "compare"],
        }
    }

    fn names(line: &str) -> Vec<String> {
        let (typed, partial) = words(line);
        candidates(Schema::compiled(), &context(&[]), &typed, partial)
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    }

    #[test]
    fn a_partial_command_narrows_to_it() {
        assert_eq!(names("se"), ["set"]);
        assert_eq!(names("c"), ["commit", "compare"]);
    }

    /// The acceptance case: `set inter<Tab>` gives `interfaces`.
    #[test]
    fn a_partial_path_narrows_to_it() {
        assert_eq!(names("set inter"), ["interfaces"]);
        assert_eq!(names("set sys"), ["system"]);
    }

    #[test]
    fn a_finished_word_offers_what_comes_next() {
        assert_eq!(names("set "), ["interfaces", "system"]);
        assert_eq!(
            names("set system "),
            ["host-name", "name-server", "time-zone"]
        );
    }

    #[test]
    fn a_tag_node_offers_what_is_configured_and_what_the_box_has() {
        let context = context(&[("interfaces ethernet eth7 mtu", "9000")]);
        let (typed, partial) = words("set interfaces ethernet ");
        let offered: Vec<String> = candidates(Schema::compiled(), &context, &typed, partial)
            .into_iter()
            .map(|entry| entry.name)
            .collect();

        // Configured but not present, and present but not configured.
        assert!(offered.contains(&"eth7".to_string()), "{offered:?}");
        assert!(offered.contains(&"eth0".to_string()), "{offered:?}");
    }

    /// With nothing to offer, the schema's hint is what `?` shows.
    #[test]
    fn a_tag_node_with_nothing_configured_falls_back_to_the_hint() {
        let context = Context {
            system_interfaces: Vec::new(),
            ..context(&[])
        };
        let (typed, partial) = words("set interfaces vxlan ");
        let offered = candidates(Schema::compiled(), &context, &typed, partial);
        assert_eq!(offered.len(), 1);
        assert!(offered[0].placeholder);
        assert!(offered[0].name.contains("interface"), "{}", offered[0].name);
    }

    #[test]
    fn inside_an_instance_the_leaves_come_back() {
        assert_eq!(
            names("set interfaces ethernet eth0 "),
            ["address", "description", "disable", "duplex", "mac", "mtu", "speed"]
        );
        assert_eq!(names("set interfaces ethernet eth0 mt"), ["mtu"]);
    }

    #[test]
    fn a_leaf_offers_the_shape_of_its_value() {
        let (typed, partial) = words("set interfaces ethernet eth0 mtu ");
        let offered = candidates(Schema::compiled(), &context(&[]), &typed, partial);
        assert_eq!(offered.len(), 1);
        assert!(offered[0].placeholder);
        assert_eq!(offered[0].name, "<68-9216>");
        assert_eq!(offered[0].default.as_deref(), Some("1500"));
    }

    #[test]
    fn an_edit_prefix_is_prepended_to_everything() {
        let context = Context {
            prefix: Path::parse("interfaces ethernet eth0").unwrap(),
            ..context(&[])
        };
        let (typed, partial) = words("set mt");
        let offered: Vec<String> = candidates(Schema::compiled(), &context, &typed, partial)
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(offered, ["mtu"]);
    }

    #[test]
    fn a_command_that_takes_no_path_offers_nothing() {
        assert!(names("commit ").is_empty());
        assert!(names("compare ").is_empty());
    }

    #[test]
    fn nonsense_paths_offer_nothing_rather_than_failing() {
        assert!(names("set nonsense ").is_empty());
        assert!(names("set interfaces ethernet eth0 mtu 9000 ").is_empty());
    }

    #[test]
    fn quoted_words_are_one_word() {
        let (typed, partial) = words(r#"set interfaces ethernet eth0 description "two words""#);
        assert_eq!(typed.len(), 6);
        assert_eq!(typed[5], "two words");
        assert!(partial);
    }
}
