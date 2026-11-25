//! The desktop's portal (`org.freedesktop.portal.*`, over D-Bus): how a
//! program asks the desktop for what only it may give, in dialogs of its
//! own — a screen to share (`screen_wayland.rs`), a file to send
//! (`filedialog.rs`).

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

pub const DESTINATION: &str = "org.freedesktop.portal.Desktop";
pub const PATH: &str = "/org/freedesktop/portal/desktop";

/// Calls `method` on the portal's `interface` and waits for its answer:
/// what came back once the person's done, `None` if they cancelled.
/// `token` is the call's `handle_token` (see `token`).
pub fn request(conn: &Connection, interface: &str, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType), token: &str) -> Result<Option<HashMap<String, OwnedValue>>> {
    let sender = conn.unique_name().ok_or_else(|| anyhow!("no name on the session bus"))?.trim_start_matches(':').replace('.', "_");
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = Proxy::new(conn, DESTINATION, path.as_str(), "org.freedesktop.portal.Request")?;
    let mut answers = request.receive_signal("Response")?;
    let portal = Proxy::new(conn, DESTINATION, PATH, interface)?;
    let _: OwnedObjectPath = portal.call(method, body).with_context(|| format!("asking the desktop's portal ({method})"))?;
    let answer = answers.next().ok_or_else(|| anyhow!("the desktop's portal didn't answer"))?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = answer.body().deserialize()?;
    match code {
        0 => Ok(Some(results)),
        1 => Ok(None),
        _ => bail!("the desktop's portal couldn't do it"),
    }
}

/// A `handle_token` not used before by this process.
pub fn token() -> String {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    format!("swarm{}_{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}
