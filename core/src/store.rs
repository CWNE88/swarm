//! Shared helpers for the flat files under `~/.swarm`: replacing a
//! file so a crash can never leave it half-written, and one lock that
//! every read-modify-write of those files takes, so two tasks in the
//! same process (the outbox flush and a new message being queued, say)
//! can't interleave and lose each other's change.
//!
//! **Everything is sealed.** A data folder is a *vault*, opened with
//! that identity's storage keys (`open_vault`, done when it's unlocked).
//! Each line written goes to disk as `base64(nonce ‖ sealed line)`:
//! XChaCha20-Poly1305 with a fresh random nonce, the file's path within
//! the vault bound in as associated data, so a line moved to another
//! file doesn't open. Reads (`read`, `Seen`, `Kept`) open each line and
//! skip any that don't. A folder with no vault open can't be read or
//! written at all: there's no unsealed mode.
//!
//! **Names are hidden** where they'd say who: a file or folder named for
//! a peer, account or channel is named by `named` instead, an HMAC of
//! its real name under the vault's name key. Its real name is kept
//! inside, sealed — as a first line in a file, or in `.name` in a
//! folder — so `names_in` can list them.

use crate::lock::{Locked, Shared};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use zeroize::Zeroizing;

static LOCK: Mutex<()> = Mutex::new(());

/// One data folder's keys: what seals its lines, and what names its
/// files.
pub struct Keys {
    pub seal: Zeroizing<[u8; 32]>,
    pub names: Zeroizing<[u8; 32]>,
}

struct Vault {
    dir: PathBuf,
    cipher: XChaCha20Poly1305,
    names: Zeroizing<[u8; 32]>,
}

static VAULTS: RwLock<Vec<Arc<Vault>>> = RwLock::new(Vec::new());

/// The real names of files and folders `named` here, by their path.
static NAMES: Mutex<Option<HashMap<PathBuf, String>>> = Mutex::new(None);

/// Opens `dir` as a vault: from now on everything under it is sealed
/// and opened with `keys`. Opening it again (a second node for the same
/// folder, say) replaces the keys.
pub fn open_vault(dir: &Path, keys: &Keys) {
    let dir = absolute(dir);
    let vault = Arc::new(Vault { dir: dir.clone(), cipher: XChaCha20Poly1305::new((&*keys.seal).into()), names: keys.names.clone() });
    crate::secure::keep_in_memory(&*vault);
    let mut vaults = VAULTS.writing();
    vaults.retain(|v| v.dir != dir);
    vaults.push(vault);
}

/// The format of what this build keeps in a vault: the files there and
/// what a line of each holds. It goes up when a change needs what's
/// already kept converted (`MIGRATIONS`), and is kept in the vault, as
/// sealed as everything else, in `format` (see `check_format`).
pub const FORMAT: u32 = 1;

/// What takes a vault from each format to the next: entry `n` from
/// format `n + 1` to `n + 2`. None yet.
const MIGRATIONS: &[fn(&Path) -> std::io::Result<()>] = &[];

/// Checks the format of `dir`'s vault, just opened: one made by a newer
/// build is refused (this one would misread it), an older one is
/// brought up to date (`MIGRATIONS`), and a new one is marked as this
/// build's.
pub fn check_format(dir: &Path) -> anyhow::Result<()> {
    const _: () = assert!(MIGRATIONS.len() + 1 == FORMAT as usize, "a migration for every format before this one");
    let path = dir.join("format");
    let found = match read(&path) {
        Ok(text) => match text.trim().parse::<u32>() {
            Ok(n) if n >= 1 => n,
            _ => anyhow::bail!("{} can't be read: it may be damaged", path.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 1,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
    };
    if found > FORMAT {
        anyhow::bail!("this data was saved by a newer Swarm (format {found}; this one reads up to {FORMAT}): update Swarm to open it");
    }
    let _guard = lock();
    for (n, migrate) in MIGRATIONS.iter().enumerate().skip(found as usize - 1) {
        migrate(dir).map_err(|e| anyhow::Error::new(e).context(format!("bringing the data up to format {}", n + 2)))?;
        write_atomic(&path, format!("{}\n", n + 2))?;
    }
    if read(&path).is_err() {
        write_atomic(&path, format!("{FORMAT}\n"))?;
    }
    Ok(())
}

/// Closes `dir`'s vault, if it's open: nothing under it can be read or
/// written until it's opened again.
pub fn close_vault(dir: &Path) {
    let dir = absolute(dir);
    VAULTS.writing().retain(|v| v.dir != dir);
    OPENED.locked().files.retain(|(p, _, _, _)| !p.starts_with(&dir));
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The vault `path` is in (the deepest, should one be inside another),
/// and the path within it, `/`-separated: what's bound into each seal.
fn vault_for(path: &Path) -> std::io::Result<(Arc<Vault>, String)> {
    let path = absolute(path);
    let vaults = VAULTS.reading();
    let found = vaults.iter().filter(|v| path.starts_with(&v.dir)).max_by_key(|v| v.dir.components().count()).cloned();
    #[cfg(test)]
    let found = found.or_else(|| {
        drop(vaults);
        let root = absolute(&std::env::temp_dir());
        if !path.starts_with(&root) {
            return None;
        }
        open_vault(&root, &Keys { seal: Zeroizing::new([7; 32]), names: Zeroizing::new([8; 32]) });
        VAULTS.reading().iter().find(|v| v.dir == root).cloned()
    });
    let Some(vault) = found else {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("{} is locked", path.display())));
    };
    let within = path.strip_prefix(&vault.dir).unwrap_or(&path).components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
    Ok((vault, within))
}

/// Whether `path` is in an open vault.
pub fn is_open(path: &Path) -> bool {
    vault_for(path).is_ok()
}

fn seal_line(vault: &Vault, within: &str, line: &str) -> String {
    let mut nonce = [0u8; 24];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
    let sealed = vault
        .cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: line.as_bytes(), aad: within.as_bytes() })
        .expect("sealing can't fail for a line this size");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    STANDARD.encode(out)
}

fn open_line(vault: &Vault, within: &str, line: &str) -> Option<String> {
    let bytes = STANDARD.decode(line.trim_end_matches('\r')).ok()?;
    if bytes.len() < 24 {
        return None;
    }
    let (nonce, sealed) = bytes.split_at(24);
    let opened = vault.cipher.decrypt(XNonce::from_slice(nonce), Payload { msg: sealed, aad: within.as_bytes() }).ok()?;
    String::from_utf8(opened).ok()
}

/// `line` as it'd be sealed in `path`, without its newline: for tests
/// that write a file part by part, as a crash might leave it.
#[cfg(test)]
pub(crate) fn sealed_line(path: &Path, line: &str) -> String {
    let (vault, within) = vault_for(path).unwrap();
    seal_line(&vault, &within, line)
}

/// A line that holds a file's real name (see `named`), not a record.
const NAME_MARK: char = '\0';

/// Seals `text` line by line for `path`.
fn seal_text(path: &Path, text: &str) -> std::io::Result<String> {
    let (vault, within) = vault_for(path)?;
    let mut out = String::new();
    if let Some(name) = name_of(path) {
        out.push_str(&seal_line(&vault, &within, &format!("{NAME_MARK}{name}")));
        out.push('\n');
    }
    for line in text.lines() {
        out.push_str(&seal_line(&vault, &within, line));
        out.push('\n');
    }
    Ok(out)
}

/// Opens sealed `text` from `path`: the records, one a line, each
/// newline-ended. A line that doesn't open (damaged, or moved from
/// another file) is left out, as is a file's name.
fn open_text(path: &Path, text: &str) -> std::io::Result<String> {
    let (vault, within) = vault_for(path)?;
    let mut out = String::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        if let Some(opened) = open_line(&vault, &within, line).filter(|l| !l.starts_with(NAME_MARK)) {
            out.push_str(&opened);
            out.push('\n');
        }
    }
    Ok(out)
}

/// `path`'s records, opened: what `fs::read_to_string` would have given
/// if the file weren't sealed. A missing file is `NotFound`, as ever.
///
/// A small file's opened again only when it's changed (see `Opened`):
/// the same few files are read every second or two (your devices, the
/// peers book, settings), and opening them each time kept a phone with
/// nothing to do at a tenth of a core.
pub fn read(path: impl AsRef<Path>) -> std::io::Result<String> {
    use std::io::Read;
    let path = path.as_ref();
    let (vault, _) = vault_for(path)?;
    let mut file = fs::File::open(path)?;
    let stamp = Stamp::of(&file.metadata()?);
    let key = absolute(path);
    if let Some(text) = OPENED.locked().get(&key, &vault, &stamp) {
        return Ok(text);
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let opened = open_text(path, &text)?;
    if text.len() == stamp.len as usize {
        OPENED.locked().put(key, &vault, stamp, &opened);
    }
    Ok(opened)
}

/// What says a file's the same as when it was last read: the file itself
/// (a rewrite puts a new one in its place), its length (an append) and
/// when it was last changed. This process's own writes forget it anyway
/// (`Opened::forget`), so only another process's rewrite to the same
/// length, in the same file and in the same instant, could go unseen.
#[derive(Clone, PartialEq)]
struct Stamp {
    file: u64,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Stamp {
        Stamp { file: file_id(meta), len: meta.len(), modified: meta.modified().ok() }
    }
}

/// Small files' opened text, kept between reads (see `read`): by path,
/// with the vault it was opened by (closed or opened afresh, it's not
/// used again) and the file's `Stamp`.
struct Opened {
    files: Vec<(PathBuf, std::sync::Weak<Vault>, Stamp, Zeroizing<String>)>,
}

/// Larger files aren't kept: the ones read over and over are small, the
/// largest a profile with its picture (about 140 KB).
const OPENED_LARGEST: usize = 256 * 1024;
/// Nor more than this altogether, the longest unread going first.
const OPENED_MOST: usize = 4 * 1024 * 1024;

static OPENED: Mutex<Opened> = Mutex::new(Opened { files: Vec::new() });

impl Opened {
    fn get(&mut self, path: &Path, vault: &Arc<Vault>, stamp: &Stamp) -> Option<String> {
        let i = self.files.iter().position(|(p, _, _, _)| p == path)?;
        let (_, by, kept, _) = &self.files[i];
        if by.as_ptr() != Arc::as_ptr(vault) || kept != stamp {
            return None;
        }
        let entry = self.files.remove(i);
        let text = String::clone(&entry.3);
        self.files.push(entry);
        Some(text)
    }

    fn put(&mut self, path: PathBuf, vault: &Arc<Vault>, stamp: Stamp, text: &str) {
        self.files.retain(|(p, by, _, _)| *p != path && by.strong_count() > 0);
        if text.len() > OPENED_LARGEST {
            return;
        }
        let mut kept: usize = self.files.iter().map(|(_, _, _, t)| t.len()).sum::<usize>() + text.len();
        while kept > OPENED_MOST && !self.files.is_empty() {
            kept -= self.files.remove(0).3.len();
        }
        self.files.push((path, Arc::downgrade(vault), stamp, Zeroizing::new(text.to_string())));
    }

    /// `path`'s written here: read afresh next time.
    fn forget(path: &Path) {
        let path = absolute(path);
        OPENED.locked().files.retain(|(p, _, _, _)| *p != path);
    }
}

/// Where a file or folder named `name` in `dir` goes: the name hidden
/// (an HMAC of where it is and what it's called, so the same peer's
/// files in two folders don't share a name), the real one kept inside.
pub fn named(dir: &Path, name: &str) -> PathBuf {
    use hmac::{Hmac, Mac};
    let Ok((vault, within)) = vault_for(&dir.join("_")) else {
        return dir.join(".locked");
    };
    let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(&*vault.names).expect("HMAC takes any key length");
    mac.update(within.as_bytes());
    mac.update(b"\0");
    mac.update(name.as_bytes());
    let hidden: String = mac.finalize().into_bytes()[..16].iter().map(|b| format!("{b:02x}")).collect();
    let path = dir.join(hidden);
    NAMES.locked().get_or_insert_with(HashMap::new).insert(absolute(&path), name.to_string());
    path
}

fn name_of(path: &Path) -> Option<String> {
    NAMES.locked().as_ref()?.get(&absolute(path)).cloned()
}

/// A folder `named` gets its real name in `.name` inside it, written
/// with the first file put there.
fn name_folder(path: &Path) -> std::io::Result<()> {
    let Some(dir) = path.parent() else { return Ok(()) };
    let Some(name) = name_of(dir) else { return Ok(()) };
    let marker = dir.join(".name");
    if marker.exists() {
        return Ok(());
    }
    let (vault, within) = vault_for(&marker)?;
    fs::create_dir_all(dir)?;
    fs::write(marker, seal_line(&vault, &within, &format!("{NAME_MARK}{name}")) + "\n")
}

/// Everything `named` in `dir`: each one's real name and path. Anything
/// whose name doesn't open is left out.
pub fn names_in(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let holder = if path.is_dir() { path.join(".name") } else { path.clone() };
        let Ok(text) = fs::read_to_string(&holder) else { continue };
        let Ok((vault, within)) = vault_for(&holder) else { continue };
        let Some(first) = text.lines().next() else { continue };
        let Some(name) = open_line(&vault, &within, first).and_then(|l| l.strip_prefix(NAME_MARK).map(str::to_string)) else { continue };
        NAMES.locked().get_or_insert_with(HashMap::new).insert(absolute(&path), name.clone());
        found.push((name, path));
    }
    found.sort();
    found
}

/// Held around any read-modify-write of a store file. One lock for all
/// of them: these are small, rare writes, so there's nothing to gain
/// from finer locking, and one lock can't deadlock against another.
/// Never take it twice on one path — it isn't reentrant.
pub fn lock() -> MutexGuard<'static, ()> {
    LOCK.locked()
}

/// Replaces `path` with `contents` all at once: written to a temporary
/// file beside it, flushed to disk, then renamed over it. A reader (or
/// a crash) sees either the old file or the new one, never a partial
/// write — `fs::write` truncates first, so a crash part-way through
/// would lose the whole file.
pub fn write_atomic(path: &Path, contents: impl AsRef<str>) -> std::io::Result<()> {
    let sealed = seal_text(path, contents.as_ref())?;
    name_folder(path)?;
    let contents = sealed.as_bytes();
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{file_name}.tmp{}-{n}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents.as_ref())?;
        file.sync_all()?;
    }
    let renamed = fs::rename(&tmp, path);
    Opened::forget(path);
    #[cfg(unix)]
    if renamed.is_ok()
        && let Ok(folder) = fs::File::open(dir)
    {
        let _ = folder.sync_all();
    }
    renamed
}

/// Appends one line to `path`, creating it (and its directory) if
/// needed — on a line of its own even if the file was left without a
/// final newline (edited by hand, say). Appends of a single short line
/// are atomic on the platforms this runs on, so two appends can't mix —
/// but a file that's also read and replaced (`write_atomic`, under
/// `lock`) loses an append made between its read and its replacing, so
/// whoever appends to such a file holds `lock` too.
pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let (vault, within) = vault_for(path)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    name_folder(path)?;
    let mut file = fs::OpenOptions::new().create(true).read(true).append(true).open(path)?;
    let mut last = *b"\n";
    let mut sealed = String::new();
    if file.seek(SeekFrom::End(0))? > 0 {
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
    } else if let Some(name) = name_of(path) {
        sealed.push_str(&seal_line(&vault, &within, &format!("{NAME_MARK}{name}")));
        sealed.push('\n');
    }
    sealed.push_str(&seal_line(&vault, &within, line));
    let lead = if last[0] == b'\n' { "" } else { "\n" };
    let written = file.write_all(format!("{lead}{sealed}\n").as_bytes());
    Opened::forget(path);
    written
}

/// How far into a line-per-record file has been read: up to the end of its last
/// whole line, and the bytes just before that point — still the same
/// means the file has only been added to since, so only the rest needs
/// reading. Anything else (it was rewritten) means reading it all again.
/// The file itself, which a rewrite (`write_atomic` puts a new one in
/// its place) changes: its inode on Unix, when it was made on Windows.
#[derive(Default)]
pub(crate) struct Seen {
    len: u64,
    tail: Vec<u8>,
    file: u64,
}

fn file_id(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::ino(meta)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::MetadataExt::creation_time(meta)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = meta;
        0
    }
}

pub(crate) enum More {
    /// Lines added since.
    After(String),
    /// The whole file, read afresh.
    Whole(String),
}

impl Seen {
    /// What's new in `path` since last time: `None` if nothing is.
    pub(crate) fn read_on(&mut self, path: &Path) -> std::io::Result<Option<More>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let was = self.len;
                *self = Seen::default();
                return Ok((was > 0).then(|| More::Whole(String::new())));
            }
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        let (len, id) = (meta.len(), file_id(&meta));
        let tail_from = self.len - self.tail.len() as u64;
        let unchanged_before = len >= self.len && (self.len == 0 || id == self.file) && {
            let mut tail = vec![0; self.tail.len()];
            file.seek(SeekFrom::Start(tail_from))?;
            file.read_exact(&mut tail).is_ok() && tail == self.tail
        };
        if unchanged_before && len == self.len {
            return Ok(None);
        }
        let start = if unchanged_before { self.len } else { 0 };
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(len - start).read_to_end(&mut bytes)?;
        let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        bytes.truncate(whole);
        self.len = start + whole as u64;
        self.file = id;
        let mut tail = if unchanged_before { std::mem::take(&mut self.tail) } else { Vec::new() };
        tail.extend_from_slice(&bytes);
        self.tail = tail[tail.len().saturating_sub(64)..].to_vec();
        let text = open_text(path, &String::from_utf8_lossy(&bytes))?;
        Ok(Some(if unchanged_before { More::After(text) } else { More::Whole(text) }))
    }
}

/// Files' records kept in memory between reads, read on from where they
/// got to (see `Seen`): for files asked for over and over — on every
/// redraw — that mostly only grow.
pub(crate) struct Kept<T> {
    files: Mutex<Vec<(std::path::PathBuf, Seen, Vec<T>)>>,
    most: usize,
}

impl<T: Clone> Kept<T> {
    /// Keeping at most `most` files.
    pub(crate) const fn new(most: usize) -> Self {
        Kept { files: Mutex::new(Vec::new()), most }
    }

    /// `path`'s records (none if there's no file), each line read by
    /// `parse` (a line that doesn't parse is skipped).
    pub(crate) fn read(&self, path: &Path, parse: impl Fn(&str) -> Option<T>) -> std::io::Result<Vec<T>> {
        self.with(path, parse, <[T]>::to_vec)
    }

    /// `f` of `path`'s records, without a copy of them all: for whoever
    /// needs only a few, or only to look. A copy of a long file made over
    /// and over (on whichever thread was free) left a copy's worth of
    /// memory with each thread for good: 100,000 messages, 100 MB.
    pub(crate) fn with<R>(&self, path: &Path, parse: impl Fn(&str) -> Option<T>, f: impl FnOnce(&[T]) -> R) -> std::io::Result<R> {
        let mut files = self.files.locked();
        let (mut seen, mut records) = match files.iter().position(|(p, _, _)| p == path) {
            Some(i) => {
                let (_, seen, records) = files.remove(i);
                (seen, records)
            }
            None => (Seen::default(), Vec::new()),
        };
        match seen.read_on(path)? {
            Some(More::After(text)) => records.extend(text.lines().filter_map(&parse)),
            Some(More::Whole(text)) => records = text.lines().filter_map(&parse).collect(),
            None => {}
        }
        let out = f(&records);
        files.push((path.to_path_buf(), seen, records));
        if files.len() > self.most {
            files.remove(0);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rewrite the same length and ending the same way (a post merged
    /// into the middle, the oldest dropped) is still noticed.
    #[test]
    fn kept_records_notice_a_rewrite_of_the_same_length_and_ending() {
        let dir = std::env::temp_dir().join(format!("swarm-store-kept-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("f");
        let kept: Kept<String> = Kept::new(2);
        let line = |c: char| format!("{}\n", c.to_string().repeat(99));
        write_atomic(&path, line('a') + &line('z')).unwrap();
        let read = || kept.read(&path, |l| Some(l.chars().next().unwrap().to_string())).unwrap();
        assert_eq!(read(), ["a", "z"]);
        append_line(&path, &"y".repeat(99)).unwrap();
        assert_eq!(read(), ["a", "z", "y"]);
        write_atomic(&path, line('b') + &line('z') + &line('y')).unwrap();
        assert_eq!(read(), ["b", "z", "y"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Writes of one file from many threads at once, without `lock`:
    /// every result is one whole version, never a mix or a cut-off one.
    #[test]
    fn writes_racing_on_one_file_never_mix() {
        let dir = std::env::temp_dir().join(format!("swarm-store-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("f");
        let versions: Vec<String> = (0..8).map(|i| format!("{i}").repeat(200_000)).collect();
        std::thread::scope(|scope| {
            for v in &versions {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..10 {
                        let _ = write_atomic(path, v);
                    }
                });
            }
        });
        let now = read(&path).unwrap();
        assert!(versions.contains(&now.trim_end().to_string()), "a mixed or partial file ({} bytes)", now.len());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temporary files left");
    }

    /// A file read again unchanged comes from what was opened before;
    /// changed in any way, or its vault closed, it doesn't.
    #[test]
    fn a_file_read_again_is_opened_again_only_when_changed() {
        let dir = fresh("opened");
        let path = dir.join("f");
        write_atomic(&path, "one\n").unwrap();
        assert_eq!(read(&path).unwrap(), "one\n");
        assert_eq!(read(&path).unwrap(), "one\n");
        write_atomic(&path, "two\n").unwrap();
        assert_eq!(read(&path).unwrap(), "two\n");
        append_line(&path, "three").unwrap();
        assert_eq!(read(&path).unwrap(), "two\nthree\n");
        let sealed = seal_text(&path, "owt\neerht\n").unwrap();
        assert_eq!(sealed.len() as u64, fs::metadata(&path).unwrap().len());
        let tmp = dir.join("elsewhere");
        fs::write(&tmp, sealed).unwrap();
        fs::rename(&tmp, &path).unwrap();
        assert_eq!(read(&path).unwrap(), "owt\neerht\n");
        fs::remove_file(&path).unwrap();
        assert_eq!(read(&path).unwrap_err().kind(), std::io::ErrorKind::NotFound);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A vault is marked with this build's format, sealed: one marked
    /// newer is refused, and one that can't be read isn't guessed at.
    #[test]
    fn a_vaults_format_is_kept_sealed_and_a_newer_one_refused() {
        let dir = absolute(&fresh("format"));
        open_vault(&dir, &Keys { seal: Zeroizing::new([3; 32]), names: Zeroizing::new([4; 32]) });
        check_format(&dir).unwrap();
        let path = dir.join("format");
        assert_eq!(read(&path).unwrap(), format!("{FORMAT}\n"));
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(on_disk.trim().parse::<u32>().is_err() && on_disk.trim().len() > 20, "sealed on disk: {on_disk:?}");
        check_format(&dir).unwrap();
        write_atomic(&path, format!("{}\n", FORMAT + 1)).unwrap();
        assert!(check_format(&dir).unwrap_err().to_string().contains("newer Swarm"));
        write_atomic(&path, "x\n").unwrap();
        assert!(check_format(&dir).unwrap_err().to_string().contains("damaged"));
        close_vault(&dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_read_before_goes_with_its_vault() {
        let dir = absolute(&fresh("opened-locked"));
        open_vault(&dir, &Keys { seal: Zeroizing::new([3; 32]), names: Zeroizing::new([4; 32]) });
        let path = dir.join("f");
        write_atomic(&path, "secret\n").unwrap();
        assert_eq!(read(&path).unwrap(), "secret\n");
        close_vault(&dir);
        assert_eq!(read(&path).unwrap(), "");
        open_vault(&dir, &Keys { seal: Zeroizing::new([5; 32]), names: Zeroizing::new([4; 32]) });
        assert_eq!(read(&path).unwrap(), "");
        close_vault(&dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_replaces_the_whole_file_and_leaves_no_temp_behind() {
        let dir = std::env::temp_dir().join(format!("swarm-store-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("f");
        write_atomic(&path, "a long first version\n").unwrap();
        write_atomic(&path, "short\n").unwrap();
        assert_eq!(read(&path).unwrap(), "short\n");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    fn fresh(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn what_is_written_is_sealed_on_disk_and_read_back() {
        let dir = fresh("sealed");
        let path = dir.join("history");
        write_atomic(&path, "100\t<\tmeet at the old mill\n").unwrap();
        append_line(&path, "101\t>\tsee you there").unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("mill") && !raw.contains("see you"), "nothing readable on disk: {raw}");
        assert_eq!(raw.lines().count(), 2, "one sealed line a record");
        assert_eq!(read(&path).unwrap(), "100\t<\tmeet at the old mill\n101\t>\tsee you there\n");
        append_line(&path, "101\t>\tsee you there").unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_ne!(lines[1], lines[2]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_moved_to_another_file_or_damaged_doesnt_open() {
        let dir = fresh("moved");
        write_atomic(&dir.join("a"), "from a\n").unwrap();
        write_atomic(&dir.join("b"), "from b\n").unwrap();
        let from_a = fs::read_to_string(dir.join("a")).unwrap();
        let mut b = fs::read_to_string(dir.join("b")).unwrap();
        b.push_str(&from_a);
        let mut damaged = from_a.clone().into_bytes();
        damaged[10] ^= 1;
        b.push_str(&String::from_utf8(damaged).unwrap());
        b.push_str("plain text, never sealed\n");
        fs::write(dir.join("b"), b).unwrap();
        assert_eq!(read(dir.join("b")).unwrap(), "from b\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_vaults_keys_dont_open_it_and_a_closed_one_is_locked() {
        let dir = fresh("vaults");
        let mine = Keys { seal: Zeroizing::new([1; 32]), names: Zeroizing::new([2; 32]) };
        open_vault(&dir, &mine);
        write_atomic(&dir.join("peers"), "bob\n").unwrap();
        assert_eq!(read(dir.join("peers")).unwrap(), "bob\n");
        open_vault(&dir, &Keys { seal: Zeroizing::new([3; 32]), names: Zeroizing::new([2; 32]) });
        assert_eq!(read(dir.join("peers")).unwrap(), "", "someone else's keys open nothing");
        close_vault(&dir);
        let outside = Path::new("/swarm-no-vault-here/peers");
        assert_eq!(write_atomic(outside, "x\n").unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(append_line(outside, "x").unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn named_files_and_folders_hide_their_names_and_can_be_listed() {
        let dir = fresh("named");
        let bob = "ab".repeat(32);
        let file = named(&dir.join("history"), &bob);
        assert_eq!(file.parent().unwrap(), dir.join("history"));
        let hidden = file.file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(hidden.len(), 32);
        assert!(!hidden.contains("abab"));
        append_line(&file, "100\t<\thi").unwrap();
        assert_eq!(read(&file).unwrap(), "100\t<\thi\n", "the name isn't a record");
        assert_ne!(named(&dir.join("outbox"), &bob).file_name(), file.file_name());
        let channel = named(&dir.join("channels"), "owner-general");
        write_atomic(&channel.join("roster"), "").unwrap();
        let forget = || NAMES.locked().as_mut().unwrap().retain(|p, _| !p.starts_with(&dir));
        forget();
        assert_eq!(names_in(&dir.join("history")), vec![(bob.clone(), file.clone())]);
        assert_eq!(names_in(&dir.join("channels")), vec![("owner-general".to_string(), channel.clone())]);
        write_atomic(&file, "200\t>\tagain\n").unwrap();
        forget();
        assert_eq!(names_in(&dir.join("history")), vec![(bob, file.clone())]);
        assert_eq!(read(&file).unwrap(), "200\t>\tagain\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
