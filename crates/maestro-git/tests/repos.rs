//! Integration tests against real repositories built with the git CLI.
//!
//! Each test gets its own temporary HOME and global config file,
//! `GIT_CONFIG_NOSYSTEM=1`, no inherited `-c` settings, and an identity
//! passed with `-c`, so nothing reads or writes the user's config.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use maestro_git::{
    Base, ChangeKind, ConflictKind, DiffLineKind, DiffSpec, EntryState, FileChange, Git, Head,
    MergeCheck, RemoveError, RemoveRefusal, Sign, SignMap,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

const CONFIG: &[&str] = &[
    "user.name=Maestro Test",
    "user.email=test@example.invalid",
    "init.defaultBranch=main",
    "commit.gpgSign=false",
    "core.autocrlf=false",
];

struct Fixture {
    /// Holds HOME and every repository; removed on drop.
    tmp: PathBuf,
    repo: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

impl Fixture {
    /// An initialized repository with one commit of `a.txt`.
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("maestro-git-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let f = Fixture { tmp, repo };
        f.git(&["init", "-q"]);
        f.write("a.txt", "one\ntwo\nthree\n");
        f.commit_all("initial");
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

    fn repo(&self) -> maestro_git::Repository {
        self.api().discover(&self.repo).unwrap().expect("repo")
    }

    fn git_in(&self, dir: &Path, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new("git");
        for c in CONFIG {
            cmd.arg("-c").arg(c);
        }
        cmd.args(args)
            .current_dir(dir)
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
            .unwrap()
    }

    /// Runs git in the main repository and requires success.
    fn git(&self, args: &[&str]) -> String {
        self.git_ok(&self.repo.clone(), args)
    }

    fn git_ok(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.git_in(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(&self, rel: &str, text: &str) {
        write_in(&self.repo, rel, text);
    }

    fn commit_all(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
    }
}

fn write_in(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

#[test]
fn reports_the_installed_version() {
    let f = Fixture::new();
    let v = f.api().version().unwrap();
    assert!(v.major >= 2, "{v}");
}

#[test]
fn discovers_from_files_subdirectories_and_new_paths() {
    let f = Fixture::new();
    f.write("src/deep/x.rs", "x\n");
    let api = f.api();
    let from_file = api
        .discover(&f.repo.join("src/deep/x.rs"))
        .unwrap()
        .unwrap();
    assert_eq!(from_file.root(), f.repo);
    let from_new = api
        .discover(&f.repo.join("src/not/yet/there.rs"))
        .unwrap()
        .unwrap();
    assert_eq!(from_new.root(), f.repo);
    assert!(from_new.is_main_worktree());
    assert_eq!(
        from_new.relative(&f.repo.join("src/deep/x.rs")),
        Some(PathBuf::from("src/deep/x.rs"))
    );
    let outside = f.tmp.join("plain");
    std::fs::create_dir_all(&outside).unwrap();
    assert!(api.discover(&outside).unwrap().is_none());
    // Inside .git is not a working tree.
    assert!(api.discover(&f.repo.join(".git")).unwrap().is_none());
}

#[test]
fn head_reports_branch_and_detached() {
    let f = Fixture::new();
    let repo = f.repo();
    assert_eq!(repo.head().unwrap(), Head::Branch("main".into()));
    f.git(&["checkout", "-q", "--detach"]);
    match repo.head().unwrap() {
        Head::Detached(id) => assert!(id.len() >= 7, "{id}"),
        other => panic!("expected detached, got {other:?}"),
    }
    assert!(repo.head().unwrap().label().starts_with("(detached "));
}

#[test]
fn status_reports_every_category() {
    let f = Fixture::new();
    f.write("b.txt", "b\n");
    f.write(
        "c.txt",
        "c is a file long enough to be detected as renamed\n",
    );
    f.write("d.txt", "base\n");
    f.write(".gitignore", "*.log\n");
    f.commit_all("more files");
    // A conflict in d.txt.
    f.git(&["checkout", "-q", "-b", "other"]);
    f.write("d.txt", "theirs\n");
    f.commit_all("theirs");
    f.git(&["checkout", "-q", "main"]);
    f.write("d.txt", "ours\n");
    f.commit_all("ours");
    let merge = f.git_in(&f.repo, &["merge", "-q", "other"]);
    assert!(!merge.status.success(), "the merge should conflict");
    // Unstaged, staged, renamed, untracked and ignored.
    f.write("a.txt", "one\nTWO\nthree\n");
    f.write("b.txt", "b staged\n");
    f.git(&["add", "b.txt"]);
    f.git(&["mv", "c.txt", "c2.txt"]);
    f.write("new dir/untracked file.txt", "u\n");
    f.write("debug.log", "ignored\n");

    let status = f.repo().status().unwrap();
    let get = |p: &str| {
        status
            .entry(Path::new(p))
            .unwrap_or_else(|| panic!("{p} missing from {:#?}", status.entries))
    };
    assert_eq!(status.branch.head.as_deref(), Some("main"));
    assert_eq!(get("a.txt").unstaged(), Some(ChangeKind::Modified));
    assert_eq!(get("a.txt").staged(), None);
    assert_eq!(get("b.txt").staged(), Some(ChangeKind::Modified));
    assert_eq!(get("b.txt").unstaged(), None);
    let renamed = get("c2.txt");
    assert_eq!(renamed.staged(), Some(ChangeKind::Renamed));
    assert_eq!(renamed.orig_path, Some(PathBuf::from("c.txt")));
    assert!(get("new dir/untracked file.txt").is_untracked());
    assert_eq!(
        get("d.txt").state,
        EntryState::Conflicted(ConflictKind::BothModified)
    );
    assert!(status.entry(Path::new("debug.log")).is_none());
    assert!(!status.is_clean());
    assert_eq!(status.conflicted().count(), 1);

    let with_ignored = f.repo().status_with(true).unwrap();
    assert_eq!(
        with_ignored.entry(Path::new("debug.log")).map(|e| e.state),
        Some(EntryState::Ignored)
    );
}

#[test]
fn status_works_while_another_git_holds_the_index_lock() {
    let f = Fixture::new();
    // Make stat data stale so a locking status would want to rewrite it.
    f.write("a.txt", "one\ntwo\nthree\n");
    let lock = f.repo.join(".git/index.lock");
    std::fs::write(&lock, "held by another git").unwrap();
    let status = f.repo().status();
    std::fs::remove_file(&lock).unwrap();
    assert!(status.unwrap().is_clean());
}

#[test]
fn base_texts_and_line_signs() {
    let f = Fixture::new();
    f.write("nonl.txt", "first\nlast");
    f.write("ignored.tmp", "x\n");
    f.write(".gitignore", "*.tmp\n");
    f.commit_all("files");
    let repo = f.repo();
    let a = f.repo.join("a.txt");
    assert_eq!(
        repo.base_text(&a, Base::Index).unwrap().as_deref(),
        Some(&b"one\ntwo\nthree\n"[..])
    );
    // Stage a change: the index and HEAD now differ.
    f.write("a.txt", "zero\none\ntwo\nthree\n");
    f.git(&["add", "a.txt"]);
    let index = String::from_utf8(repo.base_text(&a, Base::Index).unwrap().unwrap()).unwrap();
    let head = String::from_utf8(repo.base_text(&a, Base::Head).unwrap().unwrap()).unwrap();
    assert_eq!(index, "zero\none\ntwo\nthree\n");
    assert_eq!(head, "one\ntwo\nthree\n");

    // Buffer edits against the index: start, middle, end.
    let buffer = "ZERO\none\nthree\nfour\n";
    let signs: Vec<(usize, Sign)> = SignMap::diff(&index, buffer).iter().collect();
    assert_eq!(
        signs,
        vec![(0, Sign::Changed), (1, Sign::Removed), (3, Sign::Added)]
    );

    // A committed file without a trailing newline.
    let nonl = f.repo.join("nonl.txt");
    let base = String::from_utf8(repo.base_text(&nonl, Base::Index).unwrap().unwrap()).unwrap();
    assert_eq!(base, "first\nlast");
    assert!(SignMap::diff(&base, "first\nlast").is_empty());
    let signs: Vec<_> = SignMap::diff(&base, "first\nlast\nmore").iter().collect();
    assert_eq!(signs, vec![(1, Sign::Changed), (2, Sign::Added)]);

    // New and ignored files have no base; only the first is untracked.
    f.write("brand new.txt", "n\n");
    let new = f.repo.join("brand new.txt");
    assert_eq!(repo.base_text(&new, Base::Index).unwrap(), None);
    assert!(repo.is_untracked(&new).unwrap());
    let ign = f.repo.join("ignored2.tmp");
    std::fs::write(&ign, "i\n").unwrap();
    assert!(!repo.is_untracked(&ign).unwrap());
    assert_eq!(repo.base_text(&ign, Base::Head).unwrap(), None);
}

#[test]
fn base_text_is_none_before_the_first_commit() {
    let f = Fixture::new();
    let fresh = f.tmp.join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    f.git_ok(&fresh, &["init", "-q"]);
    write_in(&fresh, "x.txt", "x\n");
    let repo = f.api().discover(&fresh).unwrap().unwrap();
    assert_eq!(
        repo.base_text(&fresh.join("x.txt"), Base::Head).unwrap(),
        None
    );
    assert_eq!(repo.head().unwrap(), Head::Branch("main".into()));
}

#[test]
fn unified_diff_model_from_git() {
    let f = Fixture::new();
    f.write("big.txt", &"line\n".repeat(20));
    f.commit_all("big");
    f.write("a.txt", "one\nTWO\nthree\nfour\n");
    f.git(&["mv", "big.txt", "moved.txt"]);
    let repo = f.repo();

    let unstaged = repo.diff(&DiffSpec::Unstaged, &[]).unwrap();
    assert_eq!(unstaged.len(), 1);
    let file = &unstaged[0];
    assert_eq!(file.new_path, Some(PathBuf::from("a.txt")));
    assert_eq!(file.change, FileChange::Modified);
    assert_eq!(file.counts(), (2, 1));
    let lines: Vec<_> = file.hunks[0]
        .lines
        .iter()
        .map(|l| (l.kind, l.old_line, l.new_line))
        .collect();
    assert_eq!(
        lines,
        vec![
            (DiffLineKind::Context, Some(1), Some(1)),
            (DiffLineKind::Removed, Some(2), None),
            (DiffLineKind::Added, None, Some(2)),
            (DiffLineKind::Context, Some(3), Some(3)),
            (DiffLineKind::Added, None, Some(4)),
        ]
    );

    let staged = repo.diff(&DiffSpec::Staged, &[]).unwrap();
    assert_eq!(staged.len(), 1);
    assert!(matches!(staged[0].change, FileChange::Renamed { .. }));
    assert_eq!(staged[0].old_path, Some(PathBuf::from("big.txt")));
    assert_eq!(staged[0].new_path, Some(PathBuf::from("moved.txt")));

    let limited = repo
        .diff(&DiffSpec::AgainstHead, &[PathBuf::from("a.txt")])
        .unwrap();
    assert_eq!(limited.len(), 1);
}

#[test]
fn worktree_add_list_remove() {
    let f = Fixture::new();
    let repo = f.repo();
    let path = f.tmp.join("wt one");
    let wt = repo.add_worktree(&path, "agent/one", "main").unwrap();
    assert_eq!(wt.branch.as_deref(), Some("agent/one"));
    assert!(!wt.is_main);
    assert!(path.join("a.txt").exists());

    let list = repo.worktrees().unwrap();
    assert_eq!(list.len(), 2);
    assert!(list[0].is_main);
    assert_eq!(std::fs::canonicalize(&list[1].path).unwrap(), path);

    // The worktree is itself a repository sharing the common directory.
    let inner = f.api().discover(&path).unwrap().unwrap();
    assert!(!inner.is_main_worktree());
    assert_eq!(inner.common_dir(), repo.common_dir());
    assert_eq!(inner.head().unwrap(), Head::Branch("agent/one".into()));

    assert!(repo.removal_check(&path).unwrap().is_empty());
    repo.remove_worktree(&path, false).unwrap();
    assert!(!path.exists());
    assert_eq!(repo.worktrees().unwrap().len(), 1);
    // The branch survives.
    assert!(
        f.git(&["branch", "--list", "agent/one"])
            .contains("agent/one")
    );

    // Bad names and revisions are rejected before git sees them.
    assert!(repo.add_worktree(&f.tmp.join("x"), "-x", "main").is_err());
    assert!(repo.add_worktree(&f.tmp.join("x"), "a..b", "main").is_err());
    assert!(
        repo.add_worktree(&f.tmp.join("x"), "ok", "--orphan")
            .is_err()
    );
}

fn refusals(err: RemoveError) -> Vec<RemoveRefusal> {
    match err {
        RemoveError::Refused(r) => r,
        RemoveError::Git(e) => panic!("expected a refusal, got {e}"),
    }
}

#[test]
fn remove_refuses_dirty_or_untracked_worktrees_unless_forced() {
    let f = Fixture::new();
    let repo = f.repo();
    let path = f.tmp.join("wt");
    repo.add_worktree(&path, "agent/dirty", "main").unwrap();

    write_in(&path, "a.txt", "changed\n");
    let r = refusals(repo.remove_worktree(&path, false).unwrap_err());
    assert_eq!(r, vec![RemoveRefusal::Uncommitted(vec!["a.txt".into()])]);
    assert!(path.exists());

    f.git_ok(&path, &["checkout", "--", "a.txt"]);
    write_in(&path, "notes.txt", "scratch\n");
    let r = refusals(repo.remove_worktree(&path, false).unwrap_err());
    assert_eq!(r, vec![RemoveRefusal::Untracked(vec!["notes.txt".into()])]);
    let msg = RemoveError::Refused(r).to_string();
    assert!(msg.contains("1 untracked file"), "{msg}");

    write_in(&path, "a.txt", "changed again\n");
    let r = refusals(repo.remove_worktree(&path, false).unwrap_err());
    assert_eq!(r.len(), 2);

    repo.remove_worktree(&path, true).unwrap();
    assert!(!path.exists());
}

#[test]
fn remove_refuses_unpushed_commits_unless_forced() {
    let f = Fixture::new();
    let repo = f.repo();

    // No upstream: a commit only this worktree's branch has.
    let local = f.tmp.join("local");
    repo.add_worktree(&local, "agent/local", "main").unwrap();
    write_in(&local, "a.txt", "agent work\n");
    f.git_ok(&local, &["commit", "-q", "-am", "agent work"]);
    let r = refusals(repo.remove_worktree(&local, false).unwrap_err());
    assert_eq!(
        r,
        vec![RemoveRefusal::Unpushed {
            count: 1,
            upstream: None
        }]
    );
    // Once main has the commit, nothing is lost by removing it.
    f.git(&["merge", "-q", "--ff-only", "agent/local"]);
    assert!(repo.removal_check(&local).unwrap().is_empty());
    repo.remove_worktree(&local, false).unwrap();

    // With an upstream: commits ahead of it.
    let origin = f.tmp.join("origin.git");
    f.git_ok(&f.tmp, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    f.git(&["remote", "add", "origin", origin.to_str().unwrap()]);
    f.git(&["push", "-q", "-u", "origin", "main"]);
    let pushed = f.tmp.join("pushed");
    repo.add_worktree(&pushed, "agent/pushed", "main").unwrap();
    f.git_ok(&pushed, &["push", "-q", "-u", "origin", "agent/pushed"]);
    assert!(repo.removal_check(&pushed).unwrap().is_empty());
    write_in(&pushed, "b.txt", "new\n");
    f.git_ok(&pushed, &["add", "b.txt"]);
    f.git_ok(&pushed, &["commit", "-q", "-m", "ahead"]);
    let r = refusals(repo.remove_worktree(&pushed, false).unwrap_err());
    assert_eq!(
        r,
        vec![RemoveRefusal::Unpushed {
            count: 1,
            upstream: Some("origin/agent/pushed".into())
        }]
    );
    // The upstream branch is deleted: git no longer reports "ahead", but the
    // commit still exists only here.
    f.git(&["push", "-q", "origin", "--delete", "agent/pushed"]);
    let r = refusals(repo.remove_worktree(&pushed, false).unwrap_err());
    assert_eq!(
        r,
        vec![RemoveRefusal::Unpushed {
            count: 1,
            upstream: None
        }]
    );
    repo.remove_worktree(&pushed, true).unwrap();
    assert!(!pushed.exists());
}

#[test]
fn remove_never_touches_main_or_locked_worktrees() {
    let f = Fixture::new();
    let repo = f.repo();
    let r = refusals(repo.remove_worktree(&f.repo, true).unwrap_err());
    assert_eq!(r, vec![RemoveRefusal::MainWorktree]);

    let path = f.tmp.join("locked");
    repo.add_worktree(&path, "agent/locked", "main").unwrap();
    f.git(&[
        "worktree",
        "lock",
        "--reason",
        "agent running",
        path.to_str().unwrap(),
    ]);
    let r = refusals(repo.remove_worktree(&path, true).unwrap_err());
    assert_eq!(r, vec![RemoveRefusal::Locked("agent running".into())]);
    assert!(path.exists());

    let r = refusals(
        repo.remove_worktree(&f.tmp.join("nowhere"), true)
            .unwrap_err(),
    );
    assert_eq!(r, vec![RemoveRefusal::NotAWorktree]);
}

#[test]
fn prune_drops_missing_worktrees() {
    let f = Fixture::new();
    let repo = f.repo();
    let path = f.tmp.join("doomed");
    repo.add_worktree(&path, "agent/doomed", "main").unwrap();
    std::fs::remove_dir_all(&path).unwrap();
    let list = repo.worktrees().unwrap();
    assert!(list[1].prunable.is_some(), "{list:?}");
    let msgs = repo.prune_worktrees().unwrap();
    assert!(!msgs.is_empty());
    assert_eq!(repo.worktrees().unwrap().len(), 1);
}

#[test]
fn fast_forward_and_merge_tree_checks() {
    let f = Fixture::new();
    let repo = f.repo();
    f.git(&["checkout", "-q", "-b", "feature"]);
    f.write("feature.txt", "f\n");
    f.commit_all("feature");
    assert!(repo.can_fast_forward("main", "feature").unwrap());
    assert!(!repo.can_fast_forward("feature", "main").unwrap());
    assert!(repo.merge_check("main", "feature").unwrap().is_clean());

    // Diverge with a conflicting change to a.txt.
    f.write("a.txt", "one\nfeature two\nthree\n");
    f.commit_all("feature edits a");
    f.git(&["checkout", "-q", "main"]);
    f.write("a.txt", "one\nmain two\nthree\n");
    f.commit_all("main edits a");
    assert!(!repo.can_fast_forward("main", "feature").unwrap());
    match repo.merge_check("main", "feature").unwrap() {
        MergeCheck::Conflicts { files, tree } => {
            assert_eq!(files, vec![PathBuf::from("a.txt")]);
            assert!(!tree.is_empty());
        }
        clean => panic!("expected conflicts, got {clean:?}"),
    }
    // Nothing changed in the working tree or index.
    assert!(repo.status().unwrap().is_clean());
    assert_eq!(repo.head().unwrap(), Head::Branch("main".into()));

    assert!(repo.can_fast_forward("--all", "main").is_err());
    assert!(repo.can_fast_forward("main", "no-such-branch").is_err());
}

#[test]
fn relative_paths_name_worktrees_from_the_repository_root() {
    let f = Fixture::new();
    let repo = f.repo();
    // The test process runs elsewhere; "inner" must mean <root>/inner.
    repo.add_worktree(Path::new("inner"), "agent/inner", "main")
        .unwrap();
    assert!(f.repo.join("inner/a.txt").exists());
    assert!(repo.removal_check(Path::new("inner")).unwrap().is_empty());
    repo.remove_worktree(Path::new("inner"), false).unwrap();
    assert!(!f.repo.join("inner").exists());
}

#[test]
fn a_missing_worktree_still_protects_detached_commits() {
    let f = Fixture::new();
    let repo = f.repo();
    let path = f.tmp.join("detached");
    repo.add_worktree(&path, "agent/det", "main").unwrap();
    f.git_ok(&path, &["checkout", "-q", "--detach"]);
    write_in(&path, "a.txt", "only here\n");
    f.git_ok(&path, &["commit", "-q", "-am", "detached work"]);
    std::fs::remove_dir_all(&path).unwrap();
    let r = refusals(repo.remove_worktree(&path, false).unwrap_err());
    assert_eq!(
        r,
        vec![RemoveRefusal::Unpushed {
            count: 1,
            upstream: None
        }]
    );
    repo.remove_worktree(&path, true).unwrap();
    assert_eq!(repo.worktrees().unwrap().len(), 1);
}

#[test]
fn base_text_resolves_dot_dot_paths() {
    let f = Fixture::new();
    f.write("src/x.txt", "x\n");
    f.commit_all("x");
    let repo = f.repo();
    let odd = f.repo.join("src/../src/x.txt");
    assert_eq!(
        repo.base_text(&odd, Base::Index).unwrap().as_deref(),
        Some(&b"x\n"[..])
    );
}
