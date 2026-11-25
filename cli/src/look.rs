//! How the terminal UI looks: its few colours (and how the shared layer's
//! colour roles map onto them), the chat layout — times, a column of
//! names, text wrapped beside it — and its panels, cards and key hints.
//!
//! Everything here is drawn with ordinary characters and the 16 basic
//! colours, so it looks the same on a plain console as in a desktop
//! terminal.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use ratatui::Frame;
use swarm_app::prefs::Prefs;
use swarm_app::time::{day_heading, when};

/// The colours in use: the terminal's own (the "system" choice), or a
/// light or dark scheme that paints its own background — so it looks the
/// same whatever the terminal is set to. Chosen in Settings, shared with
/// the window. Read on every redraw; set by `set_theme`.
static THEME: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_theme(theme: swarm_app::prefs::Theme) {
    THEME.store(theme.index() as u8, std::sync::atomic::Ordering::Relaxed);
}

fn light() -> bool {
    THEME.load(std::sync::atomic::Ordering::Relaxed) == 1
}

/// Whether the scheme paints its own background (light or dark, not
/// the terminal's own).
pub fn paints_background() -> bool {
    THEME.load(std::sync::atomic::Ordering::Relaxed) != 0
}

/// The background and ordinary text, when the scheme paints its own.
pub fn background() -> Color {
    if light() { Color::White } else { Color::Black }
}
pub fn text() -> Color {
    if !paints_background() { Color::Reset } else if light() { Color::Black } else { Color::Gray }
}

/// The one colour that marks what's active: the focused panel, keys in
/// the hints, the cursor.
pub fn accent() -> Color {
    if light() { Color::Blue } else { Color::Cyan }
}
/// Someone's `/me`: what they did, apart from what's said.
pub fn did() -> Color {
    Color::Magenta
}
/// A missed call's line.
pub fn missed() -> Color {
    Color::Red
}
/// A channel line that names you.
pub fn mention() -> Color {
    if light() { Color::Rgb(160, 100, 0) } else { Color::Yellow }
}
/// Borders, times, hints, anything in the background.
pub fn dim() -> Color {
    Color::DarkGray
}
/// Secondary text, a step brighter than `dim`.
pub fn soft() -> Color {
    if light() { Color::DarkGray } else { Color::Gray }
}
/// Your own name in chats.
pub fn me() -> Color {
    if light() { Color::Black } else { Color::White }
}
/// The other person in a one-to-one chat, and anything that's fine.
pub fn good() -> Color {
    Color::Green
}
/// Needs attention.
pub fn warn() -> Color {
    if light() { Color::Magenta } else { Color::Yellow }
}
/// Gone wrong.
pub fn bad() -> Color {
    Color::Red
}

/// A panel's frame: dim, or in the accent colour when it's the one
/// being typed into or acted on, with its title in bold.
pub fn panel(title: impl Into<Line<'static>>, active: bool) -> Block<'static> {
    let colour = if active { accent() } else { dim() };
    let title: Line = title.into();
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colour))
        .padding(Padding::horizontal(1))
        .title(title.style(Style::default().fg(if active { accent() } else { text() }).add_modifier(Modifier::BOLD)))
}

/// A title like ` name ` with breathing space either side.
pub fn title(text: impl AsRef<str>) -> Line<'static> {
    Line::from(format!(" {} ", text.as_ref()))
}

/// The ten channel-member colours (see `swarm_app::colours`), the most
/// distinct first. No red (that's for things going wrong) and no white
/// or black (that's you).
const PERSON_COLOURS: [Color; 10] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::LightBlue,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightMagenta,
    Color::LightCyan,
    Color::Blue,
];

/// The same for a light background, where the pale ones don't show.
const PERSON_COLOURS_LIGHT: [Color; 10] = [
    Color::Blue,
    Color::Green,
    Color::Magenta,
    Color::Cyan,
    Color::Yellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
];

/// A status message's colour, by its kind.
pub fn kind_colour(kind: swarm_app::notes::Kind) -> Color {
    use swarm_app::notes::Kind;
    match kind {
        Kind::Info => soft(),
        Kind::Success => good(),
        Kind::Warning => warn(),
        Kind::Error => bad(),
    }
}

/// A name's colour role as a terminal colour.
pub fn name_colour(colour: swarm_app::colours::NameColour) -> Color {
    use swarm_app::colours::NameColour;
    match colour {
        NameColour::Me => me(),
        NameColour::Peer => good(),
        NameColour::Member(slot) => {
            let palette = if light() { &PERSON_COLOURS_LIGHT } else { &PERSON_COLOURS };
            palette[slot as usize % palette.len()]
        }
        NameColour::Other => soft(),
    }
}

use swarm_app::chat::{ChatItem, ChatWhat};

/// The widest the name column gets; longer names are cut.
const NAME_COLUMN: usize = 14;

/// A chat laid out for `width` columns, oldest first:
///
/// ```text
///         ── Thursday 15 October 2026 ──
/// 14:02    owen │ morning
/// 14:03  rupert │ hello — this one runs on, so it carries on
///               │ under itself rather than under the name
/// 14:05       → │ harrison joined
/// ```
///
/// Each day starts with its date; times are left out if `show_times`
/// is off. Done here rather than by the paragraph's own wrapping so
/// wrapped lines keep to the text column. The item whose `lines` are
/// `picked` is shown picked (reversed), and where its lines are is given
/// too.
pub fn chat_lines(items: &[ChatItem], width: u16, prefs: &Prefs, picked: Option<&[swarm_core::history::LineRef]>) -> (Vec<Line<'static>>, Option<std::ops::Range<usize>>) {
    let width = width as usize;
    let mut picked_at = None;
    let names = items
        .iter()
        .map(|item| match &item.what {
            ChatWhat::Said { name, .. } => name.chars().count(),
            ChatWhat::Happened { .. } | ChatWhat::Did { .. } | ChatWhat::Missed { .. } => 1,
        })
        .max()
        .unwrap_or(1)
        .min(NAME_COLUMN);
    let time_width = if prefs.show_times { 6 } else { 0 };
    let gutter = time_width + names + 3;
    let columns = width >= gutter + 12;
    let text_width = if columns { width - gutter } else { width.max(1) };

    let mut lines = Vec::new();
    let mut last_day = None;
    for item in items {
        let w = when(item.at, prefs.utc);
        if last_day != Some(w.day_number) {
            last_day = Some(w.day_number);
            let heading = format!(" {} ", day_heading(item.at, prefs.utc));
            let rule = width.saturating_sub(heading.chars().count()) / 2;
            let rule = "─".repeat(rule.min(20));
            lines.push(Line::from(Span::styled(format!("{rule}{heading}{rule}"), Style::default().fg(dim()))).centered());
        }
        let first = lines.len();
        let is_picked = !item.lines.is_empty() && picked == Some(item.lines.as_slice());
        let time = if prefs.show_times { format!("{:02}:{:02} ", w.hour, w.minute) } else { String::new() };
        let (marker, marker_style, text, text_style) = match &item.what {
            ChatWhat::Said { name, colour, text } => {
                let text_style = if item.for_me { Style::default().fg(mention()).add_modifier(Modifier::BOLD) } else { Style::default() };
                (name.chars().take(names).collect::<String>(), Style::default().fg(name_colour(*colour)).add_modifier(Modifier::BOLD), text.clone(), text_style)
            }
            ChatWhat::Happened { mark, text } => (mark.to_string(), Style::default().fg(dim()), text.to_string(), Style::default().fg(dim())),
            ChatWhat::Did { name, text, .. } => ("*".to_string(), Style::default().fg(did()), format!("{name} {text}"), Style::default().fg(did()).add_modifier(Modifier::ITALIC)),
            ChatWhat::Missed { name } => (String::new(), Style::default(), swarm_app::chat::missed_call(name), Style::default().fg(missed())),
        };
        let (marker_style, text_style) = if item.sent == swarm_app::chat::Sent::Deleting {
            (Style::default().fg(dim()), Style::default().fg(dim()).add_modifier(Modifier::DIM))
        } else {
            (marker_style, text_style)
        };
        let sent = item.sent.mark();
        let sent_span = || Span::styled(format!(" {sent}"), Style::default().fg(dim()));
        if !columns {
            let mut spans = vec![Span::styled(time, Style::default().fg(dim())), Span::styled(format!("{marker} "), marker_style)];
            spans.push(Span::styled(text.to_string(), text_style));
            if !sent.is_empty() {
                spans.push(sent_span());
            }
            lines.push(Line::from(spans));
            if is_picked {
                picked_at = Some(pick(&mut lines, first));
            }
            continue;
        }
        let pieces = wrap_words(&text, if sent.is_empty() { text_width } else { text_width.saturating_sub(2).max(1) });
        let last = pieces.len().saturating_sub(1);
        for (i, piece) in pieces.into_iter().enumerate() {
            let lead = if i == 0 {
                vec![
                    Span::styled(format!("{time:>time_width$}"), Style::default().fg(dim())),
                    Span::styled(format!("{marker:>names$}"), marker_style),
                ]
            } else {
                vec![Span::raw(" ".repeat(time_width + names))]
            };
            let mut spans = lead;
            spans.push(Span::styled(" │ ", Style::default().fg(dim())));
            spans.push(Span::styled(piece, text_style));
            if i == last && !sent.is_empty() {
                spans.push(sent_span());
            }
            lines.push(Line::from(spans));
        }
        if is_picked {
            picked_at = Some(pick(&mut lines, first));
        }
    }
    (lines, picked_at)
}

/// Shows `lines` from `first` on as picked (reversed); where they are.
fn pick(lines: &mut [Line<'static>], first: usize) -> std::ops::Range<usize> {
    for line in &mut lines[first..] {
        for span in &mut line.spans {
            span.style = span.style.add_modifier(Modifier::REVERSED);
        }
    }
    first..lines.len()
}

/// `text` broken into lines no wider than `width`, at spaces where it
/// can be, mid-word where a word is longer than a line.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut len = 0;
        for word in paragraph.split(' ') {
            let mut word: Vec<char> = word.chars().collect();
            if len > 0 && len + 1 + word.len() <= width {
                line.push(' ');
                len += 1;
            } else if len > 0 {
                lines.push(std::mem::take(&mut line));
                len = 0;
            }
            while word.len() > width {
                let rest = word.split_off(width);
                lines.push(word.into_iter().collect());
                word = rest;
            }
            len += word.len();
            line.extend(word);
        }
        lines.push(line);
    }
    lines
}

/// The key hints, as `key: what` pairs (two or more spaces apart):
/// each key in the accent colour, what it does dimmed.
pub fn hints(text: &str) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, part) in text.split("  ").map(str::trim).filter(|p| !p.is_empty()).enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", Style::default().fg(dim())));
        }
        match part.split_once(": ") {
            Some((key, what)) => {
                spans.push(Span::styled(key.to_string(), Style::default().fg(accent())));
                spans.push(Span::styled(format!(" {what}"), Style::default().fg(dim())));
            }
            None => spans.push(Span::styled(part.to_string(), Style::default().fg(dim()))),
        }
    }
    Line::from(spans)
}

/// `hints`, leaving off whole hints from the end until it fits in
/// `width` — never one cut in half.
pub fn hints_fitting(text: &str, width: u16) -> Line<'static> {
    let mut parts: Vec<&str> = text.split("  ").map(str::trim).filter(|p| !p.is_empty()).collect();
    loop {
        let line = hints(&parts.join("   "));
        if line.width() <= width as usize || parts.len() <= 1 {
            return line;
        }
        parts.pop();
    }
}

/// A box in the middle of `area`, sized to its lines — for things that
/// ask for an answer (a ringing call, a file or an invite on offer).
pub fn card(frame: &mut Frame, area: Rect, title: &str, colour: Color, lines: Vec<Line<'static>>) {
    card_with_border(frame, area, title, colour, colour, lines);
}

/// `card`, with its border in a colour of its own (a flashing one, say).
pub fn card_with_border(frame: &mut Frame, area: Rect, title: &str, colour: Color, border: Color, lines: Vec<Line<'static>>) {
    let widest = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let width = (widest + 6).max(title.chars().count() as u16 + 6).min(area.width);
    let height = (lines.len() as u16 + 4).min(area.height);
    let card = Rect::new(area.x + (area.width - width) / 2, area.y + (area.height - height) / 3, width, height);
    frame.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Line::from(format!(" {title} ")).style(Style::default().fg(colour).add_modifier(Modifier::BOLD)).centered());
    let inner = block.inner(card);
    frame.render_widget(block, card);
    let padded = Rect::new(inner.x + 2, inner.y + 1, inner.width.saturating_sub(4), inner.height.saturating_sub(1));
    frame.render_widget(Paragraph::new(lines), padded);
}

/// `label` dimmed and padded to `width`, then `value` — for the
/// aligned `name   value` lines of the detail views.
pub fn field(label: &str, width: usize, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![Span::styled(format!("  {label:<width$}  "), Style::default().fg(dim())), Span::raw(value.into())])
}

/// `text` in at most `cells` columns of the screen (wide characters count
/// two): cut in the middle with `…`, keeping a short extension — so a
/// long file name still shows what kind of file it is.
pub fn fit_middle(text: &str, cells: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if text.width() <= cells {
        return text.to_string();
    }
    let tail: &str = match text.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.width() <= 12 && ext.width() + 2 < cells => &text[stem.len()..],
        _ => "",
    };
    let room = cells.saturating_sub(tail.width() + 1);
    let mut head = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > room {
            break;
        }
        used += w;
        head.push(c);
    }
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_name_is_cut_in_the_middle_keeping_its_extension() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(fit_middle("short.pdf", 20), "short.pdf");
        let cut = fit_middle(&format!("{}.exe", "invoice".repeat(10)), 20);
        assert!(cut.ends_with("….exe") && cut.width() <= 20, "{cut}");
        let wide = fit_middle(&format!("{}.pdf", "名".repeat(60)), 30);
        assert!(wide.ends_with("….pdf") && wide.width() <= 30, "{wide}");
        assert!(fit_middle(&"x".repeat(50), 10).width() <= 10);
    }

    /// The line picked is shown reversed, every line it wraps onto, and
    /// where it is is given; one being deleted is faint.
    #[test]
    fn a_picked_line_is_reversed_and_found_and_one_being_deleted_is_faint() {
        use swarm_app::chat::Sent;
        use swarm_app::colours::NameColour;
        use swarm_core::history::{digest, Direction, LineRef};
        let item = |at, text: &str, sent| {
            let lines = vec![LineRef { direction: Direction::Sent, id: None, at, digest: digest(text) }];
            ChatItem { at, what: ChatWhat::said("me".into(), NameColour::Me, text.into()), sent, for_me: false, lines }
        };
        let items = [item(100, "first", Sent::Delivered), item(101, &"a long one ".repeat(10), Sent::Deleting), item(102, "last", Sent::Waiting)];
        let prefs = Prefs::load(std::path::Path::new("/nonexistent/settings"));
        let (lines, at) = chat_lines(&items, 40, &prefs, Some(&items[1].lines));
        let at = at.expect("found");
        assert!(at.len() > 1, "it wraps: {at:?}");
        let reversed = |l: &Line| l.spans.iter().all(|s| s.style.add_modifier.contains(Modifier::REVERSED));
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(reversed(line), at.contains(&i), "line {i}");
        }
        assert!(lines[at.start].spans.iter().any(|s| s.style.add_modifier.contains(Modifier::DIM)), "being deleted: faint");
        assert_eq!(chat_lines(&items, 40, &prefs, None).1, None, "none picked");
    }

    #[test]
    fn words_wrap_at_spaces_and_long_words_are_cut() {
        assert_eq!(wrap_words("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap_words("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap_words("", 4), vec![""]);
        for line in wrap_words("a bcdefghijkl mn", 5) {
            assert!(line.chars().count() <= 5, "{line}");
        }
    }
}
