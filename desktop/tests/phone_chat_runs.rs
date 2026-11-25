//! On a narrow screen a chat stacks: a run of one person's messages has
//! their name once, above it, and the text the whole width; wider, every
//! line has its name in a column (its own process: Slint takes one
//! platform per process).

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, VecModel};
use std::rc::Rc;
use swarm_desktop::{App, ChatLine, LineKind, Offscreen, Pane, PhoneWindow, Row, RowKind};

fn draw(window: &MinimalSoftwareWindow) {
    let size = window.size();
    let mut buffer = vec![slint::Rgb8Pixel::new(0, 0, 0); (size.width * size.height) as usize];
    for _ in 0..4 {
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, size.width as usize);
        });
    }
}

#[test]
fn a_narrow_chat_names_a_run_once() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen { window: window.clone() })).unwrap();
    let ui = PhoneWindow::new().unwrap();
    window.set_size(slint::PhysicalSize::new(1080, 2400));
    ui.show().unwrap();
    let app = ui.global::<App>();
    let said = |name: &str, text: &str, first: bool| ChatLine { kind: LineKind::Said, time: "12:00".into(), name: name.into(), role: 0, text: text.into(), mark: "".into(), for_me: false, linked: false, rich: Default::default(), refs: "".into(), deleting: false, first };
    let lines = vec![said("Dave", "one", true), said("Dave", "two", false), said("Dave", "three", false), said("Owen", "four", true)];
    app.set_lines(Rc::new(VecModel::from(lines)).into());
    app.set_chat(swarm_desktop::ChatShown { key: "dave".into(), widest_name: "Dave\nPaul".into(), show_times: true, ..Default::default() });
    app.set_rows(Rc::new(VecModel::from(vec![Row { kind: RowKind::Peer, name: "Dave".into(), letter: "B".into(), ..Default::default() }])).into());
    app.set_view(Pane::Mine);
    draw(&window);
    let mut y = 40.0;
    while !ui.get_open() && y < 400.0 {
        let p = LogicalPosition::new(200.0, y);
        ui.window().dispatch_event(WindowEvent::PointerPressed { position: p, button: PointerEventButton::Left });
        ui.window().dispatch_event(WindowEvent::PointerReleased { position: p, button: PointerEventButton::Left });
        ui.window().dispatch_event(WindowEvent::PointerExited);
        draw(&window);
        y += 4.0;
    }
    assert!(ui.get_open(), "their chat open");
    app.set_view(Pane::Peer);
    let shown = |name: &'static str| {
        use i_slint_backend_testing::ElementRoot;
        ui.root_element().query_descendants().match_predicate(move |e| e.accessible_label().as_deref() == Some(name)).find_all().len()
    };
    let at = |scale: f32| {
        ui.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
        ui.window().dispatch_event(WindowEvent::Resized { size: slint::LogicalSize::new(1080.0 / scale, 2400.0 / scale) });
        draw(&window);
    };
    at(2.625);
    assert_eq!((shown("Dave"), shown("Owen")), (1, 1), "each run named once");
    assert_eq!(shown("three"), 1, "the run's lines all there");
    at(1.5);
    assert_eq!((shown("Dave"), shown("Owen")), (3, 1), "every line named");
}
