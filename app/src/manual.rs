//! Swarm's manual, built into the program: `manual/manual.md` (its
//! format is in a note at its top), read into numbered chapters and
//! sections, with its pictures. Every front end shows it: the window's
//! and the phone's Help tab, the terminal UI's (`?`), and `swarm manual`,
//! which prints it. The Word copy is made from the same file
//! (`tools/manual-docx.py`).

use std::ops::Range;
use std::sync::OnceLock;

/// The manual's source.
const SOURCE: &str = include_str!("../manual/manual.md");

/// Its pictures, by the name the source gives them.
const PICTURES: &[(&str, &[u8])] = &[("encryption-keys.svg", include_bytes!("../manual/encryption-keys.svg"))];

/// A chapter (level 1), section (2) or subsection (3), numbered as it
/// comes: "17", "17.1", "17.1.1".
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub number: String,
    pub title: String,
    pub level: usize,
    pub blocks: Vec<Block>,
}

/// One piece of a section. Text is as the source has it — `**bold**`,
/// `` `code` `` and `[[Title]]` links — for `Manual::markdown` or
/// `Manual::plain` to turn into what's shown.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// A heading of its own (`####`), not numbered.
    Heading(String),
    Para(String),
    /// A bullet, or (`under`) one under the bullet before.
    Bullet { under: bool, text: String },
    /// A numbered step, its number as typed.
    Step { number: String, text: String },
    /// A command on a line of its own, without its backticks.
    Command(String),
    /// Two columns, the first row their headings.
    Table { head: [String; 2], rows: Vec<[String; 2]> },
    /// A picture of `PICTURES`, and what it shows.
    Picture { file: String, caption: String },
}

/// The manual, read.
#[derive(Debug)]
pub struct Manual {
    pub sections: Vec<Section>,
}

/// The built-in manual.
pub fn get() -> &'static Manual {
    static MANUAL: OnceLock<Manual> = OnceLock::new();
    MANUAL.get_or_init(|| {
        let mut manual = Manual::parse(SOURCE).expect("the built-in manual reads");
        if let Some(first) = manual.sections.first_mut() {
            first.blocks.insert(0, Block::Para(version()));
        }
        manual
    })
}

/// "Version" and when this build was made.
pub fn version() -> String {
    format!("Version {}", crate::VERSION)
}

/// A picture the manual shows, as its file holds it (SVG).
pub fn picture(file: &str) -> Option<&'static [u8]> {
    PICTURES.iter().find(|(name, _)| *name == file).map(|(_, bytes)| *bytes)
}

impl Manual {
    /// Reads the source: an error says which line is wrong, and why.
    pub fn parse(source: &str) -> Result<Manual, String> {
        let mut sections: Vec<Section> = Vec::new();
        let mut counts = [0usize; 3];
        let mut lines = strip_comments(source).into_iter().enumerate().peekable();
        while let Some((at, line)) = lines.next() {
            let at = at + 1;
            let line = line.trim_end();
            if line.trim().is_empty() {
                continue;
            }
            let hashes = line.chars().take_while(|c| *c == '#').count();
            if (1..=3).contains(&hashes) && line[hashes..].starts_with(' ') {
                let level = hashes;
                if level > 1 && sections.last().is_none_or(|s| s.level < level - 1) {
                    return Err(format!("line {at}: a level {level} heading with no level {} above it", level - 1));
                }
                counts[level - 1] += 1;
                counts[level..].iter_mut().for_each(|c| *c = 0);
                let number = counts[..level].iter().map(|c| c.to_string()).collect::<Vec<_>>().join(".");
                sections.push(Section { number, title: line[hashes + 1..].trim().to_string(), level, blocks: Vec::new() });
                continue;
            }
            let Some(section) = sections.last_mut() else {
                return Err(format!("line {at}: text before the first chapter"));
            };
            let block = if let Some(heading) = line.strip_prefix("#### ") {
                Block::Heading(heading.trim().to_string())
            } else if let Some(text) = line.strip_prefix("  - ") {
                Block::Bullet { under: true, text: text.trim().to_string() }
            } else if let Some(text) = line.strip_prefix("- ") {
                Block::Bullet { under: false, text: text.trim().to_string() }
            } else if let Some((number, text)) = step(line) {
                Block::Step { number, text }
            } else if let Some(rest) = line.strip_prefix("![") {
                let (caption, file) = rest.split_once("](").and_then(|(c, f)| Some((c, f.strip_suffix(')')?))).ok_or(format!("line {at}: a picture is ![what it shows](file)"))?;
                if picture(file).is_none() {
                    return Err(format!("line {at}: no picture {file} (manual.rs's PICTURES lists them)"));
                }
                Block::Picture { file: file.to_string(), caption: caption.to_string() }
            } else if line.starts_with('|') {
                let head = cells(line).ok_or(format!("line {at}: a table's row has two columns"))?;
                let mut rows = Vec::new();
                while let Some((at, next)) = lines.peek() {
                    if !next.starts_with('|') {
                        break;
                    }
                    let row = cells(next).ok_or(format!("line {}: a table's row has two columns", at + 1))?;
                    if !row[0].starts_with("---") {
                        rows.push(row);
                    }
                    lines.next();
                }
                Block::Table { head, rows }
            } else if line.len() > 2 && line.starts_with('`') && line.ends_with('`') && line[1..line.len() - 1].find('`').is_none() {
                Block::Command(line[1..line.len() - 1].to_string())
            } else {
                Block::Para(line.trim().to_string())
            };
            section.blocks.push(block);
        }
        let manual = Manual { sections };
        for (i, a) in manual.sections.iter().enumerate() {
            if manual.sections[..i].iter().any(|b| b.title == a.title) {
                return Err(format!("two sections called {:?}: a link couldn't tell them apart", a.title));
            }
        }
        for section in &manual.sections {
            for text in section.blocks.iter().flat_map(Block::texts) {
                for title in links(text) {
                    if manual.find(title).is_none() {
                        return Err(format!("{} {}: a link to {title:?}, which isn't a section", section.number, section.title));
                    }
                }
            }
        }
        Ok(manual)
    }

    /// The section called `title`.
    pub fn find(&self, title: &str) -> Option<usize> {
        self.sections.iter().position(|s| s.title == title)
    }

    /// The section numbered `number` ("7.2").
    pub fn numbered(&self, number: &str) -> Option<usize> {
        self.sections.iter().position(|s| s.number == number)
    }

    /// What can be opened as a page, in order: chapters and their
    /// sections (a subsection is part of its section's page).
    pub fn pages(&self) -> Vec<usize> {
        (0..self.sections.len()).filter(|i| self.sections[*i].level <= 2).collect()
    }

    /// The page holding section `at`: itself, or the section a
    /// subsection is in.
    pub fn page_of(&self, at: usize) -> usize {
        (0..=at.min(self.sections.len() - 1)).rev().find(|i| self.sections[*i].level <= 2).unwrap_or(0)
    }

    /// The chapter section `at` is in.
    pub fn chapter_of(&self, at: usize) -> usize {
        (0..=at.min(self.sections.len() - 1)).rev().find(|i| self.sections[*i].level == 1).unwrap_or(0)
    }

    /// The contents, as a list shows them with `at` open: every chapter,
    /// and the sections of the one `at` is in.
    pub fn listed(&self, at: usize) -> Vec<usize> {
        let chapter = self.chapter_of(at);
        self.pages().into_iter().filter(|i| self.sections[*i].level == 1 || self.chapter_of(*i) == chapter).collect()
    }

    /// The page `step` pages on from `at` (back, if negative), stopping
    /// at either end.
    pub fn step(&self, at: usize, step: i32) -> usize {
        let pages = self.pages();
        let here = pages.iter().position(|p| *p == self.page_of(at)).unwrap_or(0);
        pages[(here as i64 + i64::from(step)).clamp(0, pages.len() as i64 - 1) as usize]
    }

    /// What a page shows: the section and everything under it (a
    /// chapter's page is the whole chapter).
    pub fn page(&self, at: usize) -> Range<usize> {
        let level = self.sections[at].level;
        let end = (at + 1..self.sections.len()).find(|i| self.sections[*i].level <= level).unwrap_or(self.sections.len());
        at..end
    }

    /// The sections `page`'s text links to, each once, in order.
    pub fn linked_from(&self, page: Range<usize>) -> Vec<usize> {
        let mut linked = Vec::new();
        for section in &self.sections[page.clone()] {
            for text in section.blocks.iter().flat_map(Block::texts) {
                for title in links(text) {
                    if let Some(i) = self.find(title).filter(|i| !page.contains(i) && !linked.contains(i)) {
                        linked.push(i);
                    }
                }
            }
        }
        linked
    }

    /// `text` as Markdown for the window (Slint's StyledText): links to
    /// sections as their numbers, linked to them (`link-clicked` is given
    /// the number); anything that would be taken as Markdown's own, or as
    /// HTML, kept as text.
    pub fn markdown(&self, text: &str) -> String {
        let mut out = String::new();
        for (i, piece) in text.split('`').enumerate() {
            if i % 2 == 1 {
                out.push('`');
                out.push_str(piece);
                out.push('`');
                continue;
            }
            let mut rest = piece;
            while let Some(start) = rest.find("[[") {
                out.push_str(&escape(&rest[..start]));
                let Some(end) = rest[start..].find("]]") else { break };
                let title = &rest[start + 2..start + end];
                match self.find(title) {
                    Some(at) => out.push_str(&format!("[{0}]({0})", self.sections[at].number)),
                    None => out.push_str(&escape(title)),
                }
                rest = &rest[start + end + 2..];
            }
            out.push_str(&escape(rest));
        }
        if (out.starts_with(['+', '-', '*', '#', '>']) && !out.starts_with("**")) || step(&out).is_some() {
            out.insert(0, '\\');
        }
        out
    }

    /// `text` as plain text: no marks, links as their numbers.
    pub fn plain(&self, text: &str) -> String {
        let mut out = text.replace("**", "").replace('`', "");
        while let Some(start) = out.find("[[") {
            let Some(end) = out[start..].find("]]") else { break };
            let title = out[start + 2..start + end].to_string();
            let number = self.find(&title).map_or(title, |at| self.sections[at].number.clone());
            out.replace_range(start..start + end + 2, &number);
        }
        out
    }

    /// A page as lines of text `width` wide, for a terminal.
    pub fn lines(&self, page: Range<usize>, width: usize) -> Vec<TextLine> {
        let width = width.max(20);
        let mut out: Vec<TextLine> = Vec::new();
        let gap = |out: &mut Vec<TextLine>| {
            if out.last().is_some_and(|l| l.kind != LineKind::Gap) {
                out.push(TextLine::new(LineKind::Gap, String::new()));
            }
        };
        for section in &self.sections[page] {
            gap(&mut out);
            for line in wrap(&format!("{} {}", section.number, section.title), width, section.number.chars().count() + 1, "") {
                out.push(TextLine::new(LineKind::Title(section.level), line));
            }
            out.push(TextLine::new(LineKind::Gap, String::new()));
            let listed = |b: &Block| matches!(b, Block::Bullet { .. } | Block::Step { .. });
            let mut before: Option<&Block> = None;
            for block in &section.blocks {
                if before.is_some_and(|b| !matches!(b, Block::Heading(_)) && !(listed(b) && listed(block))) {
                    gap(&mut out);
                }
                before = Some(block);
                match block {
                    Block::Heading(text) => {
                        gap(&mut out);
                        for line in wrap(text, width, 0, "") {
                            out.push(TextLine::new(LineKind::Heading, line));
                        }
                    }
                    Block::Para(text) => self.wrapped(&mut out, text, width, 0, ""),
                    Block::Bullet { under, text } => {
                        let indent = if *under { 4 } else { 2 };
                        self.wrapped(&mut out, text, width, indent, if *under { "  ◦ " } else { "• " });
                    }
                    Block::Step { number, text } => self.wrapped(&mut out, text, width, number.len() + 1, &format!("{number} ")),
                    Block::Command(command) => out.push(TextLine::new(LineKind::Code, format!("  {command}"))),
                    Block::Table { head, rows } => {
                        gap(&mut out);
                        let keys = rows.iter().map(|r| self.plain(&r[0]).chars().count()).chain([head[0].chars().count()]).max().unwrap_or(0);
                        let key_width = keys.min(width / 2);
                        for (kind, row) in std::iter::once((LineKind::TableHead, head)).chain(rows.iter().map(|r| (LineKind::Body, r))) {
                            let key = self.plain(&row[0]);
                            let value = self.plain(&row[1]);
                            let mut key_lines = wrap(&key, key_width, 0, "");
                            let value_lines = wrap(&value, width.saturating_sub(key_width + 2).max(10), 0, "");
                            key_lines.resize(key_lines.len().max(value_lines.len()), String::new());
                            for (i, key) in key_lines.iter().enumerate() {
                                let value = value_lines.get(i).map_or("", String::as_str);
                                let pad = key_width.saturating_sub(key.chars().count());
                                let mut line = TextLine::new(kind, format!("{key}{}  {value}", " ".repeat(pad)).trim_end().to_string());
                                line.lead = key.chars().count();
                                out.push(line);
                            }
                        }
                        gap(&mut out);
                    }
                    Block::Picture { caption, .. } => {
                        gap(&mut out);
                        for line in wrap(&format!("[Picture: {caption}. Shown in the window's and the phone's Help.]"), width, 1, " ") {
                            out.push(TextLine::new(LineKind::Picture, line));
                        }
                        gap(&mut out);
                    }
                }
            }
        }
        while out.last().is_some_and(|l| l.kind == LineKind::Gap) {
            out.pop();
        }
        out
    }

    /// Text wrapped into `out`, the first line starting with `first`, the
    /// rest `indent` in; a term at its start (`**term**`) in bold.
    fn wrapped(&self, out: &mut Vec<TextLine>, text: &str, width: usize, indent: usize, first: &str) {
        let term = text.strip_prefix("**").and_then(|t| t.find("**")).map(|end| self.plain(&text[2..end + 2]).chars().count());
        for (i, line) in wrap(&self.plain(text), width, indent, first).into_iter().enumerate() {
            let mut line = TextLine::new(LineKind::Body, line);
            if i == 0 {
                if let Some(term) = term {
                    line.lead = first.chars().count() + term;
                    line.from = first.chars().count();
                }
            }
            out.push(line);
        }
    }

    /// The whole manual as text `width` wide (`swarm manual`).
    pub fn text(&self, width: usize) -> String {
        let mut out = String::new();
        for chapter in (0..self.sections.len()).filter(|i| self.sections[*i].level == 1) {
            for line in self.lines(self.page(chapter), width) {
                out.push_str(&line.text);
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }
}

/// A line of a page as text (see `Manual::lines`).
#[derive(Debug, Clone, PartialEq)]
pub struct TextLine {
    pub kind: LineKind,
    pub text: String,
    /// Characters `from` to `lead` are a term, or a table's first column,
    /// to be shown bold.
    pub from: usize,
    pub lead: usize,
}

impl TextLine {
    fn new(kind: LineKind, text: String) -> TextLine {
        TextLine { kind, text, from: 0, lead: 0 }
    }
}

/// What a line of text is, for how it's shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A chapter's (1), section's (2) or subsection's (3) title.
    Title(usize),
    Heading,
    Body,
    Code,
    TableHead,
    Picture,
    Gap,
}

impl Block {
    /// The text this holds, for its links.
    fn texts(&self) -> Vec<&str> {
        match self {
            Block::Heading(t) | Block::Para(t) | Block::Bullet { text: t, .. } | Block::Step { text: t, .. } => vec![t],
            Block::Table { rows, .. } => rows.iter().flat_map(|r| [r[0].as_str(), r[1].as_str()]).collect(),
            Block::Command(_) | Block::Picture { .. } => Vec::new(),
        }
    }
}

/// The source without its `<!-- -->` comments (each line of one left
/// blank, so line numbers still match).
fn strip_comments(source: &str) -> Vec<String> {
    let mut inside = false;
    source
        .lines()
        .map(|line| {
            if inside || line.trim_start().starts_with("<!--") {
                inside = !line.contains("-->");
                String::new()
            } else {
                line.to_string()
            }
        })
        .collect()
}

/// "1. text": a numbered step's number and text.
fn step(line: &str) -> Option<(String, String)> {
    let (number, text) = line.split_once(". ")?;
    (!number.is_empty() && number.chars().all(|c| c.is_ascii_digit())).then(|| (number.to_string(), text.trim().to_string()))
}

/// A table row's two cells: split at bars outside backticks.
fn cells(line: &str) -> Option<[String; 2]> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut code = false;
    for c in line.trim().trim_start_matches('|').chars() {
        match c {
            '`' => {
                code = !code;
                cell.push(c);
            }
            '|' if !code => cells.push(std::mem::take(&mut cell).trim().to_string()),
            _ => cell.push(c),
        }
    }
    if !cell.trim().is_empty() {
        cells.push(cell.trim().to_string());
    }
    let [a, b]: [String; 2] = cells.try_into().ok()?;
    Some([a, b])
}

/// The titles `text` links to.
fn links(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let Some(end) = rest[start..].find("]]") else { break };
        found.push(&rest[start + 2..start + end]);
        rest = &rest[start + end + 2..];
    }
    found
}

/// Text for Markdown: what it would take as markup (HTML, emphasis,
/// links) kept as written.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                out.push_str("**");
            }
            '*' | '_' | '[' | ']' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// `text` in lines no wider than `width`: the first starting with
/// `first`, the rest `indent` spaces in. A word too long for a line has
/// one to itself.
fn wrap(text: &str, width: usize, indent: usize, first: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = first.to_string();
    let mut used = first.chars().count();
    let mut start = used;
    for word in text.split_whitespace() {
        let len = word.chars().count();
        if used > start && used + 1 + len > width {
            lines.push(std::mem::take(&mut line));
            line = " ".repeat(indent);
            used = indent;
            start = indent;
        }
        if used > start {
            line.push(' ');
            used += 1;
        }
        line.push_str(word);
        used += len;
    }
    if used > start || lines.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_manual_reads_with_every_link_and_picture_found() {
        let manual = Manual::parse(SOURCE).unwrap();
        assert_eq!(manual.sections[0].number, "1");
        assert_eq!(manual.sections[0].title, "Introduction");
        assert_eq!(manual.sections[1].number, "1.1");
        let encryption = manual.find("Encryption").unwrap();
        assert!(manual.sections[encryption].number.ends_with(".1"));
        let pictures: Vec<_> = manual.sections.iter().flat_map(|s| &s.blocks).filter(|b| matches!(b, Block::Picture { .. })).collect();
        assert!(!pictures.is_empty(), "the encryption diagram");
        assert!(manual.find("Help tab").is_some());
    }

    #[test]
    fn numbers_come_from_the_headings() {
        let manual = Manual::parse("# A\n## B\n### C\n### D\n## E\n# F\ntext\n").unwrap();
        let numbers: Vec<_> = manual.sections.iter().map(|s| s.number.as_str()).collect();
        assert_eq!(numbers, ["1", "1.1", "1.1.1", "1.1.2", "1.2", "2"]);
        assert_eq!(manual.sections[5].blocks, [Block::Para("text".into())]);
    }

    #[test]
    fn mistakes_are_told_by_line() {
        assert!(Manual::parse("# A\n### B\n").unwrap_err().contains("line 2"));
        assert!(Manual::parse("# A\nsee [[Nowhere]]\n").unwrap_err().contains("Nowhere"));
        assert!(Manual::parse("# A\n![x](missing.svg)\n").unwrap_err().contains("missing.svg"));
        assert!(Manual::parse("# A\n# A\n").unwrap_err().contains("two sections"));
        assert!(Manual::parse("text\n# A\n").is_err());
    }

    #[test]
    fn blocks_are_read_as_the_note_says() {
        let source = "<!--\nnote\n-->\n# A\n#### Sub\n- one\n  - under\n1. first\n`swarm words`\n| Key | Does |\n| --- | --- |\n| `/timer off|day` | it |\n| q | quit |\n![Keys](encryption-keys.svg)\n**Term** – text\n";
        let manual = Manual::parse(source).unwrap();
        assert_eq!(
            manual.sections[0].blocks,
            [
                Block::Heading("Sub".into()),
                Block::Bullet { under: false, text: "one".into() },
                Block::Bullet { under: true, text: "under".into() },
                Block::Step { number: "1".into(), text: "first".into() },
                Block::Command("swarm words".into()),
                Block::Table { head: ["Key".into(), "Does".into()], rows: vec![["`/timer off|day`".into(), "it".into()], ["q".into(), "quit".into()]] },
                Block::Picture { file: "encryption-keys.svg".into(), caption: "Keys".into() },
                Block::Para("**Term** – text".into()),
            ]
        );
    }

    #[test]
    fn links_show_as_numbers_and_markup_stays_text() {
        let manual = Manual::parse("# A\n## B\nsee [[B]]\n").unwrap();
        assert_eq!(manual.plain("**T** – see [[B]], `x`"), "T – see 1.1, x");
        assert_eq!(manual.markdown("see [[B]] and `<code>`"), "see [1.1](1.1) and `<code>`");
        assert_eq!(manual.markdown("<name> asks & *one* x_y"), "&lt;name&gt; asks &amp; \\*one\\* x\\_y");
        assert_eq!(manual.markdown("**Term** – text"), "**Term** – text");
        assert_eq!(manual.markdown("+ Add > New"), "\\+ Add &gt; New");
        assert_eq!(manual.linked_from(manual.page(0)), Vec::<usize>::new(), "B is on A's page");
        assert_eq!(manual.linked_from(manual.page(1)), Vec::<usize>::new(), "B links to itself");
    }

    #[test]
    fn pages_lists_and_steps() {
        let manual = Manual::parse("# A\n## A1\n### A1a\n## A2\n# B\n## B1\n# C\n").unwrap();
        assert_eq!(manual.pages(), [0, 1, 3, 4, 5, 6]);
        assert_eq!(manual.page(0), 0..4, "a chapter's page is all of it");
        assert_eq!(manual.page(1), 1..3, "a section's has its subsections");
        assert_eq!(manual.page_of(2), 1);
        assert_eq!(manual.listed(5), [0, 4, 5, 6], "B open");
        assert_eq!(manual.step(3, 1), 4);
        assert_eq!(manual.step(2, 1), 3, "from a subsection, on from its section");
        assert_eq!(manual.step(0, -1), 0);
        assert_eq!(manual.step(6, 5), 6);
    }

    #[test]
    fn a_page_as_text_fits_its_width() {
        let manual = get();
        for width in [30, 60, 100] {
            for page in manual.pages() {
                for line in manual.lines(manual.page(page), width) {
                    if line.kind != LineKind::Code && line.text.split_whitespace().all(|w| w.chars().count() < width / 2) {
                        assert!(line.text.chars().count() <= width, "{width}: {:?}", line.text);
                    }
                }
            }
        }
        let text = manual.text(80);
        assert!(text.starts_with("1 Introduction\n"));
        assert!(text.contains("[Picture: How swarm's encryption keys are made and used."));
        assert!(!text.contains("[["), "links are numbers");
        assert!(!text.contains("**"));
    }

    #[test]
    fn a_term_and_a_tables_first_column_are_marked_bold() {
        let manual = Manual::parse("# A\n- **Echo** – off\n| Key | Does |\n| --- | --- |\n| q | quit |\n").unwrap();
        let lines = manual.lines(manual.page(0), 40);
        let echo = lines.iter().find(|l| l.text.starts_with("• Echo")).unwrap();
        assert_eq!(&echo.text.chars().collect::<Vec<_>>()[echo.from..echo.lead].iter().collect::<String>(), "Echo");
        let quit = lines.iter().find(|l| l.text.starts_with('q')).unwrap();
        assert_eq!((quit.from, quit.lead), (0, 1));
    }

    #[test]
    fn words_wrap_with_a_hanging_indent() {
        assert_eq!(wrap("one two three four", 9, 2, "• "), ["• one two", "  three", "  four"]);
        assert_eq!(wrap("", 10, 0, ""), [""]);
        assert_eq!(wrap("abcdefghijkl", 5, 0, ""), ["abcdefghijkl"]);
    }
}
