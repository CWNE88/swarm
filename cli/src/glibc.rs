//! Keeps the Linux binary running on glibc older than the build machine's.
//!
//! Rust's standard library refers (weakly) to glibc 2.39's `pidfd_spawnp`
//! and `pidfd_getpid`, which it uses only when a child process is started
//! with a pidfd (an unstable feature swarm never uses). Linked against a
//! newer glibc, those weak references still bring in the version
//! GLIBC_2.39, and an older system's loader then complains at start
//! ("weak version `GLIBC_2.39' not found"). Defined here, they're found in
//! the binary itself and glibc isn't asked for them. They say "not
//! supported", as an older glibc would, so if std ever did call them it
//! would fall back to its usual way of starting a process.
//! `tools/build-all.sh` checks no newer glibc than 2.36 is needed.

use std::ffi::{c_char, c_int, c_void};

const ENOSYS: c_int = 38;

extern "C" {
    fn __errno_location() -> *mut c_int;
}

#[no_mangle]
extern "C" fn pidfd_spawnp(
    _pidfd: *mut c_int,
    _file: *const c_char,
    _file_actions: *const c_void,
    _attr: *const c_void,
    _argv: *const *mut c_char,
    _envp: *const *mut c_char,
) -> c_int {
    ENOSYS
}

#[no_mangle]
extern "C" fn pidfd_getpid(_pidfd: c_int) -> c_int {
    // SAFETY: glibc's own errno for this thread.
    unsafe { *__errno_location() = ENOSYS };
    -1
}
