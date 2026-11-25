//! A rough file browser for the TUI: picking a file to send, or the
//! folder a received one is saved in. Folders first, then files with
//! their sizes; Enter opens a folder (or picks), Backspace goes up, `/`
//! filters by name, `.` shows hidden entries, `~` jumps home, Esc gives
//! up. Choosing a folder has a "save here" line at the top.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    File,
    Folder,
}

struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
}

pub enum Outcome {
    /// Still browsing.
    Browsing,
    Cancelled,
    File(PathBuf),
    Folder(PathBuf),
}

pub struct Browser {
    purpose: Purpose,
    dir: PathBuf,
    entries: Vec<Entry>,
    /// Index into the rows shown (`rows()`), "save here" included.
    selected: usize,
    filter: String,
    filtering: bool,
    show_hidden: bool,
    error: Option<String>,
}

pub use swarm_app::paths::home;

pub use swarm_app::text::human_size;

impl Browser {
    pub fn new(purpose: Purpose, start: &Path) -> Browser {
        let dir = if start.is_dir() { start.to_path_buf() } else { home() };
        let mut browser = Browser { purpose, dir, entries: Vec::new(), selected: 0, filter: String::new(), filtering: false, show_hidden: false, error: None };
        browser.reload();
        browser
    }

    fn reload(&mut self) {
        self.entries.clear();
        self.error = None;
        match std::fs::read_dir(&self.dir) {
            Ok(read) => {
                for entry in read.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let meta = std::fs::metadata(entry.path());
                    let (is_dir, size) = meta.map(|m| (m.is_dir(), m.len())).unwrap_or((false, 0));
                    self.entries.push(Entry { name, is_dir, size });
                }
                self.entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
            }
            Err(e) => self.error = Some(format!("can't read {}: {e}", self.dir.display())),
        }
        self.selected = 0;
    }

    /// The entries shown: hidden ones only on request, and only those
    /// matching the filter.
    fn shown(&self) -> Vec<&Entry> {
        let filter = self.filter.to_lowercase();
        self.entries
            .iter()
            .filter(|e| self.show_hidden || !e.name.starts_with('.'))
            .filter(|e| filter.is_empty() || e.name.to_lowercase().contains(&filter))
            .collect()
    }

    /// Rows: "save here" first when choosing a folder, then the entries.
    fn row_count(&self) -> usize {
        self.shown().len() + (self.purpose == Purpose::Folder) as usize
    }

    fn go(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.filter.clear();
        self.filtering = false;
        self.reload();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            self.selected = self.selected.min(self.row_count().saturating_sub(1));
            return Outcome::Browsing;
        }
        let rows = self.row_count();
        match key.code {
            KeyCode::Esc => return Outcome::Cancelled,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(rows.saturating_sub(1)),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::PageDown => self.selected = (self.selected + 10).min(rows.saturating_sub(1)),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = rows.saturating_sub(1),
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
                    let came_from = self.dir.file_name().map(|n| n.to_string_lossy().into_owned());
                    self.go(parent);
                    if let Some(name) = came_from {
                        let offset = (self.purpose == Purpose::Folder) as usize;
                        if let Some(i) = self.shown().iter().position(|e| e.name == name) {
                            self.selected = i + offset;
                        }
                    }
                }
            }
            KeyCode::Char('~') => self.go(home()),
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                self.selected = 0;
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.filter.clear();
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if self.purpose == Purpose::Folder && self.selected == 0 {
                    return Outcome::Folder(self.dir.clone());
                }
                let index = self.selected - (self.purpose == Purpose::Folder) as usize;
                let Some(entry) = self.shown().get(index).map(|e| (e.name.clone(), e.is_dir)) else { return Outcome::Browsing };
                let path = self.dir.join(&entry.0);
                if entry.1 {
                    self.go(path);
                } else if self.purpose == Purpose::File && key.code == KeyCode::Enter {
                    return Outcome::File(path);
                }
            }
            _ => {}
        }
        Outcome::Browsing
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, title: &str) {
        let mut items: Vec<ListItem> = Vec::new();
        if self.purpose == Purpose::Folder {
            items.push(ListItem::new(Line::from(Span::styled(
                format!(" [ save here: {} ]", self.dir.display()),
                Style::default().fg(crate::look::good()).add_modifier(Modifier::BOLD),
            ))));
        }
        for entry in self.shown() {
            let line = if entry.is_dir {
                Line::from(Span::styled(format!(" {}/", entry.name), Style::default().fg(crate::look::accent())))
            } else {
                let style = if self.purpose == Purpose::Folder { Style::default().fg(crate::look::dim()) } else { Style::default() };
                Line::from(vec![Span::styled(format!(" {}", entry.name), style), Span::styled(format!("  {}", human_size(entry.size)), Style::default().fg(crate::look::dim()))])
            };
            items.push(ListItem::new(line));
        }
        let mut status = self.dir.display().to_string();
        if self.filtering || !self.filter.is_empty() {
            status = format!("{status}   filter: {}{}", self.filter, if self.filtering { "_" } else { "" });
        }
        if let Some(e) = &self.error {
            status = e.clone();
        }
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(crate::look::accent())).title(format!("{title} — {status}"));
        if items.is_empty() {
            frame.render_widget(Paragraph::new(" (empty)").block(block), area);
            return;
        }
        let mut state = ListState::default();
        state.select(Some(self.selected));
        let list = List::new(items).block(block).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, area, &mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A folder of its own for each test, which run at the same time.
    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-browser-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("photos")).unwrap();
        std::fs::write(dir.join("photos/cat.jpg"), "meow").unwrap();
        std::fs::write(dir.join("notes.txt"), "hi").unwrap();
        std::fs::write(dir.join(".secret"), "x").unwrap();
        dir
    }

    #[test]
    fn folders_first_into_one_and_pick_a_file() {
        let dir = scratch("pick");
        let mut b = Browser::new(Purpose::File, &dir);
        assert_eq!(b.shown().iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["photos", "notes.txt"], "hidden left out");
        b.handle_key(key(KeyCode::Enter));
        assert!(matches!(b.handle_key(key(KeyCode::Enter)), Outcome::File(p) if p == dir.join("photos/cat.jpg")));
        b.handle_key(key(KeyCode::Backspace));
        assert_eq!(b.dir, dir);
        assert_eq!(b.selected, 0, "back on the folder just left");
    }

    #[test]
    fn choosing_a_folder_offers_save_here_and_filters() {
        let dir = scratch("folder");
        let mut b = Browser::new(Purpose::Folder, &dir);
        assert!(matches!(b.handle_key(key(KeyCode::Enter)), Outcome::Folder(p) if p == dir));
        b.handle_key(key(KeyCode::Char('/')));
        for c in "note".chars() {
            b.handle_key(key(KeyCode::Char(c)));
        }
        b.handle_key(key(KeyCode::Enter));
        assert_eq!(b.shown().len(), 1);
        b.handle_key(key(KeyCode::Char('.')));
        b.handle_key(key(KeyCode::Char('/')));
        b.handle_key(key(KeyCode::Esc));
        assert_eq!(b.shown().len(), 3, "hidden shown, filter cleared");
        assert!(matches!(b.handle_key(key(KeyCode::Esc)), Outcome::Cancelled));
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(3_100_000), "3.1 MB");
    }
}
