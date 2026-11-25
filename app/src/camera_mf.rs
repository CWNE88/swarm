//! Windows cameras, through Media Foundation's source reader — the
//! Windows half of `camera`. A thread of its own opens the camera (COM
//! objects stay on the thread that made them), picks the format nearest
//! the size `camera` wants, has Media Foundation turn it into 32-bit RGB
//! (it brings its own decoder for MJPEG cameras) — or, with video on the
//! graphics chip, into NV12, decoding MJPEG on the chip where
//! it can, for the encoder there to take as it is — and keeps only the
//! newest frame, as on Linux. As on Linux too, the camera's "auto
//! exposure priority" (slowing down in dim light for more exposure: a
//! C922 gave 15 frames a second instead of 30) is off while swarm has
//! it, and put back after.
//!
//! `SWARM_CAMERA` picks a camera by its number in the list (`0`, `1`...)
//! or by part of its name (`logitech`); otherwise the first one that
//! starts is used.

use super::{Frame, Size, SMOOTH_FPS};
use anyhow::{anyhow, bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::watch;
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, KSIDENTIFIER, KSIDENTIFIER_0, KSIDENTIFIER_0_0, KSPROPERTY_CAMERACONTROL_AUTO_EXPOSURE_PRIORITY, KSPROPERTY_CAMERACONTROL_FLAGS_MANUAL,
    KSPROPERTY_CAMERACONTROL_S, KSPROPERTY_TYPE_GET, KSPROPERTY_TYPE_SET, PROPSETID_VIDCAP_CAMERACONTROL,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED};

/// What the capture thread reports once the camera is (or isn't) running.
pub struct Started {
    pub name: String,
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub format: String,
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
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            let _ = ready_tx.send(Err(anyhow!("couldn't start COM")));
            return;
        }
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            let _ = ready_tx.send(Err(anyhow!("couldn't start Media Foundation: {e}")));
            CoUninitialize();
            return;
        }
        let nv12 = super::chip_wanted();
        match open(choice.as_deref(), want, nv12) {
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
            Ok((reader, started, stride, exposure, nv12, source)) => {
                let (width, height) = (started.width as usize, started.height as usize);
                let _ = ready_tx.send(Ok(started));
                while !stop.load(Ordering::Relaxed) {
                    match next_frame(&reader, width, height, stride, want, nv12) {
                        Ok(Some(frame)) => {
                            latest.send_replace(Some(Arc::new(frame)));
                            captured.add();
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
                if let Some((control, was)) = exposure {
                    let _ = exposure_priority(&control, Some(was));
                }
                drop(reader);
                let _ = source.Shutdown();
            }
        }
        let _ = MFShutdown();
        CoUninitialize();
    });
    match ready_rx.recv() {
        Ok(Ok(started)) => Ok((started, thread)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(anyhow!("the camera thread ended before the camera started"))
        }
    }
}

/// The camera's controls, and its auto exposure priority as it was.
type Exposure = Option<(IKsControl, (i32, i32))>;

/// Finds the camera, opens it at the format nearest the wanted size, and
/// asks for RGB32 out (or, with `nv12`, NV12 if it can). Returns the
/// reader, what it's showing, the stride of a frame row (negative: bottom
/// row first), the exposure priority to put back (see
/// `exposure_priority`), and whether it's NV12.
unsafe fn open(choice: Option<&str>, want: Size, nv12: bool) -> Result<(IMFSourceReader, Started, i32, Exposure, bool, IMFMediaSource)> {
    let devices = devices()?;
    if devices.is_empty() {
        bail!("no camera found — SWARM_CAMERA=test shows a test pattern");
    }
    let names: Vec<String> = devices.iter().map(|d| friendly_name(d).unwrap_or_else(|| "camera".to_string())).collect();

    let order: Vec<usize> = match choice {
        Some(c) => match c.parse::<usize>() {
            Ok(n) if n < devices.len() => vec![n],
            Ok(n) => bail!("there's no camera {n} — there are {} ({})", devices.len(), numbered(&names)),
            Err(_) => {
                let wanted = c.to_lowercase();
                let found: Vec<usize> = (0..names.len()).filter(|&i| names[i].to_lowercase().contains(&wanted)).collect();
                if found.is_empty() {
                    bail!("no camera called {c} ({})", numbered(&names));
                }
                found
            }
        },
        None => (0..devices.len()).collect(),
    };
    let mut tried = Vec::new();
    for i in order {
        match open_one(&devices[i], want, nv12) {
            Ok((reader, width, height, format, stride, exposure, nv12, source)) => {
                return Ok((reader, Started { name: names[i].clone(), device: format!("camera {i}"), width, height, format }, stride, exposure, nv12, source));
            }
            Err(e) => tried.push(format!("{}: {e:#}", names[i])),
        }
    }
    bail!("no camera would start ({})", tried.join("; "))
}

/// The cameras there are.
unsafe fn devices() -> Result<Vec<IMFActivate>> {
    let mut attributes: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attributes, 1).context("listing cameras")?;
    let attributes = attributes.ok_or_else(|| anyhow!("listing cameras"))?;
    attributes.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)?;
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFEnumDeviceSources(&attributes, &mut list, &mut count).context("listing cameras")?;
    let devices: Vec<IMFActivate> = (0..count as usize).filter_map(|i| (*list.add(i)).take()).collect();
    if !list.is_null() {
        CoTaskMemFree(Some(list as *const _));
    }
    Ok(devices)
}

/// The cameras' names, in the order they're numbered — on a thread of
/// its own, as COM wants. Starts none of them.
pub fn names() -> Vec<String> {
    std::thread::spawn(|| unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            return Vec::new();
        }
        let mut names = Vec::new();
        if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_ok() {
            if let Ok(devices) = devices() {
                names = devices.iter().map(|d| friendly_name(d).unwrap_or_else(|| "camera".to_string())).collect();
            }
            let _ = MFShutdown();
        }
        CoUninitialize();
        names
    })
    .join()
    .unwrap_or_default()
}

fn numbered(names: &[String]) -> String {
    names.iter().enumerate().map(|(i, n)| format!("{i}: {n}")).collect::<Vec<_>>().join(", ")
}

unsafe fn friendly_name(device: &IMFActivate) -> Option<String> {
    let mut text = PWSTR::null();
    let mut len = 0u32;
    device.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut text, &mut len).ok()?;
    let name = text.to_string().ok();
    CoTaskMemFree(Some(text.0 as *const _));
    name
}

#[allow(clippy::type_complexity)]
unsafe fn open_one(device: &IMFActivate, want: Size, nv12: bool) -> Result<(IMFSourceReader, u32, u32, String, i32, Exposure, bool, IMFMediaSource)> {
    let source: IMFMediaSource = device.ActivateObject().context("opening it")?;
    let exposure = source.cast::<IKsControl>().ok().and_then(|control| {
        let was = exposure_priority(&control, Some((0, KSPROPERTY_CAMERACONTROL_FLAGS_MANUAL)))?;
        Some((control, was))
    });
    if nv12 {
        if let Ok((reader, width, height, format, stride)) = reader_for(&source, want, true) {
            return Ok((reader, width, height, format, stride, exposure, true, source));
        }
    }
    let (reader, width, height, format, stride) = reader_for(&source, want, false)?;
    Ok((reader, width, height, format, stride, exposure, false, source))
}

/// A reader for `source` at the format nearest the wanted size, giving
/// RGB32 or (`nv12`) NV12, with the size, the camera's own format, and
/// the stride of a row.
unsafe fn reader_for(source: &IMFMediaSource, want: Size, nv12: bool) -> Result<(IMFSourceReader, u32, u32, String, i32)> {
    let mut attributes: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attributes, 3)?;
    let attributes = attributes.ok_or_else(|| anyhow!("reader settings"))?;
    if nv12 {
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attributes.SetUINT32(&MF_SOURCE_READER_DISCONNECT_MEDIASOURCE_ON_SHUTDOWN, 1)?;
    } else {
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
    }
    let reader = MFCreateSourceReaderFromMediaSource(source, &attributes).context("starting it")?;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    let mut formats: Vec<(IMFMediaType, u32, u32, u32, bool)> = Vec::new();
    let mut index = 0;
    while let Ok(kind) = reader.GetNativeMediaType(stream, index) {
        index += 1;
        let Ok(size) = kind.GetUINT64(&MF_MT_FRAME_SIZE) else { continue };
        let (w, h) = ((size >> 32) as u32, size as u32);
        let rate = kind.GetUINT64(&MF_MT_FRAME_RATE).ok().map(|r| ((r >> 32) as u32) / (r as u32).max(1)).unwrap_or(0);
        let jpeg = kind.GetGUID(&MF_MT_SUBTYPE).is_ok_and(|subtype| subtype == MFVideoFormat_MJPG);
        formats.push((kind, w, h, rate, jpeg));
    }
    if formats.is_empty() {
        bail!("it offers no video formats");
    }
    formats.sort_by_key(|(_, w, h, rate, jpeg)| {
        let big_enough = *w >= want.0 && *h >= want.1;
        let area = *w as i64 * *h as i64;
        (*rate < SMOOTH_FPS, !big_enough, if big_enough { area } else { -area }, *jpeg, std::cmp::Reverse((*rate).min(30)))
    });

    let mut last_error = None;
    for (native, width, height, _, _) in formats {
        let native_format = subtype_name(native.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default());
        let chosen = (|| -> Result<(u32, u32, i32)> {
            reader.SetCurrentMediaType(stream, None, &native).context("choosing its format")?;
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, if nv12 { &MFVideoFormat_NV12 } else { &MFVideoFormat_RGB32 })?;
            reader.SetCurrentMediaType(stream, None, &output).context(if nv12 { "asking for NV12" } else { "asking for RGB" })?;
            let current = reader.GetCurrentMediaType(stream)?;
            let size = current.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(((width as u64) << 32) | height as u64);
            let (width, height) = ((size >> 32) as u32, size as u32);
            let stride = current.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or(width as i32 * if nv12 { 1 } else { 4 });
            Ok((width, height, stride))
        })();
        match chosen {
            Ok((width, height, stride)) => return Ok((reader, width, height, if nv12 { format!("{native_format} as NV12") } else { native_format }, stride)),
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no usable format")))
}

/// Reads the camera's auto exposure priority (value, flags) and, with
/// `set`, sets it; returns what it was. `None` when the camera doesn't
/// have it, or wouldn't change it.
unsafe fn exposure_priority(control: &IKsControl, set: Option<(i32, i32)>) -> Option<(i32, i32)> {
    let property = |flags| KSIDENTIFIER {
        Anonymous: KSIDENTIFIER_0 {
            Anonymous: KSIDENTIFIER_0_0 { Set: PROPSETID_VIDCAP_CAMERACONTROL, Id: KSPROPERTY_CAMERACONTROL_AUTO_EXPOSURE_PRIORITY.0 as u32, Flags: flags },
        },
    };
    let size = std::mem::size_of::<KSPROPERTY_CAMERACONTROL_S>() as u32;
    let mut data = KSPROPERTY_CAMERACONTROL_S { Property: property(KSPROPERTY_TYPE_GET), ..Default::default() };
    let mut returned = 0u32;
    control.KsProperty(&data.Property, size, (&raw mut data).cast(), size, &mut returned).ok()?;
    let was = (data.Value, data.Flags as i32);
    if let Some((value, flags)) = set {
        let mut data = KSPROPERTY_CAMERACONTROL_S { Property: property(KSPROPERTY_TYPE_SET), Value: value, Flags: flags as u32, Capabilities: 0 };
        control.KsProperty(&data.Property, size, (&raw mut data).cast(), size, &mut returned).ok()?;
    }
    Some(was)
}

fn subtype_name(subtype: GUID) -> String {
    let fourcc = subtype.data1.to_le_bytes();
    if fourcc.iter().all(|b| b.is_ascii_alphanumeric()) {
        String::from_utf8_lossy(&fourcc).into_owned()
    } else {
        "video".to_string()
    }
}

/// Waits for the next frame and turns it from BGRX into RGB (or from NV12
/// into I420). `None`: the reader handed back nothing this time (a gap in
/// the stream).
unsafe fn next_frame(reader: &IMFSourceReader, width: usize, height: usize, stride: i32, want: Size, nv12: bool) -> Result<Option<Frame>> {
    let mut flags = 0u32;
    let mut sample: Option<IMFSample> = None;
    reader.ReadSample(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0, None, Some(&mut flags), None, Some(&mut sample))?;
    if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
        bail!("the camera stopped");
    }
    let Some(sample) = sample else { return Ok(None) };
    let buffer = sample.ConvertToContiguousBuffer()?;
    if nv12 {
        return Ok(nv12_frame(&buffer, width, height, stride).map(|frame| super::shrink(frame, want)));
    }

    let mut rgb = Vec::with_capacity(width * height * 3);
    if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
        let mut top: *mut u8 = std::ptr::null_mut();
        let mut pitch = 0i32;
        two_d.Lock2D(&mut top, &mut pitch)?;
        for y in 0..height {
            let row = top.offset(y as isize * pitch as isize);
            push_row(&mut rgb, std::slice::from_raw_parts(row, width * 4));
        }
        two_d.Unlock2D()?;
    } else {
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut length = 0u32;
        buffer.Lock(&mut data, None, Some(&mut length))?;
        let row_bytes = stride.unsigned_abs() as usize;
        if (length as usize) < row_bytes * height || row_bytes < width * 4 {
            buffer.Unlock()?;
            return Ok(None);
        }
        for y in 0..height {
            let from = if stride < 0 { height - 1 - y } else { y };
            push_row(&mut rgb, std::slice::from_raw_parts(data.add(from * row_bytes), width * 4));
        }
        buffer.Unlock()?;
    }
    Ok(Some(super::shrink(Frame::rgb(width, height, rgb), want)))
}

/// One row of BGRX pixels, as RGB.
fn push_row(rgb: &mut Vec<u8>, bgrx: &[u8]) {
    for px in bgrx.as_chunks::<4>().0 {
        rgb.extend([px[2], px[1], px[0]]);
    }
}

/// An NV12 frame's buffer as I420. Its UV plane follows the Y plane's
/// rows, of which there may be more than the picture's (padded to a
/// multiple of 16): worked out from the buffer's length, which is one
/// and a half times the Y plane's.
unsafe fn nv12_frame(buffer: &IMFMediaBuffer, width: usize, height: usize, stride: i32) -> Option<Frame> {
    if let Ok(two_d) = buffer.cast::<IMF2DBuffer2>() {
        let (mut top, mut start) = (std::ptr::null_mut(), std::ptr::null_mut());
        let (mut pitch, mut length) = (0i32, 0u32);
        two_d.Lock2DSize(MF2DBuffer_LockFlags_Read, &mut top, &mut pitch, &mut start, &mut length).ok()?;
        let frame = if pitch > 0 && top == start {
            let pitch = pitch as usize;
            let rows = (length as usize * 2 / 3) / pitch;
            crate::hardware::i420_from_nv12(std::slice::from_raw_parts(top, length as usize), width, height, pitch, pitch * rows.max(height))
        } else {
            None
        };
        let _ = two_d.Unlock2D();
        return frame;
    }
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut length = 0u32;
    buffer.Lock(&mut data, None, Some(&mut length)).ok()?;
    let pitch = (stride.unsigned_abs() as usize).max(width);
    let rows = (length as usize * 2 / 3) / pitch;
    let frame = crate::hardware::i420_from_nv12(std::slice::from_raw_parts(data, length as usize), width, height, pitch, pitch * rows.max(height));
    let _ = buffer.Unlock();
    frame
}
