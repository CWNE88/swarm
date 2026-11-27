//! Your identity on the command line: making it (or adding this device
//! to your user), the passphrase that locks it, and unlocking it.

use super::*;

/// The first run, as the window has it: a new user, or this device added
/// to yours (with a code from another of your devices, or the 12 words).
/// `None` at the end of input.
pub(crate) fn first_run(identity_path: &std::path::Path, rt: &tokio::runtime::Runtime) -> Result<Option<Identity>> {
    let named = |identity: &Identity, name: &str| {
        if !name.is_empty() {
            let _ = swarm_core::displayname::set(&paths().displayname(), identity, name);
        }
    };
    println!("Welcome to Swarm\n");
    println!("Is this your first device with Swarm, or are you already on Swarm on another?\n");
    println!("  1  New user");
    println!("  2  Add this device to my user\n");
    println!("A new user is someone new: people add you by it. Adding this device makes it one of yours, with your name, contacts and chats, kept in step with your others.\n");
    let adding = loop {
        match prompt("1 or 2: ").as_deref() {
            None => return Ok(None),
            Some("1") => break false,
            Some("2") => break true,
            Some(_) => {}
        }
    };
    if !adding {
        println!("\nNew user\n");
        let Some(name) = prompt("Your name (optional): ") else { return Ok(None) };
        let Some(pass) = ask_new_passphrase() else { return Ok(None) };
        let identity = swarm_app::launch::create_identity(identity_path, &pass)?;
        named(&identity, &name);
        println!("made your identity: {}\n", identity.public_key_base64());
        println!("Your recovery words:\n\n{}\n\n{}\n", swarm_app::launch::words_for_terminal(&identity), swarm_app::launch::WORDS_ADVICE);
        if prompt("Press Enter once you've written them down. ").is_none() {
            return Ok(None);
        }
        return Ok(Some(identity));
    }
    println!("\nAdd this device to my user\n");
    println!("On another of your devices, in Contacts choose Add a device, at the end of your devices under You. Paste the line under its code here.");
    println!("Or, with no other device to hand, press Enter and type your 12 recovery words instead.\n");
    let code = loop {
        let Some(line) = prompt("The line under the code: ") else { return Ok(None) };
        if line.is_empty() {
            break None;
        }
        match swarm_core::node::joining::JoinCode::parse(&line) {
            Ok(code) => break Some(code),
            Err(e) => println!("{e:#}"),
        }
    };
    let Some(code) = code else {
        let Some(restored) = ask_for_words() else { return Ok(None) };
        let Some(name) = prompt("Your name (optional): ") else { return Ok(None) };
        let Some(pass) = ask_new_passphrase() else { return Ok(None) };
        let identity = swarm_app::launch::save_new_identity(identity_path, restored, &pass)?;
        named(&identity, &name);
        println!("brought back your identity: {}", identity.public_key_base64());
        return Ok(Some(identity));
    };
    let Some(name) = prompt("This device's name (optional): ") else { return Ok(None) };
    let name = if name.is_empty() { swarm_app::devices::default_name() } else { name };
    let Some(pass) = ask_new_passphrase() else { return Ok(None) };
    println!("Asking your other device…");
    let device = Identity::generate();
    let joined = rt.block_on(swarm_core::node::joining::join(&code, &device, &name))?;
    let identity = swarm_app::launch::save_joined_identity(identity_path, joined, &device, &pass)?;
    println!("this device is one of yours now, called {name}");
    Ok(Some(identity))
}

/// Asks for the 12 recovery words until they make an identity (each
/// mistake said), or `None` at the end of input.
pub(crate) fn ask_for_words() -> Option<Identity> {
    loop {
        let typed = prompt("Your 12 recovery words (the first four letters of each are enough): ")?;
        match Identity::from_words(&typed) {
            Ok(identity) => return Some(identity),
            Err(e) => println!("{e}"),
        }
    }
}

pub(crate) fn keygen(out: Option<PathBuf>, restore: bool, join: Option<String>) -> Result<()> {
    if let Some(code) = join {
        return keygen_joining(out, &code);
    }
    let made = if restore {
        let Some(identity) = ask_for_words() else { anyhow::bail!("no words given") };
        identity
    } else {
        Identity::generate()
    };
    let identity = match out {
        Some(path) => {
            let path = expand_home(&path);
            let Some(pass) = ask_new_passphrase() else { anyhow::bail!("no passphrase given") };
            let identity = swarm_app::launch::save_new_identity(&path, made, &pass)?;
            eprintln!("identity written to {}", path.display());
            eprintln!("public key written to {}.pub", path.display());
            identity
        }
        None => made,
    };
    if !restore {
        println!("{}\n\n{}\n", swarm_app::launch::words_for_terminal(&identity), swarm_app::launch::WORDS_ADVICE);
    }
    println!("public:  {}", identity.public_key_base64());
    Ok(())
}

/// Adds this device to your user with `code` (see `keygen --join`).
pub(crate) fn keygen_joining(out: Option<PathBuf>, code: &str) -> Result<()> {
    // Kept in Swarm's folder unless said otherwise.
    let path = out.unwrap_or_else(|| swarm_app::paths::data_dir().join("identity"));
    let path = expand_home(&path);
    if path.exists() {
        anyhow::bail!("there's an identity at {} already — it isn't replaced; move it away first if you really mean to", path.display());
    }
    let code = swarm_core::node::joining::JoinCode::parse(code)?;
    let Some(pass) = ask_new_passphrase() else { anyhow::bail!("no passphrase given") };
    let device = Identity::generate();
    let name = swarm_core::devicelist::default_name();
    let joined = tokio::runtime::Runtime::new()?.block_on(swarm_core::node::joining::join(&code, &device, &name))?;
    let identity = swarm_app::launch::save_joined_identity(&path, joined, &device, &pass)?;
    eprintln!("identity written to {}", path.display());
    eprintln!("this device is one of yours now, called {name} (`swarm device rename` changes that)");
    println!("public:  {}", identity.public_key_base64());
    Ok(())
}

/// Reads a passphrase: without echoing it from a terminal, or as a line
/// from standard input when that isn't one (a script, a lab). `None` at
/// the end of input.
pub(crate) fn read_passphrase(label: &str) -> Option<String> {
    use std::io::IsTerminal;
    if io::stdin().is_terminal() {
        rpassword::prompt_password(label).ok()
    } else {
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
        }
    }
}

/// A new passphrase, asked twice and checked, until one's good enough
/// (`None` at the end of input). Offers one first.
pub(crate) fn ask_new_passphrase() -> Option<String> {
    println!("Choose a passphrase. You'll type it each time Swarm starts, and everything Swarm keeps here is locked with it.");
    println!("Four or five random words, or a sentence of your own, will do — for instance: {}", swarm_app::launch::suggest_passphrase());
    loop {
        let pass = read_passphrase("Passphrase: ")?;
        let again = read_passphrase("The same again: ")?;
        match swarm_app::launch::check_new_passphrase(&pass, &again) {
            Ok(()) => return Some(pass),
            Err(why) => println!("{why}"),
        }
    }
}

/// The identities this process has unlocked, by file: each asked for
/// once (its recovery seed kept, to make it again).
pub(crate) static UNLOCKED: Mutex<Vec<(PathBuf, zeroize::Zeroizing<[u8; swarm_core::identity::RECOVERY_SEED_LEN]>)>> = Mutex::new(Vec::new());

/// The identity at `path`, unlocked with its passphrase (asked for, up
/// to three times from a terminal), its data folder opened with it.
/// Everything kept on disk is sealed: a command that reads or writes it
/// needs the passphrase first. Not those that make an identity, read only
/// the public key, unlock a waiting `listen` (or are one), or touch
/// nothing kept; the TUI and the window ask in their own way. `swarm
/// video-check` reads Settings (the camera chosen, hardware video), so
/// unlocks where there's an identity (`identity`), and goes by Settings'
/// defaults where there isn't.
pub(crate) fn keeps_nothing(command: &Option<Command>, identity: &std::path::Path) -> bool {
    match command {
        Some(Command::VideoCheck) => !identity.exists(),
        Some(Command::ScreenCheck | Command::Manual { .. }) => true,
        None | Some(Command::Keygen { .. } | Command::Pubkey { .. } | Command::Unlock | Command::Passphrase { .. } | Command::Listen { .. }) => true,
        Some(_) => false,
    }
}

pub(crate) fn load_identity(path: PathBuf) -> Result<Identity> {
    use swarm_app::launch::UnlockFailed;
    let path = expand_home(&path);
    if let Some((_, seed)) = UNLOCKED.locked().iter().find(|(p, _)| *p == path) {
        return Ok(Identity::from_recovery_seed(**seed));
    }
    if !path.exists() {
        anyhow::bail!("there's no identity at {} — run `swarm` (or `swarm keygen --out {}`) to make one", path.display(), path.display());
    }
    for _ in 0..3 {
        let Some(pass) = read_passphrase(&format!("Passphrase for {}: ", path.display())) else { anyhow::bail!("no passphrase given") };
        match swarm_app::launch::unlock(&path, &pass) {
            Ok(identity) => {
                UNLOCKED.locked().push((path, zeroize::Zeroizing::new(identity.private_key_bytes())));
                return Ok(identity);
            }
            Err(UnlockFailed::WrongPassphrase) => eprintln!("that isn't the passphrase"),
            Err(UnlockFailed::Other(e)) => return Err(e),
        }
    }
    anyhow::bail!("not unlocked")
}

/// This device's own key (see `node::device_identity`), for a connection
/// of the command's own; the vault's open once the identity is.
pub(crate) fn device_of() -> Result<Identity> {
    node::device_identity(&paths()).context("this device's key")
}

/// The user's key and this device's, as a connection takes them.
pub(crate) fn keys_of<'a>(user: &'a Identity, device: &'a Identity) -> swarm_core::transport::Keys<'a> {
    swarm_core::transport::Keys { user, device }
}

/// `swarm unlock`: hands the passphrase to a `swarm listen` waiting for
/// it, until it's taken.
pub(crate) fn run_unlock() -> Result<()> {
    loop {
        let Some(pass) = read_passphrase("Passphrase: ") else { anyhow::bail!("no passphrase given") };
        let answer = swarm_app::control::ask_blocking(&format!("UNLOCK\n{pass}\n")).context("no swarm is waiting to be unlocked here")?;
        match answer.trim_end() {
            "OK" => {
                println!("unlocked");
                return Ok(());
            }
            other => {
                println!("{}", other.strip_prefix("ERROR ").unwrap_or(other));
                if other.contains("already") {
                    return Ok(());
                }
            }
        }
    }
}

/// `swarm passphrase`: a new passphrase for the identity at `path`,
/// given the old one, or (`forgotten`) its 12 words.
pub(crate) fn change_passphrase(path: PathBuf, forgotten: bool) -> Result<()> {
    if forgotten {
        loop {
            let Some(words) = prompt("Your 12 recovery words: ") else { anyhow::bail!("no words given") };
            let Some(pass) = ask_new_passphrase() else { anyhow::bail!("no passphrase given") };
            match swarm_app::launch::reset_passphrase(&path, &words, &pass) {
                Ok(_) => break,
                Err(e) => println!("{e:#}"),
            }
        }
    } else {
        let identity = load_identity(path.clone())?;
        let Some(pass) = ask_new_passphrase() else { anyhow::bail!("no passphrase given") };
        swarm_app::launch::change_passphrase(&path, &identity, &pass)?;
    }
    println!("passphrase changed");
    Ok(())
}
