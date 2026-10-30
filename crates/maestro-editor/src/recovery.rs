//! Crash recovery.
//!
//! While a named buffer has unsaved changes, its full text is written to the
//! recovery directory after a short idle period. A crash or a killed
//! terminal therefore loses at most a couple of seconds of typing. Saving or
//! discarding the buffer deletes its recovery file. On open, a recovery file
//! whose contents differ from the file on disk is announced; `:recover`
//! loads it as an undoable change and `:recover-discard` deletes it.
//!
//! Recovery files contain file contents, so the directory is created
//! owner-only (0700) on Unix. Unnamed scratch buffers are not recovered.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use maestro_core::Rope;

use crate::editor::{DocumentId, Editor, Severity};

const HEADER: &str = "maestro-recovery v1\n";

/// FNV-1a: stable across Rust versions, unlike `DefaultHasher`, so recovery
/// files written by one build are found by the next.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn recovery_path(dir: &Path, file: &Path) -> PathBuf {
    let canonical = crate::editor::normalize_path(file);
    let hash = fnv1a(canonical.to_string_lossy().as_bytes());
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    dir.join(format!("{hash:016x}-{name}.recover"))
}

fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

pub fn write(dir: &Path, file: &Path, text: &Rope) -> std::io::Result<()> {
    ensure_dir(dir)?;
    let target = recovery_path(dir, file);
    let tmp = target.with_extension("recover.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(HEADER.as_bytes())?;
        f.write_all(format!("{}\n", crate::editor::normalize_path(file).display()).as_bytes())?;
        for chunk in text.chunks() {
            f.write_all(chunk.as_bytes())?;
        }
        f.sync_all()?;
    }
    fs::rename(&tmp, &target)
}

/// Reads the recovered text for `file`, if any.
pub fn read(dir: &Path, file: &Path) -> Option<String> {
    let raw = fs::read_to_string(recovery_path(dir, file)).ok()?;
    let rest = raw.strip_prefix(HEADER)?;
    let (_path, body) = rest.split_once('\n')?;
    Some(body.to_string())
}

pub fn remove(dir: &Path, file: &Path) {
    let _ = fs::remove_file(recovery_path(dir, file));
}

/// Announces a recovery file for a freshly opened buffer when its contents
/// differ from what was loaded.
pub fn notify_if_recoverable(editor: &mut Editor, id: DocumentId) {
    let Some(dir) = editor.recovery_dir.clone() else {
        return;
    };
    let Some(path) = editor.buffers[&id].doc.path().map(Path::to_path_buf) else {
        return;
    };
    let Some(body) = read(&dir, &path) else {
        return;
    };
    if *editor.buffers[&id].doc.text() == body.as_str() {
        remove(&dir, &path);
        return;
    }
    editor.set_status(
        Severity::Warning,
        format!(
            "unsaved changes from an earlier session exist for {}; :recover loads them, :recover-discard deletes them",
            path.display()
        ),
    );
}

/// Writes recovery files for buffers that changed and have been idle long
/// enough. Called from [`Editor::tick`].
pub fn tick(editor: &mut Editor, now: Instant) {
    let (Some(dir), Some(interval)) = (editor.recovery_dir.clone(), editor.recovery_interval())
    else {
        return;
    };
    let mut failure = None;
    for buf in editor.buffers.values_mut() {
        let Some(path) = buf.doc.path().map(Path::to_path_buf) else {
            continue;
        };
        if !buf.doc.is_modified() {
            if buf.recovery_version.take().is_some() {
                remove(&dir, &path);
            }
            continue;
        }
        let version = buf.doc.version();
        let idle = buf
            .last_edit
            .is_none_or(|t| now.saturating_duration_since(t) >= interval);
        if buf.recovery_version != Some(version) && idle {
            match write(&dir, &path, buf.doc.text()) {
                Ok(()) => buf.recovery_version = Some(version),
                Err(e) => {
                    failure = Some(format!("recovery write failed for {}: {e}", path.display()))
                }
            }
        }
    }
    if let Some(msg) = failure {
        editor.set_status(Severity::Error, msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_remove() {
        let dir =
            std::env::temp_dir().join(format!("maestro-recovery-test-{}", std::process::id()));
        let file = dir.join("some file.rs");
        write(&dir, &file, &Rope::from_str("fn main() {}\nsecond\n")).unwrap();
        assert_eq!(read(&dir, &file).as_deref(), Some("fn main() {}\nsecond\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(recovery_path(&dir, &file))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        remove(&dir, &file);
        assert!(read(&dir, &file).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hash_is_stable() {
        // Published FNV-1a 64 test vector, plus a value computed independently
        // in Python, guard against accidental algorithm changes.
        assert_eq!(fnv1a(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a(b"maestro"), 0x6e352f6b622cc0a6);
    }
}
