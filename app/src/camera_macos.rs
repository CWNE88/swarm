//! Webcam capture with AVFoundation on Apple platforms. The native delegate retains
//! one frame at a time. The camera worker takes it and publishes a Swarm
//! picture, leaving the callback queue free to receive the next one.

use super::{CameraInfo, Captured, Found, Frame, Size, WANT_FPS};
use anyhow::{anyhow, bail, Result};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use swarm_core::video::Recycled;
use tokio::sync::watch;

type DeviceCallback = extern "C" fn(*mut c_void, *const c_char, *const c_char);
type FrameCallback = extern "C" fn(*mut c_void, *const u8, usize, usize, usize);

extern "C" {
    fn swarm_camera_devices(emit: DeviceCallback, context: *mut c_void);
    fn swarm_camera_authorize() -> c_int;
    fn swarm_camera_open(
        id: *const c_char,
        width: u32,
        height: u32,
        fps: u32,
        error: *mut *mut c_char,
    ) -> *mut c_void;
    fn swarm_camera_take(
        handle: *mut c_void,
        millis: u32,
        emit: FrameCallback,
        context: *mut c_void,
    ) -> c_int;
    fn swarm_camera_close(handle: *mut c_void);
}

pub(super) fn cameras() -> Vec<(String, String)> {
    extern "C" fn found(context: *mut c_void, name: *const c_char, id: *const c_char) {
        if name.is_null() || id.is_null() {
            return;
        }
        // The discovery call invokes this synchronously. The strings are
        // valid only until the callback returns, so copy both here.
        unsafe {
            let list = &mut *context.cast::<Vec<(String, String)>>();
            let name = CStr::from_ptr(name).to_string_lossy().into_owned();
            let id = CStr::from_ptr(id).to_string_lossy().into_owned();
            list.push((name, id));
        }
    }
    let mut list = Vec::new();
    unsafe { swarm_camera_devices(found, (&mut list as *mut Vec<_>).cast()) };
    list
}

fn candidates(choice: Option<&str>) -> Result<Vec<Found>> {
    let mut cameras = super::found();
    if cameras.is_empty() {
        bail!("no camera found — SWARM_CAMERA=test shows a test pattern");
    }
    let Some(choice) = choice else {
        return Ok(cameras);
    };
    let matching = cameras
        .iter()
        .position(|camera| camera.open == choice || camera.name == choice)
        .or_else(|| choice.parse::<usize>().ok().filter(|&i| i < cameras.len()))
        .or_else(|| {
            let choice = choice.to_lowercase();
            cameras
                .iter()
                .position(|camera| camera.name.to_lowercase().contains(&choice))
        });
    match matching {
        Some(i) => Ok(vec![cameras.swap_remove(i)]),
        None => bail!("no camera matching {choice:?}"),
    }
}

struct NativeCamera(NonNull<c_void>);

struct FrameSlot<'a> {
    memory: &'a mut Recycled,
    picture: Option<Frame>,
}

extern "C" fn receive_frame(
    context: *mut c_void,
    pixels: *const u8,
    width: usize,
    height: usize,
    stride: usize,
) {
    if pixels.is_null() || width == 0 || height == 0 || width > 4096 || height > 4096 {
        return;
    }
    let row_bytes = width * 4;
    if stride < row_bytes {
        return;
    }
    let Some(len) = stride
        .checked_mul(height - 1)
        .and_then(|n| n.checked_add(row_bytes))
    else {
        return;
    };
    if len > isize::MAX as usize {
        return;
    }
    // The camera has locked a BGRA pixel buffer with these dimensions.
    // Copy its pixels before returning to release the lock.
    let (slot, bytes) = unsafe {
        (
            &mut *context.cast::<FrameSlot<'_>>(),
            std::slice::from_raw_parts(pixels, len),
        )
    };
    let mut rgb = slot.memory.bytes();
    rgb.resize(width * height * 3, 0);
    for (y, row) in rgb.chunks_exact_mut(width * 3).enumerate() {
        let source = &bytes[y * stride..][..row_bytes];
        for (rgb, bgra) in row.chunks_exact_mut(3).zip(source.chunks_exact(4)) {
            rgb.copy_from_slice(&[bgra[2], bgra[1], bgra[0]]);
        }
    }
    slot.picture = Some(Frame::rgb(width, height, rgb));
}

impl NativeCamera {
    fn open(id: &str, (width, height): Size) -> Result<Self> {
        let id = CString::new(id)?;
        let mut error = std::ptr::null_mut();
        // The native layer returns a retained camera or an owned C error.
        let camera = unsafe { swarm_camera_open(id.as_ptr(), width, height, WANT_FPS, &mut error) };
        if let Some(camera) = NonNull::new(camera) {
            return Ok(Self(camera));
        }
        if error.is_null() {
            bail!("the camera could not start");
        }
        let reason = unsafe {
            let reason = CStr::from_ptr(error).to_string_lossy().into_owned();
            libc::free(error.cast());
            reason
        };
        Err(anyhow!(reason))
    }

    fn take(&self, memory: &mut Recycled) -> Result<Option<Frame>> {
        let mut slot = FrameSlot {
            memory,
            picture: None,
        };
        let status = unsafe {
            swarm_camera_take(
                self.0.as_ptr(),
                100,
                receive_frame,
                (&mut slot as *mut FrameSlot<'_>).cast(),
            )
        };
        if status < 0 {
            bail!("camera capture stopped");
        }
        Ok(slot.picture)
    }
}

impl Drop for NativeCamera {
    fn drop(&mut self) {
        unsafe { swarm_camera_close(self.0.as_ptr()) };
    }
}

fn first_picture(camera: &NativeCamera, memory: &mut Recycled) -> Result<Frame> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(frame) = camera.take(memory)? {
            return Ok(frame);
        }
    }
    bail!("the camera did not deliver a picture")
}

fn open(
    choice: Option<String>,
    want: Size,
    memory: &mut Recycled,
) -> Result<(NativeCamera, CameraInfo, Frame)> {
    let cameras = candidates(choice.as_deref())?;
    match unsafe { swarm_camera_authorize() } {
        1 => {}
        -2 => bail!("open Swarm.app to use the camera"),
        -1 => bail!("allow camera access in the macOS prompt, then open the camera again"),
        _ => bail!("allow Swarm under System Settings > Privacy & Security > Camera"),
    }
    let mut tried = Vec::new();
    for Found { name, open } in cameras {
        match NativeCamera::open(&open, want).and_then(|camera| {
            let frame = first_picture(&camera, memory)?;
            Ok((camera, frame))
        }) {
            Ok((camera, frame)) => {
                let (width, height) = (frame.width as u32, frame.height as u32);
                let info = CameraInfo {
                    device: open,
                    name,
                    width,
                    height,
                    format: "BGRA".into(),
                    faces_away: false,
                };
                return Ok((camera, info, frame));
            }
            Err(error) => tried.push(format!("{name}: {error:#}")),
        }
    }
    bail!("no camera would start ({})", tried.join("; "))
}

pub(super) fn start(
    choice: Option<String>,
    want: Size,
    latest: watch::Sender<Option<Arc<Frame>>>,
    captured: Arc<Captured>,
    stop: Arc<AtomicBool>,
) -> Result<(CameraInfo, JoinHandle<()>)> {
    let (ready, opened) = mpsc::sync_channel(1);
    let thread = thread::spawn(move || {
        let mut memory = Recycled::default();
        let (camera, info, first) = match open(choice, want, &mut memory) {
            Ok(started) => started,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        latest.send_replace(Some(memory.share(first)));
        captured.add();
        if ready.send(Ok(info)).is_err() {
            return;
        }
        let mut last = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            match camera.take(&mut memory) {
                Ok(Some(frame)) => {
                    latest.send_replace(Some(memory.share(frame)));
                    captured.add();
                    last = Instant::now();
                }
                Ok(None) if last.elapsed() < Duration::from_secs(5) => {}
                _ => break,
            }
        }
    });
    match opened.recv() {
        Ok(Ok(info)) => Ok((info, thread)),
        result => {
            let _ = thread.join();
            Err(result
                .ok()
                .and_then(Result::err)
                .unwrap_or_else(|| anyhow!("the camera thread ended")))
        }
    }
}
