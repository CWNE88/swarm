//! Swarm as the portal's helper (`swarm --portal-helper`), against
//! `tools/portal-lab.py` on a bus of its own. The stand-in portal turns
//! down a caller it can't read, as the real one does, and swarm itself
//! can't be (it keeps out of dumps): so this shows the helper is started
//! before that, as a program the portal can read.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Lab {
    children: Vec<Child>,
    address: String,
    log: std::path::PathBuf,
}

impl Drop for Lab {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.log);
    }
}

/// `None` without dbus-daemon, or Python's D-Bus, here.
fn lab() -> Option<Lab> {
    let mut bus = Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address=1"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut address = String::new();
    BufReader::new(bus.stdout.take()?).read_line(&mut address).ok()?;
    let address = address.trim().to_string();
    let log = std::env::temp_dir().join(format!("swarm-portal-helper-{}.log", std::process::id()));
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../tools/portal-lab.py");
    let portal = Command::new("python3").arg(script).arg(&log).env("DBUS_SESSION_BUS_ADDRESS", &address).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok()?;
    let lab = Lab { children: vec![bus, portal], address, log };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::fs::read_to_string(&lab.log).unwrap_or_default().contains("ready") {
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Some(lab)
}

/// Starts the helper on the lab's bus, asks it for `first`, and returns
/// its answer.
fn ask(lab: &Lab, first: &str) -> String {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut helper = Command::new(env!("CARGO_BIN_EXE_swarm"))
        .arg("--portal-helper")
        .env("DBUS_SESSION_BUS_ADDRESS", &lab.address)
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    (&ours).write_all(format!("{first}\n").as_bytes()).unwrap();
    let mut answer = String::new();
    BufReader::new(&ours).read_line(&mut answer).unwrap();
    drop(ours);
    let deadline = Instant::now() + Duration::from_secs(5);
    while helper.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the helper didn't end with its socket");
        std::thread::sleep(Duration::from_millis(20));
    }
    answer.trim().to_string()
}

#[test]
fn the_portal_helper_is_let_ask_for_a_screen_and_for_control() {
    let Some(lab) = lab() else {
        eprintln!("no dbus-daemon or Python D-Bus here: skipped");
        return;
    };
    assert_eq!(ask(&lab, "screen -"), "ok 44 screen-1");
    assert_eq!(ask(&lab, "control -"), "ok 0 restore-1");
    let log = std::fs::read_to_string(&lab.log).unwrap();
    assert!(!log.contains("refused"), "{log}");
    assert!(log.ends_with("Start\nClose\n"), "the control session closed with the helper: {log}");
}
