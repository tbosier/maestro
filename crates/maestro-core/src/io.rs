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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskState {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

impl DiskState {
    pub fn read(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        Some(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
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
        disk: Some(DiskState {
            modified: meta.modified().ok(),
            len: meta.len(),
        }),
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

/// Resolves the path that should actually be written: a symlink is followed
/// so the link survives and its target gets the new contents.
fn write_target(path: &Path) -> PathBuf {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

/// Atomically writes `text` to `path` and returns the new disk state.
pub fn save(path: &Path, text: &Rope, encoding: Encoding) -> io::Result<DiskState> {
    let target = write_target(path);
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(
        ".{name}.maestro-tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let existing = fs::metadata(&target).ok();

    let result: io::Result<()> = (|| {
        let file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        let mut w = BufWriter::with_capacity(256 * 1024, file);
        write_encoded(&mut w, text, encoding)?;
        let file = w.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        if let Some(meta) = &existing {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        drop(file);
        fs::rename(&tmp, &target)?;
        sync_dir(&dir);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    DiskState::read(&target)
        .ok_or_else(|| io::Error::other("file vanished immediately after saving"))
}

#[cfg(unix)]
fn sync_dir(dir: &Path) {
    // Best effort: the data is already safe in the renamed file; this makes
    // the rename itself survive a power loss.
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

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

    #[test]
    fn failed_save_leaves_original_intact() {
        let dir = tmpdir("fail");
        let path = dir.join("missing-subdir").join("f.txt");
        assert!(save(&path, &Rope::from_str("x"), Encoding::Utf8).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
