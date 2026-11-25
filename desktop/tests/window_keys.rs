//! The window's ready for what comes next: the passphrase typed straight
//! away, the keys working once it's taken, a peer called with c and v,
//! Settings scrolling when they're taller than the window, and a setting's
//! help on the mouse (its own process: Slint takes one platform per
//! process).

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::WindowEvent;
use slint::{ComponentHandle, LogicalPosition, Model, SharedString};
use std::cell::RefCell;
use std::rc::Rc;
use swarm_desktop::{App, AppWindow, Pane, PeerDetails, RequestDetails, TransferAnswer, TransferState};

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

fn press(ui: &AppWindow, text: impl Into<SharedString>) {
    let text = text.into();
    ui.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window().dispatch_event(WindowEvent::KeyReleased { text });
}

#[test]
fn the_window_is_ready_for_what_comes_next() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen { window: window.clone() })).unwrap();
    let ui = AppWindow::new().unwrap();
    swarm_desktop::wire_keys(&ui);
    window.set_size(slint::PhysicalSize::new(1200, 780));
    ui.show().unwrap();

    let unlocked = Rc::new(RefCell::new(Vec::new()));
    let got = unlocked.clone();
    ui.global::<App>().on_unlock(move |pass| got.borrow_mut().push(pass.to_string()));
    let used = Rc::new(RefCell::new(0));
    let got = used.clone();
    ui.global::<App>().on_used(move || *got.borrow_mut() += 1);
    ui.global::<App>().set_view(Pane::Unlock);
    draw(&window);
    for c in "five words".chars() {
        press(&ui, c.to_string());
    }
    press(&ui, slint::platform::Key::Return);
    assert_eq!(*unlocked.borrow(), ["five words"], "typed straight into the passphrase");
    assert_eq!(*used.borrow(), 11, "each key typed into the passphrase, and Enter");

    let views = Rc::new(RefCell::new(Vec::new()));
    let got = views.clone();
    ui.global::<App>().on_show_view(move |v| got.borrow_mut().push(v));
    ui.global::<App>().set_view(Pane::Mine);
    draw(&window);
    press(&ui, "s");
    assert_eq!(*views.borrow(), [Pane::Settings], "s: Settings");

    let calls = Rc::new(RefCell::new(Vec::new()));
    let got = calls.clone();
    ui.global::<App>().on_place_call(move |video| got.borrow_mut().push(video));
    ui.global::<App>().set_view(Pane::Peer);
    draw(&window);
    press(&ui, "c");
    press(&ui, "v");
    assert_eq!(*calls.borrow(), [false, true], "c a call, v a video call");
    ui.global::<App>().set_view(Pane::Channels);
    draw(&window);
    press(&ui, "v");
    assert_eq!(calls.borrow().len(), 2);

    ui.global::<App>().set_view(Pane::Peers);
    ui.global::<App>().set_pd(PeerDetails { request: true, ..Default::default() });
    ui.global::<App>().set_req(RequestDetails { suggested: "bob".into(), address: "[fd99::2]:1977".into(), key: "S2V5S2V5".into(), ..Default::default() });
    draw(&window);
    press(&ui, "a");
    draw(&window);
    assert_eq!(ui.get_form_filled(), "bob|[fd99::2]:1977|S2V5S2V5");
    press(&ui, slint::platform::Key::Escape);
    ui.global::<App>().set_pd(PeerDetails::default());

    ui.global::<App>().set_pd(PeerDetails { shown: true, ..Default::default() });
    ui.global::<App>().invoke_keys_changed();
    draw(&window);
    let hint_keys = || ui.global::<App>().get_hints().iter().map(|h| h.key.to_string()).collect::<Vec<_>>();
    assert!(hint_keys().contains(&"o".to_string()), "{:?}", hint_keys());
    assert!(!hint_keys().contains(&"l".to_string()), "{:?}", hint_keys());
    ui.global::<App>().set_pd(PeerDetails::default());

    let asked = Rc::new(RefCell::new(Vec::new()));
    let got = asked.clone();
    ui.global::<App>().on_transfer(move |i, what| got.borrow_mut().push((i, what)));
    let item = swarm_desktop::TransferItem { arrow: "↓".into(), who: "bob".into(), name: "gift.txt".into(), size: "1 KB".into(), state: "offered".into(), kind: TransferState::Offered, can_cancel: true, ..Default::default() };
    ui.global::<App>().set_transfers(Rc::new(slint::VecModel::from(vec![item])).into());
    ui.global::<App>().set_view(Pane::Transfers);
    draw(&window);
    let click = |x: f32, y: f32| {
        let position = LogicalPosition::new(x, y);
        ui.window().dispatch_event(WindowEvent::PointerPressed { position, button: slint::platform::PointerEventButton::Left });
        ui.window().dispatch_event(WindowEvent::PointerReleased { position, button: slint::platform::PointerEventButton::Left });
        draw(&window);
    };
    let hit = (0..780).step_by(6).any(|y| (0..60).any(|n| {
        click(1180.0 - n as f32 * 4.0, y as f32);
        !asked.borrow().is_empty()
    }));
    assert!(hit, "a row's Cancel was clicked");
    assert_eq!(*asked.borrow(), [(0, TransferAnswer::Cancel)], "Cancel on the first");
    ui.global::<App>().set_transfers(Rc::new(slint::VecModel::<swarm_desktop::TransferItem>::default()).into());

    ui.global::<App>().set_view(Pane::Settings);
    draw(&window);
    let over = |x: f32, y: f32| {
        ui.window().dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(x, y) });
        draw(&window);
    };
    let found = (0..780).step_by(4).find(|y| {
        over(300.0, *y as f32);
        ui.get_tip().contains("speakers")
    });
    assert!(found.is_some(), "Echo's help came up somewhere down the names");
    over(900.0, 10.0);
    assert_eq!(ui.get_tip(), "", "and went with the mouse");

    window.set_size(slint::PhysicalSize::new(760, 480));
    ui.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.75 });
    ui.window().dispatch_event(WindowEvent::Resized { size: slint::LogicalSize::new(760.0 / 1.75, 480.0 / 1.75) });
    draw(&window);
    assert_eq!(ui.get_settings_scrolled(), 0.0);
    for _ in 0..20 {
        ui.window().dispatch_event(WindowEvent::PointerScrolled { position: LogicalPosition::new(300.0, 150.0), delta_x: 0.0, delta_y: -120.0 });
        draw(&window);
    }
    assert!(ui.get_settings_scrolled() > 200.0, "scrolled {}", ui.get_settings_scrolled());
}
