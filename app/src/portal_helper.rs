//! The desktop's portal, asked by a helper: swarm started afresh with
//! `HELPER`, holding the portal's sessions for as long as they're
//! wanted, and talking to swarm over a Unix socket (its stdin).
//!
//! Why: swarm keeps its memory out of dumps (see
//! `swarm_core::secure::no_dumps`), which also makes its
//! `/proc/<pid>/root` unreadable to the user's other processes, and the
//! portal reads it to know who's asking, so it turns swarm itself down
//! ("Unable to open /proc/…/root"). The file dialog has a helper of its
//! own for the same reason (`filedialog`). This one never holds a key.
//!
//! What's said, a line at a time:
//! - swarm first: `screen <restore>` or `control <restore>`, the restore
//!   token kept from last time (`-` for none);
//! - the helper: `ok <node> <restore>` (the screen's PipeWire node, and
//!   the new token, `-` for none), with, for the screen, the PipeWire
//!   connection's descriptor attached; or `err <why>`;
//! - then, for control, swarm: `p <x> <y>` (the pointer, 0.0 to 1.0 of
//!   the screen), `b <button> <0|1>`, `a <axis> <steps>`,
//!   `k <keysym> <0|1>`; and the helper `closed` once the desktop has
//!   closed the session itself.
//!
//! The helper ends, and with it the sessions, once swarm closes the
//! socket (or ends itself).

use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use zbus::blocking::Connection;

/// The first argument that makes swarm the portal's helper.
pub const HELPER: &str = "--portal-helper";

/// What the helper's asked to hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A screen to share: its PipeWire connection and node.
    Screen,
    /// The mouse and keys, for being controlled.
    Control,
}

/// Swarm's side of a helper.
pub(crate) struct Helper {
    stream: UnixStream,
    /// What's come after the answer, not read yet.
    reader: BufReader<std::io::Chain<std::io::Cursor<Vec<u8>>, UnixStream>>,
    child: Option<Child>,
}

/// The helper's answer.
pub(crate) struct Answer {
    pub node: u32,
    pub restore: Option<String>,
    pub fd: Option<OwnedFd>,
}

impl Helper {
    /// Starts one (swarm itself, afresh; or, with `bus`, a thread on that
    /// bus, for labs) and asks it for `kind`; see `answer`.
    pub(crate) fn start(kind: Kind, restore: Option<&str>, bus: Option<&str>) -> Result<Helper> {
        let (ours, theirs) = UnixStream::pair().context("making the portal helper's socket")?;
        let child = match bus {
            None => Some(
                Command::new(crate::filedialog::swarm_itself()?)
                    .arg(HELPER)
                    .stdin(Stdio::from(OwnedFd::from(theirs)))
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .context("starting the portal helper")?,
            ),
            Some(address) => {
                let address = address.to_string();
                std::thread::spawn(move || {
                    let conn = zbus::blocking::connection::Builder::address(address.as_str()).and_then(|b| b.build());
                    serve(theirs, conn.map_err(Into::into));
                });
                None
            }
        };
        let mut helper = Helper { reader: BufReader::new(std::io::Cursor::new(Vec::new()).chain(ours.try_clone()?)), stream: ours, child };
        let first = match kind {
            Kind::Screen => "screen",
            Kind::Control => "control",
        };
        helper.say(&format!("{first} {}", restore.unwrap_or("-")))?;
        Ok(helper)
    }

    /// Its process, if it's one.
    pub(crate) fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Waits for its answer: until the desktop's answered (its dialog,
    /// unless a choice it remembers is still good).
    pub(crate) fn answer(&mut self) -> Result<Answer> {
        let (line, fd, rest) = receive_line(&self.stream).context("the portal helper's answer")?;
        self.reader = BufReader::new(std::io::Cursor::new(rest).chain(self.stream.try_clone()?));
        let mut words = line.splitn(3, ' ');
        match words.next() {
            Some("ok") => {
                let node = words.next().and_then(|n| n.parse().ok()).ok_or_else(|| anyhow!("the portal helper's answer: {line}"))?;
                let restore = words.next().filter(|r| *r != "-").map(str::to_string);
                Ok(Answer { node, restore, fd })
            }
            Some("err") => bail!("{}", line.strip_prefix("err ").unwrap_or_default()),
            _ => bail!("the portal helper's answer: {line}"),
        }
    }

    /// Sends it a line.
    pub(crate) fn say(&mut self, line: &str) -> std::io::Result<()> {
        self.stream.write_all(format!("{line}\n").as_bytes())
    }

    /// Its next line (blocking); `None` once it's gone.
    pub(crate) fn hear(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(n) if n > 0 => Some(line.trim_end().to_string()),
            _ => None,
        }
    }

    /// A second handle on the socket, to hear it on another thread.
    pub(crate) fn listener(&self) -> std::io::Result<Helper> {
        Ok(Helper { stream: self.stream.try_clone()?, reader: BufReader::new(std::io::Cursor::new(Vec::new()).chain(self.stream.try_clone()?)), child: None })
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Reads one line, and a descriptor sent with it if there is one; and
/// whatever came after the line.
fn receive_line(stream: &UnixStream) -> Result<(String, Option<OwnedFd>, Vec<u8>)> {
    let mut got = Vec::new();
    let mut fd = None;
    loop {
        let mut buf = [0u8; 4096];
        let mut control = [0u8; 64];
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        let n = unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = control.len() as _;
            let n = libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let raw: RawFd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const RawFd);
                    fd = Some(OwnedFd::from_raw_fd(raw));
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
            n
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if n == 0 {
            bail!("the portal helper ended without answering");
        }
        got.extend_from_slice(&buf[..n as usize]);
        if let Some(end) = got.iter().position(|&b| b == b'\n') {
            let rest = got.split_off(end + 1);
            got.pop();
            return Ok((String::from_utf8_lossy(&got).into_owned(), fd, rest));
        }
    }
}

/// Sends one line, with `fd` attached if given.
fn send_line(stream: &UnixStream, line: &str, fd: Option<RawFd>) -> std::io::Result<()> {
    let bytes = format!("{line}\n");
    let Some(fd) = fd else {
        return (&*stream).write_all(bytes.as_bytes());
    };
    unsafe {
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut control = vec![0u8; space];
        let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut _, iov_len: bytes.len() };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, fd);
        let n = libc::sendmsg(stream.as_raw_fd(), &msg, 0);
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if (n as usize) < bytes.len() {
            (&*stream).write_all(&bytes.as_bytes()[n as usize..])?;
        }
    }
    Ok(())
}

/// Run as the helper (`swarm --portal-helper`, its stdin the socket):
/// its exit code.
pub fn main() -> i32 {
    let stream = UnixStream::from(unsafe { OwnedFd::from_raw_fd(0) });
    serve(stream, Connection::session().map_err(Into::into));
    0
}

/// The helper's work, on `stream`, through `conn`.
fn serve(stream: UnixStream, conn: Result<Connection>) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let mut words = first.split_whitespace();
    let kind = words.next().unwrap_or_default().to_string();
    let restore = words.next().filter(|r| *r != "-").map(str::to_string);
    let conn = match conn.context("reaching the desktop's session bus") {
        Ok(conn) => conn,
        Err(e) => {
            let _ = send_line(&stream, &format!("err {}", one_line(&e)), None);
            return;
        }
    };
    match kind.as_str() {
        "screen" => match crate::screen::wayland_ask(&conn, restore.as_deref()) {
            Ok((fd, node, restore)) => {
                if send_line(&stream, &format!("ok {node} {}", restore.as_deref().unwrap_or("-")), Some(fd.as_raw_fd())).is_err() {
                    return;
                }
                let mut rest = Vec::new();
                let _ = reader.read_to_end(&mut rest);
            }
            Err(e) => {
                let _ = send_line(&stream, &format!("err {}", one_line(&e)), None);
            }
        },
        "control" => crate::remote_wayland::serve_helper(&conn, restore.as_deref(), &stream, reader),
        other => {
            let _ = send_line(&stream, &format!("err the portal helper can't do \"{other}\""), None);
        }
    }
}

/// Answers the helper's `ok` for control.
pub(crate) fn answer_ok(stream: &UnixStream, restore: Option<&str>) -> std::io::Result<()> {
    send_line(stream, &format!("ok 0 {}", restore.unwrap_or("-")), None)
}

/// Answers the helper's `err`.
pub(crate) fn answer_err(stream: &UnixStream, why: &anyhow::Error) -> std::io::Result<()> {
    send_line(stream, &format!("err {}", one_line(why)), None)
}

/// Tells swarm the desktop's closed the session.
pub(crate) fn say_closed(stream: &UnixStream) -> std::io::Result<()> {
    send_line(stream, "closed", None)
}

fn one_line(e: &anyhow::Error) -> String {
    format!("{e:#}").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answer's line, with or without a descriptor, and whatever
    /// follows it read next, wherever it was left (Linux ends a read at
    /// a descriptor; without one, both lines may come in one read).
    #[test]
    fn a_line_and_a_descriptor_go_across_together_and_what_follows_is_kept() {
        let file = std::fs::File::open("/dev/null").unwrap();
        for fd in [Some(file.as_raw_fd()), None] {
            let (a, b) = UnixStream::pair().unwrap();
            send_line(&a, "ok 7 token", fd).unwrap();
            (&a).write_all(b"closed\n").unwrap();
            drop(a);
            std::thread::sleep(std::time::Duration::from_millis(50));
            let (line, got, rest) = receive_line(&b).unwrap();
            assert_eq!(line, "ok 7 token");
            assert_eq!(got.is_some(), fd.is_some(), "the descriptor, if sent");
            let mut next = String::new();
            BufReader::new(std::io::Cursor::new(rest).chain(b)).read_to_string(&mut next).unwrap();
            assert_eq!(next, "closed\n");
        }
    }
}
