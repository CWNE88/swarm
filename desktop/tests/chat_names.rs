//! A chat's name column is as wide as its widest name drawn, not its
//! name with most letters (its own process: Slint takes one platform per
//! process).

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::{ComponentHandle, VecModel};
use std::rc::Rc;
use swarm_desktop::{App, AppWindow, ChatLine, LineKind, Pane};

use swarm_desktop::Offscreen;

fn frame(ui: &AppWindow, window: &MinimalSoftwareWindow, names: &str) -> Vec<slint::Rgb8Pixel> {
    ui.global::<App>().set_chat(swarm_desktop::ChatShown { key: "#somewhere".into(), widest_name: names.into(), ..Default::default() });
    let size = window.size();
    let mut buffer = vec![slint::Rgb8Pixel::new(0, 0, 0); (size.width * size.height) as usize];
    for _ in 0..5 {
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, size.width as usize);
        });
    }
    buffer
}

#[test]
fn the_name_column_fits_the_widest_name_drawn() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen { window: window.clone() })).unwrap();
    let ui = AppWindow::new().unwrap();
    window.set_size(slint::PhysicalSize::new(1100, 700));
    ui.global::<App>().set_view(Pane::Channel);
    let (narrow, wide) = ("Lilli ilji", "MWMW");
    let lines: Vec<ChatLine> = [narrow, wide]
        .iter()
        .map(|name| ChatLine { kind: LineKind::Said, time: "12:00".into(), name: (*name).into(), role: 0, text: "hello".into(), mark: "".into(), for_me: false, linked: false, rich: Default::default(), refs: "".into(), deleting: false, first: true })
        .collect();
    ui.global::<App>().set_lines(Rc::new(VecModel::from(lines)).into());
    let by_narrow = frame(&ui, &window, narrow);
    let by_wide = frame(&ui, &window, wide);
    let by_both = frame(&ui, &window, &format!("{narrow}\n{wide}"));
    assert!(by_narrow != by_wide, "the two names size the column differently");
    assert!(by_both == by_wide, "both names, one per line, size it by the wider drawn");
}
