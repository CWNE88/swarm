//! The `swarm` program: with no command, the window (or the terminal UI,
//! `tui`); with one, that command (see `swarm --help`). The commands'
//! arguments are here; what they do is in `commands`, and making and
//! unlocking your identity in `identity`.

use swarm_core::lock::Locked;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use clap::{Parser, Subcommand};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use swarm_core::displayname;
use swarm_core::identity::Identity;
use swarm_core::node::{self, parse_bitrate, Delivery, NodeConfig, PostsResult, NOT_ADDED};
use swarm_core::outbox;
use swarm_core::peers::{self, Peer};
use swarm_core::posts::{self, Visibility};
use swarm_core::requests;
use swarm_core::settings::AutoAnswer;
use swarm_core::transport::DEFAULT_PORT;
use tokio::sync::{mpsc, oneshot};
use swarm_app::control::{ask_node, ensure_single_instance};
use swarm_app::{audio, expand_home, paths, peerbook};

mod browser;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod glibc;
mod commands;
mod identity;
mod lineedit;
mod look;
mod picture;
mod tui;

use commands::*;
use identity::*;

#[derive(Parser)]
#[command(
    name = "swarm",
    version = swarm_app::VERSION,
    about = "Private messages, calls and files between people, with no server",
    after_help = "\
With no command, swarm opens its window (or, with --tui or no screen,
the terminal UI). Everything below can be done there too; `swarm manual`
prints the whole manual.

EXAMPLES:
    Make your identity, once on each device (or open the window and
    follow it):
        swarm keygen --out ~/.swarm/identity

    Your key, to give to the people who'll add you:
        swarm pubkey

    Add a contact, with the address and key they gave you:
        swarm contact add bob 2001:db8::5 3ZxK9f...==

    Run with no window, keeping in touch with every contact:
        swarm listen
    ...with cover traffic, so nobody watching can tell when you use it:
        swarm listen --cover 64k

    Check whether a contact is online, or send them a message:
        swarm ping bob
        swarm send bob \"are you around?\"

Everything is kept under ~/.swarm, locked with your passphrase. Every
command reads the passphrase from its standard input when it isn't
typed. swarm listens on port 1977 unless `listen --port` says otherwise;
a contact on another port gives their address as `[address]:port`."
)]
struct Cli {
    /// Omit to open Swarm's window (or, with --tui or no screen to open a
    /// window on, the terminal UI)
    #[command(subcommand)]
    command: Option<Command>,
    /// Same as `listen --cover`, for the window or the terminal UI
    #[arg(long, value_parser = parse_bitrate)]
    cover: Option<u64>,
    /// The terminal UI instead of the window
    #[arg(long)]
    tui: bool,
    /// Draw one screen of the window into this image file (PPM) instead of
    /// opening it — for checking the layout with no screen
    #[arg(long, hide = true)]
    snapshot: Option<String>,
    #[arg(long, hide = true)]
    size: Option<String>,
    #[arg(long, hide = true)]
    wait: Option<String>,
    #[arg(long, hide = true)]
    select: Option<String>,
    #[arg(long, hide = true)]
    view: Option<String>,
    #[arg(long, hide = true)]
    keys: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Make a new identity, shown as its 12 recovery words
    Keygen {
        /// Save it to this file (created with 0600 permissions) instead of
        /// only showing it
        #[arg(long)]
        out: Option<PathBuf>,
        /// Bring an identity back from its 12 recovery words instead of
        /// making a new one
        #[arg(long)]
        restore: bool,
        /// Add this device to your user with a code from one of your
        /// devices (`swarm device add` there), instead of making a new
        /// user. Needs --out
        #[arg(long, conflicts_with = "restore")]
        join: Option<String>,
    },
    /// Show your identity's 12 recovery words
    Words {
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// Unlock a `swarm listen` that's waiting for its passphrase (one
    /// started with nobody at its keyboard, at boot say)
    Unlock,
    /// Change your passphrase; with --forgotten, set a new one from your
    /// 12 recovery words instead of the old one
    Passphrase {
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
        #[arg(long)]
        forgotten: bool,
    },
    /// Print your public key (your Swarm ID)
    Pubkey {
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// Set the name your contacts see beside your key. Only a label: your
    /// key is what proves who you are, so it's always shown with it. Run
    /// with no name to print your current one.
    Name {
        name: Option<String>,
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// Your status: a few words beside your name, for your contacts only.
    /// Run with no text to print it.
    Status {
        text: Option<String>,
        /// Clear it.
        #[arg(long, conflicts_with = "text")]
        clear: bool,
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// Your manually set IP address: where you can be reached that no
    /// device of yours can see for itself (your router's public address
    /// with a port forwarded to one of them, or a host name for it;
    /// `ip:port` if it isn't 1977; several separated by commas). It's
    /// first in your code, and on all your devices. Run with no address
    /// to print it.
    Address {
        address: Option<String>,
        /// Clear it.
        #[arg(long, conflicts_with = "address")]
        clear: bool,
    },
    /// Whether anyone on the network one of your devices is on can find
    /// you under Add a contact (your name, key and address there): `on`
    /// or `off`, on all your devices. Run with neither to print it.
    Discoverable {
        #[arg(value_parser = ["on", "off"])]
        on: Option<String>,
    },
    /// Whether this device receives connections only: it dials no one,
    /// and your other devices and contacts connect to it. `on` or `off`,
    /// for this device alone; a running Swarm keeps to it within ten
    /// seconds. Run with neither to print it.
    ReceiveOnly {
        #[arg(value_parser = ["on", "off"])]
        on: Option<String>,
    },
    /// The most this device sends, all told: `2m` (2 Mbit/s), `500k`, or
    /// `none`. A ceiling, not a rate: below it, everything goes as it
    /// would. For this device alone; a running Swarm keeps to it within
    /// ten seconds. Run with neither to print it.
    BandwidthLimit {
        limit: Option<String>,
    },
    /// The most this device wants sent to it: `1m`, `500k`, or `none`.
    /// Every contact is asked to keep under it: their call video at once,
    /// files asked for no faster, their cover from their next connection.
    /// For this device alone; a running Swarm keeps to it within ten
    /// seconds. Run with neither to print it.
    ReceiveLimit {
        limit: Option<String>,
    },
    /// Your picture, for your contacts only: a JPEG or PNG, its middle
    /// square kept, up to 640 across. Run with no file to say whether
    /// there's one.
    Picture {
        file: Option<PathBuf>,
        /// Remove it.
        #[arg(long, conflicts_with = "file")]
        remove: bool,
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// Add, change and list your contacts
    Contact {
        #[command(subcommand)]
        action: ContactCommand,
    },
    /// Run swarm with no window: listen for connections (on port 1977
    /// unless --port says otherwise), keep in touch with every contact,
    /// and print what happens. For a device left running on its own.
    Listen {
        /// Your identity file (see `keygen --out`)
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
        #[arg(long, default_value = "~/.swarm/peers")]
        contacts: PathBuf,
        /// Listen on this port instead of 1977. A port below 1024 (443,
        /// say, to look like ordinary web traffic) needs running as root,
        /// or allowing once with
        /// `sudo setcap cap_net_bind_service=+ep <path to swarm>`.
        /// Contacts then need the port with your address: `[address]:port`.
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Cover traffic at this rate (`64k`, `1m`, or bits a second):
        /// every connection sends the same amount at a steady beat
        /// whether anything real is going or not, so nobody watching can
        /// tell when you're using it. Off unless given; fixed until
        /// restarted.
        #[arg(long, value_parser = parse_bitrate)]
        cover: Option<u64>,
    },
    /// Check whether a contact is online
    Ping {
        /// A contact: their name, or their key
        contact: String,
        #[arg(long, default_value = "~/.swarm/peers")]
        contacts: PathBuf,
        /// Your identity file (see `keygen --out`)
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// Send a contact a message, and wait until it's delivered
    Send {
        /// A contact: their name, or their key
        contact: String,
        /// The message: in quotes if it's more than one word
        message: String,
        #[arg(long, default_value = "~/.swarm/peers")]
        contacts: PathBuf,
        /// Your identity file (see `keygen --out`)
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// Write a post. Posts are fetched by whoever wants to read them,
    /// from a swarm running here. Only your contacts see it, unless
    /// --public.
    Post {
        /// The post: in quotes if it's more than one word
        text: String,
        /// Let anyone who connects see it, not only your contacts
        #[arg(long)]
        public: bool,
        #[arg(long, default_value = "~/.swarm/posts")]
        out: PathBuf,
        /// Your identity file (see `keygen --out`)
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// Fetch and print a contact's posts: their public ones, and the rest
    /// too if they've added you. They need swarm running.
    Posts {
        /// A contact: their name, or their key
        contact: String,
        #[arg(long, default_value = "~/.swarm/peers")]
        contacts: PathBuf,
        /// Your identity file (see `keygen --out`)
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// Call a contact and wait for them to answer (it rings for 30
    /// seconds). Needs a microphone and speaker here, and someone at
    /// their window or terminal UI to answer.
    Call {
        /// A contact: their name, or their key
        contact: String,
        /// A video call: the camera's pictures go to them (`SWARM_CAMERA`
        /// picks one, `test` for a test pattern). Theirs aren't shown;
        /// how both ways are going is printed every few seconds instead.
        #[arg(long)]
        video: bool,
        #[arg(long, default_value = "~/.swarm/peers")]
        contacts: PathBuf,
        /// Your identity file (see `keygen --out`)
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// Groups: chats of several people, with no server. A group is named
    /// by what it's called, or by the start of its tag (`swarm group
    /// list` shows both). Most need swarm running here.
    Group {
        #[command(subcommand)]
        action: GroupCommand,
    },
    /// Offer a contact a file. It goes once they accept it, through the
    /// swarm running here (window, terminal UI or `listen`), which has to
    /// keep running until it's done.
    SendFile {
        /// A contact: their name, or their key
        contact: String,
        /// The file to send
        path: PathBuf,
    },
    /// List file transfers, both ways, newest first
    Transfers,
    /// Your devices: to your contacts every one of them is you, and a
    /// call rings them all. Needs swarm running here.
    Device {
        #[command(subcommand)]
        action: DeviceCommand,
    },
    /// Answer or stop a file transfer (ids as `swarm transfers` shows
    /// them; the first few characters are enough)
    Transfer {
        #[command(subcommand)]
        action: TransferCommand,
    },
    /// Check video on this computer's graphics chip: whether it encodes
    /// and decodes for calls, how quickly against software, whether its
    /// pictures come out right in other swarms, and what the camera
    /// gives. Takes about half a minute; changes nothing.
    VideoCheck,
    /// Check sharing the screen: takes its pictures for five seconds and
    /// says how many came and how many couldn't be taken (move the
    /// pointer meanwhile: a picture's only new when something's changed).
    /// Sends nothing.
    ScreenCheck,
    /// Print Swarm's manual, as the Help tab has it, to read with a pager
    /// (`swarm manual | less`) or keep as a file. Needs no passphrase.
    Manual {
        /// Lines this wide (otherwise the terminal's, or 80)
        #[arg(long)]
        width: Option<usize>,
    },
}

#[derive(Subcommand)]
enum DeviceCommand {
    /// Your devices, this one among them
    List,
    /// A code for adding a device to your user, good once for ten
    /// minutes: on the new device, `swarm keygen --out <file> --join <code>`
    Add,
    /// Take one of your devices out (by name, or the start of its key).
    /// Not this one: another of yours does.
    Remove { device: String },
    /// Let one of your devices relay for the others (`on`): a contact who
    /// can't reach one of them directly goes through it. Or stop it
    /// (`off`). By name, or the start of its key.
    Relay {
        device: String,
        #[arg(value_parser = ["on", "off"])]
        on: String,
    },
    /// Let another of your devices control this one's shared screen as
    /// soon as it asks (`on`), or ask each time (`off`), as `swarm
    /// contact control` does for a contact. By name, or the start of its
    /// key.
    Control {
        device: String,
        #[arg(value_parser = ["on", "off"])]
        on: String,
    },
    /// Have calls from another of your devices answered here without
    /// asking (`voice`, or `video` with video too), or ring as usual
    /// (`off`), as `swarm contact answer` does for a contact. By name, or
    /// the start of its key.
    Answer {
        device: String,
        #[arg(value_parser = ["off", "voice", "video"])]
        mode: String,
    },
    /// Set where this device finds another of yours, when it can't have
    /// heard (both moved to a new network, say), or a host name to look up
    /// at each try, and try it now. By name, or the start of its key; IPs
    /// or host names, each with a port if it isn't 1977, separated by
    /// commas, in place of what was set before ("" clears it).
    Address { device: String, addresses: String },
    /// Rename this device, among your devices
    Rename { name: String },
}

#[derive(Subcommand)]
enum TransferCommand {
    /// Accept a file offered to you, to be saved in `to`
    Accept {
        id: String,
        #[arg(long)]
        to: PathBuf,
    },
    /// Decline a file offered to you
    Decline { id: String },
    /// Stop a transfer either way (a part-received file is deleted)
    Cancel { id: String },
    /// Clear a finished transfer from the list (a file received stays
    /// where it was saved)
    Clear { id: String },
    /// Clear every finished transfer from the list
    ClearAll,
}

#[derive(Subcommand)]
enum GroupCommand {
    /// Start a group of you and these contacts (by your names for them).
    New { contacts: Vec<String> },
    /// Add a contact to a group you're in: they see what's said from now
    /// on.
    Add { group: String, contact: String },
    /// Write in a group. It goes out at once through the swarm running
    /// here, if there is one; otherwise when one runs.
    Post {
        group: String,
        text: String,
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// Give a group a name (none: it goes by its members' names).
    Name { group: String, name: Option<String> },
    /// Leave a group: it goes from your devices, and the others see you
    /// left.
    Leave { group: String },
    /// Catch up on a group with everyone in it who's connected.
    Sync { group: String },
    /// Print a group's members and messages, as kept here.
    Show {
        group: String,
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
    /// List the groups you're in.
    List {
        #[arg(long, default_value = "~/.swarm/identity")]
        identity: PathBuf,
    },
}

#[derive(Subcommand)]
enum ContactCommand {
    /// Add a contact: a name for them, their address (or several, IPv6
    /// or IPv4, separated by commas) and their key
    Add {
        name: String,
        address: String,
        key: String,
        #[arg(long, default_value = "~/.swarm/peers")]
        out: PathBuf,
    },
    /// Change a contact's address and key (`add` for a new one)
    Update {
        name: String,
        address: String,
        key: String,
        /// Rename them too. Your chat stays with them: it goes by their
        /// key, not their name.
        #[arg(long)]
        rename: Option<String>,
        #[arg(long, default_value = "~/.swarm/peers")]
        out: PathBuf,
    },
    /// Delete a contact, by name, and everything kept about them, on all
    /// your devices. They can ask to be added again.
    Remove {
        name: String,
        /// Block them too: they can't ask again, and only their key is
        /// kept (see `contact blocked`)
        #[arg(long)]
        block: bool,
        #[arg(long, default_value = "~/.swarm/peers")]
        out: PathBuf,
    },
    /// Set a contact's own cover rate (see `swarm listen --cover`), or
    /// `off` to use the usual one. A swarm already running takes it at
    /// once.
    Cover {
        name: String,
        /// A rate, as for `listen --cover` (`64k`, say), or `off`
        rate: String,
        /// Ask it of them too: their side raises its own cover to match,
        /// so a call or message from them can't show even if they've
        /// left cover off.
        #[arg(long)]
        require: bool,
        #[arg(long, default_value = "~/.swarm/peers")]
        out: PathBuf,
    },
    /// Let this contact control your shared screen as soon as they ask,
    /// with no Allow to press (`on`), or ask each time (`off`): for
    /// someone you trust to help when you can't see the screen. Only
    /// while you're sharing it with them; Stop still works.
    Control {
        name: String,
        #[arg(value_parser = ["on", "off"])]
        on: String,
        #[arg(long, default_value = "~/.swarm/peers")]
        out: PathBuf,
    },
    /// Have calls from this contact answered without asking, for a device
    /// left listening somewhere, like a radio monitor. `voice`: voice
    /// calls (a video call is answered with voice only); `video`: video
    /// calls with video too; `off`: ring as usual.
    Answer {
        name: String,
        /// off, voice or video
        mode: String,
        #[arg(long, default_value = "~/.swarm/peers")]
        out: PathBuf,
    },
    /// Save files from this contact straight to a folder, without asking,
    /// for a device left running somewhere, a server say. `off`: ask
    /// each time.
    Receive {
        name: String,
        /// The folder, or off
        folder: String,
        #[arg(long, default_value = "~/.swarm/peers")]
        out: PathBuf,
    },
    /// List your contacts
    List {
        #[arg(long, default_value = "~/.swarm/peers")]
        path: PathBuf,
    },
    /// List contact requests: people who've added you, whom you haven't
    /// added. Their messages wait on their side until you add them
    /// (`contact add`, with the address and key shown here).
    Requests,
    /// List the keys you've blocked: told they're removed when they
    /// connect, never shown as a request.
    Blocked,
    /// Let someone blocked ask to be added again (their key, as `contact
    /// blocked` shows it). Adding them does this too.
    Unblock { key: String },
}

fn main() -> Result<()> {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).is_some_and(|a| a == swarm_app::filedialog::HELPER) {
        std::process::exit(swarm_app::filedialog::helper(&std::env::args_os().skip(2).collect::<Vec<_>>()));
    }
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).is_some_and(|a| a == swarm_app::portal_helper::HELPER) {
        std::process::exit(swarm_app::portal_helper::main());
    }
    swarm_core::secure::no_dumps();
    let cli = Cli::parse();
    let cover = cli.cover;
    let identity = PathBuf::from("~/.swarm/identity");
    if !keeps_nothing(&cli.command, &expand_home(&identity)) {
        load_identity(identity)?;
    }
    match cli.command {
        #[cfg(feature = "gui")]
        None if cli.snapshot.is_some() => {
            swarm_desktop::snapshot_to_file(cli.snapshot.as_deref().unwrap_or_default(), cli.size, cli.wait, cli.select, cli.view, cli.keys)
        }
        None if cli.tui || !can_open_window() => run_tui(cover),
        #[cfg(feature = "gui")]
        None => run_window(cover),
        #[cfg(not(feature = "gui"))]
        None => run_tui(cover),
        Some(Command::Keygen { out, restore, join }) => keygen(out, restore, join),
        Some(Command::Words { identity }) => {
            let identity = load_identity(identity)?;
            println!("{}\n\n{}", swarm_app::launch::words_for_terminal(&identity), swarm_app::launch::WORDS_ADVICE);
            Ok(())
        }
        Some(Command::Pubkey { identity }) => {
            let pub_path = PathBuf::from(format!("{}.pub", expand_home(&identity).display()));
            let key = fs::read_to_string(&pub_path).with_context(|| format!("reading {}", pub_path.display()))?;
            println!("{}", key.trim());
            Ok(())
        }
        Some(Command::Unlock) => run_unlock(),
        Some(Command::Passphrase { identity, forgotten }) => change_passphrase(expand_home(&identity), forgotten),
        Some(Command::Name { name, identity }) => {
            let identity = load_identity(identity)?;
            match name {
                Some(name) => {
                    displayname::set(&paths().displayname(), &identity, &name)?;
                    println!("display name set to {name}");
                }
                None => match displayname::current(&paths().displayname())? {
                    Some(claimed) => println!("{}", claimed.name),
                    None => println!("no display name set — run `swarm name <name>` to set one"),
                },
            }
            Ok(())
        }
        Some(Command::Address { address, clear }) => {
            let path = paths().manual_address();
            if address.is_none() && !clear {
                let kept = swarm_core::myaddress::load(&path);
                if kept.items.is_empty() {
                    println!("no manually set address — run `swarm address <address>` to set one");
                } else {
                    println!("{}", kept.text());
                }
                return Ok(());
            }
            let items = swarm_core::myaddress::check(&address.unwrap_or_default()).map_err(|e| anyhow::anyhow!(e))?;
            let kept = {
                let _guard = swarm_core::store::lock();
                swarm_core::myaddress::set(&path, items, swarm_core::unix_now())?
            };
            if kept.items.is_empty() {
                println!("manually set address cleared");
            } else {
                println!("manually set address: {} — first in your code, on all your devices", kept.text());
            }
            Ok(())
        }
        Some(Command::Discoverable { on }) => {
            let path = paths().discoverable();
            let Some(on) = on else {
                let kept = swarm_core::discoverable::load(&path);
                println!("{}", if kept.on { "on" } else { "off" });
                return Ok(());
            };
            let kept = {
                let _guard = swarm_core::store::lock();
                swarm_core::discoverable::set(&path, on == "on", swarm_core::unix_now())?
            };
            println!("{}", if kept.on { "discoverable on each network your devices are on" } else { "not discoverable on any network" });
            Ok(())
        }
        Some(Command::ReceiveOnly { on }) => {
            let path = paths().settings();
            let Some(on) = on else {
                println!("{}", if swarm_core::settings::receive_only(&path) { "on" } else { "off" });
                return Ok(());
            };
            swarm_core::settings::set_receive_only(&path, on == "on")?;
            println!("{}", if on == "on" { "this device receives connections only: it dials no one" } else { "this device dials again" });
            Ok(())
        }
        Some(Command::ReceiveLimit { limit }) => {
            let path = paths().settings();
            let Some(limit) = limit else {
                match swarm_core::settings::receive_limit(&path) {
                    Some(limit) => println!("{}bit/s", swarm_core::node::format_bitrate(limit)),
                    None => println!("none"),
                }
                return Ok(());
            };
            let limit = if limit.eq_ignore_ascii_case("none") || limit.eq_ignore_ascii_case("off") { None } else { Some(swarm_core::node::parse_bandwidth_limit(&limit).map_err(|e| anyhow::anyhow!(e))?) };
            swarm_core::settings::set_receive_limit(&path, limit)?;
            match limit {
                Some(limit) => println!("contacts are asked to send this device at most {}bit/s", swarm_core::node::format_bitrate(limit)),
                None => println!("no receive limit"),
            }
            Ok(())
        }
        Some(Command::BandwidthLimit { limit }) => {
            let path = paths().settings();
            let Some(limit) = limit else {
                match swarm_core::settings::bandwidth_limit(&path) {
                    Some(limit) => println!("{}bit/s", swarm_core::node::format_bitrate(limit)),
                    None => println!("none"),
                }
                return Ok(());
            };
            let limit = if limit.eq_ignore_ascii_case("none") || limit.eq_ignore_ascii_case("off") { None } else { Some(swarm_core::node::parse_bandwidth_limit(&limit).map_err(|e| anyhow::anyhow!(e))?) };
            swarm_core::settings::set_bandwidth_limit(&path, limit)?;
            match limit {
                Some(limit) => println!("this device sends at most {}bit/s", swarm_core::node::format_bitrate(limit)),
                None => println!("no bandwidth limit"),
            }
            Ok(())
        }
        Some(Command::Status { text, clear, identity }) => {
            let current = swarm_core::profile::current(&paths().profile());
            if text.is_none() && !clear {
                match current.map(|p| p.status).filter(|s| !s.is_empty()) {
                    Some(status) => println!("{status}"),
                    None => println!("no status set — run `swarm status <text>` to set one"),
                }
                return Ok(());
            }
            let status = text.unwrap_or_default();
            let avatar = current.map(|p| p.avatar).unwrap_or_default();
            set_profile(identity, status.trim(), &avatar)?;
            println!("{}", if status.trim().is_empty() { "status cleared" } else { "status set — your contacts see it" });
            Ok(())
        }
        Some(Command::Picture { file, remove, identity }) => {
            let current = swarm_core::profile::current(&paths().profile());
            if file.is_none() && !remove {
                match current.filter(|p| !p.avatar.is_empty()) {
                    Some(p) => println!("a picture is set ({} bytes) — your contacts see it", p.avatar.len()),
                    None => println!("no picture set — run `swarm picture <file>` to set one"),
                }
                return Ok(());
            }
            let avatar = match file {
                Some(file) => swarm_app::avatar::from_file(&expand_home(&file))?,
                None => Vec::new(),
            };
            set_profile(identity, &current.map(|p| p.status).unwrap_or_default(), &avatar)?;
            println!("{}", if avatar.is_empty() { "picture removed" } else { "picture set — your contacts see it" });
            Ok(())
        }
        Some(Command::Contact { action }) => match action {
            ContactCommand::Add { name, address, key, out } => peer_add(name, address, key, out),
            ContactCommand::Update { name, address, key, rename, out } => peer_update(named(&out, name), rename, address, key, out),
            ContactCommand::Remove { name, block, out } => peer_remove(named(&out, name), block, out),
            ContactCommand::Cover { name, rate, require, out } => {
                let name = named(&out, name);
                let cover = if rate.eq_ignore_ascii_case("off") { None } else { Some(parse_bitrate(&rate).map_err(|e| anyhow::anyhow!(e))?) };
                peer_set_cover(name, cover, require, out)
            }
            ContactCommand::Answer { name, mode, out } => peer_set_answer(named(&out, name), mode, out),
            ContactCommand::Control { name, on, out } => peer_set_control(named(&out, name), on == "on", out),
            ContactCommand::Receive { name, folder, out } => peer_set_receive(named(&out, name), folder, out),
            ContactCommand::List { path } => peer_list(path),
            ContactCommand::Requests => peer_requests(),
            ContactCommand::Blocked => peer_blocked(),
            ContactCommand::Unblock { key } => {
                let key = peerbook::decode_key(&key)?;
                requests::forget(&paths().requests(), &key).context("updating contact requests")?;
                println!("they can ask to be added again");
                Ok(())
            }
        },
        Some(Command::Listen { identity, contacts, port, cover }) => run_listen(expand_home(&identity), contacts, port, cover),
        Some(Command::Ping { contact, contacts, identity }) => {
            let identity = load_identity(identity)?;
            run_ping(identity, named(&contacts, contact), contacts)
        }
        Some(Command::Send { contact, message, contacts, identity }) => {
            let identity = load_identity(identity)?;
            run_send(identity, named(&contacts, contact), message, contacts)
        }
        Some(Command::Post { text, public, out, identity }) => {
            let identity = load_identity(identity)?;
            let visibility = if public { Visibility::Public } else { Visibility::Private };
            posts::add(&expand_home(&out), &identity, &text, visibility)?;
            println!("posted ({})", if public { "public" } else { "private" });
            Ok(())
        }
        Some(Command::Posts { contact, contacts, identity }) => {
            let identity = load_identity(identity)?;
            run_posts(identity, named(&contacts, contact), contacts)
        }
        Some(Command::Call { contact, video, contacts, identity }) => {
            let identity = load_identity(identity)?;
            run_call(identity, named(&contacts, contact), contacts, video)
        }
        Some(Command::SendFile { contact, path }) => run_send_file(named(&paths().peers(), contact), path),
        Some(Command::Transfers) => run_transfers(),
        Some(Command::VideoCheck) => {
            swarm_app::hardware::check(&mut |line| println!("{line}"));
            Ok(())
        }
        Some(Command::Manual { width }) => {
            use std::io::IsTerminal;
            let terminal = std::io::stdout().is_terminal().then(|| crossterm::terminal::size().ok()).flatten().map(|(columns, _)| usize::from(columns));
            print!("{}", swarm_app::manual::get().text(width.or(terminal).unwrap_or(80)));
            Ok(())
        }
        Some(Command::ScreenCheck) => {
            let screen = swarm_app::screen::Screen::open()?;
            println!("sharing {} — move the pointer for five seconds...", screen.info().name);
            let mut pictures = screen.subscribe();
            let mut new = 0;
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(20));
                if pictures.has_changed().unwrap_or(false) {
                    pictures.borrow_and_update();
                    new += 1;
                }
            }
            let (taken, failed) = screen.counts();
            println!("{taken} pictures taken, {failed} couldn't be, {new} new ones (what changed)");
            if let Some(why) = screen.ended() {
                println!("sharing ended: {why}");
            }
            Ok(())
        }
        Some(Command::Transfer { action }) => run_transfer(action),
        Some(Command::Device { action }) => run_device(action),
        Some(Command::Group { action }) => {
            let me = || load_identity(PathBuf::from("~/.swarm/identity"));
            match action {
                GroupCommand::New { contacts } => run_group_new(contacts),
                GroupCommand::Add { group, contact } => {
                    let contact = named(&paths().peers(), contact);
                    run_group_ask(me()?, group, "GROUP_ADD", Some(contact.clone()), &format!("added {contact}"))
                }
                GroupCommand::Post { group, text, identity } => run_group_post(load_identity(identity)?, group, text),
                GroupCommand::Name { group, name } => run_group_ask(me()?, group, "GROUP_NAME", Some(name.unwrap_or_default()), "renamed"),
                GroupCommand::Leave { group } => run_group_ask(me()?, group, "GROUP_LEAVE", None, "left"),
                GroupCommand::Sync { group } => run_group_ask(me()?, group, "GROUP_SYNC", None, "catching up with everyone in it who's connected"),
                GroupCommand::Show { group, identity } => run_group_show(load_identity(identity)?, group),
                GroupCommand::List { identity } => run_group_list(load_identity(identity)?),
            }
        }
    }
}

/// Reads one line from stdin after printing `label` (no newline, so the
/// answer appears on the same line). `None` means end-of-input (Ctrl+D,
/// or stdin isn't a terminal) — callers treat that the same as
/// "cancelled" rather than looping forever on an instantly-returning
/// empty read.
fn prompt(label: &str) -> Option<String> {
    print!("{label}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line.trim().to_string()),
        Err(_) => None,
    }
}

/// Whether there's a screen to open Swarm's window on: always on Windows;
/// on Linux and the like, any X11 or Wayland display — a desktop session,
/// or an SSH login forwarding X, where the window opens over the network.
/// A plain text console or SSH login gets the terminal UI, as does `--tui`.
fn can_open_window() -> bool {
    if !cfg!(feature = "gui") {
        return false;
    }
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    cfg!(windows) || set("DISPLAY") || set("WAYLAND_DISPLAY")
}

/// Swarm's window (see `desktop/`). On Windows, started by double-clicking
/// (no console of its own to keep), the console window it came with is let
/// go, so there's just the window.
#[cfg(feature = "gui")]
fn run_window(cover: Option<u64>) -> Result<()> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
        let mut list = [0u32; 2];
        if GetConsoleProcessList(list.as_mut_ptr(), 2) == 1 {
            FreeConsole();
        }
    }
    #[cfg(not(windows))]
    if std::env::var_os("WINIT_X11_SCALE_FACTOR").is_none() {
        std::env::set_var("WINIT_X11_SCALE_FACTOR", "randr");
    }
    swarm_desktop::run(cover)
}

fn run_tui(cover_bits_per_second: Option<u64>) -> Result<()> {
    swarm_app::control::ensure_single_instance(DEFAULT_PORT)?;
    let rt = tokio::runtime::Runtime::new()?;
    let identity_path = swarm_app::launch::identity_path();
    let identity = match swarm_app::launch::has_identity(&identity_path) {
        true => load_identity(identity_path.clone())?,
        false => match first_run(&identity_path, &rt)? {
            Some(identity) => identity,
            None => return Ok(()),
        },
    };
    let backend_error: tui::BackendError = Arc::new(Mutex::new(None));

    tui::silence_stderr();

    rt.block_on(async move {
        let (node, call_events) = match swarm_app::launch::start(identity, cover_bits_per_second, Arc::new(|line| eprintln!("{line}"))).await {
            Ok(started) => started,
            Err(e) => {
                tui::restore_stderr();
                return Err(e);
            }
        };
        tui::run(node, paths().peers(), backend_error, call_events).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_check_unlocks_where_there_is_an_identity() {
        let dir = std::env::temp_dir().join(format!("swarm-keeps-nothing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let identity = dir.join("identity");
        assert!(keeps_nothing(&Some(Command::VideoCheck), &identity), "no identity: nothing to unlock");
        std::fs::write(&identity, b"sealed").unwrap();
        assert!(!keeps_nothing(&Some(Command::VideoCheck), &identity));
        assert!(keeps_nothing(&Some(Command::Unlock), &identity));
        assert!(keeps_nothing(&Some(Command::Manual { width: None }), &identity), "the manual's built in, not kept");
        assert!(!keeps_nothing(&Some(Command::Transfers), &identity));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
