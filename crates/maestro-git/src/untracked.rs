//! Untracked files by name and content, tracked files' contents in the
//! index and on disk, and pathspecs that leave chosen files out of `git
//! add`. The editor uses these to tell apart files its own verification run
//! created or changed in an agent's worktree, and to keep them out of what
//! it commits.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Component, PathBuf};

use crate::repo::check_rev;
use crate::{Access, GitError, Repository, Result, bytes_to_path, split_nul};

/// Paths passed to `hash-object` per call, to stay far below the
/// argument length limit.
const HASH_CHUNK: usize = 256;

/// Refuses absolute paths, `..`, `.` and empty paths.
fn check_relative(paths: &[PathBuf]) -> Result<()> {
    for p in paths {
        let plain = p.components().all(|c| matches!(c, Component::Normal(_)));
        if p.as_os_str().is_empty() || !plain {
            return Err(GitError::InvalidArgument(format!(
                "{} is not a path inside the working tree",
                p.display()
            )));
        }
    }
    Ok(())
}

/// A path whose file on disk is not to be committed, for
/// [`Repository::commit_all_leaving_out`] and
/// [`Repository::work_snapshot_leaving_out`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LeftOut {
    /// Relative to the root.
    pub path: PathBuf,
    /// `None`: an untracked file, which stays untracked. `Some(blob)`: a
    /// tracked file, committed with these contents (a blob id that must be
    /// in the object store) instead of the file on disk, which is left as
    /// it is.
    pub instead: Option<String>,
}

impl LeftOut {
    /// An untracked file left untracked.
    pub fn untracked(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            instead: None,
        }
    }

    /// A tracked file committed with the contents of `blob`.
    pub fn instead(path: impl Into<PathBuf>, blob: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            instead: Some(blob.into()),
        }
    }
}

/// A tracked file's contents in the index and on disk, from
/// [`Repository::tracked_blobs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedBlob {
    /// Relative to the root.
    pub path: PathBuf,
    /// The blob id of its (stage 0) index entry.
    pub index: String,
    /// The blob id `git add` would store for the file on disk, or `None`
    /// when it is missing or not a regular file.
    pub file: Option<String>,
}

/// A file whose contents differ between two trees, from
/// [`Repository::changed_blobs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedBlob {
    /// Relative to the root.
    pub path: PathBuf,
    pub from: String,
    pub to: String,
}

/// True for a full object id: 40 or 64 lowercase hex digits.
pub(crate) fn is_object_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A regular file's mode in a tree or the index.
fn is_file_mode(mode: &[u8]) -> bool {
    mode == b"100644" || mode == b"100755"
}

/// `git add` arguments after `--` that name every file of the working
/// tree except `leave_out`, each excluded by its exact name. They need
/// pathspec magic, so the repository running them must have
/// `GIT_LITERAL_PATHSPECS` off (see [`Repository::with_magic_pathspecs`]).
pub(crate) fn all_but(leave_out: &[PathBuf]) -> Result<Vec<OsString>> {
    check_relative(leave_out)?;
    let mut args = vec![OsString::from(":(top)")];
    for p in leave_out {
        let mut s = OsString::from(":(top,exclude,literal)");
        s.push(p.as_os_str());
        args.push(s);
    }
    Ok(args)
}

impl Repository {
    /// This repository with pathspec magic on. Every other command treats
    /// path arguments as plain file names (`GIT_LITERAL_PATHSPECS=1`); a
    /// caller of this must quote each path with `:(literal)` itself.
    pub(crate) fn with_magic_pathspecs(&self) -> Repository {
        self.with_git(self.git().clone().env("GIT_LITERAL_PATHSPECS", "0"))
    }

    /// Every untracked file that is not ignored, relative to the root,
    /// each file listed (not its directory).
    pub fn untracked_files(&self) -> Result<Vec<PathBuf>> {
        let out = self.run(
            &["ls-files", "-z", "--others", "--exclude-standard"],
            Access::ReadOnly,
        )?;
        Ok(split_nul(&out).into_iter().map(bytes_to_path).collect())
    }

    /// For each of `paths` (relative to the root) that is now an
    /// untracked, not ignored, regular file, its blob id as `git add`
    /// would store it (clean filters and line-ending conversion applied).
    /// Paths that are tracked, ignored, missing, directories or symlinks
    /// are left out. Nothing is written to the object store.
    pub fn untracked_blobs(&self, paths: &[PathBuf]) -> Result<Vec<(PathBuf, String)>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        check_relative(paths)?;
        let mut args: Vec<OsString> = ["ls-files", "-z", "--others", "--exclude-standard", "--"]
            .into_iter()
            .map(OsString::from)
            .collect();
        args.extend(paths.iter().map(|p| p.as_os_str().to_os_string()));
        let out = self.run(&args, Access::ReadOnly)?;
        let listed: BTreeSet<PathBuf> = split_nul(&out).into_iter().map(bytes_to_path).collect();
        let files: Vec<&PathBuf> = paths
            .iter()
            .filter(|p| listed.contains(*p))
            .filter(|p| {
                std::fs::symlink_metadata(self.root().join(p))
                    .is_ok_and(|m| m.file_type().is_file())
            })
            .collect();
        let mut found = Vec::with_capacity(files.len());
        for chunk in files.chunks(HASH_CHUNK) {
            let mut args: Vec<OsString> = vec!["hash-object".into(), "--".into()];
            args.extend(chunk.iter().map(|p| p.as_os_str().to_os_string()));
            let out = self.run(&args, Access::ReadOnly)?;
            let text = String::from_utf8_lossy(&out);
            let ids: Vec<&str> = text.lines().collect();
            if ids.len() != chunk.len() {
                return Err(GitError::Parse {
                    args: "hash-object".into(),
                    detail: format!("{} ids for {} files", ids.len(), chunk.len()),
                });
            }
            for (p, id) in chunk.iter().zip(ids) {
                check_rev(id)?;
                found.push(((*p).clone(), id.to_string()));
            }
        }
        Ok(found)
    }

    /// Which of `paths` (relative to the root) the tree of commit `rev`
    /// holds as a file, symlink or submodule.
    pub fn paths_in(&self, rev: &str, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
        check_rev(rev)?;
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        check_relative(paths)?;
        let mut args: Vec<OsString> = vec![
            "ls-tree".into(),
            "-r".into(),
            "-z".into(),
            "--name-only".into(),
            "--full-tree".into(),
            "--end-of-options".into(),
            rev.into(),
            "--".into(),
        ];
        args.extend(paths.iter().map(|p| p.as_os_str().to_os_string()));
        let out = self.run(&args, Access::ReadOnly)?;
        let listed: BTreeSet<PathBuf> = split_nul(&out).into_iter().map(bytes_to_path).collect();
        Ok(paths
            .iter()
            .filter(|p| listed.contains(*p))
            .cloned()
            .collect())
    }

    /// For each of `paths` (relative to the root) that has an index entry
    /// at stage 0 and no other stage, the entry's blob id and the blob id
    /// of the file on disk. Paths that are not in the index, or are in
    /// conflict, are left out. Nothing is written to the object store.
    pub fn tracked_blobs(&self, paths: &[PathBuf]) -> Result<Vec<TrackedBlob>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        check_relative(paths)?;
        let mut args: Vec<OsString> = ["ls-files", "-z", "--stage", "--"]
            .into_iter()
            .map(OsString::from)
            .collect();
        args.extend(paths.iter().map(|p| p.as_os_str().to_os_string()));
        let out = self.run(&args, Access::ReadOnly)?;
        // `<mode> <blob> <stage>\t<path>`
        let mut index: Vec<(PathBuf, String)> = Vec::new();
        let mut conflicted: BTreeSet<PathBuf> = BTreeSet::new();
        for entry in split_nul(&out) {
            let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
                return Err(GitError::Parse {
                    args: "ls-files --stage".into(),
                    detail: "no tab".into(),
                });
            };
            let path = bytes_to_path(&entry[tab + 1..]);
            let head = String::from_utf8_lossy(&entry[..tab]);
            let fields: Vec<&str> = head.split(' ').collect();
            match fields.as_slice() {
                [_, blob, "0"] => index.push((path, blob.to_string())),
                [_, _, _] => {
                    conflicted.insert(path);
                }
                _ => {
                    return Err(GitError::Parse {
                        args: "ls-files --stage".into(),
                        detail: format!("bad entry {head:?}"),
                    });
                }
            }
        }
        index.retain(|(p, _)| paths.contains(p) && !conflicted.contains(p));
        let files: Vec<&PathBuf> = index
            .iter()
            .map(|(p, _)| p)
            .filter(|p| {
                std::fs::symlink_metadata(self.root().join(p))
                    .is_ok_and(|m| m.file_type().is_file())
            })
            .collect();
        let mut on_disk: Vec<(PathBuf, String)> = Vec::with_capacity(files.len());
        for chunk in files.chunks(HASH_CHUNK) {
            let mut args: Vec<OsString> = vec!["hash-object".into(), "--".into()];
            args.extend(chunk.iter().map(|p| p.as_os_str().to_os_string()));
            let out = self.run(&args, Access::ReadOnly)?;
            let text = String::from_utf8_lossy(&out);
            let ids: Vec<&str> = text.lines().collect();
            if ids.len() != chunk.len() {
                return Err(GitError::Parse {
                    args: "hash-object".into(),
                    detail: format!("{} ids for {} files", ids.len(), chunk.len()),
                });
            }
            for (p, id) in chunk.iter().zip(ids) {
                check_rev(id)?;
                on_disk.push(((*p).clone(), id.to_string()));
            }
        }
        Ok(index
            .into_iter()
            .map(|(path, index)| {
                let file = on_disk
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, b)| b.clone());
                TrackedBlob { path, index, file }
            })
            .collect())
    }

    /// Which of `ids` (full object ids) the object store has.
    pub fn objects_present(&self, ids: &[String]) -> Result<Vec<String>> {
        let ids: Vec<&String> = ids.iter().filter(|i| is_object_id(i)).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = Vec::new();
        for id in &ids {
            input.extend_from_slice(id.as_bytes());
            input.push(b'\n');
        }
        let out = self.run_input(
            &["cat-file", "--batch-check=%(objectname)"],
            &input,
            Access::ReadOnly,
        )?;
        let text = String::from_utf8_lossy(&out);
        Ok(text
            .lines()
            .filter(|l| is_object_id(l))
            .map(str::to_string)
            .collect())
    }

    /// The regular files whose contents differ between the trees `from`
    /// and `to` (tree or commit ids), with their blob ids on each side.
    /// Files added, deleted, or not regular files on either side (symlinks,
    /// submodules) are left out, as are mode-only changes.
    pub fn changed_blobs(&self, from: &str, to: &str) -> Result<Vec<ChangedBlob>> {
        check_rev(from)?;
        check_rev(to)?;
        let out = self.run(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--end-of-options",
                from,
                to,
            ],
            Access::ReadOnly,
        )?;
        // `:<mode> <mode> <blob> <blob> <status>\0<path>\0` per file.
        let fields = split_nul(&out);
        let mut found = Vec::new();
        let mut it = fields.into_iter();
        while let Some(head) = it.next() {
            let Some(path) = it.next() else {
                return Err(GitError::Parse {
                    args: "diff-tree".into(),
                    detail: "a change without a path".into(),
                });
            };
            let head = head.strip_prefix(b":").unwrap_or(head);
            let parts: Vec<&[u8]> = head.split(|&b| b == b' ').collect();
            let [from_mode, to_mode, from_blob, to_blob, status] = parts.as_slice() else {
                return Err(GitError::Parse {
                    args: "diff-tree".into(),
                    detail: format!("bad entry {:?}", String::from_utf8_lossy(head)),
                });
            };
            if *status != b"M" || !is_file_mode(from_mode) || !is_file_mode(to_mode) {
                continue;
            }
            let (from, to) = (
                String::from_utf8_lossy(from_blob).to_string(),
                String::from_utf8_lossy(to_blob).to_string(),
            );
            if from != to && is_object_id(&from) && is_object_id(&to) {
                found.push(ChangedBlob {
                    path: bytes_to_path(path),
                    from,
                    to,
                });
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_but_quotes_each_path_and_refuses_escapes() {
        let args = all_but(&[PathBuf::from("a[1]/b*.txt")]).unwrap();
        assert_eq!(args[0], OsString::from(":(top)"));
        assert_eq!(args[1], OsString::from(":(top,exclude,literal)a[1]/b*.txt"));
        assert!(all_but(&[PathBuf::from("../x")]).is_err());
        assert!(all_but(&[PathBuf::from("/x")]).is_err());
        assert!(all_but(&[PathBuf::new()]).is_err());
        assert_eq!(all_but(&[]).unwrap().len(), 1);
    }

    #[test]
    fn check_relative_accepts_plain_paths() {
        assert!(check_relative(&[PathBuf::from("a/b.txt"), PathBuf::from("Cargo.lock")]).is_ok());
        assert!(check_relative(&[PathBuf::from("a/../b")]).is_err());
    }
}
