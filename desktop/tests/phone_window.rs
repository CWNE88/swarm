//! The phone's window (the Android app's), at a phone's size: the
//! passphrase typed straight away, a chat tapped open full screen and
//! Back closing it, the tabs along the top, and a setting's help tapped (its
//! own process: Slint takes one platform per process).

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, SharedString};
use std::cell::RefCell;
use std::rc::Rc;
use swarm_desktop::{App, PhoneWindow, Row, Pane, RowKind};

use swarm_desktop::Offscreen;

fn draw(window: &MinimalSoftwareWindow) {
    slint::platform::update_timers_and_animations();
    let size = window.size();
    let mut buffer = vec![slint::Rgb8Pixel::new(0, 0, 0); (size.width * size.height) as usize];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, size.width as usize);
    });
}

fn press(ui: &PhoneWindow, text: impl Into<SharedString>) {
    let text = text.into();
    ui.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window().dispatch_event(WindowEvent::KeyReleased { text });
}

fn tap(ui: &PhoneWindow, window: &MinimalSoftwareWindow, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    ui.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
    ui.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
    ui.window().dispatch_event(WindowEvent::PointerExited);
    draw(window);
}

#[test]
fn the_phone_window_takes_one_thing_at_a_time() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen { window: window.clone() })).unwrap();
    let ui = PhoneWindow::new().unwrap();
    window.set_size(slint::PhysicalSize::new(412, 915));
    ui.show().unwrap();
    let app = ui.global::<App>();

    let unlocked = Rc::new(RefCell::new(Vec::new()));
    let got = unlocked.clone();
    app.on_unlock(move |pass| got.borrow_mut().push(pass.to_string()));
    app.set_view(Pane::Unlock);
    draw(&window);
    for c in "five words".chars() {
        press(&ui, c.to_string());
    }
    press(&ui, slint::platform::Key::Return);
    assert_eq!(*unlocked.borrow(), ["five words"], "typed straight into the passphrase");

    let selected = Rc::new(RefCell::new(Vec::new()));
    let got = selected.clone();
    app.on_select(move |i| got.borrow_mut().push(i));
    let views = Rc::new(RefCell::new(Vec::new()));
    let got = views.clone();
    app.on_show_view(move |v| got.borrow_mut().push(v));
    let rows = vec![
        Row { kind: RowKind::You, name: "you".into(), ..Default::default() },
        Row { kind: RowKind::Heading, name: "CONTACTS".into(), ..Default::default() },
        Row { kind: RowKind::Peer, name: "Bob".into(), ..Default::default() },
    ];
    app.set_rows(Rc::new(slint::VecModel::from(rows)).into());
    app.set_view(Pane::Mine);
    draw(&window);
    assert!(!ui.get_open());
    let mut y = 100.0;
    while selected.borrow().is_empty() && y < 400.0 {
        tap(&ui, &window, 200.0, y);
        y += 8.0;
    }
    assert_eq!(*selected.borrow(), [0], "the first row tapped, you");
    assert!(ui.get_open(), "opened, over the list");
    press(&ui, slint::platform::Key::Back);
    assert!(!ui.get_open(), "Back closed it");
    while selected.borrow().last() != Some(&2) && y < 400.0 {
        tap(&ui, &window, 200.0, y);
        if selected.borrow().last() == Some(&1) {
            assert!(!ui.get_open(), "a heading isn't opened over the list");
        }
        if ui.get_open() {
            press(&ui, slint::platform::Key::Back);
        }
        y += 8.0;
    }
    selected.borrow_mut().dedup();
    assert_eq!(*selected.borrow(), [0, 1, 2], "you, the heading, then Bob");
    tap(&ui, &window, 200.0, y - 8.0);
    assert!(ui.get_open());
    app.set_view(Pane::Peer);
    draw(&window);
    let sending = Rc::new(RefCell::new(0));
    let got = sending.clone();
    app.on_send_file(move || *got.borrow_mut() += 1);
    let mut y = 860.0;
    while *sending.borrow() == 0 && y < 915.0 {
        let mut x = 4.0;
        while *sending.borrow() == 0 && x < 412.0 {
            tap(&ui, &window, x, y);
            x += 8.0;
        }
        y += 8.0;
    }
    assert_eq!(*sending.borrow(), 1, "a file to send, from their chat");
    assert!(ui.get_open(), "still in their chat");
    press(&ui, slint::platform::Key::Back);
    assert!(!ui.get_open());
    press(&ui, slint::platform::Key::Back);
    assert!(views.borrow().is_empty(), "nothing more for Back to do: {:?}", views.borrow());

    let mut y = 4.0;
    while views.borrow().is_empty() && y < 120.0 {
        tap(&ui, &window, 412.0 * 0.5 / 4.0, y);
        y += 4.0;
    }
    assert_eq!(*views.borrow(), [Pane::Peers], "Contacts tapped");
    assert!(!ui.get_open(), "nothing tapped open on the way");
    app.set_view(Pane::Peers);
    draw(&window);
    press(&ui, slint::platform::Key::Back);
    assert_eq!(*views.borrow(), [Pane::Peers, Pane::Mine], "Back from Contacts to Chats");

    app.set_view(Pane::Mine);
    draw(&window);
    let mut y = 52.0;
    while !ui.get_open() && y < 400.0 {
        tap(&ui, &window, 200.0, y);
        y += 8.0;
    }
    assert!(ui.get_open(), "someone tapped open in Chats");
    app.set_view(Pane::Peer);
    draw(&window);
    let tab = |ui: &PhoneWindow, pane: Pane, x: f32| {
        views.borrow_mut().clear();
        let mut y = 48.0;
        while views.borrow().is_empty() && y < 160.0 {
            tap(ui, &window, x, y);
            y += 4.0;
        }
        assert_eq!(*views.borrow(), [pane], "its tab tapped");
        app.set_view(pane);
        draw(&window);
    };
    tab(&ui, Pane::Peers, 412.0 * 0.5 / 5.0 + 8.0);
    assert!(ui.get_open(), "still open in Contacts");
    tab(&ui, Pane::Settings, 412.0 * 3.5 / 5.0);
    assert!(!ui.get_open(), "nothing open in Settings");
    views.borrow_mut().clear();

    app.set_view(Pane::Settings);
    draw(&window);
    let mut at = None;
    let mut y = 100.0;
    while at.is_none() && y < 700.0 {
        tap(&ui, &window, 30.0, y);
        if !ui.get_tip().is_empty() {
            at = Some(y);
        }
        y += 6.0;
    }
    let y = at.expect("a setting's name tapped shows its help");
    tap(&ui, &window, 30.0, y);
    assert_eq!(ui.get_tip(), "", "tapped again, gone");

    let pressed = Rc::new(RefCell::new(Vec::<&str>::new()));
    let p = pressed.clone();
    app.on_toggle_mute(move || p.borrow_mut().push("mute"));
    let p = pressed.clone();
    app.on_toggle_camera(move || p.borrow_mut().push("camera"));
    let p = pressed.clone();
    app.on_use_camera(move |i| p.borrow_mut().push(["selfie", "back"][i as usize]));
    let p = pressed.clone();
    app.on_hang_up(move || p.borrow_mut().push("hang up"));
    let p = pressed.clone();
    app.on_send_file(move || p.borrow_mut().push("file"));
    app.set_view(Pane::Mine);
    app.set_call(swarm_desktop::CallShown { live: true, video_shown: true, local_on: true, ..app.get_call() });
    let cameras = |names: &[&str]| app.set_call_cameras(std::rc::Rc::new(slint::VecModel::from(names.iter().map(|c| SharedString::from(*c)).collect::<Vec<_>>())).into());
    let buttons = |names: &[&str]| {
        cameras(names);
        draw(&window);
        pressed.borrow_mut().clear();
        let mut x = 4.0;
        while x < 412.0 {
            let before = pressed.borrow().len();
            tap(&ui, &window, x, 880.0);
            if pressed.borrow().len() == before {
                tap(&ui, &window, 206.0, 400.0);
            }
            x += 6.0;
        }
        let mut seen: Vec<&str> = pressed.borrow().clone();
        seen.dedup();
        seen
    };
    assert_eq!(buttons(&["Selfie"]), ["mute", "camera", "file", "hang up"]);
    pressed.borrow_mut().clear();
    let mut camera_x = 4.0;
    while pressed.borrow().last() != Some(&"camera") {
        tap(&ui, &window, camera_x, 880.0);
        camera_x += 4.0;
        assert!(camera_x < 412.0, "Camera found");
    }
    let chosen = |on: bool| {
        cameras(&["Selfie", "Back"]);
        app.set_call(swarm_desktop::CallShown { local_on: on, can_camera: true, ..app.get_call() });
        draw(&window);
        pressed.borrow_mut().clear();
        let mut y = 860.0;
        while y > 600.0 {
            tap(&ui, &window, camera_x, 880.0);
            tap(&ui, &window, camera_x, y);
            y -= 4.0;
        }
        let mut seen: Vec<&str> = pressed.borrow().clone();
        seen.dedup();
        seen
    };
    assert_eq!(chosen(true), ["camera", "back"], "Off, then Back: Selfie's on already");
    assert_eq!(chosen(false), ["back", "selfie"], "no Off while it's off");
}
