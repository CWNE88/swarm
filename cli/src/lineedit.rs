//! The text box everything is typed into: editing keys, and drawing it
//! wrapped over as many lines as it needs (up to a limit, then scrolled
//! to keep the cursor in view), with a still underscore for the cursor —
//! the terminal's own blinking cursor is never shown.
//!
//! The text itself stays a plain `String` (the TUI fills, takes and
//! clears it in many places); the cursor is kept as a count of
//! characters *back from the end*, so 0 — the usual case — means "at the
//! end" whatever the text was replaced with.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

/// The most lines the box grows to before it scrolls instead.
pub const MAX_LINES: u16 = 5;

/// The longest text taken (in bytes, so at most this many characters):
/// more than anything typed here is allowed to be, with room to spare.
const MAX_CHARS: usize = 10_000;

/// Applies an editing key to `text`, with the cursor `back` characters
/// from its end. Returns false for a key that isn't for editing (Enter,
/// Esc, Tab...), which the caller deals with.
pub fn edit(text: &mut String, back: &mut usize, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if let KeyCode::Char(c) = key.code {
        if !ctrl && *back == 0 {
            if text.len() < MAX_CHARS {
                text.push(c);
            }
            return true;
        }
        if !ctrl && text.len() >= MAX_CHARS {
            return true;
        }
    }
    let mut chars: Vec<char> = text.chars().collect();
    *back = (*back).min(chars.len());
    let mut at = chars.len() - *back;
    match key.code {
        KeyCode::Char('a') if ctrl => at = 0,
        KeyCode::Char('e') if ctrl => at = chars.len(),
        KeyCode::Char('u') if ctrl => {
            chars.drain(..at);
            at = 0;
        }
        KeyCode::Char('k') if ctrl => {
            chars.truncate(at);
        }
        KeyCode::Char('w') if ctrl => {
            let start = word_start(&chars, at);
            chars.drain(start..at);
            at = start;
        }
        KeyCode::Char(_) if ctrl => return false,
        KeyCode::Char(c) => {
            chars.insert(at, c);
            at += 1;
        }
        KeyCode::Backspace if at > 0 => {
            at -= 1;
            chars.remove(at);
        }
        KeyCode::Backspace => {}
        KeyCode::Delete if at < chars.len() => {
            chars.remove(at);
        }
        KeyCode::Delete => {}
        KeyCode::Left if ctrl => at = word_start(&chars, at),
        KeyCode::Right if ctrl => at = word_end(&chars, at),
        KeyCode::Left => at = at.saturating_sub(1),
        KeyCode::Right => at = (at + 1).min(chars.len()),
        KeyCode::Home => at = 0,
        KeyCode::End => at = chars.len(),
        _ => return false,
    }
    *back = chars.len() - at;
    *text = chars.into_iter().collect();
    true
}

fn word_start(chars: &[char], mut at: usize) -> usize {
    while at > 0 && chars[at - 1].is_whitespace() {
        at -= 1;
    }
    while at > 0 && !chars[at - 1].is_whitespace() {
        at -= 1;
    }
    at
}

fn word_end(chars: &[char], mut at: usize) -> usize {
    while at < chars.len() && chars[at].is_whitespace() {
        at += 1;
    }
    while at < chars.len() && !chars[at].is_whitespace() {
        at += 1;
    }
    at
}

/// `text` cut into lines of `width` characters, and where the cursor
/// falls (line, column). A cursor just past a full line goes to the
/// start of the next, so there's always room to draw it.
fn wrap(text: &str, back: usize, width: usize) -> (Vec<Vec<char>>, (usize, usize)) {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    let at = chars.len() - back.min(chars.len());
    let mut lines: Vec<Vec<char>> = chars.chunks(width).map(<[char]>::to_vec).collect();
    let cursor = (at / width, at % width);
    if cursor.0 >= lines.len() {
        lines.push(Vec::new());
    }
    (lines, cursor)
}

/// How many lines the box needs inside its border at `width` columns
/// (the border's two columns already taken off), from 1 to `MAX_LINES`.
pub fn lines_needed(text: &str, width: u16) -> u16 {
    let (lines, _) = wrap(text, 0, width as usize);
    (lines.len() as u16).clamp(1, MAX_LINES)
}

/// The box's full height for `area_width` columns: its lines, plus the
/// border above and below.
pub fn box_height(text: &str, area_width: u16) -> u16 {
    lines_needed(text, area_width.saturating_sub(2)) + 2
}

/// Draws the box: `block` round it, the text wrapped inside, and — when
/// it has the keyboard — the cursor as a still underscore (under the
/// character it's on, or after the text). Empty and not being typed in,
/// it shows `placeholder`, dimmed.
#[allow(clippy::too_many_arguments)]
pub fn render(frame: &mut Frame, area: Rect, block: Block, text: &str, back: usize, focused: bool, placeholder: &str, accent: Style) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if text.is_empty() && !focused {
        frame.render_widget(Paragraph::new(Span::styled(placeholder.to_string(), Style::default().fg(crate::look::dim()))), inner);
        return;
    }
    let (lines, (cursor_line, cursor_col)) = wrap(text, back, inner.width as usize);
    let height = inner.height as usize;
    let first = (cursor_line + 1).saturating_sub(height);
    let shown: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(i, line)| {
            if !focused || i != cursor_line {
                return Line::from(line.iter().collect::<String>());
            }
            let before: String = line[..cursor_col.min(line.len())].iter().collect();
            match line.get(cursor_col) {
                Some(c) => Line::from(vec![
                    Span::raw(before),
                    Span::styled(c.to_string(), Style::default().add_modifier(Modifier::UNDERLINED)),
                    Span::raw(line[cursor_col + 1..].iter().collect::<String>()),
                ]),
                None => Line::from(vec![Span::raw(before), Span::styled("_", accent)]),
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(shown), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A paste arrives as one key per character: a big one (a log
    /// pasted by mistake) mustn't freeze the screen while it's typed in.
    #[test]
    fn a_big_paste_is_typed_in_quickly() {
        let started = std::time::Instant::now();
        let (mut text, mut back) = (String::new(), 0);
        for _ in 0..100_000 {
            edit(&mut text, &mut back, key(KeyCode::Char('x')));
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "took {:?}", started.elapsed());
        assert_eq!(text.len(), MAX_CHARS, "and no more than fits");
        edit(&mut text, &mut back, key(KeyCode::Home));
        edit(&mut text, &mut back, key(KeyCode::Char('y')));
        assert_eq!(text.len(), MAX_CHARS, "nor in the middle");
    }

    #[test]
    fn edits_at_the_cursor() {
        let (mut text, mut back) = (String::from("helo"), 0);
        edit(&mut text, &mut back, key(KeyCode::Left));
        edit(&mut text, &mut back, key(KeyCode::Char('l')));
        assert_eq!((text.as_str(), back), ("hello", 1));
        edit(&mut text, &mut back, key(KeyCode::Home));
        edit(&mut text, &mut back, key(KeyCode::Delete));
        assert_eq!(text, "ello");
        edit(&mut text, &mut back, key(KeyCode::End));
        edit(&mut text, &mut back, key(KeyCode::Backspace));
        assert_eq!((text.as_str(), back), ("ell", 0));
        assert!(!edit(&mut text, &mut back, key(KeyCode::Enter)));
    }

    #[test]
    fn ctrl_w_deletes_the_word_before() {
        let (mut text, mut back) = (String::from("one two  "), 0);
        edit(&mut text, &mut back, KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(text, "one ");
    }

    #[test]
    fn wraps_and_puts_the_cursor_on_a_new_line_after_a_full_one() {
        let (lines, cursor) = wrap("abcdef", 0, 3);
        assert_eq!(lines.len(), 3);
        assert_eq!(cursor, (2, 0));
        let (_, cursor) = wrap("abcdef", 2, 3);
        assert_eq!(cursor, (1, 1));
        assert_eq!(lines_needed("", 10), 1);
        assert_eq!(lines_needed(&"x".repeat(100), 10), MAX_LINES);
    }
}
