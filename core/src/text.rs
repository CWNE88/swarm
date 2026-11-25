//! Text that came from someone else, on its way into a line-based store
//! file or onto the screen. The stores are one record per line with
//! tab-separated fields, so a newline or tab inside a field would let
//! whoever wrote it forge extra records — a fake line in your message
//! history, a fake cached display name for someone else. Other control
//! characters (escape sequences) could rewrite the terminal.

/// Characters that reorder the text around them when shown: with them,
/// `photo<U+202E>gpj.exe` reads as `photoexe.jpg`, and a name can show
/// as anything. Treated like control characters.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn is_unwanted(c: char) -> bool {
    c.is_control() || is_bidi_control(c)
}

/// Whether `s` has no control characters at all (newline, tab, escape,
/// and the rest, text-direction overrides included) — what a signed
/// field has to satisfy to be stored as it is, since it can't be
/// altered without breaking its signature.
pub fn is_single_line(s: &str) -> bool {
    !s.chars().any(is_unwanted)
}

/// `s` with every control character replaced by a space — for unsigned
/// text (messages) that can be cleaned up rather than refused.
pub fn single_line(s: &str) -> String {
    s.chars().map(|c| if is_unwanted(c) { ' ' } else { c }).collect()
}

/// `512 B`, `1.4 MB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_are_found_and_replaced() {
        assert!(is_single_line("hello, world — ok"));
        assert!(!is_single_line("two\nlines"));
        assert!(!is_single_line("a\ttab"));
        assert!(!is_single_line("esc\x1b[2J"));
        assert_eq!(single_line("a\nb\tc\x1bd"), "a b c d");
        assert!(!is_single_line("Ann\u{202E}nimda"));
        assert_eq!(single_line("photo\u{202E}gpj.exe"), "photo gpj.exe");
    }
}
