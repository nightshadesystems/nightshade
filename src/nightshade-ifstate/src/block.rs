//! The builder every indented detail block is written with.
//!
//! `show interfaces`, `... transceiver detail`, `... phy detail` and the rest
//! are all the same shape: a heading in column zero, sections indented under
//! it, and fields whose values line up at a column the section decides. Doing
//! that with a `format!` per line works right up until one of them is indented
//! three spaces instead of two and nobody notices for a release.
//!
//! So the indent and the value column are arguments, the padding is computed
//! here, and a line that would run past its value column pushes the value one
//! space right rather than colliding with it.

/// An indented block under way.
#[derive(Debug, Default)]
pub struct Block {
    out: String,
}

impl Block {
    pub fn new() -> Self {
        Self::default()
    }

    /// The interface name, in column zero.
    pub fn heading(&mut self, text: &str) -> &mut Self {
        self.raw(0, text)
    }

    /// A section heading, or any line that is text rather than a field.
    pub fn raw(&mut self, indent: usize, text: &str) -> &mut Self {
        self.out.extend(std::iter::repeat_n(' ', indent));
        self.out.push_str(text);
        self.end_line()
    }

    /// End the line under construction, with no trailing whitespace on it.
    ///
    /// Enforced here rather than remembered at each call site. A field whose
    /// value turned out to be empty leaves a `Label: ` with a space after it,
    /// which is invisible on a terminal and shows up in every diff of two
    /// support bundles.
    fn end_line(&mut self) -> &mut Self {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        self.out.push('\n');
        self
    }

    /// `  Label: value` -- a field whose value follows its label directly.
    pub fn field(&mut self, indent: usize, label: &str, value: &str) -> &mut Self {
        self.raw(indent, &format!("{label}: {value}"))
    }

    /// A field whose value starts at absolute column `value_column`.
    ///
    /// This is the two-column layout the PHY, MAC and capabilities sections
    /// use. `indent` and the column are absolute so a nested row -- the
    /// `Last change` under `PHY state changes` -- lines its value up with the
    /// rows above it rather than with its own indent.
    pub fn aligned(
        &mut self,
        indent: usize,
        label: &str,
        value: &str,
        value_column: usize,
    ) -> &mut Self {
        let used = indent + label.chars().count();
        // At least one space, always. A label that runs past the column moves
        // its own value rather than being run into by it.
        let pad = value_column.saturating_sub(used).max(1);
        self.out.extend(std::iter::repeat_n(' ', indent));
        self.out.push_str(label);
        self.out.extend(std::iter::repeat_n(' ', pad));
        self.out.push_str(value);
        self.end_line()
    }

    /// A label at `indent` and a right-aligned value ending at `end_column`.
    ///
    /// The frame-size bins, whose labels are ragged and whose counts are a
    /// column of digits.
    pub fn ragged(
        &mut self,
        indent: usize,
        label: &str,
        label_width: usize,
        value: &str,
        end_column: usize,
    ) -> &mut Self {
        let start = indent + label_width;
        let pad_label = label_width.saturating_sub(label.chars().count());
        let pad_value = end_column
            .saturating_sub(start)
            .saturating_sub(value.chars().count());
        self.out.extend(std::iter::repeat_n(' ', indent));
        self.out.push_str(label);
        self.out.extend(std::iter::repeat_n(' ', pad_label + pad_value));
        self.out.push_str(value);
        self.end_line()
    }

    /// An optional field.
    ///
    /// An empty value counts as absent. A description set to the empty string
    /// is a description nobody wrote, and a `Description:` with nothing after
    /// it is a line that says less than no line at all.
    pub fn maybe(&mut self, indent: usize, label: &str, value: Option<&str>) -> &mut Self {
        match value.filter(|value| !value.is_empty()) {
            Some(value) => self.field(indent, label, value),
            None => self,
        }
    }

    /// An optional field in the two-column layout.
    pub fn maybe_aligned(
        &mut self,
        indent: usize,
        label: &str,
        value: Option<String>,
        value_column: usize,
    ) -> &mut Self {
        match value.filter(|value| !value.is_empty()) {
            Some(value) => self.aligned(indent, label, &value, value_column),
            None => self,
        }
    }

    pub fn blank(&mut self) -> &mut Self {
        self.out.push('\n');
        self
    }

    pub fn is_empty(&self) -> bool {
        self.out.is_empty()
    }

    /// Everything written so far, with the trailing blank lines removed and
    /// exactly one newline at the end.
    ///
    /// Blocks are written with a blank line after each interface, which leaves
    /// one at the end that nothing follows. Trimming it here means no caller
    /// has to remember whether it is the last one.
    pub fn finish(mut self) -> String {
        while self.out.ends_with("\n\n") {
            self.out.pop();
        }
        self.out
    }

    /// Everything written so far, exactly.
    pub fn take(self) -> String {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_follows_its_label() {
        let mut block = Block::new();
        block.heading("eth0").field(2, "Description", "the uplink");
        assert_eq!(block.take(), "eth0\n  Description: the uplink\n");
    }

    #[test]
    fn an_aligned_field_starts_its_value_at_the_column_it_was_given() {
        let mut block = Block::new();
        block.aligned(4, "PHY state", "linkUp", 45);
        let line = block.take();
        assert_eq!(line.find("linkUp"), Some(45));
        assert_eq!(line.trim_end().len(), 51);
    }

    /// A long label may not be run into by its own value.
    #[test]
    fn a_label_past_the_value_column_pushes_its_value_right() {
        let mut block = Block::new();
        block.aligned(2, "a label longer than the column", "value", 10);
        assert_eq!(block.take(), "  a label longer than the column value\n");
    }

    #[test]
    fn a_ragged_field_right_aligns_its_value() {
        let mut block = Block::new();
        block.ragged(4, "64 bytes:", 22, "412334981", 38);
        let line = block.take();
        assert_eq!(line.trim_end().chars().count(), 38);
        assert_eq!(line.find("412334981"), Some(29));
    }

    #[test]
    fn an_absent_field_prints_nothing() {
        let mut block = Block::new();
        block.maybe(2, "Description", None).maybe(2, "Model", Some("NS-1"));
        assert_eq!(block.take(), "  Model: NS-1\n");
    }

    #[test]
    fn finishing_leaves_one_newline_and_no_trailing_blank_lines() {
        let mut block = Block::new();
        block.heading("eth0").blank().blank();
        assert_eq!(block.finish(), "eth0\n");
        assert_eq!(Block::new().finish(), "");
    }
}
