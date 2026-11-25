//! The mouse wheel scrolls a chat back from its newest lines (its own
//! process: Slint takes one platform per process).

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::WindowEvent;
use slint::{ComponentHandle, LogicalPosition, VecModel};
use std::rc::Rc;
use swarm_desktop::{App, AppWindow, ChatLine, LineKind, Pane};

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

#[test]
fn the_wheel_scrolls_a_chat_back_from_its_newest() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen { window: window.clone() })).unwrap();
    let ui = AppWindow::new().unwrap();
    swarm_desktop::wire_keys(&ui);
    window.set_size(slint::PhysicalSize::new(1100, 700));
    ui.global::<App>().set_view(Pane::Channel);
    ui.global::<App>().set_chat(swarm_desktop::ChatShown { key: "#somewhere".into(), ..Default::default() });
    let lines: Vec<ChatLine> = (0..300)
        .map(|i| ChatLine { kind: LineKind::Said, time: "12:00".into(), name: format!("person{}", i % 7).into(), role: 0, text: "word ".repeat(1 + (i * 37) % 90).into(), mark: "".into(), for_me: false, linked: false, rich: Default::default(), refs: "".into(), deleting: false, first: true })
        .collect();
    ui.global::<App>().set_lines(Rc::new(VecModel::from(lines)).into());
    for _ in 0..5 {
        draw(&window);
    }
    assert!(!ui.get_chat_back(), "opens at the newest");
    let over_the_chat = LogicalPosition::new(400.0, 300.0);
    ui.window().dispatch_event(WindowEvent::PointerMoved { position: over_the_chat });
    for step in [10.0, 60.0] {
        for _ in 0..6 {
            ui.window().dispatch_event(WindowEvent::PointerScrolled { position: over_the_chat, delta_x: 0.0, delta_y: step });
            for _ in 0..3 {
                draw(&window);
            }
        }
        assert!(ui.get_chat_back(), "scrolled back by six steps of {step}px");
        for _ in 0..40 {
            ui.window().dispatch_event(WindowEvent::PointerScrolled { position: over_the_chat, delta_x: 0.0, delta_y: -step });
            draw(&window);
        }
        assert!(!ui.get_chat_back(), "back at the newest after {step}px steps");
    }
}
