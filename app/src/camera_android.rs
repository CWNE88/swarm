//! Android phones' cameras, through the NDK's Camera2 — the Android half
//! of `camera`. A thread of its own opens the camera, has it deliver
//! YUV_420_888 pictures to an image reader at the size nearest the one
//! `camera` wants (no bigger than 1080p),
//! and keeps only the newest, turned the right way up for how the phone's
//! held (`camera::turn_for`). As on the desktop, smooth video matters
//! more than light: the camera's held to its steadiest rate up to 30 a
//! second.
//!
//! The app has to have been allowed the camera (it asks when it starts);
//! otherwise opening it says so.

use super::{rate_for, yuv_size_for, Frame, Plane, Size};
use anyhow::{anyhow, bail, Result};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use swarm_core::video::Recycled;
use tokio::sync::watch;

type Status = c_int;
const OK: Status = 0;
const PERMISSION_DENIED: Status = -10_013;
const CAMERA_DISABLED: Status = -10_012;

const LENS_FACING: u32 = (8 << 16) + 5;
const SENSOR_ORIENTATION: u32 = (14 << 16) + 14;
const STREAM_CONFIGURATIONS: u32 = (13 << 16) + 10;
const AE_TARGET_FPS_RANGE: u32 = (1 << 16) + 5;
const AE_AVAILABLE_TARGET_FPS_RANGES: u32 = (1 << 16) + 20;
const FACING_FRONT: u8 = 0;
const FACING_BACK: u8 = 1;
const STREAM_OUTPUT: i32 = 0;
const YUV_420_888: i32 = 0x23;
const TEMPLATE_RECORD: c_int = 3;

#[repr(C)]
struct IdList {
    count: c_int,
    ids: *const *const c_char,
}

#[repr(C)]
struct Entry {
    tag: u32,
    kind: u8,
    count: u32,
    data: *const c_void,
}

#[repr(C)]
struct DeviceCallbacks {
    context: *mut c_void,
    on_disconnected: extern "C" fn(*mut c_void, *mut c_void),
    on_error: extern "C" fn(*mut c_void, *mut c_void, c_int),
}

#[repr(C)]
struct SessionCallbacks {
    context: *mut c_void,
    on_closed: extern "C" fn(*mut c_void, *mut c_void),
    on_ready: extern "C" fn(*mut c_void, *mut c_void),
    on_active: extern "C" fn(*mut c_void, *mut c_void),
}

#[repr(C)]
struct ImageListener {
    context: *mut c_void,
    on_image: extern "C" fn(*mut c_void, *mut c_void),
}

#[link(name = "camera2ndk")]
extern "C" {
    fn ACameraManager_create() -> *mut c_void;
    fn ACameraManager_delete(manager: *mut c_void);
    fn ACameraManager_getCameraIdList(manager: *mut c_void, list: *mut *mut IdList) -> Status;
    fn ACameraManager_deleteCameraIdList(list: *mut IdList);
    fn ACameraManager_getCameraCharacteristics(manager: *mut c_void, id: *const c_char, metadata: *mut *mut c_void) -> Status;
    fn ACameraManager_openCamera(manager: *mut c_void, id: *const c_char, callbacks: *mut DeviceCallbacks, device: *mut *mut c_void) -> Status;
    fn ACameraMetadata_getConstEntry(metadata: *const c_void, tag: u32, entry: *mut Entry) -> Status;
    fn ACameraMetadata_free(metadata: *mut c_void);
    fn ACameraDevice_close(device: *mut c_void) -> Status;
    fn ACameraDevice_createCaptureRequest(device: *mut c_void, template: c_int, request: *mut *mut c_void) -> Status;
    fn ACameraDevice_createCaptureSession(device: *mut c_void, outputs: *const c_void, callbacks: *const SessionCallbacks, session: *mut *mut c_void) -> Status;
    fn ACaptureRequest_free(request: *mut c_void);
    fn ACaptureRequest_addTarget(request: *mut c_void, target: *const c_void) -> Status;
    fn ACaptureRequest_setEntry_i32(request: *mut c_void, tag: u32, count: u32, data: *const i32) -> Status;
    fn ACameraOutputTarget_create(window: *mut c_void, target: *mut *mut c_void) -> Status;
    fn ACameraOutputTarget_free(target: *mut c_void);
    fn ACaptureSessionOutputContainer_create(container: *mut *mut c_void) -> Status;
    fn ACaptureSessionOutputContainer_free(container: *mut c_void);
    fn ACaptureSessionOutputContainer_add(container: *mut c_void, output: *const c_void) -> Status;
    fn ACaptureSessionOutput_create(window: *mut c_void, output: *mut *mut c_void) -> Status;
    fn ACaptureSessionOutput_free(output: *mut c_void);
    fn ACameraCaptureSession_setRepeatingRequest(session: *mut c_void, callbacks: *mut c_void, count: c_int, requests: *mut *mut c_void, sequence: *mut c_int) -> Status;
    fn ACameraCaptureSession_stopRepeating(session: *mut c_void) -> Status;
    fn ACameraCaptureSession_close(session: *mut c_void);
}

#[link(name = "mediandk")]
extern "C" {
    fn AImageReader_new(width: i32, height: i32, format: i32, most: i32, reader: *mut *mut c_void) -> Status;
    fn AImageReader_delete(reader: *mut c_void);
    fn AImageReader_getWindow(reader: *mut c_void, window: *mut *mut c_void) -> Status;
    fn AImageReader_setImageListener(reader: *mut c_void, listener: *mut ImageListener) -> Status;
    fn AImageReader_acquireLatestImage(reader: *mut c_void, image: *mut *mut c_void) -> Status;
    fn AImage_delete(image: *mut c_void);
    fn AImage_getPlaneData(image: *const c_void, plane: c_int, data: *mut *mut u8, len: *mut c_int) -> Status;
    fn AImage_getPlaneRowStride(image: *const c_void, plane: c_int, stride: *mut i32) -> Status;
    fn AImage_getPlanePixelStride(image: *const c_void, plane: c_int, stride: *mut i32) -> Status;
}

/// What the capture thread reports once the camera is (or isn't) running.
pub struct Started {
    pub name: String,
    pub device: String,
    pub width: u32,
    pub height: u32,
    /// It's on the back (see `camera::CameraInfo::faces_away`).
    pub faces_away: bool,
}

/// A camera as its characteristics say: which way it faces, how its
/// sensor's mounted, the sizes it gives YUV in, and the rates it keeps.
struct Facts {
    facing: u8,
    sensor: u32,
    sizes: Vec<Size>,
    rates: Vec<(i32, i32)>,
}

fn ok(status: Status, what: &str) -> Result<()> {
    match status {
        OK => Ok(()),
        PERMISSION_DENIED => bail!("Swarm hasn't been allowed the camera (Android's Settings, Apps, Swarm, Permissions)"),
        CAMERA_DISABLED => bail!("the camera's turned off on this phone"),
        _ => bail!("{what} failed ({status})"),
    }
}

/// The cameras there are, by name and id: those at the front first (for a
/// call, the usual one), then those at the back.
pub fn cameras() -> Vec<(String, String)> {
    unsafe {
        let manager = ACameraManager_create();
        if manager.is_null() {
            return Vec::new();
        }
        let mut found = Vec::new();
        for id in ids(manager) {
            let Ok(facts) = facts(manager, &id) else { continue };
            found.push((facts.facing, id));
        }
        ACameraManager_delete(manager);
        found.sort_by_key(|(facing, _)| *facing);
        found.into_iter().map(|(facing, id)| (name(facing).to_string(), id)).collect()
    }
}

fn name(facing: u8) -> &'static str {
    match facing {
        FACING_FRONT => "front camera",
        FACING_BACK => "back camera",
        _ => "plugged-in camera",
    }
}

unsafe fn ids(manager: *mut c_void) -> Vec<String> {
    let mut list = std::ptr::null_mut();
    if ACameraManager_getCameraIdList(manager, &mut list) != OK || list.is_null() {
        return Vec::new();
    }
    let ids = (0..(*list).count.max(0) as usize).map(|i| CStr::from_ptr(*(*list).ids.add(i)).to_string_lossy().into_owned()).collect();
    ACameraManager_deleteCameraIdList(list);
    ids
}

unsafe fn facts(manager: *mut c_void, id: &str) -> Result<Facts> {
    let id = CString::new(id)?;
    let mut metadata = std::ptr::null_mut();
    ok(ACameraManager_getCameraCharacteristics(manager, id.as_ptr(), &mut metadata), "reading the camera's characteristics")?;
    let entry = |tag| {
        let mut entry = Entry { tag: 0, kind: 0, count: 0, data: std::ptr::null() };
        (ACameraMetadata_getConstEntry(metadata, tag, &mut entry) == OK && !entry.data.is_null()).then_some(entry)
    };
    let i32s = |tag| entry(tag).map_or(Vec::new(), |e: Entry| std::slice::from_raw_parts(e.data as *const i32, e.count as usize).to_vec());
    let facing = entry(LENS_FACING).map_or(FACING_BACK, |e| *(e.data as *const u8));
    let sensor = i32s(SENSOR_ORIENTATION).first().copied().unwrap_or(0).rem_euclid(360) as u32;
    let sizes = i32s(STREAM_CONFIGURATIONS)
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|c| c[0] == YUV_420_888 && c[3] == STREAM_OUTPUT)
        .map(|c| (c[1] as u32, c[2] as u32))
        .collect();
    let rates = i32s(AE_AVAILABLE_TARGET_FPS_RANGES).as_chunks::<2>().0.iter().map(|c| (c[0], c[1])).collect();
    ACameraMetadata_free(metadata);
    Ok(Facts { facing, sensor, sizes, rates })
}

/// Woken when the reader has a picture, or the camera's gone.
#[derive(Default)]
struct Waker {
    ready: Mutex<bool>,
    wake: Condvar,
    lost: AtomicBool,
}

impl Waker {
    fn wake(&self) {
        if let Ok(mut ready) = self.ready.lock() {
            *ready = true;
            self.wake.notify_one();
        }
    }
}

extern "C" fn on_image(context: *mut c_void, _reader: *mut c_void) {
    unsafe { &*(context as *const Waker) }.wake();
}

extern "C" fn on_disconnected(context: *mut c_void, _device: *mut c_void) {
    let waker = unsafe { &*(context as *const Waker) };
    waker.lost.store(true, Ordering::Relaxed);
    waker.wake();
}

extern "C" fn on_error(context: *mut c_void, device: *mut c_void, error: c_int) {
    eprintln!("camera: error {error}");
    on_disconnected(context, device);
}

extern "C" fn on_session(_context: *mut c_void, _session: *mut c_void) {}

/// Everything opened for a camera, closed (in the order Camera2 wants)
/// when dropped: whatever got opened before a step failed.
struct Opened {
    manager: *mut c_void,
    device: *mut c_void,
    reader: *mut c_void,
    output: *mut c_void,
    container: *mut c_void,
    target: *mut c_void,
    request: *mut c_void,
    session: *mut c_void,
}

impl Drop for Opened {
    fn drop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                ACameraCaptureSession_stopRepeating(self.session);
                ACameraCaptureSession_close(self.session);
            }
            if !self.request.is_null() {
                ACaptureRequest_free(self.request);
            }
            if !self.target.is_null() {
                ACameraOutputTarget_free(self.target);
            }
            if !self.container.is_null() {
                ACaptureSessionOutputContainer_free(self.container);
            }
            if !self.output.is_null() {
                ACaptureSessionOutput_free(self.output);
            }
            if !self.device.is_null() {
                ACameraDevice_close(self.device);
            }
            if !self.reader.is_null() {
                AImageReader_delete(self.reader);
            }
            if !self.manager.is_null() {
                ACameraManager_delete(self.manager);
            }
        }
    }
}

/// Opens the camera with `id` (or the first there is) at about `want`,
/// pictures coming to `waker`; with how far to turn them.
unsafe fn open(id: Option<&str>, want: Size, waker: &Arc<Waker>, opened: &mut Opened) -> Result<(Started, u8, u32)> {
    opened.manager = ACameraManager_create();
    if opened.manager.is_null() {
        bail!("no camera service");
    }
    let id = match id {
        Some(id) => id.to_string(),
        None => cameras().into_iter().next().map(|(_, id)| id).ok_or_else(|| anyhow!("this phone has no camera"))?,
    };
    let facts = facts(opened.manager, &id)?;
    let (width, height) = yuv_size_for(&facts.sizes, want).ok_or_else(|| anyhow!("the camera gives no YUV pictures"))?;
    let context = Arc::as_ptr(waker) as *mut c_void;

    ok(AImageReader_new(width as i32, height as i32, YUV_420_888, 3, &mut opened.reader), "making an image reader")?;
    let mut listener = ImageListener { context, on_image };
    ok(AImageReader_setImageListener(opened.reader, &mut listener), "listening for pictures")?;
    let mut window = std::ptr::null_mut();
    ok(AImageReader_getWindow(opened.reader, &mut window), "the image reader's window")?;

    let mut callbacks = DeviceCallbacks { context, on_disconnected, on_error };
    let id_c = CString::new(id.clone())?;
    ok(ACameraManager_openCamera(opened.manager, id_c.as_ptr(), &mut callbacks, &mut opened.device), "opening the camera")?;

    ok(ACaptureSessionOutputContainer_create(&mut opened.container), "a capture session's outputs")?;
    ok(ACaptureSessionOutput_create(window, &mut opened.output), "a capture session's output")?;
    ok(ACaptureSessionOutputContainer_add(opened.container, opened.output), "adding the output")?;
    ok(ACameraOutputTarget_create(window, &mut opened.target), "a capture target")?;
    ok(ACameraDevice_createCaptureRequest(opened.device, TEMPLATE_RECORD, &mut opened.request), "a capture request")?;
    ok(ACaptureRequest_addTarget(opened.request, opened.target), "adding the target")?;
    if let Some((bottom, top)) = rate_for(&facts.rates) {
        let range = [bottom, top];
        ACaptureRequest_setEntry_i32(opened.request, AE_TARGET_FPS_RANGE, 2, range.as_ptr());
    }
    let session_callbacks = SessionCallbacks { context, on_closed: on_session, on_ready: on_session, on_active: on_session };
    ok(ACameraDevice_createCaptureSession(opened.device, opened.container, &session_callbacks, &mut opened.session), "a capture session")?;
    let mut requests = [opened.request];
    ok(ACameraCaptureSession_setRepeatingRequest(opened.session, std::ptr::null_mut(), 1, requests.as_mut_ptr(), std::ptr::null_mut()), "starting the camera")?;
    let started = Started { name: name(facts.facing).to_string(), device: id, width, height, faces_away: facts.facing == FACING_BACK };
    Ok((started, facts.facing, facts.sensor))
}

/// The newest picture in `reader`, if there's one, turned `turn`, in
/// `memory`'s memory.
unsafe fn take(reader: *mut c_void, width: usize, height: usize, turn: u32, memory: &mut Recycled) -> Option<Frame> {
    let mut image = std::ptr::null_mut();
    if AImageReader_acquireLatestImage(reader, &mut image) != OK || image.is_null() {
        return None;
    }
    let mut planes = Vec::with_capacity(3);
    for i in 0..3 {
        let (mut data, mut len, mut row, mut pixel) = (std::ptr::null_mut(), 0, 0, 0);
        if AImage_getPlaneData(image, i, &mut data, &mut len) != OK
            || AImage_getPlaneRowStride(image, i, &mut row) != OK
            || AImage_getPlanePixelStride(image, i, &mut pixel) != OK
            || data.is_null()
        {
            AImage_delete(image);
            return None;
        }
        planes.push(Plane { data: std::slice::from_raw_parts(data, len as usize), row: row as usize, pixel: pixel as usize });
    }
    let frame = super::yuv420_turned([planes[0], planes[1], planes[2]], width, height, turn, memory.bytes());
    AImage_delete(image);
    Some(frame)
}

/// Starts capturing on a thread of its own; returns once the camera is
/// running (or failed to start), with the thread.
pub fn start(
    choice: Option<String>,
    want: Size,
    latest: watch::Sender<Option<Arc<Frame>>>,
    captured: Arc<super::Captured>,
    stop: Arc<AtomicBool>,
) -> Result<(Started, std::thread::JoinHandle<()>)> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Started>>();
    let thread = std::thread::spawn(move || unsafe {
        let waker = Arc::new(Waker::default());
        let mut memory = Recycled::default();
        let mut opened = Opened {
            manager: std::ptr::null_mut(),
            device: std::ptr::null_mut(),
            reader: std::ptr::null_mut(),
            output: std::ptr::null_mut(),
            container: std::ptr::null_mut(),
            target: std::ptr::null_mut(),
            request: std::ptr::null_mut(),
            session: std::ptr::null_mut(),
        };
        let (width, height, front, sensor) = match open(choice.as_deref(), want, &waker, &mut opened) {
            Ok((started, facing, sensor)) => {
                let size = (started.width as usize, started.height as usize);
                let _ = ready_tx.send(Ok(started));
                (size.0, size.1, facing == FACING_FRONT, sensor)
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        while !stop.load(Ordering::Relaxed) && !waker.lost.load(Ordering::Relaxed) {
            {
                let Ok(ready) = waker.ready.lock() else { break };
                let Ok((mut ready, _)) = waker.wake.wait_timeout_while(ready, Duration::from_millis(200), |ready| !*ready) else { break };
                *ready = false;
            }
            let turn = super::turn_for(sensor, front, super::display_rotation());
            if let Some(frame) = take(opened.reader, width, height, turn, &mut memory) {
                latest.send_replace(Some(memory.share(frame)));
                captured.add();
            }
        }
        drop(opened);
        drop(waker);
    });
    let started = ready_rx.recv().map_err(|_| anyhow!("the camera thread ended"))??;
    Ok((started, thread))
}
