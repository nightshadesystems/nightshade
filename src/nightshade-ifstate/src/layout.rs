//! Columns, pads and sorting -- the whole of how a table is shaped.
//!
//! Every `show interfaces` table is built from a [`Layout`]: a list of column
//! widths and alignments. The widths are the ones EOS uses, so the output has
//! EOS's rhythm; the name column grows when a Linux interface name is longer
//! than the widest one EOS ever had to print, which is the one place the two
//! could not both be satisfied.
//!
//! Nothing here knows what an interface is. That is the point: a rendering
//! rule that lives in one module is a rendering rule that can be tested, and
//! twenty `format!` calls agreeing with each other by inspection is not a
//! test.

/// Which edge of its column a cell is pushed against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// A fixed set of column widths.
///
/// Widths include the gap to the next column, so a row is the cells
/// concatenated with nothing between them. The last column has no width: it
/// runs to the end of the line and the line is trimmed.
#[derive(Debug, Clone)]
pub struct Layout {
    columns: Vec<(usize, Align)>,
}

impl Layout {
    pub fn new(columns: &[(usize, Align)]) -> Self {
        Self {
            columns: columns.to_vec(),
        }
    }

    /// The same layout with column `index` widened to `width`.
    ///
    /// This is how the name column grows for `eth0.100` and `wg-office`
    /// without any of the others moving relative to each other.
    pub fn widen(mut self, index: usize, width: usize) -> Self {
        if let Some(column) = self.columns.get_mut(index) {
            column.0 = column.0.max(width);
        }
        self
    }

    pub fn width(&self, index: usize) -> usize {
        self.columns.get(index).map(|column| column.0).unwrap_or(0)
    }

    /// One row. Cells past the end of the layout are appended unpadded, which
    /// is what makes the final column free-width.
    pub fn row<S: AsRef<str>>(&self, cells: &[S]) -> String {
        let mut out = String::new();
        for (index, cell) in cells.iter().enumerate() {
            let cell = cell.as_ref();
            match self.columns.get(index) {
                Some((width, align)) => pad_into(&mut out, cell, *width, *align),
                None => out.push_str(cell),
            }
        }
        while out.ends_with(' ') {
            out.pop();
        }
        out
    }

    /// A row of dashes, one run per column, sized to the column rather than to
    /// its content.
    ///
    /// `runs` gives the length of each run; a zero leaves the column blank.
    pub fn rule(&self, runs: &[usize]) -> String {
        let cells: Vec<String> = runs.iter().map(|run| "-".repeat(*run)).collect();
        self.row(&cells)
    }
}

fn pad_into(out: &mut String, cell: &str, width: usize, align: Align) {
    let length = cell.chars().count();
    let pad = width.saturating_sub(length);
    match align {
        Align::Left => {
            out.push_str(cell);
            out.extend(std::iter::repeat_n(' ', pad));
        }
        Align::Right => {
            out.extend(std::iter::repeat_n(' ', pad));
            out.push_str(cell);
        }
    }
}

/// How wide the name column has to be to hold `names`.
///
/// `minimum` is the width EOS uses. Linux interface names are not four
/// characters and a digit -- `enp3s0f1np1` is a name a kernel really produces
/// -- so the column grows to fit the longest one plus a space, and every
/// column after it moves right by the same amount. The alternative is a table
/// whose second column is sometimes in the first column's last character.
pub fn name_width<S: AsRef<str>>(names: &[S], minimum: usize) -> usize {
    let longest = names
        .iter()
        .map(|name| name.as_ref().chars().count())
        .max()
        .unwrap_or(0);
    minimum.max(longest + 1)
}

/// A hard cut at `limit` characters. No ellipsis, no word boundary.
///
/// The `Name` column of `show interfaces status` is 26 characters of
/// description and EOS simply stops there. An ellipsis would cost one of the
/// characters that were worth keeping, and a word-boundary cut would make the
/// column's width depend on the text in it.
pub fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Greedy word wrap at `width`.
///
/// Used for the advertisement column of `show interfaces negotiation`, which
/// is a list of speeds that runs onto continuation lines under itself. A word
/// longer than the column is left long rather than broken: a speed name cut in
/// half is worse than a column that bulges.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Order two interface names the way a person counts.
///
/// `eth9` before `eth10`, which byte order gets wrong, and it gets it wrong in
/// the direction that matters: an eight-port box lists fine and a
/// forty-eight-port box lists `eth1, eth10, eth11, ...`, with `eth2` two
/// screens down.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let mut left = a.chars().peekable();
    let mut right = b.chars().peekable();

    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(l), Some(r)) => {
                if l.is_ascii_digit() && r.is_ascii_digit() {
                    let left_digits = take_digits(&mut left);
                    let right_digits = take_digits(&mut right);
                    // Compared as numbers, so `007` and `7` are the same run
                    // and `10` is bigger than `9`. Length first, because two
                    // digit strings with no leading zeroes compare by length
                    // before they compare by content.
                    let ordering = left_digits
                        .len()
                        .cmp(&right_digits.len())
                        .then_with(|| left_digits.cmp(&right_digits));
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                } else {
                    if l != r {
                        return l.cmp(&r);
                    }
                    left.next();
                    right.next();
                }
            }
        }
    }
}

fn take_digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut digits = String::new();
    while let Some(c) = chars.peek().copied() {
        if !c.is_ascii_digit() {
            break;
        }
        digits.push(c);
        chars.next();
    }
    // Leading zeroes carry no value, and keeping them would make the
    // length-first comparison below say `007 > 7`.
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Sort interface names naturally, in place.
pub fn sort_names(names: &mut [String]) {
    names.sort_by(|a, b| natural_cmp(a, b));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_is_its_columns_and_nothing_between_them() {
        let layout = Layout::new(&[(8, Align::Left), (10, Align::Right)]);
        assert_eq!(layout.row(&["eth0", "12"]), "eth0            12");
        //                                       ^0      ^8        ^18
        assert_eq!(layout.row(&["eth0", "12"]).len(), 18);
    }

    #[test]
    fn a_row_is_trimmed_rather_than_padded_to_the_last_column() {
        let layout = Layout::new(&[(8, Align::Left), (10, Align::Left)]);
        assert_eq!(layout.row(&["eth0", "up"]), "eth0    up");
        assert_eq!(layout.row(&["eth0"]), "eth0");
        assert_eq!(layout.row::<&str>(&[]), "");
    }

    /// A cell wider than its column pushes the rest of the line right rather
    /// than being cut. Truncation is a decision each column makes for itself;
    /// see [`truncate`].
    #[test]
    fn an_oversized_cell_is_not_silently_cut() {
        let layout = Layout::new(&[(4, Align::Left), (4, Align::Left)]);
        assert_eq!(layout.row(&["enp3s0f1", "up"]), "enp3s0f1up");
    }

    #[test]
    fn cells_past_the_layout_run_free() {
        let layout = Layout::new(&[(6, Align::Left)]);
        assert_eq!(
            layout.row(&["eth0", "a description with spaces"]),
            "eth0  a description with spaces"
        );
    }

    #[test]
    fn a_rule_is_dashes_in_the_shape_of_the_columns() {
        let layout = Layout::new(&[(11, Align::Left), (8, Align::Left), (7, Align::Right)]);
        assert_eq!(layout.rule(&[9, 5, 7]), "---------  -----   -------");
    }

    #[test]
    fn the_name_column_grows_for_names_the_column_was_not_built_for() {
        assert_eq!(name_width(&["eth0", "eth1"], 11), 11);
        assert_eq!(name_width(&["enp3s0f1np1"], 11), 12);
        assert_eq!(name_width::<&str>(&[], 11), 11);
        // Exactly at the minimum, a name still gets its separating space.
        assert_eq!(name_width(&["0123456789"], 11), 11);
        assert_eq!(name_width(&["01234567890"], 11), 12);
    }

    #[test]
    fn truncation_is_a_hard_cut() {
        assert_eq!(
            truncate("WAN uplink to ISP - Circuit ID 4471-A", 26),
            "WAN uplink to ISP - Circui"
        );
        assert_eq!(truncate("short", 26), "short");
        assert_eq!(truncate("", 26), "");
        assert_eq!(truncate("abc", 0), "");
    }

    /// Counted in characters, not bytes: a description is free text and an
    /// operator may well put a degree sign in it.
    #[test]
    fn truncation_counts_characters() {
        assert_eq!(truncate("°°°°°", 3), "°°°");
    }

    #[test]
    fn wrapping_fills_each_line_before_starting_the_next() {
        assert_eq!(
            wrap("10M/half 10M/full 100M/half 100M/full 1G/full", 20),
            ["10M/half 10M/full", "100M/half 100M/full", "1G/full"]
        );
        assert_eq!(wrap("", 20), Vec::<String>::new());
        assert_eq!(wrap("1G/full", 20), ["1G/full"]);
    }

    #[test]
    fn a_word_wider_than_the_column_is_left_whole() {
        assert_eq!(wrap("100000M/full 1G/full", 8), ["100000M/full", "1G/full"]);
    }

    #[test]
    fn names_sort_the_way_a_person_counts() {
        let mut names: Vec<String> = ["eth10", "eth9", "eth0", "eth1", "bond0", "lo", "vlan10"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        sort_names(&mut names);
        assert_eq!(names, ["bond0", "eth0", "eth1", "eth9", "eth10", "lo", "vlan10"]);
    }

    #[test]
    fn natural_order_handles_the_shapes_a_kernel_produces() {
        let mut names: Vec<String> = [
            "eth0.100", "eth0.20", "eth0", "enp3s0f1", "enp3s0f0", "wg0", "tun1", "tun0",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        sort_names(&mut names);
        assert_eq!(
            names,
            ["enp3s0f0", "enp3s0f1", "eth0", "eth0.20", "eth0.100", "tun0", "tun1", "wg0"]
        );
    }

    #[test]
    fn leading_zeroes_do_not_make_a_bigger_number() {
        assert_eq!(natural_cmp("eth007", "eth7"), std::cmp::Ordering::Equal);
        assert_eq!(natural_cmp("eth08", "eth9"), std::cmp::Ordering::Less);
    }

    #[test]
    fn a_prefix_sorts_before_what_extends_it() {
        assert_eq!(natural_cmp("eth", "eth0"), std::cmp::Ordering::Less);
        assert_eq!(natural_cmp("eth0", "eth"), std::cmp::Ordering::Greater);
        assert_eq!(natural_cmp("", ""), std::cmp::Ordering::Equal);
    }
}
