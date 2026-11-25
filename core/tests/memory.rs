//! Memory that has to stay in hand on a device left running: measured
//! from the process's own size, so each test file here is a process of
//! its own.

#[cfg(target_os = "linux")]
fn resident_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("VmRSS"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        .unwrap()
}

/// Copies of messages from another of your devices, each checked against
/// a long conversation from whichever thread was free: every check used
/// to copy the whole conversation, and each thread kept a copy's worth of
/// memory for good (100,000 messages: 22 MB became 104 MB).
#[cfg(target_os = "linux")]
#[test]
fn copies_checked_from_many_threads_keep_memory_in_hand() {
    use swarm_core::history::{self, Direction};
    let dir = std::env::temp_dir().join(format!("swarm-memory-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let key = [7u8; 32];
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let text: String = (0..50_000u64).map(|i| format!("{}\t<\tmessage number {i} with some ordinary words in it\n", now - 100_000 + i)).collect();
    swarm_core::store::open_vault(&dir, &swarm_core::identity::Identity::generate().storage_keys());
    swarm_core::store::write_atomic(&swarm_core::peerstore::file_for(&dir, &key), text).unwrap();
    history::append_new(&dir, &key, Direction::Sent, "the first", now, 0, None).unwrap();
    let before = resident_kb();
    for round in 0..4u64 {
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for i in 0..50u64 {
                        history::append_new(&dir, &key, Direction::Sent, &format!("copy {round} {t} {i}"), now, 0, None).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
    }
    let grew = resident_kb().saturating_sub(before);
    assert!(grew < 8_000, "grew by {grew} kB for 1600 messages");
    let _ = std::fs::remove_dir_all(&dir);
}
