//! What `show interfaces ...` was asking for.
//!
//! The words after `interfaces` become a [`Query`]: which interfaces, and
//! which view of them. Both halves cross the socket, because the view decides
//! what the daemon has to go and read -- an EEPROM page costs two ioctls per
//! module and is not collected for `show interfaces description`.
//!
//! Parsing lives here rather than in the CLI so that the daemon can be told
//! what was asked in the same words the operator used, and so a second
//! frontend cannot invent a third spelling of `notconnect`.

use serde::{Deserialize, Serialize};

use crate::model::Link;

/// Which of the `show interfaces` commands this is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum View {
    /// `show interfaces`, `show interfaces eth0`.
    Detail,
    Description,
    /// The optional word restricts the rows and leaves the header alone.
    Status(Option<Link>),
    Counters,
    CountersErrors,
    CountersDiscards,
    CountersRates,
    CountersQueue,
    CountersBins,
    Transceiver,
    TransceiverDetail,
    TransceiverProperties,
    TransceiverEeprom,
    Capabilities,
    FlowControl,
    Negotiation,
    NegotiationDetail,
    Phy,
    PhyDetail,
    Mac,
    MacDetail,
}

impl View {
    /// Whether answering this needs the module EEPROM read off the wire.
    ///
    /// Two ioctls and 512 bytes of i2c per module, which is why it is asked
    /// for rather than always collected.
    pub fn needs_eeprom(&self) -> bool {
        matches!(self, View::TransceiverEeprom)
    }

    /// Whether answering this needs `ETHTOOL_GSTATS`, which on some drivers
    /// means a few hundred string comparisons per port.
    pub fn needs_driver_stats(&self) -> bool {
        matches!(
            self,
            View::Detail
                | View::Counters
                | View::CountersErrors
                | View::CountersDiscards
                | View::CountersQueue
                | View::CountersBins
                | View::FlowControl
                | View::Mac
                | View::MacDetail
        )
    }
}

/// One `show interfaces` command, parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub view: View,
    /// Empty means every interface. Otherwise these names, in the order the
    /// operator gave them, already expanded from any ranges.
    pub names: Vec<String>,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            view: View::Detail,
            names: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("`{0}` is not something `show interfaces` can show")]
    UnknownView(String),

    #[error("`show interfaces {command}` does not take `{extra}`")]
    Extra { command: String, extra: String },

    #[error(
        "`show interfaces status {0}` is not a filter; \
         try connected, notconnect, disabled, errdisabled or inactive"
    )]
    UnknownFilter(String),

    #[error("`{0}` is not an interface name or a range of them")]
    BadName(String),

    #[error("`{0}` counts backwards; a range runs from the lower number to the higher")]
    BackwardsRange(String),

    #[error("`{0}` names more than {max} interfaces", max = MAX_RANGE)]
    HugeRange(String),
}

/// How many interfaces one range may name.
///
/// A bound rather than a limit anyone will meet: `eth0-4294967295` is a
/// request that would otherwise be answered by allocating for four billion
/// names, and this is a daemon running as root parsing something a client
/// sent.
pub const MAX_RANGE: usize = 1024;

/// Parse the words after `show interfaces`.
///
/// The interface names, when there are any, come first:
/// `show interfaces eth0-3 counters errors`. A first word that is one of the
/// view keywords is the view; anything else is taken as a name specification,
/// which is why no view is ever named after an interface.
pub fn parse(words: &[&str]) -> Result<Query, QueryError> {
    let (names, rest) = match words.split_first() {
        Some((first, rest)) if !is_view_keyword(first) => (expand(first)?, rest),
        _ => (Vec::new(), words),
    };

    let view = parse_view(rest)?;
    Ok(Query { view, names })
}

/// The words that begin a view rather than an interface name.
///
/// An interface really named `status` would be unreachable by name, which is
/// the trade every CLI with an optional positional argument makes. Nothing the
/// Nightshade schema will accept as an interface name is on this list.
fn is_view_keyword(word: &str) -> bool {
    matches!(
        word,
        "description"
            | "status"
            | "counters"
            | "transceiver"
            | "capabilities"
            | "flowcontrol"
            | "negotiation"
            | "phy"
            | "mac"
    )
}

fn parse_view(words: &[&str]) -> Result<View, QueryError> {
    match words {
        [] => Ok(View::Detail),

        ["description"] => Ok(View::Description),

        ["status"] => Ok(View::Status(None)),
        ["status", filter] => Link::parse(filter)
            .map(|link| View::Status(Some(link)))
            .ok_or_else(|| QueryError::UnknownFilter((*filter).to_string())),

        ["counters"] => Ok(View::Counters),
        ["counters", "errors"] => Ok(View::CountersErrors),
        ["counters", "discards"] => Ok(View::CountersDiscards),
        ["counters", "rates"] => Ok(View::CountersRates),
        ["counters", "queue"] => Ok(View::CountersQueue),
        ["counters", "bins"] => Ok(View::CountersBins),

        ["transceiver"] => Ok(View::Transceiver),
        ["transceiver", "detail"] => Ok(View::TransceiverDetail),
        ["transceiver", "properties"] => Ok(View::TransceiverProperties),
        ["transceiver", "eeprom"] => Ok(View::TransceiverEeprom),

        ["capabilities"] => Ok(View::Capabilities),
        ["flowcontrol"] => Ok(View::FlowControl),

        ["negotiation"] => Ok(View::Negotiation),
        ["negotiation", "detail"] => Ok(View::NegotiationDetail),

        ["phy"] => Ok(View::Phy),
        ["phy", "detail"] => Ok(View::PhyDetail),

        ["mac"] => Ok(View::Mac),
        ["mac", "detail"] => Ok(View::MacDetail),

        // A known command with an unknown word after it, so the message can
        // name the command rather than only the word.
        [command, extra, ..] if is_view_keyword(command) => Err(QueryError::Extra {
            command: (*command).to_string(),
            extra: (*extra).to_string(),
        }),

        [other, ..] => Err(QueryError::UnknownView((*other).to_string())),
    }
}

/// Expand an interface specification into names.
///
/// `eth0` is one. `eth0-3` and `eth0-eth3` are four. `eth0,eth4` is two, and
/// the parts of a comma list may themselves be ranges.
pub fn expand(spec: &str) -> Result<Vec<String>, QueryError> {
    let mut names = Vec::new();
    for part in spec.split(',') {
        expand_one(part, &mut names)?;
    }
    if names.is_empty() {
        return Err(QueryError::BadName(spec.to_string()));
    }
    Ok(names)
}

fn expand_one(part: &str, out: &mut Vec<String>) -> Result<(), QueryError> {
    if part.is_empty() {
        return Err(QueryError::BadName(part.to_string()));
    }

    let Some((head, tail)) = part.rsplit_once('-') else {
        out.push(part.to_string());
        return Ok(());
    };

    // `eth0-3`: a name ending in digits, a hyphen, and digits. Anything else
    // -- `wg-office`, `br-lan` -- is a name that happens to contain a hyphen
    // and is passed through whole.
    let Some((prefix, first)) = split_trailing_number(head) else {
        out.push(part.to_string());
        return Ok(());
    };

    // `eth0-eth3` as well as `eth0-3`, because both are typed.
    let last = match split_trailing_number(tail) {
        Some((tail_prefix, last)) if tail_prefix.is_empty() || tail_prefix == prefix => last,
        Some(_) => return Err(QueryError::BadName(part.to_string())),
        None => {
            out.push(part.to_string());
            return Ok(());
        }
    };

    if last < first {
        return Err(QueryError::BackwardsRange(part.to_string()));
    }
    // Checked before anything is allocated, on the count rather than after
    // producing it.
    if (last - first) as usize + 1 > MAX_RANGE {
        return Err(QueryError::HugeRange(part.to_string()));
    }
    for number in first..=last {
        out.push(format!("{prefix}{number}"));
    }
    Ok(())
}

/// Split `eth10` into `("eth", 10)`. `None` when there is no trailing number,
/// or when it does not fit a `u32`.
fn split_trailing_number(text: &str) -> Option<(&str, u32)> {
    let digits_at = text
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_digit())
        .map(|(index, _)| index)
        .last()?;
    let (prefix, digits) = text.split_at(digits_at);
    digits.parse().ok().map(|number| (prefix, number))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(line: &str) -> View {
        let words: Vec<&str> = line.split_whitespace().collect();
        parse(&words).unwrap().view
    }

    fn names(line: &str) -> Vec<String> {
        let words: Vec<&str> = line.split_whitespace().collect();
        parse(&words).unwrap().names
    }

    #[test]
    fn every_command_in_the_tree_parses() {
        assert_eq!(view(""), View::Detail);
        assert_eq!(view("description"), View::Description);
        assert_eq!(view("status"), View::Status(None));
        assert_eq!(view("status connected"), View::Status(Some(Link::Connected)));
        assert_eq!(
            view("status errdisabled"),
            View::Status(Some(Link::ErrDisabled))
        );
        assert_eq!(view("counters"), View::Counters);
        assert_eq!(view("counters errors"), View::CountersErrors);
        assert_eq!(view("counters discards"), View::CountersDiscards);
        assert_eq!(view("counters rates"), View::CountersRates);
        assert_eq!(view("counters queue"), View::CountersQueue);
        assert_eq!(view("counters bins"), View::CountersBins);
        assert_eq!(view("transceiver"), View::Transceiver);
        assert_eq!(view("transceiver detail"), View::TransceiverDetail);
        assert_eq!(view("transceiver properties"), View::TransceiverProperties);
        assert_eq!(view("transceiver eeprom"), View::TransceiverEeprom);
        assert_eq!(view("capabilities"), View::Capabilities);
        assert_eq!(view("flowcontrol"), View::FlowControl);
        assert_eq!(view("negotiation"), View::Negotiation);
        assert_eq!(view("negotiation detail"), View::NegotiationDetail);
        assert_eq!(view("phy"), View::Phy);
        assert_eq!(view("phy detail"), View::PhyDetail);
        assert_eq!(view("mac"), View::Mac);
        assert_eq!(view("mac detail"), View::MacDetail);
    }

    #[test]
    fn a_name_comes_before_the_view() {
        assert_eq!(names("eth0"), ["eth0"]);
        assert_eq!(view("eth0"), View::Detail);
        assert_eq!(names("eth0 counters errors"), ["eth0"]);
        assert_eq!(view("eth0 counters errors"), View::CountersErrors);
        assert_eq!(view("eth0-3 status"), View::Status(None));
        assert!(names("counters errors").is_empty());
    }

    #[test]
    fn ranges_expand_the_way_they_are_typed() {
        assert_eq!(names("eth0-3 status"), ["eth0", "eth1", "eth2", "eth3"]);
        assert_eq!(names("eth0-eth2"), ["eth0", "eth1", "eth2"]);
        assert_eq!(names("eth0,eth4"), ["eth0", "eth4"]);
        assert_eq!(
            names("eth0-1,bond0,vlan10"),
            ["eth0", "eth1", "bond0", "vlan10"]
        );
        assert_eq!(names("eth7-7"), ["eth7"]);
    }

    /// A hyphen is a legal character in an interface name, and `wg-office` is
    /// a name somebody will use. Only a hyphen between two numbers is a range.
    #[test]
    fn a_name_with_a_hyphen_in_it_is_still_a_name() {
        assert_eq!(names("wg-office"), ["wg-office"]);
        assert_eq!(names("br-lan"), ["br-lan"]);
        assert_eq!(names("eth0.100"), ["eth0.100"]);
    }

    #[test]
    fn a_backwards_or_enormous_range_is_refused() {
        assert_eq!(
            expand("eth3-0"),
            Err(QueryError::BackwardsRange("eth3-0".into()))
        );
        assert!(matches!(
            expand("eth0-4294967295"),
            Err(QueryError::HugeRange(_))
        ));
        // The bound is on the count, so a range of exactly the maximum is
        // still refused and nothing is allocated for it.
        assert!(expand("eth0-1024").is_err());
        assert_eq!(expand("eth0-1023").unwrap().len(), MAX_RANGE);
    }

    #[test]
    fn a_range_across_two_different_names_is_refused() {
        assert_eq!(
            expand("eth0-bond3"),
            Err(QueryError::BadName("eth0-bond3".into()))
        );
    }

    #[test]
    fn an_unknown_command_says_which_word_was_wrong() {
        let words = ["counters", "nonsense"];
        assert_eq!(
            parse(&words),
            Err(QueryError::Extra {
                command: "counters".into(),
                extra: "nonsense".into()
            })
        );

        let words = ["status", "nonsense"];
        assert_eq!(parse(&words), Err(QueryError::UnknownFilter("nonsense".into())));

        // A first word that is not a keyword is a name, so the error is about
        // the word after it.
        let words = ["eth0", "nonsense"];
        assert_eq!(parse(&words), Err(QueryError::UnknownView("nonsense".into())));
    }

    #[test]
    fn what_a_view_needs_collected_is_what_it_prints() {
        assert!(View::TransceiverEeprom.needs_eeprom());
        assert!(!View::Transceiver.needs_eeprom());
        assert!(View::CountersBins.needs_driver_stats());
        assert!(!View::Description.needs_driver_stats());
    }
}
