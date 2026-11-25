//! The window drawn in memory, not on a screen: `snapshot_to_file` for
//! checking the layout with no screen (`swarm --snapshot <file.ppm>
//! [--size WxH] [--wait SECS] [--select N] [--view
//! chats|contacts|channels|transfers|settings|help[:<section>]] [--keys
//! ...]`), and the platforms the tests draw on.

use super::*;

/// A platform with one window drawn in memory, for `--snapshot`.
pub struct Offscreen {
    pub window: std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
}

impl slint::platform::Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

/// Offscreen, with a clipboard of its own: what's copied, for the tests.
#[cfg(test)]
pub(crate) struct Clipped {
    pub(crate) window: std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
    pub(crate) clipboard: std::rc::Rc<std::cell::RefCell<Option<String>>>,
}

#[cfg(test)]
impl slint::platform::Platform for Clipped {
    fn create_window_adapter(&self) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
    fn set_clipboard_text(&self, text: &str, clipboard: slint::platform::Clipboard) {
        if clipboard == slint::platform::Clipboard::DefaultClipboard {
            *self.clipboard.borrow_mut() = Some(text.into());
        }
    }
    fn clipboard_text(&self, clipboard: slint::platform::Clipboard) -> Option<String> {
        (clipboard == slint::platform::Clipboard::DefaultClipboard).then(|| self.clipboard.borrow().clone()).flatten()
    }
}

/// Runs the app with no window for `wait` seconds (so connections come
/// up), optionally selects list row `select` and shows view `view`, then
/// draws the screen into `out` as a PPM image.
/// `keys`, if given, are pressed before drawing — separated by commas:
/// a single character is that key, and `Up`, `Down`, `Left`, `Right`, `Return`,
/// `Escape`, `Tab`, `Backspace`, `PageUp`, `PageDown` the named ones; text
/// is typed with `type:<text>` (underscores for spaces), `click:XxY`
/// clicks there (in logical pixels), and `size:WxH` resizes the window.
/// `wait:SECS` waits (for the other side to answer, say).
/// `bigger` and `smaller` step the text size (Ctrl + and Ctrl −).
/// `shot:<file>` saves the screen as it is then, as `out` is at the end.
/// Each is drawn once after, as it would be on a screen.
pub fn snapshot_to_file(out: &str, size: Option<String>, wait: Option<String>, select: Option<String>, view: Option<String>, keys: Option<String>) -> anyhow::Result<()> {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    let (mut width, mut height) = size
        .and_then(|s| s.split_once('x').and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))))
        .unwrap_or((1280, 820));
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen { window: window.clone() })).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let ui = AppWindow::new()?;
    window.set_size(slint::PhysicalSize::new(width, height));

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let (tx, rx) = mpsc::unbounded_channel();
    wire(&ui.global::<App>(), tx.clone());
    wire_keys(&ui);
    help::wire(&ui);
    runtime.spawn(controller::run(Ui::of(&ui), rx, None, true));
    let wait = wait.and_then(|w| w.parse::<f64>().ok()).unwrap_or(3.0);
    std::thread::sleep(Duration::from_secs_f64(wait));
    let ask = |action: Action| {
        let _ = tx.send(action);
        std::thread::sleep(Duration::from_millis(500));
    };
    if let Some(view) = view {
        let (view, section) = view.split_once(':').map_or((view.clone(), None), |(v, n)| (v.to_string(), Some(n.to_string())));
        if let Some(at) = section.and_then(|n| swarm_app::manual::get().numbered(&n)) {
            help::show(&ui.global::<App>(), at);
        }
        let pane = match view.as_str() {
            "chats" => Pane::Mine,
            "contacts" | "peers" => Pane::Peers,
            "channels" => Pane::Channels,
            "transfers" => Pane::Transfers,
            "settings" => Pane::Settings,
            "help" => Pane::Help,
            other => anyhow::bail!("no view {other:?}: contacts, chats, channels, transfers, settings or help[:<section>]"),
        };
        ask(Action::ShowView(pane));
    }
    if let Some(i) = select.and_then(|i| i.parse::<usize>().ok()) {
        let (reply, got) = oneshot::channel();
        let _ = tx.send(Action::Snapshot(reply));
        let _ = runtime.block_on(got);
        ask(Action::Select(i));
    }
    let catch_up = || -> anyhow::Result<()> {
        let (reply, got) = oneshot::channel();
        tx.send(Action::Snapshot(reply)).map_err(|_| anyhow::anyhow!("the app stopped"))?;
        let snap = runtime.block_on(got)?;
        snapshot::apply(&ui.global::<App>(), ui.window(), snap);
        let (reply, got) = oneshot::channel();
        if tx.send(Action::Frames(reply)).is_ok() {
            if let Ok(frames) = runtime.block_on(got) {
                snapshot::apply_frames(&ui.global::<App>(), ui.window(), frames);
            }
        }
        let (reply, got) = oneshot::channel();
        if tx.send(Action::Levels(reply)).is_ok() {
            if let Ok(levels) = runtime.block_on(got) {
                snapshot::apply_levels(&ui.global::<App>(), levels);
            }
        }
        slint::platform::update_timers_and_animations();
        Ok(())
    };
    catch_up()?;
    let pair = |text: &str| text.split_once('x').and_then(|(a, b)| Some((a.parse::<f32>().ok()?, b.parse::<f32>().ok()?)));
    let draw = |width: u32, height: u32| {
        let size = window.size();
        let mut drawn = vec![slint::Rgb8Pixel::new(0, 0, 0); (size.width * size.height) as usize];
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut drawn, size.width as usize);
        });
        let mut buffer = vec![slint::Rgb8Pixel::new(0, 0, 0); (width * height) as usize];
        let w = width.min(size.width) as usize;
        for y in 0..height.min(size.height) as usize {
            buffer[y * width as usize..y * width as usize + w].copy_from_slice(&drawn[y * size.width as usize..y * size.width as usize + w]);
        }
        buffer
    };
    draw(width, height);
    let save = |path: &str, width: u32, height: u32, buffer: &[slint::Rgb8Pixel]| {
        let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
        for px in buffer {
            ppm.extend_from_slice(&[px.r, px.g, px.b]);
        }
        std::fs::write(path, ppm)
    };
    for word in keys.unwrap_or_default().split(',').map(str::trim).filter(|w| !w.is_empty()) {
        let press = |text: slint::SharedString| {
            ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
            ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
        };
        if let Some(secs) = word.strip_prefix("wait:").and_then(|w| w.parse::<f64>().ok()) {
            std::thread::sleep(Duration::from_secs_f64(secs));
        } else if word == "bigger" || word == "smaller" {
            let _ = tx.send(Action::TextSize(if word == "bigger" { 1 } else { -1 }));
        } else if let Some((w, h)) = word.strip_prefix("size:").and_then(pair) {
            (width, height) = (w as u32, h as u32);
            window.set_size(slint::PhysicalSize::new(width, height));
        } else if let Some((x, y)) = word.strip_prefix("click:").and_then(pair) {
            use slint::platform::{PointerEventButton, WindowEvent};
            let position = slint::LogicalPosition::new(x, y);
            ui.window().dispatch_event(WindowEvent::PointerMoved { position });
            ui.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
            ui.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
        } else if let Some(path) = word.strip_prefix("shot:") {
            save(path, width, height, &draw(width, height))?;
        } else if let Some(text) = word.strip_prefix("type:") {
            for c in text.replace('_', " ").chars() {
                press(c.to_string().into());
            }
        } else {
            let key = match word {
                "Up" => slint::platform::Key::UpArrow.into(),
                "Down" => slint::platform::Key::DownArrow.into(),
                "Left" => slint::platform::Key::LeftArrow.into(),
                "Right" => slint::platform::Key::RightArrow.into(),
                "Return" => slint::platform::Key::Return.into(),
                "Escape" => slint::platform::Key::Escape.into(),
                "Tab" => slint::platform::Key::Tab.into(),
                "Backspace" => slint::platform::Key::Backspace.into(),
                "PageUp" => slint::platform::Key::PageUp.into(),
                "PageDown" => slint::platform::Key::PageDown.into(),
                "F1" => slint::platform::Key::F1.into(),
                "Comma" => ",".into(),
                "Underscore" => "_".into(),
                other => slint::SharedString::from(other),
            };
            press(key);
        }
        std::thread::sleep(Duration::from_millis(600));
        catch_up()?;
        draw(width, height);
    }

    save(out, width, height, &draw(width, height))?;
    let (done_tx, done_rx) = oneshot::channel();
    if tx.send(Action::Quit(done_tx)).is_ok() {
        let _ = runtime.block_on(async { tokio::time::timeout(Duration::from_secs(3), done_rx).await });
    }
    Ok(())
}
