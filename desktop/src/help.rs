//! The manual, in the Help tab (`swarm_app::manual`): its contents where
//! the list is, and a page beside them. It never changes, so it's the
//! window's own — handed over once, and a page shown as it's chosen —
//! rather than part of every snapshot.

use crate::{App, HelpBlock, HelpKind, HelpRow};
use slint::{Image, Model, ModelRc, SharedString, StyledText, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use swarm_app::manual::{self, Block, Manual};

/// Opens the manual at its start, and has its contents, links and
/// buttons open their pages.
pub fn wire<W: slint::ComponentHandle + 'static>(ui: &W)
where
    for<'a> App<'a>: slint::Global<'a, W>,
{
    let app = ui.global::<App>();
    app.set_help_version(manual::version().into());
    show(&app, 0);
    let weak = ui.as_weak();
    app.on_help_open(move |at| {
        if let Some(ui) = weak.upgrade() {
            show(&ui.global::<App>(), at.max(0) as usize);
        }
    });
    let weak = ui.as_weak();
    app.on_help_link(move |number| {
        if let (Some(ui), Some(at)) = (weak.upgrade(), manual::get().numbered(&number)) {
            show(&ui.global::<App>(), at);
        }
    });
    let weak = ui.as_weak();
    app.on_help_step(move |step| {
        if let Some(ui) = weak.upgrade() {
            let app = ui.global::<App>();
            show(&app, manual::get().step(app.get_help_at() as usize, step));
        }
    });
}

/// Shows the page holding section `at`, the contents opened at it.
pub fn show(app: &App, at: usize) {
    let manual = manual::get();
    let at = manual.page_of(at);
    let rows: Vec<HelpRow> = manual
        .listed(at)
        .into_iter()
        .map(|i| {
            let section = &manual.sections[i];
            HelpRow { number: section.number.clone().into(), title: section.title.clone().into(), level: section.level as i32, selected: i == at, target: i as i32 }
        })
        .collect();
    let same = app.get_help_rows().iter().eq(rows.iter().cloned());
    if !same {
        app.set_help_rows(ModelRc::new(VecModel::from(rows)));
    }
    app.set_help_page(ModelRc::new(VecModel::from(blocks(manual, at))));
    let section = &manual.sections[at];
    app.set_help_title(format!("{} {}", section.number, section.title).into());
    let titled = |i: usize| -> SharedString { if i == at { SharedString::new() } else { manual.sections[i].title.clone().into() } };
    app.set_help_before(titled(manual.step(at, -1)));
    app.set_help_after(titled(manual.step(at, 1)));
    app.set_help_at(at as i32);
    app.set_help_token(app.get_help_token() + 1);
}

/// Text as the page shows it: Markdown, or plain if Slint can't read it.
fn styled(manual: &Manual, text: &str) -> StyledText {
    StyledText::from_markdown(&manual.markdown(text)).unwrap_or_else(|_| StyledText::from_plain_text(&manual.plain(text)))
}

fn block(kind: HelpKind) -> HelpBlock {
    HelpBlock { kind, text: StyledText::default(), second: StyledText::default(), plain: SharedString::new(), picture: Image::default(), ratio: 0.0, wide: false }
}

/// The pieces of the page holding section `at`.
pub fn blocks(manual: &Manual, at: usize) -> Vec<HelpBlock> {
    let mut out = Vec::new();
    let page = manual.page(at);
    for section in &manual.sections[page.clone()] {
        let kind = match section.level {
            _ if out.is_empty() => HelpKind::Title,
            1 | 2 => HelpKind::Section,
            _ => HelpKind::Subsection,
        };
        out.push(HelpBlock { plain: format!("{} {}", section.number, section.title).into(), ..block(kind) });
        for piece in &section.blocks {
            out.push(match piece {
                Block::Heading(text) => HelpBlock { plain: manual.plain(text).into(), ..block(HelpKind::Heading) },
                Block::Para(text) => HelpBlock { text: styled(manual, text), ..block(HelpKind::Para) },
                Block::Bullet { under, text } => HelpBlock { text: styled(manual, text), ..block(if *under { HelpKind::Under } else { HelpKind::Bullet }) },
                Block::Step { number, text } => HelpBlock { text: styled(manual, text), plain: number.as_str().into(), ..block(HelpKind::Step) },
                Block::Command(command) => HelpBlock { plain: command.as_str().into(), ..block(HelpKind::Command) },
                Block::Table { head, rows } => {
                    let wide = rows.iter().any(|r| r[0].starts_with("`swarm"));
                    out.push(HelpBlock { text: styled(manual, &format!("**{}**", head[0])), second: styled(manual, &format!("**{}**", head[1])), wide, ..block(HelpKind::TableHead) });
                    for row in rows {
                        let key = if row[0].starts_with('`') { row[0].clone() } else { format!("**{}**", row[0]) };
                        out.push(HelpBlock { text: styled(manual, &key), second: styled(manual, &row[1]), wide, ..block(HelpKind::TableRow) });
                    }
                    continue;
                }
                Block::Picture { file, caption } => {
                    let picture = picture(file);
                    let size = picture.size();
                    let ratio = if size.width > 0 { size.height as f32 / size.width as f32 } else { 0.0 };
                    HelpBlock { picture, ratio, plain: caption.as_str().into(), ..block(HelpKind::Picture) }
                }
            });
        }
    }
    let linked = manual.linked_from(page);
    if !linked.is_empty() {
        let links: Vec<String> = linked.iter().map(|i| format!("[{0} {1}]({0})", manual.sections[*i].number, manual.sections[*i].title.replace(['[', ']'], ""))).collect();
        let text = format!("**See also:** {}", links.join(" · "));
        out.push(HelpBlock { text: StyledText::from_markdown(&text).unwrap_or_default(), ..block(HelpKind::Links) });
    }
    out
}

/// A picture of the manual, made once (it's drawn from its SVG).
fn picture(file: &str) -> Image {
    thread_local! {
        static MADE: RefCell<HashMap<String, Image>> = RefCell::new(HashMap::new());
    }
    MADE.with(|made| {
        made.borrow_mut()
            .entry(file.to_string())
            .or_insert_with(|| manual::picture(file).and_then(|svg| Image::load_from_svg_data(svg).ok()).unwrap_or_default())
            .clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::ComponentHandle;

    /// A window drawn in memory, on this test's thread.
    fn window() -> crate::AppWindow {
        let window = slint::platform::software_renderer::MinimalSoftwareWindow::new(slint::platform::software_renderer::RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(crate::Offscreen { window })).unwrap();
        crate::AppWindow::new().unwrap()
    }

    #[test]
    fn every_piece_of_the_manual_reads_as_slints_markdown() {
        let manual = manual::get();
        for section in &manual.sections {
            for piece in &section.blocks {
                let texts: Vec<&str> = match piece {
                    Block::Para(t) | Block::Bullet { text: t, .. } | Block::Step { text: t, .. } => vec![t],
                    Block::Table { rows, .. } => rows.iter().flat_map(|r| [r[0].as_str(), r[1].as_str()]).collect(),
                    _ => Vec::new(),
                };
                for text in texts {
                    assert!(StyledText::from_markdown(&manual.markdown(text)).is_ok(), "{} {}: {text:?}", section.number, section.title);
                }
            }
        }
    }

    #[test]
    fn the_encryption_page_has_its_diagram() {
        let _ui = window();
        let manual = manual::get();
        let at = manual.find("Encryption").unwrap();
        let page = blocks(manual, at);
        let diagram = page.iter().find(|b| b.kind == HelpKind::Picture).expect("a picture");
        assert_eq!((diagram.picture.size().width, diagram.picture.size().height), (1440, 1440));
        assert_eq!(diagram.ratio, 1.0);
    }

    #[test]
    fn the_contents_open_links_and_step_through_the_manual() {
        let ui = window();
        wire(&ui);
        let app = ui.global::<App>();
        let manual = manual::get();
        assert_eq!(app.get_help_title(), "1 Introduction");
        assert_eq!(app.get_help_before(), "", "nothing before the start");
        let rows = app.get_help_rows();
        assert!(rows.iter().any(|r| r.title == "Overview of Swarm"));
        assert!(!rows.iter().any(|r| r.title == "Initial use"));
        app.invoke_help_link("17.1.2".into());
        assert_eq!(app.get_help_title(), "17.1 Encryption");
        assert!(app.get_help_rows().iter().any(|r| r.title == "Network traffic"), "its chapter open");
        app.invoke_help_step(1);
        assert_eq!(app.get_help_title(), format!("17.2 {}", manual.sections[manual.step(manual.find("Encryption").unwrap(), 1)].title));
        let last = *manual.pages().last().unwrap();
        app.invoke_help_open(last as i32);
        assert_eq!(app.get_help_after(), "", "nothing after the end");
        app.invoke_help_step(1);
        assert_eq!(app.get_help_at(), last as i32);
    }

    #[test]
    fn a_page_links_to_what_it_mentions() {
        let manual = manual::get();
        let at = manual.find("Unlocking the program").unwrap();
        assert!(blocks(manual, at).iter().any(|b| b.kind == HelpKind::Links), "see also");
        assert!(manual.linked_from(manual.page(at)).contains(&manual.find("General").unwrap()), "Lock when left is in General");
    }
}
