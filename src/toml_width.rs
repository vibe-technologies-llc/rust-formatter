use unicode_width::UnicodeWidthChar;

/// Columns a tab advances by when nothing configures it.
pub const DEFAULT_TAB_WIDTH: usize = 4;

/// Column reached after rendering `ch` starting at `column`.
///
/// A tab is the only character whose cost depends on where it starts, which is
/// why every measurement in the formatter carries a column rather than a width.
pub fn advance_char(column: usize, ch: char, tab_width: usize) -> usize {
    if ch == '\t' {
        let unit = tab_width.max(1);
        return column + unit - column % unit;
    }
    column + ch.width().unwrap_or(0)
}

/// Column reached after rendering `text` starting at `column`.
pub fn advance(column: usize, text: &str, tab_width: usize) -> usize {
    text.chars()
        .fold(column, |column, ch| advance_char(column, ch, tab_width))
}

/// Columns `text` occupies on a line of its own.
pub fn width(text: &str, tab_width: usize) -> usize {
    advance(0, text, tab_width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_costs_one_column_each() {
        assert_eq!(width("abc", 4), 3);
        assert_eq!(width("", 4), 0);
    }

    #[test]
    fn wide_characters_cost_two_columns() {
        assert_eq!(width("版", 4), 2);
        assert_eq!(width("日本語", 4), 6);
        assert_eq!(width("🦀", 4), 2);
    }

    #[test]
    fn combining_marks_and_controls_cost_nothing() {
        assert_eq!(width("e\u{0301}", 4), 1);
        assert_eq!(width("\u{200b}", 4), 0);
        assert_eq!(width("\u{0007}", 4), 0);
    }

    #[test]
    fn a_precomposed_character_costs_one_column() {
        assert_eq!(width("ünïcödé", 4), 7);
    }

    #[test]
    fn a_tab_advances_to_the_next_stop() {
        assert_eq!(advance(0, "\t", 4), 4);
        assert_eq!(advance(1, "\t", 4), 4);
        assert_eq!(advance(3, "\t", 4), 4);
        assert_eq!(advance(4, "\t", 4), 8);
        assert_eq!(advance(0, "\t\t", 4), 8);
        assert_eq!(advance(0, "ab\tc", 4), 5);
    }

    #[test]
    fn a_tab_width_of_one_still_advances() {
        assert_eq!(advance(0, "\t", 1), 1);
        assert_eq!(advance(5, "\t", 1), 6);
    }

    #[test]
    fn a_zero_tab_width_cannot_stall() {
        assert_eq!(advance(3, "\t", 0), 4);
    }
}
