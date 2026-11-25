//! The phone's own picture in a video call, in its box in the corner: the
//! box the picture's shape, upright or on its side as the phone's held, and
//! as big either way, a thin black line round it (its own process: Slint
//! takes one platform per process).

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::{ComponentHandle, Image, Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer};
use swarm_desktop::{App, PhoneWindow, Pane};

use swarm_desktop::Offscreen;

/// How wide and tall the red's shown, drawn at `width` by `height`, and
/// what's beside it on the left: one pixel out, then two.
fn red_shown(window: &MinimalSoftwareWindow, width: u32, height: u32) -> ((usize, usize), [Rgb8Pixel; 2]) {
    window.set_size(slint::PhysicalSize::new(width, height));
    slint::platform::update_timers_and_animations();
    let mut buffer = vec![Rgb8Pixel::new(0, 0, 0); (width * height) as usize];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, width as usize);
    });
    let (mut left, mut right, mut top, mut bottom) = (usize::MAX, 0, usize::MAX, 0);
    for (i, p) in buffer.iter().enumerate() {
        if p.r > 200 && p.g < 50 && p.b < 50 {
            let (x, y) = (i % width as usize, i / width as usize);
            left = left.min(x);
            right = right.max(x);
            top = top.min(y);
            bottom = bottom.max(y);
        }
    }
    assert!(left <= right, "no red shown at {width} by {height}");
    let middle = (top + bottom) / 2 * width as usize;
    ((right + 1 - left, bottom + 1 - top), [buffer[middle + left - 1], buffer[middle + left - 2]])
}

fn red(width: u32, height: u32) -> Image {
    filled(width, height, Rgba8Pixel { r: 255, g: 0, b: 0, a: 255 })
}

fn filled(width: u32, height: u32, colour: Rgba8Pixel) -> Image {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    buffer.make_mut_slice().fill(colour);
    Image::from_rgba8(buffer)
}

#[test]
fn own_picture_the_shape_it_is_upright_or_on_its_side() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen { window: window.clone() })).unwrap();
    let ui = PhoneWindow::new().unwrap();
    window.set_size(slint::PhysicalSize::new(412, 915));
    ui.show().unwrap();
    let app = ui.global::<App>();
    app.set_view(Pane::Mine);
    app.set_call(swarm_desktop::CallShown { live: true, video_shown: true, local_on: true, ..app.get_call() });
    let theirs = |w, h| app.set_remote_frame(filled(w, h, Rgba8Pixel { r: 255, g: 255, b: 255, a: 255 }));
    let white = Rgb8Pixel::new(255, 255, 255);
    let black = Rgb8Pixel::new(0, 0, 0);

    theirs(412, 915);
    app.set_local_frame(red(360, 640));
    let ((w, h), beside) = red_shown(&window, 412, 915);
    assert!(h > w && w >= 110, "upright: {w} by {h}");
    assert_eq!(beside, [black, white], "upright: beside it");
    let upright = (w, h);

    theirs(915, 412);
    app.set_local_frame(red(640, 360));
    let ((w, h), beside) = red_shown(&window, 915, 412);
    assert!(w > h, "on its side: {w} by {h}");
    assert_eq!(beside, [black, white], "on its side: beside it");
    assert!(w + 4 >= upright.1 && h + 4 >= upright.0, "on its side {w} by {h}, as big as upright {upright:?}");
}
