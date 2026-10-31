//! File reads and writes on behalf of an agent, confined to one root.
//!
//! Every path is resolved the way the kernel would resolve it (`..` after
//! symlinks, symlinks in any component, a symlink as the last component
//! followed to its target even when that target does not exist yet) and
//! the result must lie inside the root, outside any `.git` directory or
//! file, and be a regular file. Writes go to a temporary file in the same
//! directory, which is then renamed over the target.
//!
//! This is enforcement against what the agent asks for, not a sandbox: a
//! process that swaps a checked directory for a symlink between the check
//! and the open can still win that race, and the agent's own tools are not
//! affected at all.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Largest file the agent may read through the client.
pub const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;

/// How many symlinks one resolution may follow before giving up.
const MAX_LINKS: usize = 40;

#[derive(Debug)]
pub enum AccessError {
    /// The path resolves outside the root.
    OutsideRoot(PathBuf),
    /// The path is inside a `.git` directory (or is the `.git` file of a
    /// worktree).
    GitDir(PathBuf),
    /// The path names something other than a regular file.
    NotRegularFile(PathBuf),
    NotFound(PathBuf),
    /// The path cannot name a file at all (empty, or ends in `..`).
    BadPath(PathBuf),
    /// Too many symlinks, a file too large to read, or a file that is not
    /// UTF-8.
    Refused(String),
    Io(io::Error),
}

impl std::fmt::Display for AccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessError::OutsideRoot(p) => {
                write!(f, "{} is outside the agent's root", p.display())
            }
            AccessError::GitDir(p) => write!(f, "{} is inside .git", p.display()),
            AccessError::NotRegularFile(p) => write!(f, "{} is not a regular file", p.display()),
            AccessError::NotFound(p) => write!(f, "{} does not exist", p.display()),
            AccessError::BadPath(p) => write!(f, "{} does not name a file", p.display()),
            AccessError::Refused(why) => f.write_str(why),
            AccessError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AccessError {}

impl From<io::Error> for AccessError {
    fn from(e: io::Error) -> Self {
        AccessError::Io(e)
    }
}

/// File access confined to a root directory.
#[derive(Debug, Clone)]
pub struct FileAccess {
    root: PathBuf,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl FileAccess {
    /// The root is canonicalized now; it must be an existing directory.
    pub fn new(root: &Path) -> io::Result<Self> {
        let root = fs::canonicalize(root)?;
        if !fs::metadata(&root)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a directory", root.display()),
            ));
        }
        Ok(Self { root })
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reads a UTF-8 text file. `line` is 1-based (0 is treated as 1);
    /// `limit` is a number of lines.
    pub fn read(
        &self,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, AccessError> {
        let target = self.resolve(path)?;
        if !target.exists {
            return Err(AccessError::NotFound(path.to_path_buf()));
        }
        let mut file = fs::File::open(&target.path)?;
        // Checked again on the open handle: the path may have changed.
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(AccessError::NotRegularFile(path.to_path_buf()));
        }
        if meta.len() > MAX_READ_BYTES {
            return Err(AccessError::Refused(format!(
                "{} is larger than {MAX_READ_BYTES} bytes",
                path.display()
            )));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_READ_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_READ_BYTES {
            return Err(AccessError::Refused(format!(
                "{} is larger than {MAX_READ_BYTES} bytes",
                path.display()
            )));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| AccessError::Refused(format!("{} is not UTF-8 text", path.display())))?;
        if line.is_none() && limit.is_none() {
            return Ok(text);
        }
        let skip = line.unwrap_or(1).max(1) as usize - 1;
        let take = limit.map_or(usize::MAX, |n| n as usize);
        Ok(text.split_inclusive('\n').skip(skip).take(take).collect())
    }

    /// Replaces (or creates) a file atomically and returns its resolved
    /// path. Missing parent directories inside the root are created. A
    /// symlink inside the root keeps pointing where it did and its target
    /// gets the contents.
    pub fn write(&self, path: &Path, content: &str) -> Result<PathBuf, AccessError> {
        let target = self.resolve(path)?;
        let dir = target
            .path
            .parent()
            .ok_or_else(|| AccessError::BadPath(path.to_path_buf()))?;
        if !target.exists {
            self.create_dirs(dir, path)?;
        }
        let name = target
            .path
            .file_name()
            .ok_or_else(|| AccessError::BadPath(path.to_path_buf()))?;
        let tmp = dir.join(format!(
            ".{}.maestro-agent-{}-{}",
            name.to_string_lossy(),
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let existing = if target.exists {
            Some(fs::metadata(&target.path)?)
        } else {
            None
        };
        let mut opts = OpenOptions::new();
        // `create_new` never follows or clobbers whatever is at `tmp`.
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if existing.is_some() {
            use std::os::unix::fs::OpenOptionsExt;
            // Private until the original permissions are copied over.
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        let result = (|| -> io::Result<()> {
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            if let Some(meta) = &existing {
                fs::set_permissions(&tmp, meta.permissions())?;
            }
            // `rename` replaces the directory entry and never follows a
            // symlink that appeared at the target meanwhile.
            fs::rename(&tmp, &target.path)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(target.path)
    }

    /// Creates `dir` and its missing ancestors. Each existing ancestor is
    /// resolved and checked; the missing ones are plain names (resolution
    /// already removed any `..`).
    fn create_dirs(&self, dir: &Path, original: &Path) -> Result<(), AccessError> {
        match fs::symlink_metadata(dir) {
            Ok(m) if m.is_dir() => return Ok(()),
            Ok(_) => return Err(AccessError::NotRegularFile(original.to_path_buf())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let parent = dir
            .parent()
            .ok_or_else(|| AccessError::BadPath(original.to_path_buf()))?;
        self.create_dirs(parent, original)?;
        match fs::create_dir(dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Resolves `path` (relative paths are taken from the root) and checks
    /// the result.
    fn resolve(&self, path: &Path) -> Result<Resolved, AccessError> {
        if path.as_os_str().is_empty() {
            return Err(AccessError::BadPath(path.to_path_buf()));
        }
        let mut current = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        for _ in 0..MAX_LINKS {
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    let link = fs::read_link(&current)?;
                    let base = current.parent().unwrap_or(Path::new("/"));
                    current = if link.is_absolute() {
                        link
                    } else {
                        base.join(link)
                    };
                }
                Ok(_) => {
                    let resolved = fs::canonicalize(&current)?;
                    self.check(&resolved, path)?;
                    if !fs::metadata(&resolved)?.is_file() {
                        return Err(AccessError::NotRegularFile(path.to_path_buf()));
                    }
                    return Ok(Resolved {
                        path: resolved,
                        exists: true,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let resolved = self.resolve_missing(&current, path)?;
                    self.check(&resolved, path)?;
                    return Ok(Resolved {
                        path: resolved,
                        exists: false,
                    });
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(AccessError::Refused(format!(
            "{}: too many levels of symbolic links",
            path.display()
        )))
    }

    /// Resolves a path whose last component does not exist: the deepest
    /// existing ancestor is canonicalized and the missing names appended.
    /// A missing name followed by `..` cannot be resolved without guessing
    /// and is refused.
    fn resolve_missing(&self, p: &Path, original: &Path) -> Result<PathBuf, AccessError> {
        let mut missing: Vec<&OsStr> = Vec::new();
        let mut ancestor = p;
        loop {
            match ancestor.components().next_back() {
                Some(Component::Normal(name)) => missing.push(name),
                Some(Component::CurDir) => {}
                _ => return Err(AccessError::BadPath(original.to_path_buf())),
            }
            ancestor = ancestor
                .parent()
                .ok_or_else(|| AccessError::BadPath(original.to_path_buf()))?;
            match fs::symlink_metadata(ancestor) {
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        // `canonicalize` follows a symlinked or dangling ancestor; a
        // dangling one fails here and is refused.
        let mut resolved = match fs::canonicalize(ancestor) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(AccessError::NotFound(original.to_path_buf()));
            }
            Err(e) => return Err(e.into()),
        };
        if !resolved.is_dir() {
            return Err(AccessError::NotRegularFile(original.to_path_buf()));
        }
        for name in missing.iter().rev() {
            resolved.push(name);
        }
        Ok(resolved)
    }

    fn check(&self, resolved: &Path, original: &Path) -> Result<(), AccessError> {
        let Ok(rest) = resolved.strip_prefix(&self.root) else {
            return Err(AccessError::OutsideRoot(original.to_path_buf()));
        };
        if rest.as_os_str().is_empty() {
            return Err(AccessError::NotRegularFile(original.to_path_buf()));
        }
        let in_git = rest.components().any(|c| match c {
            Component::Normal(name) => name.to_string_lossy().eq_ignore_ascii_case(".git"),
            _ => false,
        });
        if in_git {
            return Err(AccessError::GitDir(original.to_path_buf()));
        }
        Ok(())
    }
}

struct Resolved {
    path: PathBuf,
    exists: bool,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
        outside: PathBuf,
        access: FileAccess,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn fixture(name: &str) -> Fixture {
        static N: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "maestro-agent-files-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("src/lib.rs"), "one\ntwo\nthree\n").unwrap();
        fs::write(root.join(".git/config"), "[core]\n").unwrap();
        fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        let access = FileAccess::new(&root).unwrap();
        Fixture {
            root: fs::canonicalize(&root).unwrap(),
            outside: fs::canonicalize(&outside).unwrap(),
            base,
            access,
        }
    }

    #[test]
    fn reads_whole_files_and_line_ranges() {
        let f = fixture("read");
        let p = f.root.join("src/lib.rs");
        assert_eq!(f.access.read(&p, None, None).unwrap(), "one\ntwo\nthree\n");
        assert_eq!(f.access.read(&p, Some(2), None).unwrap(), "two\nthree\n");
        assert_eq!(f.access.read(&p, Some(2), Some(1)).unwrap(), "two\n");
        assert_eq!(f.access.read(&p, Some(0), Some(1)).unwrap(), "one\n");
        assert_eq!(f.access.read(&p, Some(9), None).unwrap(), "");
        // Relative paths are taken from the root.
        assert_eq!(
            f.access
                .read(Path::new("src/lib.rs"), None, Some(1))
                .unwrap(),
            "one\n"
        );
        assert!(matches!(
            f.access.read(&f.root.join("nope.rs"), None, None),
            Err(AccessError::NotFound(_))
        ));
    }

    #[test]
    fn writes_atomically_and_keeps_permissions() {
        let f = fixture("write");
        let p = f.root.join("src/lib.rs");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(f.access.write(&p, "new\n").unwrap(), p);
        assert_eq!(fs::read_to_string(&p).unwrap(), "new\n");
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        // New files in new directories.
        let q = f.root.join("a/b/c.txt");
        assert_eq!(f.access.write(&q, "c").unwrap(), q);
        assert_eq!(fs::read_to_string(&q).unwrap(), "c");
        // No temporary files are left behind.
        let leftovers: Vec<_> = fs::read_dir(f.root.join("src"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().contains("maestro-agent"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn refuses_paths_outside_the_root() {
        let f = fixture("outside");
        let secret = f.outside.join("secret.txt");
        assert!(matches!(
            f.access.read(&secret, None, None),
            Err(AccessError::OutsideRoot(_))
        ));
        assert!(matches!(
            f.access.write(&f.outside.join("new.txt"), "x"),
            Err(AccessError::OutsideRoot(_))
        ));
        assert!(!f.outside.join("new.txt").exists());
    }

    #[test]
    fn refuses_dot_dot_escapes() {
        let f = fixture("dotdot");
        let escape = f.root.join("src/../../outside/secret.txt");
        assert!(matches!(
            f.access.read(&escape, None, None),
            Err(AccessError::OutsideRoot(_))
        ));
        assert!(matches!(
            f.access.write(&escape, "x"),
            Err(AccessError::OutsideRoot(_))
        ));
        assert!(matches!(
            f.access.write(Path::new("../outside/new.txt"), "x"),
            Err(AccessError::OutsideRoot(_))
        ));
        assert_eq!(
            fs::read_to_string(f.outside.join("secret.txt")).unwrap(),
            "secret\n"
        );
        assert!(!f.outside.join("new.txt").exists());
        // `..` inside the root is fine.
        let inside = f.root.join("src/../src/lib.rs");
        assert!(f.access.read(&inside, None, None).is_ok());
        // A missing directory followed by `..` cannot be resolved.
        assert!(matches!(
            f.access.write(&f.root.join("missing/../x.txt"), "x"),
            Err(AccessError::BadPath(_)) | Err(AccessError::NotFound(_))
        ));
        assert!(matches!(
            f.access.write(&f.root.join("src/.."), "x"),
            Err(AccessError::NotRegularFile(_))
        ));
    }

    #[test]
    fn refuses_symlinked_parent_directories() {
        let f = fixture("symdir");
        symlink(&f.outside, f.root.join("link")).unwrap();
        assert!(matches!(
            f.access.read(&f.root.join("link/secret.txt"), None, None),
            Err(AccessError::OutsideRoot(_))
        ));
        assert!(matches!(
            f.access.write(&f.root.join("link/new.txt"), "x"),
            Err(AccessError::OutsideRoot(_))
        ));
        assert!(matches!(
            f.access.write(&f.root.join("link/deeper/new.txt"), "x"),
            Err(AccessError::OutsideRoot(_))
        ));
        assert!(!f.outside.join("new.txt").exists());
        assert!(!f.outside.join("deeper").exists());
    }

    #[test]
    fn refuses_symlinked_files_pointing_out() {
        let f = fixture("symfile");
        symlink(f.outside.join("secret.txt"), f.root.join("s.txt")).unwrap();
        assert!(matches!(
            f.access.read(&f.root.join("s.txt"), None, None),
            Err(AccessError::OutsideRoot(_))
        ));
        assert!(matches!(
            f.access.write(&f.root.join("s.txt"), "x"),
            Err(AccessError::OutsideRoot(_))
        ));
        assert_eq!(
            fs::read_to_string(f.outside.join("secret.txt")).unwrap(),
            "secret\n"
        );
        // A relative link that climbs out.
        symlink("../outside/secret.txt", f.root.join("r.txt")).unwrap();
        assert!(matches!(
            f.access.write(&f.root.join("r.txt"), "x"),
            Err(AccessError::OutsideRoot(_))
        ));
    }

    #[test]
    fn refuses_dangling_symlinks_pointing_out() {
        let f = fixture("dangling");
        symlink(f.outside.join("created.txt"), f.root.join("d.txt")).unwrap();
        assert!(matches!(
            f.access.write(&f.root.join("d.txt"), "x"),
            Err(AccessError::OutsideRoot(_))
        ));
        assert!(!f.outside.join("created.txt").exists());
        // A dangling link into a missing directory outside.
        symlink(f.outside.join("nodir/created.txt"), f.root.join("e.txt")).unwrap();
        assert!(matches!(
            f.access.write(&f.root.join("e.txt"), "x"),
            Err(AccessError::OutsideRoot(_))
        ));
        assert!(!f.outside.join("nodir").exists());
        // A dangling directory link used as a parent.
        symlink(f.outside.join("gone"), f.root.join("gone")).unwrap();
        assert!(f.access.write(&f.root.join("gone/x.txt"), "x").is_err());
        assert!(!f.outside.join("gone").exists());
    }

    #[test]
    fn follows_symlinks_that_stay_inside() {
        let f = fixture("inside");
        symlink("src/lib.rs", f.root.join("alias.rs")).unwrap();
        let written = f
            .access
            .write(&f.root.join("alias.rs"), "via link\n")
            .unwrap();
        assert_eq!(written, f.root.join("src/lib.rs"));
        assert!(
            fs::symlink_metadata(f.root.join("alias.rs"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
            "via link\n"
        );
        // A dangling link to a new file inside the root creates the target.
        symlink("src/new.rs", f.root.join("pending.rs")).unwrap();
        let written = f.access.write(&f.root.join("pending.rs"), "n").unwrap();
        assert_eq!(written, f.root.join("src/new.rs"));
    }

    #[test]
    fn refuses_git_directory() {
        let f = fixture("git");
        assert!(matches!(
            f.access.read(&f.root.join(".git/config"), None, None),
            Err(AccessError::GitDir(_))
        ));
        assert!(matches!(
            f.access.write(&f.root.join(".git/config"), "x"),
            Err(AccessError::GitDir(_))
        ));
        assert!(matches!(
            f.access.write(&f.root.join(".git/hooks/pre-commit"), "x"),
            Err(AccessError::GitDir(_))
        ));
        assert!(matches!(
            f.access.write(&f.root.join("src/../.git/HEAD"), "x"),
            Err(AccessError::GitDir(_))
        ));
        // Through a symlink that stays inside the root.
        symlink(".git", f.root.join("g")).unwrap();
        assert!(matches!(
            f.access.write(&f.root.join("g/config"), "x"),
            Err(AccessError::GitDir(_))
        ));
        // A worktree's `.git` file, and other spellings.
        assert!(matches!(
            f.access.write(&f.root.join(".GIT"), "x"),
            Err(AccessError::GitDir(_))
        ));
        assert!(matches!(
            f.access.write(&f.root.join("sub/.git/config"), "x"),
            Err(AccessError::GitDir(_))
        ));
        assert!(!f.root.join("sub").exists());
        assert_eq!(
            fs::read_to_string(f.root.join(".git/config")).unwrap(),
            "[core]\n"
        );
        // Names that merely start with .git are fine.
        assert!(f.access.write(&f.root.join(".gitignore"), "x").is_ok());
    }

    #[test]
    fn refuses_non_regular_files() {
        let f = fixture("special");
        assert!(matches!(
            f.access.read(&f.root.join("src"), None, None),
            Err(AccessError::NotRegularFile(_))
        ));
        assert!(matches!(
            f.access.write(&f.root.join("src"), "x"),
            Err(AccessError::NotRegularFile(_))
        ));
        assert!(matches!(
            f.access.write(&f.root, "x"),
            Err(AccessError::NotRegularFile(_))
        ));
        let fifo = f.root.join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        // Refused before opening, so this does not block.
        assert!(matches!(
            f.access.read(&fifo, None, None),
            Err(AccessError::NotRegularFile(_))
        ));
        assert!(matches!(
            f.access.write(&fifo, "x"),
            Err(AccessError::NotRegularFile(_))
        ));
        assert!(matches!(
            f.access.read(Path::new("/dev/null"), None, None),
            Err(AccessError::OutsideRoot(_))
        ));
        // A file used as a directory.
        assert!(f.access.write(&f.root.join("src/lib.rs/x"), "x").is_err());
        assert!(matches!(
            f.access.write(Path::new(""), "x"),
            Err(AccessError::BadPath(_))
        ));
    }

    #[test]
    fn refuses_symlink_loops() {
        let f = fixture("loop");
        symlink("b", f.root.join("a")).unwrap();
        symlink("a", f.root.join("b")).unwrap();
        assert!(matches!(
            f.access.write(&f.root.join("a"), "x"),
            Err(AccessError::Refused(_))
        ));
    }

    #[test]
    fn refuses_non_utf8_and_huge_reads() {
        let f = fixture("bytes");
        fs::write(f.root.join("bin"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(matches!(
            f.access.read(&f.root.join("bin"), None, None),
            Err(AccessError::Refused(_))
        ));
        let big = fs::File::create(f.root.join("big")).unwrap();
        big.set_len(MAX_READ_BYTES + 1).unwrap();
        assert!(matches!(
            f.access.read(&f.root.join("big"), None, None),
            Err(AccessError::Refused(_))
        ));
    }
}
