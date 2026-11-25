//! Video on the graphics chip on Windows, through Media Foundation: the
//! chip maker's H.264 and H.265 encoders (Intel Quick Sync, NVIDIA, AMD —
//! whatever Windows lists as hardware) and Microsoft's H.264 and H.265
//! decoders working through Direct3D 11, which hands decoding to the chip
//! (H.265's comes with Windows' HEVC Video Extensions). Given to
//! `swarm_core::video` as its `Hardware`, which falls back to OpenH264
//! for anything this can't do.
//!
//! Each encoder and decoder lives on a thread of its own, with COM and
//! Media Foundation started there, and is worked from outside by
//! messages: its COM objects never leave that thread, and calls can
//! move between threads as they like.
//!
//! The encoder is set up as WebRTC sets one up for calls: Baseline
//! profile (so OpenH264, and so every swarm, decodes it; H.265's Main),
//! no B-frames,
//! constant bitrate, low latency (a picture out for each in), full
//! frames only when asked for. Hardware encoders are asynchronous: they
//! say when they want a picture and when they have one ready, through
//! events, which are waited on here.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use crate::common::{i420_from_nv12, nv12_from, parameter_sets, sps_size};
use swarm_core::video::{Codec, HardDecoder, HardEncoder, Hardware, Picture, Shape};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11VideoDevice, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, D3D11_SDK_VERSION};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

/// How long an encoder may take to ask for a picture, or a frame to
/// come out of one, before it's taken to have stopped.
const PATIENCE: Duration = Duration::from_millis(500);
/// How long to wait for a frame to come out of the encoder before
/// going on without it (it comes with the next picture instead).
const OUTPUT_WAIT: Duration = Duration::from_millis(60);

/// `SWARM_MF_DEBUG` set: each step with the encoder and decoder is
/// printed (to stderr), for finding out what a particular chip does.
fn debugging() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SWARM_MF_DEBUG").is_some())
}

/// Media Foundation's hardware, for `swarm_core::video::provide_hardware`.
pub struct MediaFoundation;

impl Hardware for MediaFoundation {
    fn encoder(&self, shape: Shape, bitrate: u32, codec: Codec) -> Result<Box<dyn HardEncoder>> {
        let worker = Worker::start(move || unsafe { MfEncoder::open(shape, bitrate, codec) })?;
        let name = worker.run(|e| e.name.clone())?;
        Ok(Box::new(EncoderHandle { worker, name, shape }))
    }

    fn decoder(&self, codec: Codec) -> Result<Box<dyn HardDecoder>> {
        let software = std::env::var_os("SWARM_MF_SOFTWARE").is_some();
        let worker = Worker::start(move || unsafe { MfDecoder::open(software, codec) })?;
        let name = worker.run(|d| d.name.clone())?;
        Ok(Box::new(DecoderHandle { worker, name }))
    }

    fn h265(&self) -> (bool, bool) {
        static FOUND: std::sync::OnceLock<(bool, bool)> = std::sync::OnceLock::new();
        *FOUND.get_or_init(|| {
            let shape = Shape { width: 1280, height: 720, fps: 25 };
            let encodes = Worker::start(move || unsafe { MfEncoder::open(shape, 1_000_000, Codec::H265) });
            let decodes = Worker::start(|| unsafe { MfDecoder::open(false, Codec::H265) });
            if debugging() {
                for (what, found) in [("encoding", encodes.as_ref().err()), ("decoding", decodes.as_ref().err())] {
                    if let Some(e) = found {
                        eprintln!("no H.265 {what}: {e:#}");
                    }
                }
            }
            (encodes.is_ok(), decodes.is_ok())
        })
    }

    fn name(&self) -> String {
        "Media Foundation".to_string()
    }
}

/// Something to do with a worker's `T`, on its thread.
type Job<T> = Box<dyn FnOnce(&mut T) + Send>;

/// A thread holding `T` (made there, used there, dropped there), with
/// COM and Media Foundation started for it.
struct Worker<T: 'static> {
    jobs: Option<mpsc::Sender<Job<T>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl<T: 'static> Worker<T> {
    fn start(make: impl FnOnce() -> Result<T> + Send + 'static) -> Result<Self> {
        let (jobs, inbox) = mpsc::channel::<Job<T>>();
        let (ready_tx, ready) = mpsc::channel::<Result<()>>();
        let thread = std::thread::spawn(move || unsafe {
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                let _ = ready_tx.send(Err(anyhow!("couldn't start COM")));
                return;
            }
            if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_LITE) {
                let _ = ready_tx.send(Err(anyhow!("couldn't start Media Foundation: {e}")));
                CoUninitialize();
                return;
            }
            match make() {
                Ok(mut state) => {
                    let _ = ready_tx.send(Ok(()));
                    while let Ok(job) = inbox.recv() {
                        job(&mut state);
                    }
                    drop(state);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
            let _ = MFShutdown();
            CoUninitialize();
        });
        match ready.recv() {
            Ok(Ok(())) => Ok(Worker { jobs: Some(jobs), thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("its thread ended before it started"))
            }
        }
    }

    /// Runs `job` on the thread, and waits for what it gives back.
    fn run<R: Send + 'static>(&self, job: impl FnOnce(&mut T) -> R + Send + 'static) -> Result<R> {
        let (tx, rx) = mpsc::channel();
        self.jobs
            .as_ref()
            .ok_or_else(|| anyhow!("stopped"))?
            .send(Box::new(move |state: &mut T| {
                let _ = tx.send(job(state));
            }))
            .map_err(|_| anyhow!("its thread has gone"))?;
        rx.recv().map_err(|_| anyhow!("its thread has gone"))
    }
}

impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct EncoderHandle {
    worker: Worker<MfEncoder>,
    name: String,
    shape: Shape,
}

impl HardEncoder for EncoderHandle {
    fn encode(&mut self, picture: &Picture, key: bool) -> Result<Vec<u8>> {
        if (picture.width, picture.height) != (self.shape.width, self.shape.height) {
            bail!("a {}×{} picture for a {}×{} encoder", picture.width, picture.height, self.shape.width, self.shape.height);
        }
        let nv12 = nv12_from(picture);
        self.worker.run(move |e| unsafe { e.encode(&nv12, key) })?
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        self.worker.run(move |e| unsafe { e.set_bitrate(bitrate) })?
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

struct DecoderHandle {
    worker: Worker<MfDecoder>,
    name: String,
}

impl HardDecoder for DecoderHandle {
    fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>> {
        let frame = frame.to_vec();
        self.worker.run(move |d| unsafe { d.decode(&frame) })?
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

unsafe fn set_value(codec: &ICodecAPI, api: &GUID, value: VARIANT) -> Result<()> {
    codec.SetValue(api, &value).map_err(Into::into)
}

unsafe fn friendly_name(activate: &IMFActivate, coding: Codec) -> String {
    let mut text = PWSTR::null();
    let mut len = 0u32;
    if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut len).is_err() {
        return format!("an {} encoder", coding_name(coding));
    }
    let name = text.to_string().unwrap_or_default();
    CoTaskMemFree(Some(text.0 as *const _));
    name
}

fn coding_name(coding: Codec) -> &'static str {
    match coding {
        Codec::H264 => "H.264",
        Codec::H265 => "H.265",
    }
}

/// Media Foundation's name for `coding`.
fn subtype_of(coding: Codec) -> GUID {
    match coding {
        Codec::H264 => MFVideoFormat_H264,
        Codec::H265 => MFVideoFormat_HEVC,
    }
}

/// A video media type: `subtype` at `shape`'s size and pace.
unsafe fn video_type(subtype: &GUID, shape: Shape) -> Result<IMFMediaType> {
    let kind = MFCreateMediaType()?;
    kind.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    kind.SetGUID(&MF_MT_SUBTYPE, subtype)?;
    kind.SetUINT64(&MF_MT_FRAME_SIZE, ((shape.width as u64) << 32) | shape.height as u64)?;
    kind.SetUINT64(&MF_MT_FRAME_RATE, ((shape.fps as u64) << 32) | 1)?;
    kind.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
    kind.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    Ok(kind)
}

/// A sample holding `data`, at picture `frame` of a stream `fps` a second.
unsafe fn sample_of(data: &[u8], frame: u64, fps: u32) -> Result<IMFSample> {
    let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
    let mut at: *mut u8 = std::ptr::null_mut();
    buffer.Lock(&mut at, None, None)?;
    std::ptr::copy_nonoverlapping(data.as_ptr(), at, data.len());
    buffer.Unlock()?;
    buffer.SetCurrentLength(data.len() as u32)?;
    let sample = MFCreateSample()?;
    sample.AddBuffer(&buffer)?;
    let duration = 10_000_000 / fps.max(1) as i64;
    sample.SetSampleTime(frame as i64 * duration)?;
    sample.SetSampleDuration(duration)?;
    Ok(sample)
}

/// A sample's bytes, all of them.
unsafe fn bytes_of(sample: &IMFSample) -> Result<Vec<u8>> {
    let buffer = sample.ConvertToContiguousBuffer()?;
    let mut at: *mut u8 = std::ptr::null_mut();
    let mut length = 0u32;
    buffer.Lock(&mut at, None, Some(&mut length))?;
    let bytes = std::slice::from_raw_parts(at, length as usize).to_vec();
    buffer.Unlock()?;
    Ok(bytes)
}

/// The input and output stream numbers (0 and 0, unless it says).
unsafe fn stream_ids(transform: &IMFTransform) -> (u32, u32) {
    let (mut input, mut output) = ([0u32], [0u32]);
    match transform.GetStreamIDs(&mut input, &mut output) {
        Ok(()) => (input[0], output[0]),
        Err(_) => (0, 0),
    }
}

/// Whether the transform gives its own output samples (or wants them
/// given), and how big they need to be.
unsafe fn output_needs(transform: &IMFTransform, output_id: u32) -> Result<(bool, u32)> {
    let info = transform.GetOutputStreamInfo(output_id)?;
    let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32 != 0;
    Ok((provides, info.cbSize))
}

/// What one call for output gave.
enum Output {
    Sample(IMFSample),
    /// Nothing until it has more input.
    NeedMore,
    /// The output's format changed (a new size): set again.
    Changed,
}

unsafe fn take_output(transform: &IMFTransform, output_id: u32, provides: bool, size: u32) -> Result<Output> {
    let given = if provides { None } else { Some(sample_of(&vec![0u8; size.max(1) as usize], 0, 1)?) };
    let mut buffers = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: output_id, pSample: std::mem::ManuallyDrop::new(given), dwStatus: 0, pEvents: std::mem::ManuallyDrop::new(None) }];
    let mut status = 0u32;
    let result = transform.ProcessOutput(0, &mut buffers, &mut status);
    if debugging() {
        eprintln!("ProcessOutput (provides {provides}, size {size}): {result:?}, status {:x}", buffers[0].dwStatus);
    }
    let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
    drop(std::mem::ManuallyDrop::take(&mut buffers[0].pEvents));
    match result {
        Ok(()) => sample.map(Output::Sample).ok_or_else(|| anyhow!("it gave no picture")),
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Output::NeedMore),
        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(Output::Changed),
        Err(e) => Err(anyhow!("getting its output: {e}")),
    }
}

struct MfEncoder {
    transform: IMFTransform,
    /// What it was made from, which is told when it's done with: a
    /// chip has only so many encoders (NVIDIA's a handful), and one not
    /// given back may stay taken.
    activate: Option<IMFActivate>,
    /// For an asynchronous one (all hardware is): its events.
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    coding: Codec,
    name: String,
    shape: Shape,
    /// The rows a picture is coded in: its height, or more (see `set_up`).
    rows: usize,
    input_id: u32,
    output_id: u32,
    provides: bool,
    output_size: u32,
    frame: u64,
    /// Pictures it's asked for and not been given yet.
    wants: u32,
    /// Frames out and not yet passed on, oldest first.
    ready: VecDeque<Vec<u8>>,
    /// The latest SPS and PPS (and H.265's VPS) it gave, put in front of
    /// a full frame that comes without them (every frame has to be decodable from the
    /// last full one alone).
    parameter_sets: Option<Vec<u8>>,
}

impl MfEncoder {
    /// The first of the chip's `coding` encoders that takes `shape` at
    /// `bitrate`.
    unsafe fn open(shape: Shape, bitrate: u32, coding: Codec) -> Result<MfEncoder> {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype_of(coding) };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, Some(&input), Some(&output), &mut list, &mut count).context("listing encoders")?;
        let found: Vec<IMFActivate> = (0..count as usize).filter_map(|i| (*list.add(i)).take()).collect();
        if !list.is_null() {
            CoTaskMemFree(Some(list as *const _));
        }
        if found.is_empty() {
            bail!("Windows lists no {} encoder on the graphics chip", coding_name(coding));
        }
        let mut tried = Vec::new();
        for activate in &found {
            let name = friendly_name(activate, coding);
            match Self::start(activate, &name, shape, bitrate, coding) {
                Ok(encoder) => return Ok(encoder),
                Err(e) => {
                    let _ = activate.ShutdownObject();
                    tried.push(format!("{name}: {e:#}"));
                }
            }
        }
        bail!("{}", tried.join("; "))
    }

    unsafe fn start(activate: &IMFActivate, name: &str, shape: Shape, bitrate: u32, coding: Codec) -> Result<MfEncoder> {
        let transform: IMFTransform = activate.ActivateObject().context("starting it")?;
        Self::set_up(transform, Some(activate.clone()), name, shape, bitrate, coding)
    }

    /// Sets `transform` up for a call (see the module's notes).
    unsafe fn set_up(transform: IMFTransform, activate: Option<IMFActivate>, name: &str, shape: Shape, bitrate: u32, coding: Codec) -> Result<MfEncoder> {
        let attributes = transform.GetAttributes().ok();
        let asynchronous = attributes.as_ref().is_some_and(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0);
        if let Some(attributes) = &attributes {
            if asynchronous {
                attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("unlocking it")?;
            }
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        let codec = transform.cast::<ICodecAPI>().ok();
        if debugging() {
            eprintln!("encoder: {name}: asynchronous {asynchronous}, settings {}", codec.is_some());
        }
        let settings = |codec: &ICodecAPI| {
            let _ = set_value(codec, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
            let _ = set_value(codec, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
            let _ = set_value(codec, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
            let _ = set_value(codec, &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
            let _ = set_value(codec, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(shape.fps * swarm_core::video::KEYFRAME_EVERY_SECS));
        };
        if let Some(codec) = &codec {
            settings(codec);
        }
        let mut rows = shape.height;
        if let Err(e) = set_types(&transform, shape, rows, bitrate, coding) {
            if shape.height.is_multiple_of(16) {
                return Err(e);
            }
            rows = shape.height.next_multiple_of(16);
            set_types(&transform, shape, rows, bitrate, coding).map_err(|again| anyhow!("{e:#}; and at {rows} rows: {again:#}"))?;
        }
        if let Some(codec) = &codec {
            settings(codec);
        }
        let (input_id, output_id) = stream_ids(&transform);
        let (provides, output_size) = output_needs(&transform, output_id)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).context("starting it streaming")?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).context("starting its stream")?;
        let events = if asynchronous { Some(transform.cast::<IMFMediaEventGenerator>().context("its events")?) } else { None };
        Ok(MfEncoder {
            transform,
            activate,
            events,
            codec,
            coding,
            name: if rows == shape.height { name.to_string() } else { format!("{name}, {} rows coded as {rows}", shape.height) },
            shape,
            rows,
            input_id,
            output_id,
            provides,
            output_size: output_size.max((shape.width * rows * 3 / 2) as u32),
            frame: 0,
            wants: 0,
            ready: VecDeque::new(),
            parameter_sets: None,
        })
    }

    unsafe fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        let codec = self.codec.as_ref().ok_or_else(|| anyhow!("it has no settings to change"))?;
        set_value(codec, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate)).context("changing its bitrate")
    }

    unsafe fn encode(&mut self, nv12: &[u8], key: bool) -> Result<Vec<u8>> {
        if key {
            if let Some(codec) = &self.codec {
                let _ = set_value(codec, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32));
            }
        }
        let padded;
        let nv12 = if self.rows > self.shape.height {
            padded = padded_rows(nv12, self.shape.width, self.shape.height, self.rows);
            &padded
        } else {
            nv12
        };
        let sample = sample_of(nv12, self.frame, self.shape.fps)?;
        self.frame += 1;
        match self.events.clone() {
            Some(events) => {
                let deadline = Instant::now() + PATIENCE;
                while self.wants == 0 {
                    if !self.next_event(&events, deadline)? {
                        bail!("it stopped asking for pictures");
                    }
                }
                let given = self.transform.ProcessInput(self.input_id, &sample, 0);
                if debugging() {
                    eprintln!("encoder: ProcessInput picture {} (key {key}): {given:?}", self.frame - 1);
                }
                given.context("giving it a picture")?;
                self.wants -= 1;
                let waiting = Instant::now();
                while self.ready.is_empty() && !(self.wants > 0 && waiting.elapsed() > Duration::from_millis(5)) {
                    if !self.next_event(&events, waiting + OUTPUT_WAIT)? {
                        break;
                    }
                }
            }
            None => {
                self.transform.ProcessInput(self.input_id, &sample, 0).context("giving it a picture")?;
                while let Some(frame) = self.output()? {
                    self.ready.push_back(frame);
                }
            }
        }
        let Some(mut frame) = self.ready.pop_front() else {
            if debugging() {
                eprintln!("encoder: no frame out this time");
            }
            return Ok(Vec::new());
        };
        if debugging() {
            let kind = |b: &u8| if self.coding == Codec::H265 { b >> 1 & 0x3f } else { b & 0x1f };
            let kinds: Vec<u8> = crate::common::nal_units(&frame).iter().filter_map(|n| n.first().map(kind)).collect();
            eprintln!("encoder: frame of {} bytes, NAL units {kinds:?}, {} more ready", frame.len(), self.ready.len());
        }
        let sets = match self.coding {
            Codec::H264 => parameter_sets(&frame),
            Codec::H265 => crate::h265::parameter_sets(&frame),
        };
        match sets {
            Some(sets) => self.parameter_sets = Some(sets),
            None if swarm_core::video::is_key(&frame, self.coding) => {
                if let Some(sets) = &self.parameter_sets {
                    frame.splice(0..0, sets.iter().copied());
                } else if let Some(header) = self.transform.GetOutputCurrentType(self.output_id).ok().and_then(|t| blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER)) {
                    frame.splice(0..0, header.iter().copied());
                    self.parameter_sets = Some(header);
                }
            }
            None => {}
        }
        Ok(frame)
    }

    /// Waits until `deadline` for its next event, and acts on it: false
    /// if none came.
    unsafe fn next_event(&mut self, events: &IMFMediaEventGenerator, deadline: Instant) -> Result<bool> {
        loop {
            match events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(event) => {
                    let kind = event.GetType()?;
                    if debugging() {
                        eprintln!("encoder: event {kind} (601 wants a picture, 602 has a frame), wanting {}, {} ready", self.wants, self.ready.len());
                    }
                    if kind == METransformNeedInput.0 as u32 {
                        self.wants += 1;
                    } else if kind == METransformHaveOutput.0 as u32 {
                        if let Some(frame) = self.output()? {
                            self.ready.push_back(frame);
                        }
                    } else if kind == MEError.0 as u32 {
                        let status = event.GetStatus().unwrap_or_default();
                        bail!("it reported an error ({status:?})");
                    }
                    return Ok(true);
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    std::thread::sleep(Duration::from_micros(500));
                }
                Err(e) => bail!("waiting on it: {e}"),
            }
        }
    }

    /// One frame from it, if it has one.
    unsafe fn output(&mut self) -> Result<Option<Vec<u8>>> {
        let mut changed = false;
        loop {
            match take_output(&self.transform, self.output_id, self.provides, self.output_size) {
                Ok(Output::Sample(sample)) => return Ok(Some(bytes_of(&sample)?)),
                Ok(Output::NeedMore) => return Ok(None),
                Ok(Output::Changed) => {
                    let kind = self.transform.GetOutputAvailableType(self.output_id, 0)?;
                    self.transform.SetOutputType(self.output_id, &kind, 0)?;
                    (self.provides, self.output_size) = output_needs(&self.transform, self.output_id)?;
                    changed = true;
                }
                Err(_) if changed && self.events.is_some() => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            if let Some(activate) = &self.activate {
                let _ = activate.ShutdownObject();
            }
        }
    }
}

/// Sets an encoder's output type, then its input type, for pictures
/// `shape` big coded `rows` high (with the part to show said, if that's
/// more than the picture). For H.264, Baseline, which every Swarm's
/// OpenH264 decodes; failing that, Constrained Baseline. For H.265, Main
/// (8-bit 4:2:0), which every chip that decodes it does.
unsafe fn set_types(transform: &IMFTransform, shape: Shape, rows: usize, bitrate: u32, coding: Codec) -> Result<()> {
    let coded = Shape { height: rows, ..shape };
    let show_only = |kind: &IMFMediaType| -> Result<()> {
        if rows != shape.height {
            let mut area = [0u8; 16];
            area[8..12].copy_from_slice(&(shape.width as i32).to_le_bytes());
            area[12..16].copy_from_slice(&(shape.height as i32).to_le_bytes());
            kind.SetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &area)?;
        }
        Ok(())
    };
    let mut set = Err(anyhow!("no profile"));
    let profiles = match coding {
        Codec::H264 => vec![eAVEncH264VProfile_Base.0, eAVEncH264VProfile_ConstrainedBase.0],
        Codec::H265 => vec![eAVEncH265VProfile_Main_420_8.0],
    };
    for profile in profiles {
        let out = video_type(&subtype_of(coding), coded)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32)?;
        show_only(&out)?;
        set = transform.SetOutputType(0, &out, 0).map_err(|e| anyhow!("its output: {e}"));
        if set.is_ok() {
            break;
        }
    }
    set?;
    let input = video_type(&MFVideoFormat_NV12, coded)?;
    show_only(&input)?;
    transform.SetInputType(0, &input, 0).map_err(|e| anyhow!("its input: {e}"))
}

/// An NV12 picture `width`×`height` made `rows` high, the extra rows
/// copies of its last (of Y, and of UV).
fn padded_rows(nv12: &[u8], width: usize, height: usize, rows: usize) -> Vec<u8> {
    let (luma, chroma) = nv12.split_at(width * height);
    let mut out = Vec::with_capacity(width * rows * 3 / 2);
    out.extend_from_slice(luma);
    for _ in height..rows {
        out.extend_from_slice(&luma[luma.len() - width..]);
    }
    out.extend_from_slice(chroma);
    for _ in height / 2..rows / 2 {
        out.extend_from_slice(&chroma[chroma.len() - width..]);
    }
    out
}

unsafe fn blob(attributes: &IMFMediaType, key: &GUID) -> Option<Vec<u8>> {
    let size = attributes.GetBlobSize(key).ok()?;
    let mut data = vec![0u8; size as usize];
    attributes.GetBlob(key, &mut data, None).ok()?;
    Some(data)
}

struct MfDecoder {
    transform: IMFTransform,
    coding: Codec,
    name: String,
    input_id: u32,
    output_id: u32,
    provides: bool,
    output_size: u32,
    /// The pictures' size as it decodes them (rows padded to a multiple
    /// of 16, say), and the part of that to show.
    coded: (usize, usize),
    shown: (usize, usize),
    /// The size the stream's SPS says, for when the decoder doesn't say
    /// what part to show.
    said: Option<(usize, usize)>,
    /// The decoder said what part to show.
    aperture: bool,
    pitch: usize,
    frame: u64,
    _device: Option<(ID3D11Device, IMFDXGIDeviceManager)>,
}

impl MfDecoder {
    /// Microsoft's `coding` decoder, given Direct3D 11 so it decodes on
    /// the graphics chip; with `software`, without (for checking this
    /// code where there's no chip, under Wine say).
    unsafe fn open(software: bool, coding: Codec) -> Result<MfDecoder> {
        let (class, extensions) = match coding {
            Codec::H264 => (&CLSID_MSH264DecoderMFT, ""),
            Codec::H265 => (&CLSID_MSH265DecoderMFT, " (Windows' HEVC Video Extensions)"),
        };
        let what = coding_name(coding);
        let transform: IMFTransform = CoCreateInstance(class, None, CLSCTX_INPROC_SERVER).with_context(|| format!("starting Microsoft's {what} decoder{extensions}"))?;
        let attributes = transform.GetAttributes().ok();
        let mut device = None;
        if !software {
            let aware = attributes.as_ref().is_some_and(|a| a.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0);
            if !aware {
                bail!("this {what} decoder can't use the graphics chip");
            }
            let (d3d, manager) = direct3d().context("starting Direct3D 11")?;
            if coding == Codec::H265 && !decodes_on_chip(&d3d, &D3D11_DECODER_PROFILE_HEVC_VLD_MAIN)? {
                bail!("the graphics chip doesn't decode H.265");
            }
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize).context("giving it the graphics chip")?;
            device = Some((d3d, manager));
        }
        if let Some(attributes) = &attributes {
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        if let Ok(codec) = transform.cast::<ICodecAPI>() {
            let _ = set_value(&codec, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
        }
        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &subtype_of(coding))?;
        let (input_id, output_id) = stream_ids(&transform);
        transform.SetInputType(input_id, &input, 0).context("its input")?;
        let mut decoder = MfDecoder {
            transform,
            coding,
            name: if software { format!("Microsoft's {what} decoder (software)") } else { format!("Microsoft's {what} decoder, on the graphics chip (Direct3D 11)") },
            input_id,
            output_id,
            provides: false,
            output_size: 0,
            coded: (0, 0),
            shown: (0, 0),
            said: None,
            aperture: false,
            pitch: 0,
            frame: 0,
            _device: device,
        };
        decoder.choose_output()?;
        decoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        decoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        Ok(decoder)
    }

    /// Asks for NV12 out, and notes its size (afresh when it changes).
    unsafe fn choose_output(&mut self) -> Result<()> {
        let mut index = 0;
        loop {
            let kind = self.transform.GetOutputAvailableType(self.output_id, index).context("its output: no NV12")?;
            index += 1;
            if kind.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFVideoFormat_NV12) {
                continue;
            }
            self.transform.SetOutputType(self.output_id, &kind, 0).context("its output")?;
            let size = kind.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            self.coded = ((size >> 32) as usize, size as u32 as usize);
            self.shown = self.coded;
            self.aperture = false;
            if let Some(area) = blob(&kind, &MF_MT_MINIMUM_DISPLAY_APERTURE) {
                if area.len() >= 16 {
                    let cx = i32::from_le_bytes(area[8..12].try_into().unwrap_or_default());
                    let cy = i32::from_le_bytes(area[12..16].try_into().unwrap_or_default());
                    if cx > 0 && cy > 0 {
                        self.shown = ((cx as usize).min(self.coded.0), (cy as usize).min(self.coded.1));
                        self.aperture = true;
                    }
                }
            }
            self.pitch = kind.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32 as usize).ok().filter(|&p| p >= self.coded.0 && p < 1 << 16).unwrap_or(self.coded.0);
            (self.provides, self.output_size) = output_needs(&self.transform, self.output_id)?;
            self.output_size = self.output_size.max((self.pitch * self.coded.1 * 3 / 2) as u32);
            return Ok(());
        }
    }

    unsafe fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>> {
        let said = match self.coding {
            Codec::H264 => sps_size(frame),
            Codec::H265 => h265_size(frame),
        };
        if let Some(size) = said {
            self.said = Some(size);
        }
        let sample = sample_of(frame, self.frame, 25)?;
        self.frame += 1;
        let mut picture = None;
        let given = self.transform.ProcessInput(self.input_id, &sample, 0);
        if debugging() {
            eprintln!("decoder: ProcessInput {} bytes: {given:?}", frame.len());
        }
        if let Err(e) = given {
            if e.code() != MF_E_NOTACCEPTING {
                bail!("giving it a frame: {e}");
            }
            picture = self.drain()?;
            self.transform.ProcessInput(self.input_id, &sample, 0).context("giving it a frame")?;
        }
        Ok(self.drain()?.or(picture))
    }

    /// Everything it has ready, the newest picture kept.
    unsafe fn drain(&mut self) -> Result<Option<Picture>> {
        let mut picture = None;
        loop {
            match take_output(&self.transform, self.output_id, self.provides, self.output_size)? {
                Output::Sample(sample) => picture = self.picture_of(&sample)?.or(picture),
                Output::NeedMore => return Ok(picture),
                Output::Changed => self.choose_output()?,
            }
        }
    }

    unsafe fn picture_of(&self, sample: &IMFSample) -> Result<Option<Picture>> {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let (width, height) = match self.said {
            Some((w, h)) if !self.aperture && w <= self.coded.0 && h <= self.coded.1 => (w, h),
            _ => self.shown,
        };
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer2>() {
            let (mut top, mut start) = (std::ptr::null_mut(), std::ptr::null_mut());
            let (mut pitch, mut length) = (0i32, 0u32);
            two_d.Lock2DSize(MF2DBuffer_LockFlags_Read, &mut top, &mut pitch, &mut start, &mut length)?;
            let picture = if pitch > 0 && top == start {
                let pitch = pitch as usize;
                let rows = (length as usize * 2 / 3) / pitch;
                i420_from_nv12(std::slice::from_raw_parts(top, length as usize), width, height, pitch, pitch * rows.max(self.coded.1))
            } else {
                None
            };
            two_d.Unlock2D()?;
            return Ok(picture);
        }
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
            let mut top: *mut u8 = std::ptr::null_mut();
            let mut pitch = 0i32;
            two_d.Lock2D(&mut top, &mut pitch)?;
            let picture = if pitch > 0 {
                let pitch = pitch as usize;
                let length = pitch * self.coded.1 * 3 / 2;
                i420_from_nv12(std::slice::from_raw_parts(top, length), width, height, pitch, pitch * self.coded.1)
            } else {
                None
            };
            two_d.Unlock2D()?;
            return Ok(picture);
        }
        let mut at: *mut u8 = std::ptr::null_mut();
        let mut length = 0u32;
        buffer.Lock(&mut at, None, Some(&mut length))?;
        let picture = i420_from_nv12(std::slice::from_raw_parts(at, length as usize), width, height, self.pitch, self.pitch * self.coded.1);
        buffer.Unlock()?;
        Ok(picture)
    }
}

impl Drop for MfDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if self._device.is_some() {
                let _ = self.transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            }
        }
    }
}

/// The size an H.265 stream's SPS says its pictures are shown at.
fn h265_size(stream: &[u8]) -> Option<(usize, usize)> {
    let sps = crate::common::nal_units(stream).into_iter().find(|n| crate::h265::nal_type(n) == Some(crate::h265::SPS))?;
    crate::h265::Sps::parse(sps)?.shown().map(|(width, height, _, _)| (width, height))
}

/// Whether `device`'s chip decodes `profile` (one of Direct3D's
/// D3D11_DECODER_PROFILE_*) to NV12.
unsafe fn decodes_on_chip(device: &ID3D11Device, profile: &GUID) -> Result<bool> {
    let video = device.cast::<ID3D11VideoDevice>().context("its video")?;
    Ok(video.CheckVideoDecoderFormat(profile, DXGI_FORMAT_NV12).map(|ok| ok.as_bool()).unwrap_or(false))
}

/// A Direct3D 11 device on the graphics chip, able to decode video, and
/// a manager to share it with Media Foundation.
unsafe fn direct3d() -> Result<(ID3D11Device, IMFDXGIDeviceManager)> {
    let mut device = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
    D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, Default::default(), D3D11_CREATE_DEVICE_VIDEO_SUPPORT, Some(&levels), D3D11_SDK_VERSION, Some(&mut device), None, None)?;
    let device = device.ok_or_else(|| anyhow!("no device"))?;
    if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
        let _ = multithread.SetMultithreadProtected(true);
    }
    let mut token = 0u32;
    let mut manager = None;
    MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
    let manager = manager.ok_or_else(|| anyhow!("no device manager"))?;
    manager.ResetDevice(&device, token)?;
    Ok((device, manager))
}

/// The encoder driven through a stand-in for a chip's: an asynchronous
/// transform as Media Foundation describes one (events saying when it
/// wants a picture and when it has a frame, its own output samples, its
/// settings through ICodecAPI), with OpenH264 underneath. Run under Wine:
/// `cargo test --target x86_64-pc-windows-gnu -p swarm-app --lib
/// video_mf --no-run`, then the test program with `wine`.
#[cfg(test)]
mod tests {
    use swarm_core::lock::Locked;
    use super::*;
    use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod};
    use openh264::formats::YUVSlices;
    use openh264::OpenH264API;
    use std::sync::{Arc, Mutex};
    use windows::core::{implement, Ref, HRESULT};
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

    #[derive(Default)]
    struct Fake {
        encoder: Option<Encoder>,
        size: (usize, usize),
        ready: VecDeque<(Vec<u8>, bool)>,
        force_key: bool,
        bitrates: Vec<u32>,
        settings: Vec<GUID>,
        given: usize,
        /// Hands each frame out only after the next picture (pipelined),
        /// asking for that picture first.
        late: bool,
        /// Full frames after the first come without SPS and PPS.
        bare: bool,
        /// Takes only heights in whole macroblocks (16 rows).
        only_16: bool,
        /// As Intel's: its first output is a change of format, and after
        /// one it gives nothing until it says again it has a frame.
        changes: bool,
        /// Where that is: 1 the change to come, 2 the type set again.
        changing: u8,
        /// Its frames made over as H.265's (see `as_h265`).
        h265: bool,
        output_type: (u64, u64, u32, u32),
        subtype: GUID,
        profile: u32,
        keys: usize,
    }

    #[implement(IMFTransform, IMFMediaEventGenerator, ICodecAPI)]
    struct FakeMft {
        events: IMFMediaEventQueue,
        attributes: IMFAttributes,
        fake: Arc<Mutex<Fake>>,
    }

    fn not_done<T>() -> windows::core::Result<T> {
        Err(windows::core::Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }

    impl FakeMft {
        fn signal(&self, kind: MF_EVENT_TYPE) {
            unsafe {
                let _ = self.events.QueueEventParamVar(kind.0 as u32, &GUID::zeroed(), HRESULT(0), std::ptr::null());
            }
        }
    }

    impl IMFTransform_Impl for FakeMft_Impl {
        fn GetStreamLimits(&self, _: *mut u32, _: *mut u32, _: *mut u32, _: *mut u32) -> windows::core::Result<()> {
            not_done()
        }
        fn GetStreamCount(&self, _: *mut u32, _: *mut u32) -> windows::core::Result<()> {
            not_done()
        }
        fn GetStreamIDs(&self, _: u32, _: *mut u32, _: u32, _: *mut u32) -> windows::core::Result<()> {
            not_done()
        }
        fn GetInputStreamInfo(&self, _: u32, _: *mut MFT_INPUT_STREAM_INFO) -> windows::core::Result<()> {
            not_done()
        }
        fn GetOutputStreamInfo(&self, _: u32) -> windows::core::Result<MFT_OUTPUT_STREAM_INFO> {
            Ok(MFT_OUTPUT_STREAM_INFO { dwFlags: MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32, cbSize: 0, cbAlignment: 0 })
        }
        fn GetAttributes(&self) -> windows::core::Result<IMFAttributes> {
            Ok(self.attributes.clone())
        }
        fn GetInputStreamAttributes(&self, _: u32) -> windows::core::Result<IMFAttributes> {
            not_done()
        }
        fn GetOutputStreamAttributes(&self, _: u32) -> windows::core::Result<IMFAttributes> {
            not_done()
        }
        fn DeleteInputStream(&self, _: u32) -> windows::core::Result<()> {
            not_done()
        }
        fn AddInputStreams(&self, _: u32, _: *const u32) -> windows::core::Result<()> {
            not_done()
        }
        fn GetInputAvailableType(&self, _: u32, _: u32) -> windows::core::Result<IMFMediaType> {
            not_done()
        }
        fn GetOutputAvailableType(&self, _: u32, _: u32) -> windows::core::Result<IMFMediaType> {
            let (size, rate, bitrate, profile) = self.fake.locked().output_type;
            unsafe {
                let kind = MFCreateMediaType()?;
                kind.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                kind.SetGUID(&MF_MT_SUBTYPE, &self.fake.locked().subtype)?;
                kind.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
                kind.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
                kind.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
                kind.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
                Ok(kind)
            }
        }
        fn SetInputType(&self, _: u32, kind: Ref<IMFMediaType>, _: u32) -> windows::core::Result<()> {
            let kind = kind.ok()?;
            unsafe {
                assert_eq!(kind.GetGUID(&MF_MT_SUBTYPE)?, MFVideoFormat_NV12);
                if self.fake.locked().only_16 && !(kind.GetUINT64(&MF_MT_FRAME_SIZE)? as u32).is_multiple_of(16) {
                    return Err(windows::core::Error::from(MF_E_INVALIDMEDIATYPE));
                }
            }
            Ok(())
        }
        fn SetOutputType(&self, _: u32, kind: Ref<IMFMediaType>, _: u32) -> windows::core::Result<()> {
            let kind = kind.ok()?;
            let (size, rate, bitrate, profile) = unsafe { (kind.GetUINT64(&MF_MT_FRAME_SIZE)?, kind.GetUINT64(&MF_MT_FRAME_RATE)?, kind.GetUINT32(&MF_MT_AVG_BITRATE)?, kind.GetUINT32(&MF_MT_MPEG2_PROFILE)?) };
            let mut fake = self.fake.locked();
            if fake.only_16 && !(size as u32).is_multiple_of(16) {
                return Err(windows::core::Error::from(MF_E_INVALIDMEDIATYPE));
            }
            if fake.changing == 1 {
                fake.changing = 2;
                return Ok(());
            }
            if fake.changes {
                fake.changing = 1;
            }
            fake.output_type = (size, rate, bitrate, profile);
            fake.subtype = unsafe { kind.GetGUID(&MF_MT_SUBTYPE)? };
            fake.profile = profile;
            fake.size = ((size >> 32) as usize, size as u32 as usize);
            let config = EncoderConfig::new().bitrate(BitRate::from_bps(bitrate)).max_frame_rate(FrameRate::from_hz((rate >> 32) as f32)).intra_frame_period(IntraFramePeriod::from_num_frames(100_000));
            fake.encoder = Some(Encoder::with_api_config(OpenH264API::from_source(), config).unwrap());
            Ok(())
        }
        fn GetInputCurrentType(&self, _: u32) -> windows::core::Result<IMFMediaType> {
            not_done()
        }
        fn GetOutputCurrentType(&self, _: u32) -> windows::core::Result<IMFMediaType> {
            not_done()
        }
        fn GetInputStatus(&self, _: u32) -> windows::core::Result<u32> {
            not_done()
        }
        fn GetOutputStatus(&self) -> windows::core::Result<u32> {
            not_done()
        }
        fn SetOutputBounds(&self, _: i64, _: i64) -> windows::core::Result<()> {
            not_done()
        }
        fn ProcessEvent(&self, _: u32, _: Ref<IMFMediaEvent>) -> windows::core::Result<()> {
            not_done()
        }
        fn ProcessMessage(&self, message: MFT_MESSAGE_TYPE, _: usize) -> windows::core::Result<()> {
            if message == MFT_MESSAGE_NOTIFY_START_OF_STREAM {
                self.signal(METransformNeedInput);
            }
            Ok(())
        }
        fn ProcessInput(&self, _: u32, sample: Ref<IMFSample>, _: u32) -> windows::core::Result<()> {
            unsafe {
                if self.attributes.GetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK).unwrap_or(0) != 1 {
                    return Err(windows::core::Error::from(MF_E_TRANSFORM_ASYNC_LOCKED));
                }
                let nv12 = bytes_of(sample.ok()?).unwrap();
                let late = {
                    let mut fake = self.fake.locked();
                    let (w, h) = fake.size;
                    let picture = i420_from_nv12(&nv12, w, h, w, w * h).expect("an NV12 picture of the size set");
                    let (y, u, v) = picture.planes();
                    let key = std::mem::take(&mut fake.force_key);
                    let encoder = fake.encoder.as_mut().unwrap();
                    if key {
                        encoder.force_intra_frame();
                    }
                    let mut frame = encoder.encode(&YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2))).unwrap().to_vec();
                    let idr = swarm_core::video::has_idr(&frame);
                    if idr {
                        fake.keys += 1;
                        if fake.bare && fake.keys > 1 {
                            frame = crate::common::nal_units(&frame).into_iter().filter(|n| !matches!(n[0] & 0x1f, 7 | 8)).flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect();
                        }
                    }
                    if fake.h265 {
                        frame = as_h265(&frame);
                    }
                    fake.ready.push_back((frame, idr));
                    fake.given += 1;
                    fake.late
                };
                if late {
                    self.signal(METransformNeedInput);
                    if self.fake.locked().ready.len() > 1 {
                        self.signal(METransformHaveOutput);
                    }
                } else {
                    self.signal(METransformHaveOutput);
                    self.signal(METransformNeedInput);
                }
            }
            Ok(())
        }
        fn ProcessOutput(&self, _: u32, count: u32, buffers: *mut MFT_OUTPUT_DATA_BUFFER, status: *mut u32) -> windows::core::Result<()> {
            assert_eq!(count, 1);
            {
                let mut fake = self.fake.locked();
                if fake.changing == 1 && !fake.ready.is_empty() {
                    return Err(windows::core::Error::from(MF_E_TRANSFORM_STREAM_CHANGE));
                }
                if fake.changing == 2 {
                    fake.changing = 0;
                    drop(fake);
                    self.signal(METransformHaveOutput);
                    return Err(windows::core::Error::from(windows::Win32::Foundation::E_UNEXPECTED));
                }
            }
            let Some((frame, key)) = self.fake.locked().ready.pop_front() else {
                return Err(windows::core::Error::from(MF_E_TRANSFORM_NEED_MORE_INPUT));
            };
            unsafe {
                let buffer = &mut *buffers;
                assert!(buffer.pSample.is_none(), "it provides its own samples");
                let sample = sample_of(&frame, 0, 25).unwrap();
                sample.SetUINT32(&MFSampleExtension_CleanPoint, key as u32)?;
                buffer.pSample = std::mem::ManuallyDrop::new(Some(sample));
                *status = 0;
            }
            Ok(())
        }
    }

    impl IMFMediaEventGenerator_Impl for FakeMft_Impl {
        fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows::core::Result<IMFMediaEvent> {
            unsafe { self.events.GetEvent(flags.0) }
        }
        fn BeginGetEvent(&self, _: Ref<IMFAsyncCallback>, _: Ref<windows::core::IUnknown>) -> windows::core::Result<()> {
            not_done()
        }
        fn EndGetEvent(&self, _: Ref<IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
            not_done()
        }
        fn QueueEvent(&self, _: u32, _: *const GUID, _: HRESULT, _: *const PROPVARIANT) -> windows::core::Result<()> {
            not_done()
        }
    }

    impl ICodecAPI_Impl for FakeMft_Impl {
        fn IsSupported(&self, _: *const GUID) -> windows::core::Result<()> {
            Ok(())
        }
        fn IsModifiable(&self, _: *const GUID) -> HRESULT {
            HRESULT(0)
        }
        fn GetParameterRange(&self, _: *const GUID, _: *mut VARIANT, _: *mut VARIANT, _: *mut VARIANT) -> windows::core::Result<()> {
            not_done()
        }
        fn GetParameterValues(&self, _: *const GUID, _: *mut *mut VARIANT, _: *mut u32) -> windows::core::Result<()> {
            not_done()
        }
        fn GetDefaultValue(&self, _: *const GUID) -> windows::core::Result<VARIANT> {
            not_done()
        }
        fn GetValue(&self, _: *const GUID) -> windows::core::Result<VARIANT> {
            not_done()
        }
        fn SetValue(&self, api: *const GUID, value: *const VARIANT) -> windows::core::Result<()> {
            let (api, value) = unsafe { (*api, &*value) };
            let mut fake = self.fake.locked();
            fake.settings.push(api);
            if api == CODECAPI_AVEncVideoForceKeyFrame {
                fake.force_key = true;
            }
            if api == CODECAPI_AVEncCommonMeanBitRate {
                fake.bitrates.push(unsafe { value.Anonymous.Anonymous.Anonymous.ulVal });
            }
            Ok(())
        }
        fn RegisterForEvent(&self, _: *const GUID, _: isize) -> windows::core::Result<()> {
            not_done()
        }
        fn UnregisterForEvent(&self, _: *const GUID) -> windows::core::Result<()> {
            not_done()
        }
        fn SetAllDefaults(&self) -> windows::core::Result<()> {
            not_done()
        }
        fn SetValueWithNotify(&self, _: *const GUID, _: *const VARIANT, _: *mut *mut GUID, _: *mut u32) -> windows::core::Result<()> {
            not_done()
        }
        fn SetAllDefaultsWithNotify(&self, _: *mut *mut GUID, _: *mut u32) -> windows::core::Result<()> {
            not_done()
        }
        fn GetAllSettings(&self, _: Ref<windows::Win32::System::Com::IStream>) -> windows::core::Result<()> {
            not_done()
        }
        fn SetAllSettings(&self, _: Ref<windows::Win32::System::Com::IStream>) -> windows::core::Result<()> {
            not_done()
        }
        fn SetAllSettingsWithNotify(&self, _: Ref<windows::Win32::System::Com::IStream>, _: *mut *mut GUID, _: *mut u32) -> windows::core::Result<()> {
            not_done()
        }
    }

    fn fake_encoder(late: bool, bare: bool) -> (IMFTransform, Arc<Mutex<Fake>>) {
        fake_encoder_of(Fake { late, bare, ..Default::default() })
    }

    fn fake_encoder_of(fake: Fake) -> (IMFTransform, Arc<Mutex<Fake>>) {
        unsafe {
            let fake = Arc::new(Mutex::new(fake));
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 2).unwrap();
            let attributes = attributes.unwrap();
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC, 1).unwrap();
            let mft = FakeMft { events: MFCreateEventQueue().unwrap(), attributes, fake: fake.clone() };
            (mft.into(), fake)
        }
    }

    /// OpenH264's frame made over as an H.265 encoder's would be laid out
    /// (it doesn't decode): its SPS as a VPS and SPS, its PPS as a PPS,
    /// its IDR slice as IDR_W_RADL's, the rest as TRAIL_R's.
    fn as_h265(frame: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in crate::common::nal_units(frame) {
            let kinds: &[u8] = match nal[0] & 0x1f {
                7 => &[crate::h265::VPS, crate::h265::SPS],
                8 => &[crate::h265::PPS],
                5 => &[crate::h265::IDR_W_RADL],
                1 => &[crate::h265::TRAIL_R],
                _ => &[],
            };
            for &kind in kinds {
                out.extend_from_slice(&[0, 0, 0, 1, kind << 1, 1]);
                out.extend_from_slice(&nal[1..]);
            }
        }
        out
    }

    /// Drives the stand-in through 40 pictures, a full frame asked for at
    /// 20 and the bitrate changed at 30; every frame decodes in OpenH264.
    fn drive(late: bool, bare: bool) {
        drive_fake(Fake { late, bare, ..Default::default() });
    }

    fn drive_fake(fake: Fake) {
        let late = fake.late;
        let coding = if fake.h265 { Codec::H265 } else { Codec::H264 };
        let (width, height) = (640, 360);
        let background = crate::pattern::test_background((width as u32, height as u32));
        let (transform, fake) = fake_encoder_of(fake);
        let mut encoder = unsafe { MfEncoder::set_up(transform, None, "stand-in", Shape { width, height, fps: 25 }, 600_000, coding) }.unwrap();
        let mut decoder = swarm_core::video::VideoDecoder::software().unwrap();
        let (mut keys, mut shown, mut frames) = (Vec::new(), 0, 0);
        for i in 0..40u32 {
            if i == 30 {
                unsafe { encoder.set_bitrate(500_000) }.unwrap();
            }
            let picture = crate::pattern::test_pattern(i as f32 / 25.0, &background).to_i420().into_owned();
            let frame = unsafe { encoder.encode(&nv12_from(&picture), i == 0 || i == 20) }.unwrap();
            if frame.is_empty() {
                continue;
            }
            let key = swarm_core::video::is_key(&frame, coding);
            if key {
                keys.push(frames);
                let sets = match coding {
                    Codec::H264 => parameter_sets(&frame),
                    Codec::H265 => crate::h265::parameter_sets(&frame).filter(|s| crate::common::nal_units(s).len() == 3),
                };
                assert!(sets.is_some(), "a full frame, at {frames}, without its parameter sets");
            }
            frames += 1;
            if coding == Codec::H265 {
                shown += 1;
                continue;
            }
            let piece = swarm_core::video::VideoPiece { frame: frames - 1, index: 0, count: 1, key, h265: false, small: false, data: frame };
            shown += decoder.push(piece).picture.is_some() as u32;
        }
        let fake = fake.locked();
        assert_eq!(fake.given, 40);
        assert_eq!(frames, if late { 39 } else { 40 }, "a frame for each picture, but a late encoder's last");
        assert_eq!(shown, frames);
        assert_eq!(keys, vec![0, 20], "full frames where asked for");
        match coding {
            Codec::H264 => {
                assert_eq!(fake.subtype, MFVideoFormat_H264);
                assert_eq!(fake.profile, eAVEncH264VProfile_Base.0 as u32);
            }
            Codec::H265 => {
                assert_eq!(fake.subtype, MFVideoFormat_HEVC);
                assert_eq!(fake.profile, eAVEncH265VProfile_Main_420_8.0 as u32);
            }
        }
        assert_eq!(fake.bitrates.last(), Some(&500_000));
        for setting in [CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVLowLatencyMode, CODECAPI_AVEncMPVDefaultBPictureCount] {
            assert!(fake.settings.contains(&setting));
        }
    }

    fn start_media_foundation() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_LITE).unwrap();
        }
    }

    #[test]
    fn an_asynchronous_encoder_is_driven_as_media_foundation_says() {
        start_media_foundation();
        drive(false, false);
    }

    #[test]
    fn a_pipelined_encoder_frames_come_a_picture_later() {
        start_media_foundation();
        drive(true, false);
    }

    #[test]
    fn full_frames_without_their_sps_and_pps_get_them() {
        start_media_foundation();
        drive(false, true);
    }

    /// The stand-in as video's hardware.
    struct StandIn;

    impl Hardware for StandIn {
        fn encoder(&self, shape: Shape, bitrate: u32, codec: Codec) -> Result<Box<dyn HardEncoder>> {
            if codec == Codec::H265 {
                bail!("no H.265 stand-in in a call");
            }
            let worker = Worker::start(move || unsafe { MfEncoder::set_up(fake_encoder(false, false).0, None, "stand-in", shape, bitrate, Codec::H264) })?;
            Ok(Box::new(EncoderHandle { worker, name: "stand-in".to_string(), shape }))
        }
        fn decoder(&self, _: Codec) -> Result<Box<dyn HardDecoder>> {
            bail!("no stand-in decoder")
        }
    }

    /// A call's encoder on the stand-in, all the way: `swarm_core::video`
    /// choosing it, its thread, NV12, and Media Foundation's ways — the
    /// camera's RGB pictures in, frames other swarms decode out.
    #[test]
    fn a_calls_video_goes_through_the_stand_in() {
        swarm_core::video::provide_hardware(Box::new(StandIn));
        swarm_core::video::set_hardware(true);
        let mut encoder = swarm_core::video::VideoEncoder::new(2_000_000).unwrap();
        encoder.keep_shape();
        assert_eq!(encoder.hardware().as_deref(), Some("stand-in"));
        let background = crate::pattern::test_background((1280, 720));
        let mut decoder = swarm_core::video::VideoDecoder::software().unwrap();
        let (mut shown, mut keys) = (0, Vec::new());
        for i in 0..30 {
            if i == 15 {
                encoder.request_keyframe();
            }
            let pieces = encoder.encode(&crate::pattern::test_pattern(i as f32 / 25.0, &background), 1200).unwrap();
            if pieces.first().is_some_and(|p| p.key) {
                keys.push(i);
            }
            for piece in pieces {
                shown += decoder.push(piece).picture.is_some() as usize;
            }
        }
        assert_eq!(encoder.hardware().as_deref(), Some("stand-in"), "{:?}", swarm_core::video::hardware_trouble());
        assert_eq!(shown, 30);
        assert_eq!(keys, vec![0, 15]);
    }

    /// An encoder whose first output is a change of format, and which
    /// then gives the frame only when it says again it has one (as some
    /// of Intel's do).
    #[test]
    fn an_encoder_changing_its_format_first_is_waited_for() {
        start_media_foundation();
        drive_fake(Fake { changes: true, ..Default::default() });
    }

    /// An encoder taking only heights in whole macroblocks is given 360
    /// rows as 368, the last repeated, and told to show the 360.
    #[test]
    fn an_encoder_taking_only_whole_macroblocks_gets_padded_pictures() {
        start_media_foundation();
        drive_fake(Fake { only_16: true, ..Default::default() });
        let (transform, fake) = fake_encoder_of(Fake { only_16: true, ..Default::default() });
        let encoder = unsafe { MfEncoder::set_up(transform, None, "stand-in", Shape { width: 640, height: 360, fps: 25 }, 600_000, Codec::H264) }.unwrap();
        assert_eq!(encoder.name, "stand-in, 360 rows coded as 368");
        assert_eq!(fake.locked().size, (640, 368));
        let nv12 = padded_rows(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], 4, 2, 4);
        assert_eq!(nv12, vec![1, 2, 3, 4, 5, 6, 7, 8, 5, 6, 7, 8, 5, 6, 7, 8, 9, 10, 11, 12, 9, 10, 11, 12]);
    }

    /// An H.265 stream's pictures are shown at the size its SPS says,
    /// less what it crops off (1080 rows coded as 1088).
    #[test]
    fn an_h265_streams_size_is_read_from_its_sps() {
        let sps = crate::h265::Sps { ptl: crate::h265::Ptl::main(120), chroma_format_idc: 1, width: 1920, height: 1088, conformance: [0, 0, 0, 4], log2_diff_max_min_cb: 3, log2_diff_max_min_tb: 3, ..Default::default() };
        let stream = [&[0, 0, 0, 1][..], &sps.write(), &[0, 0, 0, 1, crate::h265::IDR_W_RADL << 1, 1, 0xaf]].concat();
        assert_eq!(h265_size(&stream), Some((1920, 1080)));
        assert_eq!(h265_size(&stream[stream.len() - 7..]), None);
    }

    /// An H.265 encoder is asked for H.265's Main profile, and its full
    /// frames are found and given their VPS, SPS and PPS as H.265's.
    #[test]
    fn an_h265_encoder_is_driven_as_an_h264_one() {
        start_media_foundation();
        drive_fake(Fake { h265: true, ..Default::default() });
        drive_fake(Fake { h265: true, bare: true, ..Default::default() });
        drive_fake(Fake { h265: true, late: true, changes: true, ..Default::default() });
    }
}
