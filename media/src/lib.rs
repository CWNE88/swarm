//! Video on the graphics chip (see `swarm_core::video::Hardware`): each
//! system's own interface, behind the one trait. Windows has it through
//! Media Foundation (`video_mf`), Linux through NVENC and NVDEC
//! (`video_nv`, NVIDIA's chips) or VA-API (`video_va`, Intel's and
//! AMD's), Android through MediaCodec (`video_android`, encoding only).
//! `swarm-app` chooses which (`hardware::init`).

pub mod common;
pub mod h264;
pub mod h265;
pub mod jpeg;
pub mod pattern;
#[cfg(target_os = "linux")]
pub mod nv_sys;
#[cfg(any(target_os = "linux", test))]
pub mod va_sys;
#[cfg(target_os = "android")]
pub mod video_android;
#[cfg(windows)]
pub mod video_mf;
#[cfg(target_os = "linux")]
pub mod video_nv;
#[cfg(target_os = "linux")]
pub mod video_va;
