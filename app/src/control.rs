//! The control socket: how other `swarm` commands on this device act
//! through the running app (TUI, desktop app or `swarm listen`) instead
//! of on their own — so a message sent from another terminal goes through
//! the connection that's already up, under its cover, and a peer added
//! from the command line is picked up live. One request per connection:
//! a command word and its fields, one per line; the answer is `OK ...` or
//! `ERROR <reason>`. A Unix socket, or a named pipe on Windows (see `ipc`).

use crate::ipc;
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::io::Write;
use std::path::PathBuf;
use swarm_core::node::{Delivery, NodeHandle, PostsResult};
use swarm_core::peers::{self, Peer};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Local IPC for other `swarm` commands on this device, so they act
/// through the running node instead of on their own: one request per
/// connection, answered with an `OK ...` or `ERROR <reason>` line
/// (`LIST_POSTS` follows its `OK` with the posts, one per line).
///   `SEND\n<name>\n<message>\n` — send through the node; answers
///   `OK delivered`, `OK queued` or `OK held` (see `node::Delivery`).
///   `LIST_POSTS\n<name>\n` — `OK live` or `OK cached <why>`, then posts.
///   `OFFER_FILE\n<name>\n<path>\n` — `OK <transfer id>`;
///   `ACCEPT_FILE\n<id>\n<folder>\n`, `DECLINE_FILE`/`CANCEL_FILE\n<id>\n`;
///   `CLEAR_FILES\n<ids, comma-separated, or all>\n` — `OK <how many>`,
///   finished ones cleared from the list.
///   A group is named by its tag and maker's key (`channel::ChannelId`):
///   `GROUP_NEW\n<contacts, comma-separated>\n` — `OK <tag> <maker>`;
///   `GROUP_POST\n<tag>\n<maker>\n<text>\n`,
///   `GROUP_ADD\n<tag>\n<maker>\n<contact>\n`,
///   `GROUP_NAME\n<tag>\n<maker>\n<name>\n` (blank: the members'),
///   `GROUP_LEAVE\n<tag>\n<maker>\n` — `OK`;
///   `GROUP_SYNC\n<tag>\n<maker>\n` — `OK`, catching up started.
///   `PING\n<name>\n` — `OK <round trip in microseconds>`, over the
///   connection that's up. (A one-off connection of the command's own to
///   someone already connected would be taken as the peer connection on
///   their side and knock it over.)
///   `ADD_PEER`/`SET_PEER_COVER\n<name>\n`, and
///   `UPDATE_PEER\n<old name>\n<new name>\n` — the peers file has
///   changed; the node picks the change up from it.
///   `REMOVE_PEER\n<name>\n<key>\n<delete|block>\n` — they're off the
///   peers file: their connections end, and what's kept about them is
///   deleted (blocked: their key kept as that).
///   Your devices, each by its name or the start of its key:
///   `DEVICES\n` — `OK`, then a `device\t<description>` line for each;
///   `DEVICE_REMOVE\n<device>\n`, `DEVICE_RELAY\n<device>\n<on|off>\n`
///   (let the device relay for your others, or stop it),
///   `DEVICE_ADDRESS\n<device>\n<addresses>\n` (where to try it; empty clears it),
///   `DEVICE_ANSWER\n<device>\n<off|voice|video>\n` (how its calls are
///   answered here),
///   `DEVICE_CONTROL\n<device>\n<on|off>\n` (whether it may control
///   this one's shared screen without asking),
///   `RENAME_DEVICE\n<name>\n` (this device's name among them) — `OK
///   <what happened>`.
///   `SET_PROFILE\n<status>\n<base64 picture>\n` — `OK`: your status and
///   picture (see `swarm_core::profile`), peers told.
/// A Unix socket, or a named pipe on Windows — see `ipc`. Other front
/// ends (a desktop or phone app) drive the node through `NodeHandle`
/// directly.
pub async fn serve(node: NodeHandle) {
    serve_at(control_dir(), node).await
}

/// `serve`, with the socket in `dir`.
async fn serve_at(dir: PathBuf, node: NodeHandle) {
    let mut listener = match ipc::bind(&dir) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("control socket: opening {}: {e}", ipc::describe(&dir));
            return;
        }
    };
    loop {
        let stream = match listener.accept().await {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("control socket: accept failed: {e}");
                continue;
            }
        };
        let node = node.clone();
        tokio::spawn(async move {
            let _ = handle_control_connection(stream, node).await;
        });
    }
}

/// A note as an answer: `OK <it>`, or `ERROR <it>` if it says something
/// went wrong.
fn said(note: crate::notes::Note) -> String {
    if note.kind == crate::notes::Kind::Error { format!("ERROR {}\n", note.text) } else { format!("OK {}\n", note.text) }
}

/// The peers file's current record for `name`, if it has one.
/// The answer to something done to the group `tag` made by `owner`.
fn group_said(tag: &str, owner: &str, f: impl FnOnce(swarm_core::channel::ChannelId) -> anyhow::Result<()>) -> String {
    match crate::peerbook::decode_key(owner).and_then(|owner| f(swarm_core::channel::ChannelId { name: tag.to_string(), owner })) {
        Ok(()) => "OK\n".to_string(),
        Err(e) => format!("ERROR {}\n", swarm_core::text::single_line(&format!("{e:#}"))),
    }
}

fn peer_on_file(node: &NodeHandle, name: &str) -> Option<Peer> {
    let all = peers::load(&node.paths().peers()).ok()?;
    peers::find(&all, name).ok().cloned()
}

async fn handle_control_connection(stream: ipc::ServerStream, node: NodeHandle) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut lines = Vec::new();
    let mut tag = String::new();
    reader.read_line(&mut tag).await?;
    let wanted = match tag.trim_end() {
        "SEND" | "UPDATE_PEER" | "OFFER_FILE" | "ACCEPT_FILE" => 2,
        "DEVICE_RELAY" | "DEVICE_ANSWER" | "DEVICE_CONTROL" | "DEVICE_ADDRESS" => 2,
        "RENAME_DEVICE" | "DEVICE_REMOVE" => 1,
        "DECLINE_FILE" | "CANCEL_FILE" | "CLEAR_FILES" => 1,
        "GROUP_POST" | "GROUP_ADD" | "GROUP_NAME" => 3,
        "GROUP_LEAVE" | "GROUP_SYNC" | "SET_PROFILE" => 2,
        "GROUP_NEW" => 1,
        "REMOVE_PEER" => 3,
        "PING" | "LIST_POSTS" | "ADD_PEER" | "SET_PEER_COVER" | "UNLOCK" => 1,
        _ => 0,
    };
    for _ in 0..wanted {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        lines.push(line.trim_end().to_string());
    }
    let response = match (tag.trim_end(), lines.as_slice()) {
        ("SEND", [name, text]) => match node.send_message(name, text).await {
            Ok(Delivery::Delivered) => "OK delivered\n".to_string(),
            Ok(Delivery::Queued) => "OK queued\n".to_string(),
            Ok(Delivery::HeldUntilAdded) => "OK held\n".to_string(),
            Ok(Delivery::Relayed) => "OK relayed\n".to_string(),
            Err(e) => format!("ERROR {e}\n"),
        },
        ("LIST_POSTS", [name]) => match node.fetch_posts(name).await {
            Ok(result) => {
                let (posts, head) = match result {
                    PostsResult::Live(posts) => (posts, "OK live".to_string()),
                    PostsResult::Cached { posts, reason } => (posts, format!("OK cached {}", swarm_core::text::single_line(&reason))),
                };
                let mut body = format!("{head}\n");
                for post in posts {
                    body.push_str(&format!("{}\t{}\t{}\t{}\n", post.created_at, post.visibility.as_str(), STANDARD.encode(post.signature), post.text));
                }
                body
            }
            Err(e) => format!("ERROR {e}\n"),
        },
        ("ADD_PEER", [name]) => match peer_on_file(&node, name) {
            Some(peer) => {
                node.add_peer_live(peer);
                "OK\n".to_string()
            }
            None => format!("ERROR no contact named '{name}' on file\n"),
        },
        ("UPDATE_PEER", [old, new]) => match peer_on_file(&node, new) {
            Some(peer) => {
                node.update_peer_live(old, peer);
                "OK\n".to_string()
            }
            None => format!("ERROR no contact named '{new}' on file\n"),
        },
        ("SET_PEER_COVER", [name]) => match peer_on_file(&node, name) {
            Some(peer) => {
                node.set_cover_live(name, peer.cover_bits_per_second, peer.cover_required);
                "OK\n".to_string()
            }
            None => format!("ERROR no contact named '{name}' on file\n"),
        },
        ("OFFER_FILE", [name, path]) => match node.offer_file(name, std::path::Path::new(path)).await {
            Ok(id) => format!("OK {}\n", swarm_core::transfer::format_id(&id)),
            Err(e) => format!("ERROR {e:#}\n"),
        },
        ("ACCEPT_FILE", [id, folder]) => match swarm_core::transfer::parse_id(id) {
            Some(id) => match node.accept_transfer(&id, std::path::Path::new(folder)) {
                Ok(()) => "OK\n".to_string(),
                Err(e) => format!("ERROR {e:#}\n"),
            },
            None => "ERROR bad transfer id\n".to_string(),
        },
        ("DECLINE_FILE", [id]) => match swarm_core::transfer::parse_id(id).map(|id| node.decline_transfer(&id)) {
            Some(Ok(())) => "OK\n".to_string(),
            Some(Err(e)) => format!("ERROR {e:#}\n"),
            None => "ERROR bad transfer id\n".to_string(),
        },
        ("CANCEL_FILE", [id]) => match swarm_core::transfer::parse_id(id).map(|id| node.cancel_transfer(&id)) {
            Some(Ok(())) => "OK\n".to_string(),
            Some(Err(e)) => format!("ERROR {e:#}\n"),
            None => "ERROR bad transfer id\n".to_string(),
        },
        ("CLEAR_FILES", [ids]) => {
            let ids: Option<Vec<_>> = if ids.trim() == "all" {
                Some(node.transfers().into_iter().map(|t| t.transfer.id).collect())
            } else {
                ids.split(',').map(|id| swarm_core::transfer::parse_id(id.trim())).collect()
            };
            match ids.map(|ids| node.clear_transfers(&ids)) {
                Some(Ok(n)) => format!("OK {n}\n"),
                Some(Err(e)) => format!("ERROR {e:#}\n"),
                None => "ERROR bad transfer id\n".to_string(),
            }
        }
        ("GROUP_NEW", [contacts]) => {
            let contacts: Vec<&str> = contacts.split(',').map(str::trim).filter(|c| !c.is_empty()).collect();
            match node.create_group(&contacts) {
                Ok((group, failed)) if failed.is_empty() => format!("OK {} {}\n", group.name, base64::Engine::encode(&base64::engine::general_purpose::STANDARD, group.owner)),
                Ok((_, failed)) => format!("ERROR {}\n", swarm_core::text::single_line(&failed.join("; "))),
                Err(e) => format!("ERROR {e:#}\n"),
            }
        }
        ("GROUP_POST", [tag, owner, text]) => group_said(tag, owner, |id| node.post_to_channel(&id, text)),
        ("GROUP_ADD", [tag, owner, contact]) => group_said(tag, owner, |id| node.add_to_group(&id, contact)),
        ("GROUP_NAME", [tag, owner, name]) => group_said(tag, owner, |id| node.rename_group(&id, name)),
        ("GROUP_LEAVE", [tag, owner]) => group_said(tag, owner, |id| node.leave_group(&id)),
        ("GROUP_SYNC", [tag, owner]) => group_said(tag, owner, |id| {
            node.resync_channel(&id);
            Ok(())
        }),
        ("PING", [name]) => match node.ping(name).await {
            Ok(rtt) => format!("OK {}\n", rtt.as_micros()),
            Err(e) => format!("ERROR {e:#}\n"),
        },
        ("RENAME_DEVICE", [name]) => said(crate::actions::rename_device(&node, name)),
        ("SET_PROFILE", [status, avatar]) => {
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            match STANDARD.decode(avatar).map_err(anyhow::Error::from).and_then(|avatar| node.set_profile(status, &avatar).map_err(Into::into)) {
                Ok(()) => {
                    crate::avatar::changed_own();
                    "OK\n".to_string()
                }
                Err(e) => format!("ERROR {e:#}\n"),
            }
        }
        ("REMOVE_PEER", [name, key, how]) => match crate::peerbook::decode_key(key) {
            Ok(key) => {
                node.forget_contact(name, key, *how == "block");
                crate::avatar::forget(&key);
                "OK\n".to_string()
            }
            Err(e) => format!("ERROR {e:#}\n"),
        },
        ("DEVICES", []) => {
            let mut body = "OK\n".to_string();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            for device in node.devices().devices {
                body.push_str(&format!("device\t{}\n", crate::devices::describe(&device, now)));
            }
            body
        }
        ("DEVICE_REMOVE", [device]) => {
            let view = node.devices();
            match crate::devices::find(&view, device) {
                Ok(found) if found.this => "ERROR that's this device — another of yours takes it out\n".to_string(),
                Ok(found) => said(crate::actions::remove_device(&node, found)),
                Err(e) => format!("ERROR {e}\n"),
            }
        }
        ("DEVICE_ADD", []) => match node.add_device_code() {
            Ok(code) => format!("OK {}\n", code.line()),
            Err(e) => format!("ERROR {e:#}\n"),
        },
        ("DEVICE_RELAY", [device, on]) => {
            let view = node.devices();
            match crate::devices::find(&view, device) {
                Ok(found) => said(crate::actions::allow_relay(&node, found, on == "on")),
                Err(e) => format!("ERROR {e}\n"),
            }
        }
        ("DEVICE_ADDRESS", [device, addresses]) => {
            let view = node.devices();
            match crate::devices::find(&view, device) {
                Ok(found) => said(crate::actions::set_device_address(&node, found, addresses)),
                Err(e) => format!("ERROR {e}\n"),
            }
        }
        ("DEVICE_ANSWER", [device, mode]) => {
            let view = node.devices();
            match (crate::devices::find(&view, device), swarm_core::settings::AutoAnswer::parse(mode)) {
                (Ok(found), Some(answer)) if !found.this => said(crate::actions::device_answer(&node, found, Some(answer))),
                (Ok(_), Some(_)) => "ERROR that's this device — its own calls don't come to it\n".to_string(),
                (Ok(_), None) => format!("ERROR '{mode}' isn't one of off, voice or video\n"),
                (Err(e), _) => format!("ERROR {e}\n"),
            }
        }
        ("DEVICE_CONTROL", [device, on]) => {
            let view = node.devices();
            match crate::devices::find(&view, device) {
                Ok(found) if !found.this => said(crate::actions::device_control(&node, found, Some(*on == "on"))),
                Ok(_) => "ERROR that's this device — it can't control its own screen\n".to_string(),
                Err(e) => format!("ERROR {e}\n"),
            }
        }
        ("UNLOCK", _) => "ERROR it's already unlocked\n".to_string(),
        (other, _) => format!("ERROR unknown request '{other}'\n"),
    };
    let mut stream = reader.into_inner();
    stream.write_all(response.as_bytes()).await?;
    ipc::finish(&mut stream).await;
    Ok(())
}

/// Waits, locked, for `swarm unlock` (`UNLOCK\n<passphrase>\n` on the
/// control socket) to give the passphrase of the identity at
/// `identity_path`: a `swarm listen` started with nobody at its keyboard.
/// Anything else asked meanwhile is told it's locked. The socket's left
/// for `serve` to take over once the node's up.
pub async fn wait_for_unlock(identity_path: PathBuf) -> Result<swarm_core::identity::Identity> {
    use crate::launch::UnlockFailed;
    let dir = control_dir();
    let mut listener = ipc::bind(&dir).map_err(|e| anyhow::anyhow!("control socket: opening {}: {e}", ipc::describe(&dir)))?;
    loop {
        let Ok(stream) = listener.accept().await else { continue };
        let mut reader = BufReader::new(stream);
        let (mut tag, mut pass) = (String::new(), zeroize::Zeroizing::new(String::new()));
        const SAY_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);
        if !matches!(tokio::time::timeout(SAY_WITHIN, reader.read_line(&mut tag)).await, Ok(Ok(_))) {
            continue;
        }
        let mut unlocked = None;
        let answer = if tag.trim_end() == "UNLOCK" && matches!(tokio::time::timeout(SAY_WITHIN, reader.read_line(&mut pass)).await, Ok(Ok(_))) {
            let path = identity_path.clone();
            let pass = zeroize::Zeroizing::new(pass.trim_end_matches(['\r', '\n']).to_string());
            match tokio::task::spawn_blocking(move || crate::launch::unlock(&path, &pass)).await {
                Ok(Ok(identity)) => {
                    unlocked = Some(identity);
                    "OK\n".to_string()
                }
                Ok(Err(UnlockFailed::WrongPassphrase)) => "ERROR that isn't the passphrase\n".to_string(),
                Ok(Err(UnlockFailed::Other(e))) => format!("ERROR {}\n", swarm_core::text::single_line(&format!("{e:#}"))),
                Err(e) => format!("ERROR {e}\n"),
            }
        } else {
            "ERROR Swarm is locked here: run `swarm unlock` first\n".to_string()
        };
        let mut stream = reader.into_inner();
        let _ = stream.write_all(answer.as_bytes()).await;
        ipc::finish(&mut stream).await;
        if let Some(identity) = unlocked {
            return Ok(identity);
        }
    }
}

/// Asks the running node over the control socket, waiting for its
/// one-line answer: for commands with no runtime of their own.
pub fn ask_blocking(request: &str) -> Result<String> {
    use std::io::{BufRead, BufReader as StdBufReader};
    let mut stream = ipc::connect_blocking(&control_dir())?;
    stream.write_all(request.as_bytes())?;
    let mut answer = String::new();
    StdBufReader::new(stream).read_line(&mut answer)?;
    Ok(answer)
}

/// Asks the running node (if there is one) over the control socket.
/// `None` when there's no node to ask.
pub async fn ask_node(request: &str) -> Option<String> {
    let mut stream = ipc::connect(&control_dir()).await.ok()?;
    stream.write_all(request.as_bytes()).await.ok()?;
    ipc::read_answer(stream).await
}

/// Whether a node is running on this device (its control socket answers).
pub async fn node_running() -> bool {
    ipc::connect(&control_dir()).await.is_ok()
}

/// Refuses to start a second long-running swarm (TUI or `listen`) next
/// to one that's already up. Checked up front, before the TUI takes over
/// the terminal, so the reason is printed somewhere readable instead of
/// leaving a TUI running with no network behind it — whose sends would
/// then quietly relay through the *other* instance's control socket.
/// Two checks, because either alone misses a case:
/// - the control socket answering means a swarm is running for this
///   user, even on a different `--port` (and `run_control_socket` would
///   otherwise replace that one's socket with ours);
/// - the UDP port being taken catches one running under another user or
///   `HOME`, which has its own control socket.
///
/// A socket *file* nobody answers on is just left over from an unclean
/// exit — `run_control_socket` already clears that.
pub fn ensure_single_instance(port: u16) -> Result<()> {
    let dir = control_dir();
    if ipc::connect_blocking(&dir).is_ok() {
        anyhow::bail!("Swarm is already running on this device (its control socket {} answers) — quit that one first", ipc::describe(&dir));
    }
    let addr = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
    if let Err(e) = std::net::UdpSocket::bind(addr) {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            anyhow::bail!(
                "UDP port {port} is already in use — probably another swarm running under a different \
                 user or data folder"
            );
        }
    }
    Ok(())
}

/// Where the control socket lives (`ipc` decides what it is there).
pub fn control_dir() -> PathBuf {
    crate::data_dir()
}

/// Best-effort: if a `swarm listen` — standalone, or the interactive
/// menu's background one — is running locally, tell it over the
/// control socket about a peer add/update/remove so it takes effect
/// right away. Silently does nothing if no local listener is running
/// (or its control socket couldn't be reached for any other reason) —
/// the on-disk peers file is already correct either way, this is only
/// about an already-running process's in-memory state; without it, the
/// change would just wait for `listen`'s next restart to be noticed.
/// Uses a plain blocking `std` socket rather than pulling in a tokio
/// runtime for one request/response, and works the same whether the
/// listener sharing this socket file is a separate process or
/// (interactive mode) this same one.
pub fn notify(request: &str) {
    use std::io::{BufRead, BufReader as StdBufReader};

    let Ok(mut stream) = ipc::connect_blocking(&control_dir()) else { return };
    if stream.write_all(request.as_bytes()).is_err() {
        return;
    }
    let mut response = String::new();
    let _ = StdBufReader::new(stream).read_line(&mut response);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A node of its own, its control socket served from its folder.
    async fn serving(label: &str) -> (NodeHandle, PathBuf) {
        let dir = std::env::temp_dir().join(format!("swc-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = swarm_core::node::NodeConfig { paths: swarm_core::node::Paths::new(&dir), port: 0, cover_bits_per_second: None, log: Arc::new(|_| {}) };
        let (node, call_events) = swarm_core::node::start(Arc::new(swarm_core::identity::Identity::generate()), config).await.unwrap();
        drop(call_events);
        tokio::spawn(serve_at(dir.clone(), node.clone()));
        for _ in 0..50 {
            if ipc::connect(&dir).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        (node, dir)
    }

    async fn ask(dir: &std::path::Path, request: &str) -> String {
        let mut stream = ipc::connect(dir).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        ipc::read_answer(stream).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn each_request_is_answered_ok_or_with_why_not() {
        let (node, dir) = serving("answers").await;
        assert_eq!(ask(&dir, "FROBNICATE\n").await, "ERROR unknown request 'FROBNICATE'\n");
        assert_eq!(ask(&dir, "UNLOCK\nhunter2\n").await, "ERROR it's already unlocked\n");
        assert_eq!(ask(&dir, "DECLINE_FILE\nnot-hex\n").await, "ERROR bad transfer id\n");
        assert_eq!(ask(&dir, "ACCEPT_FILE\nzz\n/tmp\n").await, "ERROR bad transfer id\n");
        assert_eq!(ask(&dir, "CLEAR_FILES\nzz\n").await, "ERROR bad transfer id\n");
        assert_eq!(ask(&dir, "CLEAR_FILES\nall\n").await, "OK 0\n");
        assert!(ask(&dir, "SEND\nnobody\nhi\n").await.starts_with("ERROR "));
        assert!(ask(&dir, "GROUP_SYNC\nnews\nnot a key\n").await.starts_with("ERROR "));
        let made = ask(&dir, "GROUP_NEW\n\n").await;
        let (tag, maker) = made.trim_end().strip_prefix("OK ").and_then(|rest| rest.split_once(' ')).unwrap_or_else(|| panic!("{made}"));
        assert_eq!(ask(&dir, &format!("GROUP_NAME\n{tag}\n{maker}\nLunch\n")).await, "OK\n");
        assert_eq!(ask(&dir, &format!("GROUP_POST\n{tag}\n{maker}\nhello\n")).await, "OK\n");
        assert!(ask(&dir, &format!("GROUP_ADD\n{tag}\n{maker}\nnobody\n")).await.starts_with("ERROR "));
        assert_eq!(ask(&dir, &format!("GROUP_SYNC\n{tag}\n{maker}\n")).await, "OK\n");
        assert_eq!(ask(&dir, &format!("GROUP_LEAVE\n{tag}\n{maker}\n")).await, "OK\n");
        assert!(ask(&dir, "SET_PROFILE\nhere\n!!not base64!!\n").await.starts_with("ERROR "));

        assert_eq!(ask(&dir, "ADD_PEER\nbob\n").await, "ERROR no contact named 'bob' on file\n");
        let bob = Peer { name: "bob".into(), address: "[::1]:9".into(), public_key: [5; 32], cover_bits_per_second: None, cover_required: false };
        peers::add(&node.paths().peers(), &bob).unwrap();
        assert_eq!(ask(&dir, "ADD_PEER\nbob\n").await, "OK\n");
        assert!(node.peer_status("bob").is_some(), "the node has bob");
        assert_eq!(ask(&dir, &format!("REMOVE_PEER\nbob\n{}\ndelete\n", STANDARD.encode([5u8; 32]))).await, "OK\n");
        assert!(node.peer_status("bob").is_none(), "and doesn't now");

        assert_eq!(ask(&dir, "RENAME_DEVICE\ndesk\n").await, "OK this device is called desk now\n");
        assert_eq!(ask(&dir, "RENAME_DEVICE\ndesk\n").await, "OK this device is called desk already\n");
        let code = ask(&dir, "DEVICE_ADD\n").await;
        assert!(code.starts_with("OK swarm-device:") || code.starts_with("ERROR "), "{code}");
        assert_eq!(ask(&dir, "DEVICE_RELAY\nnone\non\n").await.split(' ').next(), Some("ERROR"));
        assert_eq!(ask(&dir, "DEVICE_ANSWER\ndesk\nvoice\n").await, "ERROR that's this device — its own calls don't come to it\n");
        assert_eq!(ask(&dir, "DEVICE_ANSWER\nnone\nvoice\n").await.split(' ').next(), Some("ERROR"));
        let address = ask(&dir, "DEVICE_ADDRESS\nnone\n2001:db8::1\n").await;
        assert!(address.starts_with("ERROR ") && !address.contains("unknown request"), "{address}");
        assert_eq!(ask(&dir, "DEVICES\n").await.lines().count(), 2, "OK, and this device");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_cut_short_leaves_the_socket_answering() {
        let (_node, dir) = serving("cut").await;
        for request in ["SEND\n", "GROUP_POST\nnews\n", "SEND"] {
            let mut stream = ipc::connect(&dir).await.unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            drop(stream);
        }
        let mut stream = ipc::connect(&dir).await.unwrap();
        stream.write_all(b"SEND\nbob\n").await.unwrap();
        stream.shutdown().await.unwrap();
        assert!(ipc::read_answer(stream).await.unwrap().starts_with("ERROR "));
        assert!(ask(&dir, "DEVICES\n").await.starts_with("OK\n"), "still answering");
    }
}
