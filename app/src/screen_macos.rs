//! ScreenCaptureKit delivers the primary display and cursor. The native
//! side keeps only the newest pixel buffer until this thread reads it.

use super::{Grabber, ScreenInfo, MAX_FPS};
use anyhow::{anyhow, bail, Result};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr::NonNull;
use swarm_core::video::{Picture, Shape};

extern "C" {
    fn swarm_macos_screen_open(
        width: *mut u32, height: *mut u32, fps: u32, error: *mut c_char, error_size: usize,
    ) -> *mut c_void;
    fn swarm_macos_screen_read(
        screen: *mut c_void, rgb: *mut u8, rgb_size: usize, wait: bool, error: *mut c_char, error_size: usize,
    ) -> c_int;
    fn swarm_macos_screen_close(screen: *mut c_void);
}

pub(super) struct MacScreen {
    handle: NonNull<c_void>,
    width: u32,
    height: u32,
    ended: Option<String>,
    rgb: Vec<u8>,
}

impl MacScreen {
    pub(super) fn open() -> Result<Self> {
        let mut error = [0; 512];
        let mut width = Shape::LARGEST.width as u32;
        let mut height = Shape::LARGEST.height as u32;
        // The native call treats width and height as limits on input and
        // replaces them with the dimensions of the selected display.
        let handle = unsafe {
            swarm_macos_screen_open(&mut width, &mut height, MAX_FPS, error.as_mut_ptr(), error.len())
        };
        let handle = NonNull::new(handle).ok_or_else(|| anyhow!(message(&error)))?;
        Ok(Self { handle, width, height, ended: None, rgb: Vec::new() })
    }

    fn read(&mut self, wait: bool) -> Result<Option<Picture>> {
        let (width, height) = (self.width as usize, self.height as usize);
        let len = width * height * 3;
        self.rgb.reserve_exact(len);
        let mut error = [0; 512];
        // Native code writes len bytes only when a correctly sized frame
        // is available. No part of the allocation is read otherwise.
        let status = unsafe {
            swarm_macos_screen_read(self.handle.as_ptr(), self.rgb.as_mut_ptr(), len, wait, error.as_mut_ptr(), error.len())
        };
        match status {
            1 => {
                let mut rgb = std::mem::take(&mut self.rgb);
                unsafe { rgb.set_len(len) };
                Ok(Some(Picture::rgb(width, height, rgb)))
            }
            0 => Ok(None),
            -1 => {
                let why = message(&error);
                self.ended = Some(why.clone());
                Err(anyhow!(why))
            }
            _ => bail!("{}", message(&error)),
        }
    }
}

impl Grabber for MacScreen {
    fn info(&self) -> ScreenInfo {
        let (width, height) = (self.width, self.height);
        ScreenInfo { name: format!("screen {width}×{height}"), width, height }
    }

    fn grab(&mut self) -> Result<Picture> {
        self.read(true)?.ok_or_else(|| anyhow!("macOS did not return a screen picture"))
    }

    fn next(&mut self) -> Result<Option<Picture>> {
        self.read(false)
    }

    fn ended(&self) -> Option<String> {
        self.ended.clone()
    }
}

impl Drop for MacScreen {
    fn drop(&mut self) {
        unsafe { swarm_macos_screen_close(self.handle.as_ptr()) };
    }
}

fn message(buffer: &[c_char]) -> String {
    // The buffer starts zeroed and the shim writes only NUL-terminated
    // strings to it.
    let text = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    if text.is_empty() {
        "macOS screen capture failed".into()
    } else {
        text.to_string_lossy().into_owned()
    }
}
