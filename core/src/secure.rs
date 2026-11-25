//! Keeping keys out of places they could be read from later: the swap
//! file, and a crash dump. Keys are wiped when they're finished with
//! (`zeroize`, where they're made); here, the long-lived ones (an
//! identity, a data folder's keys) are kept in memory that's never
//! swapped out, and the process never leaves a dump of its memory.

/// No core dump (Unix) or crash report with memory (Windows) is ever
/// made of this process: one would hold its keys. Done once, first
/// thing; best-effort.
pub fn no_dumps() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
    #[cfg(unix)]
    unsafe {
        let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &none);
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::ErrorReporting::WerAddExcludedApplication;
        use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
        let mut path = [0u16; 1024];
        let len = GetModuleFileNameW(std::ptr::null_mut(), path.as_mut_ptr(), path.len() as u32) as usize;
        if len > 0 && len < path.len() {
            WerAddExcludedApplication(path.as_ptr(), 0);
        }
    }
}

/// Keeps `value`'s memory from being swapped out, for as long as it
/// lives (it's never unlocked: what it holds is wiped when it's
/// dropped). For a key held in one place, behind an `Arc` or a `Box`, so
/// it doesn't move. Best-effort: the system may limit how much can be
/// kept.
pub fn keep_in_memory<T>(value: &T) {
    let (start, len) = (value as *const T as *const u8, std::mem::size_of::<T>());
    if len == 0 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::mlock(start as *const libc::c_void, len);
    }
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Memory::VirtualLock(start as *const core::ffi::c_void, len);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_key_kept_in_memory_is_locked_there() {
        let locked_kb = || {
            std::fs::read_to_string("/proc/self/status").unwrap().lines().find(|l| l.starts_with("VmLck")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()).unwrap()
        };
        let before = locked_kb();
        let key = Box::new([7u8; 8192]);
        keep_in_memory(&*key);
        assert!(locked_kb() >= before + 8, "{} kB locked, {before} before", locked_kb());
    }
}
