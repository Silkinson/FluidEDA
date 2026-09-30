//! Content-addressed storage (Phase 6).
//!
//! Everything durable is either an **immutable blob** named by the SHA-256 of
//! its bytes — a design file, a room snapshot — or a small per-room pointer
//! into them. A blob never changes once written, so it can be cached forever
//! (browser, CDN) and deduplicates for free: two rooms on the same chip share
//! one design blob, and a snapshot identical to the previous one is the same
//! file.
//!
//! Layout under the store root:
//!
//! ```text
//! blobs/<sha256>      immutable content (designs, room snapshots)
//! rooms/<room>/head   the room's current snapshot hash (replaced atomically)
//! rooms/<room>/log    version history, one line per snapshot:
//!                     "<seq> <unix ms> <sha256> <bytes>"
//! rooms/<room>/wal    updates since the head snapshot, each
//!                     [u32 LE length][yrs v1 update]
//! tmp/                writes in progress (renamed into place when complete)
//! ```
//!
//! Durability: a blob or head is fsynced before its rename, so a crash leaves
//! the old or the new version, never a torn one. WAL appends are not fsynced:
//! they survive a process crash (the kernel holds them), not a power cut. A
//! torn trailing WAL record is dropped on replay.

use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Lower-case hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

/// A well-formed blob name: 64 lower-case hex digits.
pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Room names become directory names: 1–64 of `[A-Za-z0-9_-]` — no dots or
/// slashes, nothing that could walk out of `rooms/`.
pub fn is_room_name(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// One snapshot in a room's history.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Version {
    pub seq: u64,
    pub time_ms: u64,
    pub hash: String,
    pub bytes: u64,
}

pub struct Store {
    root: PathBuf,
    tmp_seq: AtomicU64,
}

impl Store {
    /// Open (creating if needed) the store at `root`. Leftovers of writes a
    /// crash interrupted are removed from `tmp/`.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        for d in ["blobs", "rooms", "tmp"] {
            fs::create_dir_all(root.join(d))?;
        }
        for e in fs::read_dir(root.join("tmp"))?.flatten() {
            let _ = fs::remove_file(e.path());
        }
        Ok(Self { root, tmp_seq: AtomicU64::new(0) })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where blob `hash` lives (`hash` must satisfy [`is_hash`]).
    pub fn blob_path(&self, hash: &str) -> PathBuf {
        assert!(is_hash(hash), "not a blob hash: {hash:?}");
        self.root.join("blobs").join(hash)
    }

    pub fn has_blob(&self, hash: &str) -> bool {
        is_hash(hash) && self.blob_path(hash).is_file()
    }

    /// A fresh path under `tmp/` for a write in progress.
    pub fn tmp_path(&self) -> PathBuf {
        let n = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        self.root.join("tmp").join(format!("{}-{n}", std::process::id()))
    }

    /// Move a complete, fsynced temp file into place as blob `hash`. Returns
    /// false if that blob already existed (the temp file is then removed).
    pub fn commit_blob(&self, tmp: &Path, hash: &str) -> io::Result<bool> {
        let dst = self.blob_path(hash);
        if dst.is_file() {
            let _ = fs::remove_file(tmp);
            return Ok(false);
        }
        fs::rename(tmp, dst)?;
        Ok(true)
    }

    /// Store `bytes` and return their hash (a no-op if already present).
    pub fn put_blob(&self, bytes: &[u8]) -> io::Result<String> {
        let hash = sha256_hex(bytes);
        if !self.has_blob(&hash) {
            let tmp = self.tmp_path();
            write_synced(&tmp, bytes)?;
            self.commit_blob(&tmp, &hash)?;
        }
        Ok(hash)
    }

    pub fn read_blob(&self, hash: &str) -> io::Result<Vec<u8>> {
        if !is_hash(hash) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a blob hash"));
        }
        fs::read(self.blob_path(hash))
    }

    fn room_dir(&self, room: &str) -> PathBuf {
        assert!(is_room_name(room), "not a room name: {room:?}");
        self.root.join("rooms").join(room)
    }

    /// Has the room ever stored anything?
    pub fn room_exists(&self, room: &str) -> bool {
        self.room_dir(room).is_dir()
    }

    /// The room's current snapshot, if it has one.
    pub fn head(&self, room: &str) -> Option<String> {
        let s = fs::read_to_string(self.room_dir(room).join("head")).ok()?;
        let h = s.trim();
        is_hash(h).then(|| h.to_string())
    }

    /// The room's snapshot history, oldest first.
    pub fn versions(&self, room: &str) -> Vec<Version> {
        let Ok(text) = fs::read_to_string(self.room_dir(room).join("log")) else { return Vec::new() };
        text.lines().filter_map(parse_version).collect()
    }

    /// Record blob `hash` (`bytes` long, already stored) as the room's new
    /// head: append it to the log, then atomically replace `head`.
    pub fn record_snapshot(&self, room: &str, hash: &str, bytes: u64, time_ms: u64) -> io::Result<Version> {
        let dir = self.room_dir(room);
        fs::create_dir_all(&dir)?;
        let seq = self.versions(room).last().map_or(1, |v| v.seq + 1);
        let v = Version { seq, time_ms, hash: hash.to_string(), bytes };
        let mut log = OpenOptions::new().create(true).append(true).open(dir.join("log"))?;
        log.write_all(format!("{} {} {} {}\n", v.seq, v.time_ms, v.hash, v.bytes).as_bytes())?;
        log.sync_data()?;
        let tmp = self.tmp_path();
        write_synced(&tmp, format!("{hash}\n").as_bytes())?;
        fs::rename(&tmp, dir.join("head"))?;
        Ok(v)
    }

    /// The room's write-ahead log, opened for appending (created if absent).
    pub fn open_wal(&self, room: &str) -> io::Result<File> {
        let dir = self.room_dir(room);
        fs::create_dir_all(&dir)?;
        OpenOptions::new().create(true).append(true).open(dir.join("wal"))
    }

    /// Every complete record in the room's WAL. A torn trailing record (a
    /// crash mid-append) is dropped *and cut from the file*, so records
    /// appended from now on are not stranded behind it.
    pub fn read_wal(&self, room: &str) -> io::Result<Vec<Vec<u8>>> {
        let path = self.room_dir(room).join("wal");
        let buf = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        let mut i = 0;
        while i + 4 <= buf.len() {
            let n = u32::from_le_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
            if i + 4 + n > buf.len() {
                break;
            }
            out.push(buf[i + 4..i + 4 + n].to_vec());
            i += 4 + n;
        }
        if i < buf.len() {
            OpenOptions::new().write(true).open(&path)?.set_len(i as u64)?;
        }
        Ok(out)
    }

    /// Drop the first `covered` bytes of the room's WAL — records a durable
    /// snapshot now holds — keeping any appended since (a record boundary).
    /// The rest is rewritten through a temp file and a rename; the WAL comes
    /// back reopened for appending.
    pub fn trim_wal(&self, room: &str, covered: u64) -> io::Result<File> {
        let path = self.room_dir(room).join("wal");
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let keep = &bytes[(covered as usize).min(bytes.len())..];
        let tmp = self.tmp_path();
        fs::write(&tmp, keep)?; // WAL durability is process-crash level: no fsync
        fs::rename(&tmp, &path)?;
        self.open_wal(room)
    }
}

/// Append one update to a WAL as a single write (a crash tears at most this
/// record, which replay then drops). Returns the bytes written.
pub fn wal_append(wal: &mut File, update: &[u8]) -> io::Result<u64> {
    let mut rec = Vec::with_capacity(4 + update.len());
    rec.extend_from_slice(&(update.len() as u32).to_le_bytes());
    rec.extend_from_slice(update);
    wal.write_all(&rec)?;
    Ok(rec.len() as u64)
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn parse_version(line: &str) -> Option<Version> {
    let mut it = line.split_ascii_whitespace();
    let v = Version {
        seq: it.next()?.parse().ok()?,
        time_ms: it.next()?.parse().ok()?,
        hash: it.next()?.to_string(),
        bytes: it.next()?.parse().ok()?,
    };
    is_hash(&v.hash).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn temp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("fluideda-store-{tag}-{}-{}", std::process::id(), crate::rand_id()));
        Store::open(dir).unwrap()
    }

    #[test]
    fn blobs_are_named_by_their_hash_and_dedupe() {
        let s = temp_store("blobs");
        let h = s.put_blob(b"hello").unwrap();
        assert_eq!(h, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert_eq!(s.put_blob(b"hello").unwrap(), h, "same bytes, same blob");
        assert_eq!(s.read_blob(&h).unwrap(), b"hello");
        assert!(s.has_blob(&h) && !s.has_blob("nope"));
        assert_eq!(fs::read_dir(s.root().join("blobs")).unwrap().count(), 1);
    }

    #[test]
    fn names_that_could_escape_the_store_are_rejected() {
        for bad in ["", "..", "a/b", "a.b", "x y", &"r".repeat(65)] {
            assert!(!is_room_name(bad), "{bad:?}");
        }
        assert!(is_room_name("sync-1727_x"));
        assert!(!is_hash("../etc/passwd") && !is_hash(&"A".repeat(64)) && is_hash(&"a".repeat(64)));
    }

    #[test]
    fn versions_log_and_head() {
        let s = temp_store("log");
        assert!(s.head("r").is_none() && s.versions("r").is_empty() && !s.room_exists("r"));
        let a = s.put_blob(b"one").unwrap();
        let b = s.put_blob(b"two").unwrap();
        s.record_snapshot("r", &a, 3, 1000).unwrap();
        let v = s.record_snapshot("r", &b, 3, 2000).unwrap();
        assert_eq!(v.seq, 2);
        assert_eq!(s.head("r").as_deref(), Some(b.as_str()));
        assert_eq!(s.versions("r").iter().map(|v| v.hash.as_str()).collect::<Vec<_>>(), [a.as_str(), b.as_str()]);
    }

    #[test]
    fn wal_replay_drops_a_torn_tail() {
        let s = temp_store("wal");
        let mut w = s.open_wal("r").unwrap();
        wal_append(&mut w, b"first").unwrap();
        wal_append(&mut w, b"second").unwrap();
        // A crash mid-record: a length prefix promising more than was written.
        w.write_all(&100u32.to_le_bytes()).unwrap();
        w.write_all(b"torn").unwrap();
        assert_eq!(s.read_wal("r").unwrap(), vec![b"first".to_vec(), b"second".to_vec()]);
        // The tear is cut, so a record appended now is not stranded behind it.
        let mut w = s.open_wal("r").unwrap();
        wal_append(&mut w, b"third").unwrap();
        assert_eq!(s.read_wal("r").unwrap().len(), 3);
    }

    #[test]
    fn trimming_keeps_what_the_snapshot_does_not_cover() {
        let s = temp_store("trim");
        let mut w = s.open_wal("r").unwrap();
        let covered = wal_append(&mut w, b"in the snapshot").unwrap();
        wal_append(&mut w, b"arrived while it was written").unwrap();
        let mut w = s.trim_wal("r", covered).unwrap();
        assert_eq!(s.read_wal("r").unwrap(), vec![b"arrived while it was written".to_vec()]);
        wal_append(&mut w, b"later").unwrap();
        assert_eq!(s.read_wal("r").unwrap().len(), 2, "the reopened handle appends");
    }
}
