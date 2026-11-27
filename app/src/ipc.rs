//! The control socket's plumbing: how other `swarm` commands on this
//! device reach the running node (see `run_control_socket` in `main.rs`
//! for what's said over it).
//!
//! - **Unix:** a socket file, `~/.swarm/control.sock`, in a folder only
//!   its owner can open.
//! - **Windows:** a named pipe, `\\.\pipe\swarm-<n>`, `<n>` worked out from
//!   the data folder's path (so two data folders never share one, as two
//!   socket files wouldn't). Remote clients are refused. The pipe is made
//!   owned by this user's account, and only that account and the system
//!   may open it. As its name can be worked out (and pipes listed), a
//!   client checks the pipe it opened is owned by its own account before
//!   saying anything (a passphrase, even): another user who made one of
//!   that name first can't make it theirs.

use std::io;
use std::path::Path;

#[cfg(unix)]
mod imp {
    use super::*;
    use tokio::net::{UnixListener, UnixStream};

    pub type ServerStream = UnixStream;
    pub type ClientStream = UnixStream;

    pub struct Listener(UnixListener);

    pub fn bind(dir: &Path) -> io::Result<Listener> {
        std::fs::create_dir_all(dir)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        let path = dir.join("control.sock");
        let _ = std::fs::remove_file(&path);
        UnixListener::bind(&path).map(Listener)
    }

    impl Listener {
        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            self.0.accept().await.map(|(stream, _)| stream)
        }
    }

    pub async fn connect(dir: &Path) -> io::Result<ClientStream> {
        UnixStream::connect(dir.join("control.sock")).await
    }

    /// A blocking connection, for the commands that don't run tokio.
    pub fn connect_blocking(dir: &Path) -> io::Result<std::os::unix::net::UnixStream> {
        std::os::unix::net::UnixStream::connect(dir.join("control.sock"))
    }

    pub fn describe(dir: &Path) -> String {
        dir.join("control.sock").display().to_string()
    }

    /// After the answer's written: nothing more to do on Unix — closing
    /// the socket delivers what's been written, then end-of-file.
    pub async fn finish(_stream: &mut ServerStream) {}
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::time::Duration;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};

    pub type ServerStream = NamedPipeServer;
    pub type ClientStream = NamedPipeClient;

    /// The pipe for the data folder `dir`.
    fn pipe_name(dir: &Path) -> String {
        let path = dir.to_string_lossy().to_lowercase();
        let hash = path.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        format!(r"\\.\pipe\swarm-{hash:016x}")
    }

    /// The pipe instance waiting for the next connection.
    pub struct Listener {
        name: String,
        next: NamedPipeServer,
    }

    pub fn bind(dir: &Path) -> io::Result<Listener> {
        std::fs::create_dir_all(dir)?;
        let name = pipe_name(dir);
        // `first_pipe_instance`: fails if another swarm (or anyone else)
        // already has it.
        let next = create(ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true), &name)?;
        Ok(Listener { name, next })
    }

    /// An instance of the pipe, owned by this user and open to nobody
    /// else (and the system).
    fn create(options: &mut ServerOptions, name: &str) -> io::Result<NamedPipeServer> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        let sid = own::sid_string()?;
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)").encode_utf16().chain([0]).collect();
        let mut descriptor = std::ptr::null_mut();
        // Safe: a NUL-ended string, and the descriptor it makes freed below.
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut attributes = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: descriptor, bInheritHandle: 0 };
        // Safe: the attributes and their descriptor outlive the call.
        let made = unsafe { options.create_with_security_attributes_raw(name, &mut attributes as *mut _ as *mut _) };
        unsafe { LocalFree(descriptor) };
        made
    }

    /// This process's user, and pipes' owners checked against it.
    mod own {
        use std::io;
        use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
        use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT};
        use windows_sys::Win32::Security::{EqualSid, GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        /// This process's user: a `TOKEN_USER`, its SID inside it.
        struct User(Vec<u64>);

        impl User {
            fn get() -> io::Result<User> {
                let mut token: HANDLE = std::ptr::null_mut();
                // Safe: our own process's token, closed below.
                if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut buffer = vec![0u64; 64];
                let mut needed = 0u32;
                let ok = unsafe { GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), (buffer.len() * 8) as u32, &mut needed) };
                unsafe { CloseHandle(token) };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(User(buffer))
            }

            fn sid(&self) -> *mut core::ffi::c_void {
                // Safe: filled in by `GetTokenInformation` as a TOKEN_USER.
                unsafe { (*(self.0.as_ptr() as *const TOKEN_USER)).User.Sid }
            }
        }

        /// This user's SID, as SDDL writes it ("S-1-5-21-…").
        pub fn sid_string() -> io::Result<String> {
            let user = User::get()?;
            let mut text = std::ptr::null_mut();
            // Safe: a SID from the token; the string it makes freed below.
            if unsafe { ConvertSidToStringSidW(user.sid(), &mut text) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
            let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
            unsafe { LocalFree(text.cast()) };
            Ok(sid)
        }

        /// That the pipe `handle` is to is owned by this user: nothing's
        /// said to one another user made.
        pub fn check(handle: HANDLE) -> io::Result<()> {
            let user = User::get()?;
            let (mut owner, mut descriptor) = (std::ptr::null_mut(), std::ptr::null_mut());
            // Safe: an open handle; the descriptor it gives freed below
            // (`owner` points into it).
            let error = unsafe { GetSecurityInfo(handle, SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION, &mut owner, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut descriptor) };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            let same = unsafe { EqualSid(owner, user.sid()) } != 0;
            unsafe { LocalFree(descriptor) };
            if same {
                Ok(())
            } else {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "the control pipe is another user's: not swarm's"))
            }
        }
    }

    impl Listener {
        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            self.next.connect().await?;
            let fresh = create(ServerOptions::new().reject_remote_clients(true), &self.name)?;
            Ok(std::mem::replace(&mut self.next, fresh))
        }
    }

    /// `ERROR_PIPE_BUSY`: every instance is in use for a moment.
    const PIPE_BUSY: i32 = 231;

    pub async fn connect(dir: &Path) -> io::Result<ClientStream> {
        let name = pipe_name(dir);
        for _ in 0..20 {
            match ClientOptions::new().open(&name) {
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => tokio::time::sleep(Duration::from_millis(50)).await,
                Ok(pipe) => {
                    use std::os::windows::io::AsRawHandle;
                    own::check(pipe.as_raw_handle() as _)?;
                    return Ok(pipe);
                }
                other => return other,
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "the control pipe stayed busy"))
    }

    /// A blocking connection, for the commands that don't run tokio: a
    /// named pipe opens like a file.
    pub fn connect_blocking(dir: &Path) -> io::Result<std::fs::File> {
        let name = pipe_name(dir);
        for _ in 0..20 {
            match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => std::thread::sleep(Duration::from_millis(50)),
                Ok(pipe) => {
                    use std::os::windows::io::AsRawHandle;
                    own::check(pipe.as_raw_handle() as _)?;
                    return Ok(pipe);
                }
                other => return other,
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "the control pipe stayed busy"))
    }

    pub fn describe(dir: &Path) -> String {
        pipe_name(dir)
    }

    /// After the answer's written: waits until the other end has read it
    /// all (`FlushFileBuffers` does exactly that on a pipe) — closing the
    /// server end any sooner could throw unread data away.
    pub async fn finish(stream: &mut ServerStream) {
        use std::os::windows::io::AsRawHandle;
        let handle = stream.as_raw_handle() as usize;
        let _ = tokio::task::spawn_blocking(move || unsafe { windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(handle as _) }).await;
    }
}

pub use imp::*;

/// Reads everything until the other end closes. On Windows a closed pipe
/// reads as an error rather than end-of-file, so an error after the
/// answer counts as its end.
pub async fn read_answer<R: tokio::io::AsyncRead + Unpin>(stream: R) -> Option<String> {
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(stream);
    let mut answer = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => answer.push_str(&line),
            Err(_) if !answer.is_empty() => break,
            Err(_) => return None,
        }
    }
    Some(answer)
}
