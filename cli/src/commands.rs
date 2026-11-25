//! What the command line's commands do, past unlocking: contacts,
//! `listen`, `ping`, `send`, posts, calls, channels, devices and file
//! transfers.

use super::*;

pub(crate) fn peer_add(name: String, address: String, pubkey: String, out: PathBuf) -> Result<()> {
    eprintln!("{}", peerbook::add(&expand_home(&out), &name, &address, &pubkey, None)?);
    Ok(())
}

pub(crate) fn peer_update(name: String, new_name: Option<String>, address: String, pubkey: String, out: PathBuf) -> Result<()> {
    let new_name = new_name.unwrap_or_else(|| name.clone());
    eprintln!("{}", peerbook::update(&expand_home(&out), &name, &new_name, &address, &pubkey, None)?);
    Ok(())
}

pub(crate) fn peer_set_cover(name: String, cover_bits_per_second: Option<u64>, required: bool, out: PathBuf) -> Result<()> {
    eprintln!("{}", peerbook::set_cover(&expand_home(&out), &name, cover_bits_per_second, required, None)?);
    Ok(())
}

pub(crate) fn peer_remove(name: String, block: bool, out: PathBuf) -> Result<()> {
    eprintln!("{}", peerbook::remove(&expand_home(&out), &name, block, None)?);
    Ok(())
}

pub(crate) fn peer_list(path: PathBuf) -> Result<()> {
    let path = expand_home(&path);
    let all = peers::load(&path).with_context(|| format!("loading {}", path.display()))?;
    if all.is_empty() {
        eprintln!("no contacts in {}", path.display());
        return Ok(());
    }
    for p in all {
        println!("{}\t{}\t{}", p.name, p.address, STANDARD.encode(p.public_key));
    }
    Ok(())
}

pub(crate) fn peer_requests() -> Result<()> {
    let pending = requests::pending(&paths().requests()).context("reading contact requests")?;
    if pending.is_empty() {
        eprintln!("no contact requests");
        return Ok(());
    }
    let names = displayname::cached_all(&paths().displaynames()).unwrap_or_default();
    for request in pending {
        let name = names.get(&request.public_key).map(|dn| dn.name.as_str()).unwrap_or("-");
        println!(
            "{name}\t{}\t{}\tfirst seen {}",
            request.address,
            STANDARD.encode(request.public_key),
            outbox::format_timestamp(request.first_seen)
        );
    }
    Ok(())
}

pub(crate) fn peer_blocked() -> Result<()> {
    let blocked = requests::blocked(&paths().requests()).context("reading contact requests")?;
    if blocked.is_empty() {
        eprintln!("nobody blocked");
        return Ok(());
    }
    let names = displayname::cached_all(&paths().displaynames()).unwrap_or_default();
    for request in blocked {
        let name = names.get(&request.public_key).map(|dn| dn.name.as_str()).unwrap_or("-");
        println!("{name}\t{}", STANDARD.encode(request.public_key));
    }
    Ok(())
}

/// Publishes your status and picture: through the swarm running here, if
/// there is one, so peers connected now are told; otherwise straight to
/// the file, for them to ask for when they next connect.
pub(crate) fn set_profile(identity: PathBuf, status: &str, avatar: &[u8]) -> Result<()> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    if !swarm_core::profile::is_valid_status(status) {
        anyhow::bail!("a status has to be at most {} characters on one line", swarm_core::profile::MAX_STATUS_CHARS);
    }
    if let Ok(answer) = swarm_app::control::ask_blocking(&format!("SET_PROFILE\n{status}\n{}\n", STANDARD.encode(avatar))) {
        return match answer.trim_end().strip_prefix("ERROR ") {
            Some(why) => Err(anyhow::anyhow!("{why}")),
            None => Ok(()),
        };
    }
    let identity = load_identity(identity)?;
    swarm_core::profile::set(&paths().profile(), &identity, status, avatar)?;
    Ok(())
}

/// `swarm listen`: asks for the passphrase if there's someone at the
/// terminal; otherwise (started at boot, say) waits, locked, for
/// `swarm unlock` — nothing on the network until then.
pub(crate) fn run_listen(identity_path: PathBuf, peers_path: PathBuf, port: u16, cover_bits_per_second: Option<u64>) -> Result<()> {
    use std::io::IsTerminal;
    ensure_single_instance(port)?;
    let rt = tokio::runtime::Runtime::new()?;
    let identity = if io::stdin().is_terminal() {
        load_identity(identity_path)?
    } else {
        if !identity_path.exists() {
            anyhow::bail!("there's no identity at {} — run `swarm` (or `swarm keygen --out {}`) to make one", identity_path.display(), identity_path.display());
        }
        eprintln!("locked: waiting for `swarm unlock`");
        let identity = rt.block_on(swarm_app::control::wait_for_unlock(identity_path))?;
        eprintln!("unlocked");
        identity
    };
    let paths = paths().with_peers_file(expand_home(&peers_path));
    rt.block_on(async move {
        let config = NodeConfig { paths, port, cover_bits_per_second, log: swarm_app::time::stamped(Arc::new(|line| eprintln!("{line}"))) };
        let (node, call_events) = node::start(Arc::new(identity), config).await?;
        tokio::spawn(swarm_app::devices::keep_addresses(node.clone()));
        tokio::spawn(swarm_app::discovery::run(node.clone()));
        tokio::spawn(swarm_app::autoanswer::answer_calls(node.clone(), expand_home(&peers_path), call_events));
        tokio::select! {
            _ = swarm_app::control::serve(node.clone()) => {}
            _ = tokio::signal::ctrl_c() => {}
            _ = stopped() => {}
        }
        swarm_app::discovery::goodbye();
        node.shutdown().await;
        Ok(())
    })
}

/// `peer_name`'s record in the peers file at `peers_path`.
/// A peer as a command names them: by their key (base64) — their
/// identity, which never changes, so what a script should use — or by the
/// name on your list, which is only a label (and can follow the name they
/// go by). The name on the list, for the rest of the command.
pub(crate) fn named(peers_path: &std::path::Path, given: String) -> String {
    let Ok(key) = peerbook::decode_key(&given) else { return given };
    peers::load(&expand_home(peers_path)).ok().and_then(|all| all.into_iter().find(|p| p.public_key == key)).map(|p| p.name).unwrap_or(given)
}

pub(crate) fn find_peer(peers_path: &Path, peer_name: &str) -> Result<Peer> {
    let peers_path = expand_home(peers_path);
    let all = peers::load(&peers_path).with_context(|| format!("loading {}", peers_path.display()))?;
    match peers::find(&all, peer_name) {
        Ok(peer) => Ok(peer.clone()),
        Err(peers::Lookup::Nobody) => anyhow::bail!("no contact with the name or key '{peer_name}' in {}", peers_path.display()),
        Err(several) => anyhow::bail!("{several}"),
    }
}

/// Asked to stop some other way than Ctrl-C: `kill` (SIGTERM) on Unix;
/// the console window closing on Windows.
#[cfg(unix)]
pub(crate) async fn stopped() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut terminate) => {
            terminate.recv().await;
        }
        Err(_) => std::future::pending().await,
    }
}

#[cfg(windows)]
pub(crate) async fn stopped() {
    match tokio::signal::windows::ctrl_close() {
        Ok(mut close) => {
            close.recv().await;
        }
        Err(_) => std::future::pending().await,
    }
}

pub(crate) fn peer_set_receive(name: String, folder: String, peers_path: PathBuf) -> Result<()> {
    let peer = find_peer(&peers_path, &name)?;
    if folder == "off" {
        swarm_core::settings::set_receive_to(&paths().settings(), &peer.public_key, None)?;
        println!("files from {name} wait for an answer, as usual");
        return Ok(());
    }
    let folder = expand_home(std::path::Path::new(&folder));
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    let folder = folder.canonicalize()?;
    swarm_core::settings::set_receive_to(&paths().settings(), &peer.public_key, Some(&folder))?;
    println!("files from {name} are saved to {} without asking", folder.display());
    Ok(())
}

pub(crate) fn peer_set_control(name: String, on: bool, peers_path: PathBuf) -> Result<()> {
    let peer = find_peer(&peers_path, &name)?;
    swarm_core::settings::set_auto_control(&paths().settings(), &peer.public_key, on)?;
    if on {
        println!("{name} may control your shared screen without asking");
    } else {
        println!("{name} must ask each time to control your screen");
    }
    Ok(())
}

pub(crate) fn peer_set_answer(name: String, mode: String, peers_path: PathBuf) -> Result<()> {
    let peer = find_peer(&peers_path, &name)?;
    let answer = AutoAnswer::parse(&mode).ok_or_else(|| anyhow::anyhow!("'{mode}' isn't one of off, voice or video"))?;
    swarm_core::settings::set_auto_answer(&paths().settings(), &peer.public_key, answer)?;
    match answer {
        AutoAnswer::Off => println!("calls from {name} ring as usual"),
        AutoAnswer::Voice => println!("calls from {name} are answered automatically, voice only"),
        AutoAnswer::VoiceAndVideo => println!("calls from {name} are answered automatically, with video when they offer it"),
    }
    Ok(())
}

pub(crate) fn run_ping(identity: Identity, peer_name: String, peers_path: PathBuf) -> Result<()> {
    let peer = find_peer(&peers_path, &peer_name)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let rtt = match ask_node(&format!("PING\n{}\n", peer.handle())).await {
            Some(response) => match response.trim_end().strip_prefix("OK ") {
                Some(micros) => std::time::Duration::from_micros(micros.parse().context("reading the node's answer")?),
                None => anyhow::bail!("{}", response.trim_end().trim_start_matches("ERROR ")),
            },
            None => {
                eprintln!("connecting to {peer_name} at {}...", peer.address);
                node::oneshot::ping(keys_of(&identity, &device_of()?), &peer).await?
            }
        };
        println!("{peer_name} is online — rtt {:.1}ms", rtt.as_secs_f64() * 1000.0);
        Ok(())
    })
}

pub(crate) fn run_posts(identity: Identity, peer_name: String, peers_path: PathBuf) -> Result<()> {
    let peer = find_peer(&peers_path, &peer_name)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let result = match ask_node(&format!("LIST_POSTS\n{}\n", peer.handle())).await {
            Some(response) => parse_posts_response(&response)?,
            None => node::oneshot::fetch_posts(keys_of(&identity, &device_of()?), &paths(), &peer, &|line| eprintln!("{line}")).await?,
        };
        print_posts(&peer_name, result);
        Ok(())
    })
}

/// The node's answer to `LIST_POSTS` (see `handle_control_connection`).
pub(crate) fn parse_posts_response(response: &str) -> Result<PostsResult> {
    let mut lines = response.lines();
    let head = lines.next().unwrap_or_default();
    let posts: Vec<posts::Post> = lines
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let created_at = parts.next()?.parse().ok()?;
            let visibility = Visibility::parse(parts.next()?);
            let signature: [u8; 64] = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
            let text = parts.next()?.to_string();
            Some(posts::Post { created_at, visibility, signature, text })
        })
        .collect();
    if head == "OK live" {
        Ok(PostsResult::Live(posts))
    } else if let Some(reason) = head.strip_prefix("OK cached ") {
        Ok(PostsResult::Cached { posts, reason: reason.to_string() })
    } else {
        Err(anyhow::anyhow!("{}", head.trim_start_matches("ERROR ")))
    }
}

/// A one-shot, standalone call — dial, ring, and if answered, stream
/// real audio (and with `video`, the camera) until Ctrl+C. Only the
/// *caller* gets to be this simple: whoever's being called needs a TUI
/// watching for the ring.
pub(crate) fn run_call(identity: Identity, peer_name: String, peers_path: PathBuf, video: bool) -> Result<()> {
    let peer = find_peer(&peers_path, &peer_name)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        if swarm_app::control::node_running().await {
            anyhow::bail!("swarm is running on this device — call {peer_name} from it");
        }
        eprintln!("connecting to {peer_name} at {}...", peer.address);
        let mut link = node::oneshot::place_call(keys_of(&identity, &device_of()?), &paths(), &peer).await?;
        eprintln!("ringing — waiting up to {}s for an answer...", swarm_core::call::RING_TIMEOUT.as_secs());
        let video = if video {
            let mut call_video = swarm_app::calls::CallVideo::start(true);
            if let Some(note) = &call_video.camera_note {
                anyhow::bail!("opening the camera: {note}");
            }
            Some((call_video.link(peer.public_key), call_video))
        } else {
            None
        };
        let Some(with_video) = swarm_core::call::ring(&mut link, video.is_some()).await.context("ringing")? else {
            println!("{peer_name} didn't answer");
            return Ok(());
        };
        if video.is_some() && !with_video {
            eprintln!("{peer_name} took the call with voice only");
        }
        let video = video.filter(|_| with_video);

        let (mic_tx, mic_rx) = mpsc::unbounded_channel();
        let (speaker_tx, speaker_rx) = mpsc::unbounded_channel();
        let audio_handle = tokio::task::block_in_place(|| audio::start(mic_tx, speaker_rx)).context("opening microphone/speaker")?;
        match audio_handle.one_way_note() {
            Some(note) => eprintln!("{peer_name} answered — call live, {note}, Ctrl+C to hang up"),
            None => eprintln!("{peer_name} answered — call live, Ctrl+C to hang up"),
        }

        let (video_io, mut video, report) = match video {
            Some((io, call_video)) => {
                let report = tokio::spawn(swarm_app::autoanswer::report_video(peer_name.clone(), io.stats.clone(), Some(call_video.remotes[0].picture.clone())));
                (Some(io), Some(call_video), Some(report))
            }
            None => (None, None, None),
        };
        let (hangup_tx, hangup_rx) = oneshot::channel();
        let mut call_task = tokio::spawn(swarm_core::call::run(link, mic_rx, speaker_tx, hangup_rx, video_io));
        let hung_up = swarm_app::autoanswer::run_fitting_camera(
            async {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => true,
                    _ = &mut call_task => false,
                }
            },
            video.as_mut(),
        )
        .await;
        if hung_up {
            let _ = hangup_tx.send(());
            let _ = call_task.await;
        }
        if let Some(report) = report {
            report.abort();
        }
        if let Some(mut video) = video {
            video.camera_off();
        }
        tokio::task::block_in_place(|| audio_handle.stop());
        println!("call with {peer_name} ended");
        Ok(())
    })
}

/// The groups this user is in, each with what it's called here.
fn my_groups(me: &[u8; 32]) -> Result<Vec<(swarm_core::channel::ChannelId, String, swarm_core::channel::Roster)>> {
    use swarm_core::channel;
    let dir = paths().channels();
    let names: std::collections::HashMap<[u8; 32], String> = swarm_core::peers::load(&paths().peers()).unwrap_or_default().into_iter().map(|p| (p.public_key, p.name)).collect();
    let mut groups = Vec::new();
    for id in channel::known_channels(&dir).context("reading the groups kept here")? {
        let roster = channel::replay(&id, &channel::load_roster(&dir, &id).unwrap_or_default());
        if roster.is_member(me) {
            let name = swarm_app::names::group_name(&names, *me, &id, &roster);
            groups.push((id, name, roster));
        }
    }
    groups.sort_by_key(|a| a.1.to_lowercase());
    Ok(groups)
}

/// The group `given` names: its name (ignoring case), or the start of
/// its tag, as `swarm group list` shows it.
fn find_group(me: &[u8; 32], given: &str) -> Result<swarm_core::channel::ChannelId> {
    let groups = my_groups(me)?;
    let wanted = given.trim().to_lowercase();
    if let Some((id, _, _)) = groups.iter().find(|(_, name, _)| name.to_lowercase() == wanted) {
        return Ok(id.clone());
    }
    let by_tag: Vec<&swarm_core::channel::ChannelId> = groups.iter().map(|(id, _, _)| id).filter(|id| wanted.len() >= 4 && id.name.starts_with(&wanted)).collect();
    match by_tag.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(anyhow::anyhow!("you're in no group called {given} — `swarm group list` shows them")),
        _ => Err(anyhow::anyhow!("{given} could be several groups — give more of its tag")),
    }
}

/// Asks the running swarm `request`, saying `done` when it's done; its
/// answer's first line.
fn ask_about_group(request: String, done: &str) -> Result<String> {
    let rt = tokio::runtime::Runtime::new()?;
    let response = rt.block_on(ask_node(&request)).ok_or_else(|| anyhow::anyhow!("no swarm is running on this device — start the TUI (`swarm`) or `swarm listen` first"))?;
    let line = response.lines().next().unwrap_or_default().to_string();
    match line.strip_prefix("ERROR ") {
        Some(e) => Err(anyhow::anyhow!("{e}")),
        None => {
            println!("{done}");
            Ok(line)
        }
    }
}

pub(crate) fn run_group_new(contacts: Vec<String>) -> Result<()> {
    let list = contacts.iter().map(|c| named(&paths().peers(), c.clone())).collect::<Vec<_>>().join(",");
    let line = ask_about_group(format!("GROUP_NEW\n{list}\n"), "group started")?;
    if let Some(tag) = line.strip_prefix("OK ").and_then(|rest| rest.split(' ').next()) {
        println!("its tag: {tag}");
    }
    Ok(())
}

/// Something done to group `given` by the running swarm: `what`, and its
/// argument, as the control socket takes them.
pub(crate) fn run_group_ask(identity: Identity, given: String, what: &str, arg: Option<String>, done: &str) -> Result<()> {
    let id = find_group(&identity.public_key_bytes(), &given)?;
    let owner = STANDARD.encode(id.owner);
    let request = match arg {
        Some(arg) => format!("{what}\n{}\n{owner}\n{}\n", id.name, swarm_core::text::single_line(&arg)),
        None => format!("{what}\n{}\n{owner}\n", id.name),
    };
    ask_about_group(request, done).map(|_| ())
}

pub(crate) fn run_group_post(identity: Identity, given: String, text: String) -> Result<()> {
    let me = identity.public_key_bytes();
    let id = find_group(&me, &given)?;
    let rt = tokio::runtime::Runtime::new()?;
    let one_line = swarm_core::text::single_line(&text);
    if let Some(response) = rt.block_on(ask_node(&format!("GROUP_POST\n{}\n{}\n{one_line}\n", id.name, STANDARD.encode(id.owner)))) {
        return match response.lines().next().unwrap_or_default().strip_prefix("ERROR ") {
            Some(e) => Err(anyhow::anyhow!("{e}")),
            None => {
                println!("sent");
                Ok(())
            }
        };
    }
    let name = displayname::current(&paths().displayname()).ok().flatten().map(|d| d.name);
    let post = swarm_core::channel::sign_channel_post(&id, &identity, &text, name.as_deref());
    swarm_core::channel::merge_posts(&paths().channels(), &id, &me, &[post]).context("saving the message")?;
    println!("saved — no swarm is running here, so it goes out once one is");
    Ok(())
}

pub(crate) fn run_group_show(identity: Identity, given: String) -> Result<()> {
    let me = identity.public_key_bytes();
    let id = find_group(&me, &given)?;
    let (_, name, roster) = my_groups(&me)?.into_iter().find(|(g, _, _)| *g == id).expect("found above");
    let peers = swarm_core::peers::load(&paths().peers()).unwrap_or_default().into_iter().map(|p| (p.public_key, p.name)).collect();
    let names = swarm_app::names::ChannelNames::for_channel(peers, me, &id);
    println!("{name} (tag {})", id.name);
    println!("\nin it:");
    let mut members: Vec<_> = roster.members.iter().collect();
    members.sort_by_key(|(_, m)| m.joined_at);
    for (key, member) in members {
        println!("  {} — added {}", names.current(key), outbox::format_timestamp(member.joined_at));
    }
    let mut posts = swarm_core::channel::load_posts(&paths().channels(), &id).context("reading what's been said")?;
    println!("\nmessages:");
    if posts.is_empty() {
        println!("  (none yet)");
    }
    posts.sort_by_key(|p| p.created_at);
    for post in posts {
        println!("  [{}] {}: {}", outbox::format_timestamp(post.created_at), names.said_as(&post), post.text);
    }
    Ok(())
}

pub(crate) fn run_group_list(identity: Identity) -> Result<()> {
    let groups = my_groups(&identity.public_key_bytes())?;
    if groups.is_empty() {
        println!("you're in no groups — `swarm group new <contact>...` starts one");
    }
    for (id, name, roster) in groups {
        println!("{name}  ({} in it, tag {})", roster.members.len(), &id.name[..8.min(id.name.len())]);
    }
    Ok(())
}

/// Asks the running node, which a transfer needs — it has to keep going
/// for as long as the transfer takes, across reconnects.
pub(crate) fn ask_node_blocking(request: &str) -> Result<String> {
    let rt = tokio::runtime::Runtime::new()?;
    let response = rt.block_on(ask_node(request)).ok_or_else(|| {
        anyhow::anyhow!("no swarm is running on this device — start the TUI (`swarm`) or `swarm listen` first; it has to keep running until the transfer's done")
    })?;
    let line = response.lines().next().unwrap_or_default().to_string();
    match line.strip_prefix("ERROR ") {
        Some(e) => Err(anyhow::anyhow!("{e}")),
        None => Ok(line),
    }
}

/// Asks the running swarm, for something only it can do.
pub(crate) fn ask_running_node(request: &str) -> Result<String> {
    let rt = tokio::runtime::Runtime::new()?;
    let response = rt
        .block_on(ask_node(request))
        .ok_or_else(|| anyhow::anyhow!("no swarm is running on this device — start the app (`swarm`) or `swarm listen` first"))?;
    match response.strip_prefix("ERROR ") {
        Some(e) => Err(anyhow::anyhow!("{}", e.trim_end())),
        None => Ok(response),
    }
}

pub(crate) fn run_device(action: DeviceCommand) -> Result<()> {
    let ask = |request: String| -> Result<()> {
        let answer = ask_running_node(&request)?;
        println!("{}", answer.trim_start_matches("OK ").trim_end());
        Ok(())
    };
    match action {
        DeviceCommand::List => {
            let answer = ask_running_node("DEVICES\n")?;
            for line in answer.lines().skip(1) {
                if let Some(("device", text)) = line.split_once('\t') {
                    println!("{text}");
                }
            }
            Ok(())
        }
        DeviceCommand::Add => {
            let answer = ask_running_node("DEVICE_ADD\n")?;
            let code = answer.trim_start_matches("OK ").trim_end();
            println!("{code}");
            let minutes = swarm_core::node::joining::CODE_LIFETIME.as_secs() / 60;
            eprintln!("On the new device: swarm keygen --out ~/.swarm/identity --join '{code}'\nThe code works once, for {minutes} minutes, while Swarm runs here.");
            Ok(())
        }
        DeviceCommand::Remove { device } => ask(format!("DEVICE_REMOVE\n{device}\n")),
        DeviceCommand::Relay { device, on } => ask(format!("DEVICE_RELAY\n{device}\n{on}\n")),
        DeviceCommand::Address { device, addresses } => ask(format!("DEVICE_ADDRESS\n{device}\n{addresses}\n")),
        DeviceCommand::Answer { device, mode } => ask(format!("DEVICE_ANSWER\n{device}\n{mode}\n")),
        DeviceCommand::Control { device, on } => ask(format!("DEVICE_CONTROL\n{device}\n{on}\n")),
        DeviceCommand::Rename { name } => ask(format!("RENAME_DEVICE\n{name}\n")),
    }
}

pub(crate) fn run_send_file(peer: String, path: PathBuf) -> Result<()> {
    let path = std::fs::canonicalize(&path).with_context(|| format!("finding {}", path.display()))?;
    let answer = ask_node_blocking(&format!("OFFER_FILE\n{peer}\n{}\n", path.display()))?;
    let id = answer.trim_start_matches("OK ").to_string();
    println!("offered {} to {peer} (transfer {}) — it goes once they accept; `swarm transfers` shows how it's going", path.display(), &id[..id.len().min(8)]);
    Ok(())
}

/// A transfer id from what was typed: the whole id, or enough of its
/// start to be the only match.
pub(crate) fn find_transfer(typed: &str) -> Result<swarm_core::transfer::Transfer> {
    let typed = typed.to_ascii_lowercase();
    let matches: Vec<_> = swarm_core::transfer::load(&paths().transfers())
        .into_iter()
        .filter(|t| !t.cleared && swarm_core::transfer::format_id(&t.id).starts_with(&typed))
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("one")),
        0 => Err(anyhow::anyhow!("no transfer starting '{typed}' — see `swarm transfers`")),
        _ => Err(anyhow::anyhow!("more than one transfer starts '{typed}' — type more of it")),
    }
}

pub(crate) fn run_transfers() -> Result<()> {
    use swarm_core::transfer::{Direction, State};
    let names: std::collections::HashMap<[u8; 32], String> =
        peers::load(&paths().peers()).unwrap_or_default().into_iter().map(|p| (p.public_key, p.name)).collect();
    let mut all: Vec<_> = swarm_core::transfer::load(&paths().transfers()).into_iter().filter(|t| !t.cleared).collect();
    if all.is_empty() {
        println!("no file transfers yet — `swarm send-file <contact> <path>`");
        return Ok(());
    }
    all.reverse();
    for t in all {
        let user = swarm_core::node::user_of_device(&paths(), &t.peer).unwrap_or(t.peer);
        let who = names.get(&user).or_else(|| names.get(&t.peer)).cloned().unwrap_or_else(|| "?".to_string());
        let (arrow, way) = if t.direction == Direction::Out { ("→", "to") } else { ("←", "from") };
        let state = match (t.direction, t.state) {
            (Direction::In, State::Offered) => "offered — `swarm transfer accept <id> --to <folder>` or `decline <id>`".to_string(),
            (Direction::Out, State::Offered) => "waiting for them to accept".to_string(),
            (Direction::In, State::Active) => format!("{}%", swarm_core::transfer::received(&t) * 100 / t.size.max(1)),
            (Direction::Out, State::Active) => "sending".to_string(),
            (Direction::In, State::Done) => format!("saved to {}", t.path.display()),
            (Direction::Out, State::Done) => "delivered".to_string(),
            (_, State::Declined) => "declined".to_string(),
            (_, State::Cancelled | State::Failed) => if t.note.is_empty() { "cancelled".to_string() } else { t.note.clone() },
        };
        let id = swarm_core::transfer::format_id(&t.id);
        println!("{} {arrow} {way} {who}: {} ({} bytes) — {state}", &id[..8], t.name, t.size);
    }
    Ok(())
}

pub(crate) fn run_transfer(action: TransferCommand) -> Result<()> {
    match action {
        TransferCommand::Accept { id, to } => {
            let t = find_transfer(&id)?;
            let to = expand_home(&to);
            ask_node_blocking(&format!("ACCEPT_FILE\n{}\n{}\n", swarm_core::transfer::format_id(&t.id), to.display()))?;
            println!("accepted {} — saving in {}", t.name, to.display());
        }
        TransferCommand::Decline { id } => {
            let t = find_transfer(&id)?;
            ask_node_blocking(&format!("DECLINE_FILE\n{}\n", swarm_core::transfer::format_id(&t.id)))?;
            println!("declined {}", t.name);
        }
        TransferCommand::Cancel { id } => {
            let t = find_transfer(&id)?;
            ask_node_blocking(&format!("CANCEL_FILE\n{}\n", swarm_core::transfer::format_id(&t.id)))?;
            println!("cancelled {}", t.name);
        }
        TransferCommand::Clear { id } => {
            let t = find_transfer(&id)?;
            if !t.state.is_finished() {
                anyhow::bail!("{} isn't finished — `swarm transfer cancel {id}` first", t.name);
            }
            ask_node_blocking(&format!("CLEAR_FILES\n{}\n", swarm_core::transfer::format_id(&t.id)))?;
            println!("cleared {}", t.name);
        }
        TransferCommand::ClearAll => {
            let answer = ask_node_blocking("CLEAR_FILES\nall\n")?;
            match answer.trim().trim_start_matches("OK ") {
                "0" => println!("nothing finished to clear"),
                "1" => println!("cleared 1 finished transfer"),
                n => println!("cleared {n} finished transfers"),
            }
        }
    }
    Ok(())
}

/// Newest first, since a feed reads more naturally that way. A cached
/// result gets a note up front saying these are the last ones fetched.
pub(crate) fn print_posts(peer_name: &str, result: PostsResult) {
    let (mut posts, stale_reason) = match result {
        PostsResult::Live(posts) => (posts, None),
        PostsResult::Cached { posts, reason } => (posts, Some(reason)),
    };
    if posts.is_empty() {
        println!("{peer_name} has no posts visible to you");
        return;
    }
    if let Some(reason) = stale_reason {
        println!("{peer_name} is unreachable right now ({reason}) — showing the last posts fetched from them:");
    }
    posts.reverse();
    for post in posts {
        let tag = match post.visibility {
            Visibility::Public => "public",
            Visibility::Private => "contacts only",
        };
        println!("[{}] ({tag}) {}", outbox::format_timestamp(post.created_at), post.text);
    }
}

pub(crate) fn run_send(identity: Identity, peer_name: String, message: String, peers_path: PathBuf) -> Result<()> {
    let peer = find_peer(&peers_path, &peer_name)?;
    let rt = tokio::runtime::Runtime::new()?;
    let delivery = rt.block_on(async {
        match ask_node(&format!("SEND\n{}\n{}\n", peer.handle(), swarm_core::text::single_line(&message))).await {
            Some(response) => match response.trim_end() {
                "OK delivered" => Ok(Delivery::Delivered),
                "OK queued" => Ok(Delivery::Queued),
                "OK held" => Ok(Delivery::HeldUntilAdded),
                "OK relayed" => Ok(Delivery::Relayed),
                other => Err(anyhow::anyhow!("{}", other.trim_start_matches("ERROR "))),
            },
            None => node::oneshot::send_message(keys_of(&identity, &device_of()?), &paths(), &peer, &message).await,
        }
    })?;
    match delivery {
        Delivery::Delivered => println!("delivered to {peer_name}"),
        Delivery::Relayed => println!("on its way to {peer_name} through another of your devices"),
        Delivery::Queued => println!("{peer_name} isn't reachable right now — message kept, and it goes as soon as they are"),
        Delivery::HeldUntilAdded => println!(
            "{peer_name} {NOT_ADDED} — message held here (not sent), will be delivered once they add you; \
             they've been sent a contact request"
        ),
    }
    Ok(())
}
