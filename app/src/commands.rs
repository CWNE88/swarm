//! The commands typed in a group (`/groupname`, `/add`, `/find`, `/mute`,
//! `/timer`, `/leave`; `/me` is a line of its own, `chat::ChatWhat::Did`)
//! — every front end takes them, so typing works the same everywhere;
//! apps with buttons call the same actions underneath.

use crate::notes::Note;
use crate::paths;
use crate::sidebar::Sidebar;
use anyhow::Result;
use swarm_core::channel::{self, ChannelId};
use swarm_core::node::NodeHandle;

/// Something about a group that's asked yes or no before it's done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    /// Leave it, and take it off the list.
    Leave,
    /// Ask someone in it who isn't a contact to add you.
    Ask([u8; 32]),
}

/// What a command came to.
pub enum Outcome {
    /// Done (or refused): say so.
    Done(Note),
    /// Needs a yes or no first: put `question`, then call `confirm`.
    Ask { what: Confirm, question: String },
}

/// What `/find` is looking for in each group, while it is: its chat
/// shows only the lines with it in (`chat::channel_view`).
static FINDING: std::sync::Mutex<Option<std::collections::HashMap<ChannelId, String>>> = std::sync::Mutex::new(None);

/// What `/find` is looking for in group `id`, if anything.
pub fn finding(id: &ChannelId) -> Option<String> {
    FINDING.lock().ok()?.as_ref()?.get(id).cloned()
}

fn find(id: &ChannelId, wanted: &str) -> Result<String> {
    let mut finding = FINDING.lock().map_err(|_| anyhow::anyhow!("couldn't search just now"))?;
    let finding = finding.get_or_insert_with(Default::default);
    if wanted.is_empty() {
        return Ok(match finding.remove(id) {
            Some(_) => "showing everything again".to_string(),
            None => "/find <text> shows only the lines with it in".to_string(),
        });
    }
    let lower = wanted.to_lowercase();
    let posts = channel::load_posts(&paths().channels(), id).unwrap_or_default();
    let many = posts.iter().filter(|p| p.text.to_lowercase().contains(&lower) || p.name.as_deref().is_some_and(|n| n.to_lowercase().contains(&lower))).count();
    finding.insert(id.clone(), wanted.to_string());
    Ok(match many {
        0 => format!("nothing said here has \"{wanted}\" in it — /find on its own shows it all again"),
        n => format!("{n} message{} with \"{wanted}\" — /find on its own shows them all again", if n == 1 { "" } else { "s" }),
    })
}

fn muted_key(id: &ChannelId) -> String {
    format!("muted.group.{}.{}", base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, id.owner), id.name)
}

/// Whether group `id` is muted on this device (`/mute`): no sound or
/// notification for it, unless something names you.
pub fn muted(id: &ChannelId) -> bool {
    swarm_core::settings::get(&paths().settings(), &muted_key(id)).as_deref() == Some("yes")
}

fn mute(id: &ChannelId, on: bool) -> Result<String> {
    swarm_core::settings::set(&paths().settings(), &muted_key(id), if on { "yes" } else { "no" })?;
    Ok(if on {
        "muted here: no sound or notification, unless someone names you — /unmute undoes it".to_string()
    } else {
        "not muted any more".to_string()
    })
}

/// The command list, for `/help`.
pub const HELP: &str = "/me <does something>  /groupname <name>  /add <contact>  /ask <name>  /find <text>  /mute  /unmute  /timer off|hour|day|week  /leave";

/// Runs `line` (starting `/`) in group `id`.
pub fn run(node: &NodeHandle, sidebar: &Sidebar, id: &ChannelId, line: &str) -> Outcome {
    let (command, arg) = line.split_once(' ').map(|(c, a)| (c, a.trim())).unwrap_or((line, ""));
    let result: Result<String> = match command {
        "/groupname" | "/name" if arg.is_empty() => Ok(match roster(id).name {
            Some(name) => format!("it's called {} — /groupname <name> changes it, /groupname - takes it off", name.text),
            None => "it has no name of its own, so it's called by its members' names — /groupname <name> gives it one".to_string(),
        }),
        "/groupname" | "/name" if arg == "-" => node.rename_group(id, "").map(|()| "the group goes by its members' names again".to_string()),
        "/groupname" | "/name" => node.rename_group(id, arg).map(|()| format!("named the group {arg}")),
        "/add" if !arg.is_empty() => node.add_to_group(id, arg).map(|()| format!("added {} — they'll see what's said from now on", node.label_of(arg))),
        "/add" => Err(anyhow::anyhow!("/add <contact> adds one of your contacts")),
        "/ask" if !arg.is_empty() => match member_named(node, sidebar, id, arg) {
            Ok((_, _, true)) => Err(anyhow::anyhow!("{arg} is your contact already")),
            Ok((key, name, false)) => {
                return Outcome::Ask {
                    what: Confirm::Ask(key),
                    question: format!("ask {name} to add you as a contact? They see your name and your addresses; if they add you, they're yours too"),
                };
            }
            Err(e) => Err(e),
        },
        "/ask" => Err(anyhow::anyhow!("/ask <name> asks someone in the group who isn't your contact to add you")),
        "/timer" if arg.is_empty() => Ok(match roster(id).timer() {
            0 => "messages here are kept — /timer hour|day|week to have them delete themselves".to_string(),
            s => format!("messages here delete themselves after {}", swarm_core::disappearing::describe(s)),
        }),
        "/timer" => match swarm_core::disappearing::parse(arg) {
            Some(0) => node.set_channel_timer(id, 0).map(|()| "messages here are kept from now on".to_string()),
            Some(s) => node.set_channel_timer(id, s).map(|()| format!("messages here delete themselves after {} from now on", swarm_core::disappearing::describe(s))),
            None => Err(anyhow::anyhow!("/timer takes off, hour, day or week")),
        },
        "/leave" | "/part" => {
            let name = crate::names::group_name(&sidebar.peer_names(), node.identity().public_key_bytes(), id, &roster(id));
            return Outcome::Ask { what: Confirm::Leave, question: format!("leave {name}? It goes from your list and your devices, and the others see you left") };
        }
        "/find" | "/search" => find(id, arg),
        "/mute" => mute(id, true),
        "/unmute" => mute(id, false),
        "/help" | "/" => Ok(HELP.to_string()),
        _ => Err(anyhow::anyhow!("don't know {line} — /help lists the commands")),
    };
    Outcome::Done(match result {
        Ok(text) => Note::success(text),
        Err(e) => Note::error(format!("{e:#}")),
    })
}

/// Does what was asked about, once the answer's yes.
pub fn confirm(node: &NodeHandle, id: &ChannelId, what: Confirm) -> Note {
    let result = match what {
        Confirm::Leave => node.leave_group(id).map(|()| "you left the group".to_string()),
        Confirm::Ask(key) => node.ask_to_add(id, &key).map(|()| "asked — it reaches them through the group, and if they add you, they're your contact too".to_string()),
    };
    match result {
        Ok(text) => Note::success(text),
        Err(e) => Note::error(format!("{e:#}")),
    }
}

/// The member of `id` that `given` names (as the members' list shows
/// them, ignoring case, or the start of one): their key, name, and
/// whether they're a contact.
fn member_named(node: &NodeHandle, sidebar: &Sidebar, id: &ChannelId, given: &str) -> Result<([u8; 32], String, bool)> {
    let me = node.identity().public_key_bytes();
    let peers = sidebar.peer_names();
    let names = crate::names::ChannelNames::for_channel(peers.clone(), me, id);
    let wanted = given.trim().trim_start_matches('@').to_lowercase();
    let all: Vec<([u8; 32], String)> = roster(id).members.keys().filter(|k| **k != me).map(|k| (*k, names.current(k))).collect();
    let found: Vec<&([u8; 32], String)> = match all.iter().find(|(_, n)| n.to_lowercase() == wanted) {
        Some(exact) => vec![exact],
        None => all.iter().filter(|(_, n)| n.to_lowercase().starts_with(&wanted)).collect(),
    };
    match found.as_slice() {
        [(key, name)] => Ok((*key, name.clone(), peers.contains_key(key))),
        [] => Err(anyhow::anyhow!("nobody in the group called {given}")),
        _ => Err(anyhow::anyhow!("{given} could be {} — say more of the name", found.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", "))),
    }
}

fn roster(id: &ChannelId) -> channel::Roster {
    channel::replay(id, &channel::load_roster(&paths().channels(), id).unwrap_or_default())
}
