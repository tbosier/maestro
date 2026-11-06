//! `default_branch`, `merge_base`, `changes_since` and the one-file diffs
//! against real repositories built with the git CLI, under a temporary HOME
//! with no system or user config.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use maestro_git::{DiffLineKind, FileChange, Git, PathChange, PathChangeKind, Repository};

static NEXT: AtomicUsize = AtomicUsize::new(0);

const CONFIG: &[&str] = &[
    "user.name=Maestro Test",
    "user.email=test@example.invalid",
    "init.defaultBranch=main",
    "commit.gpgSign=false",
    "core.autocrlf=false",
];

struct Fixture {
    tmp: PathBuf,
    repo: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

impl Fixture {
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("maestro-cmp-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let f = Fixture { tmp, repo };
        f.git(&["init", "-q"]);
        f
    }

    fn api(&self) -> Git {
        let mut g = Git::new()
            .env("HOME", self.tmp.join("home"))
            .env("XDG_CONFIG_HOME", self.tmp.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.tmp.join("home/.gitconfig"))
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env("GIT_CEILING_DIRECTORIES", &self.tmp);
        for c in CONFIG {
            g = g.config(*c);
        }
        g
    }

    fn repo(&self) -> Repository {
        self.api().discover(&self.repo).unwrap().expect("repo")
    }

    fn git(&self, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        for c in CONFIG {
            cmd.arg("-c").arg(c);
        }
        let out = cmd
            .args(args)
            .current_dir(&self.repo)
            .env("HOME", self.tmp.join("home"))
            .env("XDG_CONFIG_HOME", self.tmp.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.tmp.join("home/.gitconfig"))
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
}

fn change(path: &str, kind: PathChangeKind) -> PathChange {
    PathChange {
        path: path.into(),
        kind,
    }
}

#[test]
fn a_branch_lists_committed_staged_unstaged_and_untracked_changes_since_main() {
    let f = Fixture::new();
    let repo = f.repo();
    // No commits: no default branch, every file in the index is added.
    assert_eq!(repo.default_branch().unwrap(), None);
    f.write("a.txt", "one\n");
    f.git(&["add", "a.txt"]);
    f.write("loose.txt", "x\n");
    assert_eq!(
        repo.changes_since(None).unwrap(),
        vec![
            change("a.txt", PathChangeKind::Added),
            change("loose.txt", PathChangeKind::Untracked),
        ]
    );
    std::fs::remove_file(f.repo.join("loose.txt")).unwrap();

    f.write("keep.txt", "keep\n");
    f.write("gone.txt", "gone\n");
    f.write(
        "old.rs",
        "fn moved() {\n    let a = 1;\n    let b = 2;\n}\n",
    );
    f.git(&["add", "-A"]);
    f.git(&["commit", "-q", "-m", "base"]);
    let main = f.git(&["rev-parse", "HEAD"]);
    assert_eq!(
        repo.default_branch().unwrap().as_deref(),
        Some("refs/heads/main")
    );
    assert_eq!(maestro_git::short_ref("refs/heads/main"), "main");

    f.git(&["switch", "-q", "-c", "feature"]);
    f.write("src/new.rs", "new\n");
    f.git(&["add", "-A"]);
    f.git(&["commit", "-q", "-m", "feature work"]);
    // main moves on after the fork; that is not the branch's change.
    f.git(&["switch", "-q", "main"]);
    f.write("main_only.txt", "later\n");
    f.git(&["add", "-A"]);
    f.git(&["commit", "-q", "-m", "main moves"]);
    f.git(&["switch", "-q", "feature"]);

    f.write("a.txt", "one\nchanged\n");
    f.git(&["rm", "-q", "gone.txt"]);
    f.git(&["mv", "old.rs", "moved.rs"]);
    f.write("notes.md", "untracked\n");
    f.write(".gitignore", "*.log\n");
    f.write("build.log", "ignored\n");

    // A tag named like the branch must not stand in for it.
    f.git(&["tag", "main", "HEAD"]);
    let default = repo.default_branch().unwrap().unwrap();
    let base = repo.merge_base(&default, "HEAD").unwrap().unwrap();
    assert_eq!(base, main);
    let got = repo.changes_since(Some(&base)).unwrap();
    assert_eq!(
        got,
        vec![
            change(".gitignore", PathChangeKind::Untracked),
            change("a.txt", PathChangeKind::Modified),
            change("gone.txt", PathChangeKind::Deleted),
            change(
                "moved.rs",
                PathChangeKind::Renamed {
                    from: "old.rs".into()
                }
            ),
            change("notes.md", PathChangeKind::Untracked),
            change("src/new.rs", PathChangeKind::Added),
        ]
    );

    // origin/HEAD wins over a local main once it is set.
    f.git(&["update-ref", "refs/remotes/origin/trunk", &main]);
    f.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/trunk",
    ]);
    assert_eq!(
        repo.default_branch().unwrap().as_deref(),
        Some("refs/remotes/origin/trunk")
    );
    assert_eq!(
        maestro_git::short_ref("refs/remotes/origin/trunk"),
        "origin/trunk"
    );

    // Unrelated histories have no merge base.
    f.git(&["add", "-A"]);
    f.git(&["commit", "-q", "-m", "wip"]);
    f.git(&["switch", "-q", "--orphan", "lonely"]);
    f.write("lonely.txt", "x\n");
    f.git(&["add", "lonely.txt"]);
    f.git(&["commit", "-q", "-m", "lonely"]);
    assert_eq!(repo.merge_base("refs/heads/main", "HEAD").unwrap(), None);
}

#[test]
fn one_file_diffs_against_a_revision_and_for_an_untracked_file() {
    let f = Fixture::new();
    f.write("a.txt", "one\ntwo\n");
    f.git(&["add", "-A"]);
    f.git(&["commit", "-q", "-m", "base"]);
    let repo = f.repo();
    f.write("a.txt", "one\nthree\n");
    f.write("new dir/n.txt", "hello\nworld\n");
    f.write("empty.txt", "");

    let files = repo
        .diff_against("HEAD", &[PathBuf::from("a.txt")])
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].change, FileChange::Modified);
    let kinds: Vec<_> = files[0].hunks[0].lines.iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        [
            DiffLineKind::Context,
            DiffLineKind::Removed,
            DiffLineKind::Added
        ]
    );

    let n = repo.diff_untracked(Path::new("new dir/n.txt")).unwrap();
    assert_eq!(n.change, FileChange::Added);
    assert_eq!(n.new_path.as_deref(), Some(Path::new("new dir/n.txt")));
    let added: Vec<&str> = n.hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(added, ["hello", "world"]);

    let e = repo.diff_untracked(Path::new("empty.txt")).unwrap();
    assert_eq!(e.change, FileChange::Added);
    assert!(e.hunks.is_empty());
    // Nothing was added to the index.
    assert_eq!(f.git(&["diff", "--cached", "--name-only"]), "");
}
