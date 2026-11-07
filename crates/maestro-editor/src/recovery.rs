//! Crash recovery.
//!
//! While a named buffer has unsaved changes, its full text is written to the
//! recovery directory after a short idle period. A crash or a killed
//! terminal therefore loses at most a couple of seconds of typing.
//!
//! Each editor session writes its own file (`<path hash>-<name>.<session>.recover`),
//! so one session saving or discarding a buffer never deletes another
//! session's journal, and a session started after a crash never overwrites
//! the crashed session's file. A session only ever deletes its own files,
//! except through `:recover` (after copying the content into its own
//! journal) and `:recover-discard` (an explicit user choice).
//!
//! Recovery files contain file contents, so the directory is created
//! owner-only (0700) on Unix and files are 0600. Unnamed scratch buffers are
//! not recovered.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

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

fn prefix(file: &Path) -> String {
    let canonical = crate::editor::normalize_path(file);
    format!("{:016x}-", fnv1a(canonical.to_string_lossy().as_bytes()))
}

/// A fresh session id: process id plus time, unique per editor instance.
pub fn new_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{:x}", std::process::id(), nanos & 0xffff_ffff_ffff)
}

/// This session's recovery file for `file`.
pub fn recovery_path(dir: &Path, file: &Path, session: &str) -> PathBuf {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    dir.join(format!("{}{name}.{session}.recover", prefix(file)))
}

/// Every recovery file for `file`, from any session, newest first.
pub fn all_for(dir: &Path, file: &Path) -> Vec<(PathBuf, SystemTime)> {
    let prefix = prefix(file);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, SystemTime)> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with(&prefix) && n.ends_with(".recover")
        })
        .map(|e| {
            let t = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (e.path(), t)
        })
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.1));
    out
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

pub fn write(dir: &Path, file: &Path, session: &str, text: &Rope) -> std::io::Result<()> {
    ensure_dir(dir)?;
    let target = recovery_path(dir, file, session);
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

fn read_body(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let rest = raw.strip_prefix(HEADER)?;
    let (_path, body) = rest.split_once('\n')?;
    Some(body.to_string())
}

/// The newest recovery file for `file` written by another session.
pub fn newest_foreign(dir: &Path, file: &Path, session: &str) -> Option<(PathBuf, String)> {
    let own = recovery_path(dir, file, session);
    all_for(dir, file)
        .into_iter()
        .filter(|(p, _)| *p != own)
        .find_map(|(p, _)| read_body(&p).map(|b| (p, b)))
}

/// Reads this session's recovery text for `file`, if any.
pub fn read(dir: &Path, file: &Path, session: &str) -> Option<String> {
    read_body(&recovery_path(dir, file, session))
}

/// Deletes this session's recovery file for `file`.
pub fn remove(dir: &Path, file: &Path, session: &str) {
    let _ = fs::remove_file(recovery_path(dir, file, session));
}

/// Deletes every session's recovery files for `file`. Only on an explicit
/// user request.
pub fn remove_all(dir: &Path, file: &Path) -> usize {
    all_for(dir, file)
        .into_iter()
        .filter(|(p, _)| fs::remove_file(p).is_ok())
        .count()
}

/// Announces recovery data from another session for a freshly opened
/// buffer when its contents differ from what was loaded.
pub fn notify_if_recoverable(editor: &mut Editor, id: DocumentId) {
    let Some(dir) = editor.recovery_dir.clone() else {
        return;
    };
    let Some(path) = editor.buffers[&id].doc.path().map(Path::to_path_buf) else {
        return;
    };
    let Some((_, body)) = newest_foreign(&dir, &path, &editor.session_id) else {
        return;
    };
    if *editor.buffers[&id].doc.text() == body.as_str() {
        return;
    }
    editor.set_status(
        Severity::Warning,
        format!(
            "unsaved changes from another session exist for {}; :recover loads them, :recover-discard deletes them",
            editor.show_path(&path)
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
    let session = editor.session_id.clone();
    let mut failure = None;
    for buf in editor.buffers.values_mut() {
        let Some(path) = buf.doc.path().map(Path::to_path_buf) else {
            continue;
        };
        if !buf.doc.is_modified() {
            if buf.recovery_version.take().is_some() {
                remove(&dir, &path, &session);
            }
            continue;
        }
        let version = buf.doc.version();
        let idle = buf
            .last_edit
            .is_none_or(|t| now.saturating_duration_since(t) >= interval);
        if buf.recovery_version != Some(version) && idle {
            match write(&dir, &path, &session, buf.doc.text()) {
                Ok(()) => buf.recovery_version = Some(version),
                Err(e) => {
                    let shown = crate::navigation::display_path(&path, &editor.cwd);
                    failure = Some(format!("recovery write failed for {shown}: {e}"))
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
    fn sessions_do_not_touch_each_other() {
        let dir =
            std::env::temp_dir().join(format!("maestro-recovery-test-{}", std::process::id()));
        let file = dir.join("some file.rs");
        write(&dir, &file, "a", &Rope::from_str("from a\n")).unwrap();
        write(&dir, &file, "b", &Rope::from_str("from b\n")).unwrap();
        assert_eq!(read(&dir, &file, "a").as_deref(), Some("from a\n"));
        // Session b saving removes only its own journal.
        remove(&dir, &file, "b");
        assert_eq!(read(&dir, &file, "a").as_deref(), Some("from a\n"));
        let (_, body) = newest_foreign(&dir, &file, "c").unwrap();
        assert_eq!(body, "from a\n");
        assert!(newest_foreign(&dir, &file, "a").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(recovery_path(&dir, &file, "a"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(remove_all(&dir, &file), 1);
        assert!(all_for(&dir, &file).is_empty());
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
