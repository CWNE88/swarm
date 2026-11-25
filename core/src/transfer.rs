//! File transfers: the record of each one (`~/.swarm/transfers`), and the
//! file handling both ends need — naming, the part file a download grows
//! in, checking it, and moving it into place.
//!
//! How a transfer goes (the messages are in `protocol`, the driving in
//! the node's per-peer task):
//!
//! 1. The sender offers a file — name, size, SHA-256 — and keeps
//!    re-offering it on every connection until it's answered.
//! 2. Nothing more moves until the receiver's person accepts and picks a
//!    folder. A decline or cancel is final.
//! 3. The receiver *pulls* the file a chunk at a time, a few chunks ahead,
//!    appending to a hidden part file in that folder — so it can never be
//!    sent more than it asked for, and after a dropped connection or a
//!    restart it carries on from what the part file already holds.
//! 4. Once it has every byte it checks the SHA-256: a match is renamed into
//!    place (never over an existing file), a mismatch is deleted. It tells
//!    the sender it's done.
//!
//! The name a sender gives is only ever used as a hint (`sanitise_name`):
//! a received file can't land outside the folder chosen for it, overwrite
//! anything, or have a name the filesystem would read as something else.

use crate::store;
use crate::wire::hex;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub type TransferId = [u8; 16];

/// The most bytes asked for in one chunk — one protocol frame's worth.
pub const CHUNK: usize = 4000;
/// How many chunks a receiver keeps asked for at once, to begin with; it
/// grows while chunks come back promptly (see `node::files`), up to
/// `MAX_WINDOW` (a fixed few would cap a transfer at a few chunks per
/// round trip, whatever the link could carry).
pub const WINDOW: usize = 4;
/// The most chunks asked for at once: as many as QUIC lets be on their
/// way (`transport::STREAM_WINDOW`), more being no faster. What really
/// holds the window is chunks starting to queue (`node::files`); this is
/// the backstop. Under cover, 8 MB is well under what may wait for it
/// (`protocol::MAX_COVER_QUEUE`, 32 MB).
pub const MAX_WINDOW: usize = crate::transport::STREAM_WINDOW as usize / CHUNK;
const _: () = assert!(MAX_WINDOW * CHUNK * 3 < crate::protocol::MAX_COVER_QUEUE);
/// The most offers one peer may have waiting on an answer at once; past
/// it, new ones are ignored until some are answered.
pub const MAX_PENDING_OFFERS_PER_PEER: usize = 20;
/// The most finished transfers kept on record (newest first); open ones
/// are always kept.
pub const MAX_FINISHED_KEPT: usize = 200;
/// The longest file name kept, in bytes: filesystems count bytes (255
/// at most, usually), and the part file adds a little to the name.
const MAX_NAME_BYTES: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Out,
    In,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Offered, not answered yet.
    Offered,
    /// Accepted: data is moving (or will, next time they're connected).
    Active,
    Done,
    Declined,
    Cancelled,
    /// Couldn't be finished — see `Transfer::note`.
    Failed,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Offered => "offered",
            State::Active => "active",
            State::Done => "done",
            State::Declined => "declined",
            State::Cancelled => "cancelled",
            State::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<State> {
        Some(match s {
            "offered" => State::Offered,
            "active" => State::Active,
            "done" => State::Done,
            "declined" => State::Declined,
            "cancelled" => State::Cancelled,
            "failed" => State::Failed,
            _ => return None,
        })
    }

    /// Nothing more will happen to it.
    pub fn is_finished(self) -> bool {
        matches!(self, State::Done | State::Declined | State::Cancelled | State::Failed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    pub id: TransferId,
    pub direction: Direction,
    /// The other side's root key.
    pub peer: [u8; 32],
    pub state: State,
    pub size: u64,
    pub sha256: [u8; 32],
    /// When it was offered (unix seconds).
    pub created: u64,
    /// The file's name, as shown (already through `sanitise_name`).
    pub name: String,
    /// Outgoing: the file being sent. Incoming: the folder it's saved in
    /// (empty until accepted), and once done, the file itself.
    pub path: PathBuf,
    /// Why it failed, or was cancelled, when it did.
    pub note: String,
    /// Cleared from the list (finished, and Clear or Clear all pressed,
    /// or taken on another of the user's devices). Deleted as soon as
    /// that's safe (see `save`): until then kept, not shown, so the
    /// sender offering it again gets the same answer rather than it
    /// coming back as new.
    pub cleared: bool,
    /// Incoming: the sender knows how it ended (it said so, `FileClosed`,
    /// or ended it itself), so won't offer it again.
    pub settled: bool,
}

pub fn new_id() -> TransferId {
    rand::random()
}

pub fn format_id(id: &TransferId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_id(s: &str) -> Option<TransferId> {
    if s.len() != 32 {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(id)
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn format_line(t: &Transfer) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        format_id(&t.id),
        if t.direction == Direction::Out { "out" } else { "in" },
        hex(&t.peer),
        t.state.as_str(),
        t.size,
        hex(&t.sha256),
        t.created,
        crate::text::single_line(&t.name),
        crate::text::single_line(&t.path.to_string_lossy()),
        crate::text::single_line(&t.note),
        [t.cleared.then_some("cleared"), t.settled.then_some("settled")].into_iter().flatten().collect::<Vec<_>>().join(" "),
    )
}

fn parse_line(line: &str) -> Option<Transfer> {
    let mut f = line.split('\t');
    Some(Transfer {
        id: parse_id(f.next()?)?,
        direction: match f.next()? {
            "out" => Direction::Out,
            "in" => Direction::In,
            _ => return None,
        },
        peer: unhex(f.next()?)?,
        state: State::parse(f.next()?)?,
        size: f.next()?.parse().ok()?,
        sha256: unhex(f.next()?)?,
        created: f.next()?.parse().ok()?,
        name: f.next()?.to_string(),
        path: PathBuf::from(f.next()?),
        note: f.next()?.to_string(),
        cleared: false,
        settled: false,
    })
    .and_then(|mut t| {
        let marks = f.next()?;
        t.cleared = marks.split(' ').any(|m| m == "cleared");
        t.settled = marks.split(' ').any(|m| m == "settled");
        Some(t)
    })
}

/// Every transfer on record, oldest first.
pub fn load(store_path: &Path) -> Vec<Transfer> {
    crate::store::read(store_path).map(|s| s.lines().filter_map(parse_line).collect()).unwrap_or_default()
}

/// The longest a cleared record is kept when the sender never says it
/// knows how it ended (it's gone, say): 30 days.
pub const KEPT: u64 = 30 * 24 * 60 * 60;

/// Whether a cleared record can go now: one this side sent (nobody
/// offers it back), one whose sender knows how it ended, or one past
/// `KEPT`.
fn gone(t: &Transfer, now: u64) -> bool {
    t.cleared && (t.direction == Direction::Out || t.settled || t.created.saturating_add(KEPT) < now)
}

fn save(store_path: &Path, all: &[Transfer]) -> std::io::Result<()> {
    let now = crate::unix_now();
    store::write_atomic(store_path, all.iter().filter(|t| !gone(t, now)).map(format_line).collect::<String>())
}

/// Writes `all` as the whole record (the caller holds `store::lock`).
pub fn save_all(store_path: &Path, all: &[Transfer]) -> std::io::Result<()> {
    save(store_path, all)
}

pub fn get(store_path: &Path, id: &TransferId) -> Option<Transfer> {
    load(store_path).into_iter().find(|t| t.id == *id)
}

/// Records a new transfer. False (and nothing changed) if one with that
/// id is already on record. Keeps at most `MAX_FINISHED_KEPT` finished
/// ones.
pub fn add(store_path: &Path, transfer: Transfer) -> std::io::Result<bool> {
    let _guard = store::lock();
    let mut all = load(store_path);
    if all.iter().any(|t| t.id == transfer.id) {
        return Ok(false);
    }
    all.push(transfer);
    let mut finished = all.iter().filter(|t| t.state.is_finished()).count();
    all.retain(|t| {
        let drop = finished > MAX_FINISHED_KEPT && t.state.is_finished();
        finished -= drop as usize;
        !drop
    });
    save(store_path, &all)?;
    Ok(true)
}

/// Changes transfer `id` with `change`, and returns it as changed — or
/// `None` if there's no such transfer.
pub fn update(store_path: &Path, id: &TransferId, change: impl FnOnce(&mut Transfer)) -> std::io::Result<Option<Transfer>> {
    let _guard = store::lock();
    let mut all = load(store_path);
    let Some(t) = all.iter_mut().find(|t| t.id == *id) else { return Ok(None) };
    change(t);
    let changed = t.clone();
    save(store_path, &all)?;
    Ok(Some(changed))
}

/// Takes transfer `id` off the record (one taken on another of the
/// user's devices, see `node::proxyfile`).
pub fn remove(store_path: &Path, id: &TransferId) -> std::io::Result<()> {
    let _guard = store::lock();
    let mut all = load(store_path);
    let before = all.len();
    all.retain(|t| t.id != *id);
    if all.len() != before {
        save(store_path, &all)?;
    }
    Ok(())
}

/// Why an offer to one of a user's devices was withdrawn: another of
/// theirs took it. Such a row isn't shown on either side (`cleared`).
pub const TAKEN_ELSEWHERE: &str = "accepted on another of your devices";

/// Clears finished transfers `ids` from the list (open ones are left:
/// they're cancelled first). Returns how many were cleared.
pub fn clear(store_path: &Path, ids: &[TransferId]) -> std::io::Result<usize> {
    let _guard = store::lock();
    let mut all = load(store_path);
    let mut n = 0;
    for t in all.iter_mut().filter(|t| ids.contains(&t.id) && t.state.is_finished() && !t.cleared) {
        t.cleared = true;
        n += 1;
    }
    if n > 0 {
        save(store_path, &all)?;
    }
    Ok(n)
}

/// Room to spare beyond the file itself before an offer is taken: a full
/// disk breaks more than this transfer.
pub const SPARE_ROOM: u64 = 64 * 1024 * 1024;

/// The bytes free for this user in the folder at `dir` (on its disk), if
/// the system says.
#[cfg(unix)]
pub fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    Some((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

/// The bytes free for this user in the folder at `dir` (on its disk), if
/// the system says.
#[cfg(windows)]
pub fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free: u64 = 0;
    let ok = unsafe { windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free)
}

#[cfg(not(any(unix, windows)))]
pub fn free_space(_dir: &Path) -> Option<u64> {
    None
}

/// What's still to arrive of the files being received now (from
/// `store_path`), which the disk has to keep room for too.
pub fn still_to_come(store_path: &Path) -> u64 {
    load(store_path).iter().filter(|t| t.direction == Direction::In && t.state == State::Active).map(|t| t.size.saturating_sub(received(t))).fold(0, u64::saturating_add)
}

/// Why a file of `size` bytes won't fit in `dir`, if it won't: the file,
/// what's still to come of the others being received (`store_path`), and
/// `SPARE_ROOM` have to fit in what's free. Fine if the system won't say.
pub fn no_room(store_path: &Path, dir: &Path, size: u64) -> Option<String> {
    let free = free_space(dir)?;
    let needs = size.saturating_add(still_to_come(store_path)).saturating_add(SPARE_ROOM);
    (free < needs).then(|| format!("not enough room in {}: it needs {}, and there's {} free", dir.display(), crate::text::human_size(needs), crate::text::human_size(free)))
}

/// A name a sender offered, made safe to save under: only its last path
/// component, control and Windows-reserved characters replaced, no
/// leading dots (no hidden files, no `..`), no trailing dots or spaces,
/// not a name Windows reserves (`CON`, `NUL`, ...), and not too long.
pub fn sanitise_name(offered: &str) -> String {
    const FORBIDDEN: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    let last = offered.rsplit(['/', '\\']).next().unwrap_or("");
    let mut name: String = last.chars().map(|c| if !crate::text::is_single_line(c.encode_utf8(&mut [0; 4])) || FORBIDDEN.contains(&c) { '_' } else { c }).collect();
    name = name.trim_start_matches('.').trim_end_matches(['.', ' ']).trim().to_string();
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"];
    if RESERVED.contains(&stem.as_str()) {
        name = format!("_{name}");
    }
    if name.len() > MAX_NAME_BYTES {
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_string()).filter(|e| e.len() <= 10);
        let keep = MAX_NAME_BYTES - ext.as_ref().map_or(0, |e| e.len() + 1);
        let mut stem = String::new();
        for c in name.chars() {
            if stem.len() + c.len_utf8() > keep {
                break;
            }
            stem.push(c);
        }
        let stem = stem.trim_end_matches(['.', ' ']);
        name = match ext {
            Some(ext) => format!("{stem}.{ext}"),
            None => stem.to_string(),
        };
    }
    if name.is_empty() { "file".to_string() } else { name }
}

/// Where an incoming transfer's bytes collect until it's complete: a
/// hidden file in the chosen folder, unique to the transfer.
pub fn part_path(t: &Transfer) -> PathBuf {
    t.path.join(format!(".{}.{}.swarm-part", t.name, &format_id(&t.id)[..8]))
}

/// `name` in `dir`, or `name (1).ext`, `name (2).ext`, ... — the first
/// that isn't taken.
pub fn unique_destination(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (1..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).expect("some number is free")
}

/// A file's size and SHA-256, read through once.
pub fn hash_file(path: &Path) -> std::io::Result<(u64, [u8; 32])> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((size, hasher.finalize().into()))
}

/// Up to `len` bytes of `path` from `offset`.
pub fn read_chunk(path: &Path, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len.min(CHUNK)];
    let mut got = 0;
    while got < buf.len() {
        let n = file.read(&mut buf[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    buf.truncate(got);
    Ok(buf)
}

/// How many bytes of incoming `t` are here so far.
pub fn received(t: &Transfer) -> u64 {
    match t.state {
        State::Done => t.size,
        _ if t.path.as_os_str().is_empty() => 0,
        _ => fs::metadata(part_path(t)).map(|m| m.len()).unwrap_or(0),
    }
}

/// Adds `data`, which starts at `offset`, to incoming `t`'s part file —
/// only if it carries on exactly from what's there (anything else is a
/// repeat or out of order, and ignored). Returns how much is there now.
/// Opens it for this once: a transfer being pulled keeps a `PartFile`.
pub fn append(t: &Transfer, offset: u64, data: &[u8]) -> std::io::Result<u64> {
    PartFile::open(t)?.append(t.size, offset, data)
}

/// An incoming transfer's part file, kept open while it's pulled, not
/// opened for each chunk: on Windows every open goes through the virus
/// scanner and any sync software's filters, which for thousands of
/// chunks a second can be what holds a transfer back. Rust opens files
/// sharing them for deleting and renaming, so one held here never stops
/// a cancel deleting it; it's let go as soon as the transfer's no longer
/// pulled here (`node::files`).
pub struct PartFile {
    file: fs::File,
}

impl PartFile {
    pub fn open(t: &Transfer) -> std::io::Result<PartFile> {
        Ok(PartFile { file: fs::OpenOptions::new().create(true).append(true).open(part_path(t))? })
    }

    /// `append`, through the open file. What's there is asked of the file
    /// itself each time (cheap, with it open), not remembered: another
    /// connection may be adding to it too.
    pub fn append(&mut self, size: u64, offset: u64, data: &[u8]) -> std::io::Result<u64> {
        let have = self.file.metadata()?.len();
        if offset != have || have + data.len() as u64 > size {
            return Ok(have);
        }
        self.file.write_all(data)?;
        Ok(have + data.len() as u64)
    }
}

/// An outgoing transfer's file, kept open while it's being sent (see
/// `PartFile` for why), with when its size was last checked against
/// what was offered by its name (`changed`).
pub struct SourceFile {
    file: fs::File,
    checked: std::time::Instant,
}

/// How often a file being sent is looked up by its name again: replaced
/// by another, or cut short, the transfer fails rather than sending
/// something that can't pass the check. Through the open file, its size
/// is checked with every chunk.
const SOURCE_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

impl SourceFile {
    pub fn open(path: &Path) -> std::io::Result<SourceFile> {
        Ok(SourceFile { file: fs::File::open(path)?, checked: std::time::Instant::now() })
    }

    /// Whether the file isn't `size` any more: through the open file, and
    /// now and then by its name too.
    pub fn changed(&mut self, path: &Path, size: u64) -> bool {
        if self.file.metadata().map(|m| m.len()).ok() != Some(size) {
            return true;
        }
        if self.checked.elapsed() >= SOURCE_CHECK_EVERY {
            self.checked = std::time::Instant::now();
            return fs::metadata(path).map(|m| m.len()).ok() != Some(size);
        }
        false
    }

    /// Up to `len` bytes from `offset` (`read_chunk`, through the open file).
    pub fn read(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len.min(CHUNK)];
        let mut got = 0;
        while got < buf.len() {
            let n = read_at(&self.file, &mut buf[got..], offset + got as u64)?;
            if n == 0 {
                break;
            }
            got += n;
        }
        buf.truncate(got);
        Ok(buf)
    }
}

#[cfg(unix)]
fn read_at(file: &fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at(file: &fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

/// Finishes complete incoming `t`: checks its SHA-256 and moves it into
/// place under a name not already taken, returning where. A file that
/// doesn't match is deleted, and the error says so. The SHA-256 of what
/// was written, if it was hashed as it came (`arrived`); otherwise it's
/// read back to hash, which for a big file takes seconds.
pub fn finish(t: &Transfer, arrived: Option<[u8; 32]>) -> anyhow::Result<PathBuf> {
    let part = part_path(t);
    let (size, sha) = match arrived {
        Some(sha) => (fs::metadata(&part)?.len(), sha),
        None => hash_file(&part)?,
    };
    if size != t.size || sha != t.sha256 {
        let _ = fs::remove_file(&part);
        let came = if size == t.size { format!("{size} bytes, as offered") } else { format!("{size} bytes, of {} offered", t.size) };
        anyhow::bail!("the file that arrived doesn't match what was offered (checksum mismatch; {came}) — discarded");
    }
    let _guard = store::lock();
    let destination = unique_destination(&t.path, &t.name);
    fs::rename(&part, &destination)?;
    Ok(destination)
}

/// Removes incoming `t`'s part file, if it has one.
pub fn discard_part(t: &Transfer) {
    if t.direction == Direction::In && !t.path.as_os_str().is_empty() && t.state != State::Done {
        let _ = fs::remove_file(part_path(t));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-transfer-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn offered_names_cant_escape_hide_or_impersonate() {
        assert_eq!(sanitise_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitise_name("C:\\Windows\\evil.exe"), "evil.exe");
        assert_eq!(sanitise_name(".bashrc"), "bashrc");
        assert_eq!(sanitise_name(".."), "file");
        assert_eq!(sanitise_name("a\nb\tc.txt"), "a_b_c.txt");
        assert_eq!(sanitise_name("what?.txt"), "what_.txt");
        assert_eq!(sanitise_name("CON.txt"), "_CON.txt");
        assert_eq!(sanitise_name("trailing. . "), "trailing");
        let long = sanitise_name(&format!("{}.pdf", "x".repeat(500)));
        assert!(long.len() <= MAX_NAME_BYTES && long.ends_with(".pdf"));
        assert_eq!(sanitise_name("con .txt"), "_con .txt");
        assert_eq!(sanitise_name("photo\u{202E}gpj.exe"), "photo_gpj.exe", "can't pretend to be a .jpg");
        assert_eq!(sanitise_name("holiday photo.jpg"), "holiday photo.jpg");
    }

    /// File names are limited in bytes (255 on most filesystems), not
    /// characters: a long name in Japanese or Greek, offered whole, still
    /// has to be savable, part file and " (1)" included.
    #[test]
    fn a_long_name_in_any_script_can_still_be_saved() {
        let dir = scratch("longname");
        for offered in [format!("{}.pdf", "日本語".repeat(40)), format!("{}.txt", "Ελληνικά".repeat(30)), "😀".repeat(100)] {
            let name = sanitise_name(&offered);
            assert!(name.chars().count() > 10, "kept as much as fits: {name}");
            let t = Transfer {
                id: [3; 16],
                direction: Direction::In,
                peer: [2; 32],
                state: State::Active,
                size: 1,
                sha256: [0; 32],
                created: 0,
                name: name.clone(),
                path: dir.clone(),
                note: String::new(),
                cleared: false,
                settled: false,
            };
            fs::write(part_path(&t), "x").unwrap_or_else(|e| panic!("part file for {name}: {e}"));
            fs::write(unique_destination(&dir, &name), "x").unwrap();
            fs::write(unique_destination(&dir, &name), "x").unwrap_or_else(|e| panic!("second copy of {name}: {e}"));
        }
        assert!(sanitise_name(&format!("{}.pdf", "日本語".repeat(40))).ends_with(".pdf"));
    }

    /// Finished transfers don't pile up for ever: past
    /// `MAX_FINISHED_KEPT`, the oldest finished ones go, while every
    /// unfinished one stays. (A peer offering and cancelling over and
    /// over would otherwise grow the store without end.)
    #[test]
    fn the_store_keeps_every_open_transfer_and_only_the_newest_finished() {
        let store = scratch("prune").join("transfers");
        let t = |i: u32, state: State| Transfer {
            id: { let mut id = [0; 16]; id[..4].copy_from_slice(&i.to_be_bytes()); id },
            direction: Direction::In,
            peer: [2; 32],
            state,
            size: 1,
            sha256: [0; 32],
            created: i as u64,
            name: "x".into(),
            path: PathBuf::new(),
            note: String::new(),
            cleared: false,
            settled: false,
        };
        add(&store, t(0, State::Offered)).unwrap();
        for i in 1..=(MAX_FINISHED_KEPT as u32 + 50) {
            add(&store, t(i, State::Cancelled)).unwrap();
        }
        let all = load(&store);
        assert!(all.iter().any(|x| x.state == State::Offered), "an open one is never dropped");
        assert_eq!(all.iter().filter(|x| x.state.is_finished()).count(), MAX_FINISHED_KEPT);
        assert!(all.iter().any(|x| x.created == MAX_FINISHED_KEPT as u64 + 50), "the newest are kept");
    }

    /// Clear takes finished ones off the list, never an open one. One
    /// sent goes at once; one received goes once its sender's said it
    /// knows how it ended (`settled`), or `KEPT` after it was offered.
    #[test]
    fn clearing_deletes_what_nobody_will_offer_again() {
        let store = scratch("clear").join("transfers");
        let now = crate::unix_now();
        let t = |i: u8, direction: Direction, state: State, created: u64, settled: bool| Transfer {
            id: [i; 16],
            direction,
            peer: [2; 32],
            state,
            size: 1,
            sha256: [0; 32],
            created,
            name: "x".into(),
            path: PathBuf::new(),
            note: "a\tnote".into(),
            cleared: false,
            settled,
        };
        add(&store, t(1, Direction::Out, State::Done, now, false)).unwrap();
        add(&store, t(2, Direction::In, State::Active, now, false)).unwrap();
        add(&store, t(3, Direction::In, State::Done, now, false)).unwrap();
        add(&store, t(4, Direction::In, State::Declined, now, true)).unwrap();
        add(&store, t(5, Direction::In, State::Done, now - KEPT - 1, false)).unwrap();
        let ids: Vec<TransferId> = (1..=5).map(|i| [i; 16]).collect();
        assert_eq!(clear(&store, &ids).unwrap(), 4, "all but the open one");
        let left: Vec<_> = load(&store).into_iter().map(|x| (x.id[0], x.cleared)).collect();
        assert_eq!(left, vec![(2, false), (3, true)], "sent, settled and old ones gone; the unsettled one kept, hidden");
        update(&store, &[3; 16], |x| x.settled = true).unwrap();
        assert_eq!(load(&store).len(), 1, "gone once its sender knows");
        let line = format_line(&Transfer { cleared: true, settled: true, ..t(6, Direction::In, State::Done, now, false) });
        let back = parse_line(line.trim_end_matches('\n')).unwrap();
        assert!(back.cleared && back.settled);
    }

    /// Several files of one name finishing at the same moment (from
    /// different peers, received without asking, say) each get a name
    /// of their own; none overwrites another.
    #[test]
    fn files_of_one_name_finishing_together_all_survive() {
        let dir = scratch("together");
        let n = 16u8;
        let transfers: Vec<Transfer> = (0..n)
            .map(|i| {
                let data = vec![i; 100];
                let t = Transfer {
                    id: [i; 16],
                    direction: Direction::In,
                    peer: [2; 32],
                    state: State::Active,
                    size: 100,
                    sha256: Sha256::digest(&data).into(),
                    created: 0,
                    name: "photo.jpg".into(),
                    path: dir.clone(),
                    note: String::new(),
                    cleared: false,
                    settled: false,
                };
                fs::write(part_path(&t), &data).unwrap();
                t
            })
            .collect();
        let barrier = std::sync::Barrier::new(n as usize);
        std::thread::scope(|scope| {
            for t in &transfers {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    finish(t, None).unwrap();
                });
            }
        });
        let mut contents: Vec<u8> = fs::read_dir(&dir).unwrap().flatten().map(|e| fs::read(e.path()).unwrap()[0]).collect();
        contents.sort();
        assert_eq!(contents, (0..n).collect::<Vec<u8>>(), "every file kept, once each");
    }

    #[test]
    fn a_taken_name_gets_a_number_not_overwritten() {
        let dir = scratch("unique");
        assert_eq!(unique_destination(&dir, "a.txt"), dir.join("a.txt"));
        fs::write(dir.join("a.txt"), "x").unwrap();
        fs::write(dir.join("a (1).txt"), "x").unwrap();
        assert_eq!(unique_destination(&dir, "a.txt"), dir.join("a (2).txt"));
        fs::write(dir.join("noext"), "x").unwrap();
        assert_eq!(unique_destination(&dir, "noext"), dir.join("noext (1)"));
    }

    #[test]
    fn a_file_moves_chunk_by_chunk_resumes_and_checks_out() {
        let dir = scratch("move");
        let source = dir.join("source.bin");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7 % 251) as u8).collect();
        fs::write(&source, &data).unwrap();
        let (size, sha256) = hash_file(&source).unwrap();
        let downloads = dir.join("downloads");
        fs::create_dir_all(&downloads).unwrap();
        let t = Transfer {
            id: [1; 16],
            direction: Direction::In,
            peer: [2; 32],
            state: State::Active,
            size,
            sha256,
            created: 0,
            name: "source.bin".into(),
            path: downloads.clone(),
            note: String::new(),
            cleared: false,
            settled: false,
        };
        let mut offset = 0;
        while offset < size {
            let chunk = read_chunk(&source, offset, CHUNK).unwrap();
            if offset > 0 {
                assert_eq!(append(&t, 0, &chunk).unwrap(), offset);
            }
            offset = append(&t, offset, &chunk).unwrap();
            assert_eq!(received(&t), offset);
        }
        let placed = finish(&t, None).unwrap();
        assert_eq!(placed, downloads.join("source.bin"));
        assert_eq!(fs::read(&placed).unwrap(), data);
        assert!(!part_path(&t).exists());
    }

    /// As above, into the folder `SWARM_SAVE_IN` names: on the phone, its
    /// Download folder (Android's shared storage, through its FUSE
    /// filesystem), where files accepted there go.
    #[test]
    #[ignore]
    fn a_file_moves_chunk_by_chunk_into_the_folder_given() {
        let downloads = PathBuf::from(std::env::var_os("SWARM_SAVE_IN").expect("SWARM_SAVE_IN"));
        let data: Vec<u8> = (0..334_616u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let sha256: [u8; 32] = Sha256::digest(&data).into();
        let t = Transfer {
            id: [9; 16],
            direction: Direction::In,
            peer: [2; 32],
            state: State::Active,
            size: data.len() as u64,
            sha256,
            created: 0,
            name: "swarm-test.bin".into(),
            path: downloads.clone(),
            note: String::new(),
            cleared: false,
            settled: false,
        };
        discard_part(&t);
        let mut offset = 0u64;
        while offset < t.size {
            let end = (offset as usize + CHUNK).min(data.len());
            let chunk = &data[offset as usize..end];
            let now = append(&t, offset, chunk).unwrap();
            assert_eq!(now, end as u64, "after the chunk at {offset}");
            assert_eq!(received(&t), now, "what's there, after the chunk at {offset}");
            offset = now;
        }
        let placed = finish(&t, None).unwrap_or_else(|e| {
            let there = fs::read(part_path(&t)).unwrap_or_default();
            let first = there.iter().zip(&data).position(|(a, b)| a != b);
            panic!("{e:#}: {} bytes there, of {}; first differing at {first:?}", there.len(), data.len())
        });
        assert_eq!(fs::read(&placed).unwrap(), data);
        fs::remove_file(&placed).unwrap();
    }

    /// A file hashed as it came is checked against that, not read back:
    /// reading a big one back took seconds (7 for 2 GB), its peer's
    /// connection unread all the while. (Unreadable here, to show it.)
    #[cfg(unix)]
    #[test]
    fn a_file_hashed_as_it_came_isnt_read_back() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("hashed");
        let t = Transfer {
            id: [4; 16],
            direction: Direction::In,
            peer: [2; 32],
            state: State::Active,
            size: 5,
            sha256: Sha256::digest(b"hello").into(),
            created: 0,
            name: "x.txt".into(),
            path: dir.clone(),
            note: String::new(),
            cleared: false,
            settled: false,
        };
        append(&t, 0, b"hello").unwrap();
        fs::set_permissions(part_path(&t), fs::Permissions::from_mode(0o000)).unwrap();
        let placed = finish(&t, Some(t.sha256)).expect("checked against what came");
        fs::set_permissions(&placed, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(fs::read(&placed).unwrap(), b"hello");
        append(&t, 0, b"hullo").unwrap();
        let why = format!("{:#}", finish(&t, Some(Sha256::digest(b"hullo").into())).unwrap_err());
        assert!(why.contains("checksum mismatch"), "{why}");
        assert!(!part_path(&t).exists());
    }

    #[test]
    fn a_file_that_doesnt_match_its_checksum_is_discarded() {
        let dir = scratch("mismatch");
        let t = Transfer {
            id: [3; 16],
            direction: Direction::In,
            peer: [2; 32],
            state: State::Active,
            size: 5,
            sha256: [0; 32],
            created: 0,
            name: "x.txt".into(),
            path: dir.clone(),
            note: String::new(),
            cleared: false,
            settled: false,
        };
        append(&t, 0, b"hello").unwrap();
        let why = format!("{:#}", finish(&t, None).unwrap_err());
        assert!(why.contains("checksum mismatch") && why.contains("5 bytes, as offered"), "{why}");
        assert!(!part_path(&t).exists());
        assert!(!dir.join("x.txt").exists());
        fs::write(part_path(&t), b"hello!!").unwrap();
        let why = format!("{:#}", finish(&t, None).unwrap_err());
        assert!(why.contains("7 bytes, of 5 offered"), "{why}");
        assert!(!part_path(&t).exists());
    }

    #[test]
    fn the_store_round_trips_and_updates() {
        let dir = scratch("store");
        let store = dir.join("transfers");
        let t = Transfer {
            id: [4; 16],
            direction: Direction::Out,
            peer: [5; 32],
            state: State::Offered,
            size: 99,
            sha256: [6; 32],
            created: 1234,
            name: "report.pdf".into(),
            path: PathBuf::from("/home/me/report.pdf"),
            note: String::new(),
            cleared: false,
            settled: false,
        };
        assert!(add(&store, t.clone()).unwrap());
        assert!(!add(&store, t.clone()).unwrap());
        assert_eq!(load(&store), vec![t.clone()]);
        let changed = update(&store, &t.id, |t| t.state = State::Active).unwrap().unwrap();
        assert_eq!(changed.state, State::Active);
        assert_eq!(get(&store, &t.id).unwrap().state, State::Active);
    }

    #[test]
    fn no_room_counts_the_file_and_what_else_is_coming() {
        let dir = std::env::temp_dir().join(format!("swarm-transfer-room-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let store = dir.join("transfers");
        let free = free_space(&dir).expect("the system says");
        assert!(no_room(&store, &dir, 1000).is_none());
        assert!(no_room(&store, &dir, free).is_some(), "the spare room counts too");
        let coming = Transfer {
            id: [3; 16],
            direction: Direction::In,
            peer: [2; 32],
            state: State::Active,
            size: free,
            sha256: [0; 32],
            created: 0,
            name: "big.bin".into(),
            path: dir.clone(),
            note: String::new(),
            cleared: false,
            settled: false,
        };
        add(&store, coming.clone()).unwrap();
        assert!(no_room(&store, &dir, 1000).is_some());
        add(&store, Transfer { size: u64::MAX, ..coming.clone() }).unwrap();
        add(&store, Transfer { id: [4; 16], size: u64::MAX, ..coming }).unwrap();
        assert_eq!(still_to_come(&store), u64::MAX);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A file being sent, kept open: read as `read_chunk` reads it, and
    /// seen to have changed (cut short, or replaced by its name) as soon
    /// as it has.
    #[test]
    fn a_file_kept_open_reads_as_it_would_opened_each_time_and_sees_a_change() {
        let dir = scratch("source-open");
        let path = dir.join("source.bin");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 13) as u8).collect();
        fs::write(&path, &data).unwrap();
        let mut source = SourceFile::open(&path).unwrap();
        for (offset, len) in [(0, CHUNK), (4000, CHUNK), (8000, CHUNK), (9999, 10), (20_000, CHUNK)] {
            assert_eq!(source.read(offset, len).unwrap(), read_chunk(&path, offset, len).unwrap(), "at {offset}");
        }
        assert!(!source.changed(&path, data.len() as u64));
        fs::write(&path, &data[..5000]).unwrap();
        assert!(source.changed(&path, data.len() as u64), "cut short, seen through the open file");
        fs::write(&path, &data).unwrap();
        let mut source = SourceFile::open(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, &data[..100]).unwrap();
        source.checked -= SOURCE_CHECK_EVERY;
        assert!(source.changed(&path, data.len() as u64), "replaced: seen by its name");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A part file kept open while it's pulled takes chunks as `append`
    /// would, even with another writer adding to it meanwhile (a relayed
    /// pull), and can be deleted while it's open (a cancel).
    #[test]
    fn a_part_file_kept_open_appends_as_append_does_and_can_be_cancelled() {
        let dir = scratch("part-open");
        let t = Transfer { id: [5; 16], direction: Direction::In, peer: [2; 32], state: State::Active, size: 12, sha256: [0; 32], created: 0, name: "p.bin".into(), path: dir.clone(), note: String::new(), cleared: false, settled: false };
        let mut part = PartFile::open(&t).unwrap();
        assert_eq!(part.append(t.size, 0, b"abcd").unwrap(), 4);
        assert_eq!(part.append(t.size, 0, b"abcd").unwrap(), 4, "a repeat is ignored");
        assert_eq!(append(&t, 4, b"efgh").unwrap(), 8);
        assert_eq!(part.append(t.size, 4, b"efgh").unwrap(), 8, "what's there is asked of the file, not remembered");
        assert_eq!(part.append(t.size, 8, b"ijkl").unwrap(), 12);
        assert_eq!(fs::read(part_path(&t)).unwrap(), b"abcdefghijkl");
        assert_eq!(part.append(t.size, 12, b"more").unwrap(), 12, "nothing past its size");
        discard_part(&t);
        assert!(!part_path(&t).exists(), "deleted while open");
        drop(part);
        fs::remove_dir_all(&dir).unwrap();
    }
}
