//! Starting an app: your identity (loading it, or making one the first
//! time), making sure only one Swarm runs per device, starting the node,
//! and opening the control socket so other `swarm` commands reach it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use swarm_core::identity::Identity;
use swarm_core::node::{self, CallEvent, NodeConfig, NodeHandle};
use swarm_core::transport::DEFAULT_PORT;
use tokio::sync::mpsc;

/// Where your identity (private key) lives: `~/.swarm/identity`.
pub fn identity_path() -> PathBuf {
    crate::data_dir().join("identity")
}

/// Whether there's an identity at `path` yet (a first run if not).
pub fn has_identity(path: &Path) -> bool {
    path.exists()
}

/// Why unlocking didn't work.
#[derive(Debug)]
pub enum UnlockFailed {
    /// The passphrase isn't this identity's: try again.
    WrongPassphrase,
    /// Anything else (the file's unreadable or damaged): said, and the file left alone — never taken for a first
    /// run, which would replace it.
    Other(anyhow::Error),
}

impl std::fmt::Display for UnlockFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnlockFailed::WrongPassphrase => write!(f, "that isn't the passphrase"),
            UnlockFailed::Other(e) => write!(f, "{e:#}"),
        }
    }
}

/// Opens the identity at `path` with `passphrase`, and with it the data
/// folder beside it (its vault: see `swarm_core::store`). Slow on
/// purpose (Argon2id): run it off any thread that draws.
pub fn unlock(path: &Path, passphrase: &str) -> Result<Identity, UnlockFailed> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display())).map_err(UnlockFailed::Other)?;
    let identity = Identity::unsealed(&text, passphrase).map_err(|e| match e {
        swarm_core::identity::UnlockError::WrongPassphrase => UnlockFailed::WrongPassphrase,
        other => UnlockFailed::Other(anyhow::anyhow!("{}: {other}", path.display())),
    })?;
    open_data(path, &identity);
    Ok(identity)
}

/// Opens the data folder `identity_path` is in, with `identity`'s keys.
fn open_data(identity_path: &Path, identity: &Identity) {
    let dir = identity_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    swarm_core::store::open_vault(dir, &identity.storage_keys());
}

/// Makes a new identity at `path`, sealed under `passphrase`, and opens
/// its data folder.
pub fn create_identity(path: &Path, passphrase: &str) -> Result<Identity> {
    save_new_identity(path, Identity::generate(), passphrase)
}

/// Saves `identity` (a new one, or one brought back from its 12 words)
/// at `path`, sealed under `passphrase` (owner-only on Unix; on Windows,
/// files in your own user folder already are), with the public key beside
/// it in `<path>.pub`, and opens its data folder.
///
/// Never over one that's there: an identity replaced is one lost for
/// good (every peer knows you by it), so that has to be done by hand.
/// Private from the moment it exists, not made so afterwards.
pub fn save_new_identity(path: &Path, identity: Identity, passphrase: &str) -> Result<Identity> {
    use std::io::Write;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = match options.open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::bail!("there's an identity at {} already — it isn't replaced; move it away first if you really mean to", path.display())
        }
        other => other.with_context(|| format!("creating {}", path.display()))?,
    };
    file.write_all(identity.sealed(passphrase).as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    file.sync_all().with_context(|| format!("writing {}", path.display()))?;
    let pub_path = PathBuf::from(format!("{}.pub", path.display()));
    std::fs::write(&pub_path, format!("{}\n", identity.public_key_base64())).with_context(|| format!("writing {}", pub_path.display()))?;
    open_data(path, &identity);
    Ok(identity)
}

/// Saves the user key a device being added was given (see
/// `swarm_core::node::joining`) at `path`, sealed under `passphrase`, as
/// `save_new_identity` does, with `device` as this device's key and the
/// user's device list that came with it.
pub fn save_joined_identity(path: &Path, joined: swarm_core::node::joining::Joined, device: &Identity, passphrase: &str) -> Result<Identity> {
    let identity = save_new_identity(path, joined.user, passphrase)?;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    swarm_core::node::joining::settle(&swarm_core::node::Paths::new(dir), &identity, device, &joined.list, &joined.by)?;
    Ok(identity)
}

/// Seals `identity` at `path` under a new passphrase, in place of what's
/// there (written whole beside it, then renamed over it).
pub fn change_passphrase(path: &Path, identity: &Identity, passphrase: &str) -> Result<()> {
    use std::io::Write;
    let tmp = PathBuf::from(format!("{}.new", path.display()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(identity.sealed(passphrase).as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    file.sync_all().with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// Whether `passphrase` opens the identity at `path`: for unlocking a
/// TUI or window locked after being left alone, while it runs on. Slow
/// on purpose, as `unlock` is.
pub fn is_passphrase(path: &Path, passphrase: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| Identity::unsealed(&text, passphrase).is_ok())
}

/// A forgotten passphrase: the identity at `path` brought back from its
/// 12 `words` (which have to be that identity's), sealed under a new
/// `passphrase`, and its data folder opened.
pub fn reset_passphrase(path: &Path, words: &str, passphrase: &str) -> Result<Identity> {
    let identity = Identity::from_words(words)?;
    let pub_path = PathBuf::from(format!("{}.pub", path.display()));
    if let Ok(on_file) = std::fs::read_to_string(&pub_path) {
        anyhow::ensure!(on_file.trim() == identity.public_key_base64(), "those are the words of a different identity from the one here");
    }
    change_passphrase(path, &identity, passphrase)?;
    open_data(path, &identity);
    Ok(identity)
}

/// The same passphrase typed twice, and hard enough to guess: or why not.
pub fn check_new_passphrase(passphrase: &str, again: &str) -> Result<(), String> {
    check_passphrase(passphrase)?;
    if passphrase != again {
        return Err("the two passphrases aren't the same".into());
    }
    Ok(())
}

/// Whether `passphrase` is hard enough to guess: zxcvbn's top score
/// ("very unguessable"), or why not, said plainly. Length alone isn't
/// it: `aaaaaaaaaaaa` is long.
pub fn check_passphrase(passphrase: &str) -> Result<(), String> {
    if passphrase.trim().is_empty() {
        return Err("a passphrase is needed".into());
    }
    let estimate = zxcvbn::zxcvbn(passphrase, &["swarm"]);
    if estimate.score() == zxcvbn::Score::Four {
        return Ok(());
    }
    let mut why = String::from("that's too easy to guess");
    if let Some(feedback) = estimate.feedback() {
        if let Some(warning) = feedback.warning() {
            why = format!("that's too easy to guess: {}", warning.to_string().trim_end_matches('.').to_lowercase());
        }
    }
    Err(format!("{why}. Four or five random words, or a sentence of your own, will do"))
}

/// A passphrase to suggest: five random words from the same list as the
/// recovery words (about 55 bits).
pub fn suggest_passphrase() -> String {
    use rand::Rng;
    let words = swarm_core::identity::word_list();
    let mut rng = rand::rngs::OsRng;
    (0..5).map(|_| words[rng.gen_range(0..words.len())]).collect::<Vec<_>>().join(" ")
}

/// The 12 recovery words, numbered and laid out in four rows of three,
/// for a terminal.
pub fn words_for_terminal(identity: &Identity) -> String {
    let words = identity.recovery_words();
    let cells: Vec<String> = words.iter().enumerate().map(|(i, w)| format!("{:>2}. {w:<10}", i + 1)).collect();
    cells.chunks(3).map(|row| format!("  {}", row.join("  ").trim_end())).collect::<Vec<_>>().join("\n")
}

/// What's said with the words wherever they're shown.
pub const WORDS_ADVICE: &str = "Write these 12 words down and keep them somewhere safe and private. \
They're the only way to get your identity back if this device is lost, and anyone who has them can be you.";

/// A running app's node: refuses if Swarm is already running on this
/// device, starts the node (default port, `cover` as the default cover
/// rate, log lines to `log`), and opens the control socket.
pub async fn start(
    identity: Identity,
    cover: Option<u64>,
    log: node::Logger,
) -> Result<(NodeHandle, mpsc::UnboundedReceiver<CallEvent>)> {
    crate::control::ensure_single_instance(DEFAULT_PORT)?;
    let log = crate::time::stamped(log);
    let config = NodeConfig { paths: crate::paths(), port: DEFAULT_PORT, cover_bits_per_second: cover, log: log.clone() };
    let (node, call_events) = node::start(Arc::new(identity), config).await?;
    tokio::spawn(crate::devices::keep_addresses(node.clone()));
    tokio::spawn(crate::discovery::run(node.clone()));
    tokio::spawn(crate::control::serve(node.clone()));
    Ok((node, call_events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_is_never_replaced_and_opens_only_with_its_passphrase() {
        let dir = std::env::temp_dir().join(format!("swarm-launch-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("identity");
        assert!(!has_identity(&path), "no file: a first run");
        let made = create_identity(&path, "one two three four five").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert!(create_identity(&path, "one two three four five").is_err(), "never over one that's there");
        assert!(matches!(unlock(&path, "one two three four six"), Err(UnlockFailed::WrongPassphrase)));
        assert_eq!(unlock(&path, "one two three four five").unwrap().public_key_bytes(), made.public_key_bytes());
        change_passphrase(&path, &made, "six seven eight nine ten").unwrap();
        assert!(matches!(unlock(&path, "one two three four five"), Err(UnlockFailed::WrongPassphrase)));
        assert_eq!(unlock(&path, "six seven eight nine ten").unwrap().public_key_bytes(), made.public_key_bytes());
        std::fs::write(&path, "half a ke").unwrap();
        assert!(matches!(unlock(&path, "six seven eight nine ten"), Err(UnlockFailed::Other(_))), "damaged: said, not a wrong passphrase");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guessable_passphrases_are_refused_and_suggestions_arent() {
        for weak in ["", "password", "Fluffy2019!", "aaaaaaaaaaaaaaaa", "qwertyuiop", "swarm swarm", "orbit candle"] {
            assert!(check_passphrase(weak).is_err(), "{weak:?} should be refused");
        }
        assert!(check_passphrase("Fluffy2019!").unwrap_err().contains("guess"));
        for _ in 0..20 {
            let suggested = suggest_passphrase();
            assert_eq!(suggested.split(' ').count(), 5);
            assert!(check_passphrase(&suggested).is_ok(), "{suggested:?} was suggested but refused");
        }
        assert!(check_passphrase("my cat rides the 7:15 bus to Leeds").is_ok());
    }
}
