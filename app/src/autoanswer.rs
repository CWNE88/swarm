//! Answering calls with no screen (`swarm listen`): refused at once,
//! unless they're from a peer, or another of your devices, set to be
//! answered automatically (see
//! `swarm_core::settings::AutoAnswer`) — then answered, microphone on
//! (and the camera, for video), until they hang up. One call at a time,
//! and never into a conference.

use crate::audio;
use std::path::PathBuf;
use std::sync::Arc;
use swarm_core::node;
use tokio::sync::{mpsc, oneshot};

/// Calls to `swarm listen`, which has no screen to ring on: refused at
/// once, unless they're from a peer set to be answered automatically
/// (`swarm contact answer`), or another of your devices (`swarm device
/// answer`) — then answered, microphone on (and the camera,
/// for video), until they hang up. One call at a time.
pub async fn answer_calls(node: node::NodeHandle, peers_path: PathBuf, mut call_events: mpsc::UnboundedReceiver<node::CallEvent>) {
    let mut current: Option<tokio::task::JoinHandle<()>> = None;
    while let Some(event) = call_events.recv().await {
        let (from, link, video, conference) = match event {
            node::CallEvent::Incoming { from, link, video, conference, .. } => (from, link, video, conference),
            node::CallEvent::LinkAsked { link, .. } => {
                let _ = link.reject();
                continue;
            }
            _ => continue,
        };
        if conference.is_some() {
            let _ = link.reject();
            continue;
        }
        if current.as_ref().is_some_and(|call| !call.is_finished()) {
            let _ = link.reject();
            continue;
        }
        let settings = crate::paths().settings();
        let key = match node::devices::call_device(&from) {
            Some(device) => Some(device),
            None => swarm_core::peers::load(&peers_path).ok().and_then(|all| all.into_iter().find(|p| p.handle() == from || node.same_person(&p.handle()).contains(&from))).map(|p| p.public_key),
        };
        let answer = key.and_then(|key| swarm_core::settings::auto_answer(&settings, &key).answers(video));
        match answer {
            Some(with_video) => {
                let device = node::devices::call_device(&from).and_then(|key| node.devices().devices.into_iter().find(|d| d.key == key));
                let shown = device.map(|d| crate::devices::in_calls(&d)).unwrap_or(from);
                current = Some(tokio::spawn(answer_call(shown, link, with_video)));
            }
            None => {
                let _ = link.reject();
            }
        }
    }
}

async fn answer_call(from: String, link: swarm_core::call::CallLink, with_video: bool) {
    let (mic_tx, mic_rx) = mpsc::unbounded_channel();
    let (speaker_tx, speaker_rx) = mpsc::unbounded_channel();
    let audio_handle = match tokio::task::block_in_place(|| audio::start(mic_tx, speaker_rx)) {
        Ok(audio) => audio,
        Err(e) => {
            eprintln!("call from {from}: couldn't open audio, refused: {e:#}");
            let _ = link.reject();
            return;
        }
    };
    let mut video = None;
    let mut video_io = None;
    let mut report = None;
    if with_video {
        let mut call_video = crate::calls::CallVideo::start(true);
        match call_video.camera_note.clone() {
            Some(note) => eprintln!("call from {from}: no camera, answering with voice only: {note}"),
            None => {
                let io = call_video.link([0; 32]);
                report = Some(tokio::spawn(report_video(from.clone(), io.stats.clone(), Some(call_video.remotes[0].picture.clone()))));
                video_io = Some(io);
                video = Some(call_video);
            }
        }
    }
    if let Err(e) = link.accept(video_io.is_some()) {
        eprintln!("call from {from}: couldn't answer: {e:#}");
        tokio::task::block_in_place(|| audio_handle.stop());
        return;
    }
    eprintln!("answered {from} automatically{}", if video_io.is_some() { ", with video" } else { "" });
    let (_hangup_tx, hangup_rx) = oneshot::channel();
    let _ = run_fitting_camera(swarm_core::call::run(link, mic_rx, speaker_tx, hangup_rx, video_io), video.as_mut()).await;
    if let Some(report) = report {
        report.abort();
    }
    if let Some(mut video) = video {
        video.camera_off();
    }
    tokio::task::block_in_place(|| audio_handle.stop());
    eprintln!("call with {from} ended");
}

/// Every five seconds of a video call with no screen (`swarm call
/// --video`, or one answered here): pictures sent and received a second,
/// the bitrate going out, and — when `shown` has them — the size of theirs.
pub async fn report_video(peer_name: String, stats: Arc<swarm_core::call::VideoStats>, shown: Option<tokio::sync::watch::Receiver<Option<Arc<swarm_core::video::Picture>>>>) {
    use std::sync::atomic::Ordering::Relaxed;
    const EVERY: u64 = 5;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(EVERY));
    tick.tick().await;
    let (mut sent, mut bytes, mut got) = (0, 0, 0);
    loop {
        tick.tick().await;
        let (sent_now, bytes_now, got_now) = (stats.frames_sent.load(Relaxed), stats.bytes_sent.load(Relaxed), stats.frames_shown.load(Relaxed));
        let size = match &shown {
            Some(shown) => shown.borrow().as_ref().map(|p| format!(", {}×{}", p.width, p.height)).unwrap_or_default(),
            None => String::new(),
        };
        eprintln!(
            "video: sending {} a second{} at {} of {} kbit/s ({} full frames in all); from {peer_name}, {} a second{size} ({} full frames asked for)",
            (sent_now - sent) / EVERY,
            stats
                .sending()
                .map(|s| format!(
                    ", {}×{} (up to {}; encoding {:.1} ms{}, decoding {:.1} ms{})",
                    s.width,
                    s.height,
                    s.fps,
                    stats.encode_micros.load(Relaxed) as f32 / 1000.0,
                    stats.on_chip().0,
                    stats.decode_micros.load(Relaxed) as f32 / 1000.0,
                    stats.on_chip().1
                ))
                .unwrap_or_default(),
            (bytes_now - bytes) * 8 / EVERY / 1000,
            stats.bitrate.load(Relaxed) / 1000,
            stats.keyframes_sent.load(Relaxed),
            (got_now - got) / EVERY,
            stats.keyframes_asked.load(Relaxed),
        );
        (sent, bytes, got) = (sent_now, bytes_now, got_now);
    }
}

/// Runs `call` to its end, meanwhile keeping `video`'s camera at the size
/// the call wants (see `calls::CallVideo::fit_camera`).
pub async fn run_fitting_camera<T>(call: impl std::future::Future<Output = T>, mut video: Option<&mut crate::calls::CallVideo>) -> T {
    tokio::pin!(call);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(300));
    loop {
        tokio::select! {
            out = &mut call => return out,
            _ = tick.tick() => {
                if let Some(video) = video.as_deref_mut() {
                    video.fit_camera();
                }
            }
        }
    }
}
