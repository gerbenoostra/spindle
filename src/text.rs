//! Provider text made safe for the terminal: escaping, cell widths and
//! the bounded excerpt the store persists for a conversation.

use unicode_width::UnicodeWidthChar;

/// Provider text made printable on one line: control characters become
/// visible escapes so a prompt or reason cannot paint over the pane.
pub(crate) fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The escaped-cell bound a persisted prompt excerpt renders within,
/// ellipsis included when the excerpt truncated.
pub(crate) const PROMPT_EXCERPT_CELLS: usize = 120;

/// The display cells one character occupies once escaped: a control
/// character's escape sequence is wider than the character itself.
fn escaped_width(c: char) -> usize {
    match c {
        '\n' | '\r' | '\t' => 2,
        c if c.is_control() => format!("\\u{:04x}", c as u32).len(),
        c => UnicodeWidthChar::width(c).unwrap_or(0),
    }
}

/// The longest raw prefix of `text` whose escaped rendering fits `cells`.
fn raw_prefix_within(text: &str, cells: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = escaped_width(c);
        if used + w > cells {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

/// The prompt excerpt persisted for a conversation: the raw text when its
/// escaped rendering fits [`PROMPT_EXCERPT_CELLS`], else the longest raw
/// prefix whose escape fits one cell less, closed by a one-cell ellipsis.
/// The prefix never splits a character, and the stored text stays raw -
/// escaping happens once, at render. `None` for an empty or
/// whitespace-only prompt: text that renders nothing is not context, and
/// the full prompt is never persisted.
pub(crate) fn prompt_excerpt(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let whole = raw_prefix_within(text, PROMPT_EXCERPT_CELLS);
    if whole == text {
        return Some(whole);
    }
    let mut out = raw_prefix_within(text, PROMPT_EXCERPT_CELLS - 1);
    out.push('…');
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// The escaped cell count a stored excerpt renders into - the
    /// contract the bound keeps.
    fn rendered(text: &str) -> usize {
        UnicodeWidthStr::width(escape_text(text).as_str())
    }

    #[test]
    fn escape_text_makes_controls_visible() {
        assert_eq!(escape_text("a\nb\tc\rd"), "a\\nb\\tc\\rd");
        assert_eq!(escape_text("\u{7}\u{1b}"), "\\u0007\\u001b");
        assert_eq!(escape_text("plain 日本語"), "plain 日本語");
    }

    #[test]
    fn prompt_excerpt_bounds_by_escaped_cells() {
        // Whole when the escape fits exactly the bound.
        let exact = "a".repeat(PROMPT_EXCERPT_CELLS);
        assert_eq!(prompt_excerpt(&exact).as_deref(), Some(exact.as_str()));
        // A control character's escape counts, not the character.
        let controls = "x".repeat(PROMPT_EXCERPT_CELLS - 2) + "\n";
        assert_eq!(
            prompt_excerpt(&controls).as_deref(),
            Some(controls.as_str()),
            "the \\n escape is two cells and lands exactly on the bound"
        );
        // One cell over truncates: the prefix fits one less and closes
        // with the ellipsis.
        let over = "a".repeat(PROMPT_EXCERPT_CELLS + 1);
        let excerpt = prompt_excerpt(&over).expect("excerpt");
        assert!(excerpt.ends_with('…'));
        assert_eq!(rendered(&excerpt), PROMPT_EXCERPT_CELLS);
        assert_eq!(excerpt.chars().count(), PROMPT_EXCERPT_CELLS);
    }

    #[test]
    fn prompt_excerpt_never_splits_a_character() {
        // CJK takes two cells: a 119-cell prefix plus ellipsis, never a
        // half character.
        let cjk = "日".repeat(80);
        let excerpt = prompt_excerpt(&cjk).expect("excerpt");
        assert_eq!(excerpt.chars().count(), 60, "{excerpt}");
        assert_eq!(rendered(&excerpt), 119);
        assert!(excerpt.ends_with('…'));
        // A combining sequence stays whole: the mark adds no cell.
        let combined = format!("{}\u{301}", "e".repeat(200));
        let excerpt = prompt_excerpt(&combined).expect("excerpt");
        assert!(rendered(&excerpt) <= PROMPT_EXCERPT_CELLS);
        // A newline near the boundary cannot straddle it.
        let boundary = format!("{}\n{}", "a".repeat(119), "b".repeat(10));
        let excerpt = prompt_excerpt(&boundary).expect("excerpt");
        assert_eq!(rendered(&excerpt), PROMPT_EXCERPT_CELLS, "{excerpt}");
        // A control with a longer escape still counts and bounds.
        let del = format!("{}\u{7f}", "z".repeat(PROMPT_EXCERPT_CELLS));
        let excerpt = prompt_excerpt(&del).expect("excerpt");
        assert!(rendered(&excerpt) <= PROMPT_EXCERPT_CELLS);
        assert!(excerpt.ends_with('…'));
        // A zero-width non-BMP format character is not a control: it
        // passes through raw and never widens the excerpt.
        let zero = format!("{}\u{1d173}", "z".repeat(PROMPT_EXCERPT_CELLS));
        assert_eq!(prompt_excerpt(&zero).as_deref(), Some(zero.as_str()));
    }

    #[test]
    fn prompt_excerpt_stays_raw_and_rejects_the_empty() {
        // Stored raw - a tab stays a tab; escaping happens once at render.
        let raw = "line one\nline\twith tab";
        assert_eq!(prompt_excerpt(raw).as_deref(), Some(raw));
        for empty in ["", "   ", "\n\t ", " \u{3000} "] {
            assert_eq!(prompt_excerpt(empty), None, "{empty:?}");
        }
    }
}
