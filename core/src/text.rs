//! Text that came from someone else, on its way into a line-based store
//! file or onto the screen. The stores are one record per line with
//! tab-separated fields, so a newline or tab inside a field would let
//! whoever wrote it forge extra records — a fake line in your message
//! history, a fake cached display name for someone else. Other control
//! characters (escape sequences) could rewrite the terminal.
//!
//! What people write (messages, group posts, posts) may have line
//! breaks, and only those: `message_text` keeps them and cleans the
//! rest. In a store file each is written as `LINE_BREAK` (`to_field`),
//! a character nothing else lets through, and turned back on reading
//! (`from_field`), so it still can't start a second record.

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

/// A line break in a store file's field (see `to_field`): the record
/// separator, a control character, so never in a field otherwise.
const LINE_BREAK: char = '\u{1E}';

/// Whether `s` is fit to be what someone wrote: no control characters
/// but line breaks — what a signed post has to satisfy to be stored as
/// it is.
pub fn is_message_text(s: &str) -> bool {
    !s.chars().any(|c| c != '\n' && is_unwanted(c))
}

/// `s` as what someone wrote is kept: line breaks (`\r\n` and `\r`
/// made `\n`), and every other control character a space.
pub fn message_text(s: &str) -> String {
    s.replace("\r\n", "\n").chars().map(|c| if c == '\r' { '\n' } else if c != '\n' && is_unwanted(c) { ' ' } else { c }).collect()
}

/// `message_text(s)`, for a field of a store file: each line break as
/// `LINE_BREAK`, so it stays one line (`from_field` turns it back).
pub fn to_field(s: &str) -> String {
    message_text(s).replace('\n', &LINE_BREAK.to_string())
}

/// A field `to_field` wrote, with its line breaks back. Text stored
/// before line breaks were kept has none, and reads as it was.
pub fn from_field(s: &str) -> String {
    s.replace(LINE_BREAK, "\n")
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

    #[test]
    fn what_someone_wrote_keeps_its_line_breaks_only() {
        assert!(is_message_text("two\nlines"));
        assert!(!is_message_text("a\ttab"));
        assert!(!is_message_text("cr\rhere"));
        assert!(!is_message_text("Ann\u{202E}nimda"));
        assert_eq!(message_text("a\r\nb\rc\nd\te\x1bf"), "a\nb\nc\nd e f");
    }

    #[test]
    fn a_stored_field_keeps_line_breaks_on_one_line() {
        let field = to_field("one\ntwo\r\nthree\tfour");
        assert!(!field.contains('\n') && !field.contains('\t'));
        assert_eq!(from_field(&field), "one\ntwo\nthree four");
        assert_eq!(from_field("as it was"), "as it was");
    }
}
