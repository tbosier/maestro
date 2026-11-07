//! Untracked files by name and content, and pathspecs that leave chosen
//! files out of `git add`. The editor uses these to tell apart files its
//! own verification run created in an agent's worktree, and to keep them
//! out of what it commits.

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
