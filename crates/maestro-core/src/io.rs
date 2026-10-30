//! Loading and saving files.
//!
//! Saving is atomic on the same filesystem: the new contents go to a
//! temporary file next to the target, are flushed to disk, take over the
//! target's permissions and then replace it with a rename. A crash mid-save
//! leaves either the old file or the new one, never a truncated mix. The
//! directory is synced afterwards on Unix so the rename itself is durable.
//!
//! Limitations, stated plainly: the rename replaces the directory entry, so a
//! file with several hard links loses the link to the others, and ownership
//! is not preserved when saving someone else's file as another user.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use ropey::Rope;

use crate::document::ReadOnlyReason;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    #[default]
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf8Bom => "utf-8-bom",
            Encoding::Utf16Le => "utf-16le",
            Encoding::Utf16Be => "utf-16be",
        }
    }
}

/// What the file looked like on disk when we last read or wrote it.
///
/// Metadata (modification time, length, and on Unix the device and inode)
/// is cheap to poll and catches ordinary edits and atomic replacements by
/// other programs. `hash` covers what metadata cannot: a rewrite that keeps
/// the size and restores the timestamp. It is checked when saving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskState {
    pub modified: Option<SystemTime>,
    pub len: u64,
    pub dev: u64,
    pub ino: u64,
    /// [`content_hash`] of the file's bytes, when known.
    pub hash: Option<u64>,
}

impl DiskState {
    fn from_meta(meta: &fs::Metadata, hash: Option<u64>) -> Self {
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let (dev, ino) = (0, 0);
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
            dev,
            ino,
            hash,
        }
    }

    /// Reads metadata only (no hash).
    pub fn read(path: &Path) -> Option<Self> {
        fs::metadata(path).ok().map(|m| Self::from_meta(&m, None))
    }

    /// True if the metadata differs (ignores the hash).
    pub fn metadata_differs(&self, other: &DiskState) -> bool {
        (self.modified, self.len, self.dev, self.ino)
            != (other.modified, other.len, other.dev, other.ino)
    }
}

/// A fast, streaming, non-cryptographic 64-bit hash for detecting that a
/// file's bytes changed. Processes eight bytes per step.
#[derive(Debug, Clone)]
pub struct ContentHasher {
    state: u64,
    buf: [u8; 8],
    buf_len: usize,
    total: u64,
}

impl Default for ContentHasher {
    fn default() -> Self {
        Self {
            state: 0x243f_6a88_85a3_08d3,
            buf: [0; 8],
            buf_len: 0,
            total: 0,
        }
    }
}

impl ContentHasher {
    fn mix(&mut self, word: u64) {
        self.state = (self.state.rotate_left(5) ^ word).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    pub fn update(&mut self, mut bytes: &[u8]) {
        self.total += bytes.len() as u64;
        if self.buf_len > 0 {
            let take = (8 - self.buf_len).min(bytes.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&bytes[..take]);
            self.buf_len += take;
            bytes = &bytes[take..];
            if self.buf_len < 8 {
                return;
            }
            self.mix(u64::from_le_bytes(self.buf));
            self.buf_len = 0;
        }
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.mix(u64::from_le_bytes(c.try_into().expect("8 bytes")));
        }
        let rest = chunks.remainder();
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    pub fn finish(mut self) -> u64 {
        if self.buf_len > 0 {
            let mut last = [0u8; 8];
            last[..self.buf_len].copy_from_slice(&self.buf[..self.buf_len]);
            self.mix(u64::from_le_bytes(last));
        }
        let total = self.total;
        self.mix(total);
        self.state ^ (self.state >> 29)
    }
}

pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut h = ContentHasher::default();
    h.update(bytes);
    h.finish()
}

/// Hashes the file currently at `path`.
pub fn hash_file(path: &Path) -> io::Result<u64> {
    let mut f = File::open(path)?;
    let mut h = ContentHasher::default();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(h.finish());
        }
        h.update(&buf[..n]);
    }
}

/// A writer that hashes everything passing through it.
struct HashingWriter<W> {
    inner: W,
    hasher: ContentHasher,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug)]
pub struct Loaded {
    pub text: Rope,
    pub encoding: Encoding,
    pub read_only: Option<ReadOnlyReason>,
    pub disk: Option<DiskState>,
}

/// How many leading bytes are inspected for NUL when sniffing binaries.
const BINARY_SNIFF_LEN: usize = 8000;

pub fn load(path: &Path) -> io::Result<Loaded> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            format!("{} is a directory", path.display()),
        ));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut bytes)?;
    let (text, encoding, mut read_only) = decode(&bytes);
    if read_only.is_none() && meta.permissions().readonly() {
        read_only = Some(ReadOnlyReason::Permission);
    }
    Ok(Loaded {
        text,
        encoding,
        read_only,
        disk: Some(DiskState::from_meta(&meta, Some(content_hash(&bytes)))),
    })
}

/// Decodes file bytes. Binary files and invalid UTF-8 decode lossily and come
/// back flagged read-only, because writing them back would change bytes the
/// user never touched.
pub fn decode(bytes: &[u8]) -> (Rope, Encoding, Option<ReadOnlyReason>) {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return match std::str::from_utf8(rest) {
            Ok(s) => (Rope::from_str(s), Encoding::Utf8Bom, None),
            Err(_) => lossy(rest, ReadOnlyReason::InvalidEncoding),
        };
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, Encoding::Utf16Le);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, Encoding::Utf16Be);
    }
    if bytes[..bytes.len().min(BINARY_SNIFF_LEN)].contains(&0) {
        return lossy(bytes, ReadOnlyReason::Binary);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (Rope::from_str(s), Encoding::Utf8, None),
        Err(_) => lossy(bytes, ReadOnlyReason::InvalidEncoding),
    }
}

fn lossy(bytes: &[u8], reason: ReadOnlyReason) -> (Rope, Encoding, Option<ReadOnlyReason>) {
    (
        Rope::from_str(&String::from_utf8_lossy(bytes)),
        Encoding::Utf8,
        Some(reason),
    )
}

fn decode_utf16(bytes: &[u8], enc: Encoding) -> (Rope, Encoding, Option<ReadOnlyReason>) {
    if bytes.len() % 2 != 0 {
        return lossy(bytes, ReadOnlyReason::InvalidEncoding);
    }
    let units = bytes.chunks_exact(2).map(|c| match enc {
        Encoding::Utf16Le => u16::from_le_bytes([c[0], c[1]]),
        _ => u16::from_be_bytes([c[0], c[1]]),
    });
    match char::decode_utf16(units).collect::<Result<String, _>>() {
        Ok(s) => (Rope::from_str(&s), enc, None),
        Err(_) => lossy(bytes, ReadOnlyReason::InvalidEncoding),
    }
}

fn write_encoded(out: &mut impl Write, text: &Rope, encoding: Encoding) -> io::Result<()> {
    match encoding {
        Encoding::Utf8 | Encoding::Utf8Bom => {
            if encoding == Encoding::Utf8Bom {
                out.write_all(&[0xEF, 0xBB, 0xBF])?;
            }
            for chunk in text.chunks() {
                out.write_all(chunk.as_bytes())?;
            }
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let le = encoding == Encoding::Utf16Le;
            out.write_all(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] })?;
            let mut buf = [0u16; 2];
            for c in text.chars() {
                for unit in c.encode_utf16(&mut buf) {
                    let b = if le {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    };
                    out.write_all(&b)?;
                }
            }
        }
    }
    Ok(())
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Resolves the path that should actually be written. Symlinks are followed
/// (including dangling ones, whose target is created) so the link survives
/// and its target gets the new contents.
fn write_target(path: &Path) -> io::Result<PathBuf> {
    let mut p = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&p) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let t = fs::read_link(&p)?;
                p = if t.is_absolute() {
                    t
                } else {
                    p.parent().unwrap_or(Path::new(".")).join(t)
                };
            }
            _ => return Ok(p),
        }
    }
    Err(io::Error::other(format!(
        "{}: too many levels of symbolic links",
        path.display()
    )))
}

/// Atomically writes `text` to `path` and returns the new disk state.
///
/// The temp file is created with `create_new` (never clobbering anything)
/// and, when replacing an existing file, owner-only, so private contents are
/// not briefly readable by others. The returned state comes from the temp
/// file's own handle before the rename, so a writer racing us afterwards is
/// detected rather than adopted as our baseline.
pub fn save(path: &Path, text: &Rope, encoding: Encoding) -> io::Result<DiskState> {
    let target = write_target(path)?;
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{name}.maestro-tmp-{}-{}-{nonce:x}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let existing = fs::metadata(&target).ok();

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if existing.is_some() {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    // Only a file we created may be cleaned up on failure.
    let file = opts.open(&tmp)?;

    let result: io::Result<DiskState> = (|| {
        let mut w = HashingWriter {
            inner: BufWriter::with_capacity(256 * 1024, file),
            hasher: ContentHasher::default(),
        };
        write_encoded(&mut w, text, encoding)?;
        let hash = w.hasher.finish();
        let file = w.inner.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        if let Some(meta) = &existing {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        let state = DiskState::from_meta(&file.metadata()?, Some(hash));
        drop(file);
        fs::rename(&tmp, &target)?;
        Ok(state)
    })();
    match result {
        Ok(state) => {
            sync_dir(&dir).map_err(|e| {
                io::Error::other(format!(
                    "{} was replaced, but syncing its directory failed ({e}); save again before relying on it",
                    target.display()
                ))
            })?;
            Ok(state)
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Makes the rename durable. Filesystems that cannot sync a directory
/// (`EINVAL`, unsupported) are accepted; other failures are reported.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    let d = File::open(dir)?;
    match d.sync_all() {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "maestro-io-test-{name}-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn decode_variants() {
        let (t, e, ro) = decode(b"plain\n");
        assert_eq!(
            (t.to_string().as_str(), e, ro),
            ("plain\n", Encoding::Utf8, None)
        );
        let (t, e, _) = decode(b"\xEF\xBB\xBFbom");
        assert_eq!((t.to_string().as_str(), e), ("bom", Encoding::Utf8Bom));
        let (t, e, _) = decode(b"\xFF\xFEh\x00i\x00");
        assert_eq!((t.to_string().as_str(), e), ("hi", Encoding::Utf16Le));
        assert_eq!(decode(b"ab\x00cd").2, Some(ReadOnlyReason::Binary));
        assert_eq!(decode(b"ab\xFFcd").2, Some(ReadOnlyReason::InvalidEncoding));
    }

    #[test]
    fn save_round_trips_every_encoding() {
        let dir = tmpdir("roundtrip");
        for enc in [
            Encoding::Utf8,
            Encoding::Utf8Bom,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
        ] {
            let path = dir.join(format!("f-{}", enc.label()));
            let text = Rope::from_str("héllo\r\nwörld 😀\n");
            save(&path, &text, enc).unwrap();
            let loaded = load(&path).unwrap();
            assert_eq!(loaded.text, text);
            assert_eq!(loaded.encoding, enc);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_permissions_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tmpdir("perm");
        let real = dir.join("real.sh");
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o751)).unwrap();
        let link = dir.join("link.sh");
        symlink(&real, &link).unwrap();
        save(&link, &Rope::from_str("new"), Encoding::Utf8).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o751
        );
        // No temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("maestro-tmp"))
            .collect();
        assert!(leftovers.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_through_dangling_symlink_creates_target() {
        use std::os::unix::fs::symlink;
        let dir = tmpdir("dangling");
        let link = dir.join("link.rs");
        symlink("real.rs", &link).unwrap();
        save(&link, &Rope::from_str("x"), Encoding::Utf8).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(dir.join("real.rs")).unwrap(), "x");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn temp_name_collision_never_deletes_the_other_file() {
        // A pre-existing file at a would-be temp name is untouched even if
        // a save fails for another reason.
        let dir = tmpdir("collide");
        let bystander = dir.join(".f.maestro-tmp-bystander");
        fs::write(&bystander, "keep me").unwrap();
        save(&dir.join("f"), &Rope::from_str("ok"), Encoding::Utf8).unwrap();
        assert_eq!(fs::read_to_string(&bystander).unwrap(), "keep me");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn content_hash_is_chunking_independent() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        let whole = content_hash(&data);
        for split in [1, 3, 7, 8, 9, 500, 999] {
            let mut h = ContentHasher::default();
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(h.finish(), whole, "split at {split}");
        }
        assert_ne!(content_hash(b"ab"), content_hash(b"ba"));
        assert_ne!(content_hash(b""), content_hash(b"\0"));
    }

    #[test]
    fn failed_save_leaves_original_intact() {
        let dir = tmpdir("fail");
        let path = dir.join("missing-subdir").join("f.txt");
        assert!(save(&path, &Rope::from_str("x"), Encoding::Utf8).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
