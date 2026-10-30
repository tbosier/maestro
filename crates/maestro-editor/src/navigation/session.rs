//! Session restore: on quit, the open files, each one's primary cursor and
//! the focused view's file are saved to
//! `<state>/sessions/<hash of cwd>.toml`. Starting `maestro` with no file
//! arguments in the same directory opens them again.
//!
//! Not restored in this slice: splits (only the focused view's file is
//! shown), scroll positions, multiple selections, unnamed buffers.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use maestro_core::Selection;
use serde::{Deserialize, Serialize};

use crate::editor::{DocumentId, Editor, Severity};

const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFile {
    pub version: u32,
    /// The directory this session belongs to, to catch hash collisions.
    pub cwd: String,
    /// The file shown in the focused view.
    pub focused: Option<String>,
    pub buffers: Vec<SessionBuffer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBuffer {
    pub path: String,
    /// Zero-based line of the primary cursor.
    pub line: usize,
    /// Zero-based char column of the primary cursor.
    pub column: usize,
}

/// `<state>/sessions/<fnv1a of the canonical cwd>.toml`.
pub fn session_path(state_dir: &Path, cwd: &Path) -> PathBuf {
    let cwd = crate::editor::normalize_path(cwd);
    let hash = crate::recovery::fnv1a(cwd.to_string_lossy().as_bytes());
    state_dir.join("sessions").join(format!("{hash:016x}.toml"))
}

/// Writes `bytes` to `path` atomically: a 0600 temp file in the same
/// directory, then a rename over the target. With `durable`, the file and
/// the directory are synced so the result survives a power cut; the recent
/// list skips that, since it is written on every open.
///
/// The parent directory is created, or tightened, to 0700. The temp file is
/// created exclusively, so an existing file or symlink at its name is never
/// written through.
pub fn write_private(path: &Path, bytes: &[u8], durable: bool) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        b.mode(0o700);
        b.create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    b.create(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = match opts.open(&tmp) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Left by a crash, or planted. Remove the entry itself (not
                // what a symlink points at) and create it fresh.
                fs::remove_file(&tmp)?;
                opts.open(&tmp)?
            }
            other => other?,
        };
        f.write_all(bytes)?;
        if durable {
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        if durable && let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The session the editor would save now.
pub fn capture(ed: &Editor, cwd: &Path) -> SessionFile {
    let focused_doc = ed.view().doc;
    let mut focused = None;
    let mut buffers = Vec::new();
    for b in ed.buffers.values() {
        let Some(path) = b.doc.path().and_then(Path::to_str) else {
            continue;
        };
        // A view showing the buffer has the live selection; prefer the
        // focused one.
        let sel = if b.id == focused_doc {
            &ed.view().selection
        } else {
            ed.views
                .values()
                .find(|v| v.doc == b.id)
                .map_or(&b.last_selection, |v| &v.selection)
        };
        let text = b.doc.text();
        let head = sel.primary().head.min(text.len_chars());
        let line = text.char_to_line(head);
        let column = head - text.line_to_char(line);
        if b.id == focused_doc {
            focused = Some(path.to_string());
        }
        buffers.push(SessionBuffer {
            path: path.to_string(),
            line,
            column,
        });
    }
    SessionFile {
        version: VERSION,
        cwd: crate::editor::normalize_path(cwd)
            .to_string_lossy()
            .into_owned(),
        focused,
        buffers,
    }
}

/// Saves the session if sessions are enabled. With no named buffers open
/// the session file is removed, so closing everything and quitting starts
/// fresh next time.
pub fn save(ed: &Editor) -> Result<(), String> {
    let nav = &ed.navigation;
    let (Some(dir), Some(cwd), true) = (&nav.state_dir, &nav.session_cwd, nav.save_session) else {
        return Ok(());
    };
    let path = session_path(dir, cwd);
    let session = capture(ed, cwd);
    if session.buffers.is_empty() {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("could not remove {}: {e}", path.display()))
            }
            _ => Ok(()),
        };
    }
    let text = toml::to_string(&session).map_err(|e| e.to_string())?;
    write_private(&path, text.as_bytes(), true)
        .map_err(|e| format!("could not save the session to {}: {e}", path.display()))
}

/// Reads the saved session for `cwd`, if there is one.
pub fn load(state_dir: &Path, cwd: &Path) -> Result<Option<SessionFile>, String> {
    let path = session_path(state_dir, cwd);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    let session: SessionFile =
        toml::from_str(&text).map_err(|e| format!("invalid session {}: {e}", path.display()))?;
    let want = crate::editor::normalize_path(cwd);
    if session.version != VERSION || Path::new(&session.cwd) != want {
        return Ok(None);
    }
    Ok(Some(session))
}

/// Restores the saved session for the startup directory and reports what
/// happened in the status line. Missing files are skipped and named.
pub fn restore(ed: &mut Editor) {
    let (Some(dir), Some(cwd)) = (
        ed.navigation.state_dir.clone(),
        ed.navigation.session_cwd.clone(),
    ) else {
        return;
    };
    let session = match load(&dir, &cwd) {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            ed.set_status(Severity::Warning, e);
            return;
        }
    };
    let mut opened: Vec<(DocumentId, &SessionBuffer)> = Vec::new();
    let mut missing = Vec::new();
    let mut failed = Vec::new();
    ed.navigation.restoring = true;
    for entry in &session.buffers {
        let path = Path::new(&entry.path);
        if !path.is_file() {
            missing.push(super::display_path(path, &cwd));
            continue;
        }
        match ed.open(path) {
            Ok(id) => opened.push((id, entry)),
            Err(e) => failed.push(e),
        }
    }
    ed.navigation.restoring = false;
    // Put each cursor back, clamped to the file as it is now.
    for (id, entry) in &opened {
        let text = ed.buffers[id].doc.text().clone();
        let line = entry.line.min(text.len_lines().saturating_sub(1));
        let line_start = text.line_to_char(line);
        let line_len =
            maestro_core::text::line_end_excluding_terminator(text.slice(..), line) - line_start;
        let pos = line_start + entry.column.min(line_len);
        let sel = Selection::point(pos).clamp_normal(text.slice(..));
        if let Some(b) = ed.buffers.get_mut(id) {
            b.last_selection = sel.clone();
        }
        for v in ed.views.values_mut().filter(|v| v.doc == *id) {
            v.selection = sel.clone();
        }
    }
    let focus = session
        .focused
        .as_deref()
        .and_then(|f| opened.iter().find(|(_, e)| e.path == f))
        .or(opened.last())
        .map(|(id, _)| *id);
    if let Some(id) = focus {
        ed.switch_to(id);
    }
    // Restoring is not navigation: start with an empty jump list.
    let view = ed.view_mut();
    view.jumps.clear();
    view.jump_index = 0;
    ed.ensure_cursor_visible();
    crate::commands::align_view_center(ed);

    let mut msg = format!(
        "restored session: {} file{}",
        opened.len(),
        if opened.len() == 1 { "" } else { "s" }
    );
    let mut severity = Severity::Info;
    if !missing.is_empty() {
        severity = Severity::Warning;
        msg.push_str(&format!("; skipped missing: {}", missing.join(", ")));
    }
    if !failed.is_empty() {
        severity = Severity::Warning;
        msg.push_str(&format!("; could not open: {}", failed.join(", ")));
    }
    ed.set_status(severity, msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_path_is_stable_and_per_directory() {
        let a = session_path(Path::new("/state"), Path::new("/nonexistent/a"));
        let b = session_path(Path::new("/state"), Path::new("/nonexistent/b"));
        assert_ne!(a, b);
        assert_eq!(
            a,
            session_path(Path::new("/state"), Path::new("/nonexistent/./a"))
        );
        assert!(a.starts_with("/state/sessions"));
        assert_eq!(a.extension().unwrap(), "toml");
    }

    #[test]
    fn session_round_trips_through_toml() {
        let s = SessionFile {
            version: VERSION,
            cwd: "/w".into(),
            focused: Some("/w/a b.rs".into()),
            buffers: vec![SessionBuffer {
                path: "/w/a b.rs".into(),
                line: 3,
                column: 7,
            }],
        };
        let text = toml::to_string(&s).unwrap();
        assert_eq!(toml::from_str::<SessionFile>(&text).unwrap(), s);
    }

    #[cfg(unix)]
    #[test]
    fn private_write_is_atomic_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("maestro-session-w-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let p = dir.join("sub").join("s.toml");
        write_private(&p, b"one", true).unwrap();
        write_private(&p, b"two", false).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dmode = fs::metadata(p.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700);
        // No temp files left behind.
        assert_eq!(fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_temp_symlink_is_not_followed_and_loose_dirs_are_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("maestro-session-l-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        let victim = dir.join("victim.txt");
        fs::write(&victim, "keep me").unwrap();
        let target = state.join("recent.toml");
        let tmp = state.join(format!(".recent.toml.{}.tmp", std::process::id()));
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();
        write_private(&target, b"files = []", false).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep me");
        assert_eq!(fs::read_to_string(&target).unwrap(), "files = []");
        assert!(fs::symlink_metadata(&target).unwrap().file_type().is_file());
        let mode = fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
