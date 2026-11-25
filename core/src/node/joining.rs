//! Adding a device to a user, by a code one
//! of their devices gives out: `swarm-device:<user key>?a=<addresses>&s=<secret>`,
//! shown as a QR code or text.
//!
//! The secret is the recovery seed of a key made for this one use. The
//! new device dials the user at the code's addresses with that key as its
//! own user key, and with its own device key, so the handshake itself
//! shows it holds the secret, and that it's talking to the user the code
//! names. It asks to join (`Request::JoinUser`, with its name); the
//! device that gave out the code signs its device key into the list and
//! answers with the user key's seed and the list (`Reply::UserKey`), over
//! that end-to-end connection. A code works once, for `CODE_LIFETIME`.
//!
//! The other way in is the 12 words, which make the user key; a device
//! that has it signs itself into the list when its node starts
//! (`list_me`).

use super::{devices, Ctx, Paths};
use crate::devicelist::{self, Change};
use crate::identity::{Identity, RECOVERY_SEED_LEN};
use crate::lock::Locked;
use crate::protocol::{self, Incoming, Reply, Request};
use crate::transport::{self, AuthenticatedConnection};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What every code starts with.
const SCHEME: &str = "swarm-device:";
/// How long a code can be used for.
pub const CODE_LIFETIME: Duration = Duration::from_secs(600);
/// How long a device being added has to ask, once connected.
const ASK_WITHIN: Duration = Duration::from_secs(30);

/// A code for adding a device, taken apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinCode {
    pub user: [u8; 32],
    /// Where the device that gave it out can be dialled: comma-separated,
    /// as a peer's address.
    pub addresses: String,
    pub secret: [u8; RECOVERY_SEED_LEN],
}

impl JoinCode {
    /// The code as one line of text (what the QR code holds).
    pub fn line(&self) -> String {
        format!("{SCHEME}{}?a={}&s={}", URL_SAFE_NO_PAD.encode(self.user), self.addresses, URL_SAFE_NO_PAD.encode(self.secret))
    }

    pub fn parse(text: &str) -> Result<JoinCode> {
        let rest = text.trim().strip_prefix(SCHEME).context("that isn't a code for adding a device")?;
        let (user, query) = rest.split_once('?').context("that code is cut short")?;
        let user: [u8; 32] = URL_SAFE_NO_PAD.decode(user).ok().and_then(|k| k.try_into().ok()).context("that code's key is wrong")?;
        let (mut addresses, mut secret) = (None, None);
        for part in query.split('&') {
            match part.split_once('=') {
                Some(("a", a)) => addresses = Some(a.to_string()),
                Some(("s", s)) => secret = URL_SAFE_NO_PAD.decode(s).ok().and_then(|s| s.try_into().ok()),
                _ => {}
            }
        }
        let addresses = addresses.filter(|a| !a.is_empty()).context("that code has no address")?;
        Ok(JoinCode { user, addresses, secret: secret.context("that code's secret is wrong")? })
    }

    /// The key made for this one use.
    fn key(&self) -> Identity {
        Identity::from_recovery_seed(self.secret)
    }
}

/// A code given out here, waiting to be used.
pub(super) struct Offer {
    /// The one-use key's (the user key the new device dials with).
    key: [u8; 32],
    until: Instant,
}

/// Gives out a code for adding a device, good for `CODE_LIFETIME` and
/// once; any code given out before stops working.
pub(super) fn offer(ctx: &Ctx) -> Result<JoinCode> {
    let addresses = ctx.my_addresses.borrow().join(",");
    if addresses.is_empty() {
        bail!("this device doesn't know its addresses yet: try again in a moment");
    }
    let list = devicelist::load(&ctx.paths.device_list(), ctx.identity.public_key_bytes())?;
    if list.devices().len() >= devicelist::MAX_DEVICES {
        bail!("you have {} devices, the most there can be", devicelist::MAX_DEVICES);
    }
    let secret: [u8; RECOVERY_SEED_LEN] = rand::random();
    let code = JoinCode { user: ctx.identity.public_key_bytes(), addresses, secret };
    *ctx.join_offer.locked() = Some(Offer { key: code.key().public_key_bytes(), until: Instant::now() + CODE_LIFETIME });
    Ok(code)
}

/// Whether `user` (a connection's user key) is a code given out here
/// that's still good.
pub(super) fn is_offered(ctx: &Ctx, user: &[u8; 32]) -> bool {
    ctx.join_offer.locked().as_ref().is_some_and(|o| o.key == *user && o.until > Instant::now())
}

/// Serves a connection from a device using a code given out here.
pub(super) fn serve(ctx: Arc<Ctx>, mut conn: AuthenticatedConnection) {
    tokio::spawn(async move {
        let asked = tokio::time::timeout(ASK_WITHIN, async {
            loop {
                let incoming = protocol::recv(&mut conn).await?;
                protocol::flush(&mut conn).await?;
                if let Incoming::Request(id, Request::JoinUser { name }) = incoming {
                    return anyhow::Ok((id, name));
                }
            }
        });
        let Ok(Ok((id, name))) = asked.await else { return };
        let answer = match add(&ctx, &conn, &name) {
            Ok(answer) => {
                devices::news(&ctx, format!("{name} is one of your devices now"));
                answer
            }
            Err(e) => {
                log!(ctx, "a device asked to be added and wasn't: {e:#}");
                Reply::Refused(format!("{e:#}"))
            }
        };
        if protocol::reply(&mut conn, id, answer).await.is_ok() {
            let _ = tokio::time::timeout(ASK_WITHIN, async { while protocol::recv(&mut conn).await.is_ok() {} }).await;
        }
    });
}

/// Takes the code (it works once), signs the device asking into the list
/// as `name`, and gives what it's to have.
fn add(ctx: &Ctx, conn: &AuthenticatedConnection, name: &str) -> Result<Reply> {
    let taken = ctx.join_offer.locked().take_if(|o| o.key == conn.remote_user && o.until > Instant::now());
    if taken.is_none() {
        bail!("that code has been used, or has run out");
    }
    devicelist::validate_name(name).map_err(anyhow::Error::msg)?;
    let me = ctx.identity.public_key_bytes();
    let device = conn.remote_device;
    let list = devicelist::update(&ctx.paths.device_list(), me, |list| {
        if list.removed(&device) {
            bail!("that device was taken off your devices before");
        }
        if list.devices().len() >= devicelist::MAX_DEVICES {
            bail!("you have {} devices, the most there can be", devicelist::MAX_DEVICES);
        }
        list.sign(&ctx.identity, device, Change::Added { name: name.to_string() });
        Ok(devicelist::to_text(list))
    })??;
    let _ = crate::addressbook::see(&ctx.paths.addresses(), &device, &crate::addressbook::came_from(conn.connection.remote_address()));
    devices::changed(ctx);
    Ok(Reply::UserKey { seed: ctx.identity.private_key_bytes(), list })
}

/// What a device being added is given.
pub struct Joined {
    pub user: Identity,
    /// The user's device list, as `devicelist::to_text` has it.
    pub list: String,
    /// The device that added it, and where: what it first dials.
    pub by: Found,
}

/// Another of the user's devices, and where it was reached.
pub struct Found {
    pub device: [u8; 32],
    pub addresses: String,
}

/// Uses `code` to add this device (`device`, its own key, kept for when
/// it's set up) to the user, as `name`. Nothing's saved: see `settle`.
pub async fn join(code: &JoinCode, device: &Identity, name: &str) -> Result<Joined> {
    devicelist::validate_name(name).map_err(anyhow::Error::msg)?;
    let key = code.key();
    let attempt = async {
        let mut conn = super::oneshot::dial(transport::Keys { user: &key, device }, &code.addresses, code.user).await?;
        let reply = protocol::join_user_and_wait(&mut conn, name).await?;
        anyhow::Ok((reply, conn.remote_device))
    };
    let (reply, by) = tokio::time::timeout(protocol::REPLY_TIMEOUT * 3, attempt).await.unwrap_or_else(|_| Err(anyhow::anyhow!("no answer")))?;
    match reply {
        Reply::UserKey { seed, list } => {
            let user = Identity::from_recovery_seed(seed);
            if user.public_key_bytes() != code.user {
                bail!("the device that answered gave a different user's key");
            }
            let on_it = devicelist::from_text(&list).is_some_and(|l| l.user == code.user && l.devices().contains_key(&device.public_key_bytes()));
            if !on_it {
                bail!("the device that answered didn't add this one to your devices");
            }
            Ok(Joined { user, list, by: Found { device: by, addresses: code.addresses.clone() } })
        }
        Reply::Refused(why) => bail!("not added: {why}"),
        other => bail!("an answer that makes no sense: {other:?}"),
    }
}

/// Keeps what joining gave in `paths`, with `device` as this device's key:
/// for once the user's identity is saved and its vault open. `list` is
/// the list it came with, `by` the device that added this one, dialled
/// first.
pub fn settle(paths: &Paths, user: &Identity, device: &Identity, list: &str, by: &Found) -> Result<()> {
    let list = devicelist::from_text(list).filter(|l| l.user == user.public_key_bytes()).context("the device list isn't this user's")?;
    {
        let _guard = crate::store::lock();
        crate::store::write_atomic(&paths.device_key(), device.private_key_base64() + "\n")?;
        crate::store::write_atomic(&paths.device_list(), devicelist::to_text(&list))?;
    }
    let addresses: Vec<String> = crate::peers::address_items(&by.addresses).map(str::to_string).collect();
    crate::addressbook::see(&paths.addresses(), &by.device, &addresses)?;
    Ok(())
}

/// Puts this device on the user's list, as `name` (its host name, if
/// none), if it isn't yet: a device whose user key came from the 12
/// words, or the user's first. Not one that's been taken off.
pub(super) fn list_me(ctx: &Ctx, name: Option<&str>) -> Result<()> {
    let device = ctx.device.public_key_bytes();
    let added = devicelist::update(&ctx.paths.device_list(), ctx.identity.public_key_bytes(), |list| {
        if list.removed(&device) || list.devices().contains_key(&device) {
            return false;
        }
        let name = name.map(str::to_string).unwrap_or_else(devicelist::default_name);
        list.sign(&ctx.identity, device, Change::Added { name });
        true
    })?;
    if added {
        devices::changed(ctx);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_reads_back() {
        let code = JoinCode { user: [3; 32], addresses: "[2001:db8::1]:5000,192.168.1.2".into(), secret: [9; 16] };
        assert_eq!(JoinCode::parse(&format!("  {}\n", code.line())).unwrap(), code);
        assert!(JoinCode::parse("swarm:abc?a=1.2.3.4").is_err());
        assert!(JoinCode::parse(&code.line().replace("&s=", "&x=")).is_err());
    }
}
