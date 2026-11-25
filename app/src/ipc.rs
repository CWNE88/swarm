//! The control socket's plumbing: how other `swarm` commands on this
//! device reach the running node (see `run_control_socket` in `main.rs`
//! for what's said over it).
//!
//! - **Unix:** a socket file, `~/.swarm/control.sock`, in a folder only
//!   its owner can open.
//! - **Windows:** a named pipe, `\\.\pipe\swarm-<n>`, `<n>` worked out from
//!   the data folder's path (so two data folders never share one, as two
//!   socket files wouldn't). Remote clients are refused. Windows' default
//!   pipe security gives full access only to its creator, administrators
//!   and the system; anyone else can at most open it to read — and a
//!   request has to be written, so they can't make one.

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
        let next = ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(&name)?;
        Ok(Listener { name, next })
    }

    impl Listener {
        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            self.next.connect().await?;
            let fresh = ServerOptions::new().reject_remote_clients(true).create(&self.name)?;
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
