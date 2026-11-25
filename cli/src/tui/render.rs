//! The terminal UI drawn: the tab line, the list on the left, whatever's
//! chosen beside it, the forms, prompts and call pictures, from the
//! state in `App`. Nothing here changes it.

use super::*;

pub(super) fn ui(frame: &mut Frame, app: &App) {
    let area = frame.area();
    look::set_theme(app.prefs.theme);
    if look::paints_background() {
        frame.render_widget(Block::default().style(Style::default().bg(look::background()).fg(look::text())), area);
    }
    if let Some((typed, why)) = &app.locked {
        render_locked(frame, area, typed, why);
        return;
    }
    let control = app.control();
    if app.video_only && app.calls.incoming().is_none() && control.asking.is_none() {
        if let Some((call, video)) = app.calls.active().and_then(|c| c.video.as_ref().map(|v| (c, v))) {
            render_video_only(frame, app, call, video, call.muted());
            return;
        }
    }
    let keys = prompt_keys(app);
    let outer = Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1), Constraint::Length(u16::from(keys.is_some()))]).split(area);
    render_status_bar(frame, outer[0], app);

    let main = if app.maximised && app.mode == MainMode::View {
        outer[1]
    } else {
        let body = Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(20)]).split(outer[1]);
        if app.mode == MainMode::Help {
            render_help_contents(frame, body[0], app);
        } else {
            render_sidebar(frame, body[0], app);
        }
        body[1]
    };
    if let Some(prompt) = app.calls.incoming() {
        render_incoming_call(frame, main, prompt, app.ring_choice);
    } else if let Some(name) = &control.asking {
        render_control_ask(frame, main, name, control.asking_screen);
    } else if let Some(prompt) = &app.offer_prompt {
        render_offer_prompt(frame, main, app, prompt);
    } else if let Some((browser, purpose)) = &app.browser {
        let title = match purpose {
            Browsing::SendTo(peer) => format!("Send a file to {peer}"),
            Browsing::Avatar => "Your picture: which one?".to_string(),
            Browsing::SaveFor(id) => {
                let name = app.node.transfers().into_iter().find(|t| t.transfer.id == *id).map(|t| t.transfer.name).unwrap_or_default();
                format!("Save {name} in which folder?")
            }
        };
        browser.render(frame, main, &title);
    } else if let (Some(camera), Some(preview)) = (app.calls.preview(), &app.preview) {
        render_preview(frame, main, camera, preview);
    } else {
        let main = match app.calls.active().and_then(|c| c.video.as_ref().map(|v| (c, v))) {
            Some((call, video)) => {
                let split = Layout::vertical([Constraint::Percentage(60), Constraint::Min(8)]).split(main);
                render_call_video(frame, split[0], call, video, &app.video_rates, app.hide_self, app.prefs.call_info);
                split[1]
            }
            None => main,
        };
        match app.mode {
            MainMode::View => render_view(frame, main, app),
            MainMode::Peers => render_peers(frame, main, app),
            MainMode::Transfers => render_transfers(frame, main, app),
            MainMode::Settings => render_settings(frame, main, app),
            MainMode::Help => render_help(frame, main, app),
        }
    }

    render_status_line(frame, outer[2], app);
    if let Some(keys) = keys {
        frame.render_widget(Paragraph::new(look::hints_fitting(&keys, area.width)), outer[3]);
    }
}

/// The top line: the views as the window's tabs, on the left, the one
/// showing lit and each one's key underlined; then a call under way and
/// files on their way.
pub(super) fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let showing = match app.mode {
        MainMode::View => "Chats",
        MainMode::Peers => "Contacts",
        MainMode::Transfers => "Transfers",
        MainMode::Settings => "Settings",
        MainMode::Help => "Help",
    };
    let mut spans = Vec::new();
    for (number, name) in ["1", "2", "3", "4", "5"].into_iter().zip(["Contacts", "Chats", "Transfers", "Settings", "Help"]) {
        let style = if name == showing {
            Style::default().fg(if look::paints_background() { look::background() } else { Color::Black }).bg(look::accent()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(look::soft())
        };
        spans.extend([Span::styled(format!(" {number} "), style.add_modifier(Modifier::DIM)), Span::styled(format!("{name} "), style), Span::raw(" ")]);
    }
    if let Some(call) = app.calls.active() {
        let secs = call.started_at.elapsed().as_secs();
        spans.push(Span::styled(
            format!("    ● on a call with {}  {:02}:{:02}", call.peer_name, secs / 60, secs % 60),
            Style::default().fg(look::good()).add_modifier(Modifier::BOLD),
        ));
        if call.muted() {
            spans.push(Span::styled("  muted", Style::default().fg(look::warn()).add_modifier(Modifier::BOLD)));
        }
        if app.prefs.call_info {
            for (label, value) in call.details().into_iter().filter(|(label, _)| ["round trip", "traffic"].contains(label)) {
                let value = if label == "round trip" { format!("round trip {value}") } else { value };
                spans.push(Span::styled(format!("  {value}"), Style::default().fg(look::dim())));
            }
        }
        if let Some(name) = app.calls.adding() {
            spans.push(Span::styled(format!("  ringing {name}..."), Style::default().fg(look::warn())));
        }
        for (_, name, key, _) in call.heard() {
            let talking = key.is_some_and(|key| call.is_talking(&key));
            let style = if talking { Style::default().fg(look::good()).add_modifier(Modifier::BOLD) } else { Style::default().fg(look::soft()) };
            spans.push(Span::styled(format!("  {name} "), style));
            spans.extend(meter_spans(key.map_or_else(|| call.own_level(), |key| call.level(&key)), CALL_METER_CELLS));
        }
        if let Some(note) = call.conference_note() {
            spans.push(Span::styled(format!("  {note}"), Style::default().fg(look::warn())));
        }
    } else if let Some(name) = app.calls.outgoing() {
        spans.push(Span::styled(format!("    calling {}...", app.node.label_of(name)), Style::default().fg(look::warn())));
    }
    if let Some(err) = app.backend_error.locked().as_ref() {
        spans.push(Span::styled(format!("    not listening: {err}"), Style::default().fg(look::bad())));
    }
    if let Some(progress) = app.progress.of(&app.node.transfers()) {
        use swarm_app::progress::{LEFT_CHARS, SPEED_CHARS};
        const CELLS: usize = 10;
        let lit = usize::from(progress.percent) * CELLS / 100;
        spans.push(Span::styled(format!("    {} ", progress.what), Style::default().fg(look::dim())));
        spans.push(Span::styled("█".repeat(lit), Style::default().fg(look::accent())));
        spans.push(Span::styled("░".repeat(CELLS - lit), Style::default().fg(look::dim())));
        spans.push(Span::styled(format!(" {:>3}%", progress.percent), Style::default().fg(look::accent())));
        spans.push(Span::styled(format!("  {:>SPEED_CHARS$}  {:>LEFT_CHARS$}", progress.speed, progress.left), Style::default().fg(look::dim())));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The sidebar: you, then your peers, requests, invites and channels,
/// each group under a dim heading, ending in Peers with its ways to add. Each row as the window's: a circle coloured as their ring
/// (green online, orange not added back yet, faint otherwise), or '#'
/// for a channel; the name; the start of the key dim after it. Scrolls
/// to keep the selection in view.
pub(super) fn render_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::RIGHT).border_style(Style::default().fg(look::dim()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let list_area = inner;
    let width = list_area.width as usize;
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_item = 0;
    let dim = Style::default().fg(look::dim());
    for (i, entry) in app.sidebar.rows.iter().enumerate() {
        if let SidebarEntry::Heading(heading) = entry {
            if !items.is_empty() && heading.section.within().is_none() {
                items.push(ListItem::new(""));
            }
            let closed = heading.closed_text();
            let after = if closed.is_empty() { String::new() } else { format!("  {closed}") };
            let indent = if heading.section.within().is_some() { "  " } else { "" };
            let line = Line::from(Span::styled(format!(" {indent}{} {}{after}", heading.triangle(), heading.section.title()), dim));
            if i == app.selected {
                selected_item = items.len();
                items.push(ListItem::new(line).style(Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)));
            } else {
                items.push(ListItem::new(line));
            }
            continue;
        }
        if let SidebarEntry::Add(adding) = entry {
            let indent = if adding.section().within().is_some() { "  " } else { "" };
            let line = Line::from(Span::styled(format!(" {indent}+ {}", adding.text()), Style::default().fg(look::accent())));
            if i == app.selected {
                selected_item = items.len();
                items.push(ListItem::new(line).style(Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)));
            } else {
                items.push(ListItem::new(line));
            }
            continue;
        }

        let (mark, mark_colour, name, name_style, after, count): (&str, Color, String, Style, String, usize) = match entry {
            SidebarEntry::Heading(_) | SidebarEntry::Add(_) => unreachable!("drawn above"),
            SidebarEntry::You => (
                "●",
                look::good(),
                my_name().unwrap_or_else(|| "You".to_string()),
                Style::default().fg(look::me()).add_modifier(Modifier::BOLD),
                short_key(&app.identity.public_key_bytes()),
                0,
            ),
            SidebarEntry::Device(device) => {
                let up = device.this || device.online;
                let state = swarm_app::devices::state(device);
                let mut after = state.to_string();
                if device.relay {
                    after += " · relays";
                }
                if device.receive_only {
                    after += " · receives only";
                }
                ("  ●", if up { look::good() } else { look::dim() }, swarm_app::devices::label(device), if up { Style::default() } else { Style::default().fg(look::soft()) }, after, 0)
            }
            SidebarEntry::Peer(row) => {
                let colour = if row.not_added || row.removed_us {
                    look::warn()
                } else if row.online {
                    look::good()
                } else {
                    look::dim()
                };
                let name_style = if row.online { Style::default() } else { Style::default().fg(look::soft()) };
                let tag = row.tag();
                let after = if tag.is_empty() { short_key(&row.public_key) } else { format!("{} · {tag}", short_key(&row.public_key)) };
                ("●", colour, row.list_label.clone(), name_style, after, row.unread)
            }
            SidebarEntry::Request(request) => ("●", look::dim(), request.label(), Style::default().fg(look::warn()), format!("{} · asks to be added", short_key(&request.public_key)), 0),
            SidebarEntry::Followed(row) => ("●", look::dim(), row.label.clone(), Style::default(), format!("{} · followed", short_key(&row.public_key)), row.unread),
            SidebarEntry::Channel(row) => {
                let tag = row.tag();
                let after = if tag.is_empty() { "group".to_string() } else { format!("group · {tag}") };
                ("#", look::accent(), row.name.clone(), Style::default(), after, row.unread)
            }
        };
        let count = if count > 0 { format!("{count} ") } else { String::new() };
        let room = width.saturating_sub(3 + count.chars().count());
        let name: String = name.chars().take(room).collect();
        let after_room = room.saturating_sub(name.chars().count() + 2);
        let after: String = if after_room < 3 { String::new() } else { format!("  {}", after.chars().take(after_room).collect::<String>()) };
        let used = 3 + name.chars().count() + after.chars().count() + count.chars().count();
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(mark.to_string(), Style::default().fg(mark_colour)),
            Span::raw(" "),
            Span::styled(name, name_style),
            Span::styled(after, dim),
            Span::raw(" ".repeat(width.saturating_sub(used))),
            Span::styled(count, Style::default().fg(look::warn()).add_modifier(Modifier::BOLD)),
        ]);
        if i == app.selected {
            selected_item = items.len();
            items.push(ListItem::new(line).style(Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)));
        } else {
            items.push(ListItem::new(line));
        }
    }
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(selected_item));
    frame.render_stateful_widget(List::new(items), list_area, &mut state);

}

/// The unified view for whatever's selected: your own posts, a peer's
/// chat or posts, a channel, a request or an invite — or, while
/// composing a post, just the compose box.
pub(super) fn render_view(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(compose) = &app.post_compose {
        render_post_compose(frame, area, compose, app);
        return;
    }
    match app.sidebar.rows.get(app.selected) {
        None => {}
        Some(SidebarEntry::Heading(heading)) => render_heading(frame, area, heading),
        Some(SidebarEntry::You) => render_my_view(frame, area, app),
        Some(SidebarEntry::Device(device)) => render_device(frame, area, app, device),
        Some(SidebarEntry::Peer(row)) => render_peer_view(frame, area, app, row),
        Some(SidebarEntry::Channel(row)) => render_channel_view(frame, area, app, &row.id),
        Some(SidebarEntry::Request(request)) => render_request(frame, area, request, button_lines(app, 60)),
        Some(SidebarEntry::Followed(row)) => render_followed(frame, area, app, row),
        Some(SidebarEntry::Add(adding)) => render_adding(frame, area, app, *adding),
    }
}

/// A heading of the list: what's under it, and what Enter does.
/// A way to add, chosen in the list: what it does, and its button.
pub(super) fn render_adding(frame: &mut Frame, area: Rect, app: &App, adding: Adding) {
    let what = match adding {
        Adding::Device => "Shows a code for a new device of yours to read: it becomes one of yours.",
        Adding::Contact => "Their name, address and key, or their whole contact line.",
        Adding::Follow => "Someone's public posts, without adding them as a contact.",
        Adding::NewChannel => "A group of you and the contacts you name. Anyone in it adds their own contacts.",
    };
    let mut lines = vec![
        Line::from(Span::styled(format!(" {}", adding.text()), Style::default().fg(look::accent()).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(Span::styled(format!(" {what}"), Style::default().fg(look::dim()))),
        Line::from(""),
    ];
    lines.extend(button_lines(app, area.width));
    if matches!(adding, Adding::Contact | Adding::Follow) && app.peer_form.is_some() {
        lines.extend(nearby_lines(app));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

pub(super) fn render_heading(frame: &mut Frame, area: Rect, heading: &HeadingRow) {
    let count = match heading.count {
        1 => "1 in it".to_string(),
        n => format!("{n} in it"),
    };
    let lines = vec![
        Line::from(Span::styled(format!(" {}", heading.section.title()), Style::default().fg(look::accent()).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(Span::styled(format!(" {count} · Enter: {}", if heading.open { "close" } else { "open" }), Style::default().fg(look::dim()))),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

/// Someone followed: their public posts, newest first, as last fetched.
pub(super) fn render_followed(frame: &mut Frame, area: Rect, app: &App, row: &FollowRow) {
    let cached = swarm_core::postcache::cached(&paths().postcache(), &row.public_key).unwrap_or_default();
    let public: Vec<&posts::Post> = cached.iter().rev().filter(|p| p.visibility == Visibility::Public).collect();
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(row.label.clone(), Style::default().fg(look::accent()).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  following · {} ", short_key(&row.public_key)), Style::default().fg(look::dim())),
    ]);
    let buttons = button_lines(app, area.width.saturating_sub(2));
    let [buttons_area, area] = Layout::vertical([Constraint::Length(buttons.len() as u16), Constraint::Min(4)]).areas(area);
    frame.render_widget(Paragraph::new(buttons).block(Block::default().padding(ratatui::widgets::Padding::horizontal(1))), buttons_area);
    render_posts(frame, area, app, title, &public, None, "nothing yet — r: refresh", None);
}

/// A contact request — in the main view, and as Peers mode's detail —
/// with its buttons.
pub(super) fn render_request(frame: &mut Frame, area: Rect, request: &RequestRow, buttons: Vec<Line<'static>>) {
    let dim = Style::default().fg(look::dim());
    let mut lines = vec![
        Line::from(Span::styled(format!("{} has added you as a contact.", request.label()), Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        look::field("key", 7, STANDARD.encode(request.public_key)),
        look::field("address", 7, if request.address.is_empty() { "unknown".to_string() } else { request.address.clone() }),
        Line::from(""),
        Line::from(Span::styled("Nothing they send reaches you until you add them back.", dim)),
        Line::from(""),
    ];
    lines.extend(buttons);
    look::card(frame, area, "Contact request", look::warn(), lines);
}

pub(super) fn render_incoming_call(frame: &mut Frame, area: Rect, prompt: &Incoming, choice: usize) {
    let remaining = prompt.deadline.saturating_duration_since(Instant::now()).as_secs();
    let what = if !prompt.with.is_empty() {
        format!("conference call with {}", prompt.with.join(", "))
    } else if prompt.video {
        "video call".to_string()
    } else {
        "call".to_string()
    };
    let choices = ring_choices(prompt.video);
    let mut buttons = Vec::new();
    for (i, c) in choices.iter().enumerate() {
        if i > 0 {
            buttons.push(Span::raw("  "));
        }
        let (label, colour) = match c {
            RingChoice::Video => ("Video", look::good()),
            RingChoice::Audio => ("Audio", look::good()),
            RingChoice::Reject => ("Reject", look::bad()),
        };
        let style = if i == choice.min(choices.len() - 1) {
            Style::default().fg(if look::paints_background() { look::background() } else { Color::Black }).bg(colour).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colour)
        };
        buttons.push(Span::styled(format!(" {label} "), style));
    }
    let lines = vec![
        Line::from(vec![Span::styled(prompt.from.clone(), Style::default().add_modifier(Modifier::BOLD)), Span::raw(format!(" is calling — a {what}"))]).centered(),
        Line::from(""),
        Line::from(buttons).centered(),
        Line::from(""),
        Line::from(Span::styled(format!("stops ringing in {remaining}s"), Style::default().fg(look::dim()))).centered(),
    ];
    let border = if ring_flash_lit(prompt) { look::good() } else { look::dim() };
    look::card_with_border(frame, area, "Incoming call", look::good(), border, lines);
}

/// What can be done with a ringing call, in the order its buttons are
/// shown: answering first, refusing last (on the right).
#[derive(Clone, Copy)]
pub(super) enum RingChoice {
    Video,
    Audio,
    Reject,
}

pub(super) fn ring_choices(video: bool) -> &'static [RingChoice] {
    if video {
        &[RingChoice::Video, RingChoice::Audio, RingChoice::Reject]
    } else {
        &[RingChoice::Audio, RingChoice::Reject]
    }
}

/// Whether a ringing call's flashing border is lit just now: on and off
/// by the half second, counted from when it stops ringing.
pub(super) fn ring_flash_lit(prompt: &Incoming) -> bool {
    (prompt.deadline.saturating_duration_since(Instant::now()).as_millis() / RING_FLASH.as_millis()).is_multiple_of(2)
}

/// When a ringing call's border next goes on or off.
pub(super) fn next_ring_flash(app: &App) -> Option<tokio::time::Instant> {
    let prompt = app.calls.incoming()?;
    let left = prompt.deadline.saturating_duration_since(Instant::now()).as_millis() % RING_FLASH.as_millis();
    Some(tokio::time::Instant::now() + Duration::from_millis(left as u64 + 1))
}

pub(super) fn render_post_compose(frame: &mut Frame, area: Rect, compose: &PostCompose, app: &App) {
    match compose {
        PostCompose::Text => {
            let block = look::panel(look::title("New post"), true);
            lineedit::render(frame, area, block, &app.input, app.input_back, true, "", Style::default().fg(look::accent()));
        }
        PostCompose::AskVisibility(text) => {
            let lines = vec![
                Line::from(Span::styled(text.clone(), Style::default())),
                Line::from(""),
                Line::from("Who should see it?"),
                Line::from(""),
                look::hints("Esc: don't post   y: anyone who asks   n: only your contacts"),
            ];
            let block = look::panel(look::title("New post"), true);
            frame.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
        }
    }
}

pub(super) fn render_my_view(frame: &mut Frame, area: Rect, app: &App) {
    let mine = posts::all(&paths().posts()).unwrap_or_default();
    let (public, private): (Vec<&posts::Post>, Vec<&posts::Post>) = mine.iter().rev().partition(|p| p.visibility == Visibility::Public);
    let buttons = button_lines(app, area.width.saturating_sub(2));
    let [buttons_area, area] = Layout::vertical([Constraint::Length(buttons.len() as u16), Constraint::Min(4)]).areas(area);
    frame.render_widget(Paragraph::new(buttons).block(Block::default().padding(ratatui::widgets::Padding::horizontal(1))), buttons_area);
    let chunks = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    let empty = "nothing yet — n to write one";
    let followers = match app.node.followers() {
        1 => "1 follower".to_string(),
        n => format!("{n} followers"),
    };
    let picked = app.picked_post.as_ref();
    render_posts(frame, chunks[0], app, look::title(format!("Your posts · anyone · {followers}")), &public, None, empty, picked);
    render_posts(frame, chunks[1], app, look::title("Your posts · contacts only"), &private, None, empty, picked);
}

pub(super) fn render_peer_view(frame: &mut Frame, area: Rect, app: &App, row: &PeerRow) {
    let colour = if row.not_added || row.removed_us {
        look::warn()
    } else if row.online {
        look::good()
    } else {
        look::dim()
    };
    let mut title = Line::from(vec![
        Span::styled(" ● ", Style::default().fg(colour)),
        Span::styled(row.label.clone(), Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(format!("{} ", row.status_text(app.prefs.utc)), Style::default().fg(colour)),
    ]);
    let says = swarm_app::avatar::of_peer(&row.folder, &row.public_key).1;
    if !says.is_empty() {
        title.push_span(Span::styled(format!("“{says}” "), Style::default().fg(look::soft()).add_modifier(Modifier::ITALIC)));
    }
    let timer = app.node.timer(&row.handle).seconds;
    if timer > 0 {
        title.push_span(Span::styled(format!("· messages delete after {} ", swarm_core::disappearing::describe(timer)), Style::default().fg(look::warn())));
    }
    let new_posts = app.timelines.has_new(&row.handle);
    let tab = |name: &str, open: bool| {
        if open {
            Span::styled(format!(" {name} "), Style::default().fg(look::accent()).add_modifier(Modifier::BOLD))
        } else {
            Span::styled(format!(" {name} "), Style::default().fg(look::dim()))
        }
    };
    let tabs = Line::from(vec![
        tab("Chat", app.peer_tab == PeerTab::Chat),
        Span::styled("·", Style::default().fg(look::dim())),
        tab("Posts", app.peer_tab == PeerTab::Posts),
        Span::styled(if new_posts { "• " } else { "  " }, Style::default().fg(look::warn())),
    ])
    .right_aligned();

    if app.peer_tab == PeerTab::Posts {
        let (posts, note) = match app.timelines.posts.get(&row.handle) {
            None => (Vec::new(), Some("asking them for their posts...".to_string())),
            Some(PostsResult::Live(posts)) => (posts.clone(), None),
            Some(PostsResult::Cached { posts, .. }) => (posts.clone(), Some("they're not reachable — this is what you saw last".to_string())),
        };
        let (public, private): (Vec<&posts::Post>, Vec<&posts::Post>) = posts.iter().rev().partition(|p| p.visibility == Visibility::Public);
        let frame_block = Block::default().title(title).title(tabs);
        let buttons = button_lines(app, area.width.saturating_sub(2));
        let shown = buttons.len() as u16;
        frame.render_widget(frame_block, Rect::new(area.x, area.y, area.width, 1));
        frame.render_widget(Paragraph::new(buttons), Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), shown));
        let inner = Rect::new(area.x, area.y + 1 + shown, area.width, area.height.saturating_sub(1 + shown));
        let chunks = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);
        render_posts(frame, chunks[0], app, look::title("for anyone"), &public, note.as_deref(), "no public posts", None);
        render_posts(frame, chunks[1], app, look::title("for contacts"), &private, None, "none for you to see", None);
        return;
    }

    let input_height = lineedit::box_height(&app.input, area.width);
    let buttons = button_lines(app, area.width.saturating_sub(2));
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(buttons.len() as u16), Constraint::Length(input_height)]).split(area);
    let most = chunks[0].height as usize * 2 + app.chat_scroll as usize;
    let items = if app.picked.is_some() { chat::peer_chat(row) } else { chat::peer_chat_last(row, most) };
    let block = look::panel(title, false).title(tabs);
    render_chat(frame, chunks[0], app, block, &items, &format!("nothing said yet — Enter to write to {}", row.label));
    frame.render_widget(Paragraph::new(buttons).block(Block::default().padding(ratatui::widgets::Padding::horizontal(1))), chunks[1]);
    render_input(frame, chunks[2], app, "Message", &format!("Enter to write to {}", row.label));
}

/// Always reads fresh from local storage on every render, same as
/// `render_my_view` does for your own posts — cheap local file reads, no
/// in-memory cache to keep in sync.
pub(super) fn render_channel_view(frame: &mut Frame, area: Rect, app: &App, id: &ChannelId) {
    let view = chat::channel_view(&app.node, &app.sidebar, id, &app.prefs);

    let left = if app.maximised {
        area
    } else {
        let columns = Layout::horizontal([Constraint::Min(20), Constraint::Length(MEMBERS_WIDTH)]).split(area);
        let member_lines: Vec<Line> = view
            .members
            .iter()
            .map(|m| {
                let (dot, colour) = if m.connected { ("●", look::good()) } else { ("○", look::dim()) };
                Line::from(vec![
                    Span::styled(format!(" {dot}"), Style::default().fg(colour)),
                    Span::raw(" "),
                    Span::styled(m.name.clone(), Style::default().fg(look::name_colour(m.colour))),
                ])
            })
            .collect();
        let count = if view.members.len() == 1 { "just you".to_string() } else { format!("{} people", view.members.len()) };
        frame.render_widget(Paragraph::new(member_lines).block(look::panel(look::title(count), false)), columns[1]);
        columns[0]
    };

    let can_post = view.can_post();
    let input_height = if can_post { lineedit::box_height(&app.input, left.width) } else { 3 };
    let buttons = button_lines(app, left.width.saturating_sub(2));
    let [chat_area, buttons_area, input_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(buttons.len() as u16), Constraint::Length(input_height)]).areas(left);
    frame.render_widget(Paragraph::new(buttons).block(Block::default().padding(ratatui::widgets::Padding::horizontal(1))), buttons_area);
    let parts = [chat_area, input_area];

    let title = vec![Span::styled(" # ", Style::default().fg(look::accent())), Span::styled(format!("{} ", view.name), Style::default().add_modifier(Modifier::BOLD))];
    let empty = if can_post { "nothing said yet — g, or /add <contact>, puts your contacts in it" } else { "" };
    render_chat(frame, parts[0], app, look::panel(Line::from(title), false), &view.items, empty);

    if can_post {
        render_input(frame, parts[1], app, "Message", &format!("Enter to write to {}  ·  /help for commands", view.name));
    } else {
        let block = look::panel(look::title("Message"), false);
        frame.render_widget(Paragraph::new(Span::styled("you're not in it any more", Style::default().fg(look::dim()))).block(block), parts[1]);
    }
}

/// Peers: the details of whatever's selected — a peer, a group,
/// someone followed, a request, or you — each with its
/// buttons; and the forms of the ways to add under them.
pub(super) fn render_peers(frame: &mut Frame, area: Rect, app: &App) {
    let typing = app.peer_form.is_some() || app.channel_form.is_some() || app.receive_prompt.is_some() || app.device_input.is_some();
    let input_height = if typing { lineedit::box_height(&app.input, area.width) } else { 0 };
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(input_height)]).split(area);
    let dim = Style::default().fg(look::dim());

    match app.sidebar.rows.get(app.selected) {
        Some(SidebarEntry::Heading(heading)) => render_heading(frame, chunks[0], heading),
        Some(SidebarEntry::Add(adding)) => render_adding(frame, chunks[0], app, *adding),
        Some(SidebarEntry::Request(request)) => render_request(frame, chunks[0], request, button_lines(app, 60)),
        Some(SidebarEntry::You) if app.device_code.is_some() => render_device_code(frame, chunks[0], app),
        Some(SidebarEntry::You) => render_identity(frame, chunks[0], app),
        Some(SidebarEntry::Device(device)) => render_device(frame, chunks[0], app, device),
        Some(SidebarEntry::Peer(row)) => {
            let line = swarm_app::contact::peer_line(&row.public_key, &row.address, &row.label);
            let (colour, status) = if row.removed_us {
                (look::warn(), format!("{} — d: OK, take them off too", row.status_text(app.prefs.utc)))
            } else if row.not_added {
                (look::warn(), row.status_text(app.prefs.utc))
            } else if row.online {
                (look::good(), row.status_text(app.prefs.utc))
            } else {
                (look::dim(), row.status_text(app.prefs.utc))
            };
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("● ", Style::default().fg(colour)),
                    Span::styled(row.label.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  {}", short_key(&row.public_key)), dim),
                ]),
                Line::from(Span::styled(format!("  {status}"), Style::default().fg(colour))),
                Line::from(""),
            ];
            lines.extend(button_lines(app, beside_code(chunks[0], &line)));
            lines.push(Line::from(""));
            let says = swarm_app::avatar::of_peer(&row.folder, &row.public_key).1;
            if !says.is_empty() {
                lines.push(look::field("says", 11, says));
            }
            lines.extend(peerbook::address_rows(&app.node, row, app.prefs.utc).into_iter().map(|(label, value)| look::field(label, 15, value)));
            lines.extend([
                look::field("key", 11, STANDARD.encode(row.public_key)),
                look::field("cover", 11, row.cover_text(&app.node)),
                look::field("their calls", 11, row.auto_answer_text()),
                look::field("control", 11, swarm_app::sidebar::auto_control_text(row.auto_control)),
                look::field("their files", 11, peerbook::receive_text(&row.folder, &row.public_key)),
            ]);
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("their details at once, to pass on: this line, or its code, scanned in someone's Add a contact", dim)));
            lines.push(Line::from(line.clone()));
            render_with_code(frame, chunks[0], look::panel(look::title("Contact"), false), lines, &line);
        }
        Some(SidebarEntry::Channel(row)) => {
            let view = chat::channel_view(&app.node, &app.sidebar, &row.id, &app.prefs);
            let mut lines = vec![
                Line::from(vec![Span::styled("# ", Style::default().fg(look::accent())), Span::styled(view.name.clone(), Style::default().add_modifier(Modifier::BOLD))]),
                Line::from(""),
            ];
            lines.extend(button_lines(app, chunks[0].width.saturating_sub(4)));
            lines.extend([
                Line::from(""),
                look::field("in it", 7, view.members.iter().map(|m| m.name.clone()).collect::<Vec<_>>().join(", ")),
                Line::from(""),
                Line::from(Span::styled("In its chat: /add <contact> puts a contact of yours in it, /groupname <name> names it.", dim)),
            ]);
            frame.render_widget(Paragraph::new(lines).block(look::panel(look::title("Group"), false)).wrap(Wrap { trim: false }), chunks[0]);
        }
        Some(SidebarEntry::Followed(row)) => {
            let mut lines = vec![
                Line::from(vec![Span::styled("● ", dim), Span::styled(row.label.clone(), Style::default().add_modifier(Modifier::BOLD)), Span::styled("  followed", dim)]),
                Line::from(""),
            ];
            lines.extend(button_lines(app, chunks[0].width.saturating_sub(4)));
            lines.extend([
                Line::from(""),
                look::field("key", 7, STANDARD.encode(row.public_key)),
                look::field("address", 7, row.address.clone()),
                Line::from(""),
                Line::from(Span::styled("You see their posts for anyone; they needn't add you.", dim)),
            ]);
            frame.render_widget(Paragraph::new(lines).block(look::panel(look::title("Following"), false)).wrap(Wrap { trim: false }), chunks[0]);
        }
        None => {
            let lines = vec![Line::from(Span::styled("Nobody here yet — Add a contact is at the end of CONTACTS.", dim))];
            frame.render_widget(Paragraph::new(lines).block(look::panel(look::title("Contact"), false)).wrap(Wrap { trim: false }), chunks[0]);
        }
    }

    if let Some(form) = app.peer_form.as_ref() {
        render_input(frame, chunks[1], app, form_field_label(form), "");
    } else if let Some(form) = app.channel_form.as_ref() {
        render_input(frame, chunks[1], app, channel_form_field_label(form), "");
    } else if let Some(PeerRow { label, .. }) = app.receive_prompt.as_ref() {
        render_input(frame, chunks[1], app, &format!("Save files from {label} straight to this folder — blank: ask each time"), "");
    } else if let Some(DeviceInput::ContactAddress(_)) = app.device_input {
        render_input(frame, chunks[1], app, "Their address: IPs or host names (with :port if not 1977, separated by commas), or a swarm: code they've sent", "");
    }
}

/// Under Add a contact and Follow someone, while the form's open: who's
/// said they're on this network, new ones first (Tab puts the next one's
/// code in the address field), contacts already at the end, greyed.
fn nearby_lines(app: &App) -> Vec<Line<'static>> {
    let dim = Style::default().fg(look::dim());
    let mut lines = vec![Line::from(""), Line::from(Span::styled(" Discover locally advertised contacts", Style::default().add_modifier(Modifier::BOLD)))];
    let found = swarm_app::discovery::nearby(&app.node);
    if found.is_empty() {
        let text = if swarm_app::discovery::looked() { " Nobody on this network is discoverable just now." } else { " Looking on this network…" };
        lines.push(Line::from(Span::styled(text, dim)));
        return lines;
    }
    lines.push(Line::from(Span::styled(" At the address, Tab puts the next one's code in (Shift+Tab the one before); Enter takes it.", dim)));
    let mut new = 0;
    for n in &found {
        let name = if n.name.is_empty() { "(no name)".to_string() } else { n.name.clone() };
        let detail = format!("  {} · {}", swarm_app::names::short_key(&n.key), n.addresses.join(", "));
        if n.known {
            lines.push(Line::from(vec![Span::styled(format!("   {name}"), dim), Span::styled(format!("{detail}  already a contact"), dim)]));
        } else {
            let picked = app.nearby_pick == Some(new);
            new += 1;
            let style = if picked { Style::default().fg(look::accent()).add_modifier(Modifier::BOLD) } else { Style::default().add_modifier(Modifier::BOLD) };
            lines.push(Line::from(vec![Span::styled(format!(" {}{name}", if picked { "› " } else { "  " }), style), Span::styled(detail, dim)]));
        }
    }
    lines
}

pub(super) fn channel_form_field_label(form: &ChannelForm) -> &'static str {
    match form.kind {
        ChannelFormKind::New => "Who's in it: your contacts' names, separated by commas",
        ChannelFormKind::With(_) => "Who else: your contacts' names, separated by commas",
    }
}

pub(super) fn form_field_label(form: &PeerForm) -> &'static str {
    match (form.kind, form.step) {
        (PeerFormKind::Add, 1) => "Address (IPv6 or IPv4; several separated by commas), or their whole swarm: contact line — Tab: someone on this network",
        (PeerFormKind::Follow, 1) => "Their address (IPv6 or IPv4; several separated by commas), or their swarm: code — Tab: someone on this network",
        (PeerFormKind::Add, 2) => "Public key (base64)",
        (PeerFormKind::Add, 3) => "Cover rate, e.g. 64k or '64k require' (optional — blank for none)",
        (PeerFormKind::Update, 0) => "Name (Enter keeps it)",
        (PeerFormKind::Update, 1) => "Address, or a swarm: code they've sent (Enter keeps it)",
        (PeerFormKind::Update, 2) => "Public key (Enter keeps it)",
        (PeerFormKind::Update, 3) => "Cover rate, e.g. 64k or '64k require' (Enter keeps it, 'off': clear)",
        (PeerFormKind::Accept, 0) => "Name for them (suggested — edit or Enter to keep)",
        (PeerFormKind::Accept, 1) => "Address they connected from (Enter keeps it)",
        (PeerFormKind::Accept, 2) => "Their public key (Enter keeps it)",
        (PeerFormKind::Accept, 3) => "Cover rate, e.g. 64k or '64k require' (optional — blank for none)",
        (PeerFormKind::Follow, 2) => "Their public key (base64)",
        _ => "...",
    }
}

pub(super) fn render_identity(frame: &mut Frame, area: Rect, app: &App) {
    let dim = Style::default().fg(look::dim());
    let me = app.identity.public_key_bytes();
    let says = swarm_app::avatar::status(&me, true);
    let pictured = swarm_app::avatar::pixels(&me, true).is_some();
    let outside = seen_from_outside(&app.node, &app.sidebar.peer_names());
    let manual = app.node.manual_address();
    let mut lines = vec![
        Line::from(Span::styled(my_name().unwrap_or_else(|| "No name set — n: name".to_string()), Style::default().fg(look::me()).add_modifier(Modifier::BOLD))),
        Line::from(""),
        look::field("status", 11, if says.is_empty() { "none".to_string() } else { says }),
        look::field("picture", 11, if pictured { "set" } else { "none" }.to_string()),
        Line::from(Span::styled("  only your contacts see your status and picture", dim)),
        Line::from(""),
    ];
    let manual_text = manual.text();
    lines.push(look::field("manually set IP address", 23, if manual_text.is_empty() { "none — i: set one".to_string() } else { manual_text }));
    lines.push(Line::from(Span::styled("  first in your code, on all your devices: your router's public address with a port forwarded, say", dim)));
    let discoverable = app.node.discoverable().on;
    lines.push(look::field("discoverable on this network", 23, if discoverable { "on — d: turn off" } else { "off — d: turn on" }.to_string()));
    lines.push(Line::from(Span::styled("  anyone on the same network sees your name, key and address there (mDNS), under Add a contact; set on one of your devices, it's on all of them", dim)));
    if !outside.is_empty() {
        lines.push(Line::from(Span::styled("  they see you at", dim)));
        lines.extend(outside.iter().map(|(a, who)| Line::from(vec![Span::raw(format!("  {a}")), Span::styled(format!("  ({})", who.join(", ")), dim)])));
    }
    lines.push(Line::from(""));
    let reach: Vec<_> = global_addresses().into_iter().chain(outside.iter().map(|(a, _)| *a)).chain(local_addresses()).collect();
    let line = swarm_app::contact::line(&me, &manual.items, &reach, &my_name().unwrap_or_default());
    lines.extend(button_lines(app, beside_code(area, &line)));
    lines.extend([
        Line::from(""),
        Line::from(Span::styled("  What someone needs to add you: your key, and an address below.", dim)),
        Line::from(""),
        Line::from(Span::styled("  public key", dim)),
        Line::from(format!("  {}", app.identity.public_key_base64())),
        Line::from(""),
        Line::from(Span::styled("  addresses", dim)),
    ]);
    let addresses: Vec<_> = global_addresses().into_iter().chain(local_addresses()).collect();
    if addresses.is_empty() {
        lines.push(Line::from(Span::styled("  none", dim)));
    }
    lines.extend(addresses.into_iter().map(|a| Line::from(format!("  {a}"))));
    lines.push(Line::from(""));
    let local = app.node.local_addr();
    let families = if local.as_ref().is_ok_and(|a| a.is_ipv4()) { "IPv4 only" } else { "IPv6 and IPv4" };
    lines.push(Line::from(Span::styled(format!("  listening on port {}, {families}", local.map(|a| a.port()).unwrap_or(0)), dim)));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  or all of it at once: this line, or its code, scanned in their Add a contact", dim)));
    lines.push(Line::from(format!("  {line}")));
    lines.push(Line::from(Span::styled("  anyone who sees the code, in a screenshot or a video call, sees your addresses", dim)));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  Blocked keys", Style::default().add_modifier(Modifier::BOLD))));
    let blocked = actions::blocked(&app.node);
    if blocked.is_empty() {
        lines.push(Line::from(Span::styled("  none", dim)));
    } else {
        lines.push(Line::from(Span::styled("  told they're removed whenever they connect, never shown asking to be added; b unblocks one", dim)));
    }
    for row in blocked {
        lines.push(Line::from(vec![Span::raw(format!("  {}", row.label())), Span::styled(format!("  {}", row.key()), dim)]));
    }
    render_with_code(frame, area, look::panel(look::title("You"), false), lines, &line);
}

/// How wide the text of `render_with_code`'s panel in `area` is, with
/// `line`'s code beside it or under it.
pub(super) fn beside_code(area: Rect, line: &str) -> u16 {
    let code_width = swarm_app::contact::qr_lines(line).first().map_or(0, |row| row.chars().count()) as u16 + 3;
    let inner = area.width.saturating_sub(4);
    if inner >= code_width + 50 { inner - code_width } else { inner }
}

/// A panel of `lines` with `line`'s QR code: beside them where there's
/// room, else under them.
pub(super) fn render_with_code(frame: &mut Frame, area: Rect, block: Block, mut lines: Vec<Line>, line: &str) {
    let code = swarm_app::contact::qr_lines(line);
    let code_width = code.first().map_or(0, |row| row.chars().count()) as u16 + 3;
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let code: Vec<Line> = code.into_iter().map(|row| Line::from(format!("  {row}"))).collect();
    if inner.width >= code_width + 50 {
        let [text, beside] = Layout::horizontal([Constraint::Min(50), Constraint::Length(code_width)]).areas(inner);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text);
        frame.render_widget(Paragraph::new(code), beside);
    } else {
        lines.extend(code);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

pub(super) fn render_status_line(frame: &mut Frame, area: Rect, app: &App) {
    if let Some((text, kind)) = app.current_status() {
        frame.render_widget(Paragraph::new(Span::styled(format!(" {text}"), Style::default().fg(look::kind_colour(kind)))), area);
    }
}

/// A video call's panel: their picture as large as fits, ours as a small
/// mirrored inset in the bottom right, and (`details`, Settings' call
/// details) how it's going in the title.
/// Pictures sent and shown a second, and kilobits sent, from the counts
/// now and those in `rates` (updated once a second). The counts are one
/// link's, and the link shown can change (another member talking), so a
/// count lower than the last starts afresh rather than going below none.
pub(super) fn video_rates(rates: &std::cell::Cell<(Instant, u64, u64, u64, f32, f32, f32)>, sent: u64, shown: u64, bytes: u64) -> (Instant, u64, u64, u64, f32, f32, f32) {
    let (since, sent_then, shown_then, bytes_then, sent_fps, shown_fps, kbps) = rates.get();
    let elapsed = since.elapsed().as_secs_f32();
    if elapsed < 1.0 {
        return (since, sent_then, shown_then, bytes_then, sent_fps, shown_fps, kbps);
    }
    let rate = |now: u64, then: u64| now.saturating_sub(then) as f32 / elapsed;
    let now = (Instant::now(), sent, shown, bytes, rate(sent, sent_then), rate(shown, shown_then), rate(bytes, bytes_then) * 8.0 / 1000.0);
    rates.set(now);
    now
}

pub(super) fn render_call_video(frame: &mut Frame, area: Rect, call: &swarm_app::calls::Active, video: &CallVideo, rates: &std::cell::Cell<(Instant, u64, u64, u64, f32, f32, f32)>, hide_self: bool, details: bool) {
    let Some((remote, peer_name)) = call.shown_remote() else { return };
    let peer_name = peer_name.as_str();
    let stats = &remote.stats;
    let (sent, shown, bytes) = (
        stats.frames_sent.load(std::sync::atomic::Ordering::Relaxed),
        stats.frames_shown.load(std::sync::atomic::Ordering::Relaxed),
        stats.bytes_sent.load(std::sync::atomic::Ordering::Relaxed),
    );
    let (_, _, _, _, sent_fps, shown_fps, kbps) = video_rates(rates, sent, shown, bytes);
    let target = stats.bitrate.load(std::sync::atomic::Ordering::Relaxed) / 1000;
    let theirs = if remote.their_screen() { format!("{peer_name}'s screen") } else { format!("video with {peer_name}") };
    let controlled = call.control_state().controller.map(|(name, paused)| if paused { format!(" — {name} paused while you use your mouse, x: stop them") } else { format!(" — {name} is controlling it, x: stop them") });
    let ours = if video.screen.is_some() { format!("sharing your screen{} — e: stop sharing", controlled.unwrap_or_default()) } else { "w: camera on/off   e: share screen".to_string() };
    let (sent_in, shown_in) = stats.codecs();
    let title = if details {
        format!("{theirs} — showing {shown_fps:.0} fps {shown_in}, sending {sent_fps:.0} fps {sent_in} at {kbps:.0} of {target} kbit/s — {ours}")
    } else {
        format!("{theirs} — {ours}")
    };
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(look::good())).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    render_pictures(frame, inner, remote, peer_name, video, hide_self);
}

/// The video-only view (`v`): their picture on the whole screen, ours in
/// the corner unless left out, "muted" while it is, and the keys for the
/// first few seconds only.
pub(super) fn render_video_only(frame: &mut Frame, app: &App, call: &swarm_app::calls::Active, video: &CallVideo, muted: bool) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(Color::Black)), area);
    if let Some((remote, peer_name)) = call.shown_remote() {
        render_pictures(frame, area, remote, &peer_name, video, app.hide_self);
    }
    if muted {
        let marker = Rect::new(area.x + 1, area.y, 7.min(area.width), 1);
        frame.render_widget(Paragraph::new(Span::styled(" muted ", Style::default().fg(Color::Black).bg(look::warn()).add_modifier(Modifier::BOLD))), marker);
    }
    if let Some((name, _)) = call.control_state().controller {
        let text = format!(" {name} is controlling your screen — x: stop ");
        let marker = Rect::new(area.x + if muted { 9 } else { 1 }, area.y, (text.chars().count() as u16).min(area.width.saturating_sub(9)), 1);
        frame.render_widget(Paragraph::new(Span::styled(text, Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD))), marker);
    }
    if app.video_only_since.elapsed() < VIDEO_ONLY_HINTS {
        let hints = look::hints_fitting(
            &format!(
                "v: back   m: {}   w: camera   e: {}   s: {}   h: end call",
                if muted { "unmute" } else { "mute" },
                if video.screen.is_some() { "stop sharing" } else { "share screen" },
                "preview"
            ),
            area.width,
        );
        let line = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1.min(area.height));
        frame.render_widget(ratatui::widgets::Clear, line);
        frame.render_widget(Paragraph::new(hints), line);
    }
}

/// How long the video-only view shows its keys before leaving the picture alone.
pub(super) const VIDEO_ONLY_HINTS: Duration = Duration::from_secs(3);

/// A call's pictures in `area`: theirs as large as fits, ours (unless
/// `hide_self`) a quarter of the width in the bottom-right corner.
pub(super) fn render_pictures(frame: &mut Frame, inner: Rect, remote: &swarm_app::calls::Remote, peer_name: &str, video: &CallVideo, hide_self: bool) {
    let mode = crate::picture::ColourMode::detect();
    video.show_in((inner.width as usize, inner.height as usize * 2), (0, 0));
    match (remote.large_view().borrow().clone(), remote.note(peer_name)) {
        (Some(picture), _) => frame.render_widget(crate::picture::Picture { frame: &picture, mode, mirror: false }, inner),
        (None, note) => {
            let top = inner.y + inner.height / 2;
            let line = Rect::new(inner.x, top.min(inner.bottom().saturating_sub(1)), inner.width, 1.min(inner.height));
            frame.render_widget(Paragraph::new(note.unwrap_or_default()).alignment(ratatui::layout::Alignment::Center).style(Style::default().fg(look::dim())), line);
        }
    }
    if hide_self {
        return;
    }
    let inset_w = (inner.width / 4).max(12).min(inner.width);
    let inset_h = (inset_w / 2).max(4).min(inner.height);
    let inset = Rect::new(inner.right() - inset_w, inner.bottom() - inset_h, inset_w, inset_h);
    frame.render_widget(ratatui::widgets::Clear, inset);
    match video.local_picture() {
        Some((picture, mirror)) => frame.render_widget(crate::picture::Picture { frame: &picture, mode, mirror }, inset),
        None => frame.render_widget(
            Paragraph::new(video.camera_note.clone().unwrap_or_default()).style(Style::default().fg(look::dim())).wrap(Wrap { trim: false }),
            inset,
        ),
    }
}

/// An incoming file offer, taking the main pane like an incoming call.
pub(super) fn render_offer_prompt(frame: &mut Frame, area: Rect, app: &App, prompt: &node::TransferStatus) {
    let who = prompt.peer_name.as_deref().unwrap_or("someone");
    let estimate = app.transfer_estimate(who, prompt.transfer.size);
    let room = (area.width as usize).saturating_sub(10);
    let lines = vec![
        Line::from(vec![
            Span::styled(look::fit_middle(who, room.saturating_sub(25)), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" wants to send you a file"),
        ]),
        Line::from(""),
        Line::from(Span::styled(look::fit_middle(&prompt.transfer.name, room), Style::default().fg(look::accent()).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(format!("{}{estimate}", crate::browser::human_size(prompt.transfer.size)), Style::default().fg(look::dim()))),
        Line::from(""),
        look::hints("Esc: later   a: accept   d: decline"),
        Line::from(""),
        Line::from(Span::styled("Only accept what you're expecting.", Style::default().fg(look::dim()))),
    ];
    look::card(frame, area, "Incoming file", look::warn(), lines);
}

/// Where the transfer `picked` is in `ids` (newest first): it stays
/// picked as others come above it. The first, if it's gone or none is.
pub(super) fn picked_at(ids: &[swarm_core::transfer::TransferId], picked: Option<swarm_core::transfer::TransferId>) -> usize {
    picked.and_then(|id| ids.iter().position(|i| *i == id)).unwrap_or(0)
}

/// The transfer `by` rows on from `picked`, within the list.
pub(super) fn moved_to(ids: &[swarm_core::transfer::TransferId], picked: Option<swarm_core::transfer::TransferId>, by: isize) -> Option<swarm_core::transfer::TransferId> {
    let at = picked_at(ids, picked).saturating_add_signed(by).min(ids.len().saturating_sub(1));
    ids.get(at).copied()
}

/// Every transfer, newest first: which way, who, what, how far along.
pub(super) fn render_transfers(frame: &mut Frame, area: Rect, app: &App) {
    use swarm_core::transfer::{Direction, State};
    let all = app.shown_transfers();
    let buttons = button_lines(app, area.width.saturating_sub(4));
    let [area, buttons_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(buttons.len() as u16)]).areas(area);
    frame.render_widget(Paragraph::new(buttons).block(Block::default().padding(ratatui::widgets::Padding::horizontal(2))), buttons_area);
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = all
        .iter()
        .map(|t| {
            let arrow = if t.transfer.direction == Direction::Out { "→" } else { "←" };
            let who = t.peer_name.clone().unwrap_or_else(|| "?".to_string());
            let size = t.transfer.size.max(1);
            let state = match t.transfer.state {
                State::Offered if t.transfer.direction == Direction::In => "offered to you".to_string(),
                State::Offered => "waiting for them to accept".to_string(),
                State::Active => {
                    let bar_width = 20;
                    let done = t.done_bytes.min(size);
                    let filled = (done * bar_width / size) as usize;
                    format!(
                        "{}{} {}%  {} of {}",
                        "█".repeat(filled),
                        "░".repeat(bar_width as usize - filled),
                        done * 100 / size,
                        crate::browser::human_size(done),
                        crate::browser::human_size(t.transfer.size)
                    )
                }
                State::Done if t.transfer.direction == Direction::In => format!("saved to {}", t.transfer.path.display()),
                State::Done => "delivered".to_string(),
                State::Declined => "declined".to_string(),
                State::Cancelled | State::Failed => {
                    if t.transfer.note.is_empty() { "cancelled".to_string() } else { t.transfer.note.clone() }
                }
            };
            let line = format!(
                "{arrow} {:<12} {} ({})  {state}",
                look::fit_middle(&who, 16),
                look::fit_middle(&t.transfer.name, 48),
                crate::browser::human_size(t.transfer.size)
            );
            let style = match t.transfer.state {
                State::Active => Style::default().fg(look::accent()),
                State::Offered => Style::default().fg(look::warn()),
                State::Done => Style::default().fg(look::good()),
                _ => Style::default().fg(look::dim()),
            };
            ListItem::new(Line::from(Span::styled(line.chars().take(width.max(10)).collect::<String>(), style)))
        })
        .collect();
    let whose = match app.selected_peer() {
        Some(row) => format!("Transfers with {} · newest first", row.label),
        None => "All transfers · newest first".to_string(),
    };
    let side = if app.in_transfers { "Tab: choose whose " } else { "Tab: back to the transfers " };
    let block = look::panel(look::title(whose), app.in_transfers).title(Line::from(Span::styled(side, Style::default().fg(look::dim()))).right_aligned());
    if items.is_empty() {
        frame.render_widget(Paragraph::new(Span::styled("  nothing yet — a contact's Send a file sends one", Style::default().fg(look::dim()))).block(block), area);
        return;
    }
    let mut state = ratatui::widgets::ListState::default();
    let ids: Vec<_> = all.iter().map(|t| t.transfer.id).collect();
    state.select(Some(picked_at(&ids, app.transfer_selected)));
    let picked = if app.in_transfers { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default().add_modifier(Modifier::UNDERLINED) };
    frame.render_stateful_widget(List::new(items).block(block).highlight_style(picked), area, &mut state);
}

/// The camera preview panel: the picture, mirrored like a mirror, with
/// what it is and how fast it's going in the title.
pub(super) fn render_preview(frame: &mut Frame, area: Rect, camera: &std::result::Result<swarm_app::camera::Camera, String>, preview: &Preview) {
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(look::good()));
    let camera = match camera {
        Ok(camera) => camera,
        Err(why) => {
            frame.render_widget(
                Paragraph::new(format!("camera unavailable: {why}")).block(block.title("Camera")).wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
    };
    let drawn = preview.drawn.get() + 1;
    preview.drawn.set(drawn);
    let (since, captured_then, drawn_then, mut capture_fps, mut draw_fps) = preview.rates.get();
    let captured = camera.frames_captured();
    let elapsed = since.elapsed().as_secs_f32();
    if elapsed >= 1.0 {
        capture_fps = (captured - captured_then) as f32 / elapsed;
        draw_fps = (drawn - drawn_then) as f32 / elapsed;
        preview.rates.set((Instant::now(), captured, drawn, capture_fps, draw_fps));
    }
    let info = camera.info();
    let title = format!(
        "{} ({}) {}×{} {} — camera {capture_fps:.0} fps, shown {draw_fps:.0} fps — {}",
        info.name,
        info.device,
        info.width,
        info.height,
        info.format,
        preview.mode.name()
    );
    let inner = block.inner(area);
    frame.render_widget(block.title(title), area);
    match camera.latest() {
        Some(picture) => frame.render_widget(crate::picture::Picture { frame: &picture, mode: preview.mode, mirror: !info.faces_away }, inner),
        None => frame.render_widget(Paragraph::new("waiting for the first frame..."), inner),
    }
}

/// The keys of what's waiting on an answer or being typed, for the line
/// under the status line; nothing otherwise (the buttons have the rest).
pub(super) fn prompt_keys(app: &App) -> Option<String> {
    let text = if app.control().asking.is_some() {
        "a: allow   r: refuse".to_string()
    } else if app.removing.is_some() {
        "Esc: cancel   0-9: their number   Enter: remove".to_string()
    } else if app.focus == Focus::Input && app.device_input.is_some() {
        "Esc: cancel   Enter: go".to_string()
    } else if app.offer_prompt.is_some() {
        "Esc: later   a: accept   d: decline".to_string()
    } else if app.browser.is_some() {
        "Esc: cancel   Up/Down: move   Enter: open/choose   Backspace: up   /: filter   .: hidden   ~: home".to_string()
    } else if app.confirm_send.is_some() {
        "y: send   n: don't".to_string()
    } else if let Some(prompt) = app.calls.incoming() {
        if prompt.video { "Left/Right: choose   Enter: that   a: video   s: audio   r: reject".to_string() } else { "Left/Right: choose   Enter: that   s: audio   r: reject".to_string() }
    } else if app.calls.preview().is_some() && app.taking_picture {
        "Esc: not now   Enter: take it   m: colour mode".to_string()
    } else if app.calls.preview().is_some() {
        "Esc: close the preview   m: colour mode".to_string()
    } else if app.focus == Focus::Input && (app.peer_form.is_some() || app.channel_form.is_some()) {
        "Esc: cancel   Enter: next".to_string()
    } else if app.focus == Focus::Input && (app.receive_prompt.is_some() || app.name_prompt.is_some()) {
        "Esc: cancel   Enter: save".to_string()
    } else if let Some(compose) = &app.post_compose {
        match compose {
            PostCompose::Text => "Esc: cancel   Enter: continue".to_string(),
            PostCompose::AskVisibility(_) => "Esc: cancel   y: anyone   n: contacts only".to_string(),
        }
    } else if app.focus == Focus::Input && app.selected_channel().is_some() && app.mode == MainMode::View {
        "Esc: stop typing   Enter: send   /help: commands".to_string()
    } else if app.focus == Focus::Input {
        if app.selected_peer().is_some() && app.mode == MainMode::View {
            "Esc: stop typing   Enter: send   Tab: posts".to_string()
        } else {
            "Esc: stop typing   Enter: send".to_string()
        }
    } else {
        return None;
    };
    Some(text)
}

/// The screen's buttons (`App::buttons`) as lines `width` wide: each
/// `[Label]` with its key lit in it, or after it where the label hasn't
/// the letter; the one chosen with Left/Right lit as a whole.
pub(super) fn button_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (i, button) in app.buttons().iter().enumerate() {
        let spans = button_spans(button, app.button_choice == Some(i));
        let cells: usize = spans.iter().map(|s| s.width()).sum();
        if used > 0 && used + 1 + cells > width as usize {
            lines.push(Line::from(std::mem::take(&mut line)));
            used = 0;
        }
        if used > 0 {
            line.push(Span::raw(" "));
            used += 1;
        }
        line.extend(spans);
        used += cells;
    }
    if !line.is_empty() {
        lines.push(Line::from(line));
    }
    lines
}

pub(super) fn button_spans(button: &Button, chosen: bool) -> Vec<Span<'static>> {
    let style = if chosen { Style::default().fg(look::accent()).add_modifier(Modifier::BOLD | Modifier::REVERSED) } else { Style::default().fg(look::text()) };
    let edge = if chosen { style } else { Style::default().fg(look::dim()) };
    let lit = style.fg(look::accent()).add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let letter = match button.key {
        KeyCode::Char(key) => button.label.char_indices().find(|(_, c)| if key.is_ascii_lowercase() { c.to_ascii_lowercase() == key } else { *c == key }),
        _ => None,
    };
    let mut spans = vec![Span::styled("[", edge)];
    match letter {
        Some((at, c)) => {
            let after = at + c.len_utf8();
            spans.extend([
                Span::styled(button.label[..at].to_string(), style),
                Span::styled(c.to_string(), lit),
                Span::styled(button.label[after..].to_string(), style),
            ]);
        }
        None => {
            let key = match button.key {
                KeyCode::Char(c) => c.to_string(),
                KeyCode::Enter => "↵".to_string(),
                KeyCode::Tab => "Tab".to_string(),
                _ => String::new(),
            };
            spans.extend([Span::styled(format!("{} ", button.label), style), Span::styled(key, lit)]);
        }
    }
    spans.push(Span::styled("]", edge));
    spans
}

/// A panel of posts, newest first: when, then what, wrapped beneath.
/// `note` goes first, dimmed (why these may be out of date).
/// `picked`: the user's post picked to delete (see `App::picked_post`),
/// shown reversed.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_posts(frame: &mut Frame, area: Rect, app: &App, title: Line<'static>, list: &[&posts::Post], note: Option<&str>, empty: &str, picked: Option<&[u8; 64]>) {
    let dim = Style::default().fg(look::dim());
    let block = look::panel(title, false);
    let width = block.inner(area).width.saturating_sub(1) as usize;
    let mut lines = Vec::new();
    if let Some(note) = note {
        lines.push(Line::from(Span::styled(note.to_string(), dim.add_modifier(Modifier::ITALIC))));
        lines.push(Line::from(""));
    }
    if list.is_empty() {
        lines.push(Line::from(Span::styled(empty.to_string(), dim)));
    }
    for post in list {
        let reversed = if picked == Some(&post.signature) { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
        lines.push(Line::from(Span::styled(time::date_time(post.created_at, app.prefs.utc), dim.patch(reversed))));
        for piece in look::wrap_words(&post.text, width.saturating_sub(2)) {
            lines.push(Line::from(Span::styled(format!("  {piece}"), reversed)));
        }
        lines.push(Line::from(""));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// A chat panel, scrolled so the newest lines show (less however far
/// it's been paged back), the lines settled at the bottom like a chat
/// reads. Records the height and overflow for PageUp/PageDown.
pub(super) fn render_chat(frame: &mut Frame, area: Rect, app: &App, block: Block<'static>, items: &[ChatItem], empty: &str) {
    let inner = block.inner(area);
    let height = inner.height as usize;
    let need = height * 2 + app.chat_scroll as usize;
    let picked = app.picked.as_deref();
    let picked_item = picked.and_then(|p| items.iter().position(|i| i.lines == p));
    let mut take = need.min(items.len()).max(picked_item.map_or(0, |at| items.len() - at));
    let (mut lines, picked_lines) = loop {
        let (lines, picked_lines) = look::chat_lines(&items[items.len() - take..], inner.width, &app.prefs, picked);
        if lines.len() >= need || take == items.len() {
            break (lines, picked_lines);
        }
        take = (take * 2).min(items.len());
    };
    let overflow = lines.len().saturating_sub(height).min(u16::MAX as usize) as u16;
    app.chat_view.set((inner.height, overflow));
    let mut back = app.chat_scroll.min(overflow);
    if let Some(range) = picked_lines {
        let top = (overflow - back) as usize;
        let top = if range.start < top || range.end > top + height { range.start.min(range.end.saturating_sub(height)) } else { top };
        back = overflow.saturating_sub(top.min(overflow as usize) as u16);
    }
    let block = match (overflow, back) {
        (0, _) => block,
        (_, 0) => block.title_bottom(Line::from(Span::styled(" PgUp: older ", Style::default().fg(look::dim()))).right_aligned()),
        _ => block.title_bottom(Line::from(Span::styled(" scrolled back · PgDn: newer ", Style::default().fg(look::warn()))).right_aligned()),
    };
    if lines.is_empty() {
        let blank = height.saturating_sub(1);
        let mut padded: Vec<Line> = vec![Line::from(""); blank];
        padded.push(Line::from(Span::styled(empty.to_string(), Style::default().fg(look::dim()))).centered());
        frame.render_widget(Paragraph::new(padded).block(block), area);
        return;
    }
    if lines.len() < height {
        let mut padded = vec![Line::from(""); height - lines.len()];
        padded.append(&mut lines);
        lines = padded;
    }
    frame.render_widget(Paragraph::new(lines).block(block).scroll((overflow - back, 0)), area);
}

/// The text box, framed and lit when it's being typed in.
pub(super) fn render_input(frame: &mut Frame, area: Rect, app: &App, title: &str, placeholder: &str) {
    let focused = app.focus == Focus::Input;
    let block = look::panel(look::title(title), focused);
    lineedit::render(frame, area, block, &app.input, app.input_back, focused, placeholder, Style::default().fg(look::accent()));
}

/// Someone asking to control the screen, taking the main pane like an
/// incoming call.
pub(super) fn render_control_ask(frame: &mut Frame, area: Rect, name: &str, screen: bool) {
    let lines = vec![
        Line::from(Span::styled(if screen { format!("{name} asks to see and control your screen") } else { format!("{name} asks to control your screen") }, Style::default().add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(if screen { "allowing shares your screen with them, and gives them your mouse and keyboard" } else { "your mouse and keyboard, as if they were sitting at it" }, Style::default().fg(look::dim()))),
        Line::from(""),
        look::hints("a: allow   r: refuse"),
        Line::from(""),
        Line::from(Span::styled("x takes it back. Using your own mouse pauses them.", Style::default().fg(look::dim()))),
    ];
    look::card(frame, area, "Control of your screen", look::warn(), lines);
}

/// Your devices (Settings): each one, the one chosen marked, and while
/// one's being added, its code.
/// One of your devices, chosen under You: what's known of it, and its
/// buttons; this one's new name, while it's typed.
pub(super) fn render_device(frame: &mut Frame, area: Rect, app: &App, device: &swarm_core::node::devices::DeviceInfo) {
    let dim = Style::default().fg(look::dim());
    let input_height = if app.device_input.is_some() { lineedit::box_height(&app.input, area.width) } else { 0 };
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(input_height)]).split(area);
    let field = |label: &str, value: String| Line::from(vec![Span::styled(format!("  {label:<9}"), dim), Span::raw(value)]);
    let state = swarm_app::devices::state(device);
    let mut lines = vec![
        Line::from(Span::styled(format!("  {}", swarm_app::devices::label(device)), Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        field("status", state.to_string()),
    ];
    if let Some(in_use) = swarm_app::devices::in_use(&app.node, device) {
        lines.push(field("in use", in_use));
    }
    lines.push(field("added", swarm_app::devices::added(device, swarm_app::unix_now())));
    for (i, address) in swarm_app::devices::addresses(device).into_iter().enumerate() {
        lines.push(field(if i == 0 { "last at" } else { "" }, address));
    }
    lines.push(field("key", base64::engine::general_purpose::STANDARD.encode(device.key)));
    if device.this {
        let limit = actions_limit(app);
        lines.push(field("bandwidth limit", if limit.is_empty() { "none".to_string() } else { format!("{limit}bit/s") }));
        let receive = swarm_app::actions::receive_limit_text(&app.node);
        lines.push(field("receive limit", if receive.is_empty() { "none".to_string() } else { format!("{receive}bit/s") }));
    }
    if !device.this {
        lines.push(field("its calls", swarm_app::sidebar::auto_answer_text(swarm_app::devices::auto_answer(&app.node, &device.key)).to_string()));
        lines.push(field("control", swarm_app::sidebar::auto_control_text(swarm_app::devices::auto_control(&app.node, &device.key)).to_string()));
    }
    lines.push(Line::from(""));
    for mut line in button_lines(app, chunks[0].width.saturating_sub(6)) {
        line.spans.insert(0, Span::raw("  "));
        lines.push(line);
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  Each of your devices has your name, contacts and chats, kept in step. Contacts see your name, never a device's.", dim)));
    let title = if device.this { "This device" } else { "Your device" };
    frame.render_widget(Paragraph::new(lines).block(look::panel(look::title(title), false)).wrap(Wrap { trim: false }), chunks[0]);
    match app.device_input {
        Some(DeviceInput::Rename) => render_input(frame, chunks[1], app, "This device's name", ""),
        Some(DeviceInput::ReceiveLimit) => render_input(frame, chunks[1], app, "The most contacts are asked to send this device, e.g. 1m — emptied, Enter takes the limit away", ""),
        Some(DeviceInput::BandwidthLimit) => render_input(frame, chunks[1], app, "The most this device sends, e.g. 2m (2 Mbit/s) or 500k — emptied, Enter takes the limit away", ""),
        Some(DeviceInput::Address(_)) => render_input(frame, chunks[1], app, "Its address: IPs or host names (with :port if not 1977, separated by commas), or its code — emptied, Enter clears it", ""),
        _ => {}
    }
}

/// The code for adding a device (Add a device, under DEVICES), under You.
pub(super) fn render_device_code(frame: &mut Frame, area: Rect, app: &App) {
    let Some((code, _, _)) = &app.device_code else { return };
    let dim = Style::default().fg(look::dim());
    let lines = vec![
        Line::from("  On the new device: swarm keygen --out ~/.swarm/identity --join '<this line>'"),
        Line::from(Span::styled("  or, in Swarm's window there, Add this device to my user with it. It works once, for ten minutes,", dim)),
        Line::from(Span::styled("  while Swarm runs here. Anyone with it can add a device as you: show it to nobody else. Esc puts it away.", dim)),
        Line::from(format!("  {code}")),
    ];
    render_with_code(frame, area, look::panel(look::title("Add a device"), false), lines, code);
}

/// The manual's contents, in the list's place: every chapter, and the
/// sections of the one open.
pub(super) fn render_help_contents(frame: &mut Frame, area: Rect, app: &App) {
    let manual = swarm_app::manual::get();
    let at = manual.page_of(app.help_at);
    let width = area.width.saturating_sub(2) as usize;
    let mut lines = Vec::new();
    let mut selected = 0;
    for i in manual.listed(at) {
        let section = &manual.sections[i];
        let indent = if section.level == 1 { " " } else { "   " };
        let text = look::fit_middle(&format!("{indent}{} {}", section.number, section.title), width);
        let style = if i == at {
            selected = lines.len();
            Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
        } else if section.level == 1 {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(look::soft())
        };
        lines.push(Line::from(Span::styled(format!("{text:<width$}"), style)));
    }
    let height = area.height.saturating_sub(2) as usize;
    let scroll = selected.saturating_sub(height.saturating_sub(2)) as u16;
    frame.render_widget(Paragraph::new(lines).block(look::panel(look::title("Help"), true)).scroll((scroll, 0)), area);
}

/// A page of the manual, wrapped to the panel and scrolled as far as
/// PgUp/PgDn have taken it (kept inside the page).
pub(super) fn render_help(frame: &mut Frame, area: Rect, app: &App) {
    use swarm_app::manual::LineKind;
    let manual = swarm_app::manual::get();
    let at = manual.page_of(app.help_at);
    let width = area.width.saturating_sub(4) as usize;
    let lines: Vec<Line> = manual
        .lines(manual.page(at), width)
        .into_iter()
        .map(|line| {
            let style = match line.kind {
                LineKind::Title(1 | 2) => Style::default().fg(look::accent()).add_modifier(Modifier::BOLD),
                LineKind::Title(_) | LineKind::Heading => Style::default().add_modifier(Modifier::BOLD),
                LineKind::TableHead => Style::default().fg(look::dim()).add_modifier(Modifier::BOLD),
                LineKind::Code => Style::default().fg(look::accent()),
                LineKind::Picture => Style::default().fg(look::warn()).add_modifier(Modifier::ITALIC),
                LineKind::Body | LineKind::Gap => Style::default(),
            };
            let chars: Vec<char> = line.text.chars().collect();
            let (from, lead) = (line.from.min(chars.len()), line.lead.min(chars.len()));
            let part = |range: std::ops::Range<usize>| chars[range].iter().collect::<String>();
            Line::from(vec![
                Span::raw(" "),
                Span::styled(part(0..from), style),
                Span::styled(part(from..lead), style.add_modifier(Modifier::BOLD)),
                Span::styled(part(lead..chars.len()), style),
            ])
        })
        .collect();
    let height = area.height.saturating_sub(2);
    app.help_height.set(height);
    let most = (lines.len() as u16).saturating_sub(height);
    let scroll = app.help_scroll.get().min(most);
    app.help_scroll.set(scroll);
    let section = &manual.sections[at];
    let title = format!("{} {}", section.number, section.title);
    let keys = if most > 0 { "  ↑↓ section · PgUp/PgDn scroll · Esc back" } else { "  ↑↓ section · Esc back" };
    let block = look::panel(Line::from(vec![Span::styled(format!(" {title} "), Style::default().fg(look::accent()).add_modifier(Modifier::BOLD)), Span::styled(format!("{keys} "), Style::default().fg(look::dim()))]), true);
    frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), area);
}

/// Locked after being left: only the passphrase asked for, nothing else
/// shown.
pub(super) fn render_locked(frame: &mut Frame, area: Rect, typed: &str, why: &str) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled("Swarm is locked", Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(format!("Passphrase: {}", "•".repeat(typed.chars().count()))),
        Line::from(""),
        Line::from(Span::styled(why.to_string(), Style::default().fg(look::dim()))),
        Line::from(Span::styled("Enter: unlock   Ctrl-C: quit", Style::default().fg(look::dim()))),
    ];
    let width = area.width.min(50);
    let box_area = Rect { x: area.x + (area.width - width) / 2, y: area.y + area.height.saturating_sub(9) / 2, width, height: area.height.min(9) };
    frame.render_widget(Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center).block(look::panel(look::title("Locked"), false)), box_area);
}

/// How many lines down to start a list `height` lines high, so the lines
/// from `shown.0` to `shown.1` (the selected entry's) are all in it: from
/// the top while they fit there. Settings had more lines than a 35-row
/// terminal, and those past the bottom couldn't be seen.
pub(super) fn scroll_to_show(shown: (usize, usize), height: usize) -> usize {
    shown.1.saturating_sub(height).min(shown.0)
}

/// A call's meters in the top line: this many cells each.
pub(super) const CALL_METER_CELLS: usize = 5;

/// A voice's level, 0 to 1 (see swarm_app's meter.rs), as a meter
/// `cells` wide: lit up to it, the rest dim.
pub(super) fn meter_spans(level: f32, cells: usize) -> [Span<'static>; 2] {
    let lit = ((level.clamp(0.0, 1.0) * cells as f32).round() as usize).min(cells);
    [Span::styled("■".repeat(lit), Style::default().fg(look::good())), Span::styled("■".repeat(cells - lit), Style::default().fg(look::dim()))]
}

/// Settings (`s`): your name, and how things are shown, in sections; the
/// selected one's help under it.
pub(super) fn render_settings(frame: &mut Frame, area: Rect, app: &App) {
    let input_height = if app.name_prompt.is_some() { lineedit::box_height(&app.input, area.width) } else { 0 };
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(input_height)]).split(area);
    let mut lines = Vec::new();
    let mut shown = (0, 0);
    let mut section = "";
    for (i, setting) in prefs::SETTINGS.iter().enumerate() {
        let first = lines.len();
        if setting.section() != section {
            section = setting.section();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(format!("  {section}"), Style::default().fg(look::accent()).add_modifier(Modifier::BOLD))));
        }
        let value = match setting {
            prefs::Setting::Name => my_name().unwrap_or_else(|| "not set".to_string()),
            prefs::Setting::Status => Some(swarm_app::avatar::status(&app.identity.public_key_bytes(), true)).filter(|s| !s.is_empty()).unwrap_or_else(|| "none".to_string()),
            prefs::Setting::Picture if swarm_app::avatar::pixels(&app.identity.public_key_bytes(), true).is_some() => "set".to_string(),
            prefs::Setting::Picture => "none".to_string(),
            _ => app.prefs.value(*setting),
        };
        let selected = i == app.setting_selected;
        let label_style = if selected { Style::default().fg(look::accent()).add_modifier(Modifier::BOLD) } else { Style::default().fg(look::soft()) };
        lines.push(Line::from(vec![
            Span::styled(if selected { "  › " } else { "    " }, Style::default().fg(look::accent())),
            Span::styled(format!("{:<19}", setting.label()), label_style),
            Span::styled(value, if selected { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() }),
        ]));
        if selected {
            lines.push(Line::from(Span::styled(format!("                       {}", setting.help()), Style::default().fg(look::dim()))));
            for mut line in button_lines(app, chunks[0].width.saturating_sub(25)) {
                line.spans.insert(0, Span::raw(" ".repeat(23)));
                lines.push(line);
            }
            shown = (first, lines.len());
        }
        if *setting == prefs::Setting::Level {
            let mut line = vec![Span::raw("    "), Span::styled(format!("{:<19}", "Microphone"), Style::default().fg(look::soft()))];
            match app.calls.microphone_level() {
                Some(Ok(level)) => line.extend(meter_spans(level, 20)),
                Some(Err(why)) => line.push(Span::styled(format!("can't be heard: {why}"), Style::default().fg(look::dim()))),
                None => {}
            }
            lines.push(Line::from(line));
        }
    }
    let scroll = scroll_to_show(shown, chunks[0].height.saturating_sub(2) as usize);
    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)).block(look::panel(look::title("Settings"), false)), chunks[0]);
    if let Some(prompt) = app.name_prompt {
        render_input(frame, chunks[1], app, prompt.label(), "");
    }
}

/// This device's bandwidth limit, as typed (empty: none).
fn actions_limit(app: &App) -> String {
    swarm_app::actions::bandwidth_limit_text(&app.node)
}
