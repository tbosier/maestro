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
    IntegrateError, IntegrateOutcome, Integration, LeftOut, MergeCheck, RemoveError, RemoveRefusal,
    Reviewed, Sign, SignMap,
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

fn read_in(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap()
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

#[test]
fn worktrees_added_at_once_from_many_threads_all_succeed() {
    // `N compete` makes several worktrees of one repository at the same
    // moment. Git reads every entry under `.git/worktrees` while it adds
    // one, and fails on an entry another `worktree add` has made but not
    // filled in yet ("failed to read .../commondir"). Without the lock this
    // test failed in 9 of 60 runs on Linux, so a pass here is weak
    // evidence; a failure is the race.
    let f = Fixture::new();
    const THREADS: usize = 8;
    const ROUNDS: usize = 5;
    for round in 0..ROUNDS {
        let start = std::sync::Barrier::new(THREADS);
        let errors: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..THREADS)
                .map(|i| {
                    let repo = f.repo();
                    let path = f.tmp.join(format!("race-{round}-{i}"));
                    let start = &start;
                    s.spawn(move || {
                        let branch = format!("race/{round}-{i}");
                        // Every thread calls git at the same moment.
                        start.wait();
                        let added = repo.add_worktree(&path, &branch, "main");
                        let listed = repo.worktrees();
                        added
                            .err()
                            .into_iter()
                            .chain(listed.err())
                            .map(|e| e.to_string())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        assert!(errors.is_empty(), "round {round}: {errors:#?}");
    }
    assert_eq!(f.repo().worktrees().unwrap().len(), 1 + ROUNDS * THREADS);
}

#[test]
fn the_worktree_lock_is_per_repository() {
    // A long `gh pr checkout` (or a slow hook) in one project must not
    // hold up worktrees in another.
    let a = Fixture::new();
    let b = Fixture::new();
    let (ra, rb) = (a.repo(), b.repo());
    let (tx, rx) = std::sync::mpsc::channel();
    ra.with_worktree_lock(|| {
        std::thread::scope(|s| {
            s.spawn(|| {
                rb.add_worktree(&b.tmp.join("other"), "other", "main")
                    .unwrap();
                tx.send(()).unwrap();
            });
            rx.recv_timeout(std::time::Duration::from_secs(20))
                .expect("another repository's worktree was held up by this lock");
        });
    });
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

// ---- Review diff, commit_all and integrate (ADR 0006) ----

/// Everything a refusal must leave alone in a worktree: `HEAD`, the raw
/// index file, every file on disk (outside `.git`), and no operation in
/// progress.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    head: String,
    index: Vec<u8>,
    files: Vec<(PathBuf, Vec<u8>)>,
    in_progress: Vec<&'static str>,
}

fn snapshot(f: &Fixture, dir: &Path) -> Snapshot {
    let git_dir = PathBuf::from(
        f.git_ok(dir, &["rev-parse", "--absolute-git-dir"])
            .trim()
            .to_string(),
    );
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    files.sort();
    Snapshot {
        head: f.git_ok(dir, &["rev-parse", "HEAD"]).trim().to_string(),
        index: std::fs::read(git_dir.join("index")).unwrap_or_default(),
        files,
        in_progress: ["MERGE_HEAD", "CHERRY_PICK_HEAD", "sequencer"]
            .into_iter()
            .filter(|n| git_dir.join(n).exists())
            .collect(),
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if p.is_dir() {
            walk(root, &p, out);
        } else {
            let rel = p.strip_prefix(root).unwrap().to_path_buf();
            out.push((rel, std::fs::read(&p).unwrap()));
        }
    }
}

fn hook(f: &Fixture, name: &str, body: &str) {
    let path = f.repo.join(".git/hooks").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn unhook(f: &Fixture, name: &str) {
    std::fs::remove_file(f.repo.join(".git/hooks").join(name)).unwrap();
}

/// A worktree on a new branch `name` from main with one commit that adds
/// `file` with `text`.
fn agent_branch(f: &Fixture, name: &str, file: &str, text: &str) -> PathBuf {
    let path = f.tmp.join(name.replace('/', "-"));
    f.repo().add_worktree(&path, name, "main").unwrap();
    commit_in(f, &path, file, text, &format!("{name}: {file}"));
    path
}

fn commit_in(f: &Fixture, dir: &Path, file: &str, text: &str, msg: &str) {
    write_in(dir, file, text);
    f.git_ok(dir, &["add", "-A"]);
    f.git_ok(dir, &["commit", "-q", "-m", msg]);
}

fn rev(f: &Fixture, dir: &Path, r: &str) -> String {
    f.git_ok(dir, &["rev-parse", r]).trim().to_string()
}

fn by_path(files: &[maestro_git::DiffFile], p: &str) -> maestro_git::DiffFile {
    files
        .iter()
        .find(|d| d.path() == Some(&PathBuf::from(p)))
        .unwrap_or_else(|| panic!("{p} missing from {files:#?}"))
        .clone()
}

fn numbered(prefix: &str, n: usize) -> String {
    (1..=n).map(|i| format!("{prefix} {i}\n")).collect()
}

#[test]
fn review_diff_shows_committed_staged_unstaged_and_untracked_work() {
    let f = Fixture::new();
    f.write("keep.txt", "k1\nk2\nk3\n");
    f.write("gone.txt", "bye\n");
    f.write("staged gone.txt", "bye too\n");
    f.write("big.txt", &numbered("line", 40));
    f.write("moved by git.txt", &numbered("row", 30));
    f.commit_all("files");
    let repo = f.repo();
    let wt = f.tmp.join("review");
    repo.add_worktree(&wt, "agent/review", "main").unwrap();

    // Committed on the branch.
    write_in(&wt, "a.txt", "one\nTWO\nthree\n");
    f.git_ok(&wt, &["commit", "-q", "-am", "agent edits a"]);
    // Main moves on; that must not show up as a reverse change.
    f.write("main only.txt", "main\n");
    f.commit_all("main moves");

    // Staged: a deletion, a new file and a rename with git mv.
    f.git_ok(&wt, &["rm", "-q", "staged gone.txt"]);
    write_in(&wt, "staged.txt", "staged\n");
    f.git_ok(&wt, &["add", "staged.txt"]);
    f.git_ok(&wt, &["mv", "moved by git.txt", "renamed by git.txt"]);
    // Unstaged: an edit and a deletion from disk.
    write_in(&wt, "keep.txt", "k1\nk2 edited\nk3\n");
    std::fs::remove_file(wt.join("gone.txt")).unwrap();
    // A plain rename on disk, the way an agent's tools do it.
    std::fs::rename(wt.join("big.txt"), wt.join("big moved.txt")).unwrap();
    // Untracked: text, binary, a nested directory; one ignored file.
    write_in(&wt, "new dir/new.txt", "fresh\nlines\n");
    std::fs::write(wt.join("blob.bin"), [0u8, 1, 2, 0, 255]).unwrap();
    write_in(&wt, ".gitignore", "*.log\n");
    write_in(&wt, "debug.log", "noise\n");

    let inner = f.api().discover(&wt).unwrap().unwrap();
    let before = snapshot(&f, &wt);
    let files = inner.review_diff("main").unwrap();
    assert_eq!(
        snapshot(&f, &wt),
        before,
        "review_diff changed the worktree"
    );

    let mut paths: Vec<String> = files
        .iter()
        .map(|d| d.path().unwrap().display().to_string())
        .collect();
    paths.sort();
    let mut want = vec![
        ".gitignore",
        "a.txt",
        "big moved.txt",
        "blob.bin",
        "gone.txt",
        "keep.txt",
        "new dir/new.txt",
        "renamed by git.txt",
        "staged gone.txt",
        "staged.txt",
    ];
    want.sort();
    assert_eq!(paths, want);

    let a = by_path(&files, "a.txt");
    assert_eq!(a.change, FileChange::Modified);
    assert_eq!(a.counts(), (1, 1));
    assert_eq!(by_path(&files, "keep.txt").change, FileChange::Modified);

    let gone = by_path(&files, "gone.txt");
    assert_eq!(gone.change, FileChange::Deleted);
    assert_eq!(gone.new_path, None);
    assert_eq!(
        by_path(&files, "staged gone.txt").change,
        FileChange::Deleted
    );

    let moved = by_path(&files, "big moved.txt");
    assert_eq!(moved.change, FileChange::Renamed { similarity: 100 });
    assert_eq!(moved.old_path, Some(PathBuf::from("big.txt")));
    let by_git = by_path(&files, "renamed by git.txt");
    assert!(matches!(by_git.change, FileChange::Renamed { .. }));
    assert_eq!(by_git.old_path, Some(PathBuf::from("moved by git.txt")));

    let new = by_path(&files, "new dir/new.txt");
    assert_eq!(new.change, FileChange::Added);
    assert_eq!(new.counts(), (2, 0));
    assert_eq!(new.hunks[0].lines[0].text, "fresh");
    assert_eq!(by_path(&files, "staged.txt").change, FileChange::Added);

    let bin = by_path(&files, "blob.bin");
    assert_eq!(bin.change, FileChange::Added);
    assert!(bin.binary);

    // Without untracked files the plain path runs.
    std::fs::remove_dir_all(wt.join("new dir")).unwrap();
    for p in ["blob.bin", ".gitignore", "debug.log", "big moved.txt"] {
        std::fs::remove_file(wt.join(p)).unwrap();
    }
    let files = inner.review_diff("main").unwrap();
    assert_eq!(by_path(&files, "big.txt").change, FileChange::Deleted);
    assert_eq!(by_path(&files, "a.txt").counts(), (1, 1));
    assert_eq!(files.len(), 7, "{files:#?}");

    assert!(inner.review_diff("--all").is_err());
    assert!(inner.review_diff("no-such-branch").is_err());
}

#[test]
fn commit_all_commits_everything_or_nothing() {
    let f = Fixture::new();
    let repo = f.repo();
    assert_eq!(repo.commit_all("nothing").unwrap(), None);

    f.write("a.txt", "changed\n");
    f.write("new/untracked.txt", "u\n");
    f.write(".gitignore", "*.log\n");
    f.write("skip.log", "ignored\n");
    let id = repo.commit_all("agent work").unwrap().expect("a commit");
    assert_eq!(rev(&f, &f.repo, "HEAD"), id);
    assert_eq!(f.git(&["log", "-1", "--format=%s"]).trim(), "agent work");
    let tree = f.git(&["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.contains("new/untracked.txt"), "{tree}");
    assert!(!tree.contains("skip.log"), "{tree}");
    assert!(repo.status().unwrap().is_clean());
    assert_eq!(repo.commit_all("again").unwrap(), None);

    // Deletions are committed too.
    std::fs::remove_file(f.repo.join("new/untracked.txt")).unwrap();
    repo.commit_all("delete").unwrap().expect("a commit");
    let tree = f.git(&["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree.contains("untracked.txt"), "{tree}");
}

/// Files left out by exact name stay untracked and out of the commit,
/// even with glob characters in their names; everything else goes in,
/// and the snapshot with the same list has the commit's tree.
#[test]
fn commit_all_leaving_out_keeps_named_untracked_files_out() {
    let f = Fixture::new();
    let repo = f.repo();
    f.write("a.txt", "changed\n");
    f.write("Cargo.lock", "made by the tests\n");
    f.write("out/[1].txt", "also made by the tests\n");
    f.write("out/1.txt", "the agent's\n");
    f.write("keep.txt", "the agent's too\n");
    let left: Vec<PathBuf> = vec!["Cargo.lock".into(), "out/[1].txt".into()];
    let leave: Vec<LeftOut> = left.iter().cloned().map(LeftOut::untracked).collect();
    let snap = repo.work_snapshot_leaving_out(&leave).unwrap();
    let full = repo.work_snapshot().unwrap();
    assert_ne!(snap.tree, full.tree);

    let id = repo
        .commit_all_leaving_out("agent work", &leave)
        .unwrap()
        .expect("a commit");
    let tree = f.git(&["ls-tree", "-r", "--name-only", &id]);
    let names: Vec<&str> = tree.lines().collect();
    assert_eq!(names, ["a.txt", "keep.txt", "out/1.txt"], "{tree}");
    assert_eq!(repo.commit_shape(&id).unwrap().tree, snap.tree);
    let untracked = repo.untracked_files().unwrap();
    assert_eq!(untracked, left);
    // Only left-out files remain: nothing to commit.
    assert_eq!(repo.commit_all_leaving_out("again", &leave).unwrap(), None);
    // A path that escapes the worktree is refused before anything runs.
    assert!(
        repo.commit_all_leaving_out("bad", &[LeftOut::untracked("../x")])
            .is_err()
    );
}

/// A tracked file left out with other contents is committed with those
/// contents and the mode of the file on disk (an executable bit the agent
/// set without staging it), and stays as it is on disk; the snapshot with
/// the same list has the commit's tree. A blob id that is not one, or is
/// missing, is refused before anything is staged.
#[test]
fn commit_all_leaving_out_commits_other_contents_for_a_tracked_file() {
    let f = Fixture::new();
    let repo = f.repo();
    f.write("run.sh", "echo 1\n");
    f.commit_all("script");
    // The agent's version (made executable, not staged), then a test
    // run's rewrite of both files.
    f.write("a.txt", "the agent's\n");
    f.write("run.sh", "echo 2\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = f.repo.join("run.sh");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let agents = f.git(&["hash-object", "-w", "a.txt"]).trim().to_string();
    let script = f.git(&["hash-object", "-w", "run.sh"]).trim().to_string();
    f.write("a.txt", "rewritten by the tests\n");
    f.write("run.sh", "echo 3\n");
    f.write("new.txt", "the agent's new file\n");
    let leave = [
        LeftOut::instead("a.txt", agents.clone()),
        LeftOut::instead("run.sh", script.clone()),
    ];
    assert!(
        repo.commit_all_leaving_out("bad", &[LeftOut::instead("a.txt", "HEAD")])
            .is_err()
    );
    let missing = "1".repeat(agents.len());
    assert!(
        repo.commit_all_leaving_out("bad", &[LeftOut::instead("a.txt", missing)])
            .is_err()
    );
    assert!(f.git(&["diff", "--cached", "--name-only"]).is_empty());
    let snap = repo.work_snapshot_leaving_out(&leave).unwrap();
    let id = repo
        .commit_all_leaving_out("agent work", &leave)
        .unwrap()
        .expect("a commit");
    assert_eq!(repo.commit_shape(&id).unwrap().tree, snap.tree);
    assert_eq!(f.git(&["show", &format!("{id}:a.txt")]), "the agent's\n");
    assert_eq!(f.git(&["show", &format!("{id}:run.sh")]), "echo 2\n");
    assert_eq!(
        f.git(&["show", &format!("{id}:new.txt")]),
        "the agent's new file\n"
    );
    #[cfg(unix)]
    {
        let mode = f.git(&["ls-tree", &id, "run.sh"]);
        assert!(mode.starts_with("100755 "), "{mode}");
    }
    // The files on disk are untouched and differ from the commit.
    assert_eq!(read_in(&f.repo, "a.txt"), "rewritten by the tests\n");
    assert_eq!(f.git(&["status", "--porcelain"]), " M a.txt\n M run.sh\n");
    // Nothing else to commit.
    assert_eq!(repo.commit_all_leaving_out("again", &leave).unwrap(), None);

    // A clean worktree (a test run wrote back the committed version) still
    // commits the other contents.
    f.git(&["checkout", "--", "a.txt", "run.sh"]);
    assert!(repo.status().unwrap().is_clean());
    f.write("a.txt", "the agent's, again\n");
    let again = f.git(&["hash-object", "-w", "a.txt"]).trim().to_string();
    f.git(&["checkout", "--", "a.txt"]);
    assert!(repo.status().unwrap().is_clean());
    let id = repo
        .commit_all_leaving_out("from a clean worktree", &[LeftOut::instead("a.txt", again)])
        .unwrap()
        .expect("a commit");
    assert_eq!(
        f.git(&["show", &format!("{id}:a.txt")]),
        "the agent's, again\n"
    );
}

/// `tracked_blobs` gives the index entry's and the file's blob ids of
/// tracked files only; `changed_blobs` lists files whose contents differ
/// between two trees; `objects_present` says which ids the store has.
#[test]
fn tracked_and_changed_blobs() {
    let f = Fixture::new();
    let repo = f.repo();
    let before = repo.work_snapshot().unwrap();
    f.write("a.txt", "changed\n");
    f.write("new.txt", "x\n");
    let after = repo.work_snapshot().unwrap();
    let ask: Vec<PathBuf> = ["a.txt", "new.txt", "missing"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let got = repo.tracked_blobs(&ask).unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].path, PathBuf::from("a.txt"));
    assert_eq!(got[0].index, rev(&f, &f.repo, "HEAD:a.txt"));
    let disk = f.git(&["hash-object", "a.txt"]).trim().to_string();
    assert_eq!(got[0].file.as_deref(), Some(disk.as_str()));
    std::fs::remove_file(f.repo.join("a.txt")).unwrap();
    assert_eq!(repo.tracked_blobs(&ask).unwrap()[0].file, None);

    let changed = repo.changed_blobs(&before.tree, &after.tree).unwrap();
    assert_eq!(changed.len(), 1, "added files are not listed: {changed:?}");
    assert_eq!(changed[0].path, PathBuf::from("a.txt"));
    assert_eq!(changed[0].from, rev(&f, &f.repo, "HEAD:a.txt"));
    assert_eq!(changed[0].to, disk);

    let missing = "0".repeat(disk.len());
    assert_eq!(
        repo.objects_present(&[disk.clone(), missing]).unwrap(),
        [disk]
    );
}

/// `untracked_blobs` reports untracked regular files only, with the id
/// `git add` would store; `paths_in` says which paths a commit holds.
#[test]
fn untracked_blobs_names_only_untracked_regular_files() {
    let f = Fixture::new();
    let repo = f.repo();
    f.write(".gitignore", "*.log\n");
    f.commit_all("ignore logs");
    f.write("new.txt", "x\n");
    f.write("skip.log", "ignored\n");
    f.write("a.txt", "tracked, changed\n");
    std::fs::create_dir_all(f.repo.join("dir")).unwrap();
    f.write("dir/inner.txt", "y\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink("new.txt", f.repo.join("link")).unwrap();
    let ask: Vec<PathBuf> = [
        "new.txt",
        "skip.log",
        "a.txt",
        "dir",
        "link",
        "missing",
        "dir/inner.txt",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    let got = repo.untracked_blobs(&ask).unwrap();
    let names: Vec<&Path> = got.iter().map(|(p, _)| p.as_path()).collect();
    assert_eq!(names, [Path::new("new.txt"), Path::new("dir/inner.txt")]);
    let want = f.git(&["hash-object", "new.txt"]);
    assert_eq!(got[0].1, want.trim());
    assert!(repo.untracked_blobs(&["/etc/passwd".into()]).is_err());

    let held = repo
        .paths_in(
            "HEAD",
            &["a.txt".into(), "new.txt".into(), ".gitignore".into()],
        )
        .unwrap();
    assert_eq!(held, [PathBuf::from("a.txt"), PathBuf::from(".gitignore")]);
}

/// `leave_untracked` lets exactly those untracked files stay in the
/// source worktree; any other untracked file still refuses.
#[test]
fn integrate_reviewed_lets_named_untracked_files_stay_behind() {
    let f = Fixture::new();
    let repo = f.repo();
    let base = rev(&f, &f.repo, "HEAD");
    let wt = agent_branch(&f, "agent/left", "one.txt", "1\n");
    let reviewed = rev(&f, &wt, "HEAD");
    write_in(&wt, "Cargo.lock", "made by the tests\n");
    write_in(&wt, "stray.txt", "not named\n");
    let leave = [PathBuf::from("Cargo.lock")];
    let err = repo
        .integrate_reviewed(
            "agent/left",
            Reviewed {
                commit: &reviewed,
                target: "main",
                base: &base,
                leave_untracked: &leave,
                leave_modified: &[],
            },
            Integration::Merge,
        )
        .unwrap_err();
    assert!(
        matches!(&err, IntegrateError::SourceDirty(p) if p == &[PathBuf::from("stray.txt")]),
        "{err:?}"
    );
    std::fs::remove_file(wt.join("stray.txt")).unwrap();
    let out = repo
        .integrate_reviewed(
            "agent/left",
            Reviewed {
                commit: &reviewed,
                target: "main",
                base: &base,
                leave_untracked: &leave,
                leave_modified: &[],
            },
            Integration::Merge,
        )
        .unwrap();
    assert_eq!(out.new_head, rev(&f, &f.repo, "HEAD"));
    let tree = f.git(&["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.lines().any(|l| l == "one.txt"), "{tree}");
    assert!(!tree.lines().any(|l| l == "Cargo.lock"), "{tree}");
    assert!(wt.join("Cargo.lock").exists());
}

/// `leave_modified` lets exactly those tracked files stay changed and
/// unstaged in the source worktree; staged, or another file, still
/// refuses.
#[test]
fn integrate_reviewed_lets_named_modified_files_stay_behind() {
    let f = Fixture::new();
    let repo = f.repo();
    let base = rev(&f, &f.repo, "HEAD");
    let wt = agent_branch(&f, "agent/modified", "one.txt", "1\n");
    let reviewed = rev(&f, &wt, "HEAD");
    write_in(&wt, "a.txt", "rewritten by the tests\n");
    let leave = [PathBuf::from("a.txt")];
    let try_it = || {
        repo.integrate_reviewed(
            "agent/modified",
            Reviewed {
                commit: &reviewed,
                target: "main",
                base: &base,
                leave_untracked: &[],
                leave_modified: &leave,
            },
            Integration::Merge,
        )
    };
    f.git_ok(&wt, &["add", "a.txt"]);
    let err = try_it().unwrap_err();
    assert!(
        matches!(&err, IntegrateError::SourceDirty(p) if p == &[PathBuf::from("a.txt")]),
        "staged is refused: {err:?}"
    );
    f.git_ok(&wt, &["reset", "-q"]);
    write_in(&wt, "one.txt", "changed too\n");
    let err = try_it().unwrap_err();
    assert!(
        matches!(&err, IntegrateError::SourceDirty(p) if p == &[PathBuf::from("one.txt")]),
        "{err:?}"
    );
    f.git_ok(&wt, &["checkout", "--", "one.txt"]);
    let out = try_it().unwrap();
    assert_eq!(out.new_head, rev(&f, &f.repo, "HEAD"));
    assert_eq!(f.git(&["show", "HEAD:a.txt"]), "one\ntwo\nthree\n");
    assert_eq!(read_in(&f.repo, "a.txt"), "one\ntwo\nthree\n");
    assert_eq!(read_in(&wt, "a.txt"), "rewritten by the tests\n");
}

#[test]
fn commit_all_respects_hooks_and_identity() {
    let f = Fixture::new();
    let repo = f.repo();
    let head = rev(&f, &f.repo, "HEAD");

    hook(&f, "pre-commit", "echo 'tests failed' >&2; exit 1");
    f.write("a.txt", "blocked\n");
    let err = repo.commit_all("blocked").unwrap_err();
    assert!(err.to_string().contains("tests failed"), "{err}");
    assert_eq!(rev(&f, &f.repo, "HEAD"), head);
    unhook(&f, "pre-commit");

    // No identity: refused before anything is staged.
    let mut nobody = Git::new()
        .env("HOME", f.tmp.join("home"))
        .env("XDG_CONFIG_HOME", f.tmp.join("home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", f.tmp.join("home/.gitconfig"))
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .config("user.useConfigOnly=true")
        .config("commit.gpgSign=false");
    for v in [
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "EMAIL",
    ] {
        nobody = nobody.env_remove(v);
    }
    let anon = nobody.discover(&f.repo).unwrap().unwrap();
    f.git(&["reset", "-q"]);
    f.write("b.txt", "untracked\n");
    let before = snapshot(&f, &f.repo);
    let err = anon.commit_all("who am I").unwrap_err();
    assert!(matches!(err, maestro_git::GitError::NoIdentity(_)), "{err}");
    assert!(err.to_string().contains("user.name"), "{err}");
    assert_eq!(snapshot(&f, &f.repo), before);

    // With the identity back, it commits.
    assert!(repo.commit_all("works").unwrap().is_some());
}

#[test]
fn commit_all_refuses_conflicted_files() {
    let f = Fixture::new();
    f.git(&["checkout", "-q", "-b", "other"]);
    f.write("a.txt", "other\n");
    f.commit_all("other");
    f.git(&["checkout", "-q", "main"]);
    f.write("a.txt", "main\n");
    f.commit_all("main");
    let out = f.git_in(&f.repo, &["merge", "other"]);
    assert!(!out.status.success());
    let repo = f.repo();
    let before = snapshot(&f, &f.repo);
    let err = repo.commit_all("markers").unwrap_err();
    assert!(
        matches!(&err, maestro_git::GitError::Unmerged(p) if p == &vec![PathBuf::from("a.txt")]),
        "{err}"
    );
    assert_eq!(snapshot(&f, &f.repo), before);
    f.git(&["merge", "--abort"]);
}

/// Runs `run`, requires an error `check` accepts, and requires every
/// worktree in `dirs` to be exactly as before.
fn assert_refused(
    f: &Fixture,
    dirs: &[&Path],
    run: impl FnOnce() -> Result<IntegrateOutcome, IntegrateError>,
    check: impl FnOnce(&IntegrateError) -> bool,
) {
    let before: Vec<Snapshot> = dirs.iter().map(|d| snapshot(f, d)).collect();
    let err = run().expect_err("expected a refusal");
    assert!(check(&err), "unexpected error: {err:?}");
    let after: Vec<Snapshot> = dirs.iter().map(|d| snapshot(f, d)).collect();
    assert_eq!(after, before, "a refusal changed something ({err})");
}

const ALL: [Integration; 3] = [
    Integration::FastForwardOnly,
    Integration::Merge,
    Integration::CherryPick,
];

#[test]
fn integrate_refuses_without_changing_anything() {
    let f = Fixture::new();
    let repo = f.repo();
    let wt = agent_branch(&f, "agent/a", "agent.txt", "agent\n");
    let both: [&Path; 2] = [&f.repo, &wt];

    // The target has unstaged, then staged, changes.
    f.write("a.txt", "dirty\n");
    for how in ALL {
        assert_refused(
            &f,
            &both,
            || repo.integrate("agent/a", how),
            |e| matches!(e, IntegrateError::Dirty(p) if p == &vec![PathBuf::from("a.txt")]),
        );
    }
    f.git(&["add", "a.txt"]);
    assert_refused(
        &f,
        &both,
        || repo.integrate("agent/a", Integration::Merge),
        |e| matches!(e, IntegrateError::Dirty(_)),
    );
    f.git(&["reset", "-q", "--hard"]);

    // Untracked files in the target do not count as dirty, but the source
    // worktree's do: they would be left behind.
    f.write("scratch.txt", "mine\n");
    write_in(&wt, "forgotten.txt", "not committed\n");
    for how in ALL {
        assert_refused(
            &f,
            &both,
            || repo.integrate("agent/a", how),
            |e| {
                matches!(e, IntegrateError::SourceDirty(p)
                    if p == &vec![PathBuf::from("forgotten.txt")])
            },
        );
    }
    std::fs::remove_file(wt.join("forgotten.txt")).unwrap();
    write_in(&wt, "agent.txt", "edited, not committed\n");
    assert_refused(
        &f,
        &both,
        || repo.integrate("agent/a", Integration::CherryPick),
        |e| matches!(e, IntegrateError::SourceDirty(_)),
    );
    f.git_ok(&wt, &["checkout", "--", "agent.txt"]);

    // Nothing to integrate: main itself, and a branch main already has.
    f.git(&["branch", "old", "main"]);
    for how in ALL {
        for b in ["main", "old"] {
            assert_refused(
                &f,
                &both,
                || repo.integrate(b, how),
                |e| matches!(e, IntegrateError::NothingToIntegrate),
            );
        }
    }

    // Diverged: fast-forward only refuses.
    f.write("main.txt", "main\n");
    f.commit_all("main moves");
    assert_refused(
        &f,
        &both,
        || repo.integrate("agent/a", Integration::FastForwardOnly),
        |e| matches!(e, IntegrateError::NotFastForward),
    );

    // Conflicts found by the dry run, for merge and cherry-pick.
    write_in(&wt, "a.txt", "agent line\n");
    f.git_ok(&wt, &["commit", "-q", "-am", "agent edits a"]);
    f.write("a.txt", "main line\n");
    f.commit_all("main edits a");
    for how in [Integration::Merge, Integration::CherryPick] {
        assert_refused(
            &f,
            &both,
            || repo.integrate("agent/a", how),
            |e| matches!(e, IntegrateError::Conflicts(p) if p == &vec![PathBuf::from("a.txt")]),
        );
    }

    // Not a branch, or not a valid name.
    for b in ["no-such", "--all", "a..b"] {
        assert_refused(
            &f,
            &both,
            || repo.integrate(b, Integration::Merge),
            |e| matches!(e, IntegrateError::Git(_)),
        );
    }

    // A detached target.
    f.git(&["checkout", "-q", "--detach"]);
    assert_refused(
        &f,
        &both,
        || repo.integrate("agent/a", Integration::Merge),
        |e| matches!(e, IntegrateError::DetachedHead),
    );
    f.git(&["checkout", "-q", "main"]);
}

#[test]
fn integrate_refuses_while_another_operation_is_in_progress() {
    let f = Fixture::new();
    let repo = f.repo();
    let wt = agent_branch(&f, "agent/p", "p.txt", "p\n");
    f.write("b.txt", "b\n");
    f.commit_all("b");
    f.git(&["revert", "--no-commit", "HEAD"]);
    let both: [&Path; 2] = [&f.repo, &wt];
    assert_refused(
        &f,
        &both,
        || repo.integrate("agent/p", Integration::Merge),
        |e| matches!(e, IntegrateError::InProgress(_)),
    );
    f.git(&["revert", "--abort"]);
}

#[test]
fn integrate_refuses_cherry_picking_merge_commits() {
    let f = Fixture::new();
    let repo = f.repo();
    let wt = agent_branch(&f, "agent/m", "x.txt", "x\n");
    f.write("main.txt", "main\n");
    f.commit_all("main moves");
    f.git_ok(&wt, &["merge", "-q", "--no-edit", "main"]);
    let both: [&Path; 2] = [&f.repo, &wt];
    assert_refused(
        &f,
        &both,
        || repo.integrate("agent/m", Integration::CherryPick),
        |e| matches!(e, IntegrateError::MergeCommits(m) if m.len() == 1),
    );
    // Merging it is fine.
    let out = repo.integrate("agent/m", Integration::Merge).unwrap();
    assert_eq!(out.new_head, rev(&f, &f.repo, "HEAD"));
}

#[test]
fn integrate_fast_forwards() {
    let f = Fixture::new();
    let repo = f.repo();
    let wt = agent_branch(&f, "agent/ff", "one.txt", "1\n");
    commit_in(&f, &wt, "two.txt", "2\n", "two");
    let tip = rev(&f, &wt, "HEAD");
    f.write("untracked here.txt", "kept\n");

    let out = repo
        .integrate("agent/ff", Integration::FastForwardOnly)
        .unwrap();
    assert_eq!(out.new_head, tip);
    assert_eq!(out.commits.len(), 2);
    assert_eq!(out.commits[1], tip);
    assert_eq!(rev(&f, &f.repo, "main"), tip);
    assert!(f.repo.join("two.txt").exists());
    assert!(f.repo.join("untracked here.txt").exists());
    assert_eq!(repo.head().unwrap(), Head::Branch("main".into()));
}

/// `integrate_reviewed`'s expectations: `commit` on `main`, from `base`.
fn on_main<'a>(commit: &'a str, base: &'a str) -> Reviewed<'a> {
    Reviewed {
        commit,
        target: "main",
        base,
        leave_untracked: &[],
        leave_modified: &[],
    }
}

#[test]
fn integrate_reviewed_takes_only_the_reviewed_commit() {
    let f = Fixture::new();
    let repo = f.repo();
    // Two commits on main, so main can be moved back behind the base.
    f.write("pre.txt", "pre\n");
    f.commit_all("pre");
    let base = rev(&f, &f.repo, "HEAD");
    let wt = agent_branch(&f, "agent/r", "one.txt", "1\n");
    let reviewed = rev(&f, &wt, "HEAD");
    let both: [&Path; 2] = [&f.repo, &wt];

    // A new commit on the branch after the review.
    commit_in(&f, &wt, "late.txt", "unreviewed\n", "late");
    let late = rev(&f, &wt, "HEAD");
    for how in ALL {
        assert_refused(
            &f,
            &both,
            || repo.integrate_reviewed("agent/r", on_main(&reviewed, &base), how),
            |e| {
                matches!(e, IntegrateError::SourceMoved { expected, found }
                    if *expected == reviewed && *found == late)
            },
        );
    }

    // The branch reset to another commit with the same files: still not
    // what was reviewed.
    f.git_ok(&wt, &["reset", "-q", "--hard", &reviewed]);
    f.git_ok(&wt, &["commit", "-q", "--amend", "-m", "same files"]);
    let amended = rev(&f, &wt, "HEAD");
    assert_ne!(amended, reviewed);
    assert_eq!(
        repo.commit_shape(&amended).unwrap().tree,
        repo.commit_shape(&reviewed).unwrap().tree
    );
    assert_refused(
        &f,
        &both,
        || repo.integrate_reviewed("agent/r", on_main(&reviewed, &base), Integration::Merge),
        |e| matches!(e, IntegrateError::SourceMoved { found, .. } if *found == amended),
    );

    // A revision that names no commit fails without changing anything.
    assert_refused(
        &f,
        &both,
        || repo.integrate_reviewed("agent/r", on_main("no-such-rev", &base), Integration::Merge),
        |e| matches!(e, IntegrateError::Git(_)),
    );
    f.git_ok(&wt, &["reset", "-q", "--hard", &reviewed]);

    // Another branch checked out in the target worktree.
    f.git(&["checkout", "-q", "-b", "elsewhere"]);
    assert_refused(
        &f,
        &both,
        || repo.integrate_reviewed("agent/r", on_main(&reviewed, &base), Integration::Merge),
        |e| {
            matches!(e, IntegrateError::WrongTarget { expected, found }
                if expected == "main" && found == "elsewhere")
        },
    );
    f.git(&["checkout", "-q", "main"]);

    // The target moved back behind the base: the range would hold the
    // base itself, which the review did not show.
    f.git(&["reset", "-q", "--hard", "HEAD~1"]);
    for how in ALL {
        assert_refused(
            &f,
            &both,
            || repo.integrate_reviewed("agent/r", on_main(&reviewed, &base), how),
            |e| matches!(e, IntegrateError::BaseNotOnTarget { base: b } if *b == base),
        );
    }
    f.git(&["reset", "-q", "--hard", &base]);

    // As reviewed, it integrates exactly that.
    let out = repo
        .integrate_reviewed(
            "agent/r",
            on_main(&reviewed[..12], &base),
            Integration::FastForwardOnly,
        )
        .unwrap();
    assert_eq!(out.new_head, reviewed);
    assert_eq!(rev(&f, &f.repo, "main"), reviewed);
    assert!(!f.repo.join("late.txt").exists());
}

/// A replacement object (`git replace`) for the reviewed commit, made
/// after the review, gives it another history. Maestro's git commands
/// ignore replacements, so the history that lands is the real one.
#[test]
fn integration_ignores_replacement_objects() {
    let f = Fixture::new();
    let repo = f.repo();
    let base = rev(&f, &f.repo, "HEAD");
    let wt = agent_branch(&f, "agent/rep", "one.txt", "1\n");
    let reviewed = rev(&f, &wt, "HEAD");
    // A commit on the side that the review never saw, and a stand-in for
    // the reviewed commit with the same tree whose parent is that commit.
    commit_in(&f, &wt, "secret.txt", "token\n", "secret");
    let secret = rev(&f, &wt, "HEAD");
    f.git_ok(&wt, &["reset", "-q", "--hard", &reviewed]);
    let tree = rev(&f, &wt, "HEAD^{tree}");
    let fake = f
        .git_ok(
            &wt,
            &["commit-tree", &tree, "-p", &secret, "-m", "stand-in"],
        )
        .trim()
        .to_string();
    f.git(&["replace", &reviewed, &fake]);

    let out = repo
        .integrate_reviewed(
            "agent/rep",
            on_main(&reviewed, &base),
            Integration::CherryPick,
        )
        .unwrap();
    assert_eq!(out.commits.len(), 1, "{:?}", out.commits);
    let real = f.git_in(
        &f.repo,
        &[
            "--no-replace-objects",
            "merge-base",
            "--is-ancestor",
            &secret,
            "main",
        ],
    );
    assert!(!real.status.success(), "the stand-in's history landed");
    let files = f.git(&[
        "--no-replace-objects",
        "log",
        "--name-only",
        "--format=",
        "main",
    ]);
    assert!(!files.contains("secret.txt"), "{files}");
}

#[test]
fn commit_shape_and_snapshot_head() {
    let f = Fixture::new();
    let repo = f.repo();
    let head = rev(&f, &f.repo, "HEAD");
    let shape = repo.commit_shape("HEAD").unwrap();
    assert_eq!(shape.commit, head);
    assert_eq!(shape.tree, rev(&f, &f.repo, "HEAD^{tree}"));
    assert_eq!(shape.parents, Vec::<String>::new());

    f.write("new.txt", "uncommitted\n");
    let snap = repo.work_snapshot().unwrap();
    assert_eq!(snap.head, head);
    let shape = repo.commit_shape(&snap.commit).unwrap();
    assert_eq!(shape.parents, vec![head.clone()]);
    assert_eq!(shape.tree, snap.tree);

    // A merge commit lists both parents, in order.
    let wt = agent_branch(&f, "agent/s", "s.txt", "s\n");
    let tip = rev(&f, &wt, "HEAD");
    f.commit_all("main moves");
    let main_before = rev(&f, &f.repo, "HEAD");
    let out = repo.integrate("agent/s", Integration::Merge).unwrap();
    assert_eq!(
        repo.commit_shape(&out.new_head).unwrap().parents,
        vec![main_before, tip]
    );
}

#[test]
fn integrate_merges_with_a_merge_commit() {
    let f = Fixture::new();
    let repo = f.repo();
    let wt = agent_branch(&f, "agent/m", "agent.txt", "agent\n");
    let tip = rev(&f, &wt, "HEAD");
    f.write("main.txt", "main\n");
    f.commit_all("main moves");
    let main_before = rev(&f, &f.repo, "HEAD");

    let out = repo.integrate("agent/m", Integration::Merge).unwrap();
    let parents = f.git(&["rev-list", "--parents", "-n1", "HEAD"]);
    assert_eq!(
        parents.split_whitespace().collect::<Vec<_>>(),
        vec![out.new_head.as_str(), main_before.as_str(), tip.as_str()]
    );
    assert_eq!(out.commits, vec![tip.clone(), out.new_head.clone()]);
    assert_eq!(
        f.git(&["log", "-1", "--format=%s"]).trim(),
        "Merge branch 'agent/m' into main"
    );
    assert!(f.repo.join("agent.txt").exists());
    assert!(repo.status().unwrap().is_clean());

    // Even when a fast-forward is possible, Merge makes a merge commit.
    agent_branch(&f, "agent/m2", "second.txt", "2\n");
    let out = repo.integrate("agent/m2", Integration::Merge).unwrap();
    let parents = f.git(&["rev-list", "--parents", "-n1", &out.new_head]);
    assert_eq!(parents.split_whitespace().count(), 3);
}

#[test]
fn integrate_cherry_picks_oldest_first() {
    let f = Fixture::new();
    let repo = f.repo();
    let wt = agent_branch(&f, "agent/cp", "first.txt", "1\n");
    commit_in(&f, &wt, "second.txt", "2\n", "second");
    // A commit whose change main already has is skipped, not a failure.
    commit_in(&f, &wt, "main.txt", "same\n", "duplicate");
    f.write("main.txt", "same\n");
    f.commit_all("main has it");
    let main_before = rev(&f, &f.repo, "HEAD");

    let out = repo.integrate("agent/cp", Integration::CherryPick).unwrap();
    assert_eq!(out.commits.len(), 2);
    assert_eq!(out.new_head, rev(&f, &f.repo, "HEAD"));
    let subjects = f.git(&[
        "log",
        "--reverse",
        "--format=%s",
        &format!("{main_before}..HEAD"),
    ]);
    assert_eq!(
        subjects.lines().collect::<Vec<_>>(),
        vec!["agent/cp: first.txt", "second"]
    );
    // Linear history on main: copies, no merge commit.
    let parents = f.git(&["rev-list", "--parents", "-n1", "HEAD"]);
    assert_eq!(parents.split_whitespace().count(), 2);
    assert!(f.repo.join("first.txt").exists() && f.repo.join("second.txt").exists());
    assert!(repo.status().unwrap().is_clean());

    // Every commit already applied: nothing to integrate.
    let both: [&Path; 2] = [&f.repo, &wt];
    assert_refused(
        &f,
        &both,
        || repo.integrate("agent/cp", Integration::CherryPick),
        |e| matches!(e, IntegrateError::NothingToIntegrate),
    );
}

#[test]
fn a_failing_hook_leaves_no_merge_in_progress() {
    let f = Fixture::new();
    let repo = f.repo();
    let wt = agent_branch(&f, "agent/h", "agent.txt", "agent\n");
    // Diverge so a merge commit is needed for every mode but fast-forward.
    f.write("main.txt", "main\n");
    f.commit_all("main moves");
    let both: [&Path; 2] = [&f.repo, &wt];

    hook(
        &f,
        "pre-merge-commit",
        "echo 'pre-merge-commit says no' >&2; exit 1",
    );
    assert_refused(
        &f,
        &both,
        || repo.integrate("agent/h", Integration::Merge),
        |e| matches!(e, IntegrateError::Git(g) if g.to_string().contains("says no")),
    );
    unhook(&f, "pre-merge-commit");

    hook(&f, "commit-msg", "echo 'commit-msg says no' >&2; exit 1");
    assert_refused(
        &f,
        &both,
        || repo.integrate("agent/h", Integration::Merge),
        |e| matches!(e, IntegrateError::Git(g) if g.to_string().contains("says no")),
    );
    unhook(&f, "commit-msg");

    hook(
        &f,
        "prepare-commit-msg",
        "echo 'prepare says no' >&2; exit 1",
    );
    for how in [Integration::Merge, Integration::CherryPick] {
        assert_refused(
            &f,
            &both,
            || repo.integrate("agent/h", how),
            |e| matches!(e, IntegrateError::Git(g) if g.to_string().contains("says no")),
        );
    }
    unhook(&f, "prepare-commit-msg");

    // Without the hooks it goes through.
    repo.integrate("agent/h", Integration::Merge).unwrap();
}

#[test]
fn an_untracked_file_in_the_way_is_aborted_cleanly() {
    let f = Fixture::new();
    let repo = f.repo();
    // Two commits: the first applies, the second adds a file that exists
    // untracked in the target, so a cherry-pick stops half way.
    let wt = agent_branch(&f, "agent/u", "fine.txt", "fine\n");
    commit_in(&f, &wt, "clash.txt", "from the branch\n", "clash");
    f.write("clash.txt", "the user's own file\n");
    let both: [&Path; 2] = [&f.repo, &wt];

    for how in ALL {
        assert_refused(
            &f,
            &both,
            || repo.integrate("agent/u", how),
            |e| matches!(e, IntegrateError::Git(g) if g.to_string().contains("would be overwritten")),
        );
    }
    assert_eq!(
        std::fs::read_to_string(f.repo.join("clash.txt")).unwrap(),
        "the user's own file\n"
    );
}

#[test]
fn a_checkout_that_fails_half_way_is_reported_not_hidden() {
    let f = Fixture::new();
    let repo = f.repo();
    // The branch changes a.txt and adds a file whose required smudge
    // filter fails, so git can stop after writing some files.
    let wt = agent_branch(&f, "agent/filter", ".gitattributes", "z.dat filter=boom\n");
    write_in(&wt, "a.txt", "changed by the branch\n");
    commit_in(&f, &wt, "z.dat", "data\n", "a and z");
    f.git(&["config", "filter.boom.smudge", "false"]);
    f.git(&["config", "filter.boom.clean", "cat"]);
    f.git(&["config", "filter.boom.required", "true"]);
    let head = rev(&f, &f.repo, "HEAD");

    let err = repo
        .integrate("agent/filter", Integration::FastForwardOnly)
        .unwrap_err();
    let status = f.git(&["status", "--porcelain"]);
    match &err {
        // Either git changed nothing...
        IntegrateError::Git(_) => assert_eq!(status, "", "{err}"),
        // ...or it left files changed and that is reported.
        IntegrateError::AbortFailed { detail, .. } => {
            assert!(detail.contains("differ from HEAD"), "{detail}")
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(rev(&f, &f.repo, "HEAD"), head);
}

fn shared_index_files(f: &Fixture) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(f.repo.join(".git"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("sharedindex."))
        .collect();
    v.sort();
    v
}

#[test]
fn review_diff_leaves_a_split_index_readable() {
    let f = Fixture::new();
    // Every index write makes a new shared index and expires old ones at
    // once, the harshest split index setup.
    f.git(&["config", "core.splitIndex", "true"]);
    f.git(&["config", "splitIndex.maxPercentChange", "0"]);
    f.git(&["config", "splitIndex.sharedIndexExpire", "now"]);
    f.git(&["update-index", "--split-index"]);
    f.write("b.txt", "b\n");
    f.commit_all("b");
    f.write("new.txt", "untracked\n");
    let shared = shared_index_files(&f);
    assert!(!shared.is_empty(), "the fixture should use a split index");
    let before = snapshot(&f, &f.repo);

    let files = f.repo().review_diff("main").unwrap();
    assert_eq!(by_path(&files, "new.txt").change, FileChange::Added);
    assert_eq!(snapshot(&f, &f.repo), before);
    assert_eq!(shared_index_files(&f), shared);
    // The real index still reads.
    f.git(&["ls-files", "--stage"]);
}

#[test]
fn branch_merge_options_cannot_leave_a_merge_open() {
    let f = Fixture::new();
    let repo = f.repo();
    agent_branch(&f, "agent/nc", "nc.txt", "nc\n");
    f.write("main.txt", "main\n");
    f.commit_all("main moves");
    let main_before = rev(&f, &f.repo, "HEAD");
    f.git(&["config", "branch.main.mergeOptions", "--no-commit --squash"]);

    let out = repo.integrate("agent/nc", Integration::Merge).unwrap();
    assert_ne!(out.new_head, main_before);
    let parents = f.git(&["rev-list", "--parents", "-n1", "HEAD"]);
    assert_eq!(parents.split_whitespace().count(), 3, "{parents}");
    assert!(!f.repo.join(".git/MERGE_HEAD").exists());
    assert!(repo.status().unwrap().is_clean());

    // Fast-forward too.
    let wt = agent_branch(&f, "agent/ff2", "ff.txt", "ff\n");
    let tip = rev(&f, &wt, "HEAD");
    let out = repo
        .integrate("agent/ff2", Integration::FastForwardOnly)
        .unwrap();
    assert_eq!(out.new_head, tip);
    assert_eq!(rev(&f, &f.repo, "HEAD"), tip);
    assert!(!f.repo.join(".git/MERGE_HEAD").exists());
}

#[test]
fn drop_change_takes_single_files_out_of_the_review() {
    let f = Fixture::new();
    f.write("keep.txt", "k1\nk2\n");
    f.write("gone.txt", "bye\n");
    f.write("old name.txt", &numbered("row", 20));
    f.commit_all("files");
    let repo = f.repo();
    let wt = f.tmp.join("drop");
    repo.add_worktree(&wt, "agent/drop", "main").unwrap();
    let inner = f.api().discover(&wt).unwrap().unwrap();

    // A committed edit, an uncommitted edit, an untracked file, a
    // deletion and a rename on disk.
    write_in(&wt, "a.txt", "one\nTWO\nthree\n");
    f.git_ok(&wt, &["commit", "-q", "-am", "agent edits a"]);
    write_in(&wt, "keep.txt", "k1\nk2 edited\n");
    write_in(&wt, "junk/cache.pyc", "\0binary\0");
    std::fs::remove_file(wt.join("gone.txt")).unwrap();
    std::fs::rename(wt.join("old name.txt"), wt.join("new name.txt")).unwrap();
    // A change the user keeps.
    write_in(&wt, "wanted.txt", "keep me\n");

    let files = inner.review_diff("main").unwrap();
    for p in [
        "a.txt",
        "keep.txt",
        "junk/cache.pyc",
        "gone.txt",
        "new name.txt",
    ] {
        let d = by_path(&files, p);
        inner
            .drop_change("main", d.old_path.as_deref(), d.new_path.as_deref())
            .unwrap_or_else(|e| panic!("dropping {p}: {e}"));
    }
    let files = inner.review_diff("main").unwrap();
    let paths: Vec<String> = files
        .iter()
        .map(|d| d.path().unwrap().display().to_string())
        .collect();
    assert_eq!(paths, ["wanted.txt"]);
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("gone.txt")).unwrap(),
        "bye\n"
    );
    assert!(!wt.join("junk/cache.pyc").exists());
    assert!(!wt.join("new name.txt").exists());
    assert!(wt.join("old name.txt").exists());

    // Each refusal changes nothing.
    let refused = |old: Option<&str>, new: Option<&str>, base: &str| {
        let before = snapshot(&f, &wt);
        let r = inner.drop_change(base, old.map(Path::new), new.map(Path::new));
        assert!(r.is_err(), "dropping {old:?} -> {new:?} was not refused");
        assert_eq!(
            snapshot(&f, &wt),
            before,
            "refusing {old:?} -> {new:?} changed files"
        );
    };
    refused(None, Some("../a.txt"), "main");
    let abs = wt.join("wanted.txt");
    refused(None, Some(abs.to_str().unwrap()), "main");
    refused(Some("a.txt"), None, "--all");
    refused(None, None, "main");
    // A directory where a file was: as the added path, as a modified file,
    // and as a deleted file whose name is now a directory.
    write_in(&wt, "somedir/x.txt", "x\n");
    refused(None, Some("somedir"), "main");
    std::fs::remove_file(wt.join("keep.txt")).unwrap();
    write_in(&wt, "keep.txt/inside.txt", "work\n");
    refused(Some("keep.txt"), Some("keep.txt"), "main");
    refused(Some("keep.txt"), None, "main");
    assert_eq!(
        std::fs::read_to_string(wt.join("keep.txt/inside.txt")).unwrap(),
        "work\n"
    );
    // A parent that is a symlink to somewhere outside the worktree.
    #[cfg(unix)]
    {
        let outside = f.tmp.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim.txt"), "keep\n").unwrap();
        std::os::unix::fs::symlink(&outside, wt.join("link")).unwrap();
        refused(None, Some("link/victim.txt"), "main");
        assert_eq!(
            std::fs::read_to_string(outside.join("victim.txt")).unwrap(),
            "keep\n"
        );
    }
}

/// A file replaced by a symlink shows as a deletion plus an addition of the
/// same path. Dropping either half brings the file back.
#[cfg(unix)]
#[test]
fn drop_change_undoes_a_file_turned_into_a_symlink() {
    let f = Fixture::new();
    f.write("cfg.txt", "real\n");
    f.commit_all("cfg");
    let repo = f.repo();
    let wt = f.tmp.join("typechange");
    repo.add_worktree(&wt, "agent/typechange", "main").unwrap();
    let inner = f.api().discover(&wt).unwrap().unwrap();
    std::fs::remove_file(wt.join("cfg.txt")).unwrap();
    std::os::unix::fs::symlink("a.txt", wt.join("cfg.txt")).unwrap();
    let files = inner.review_diff("main").unwrap();
    let added = files
        .iter()
        .find(|d| d.change == FileChange::Added)
        .unwrap_or_else(|| panic!("no added half in {files:#?}"));
    assert_eq!(added.old_path, None);
    inner
        .drop_change("main", None, added.new_path.as_deref())
        .unwrap();
    let meta = std::fs::symlink_metadata(wt.join("cfg.txt")).unwrap();
    assert!(meta.is_file(), "cfg.txt should be a file again");
    assert_eq!(
        std::fs::read_to_string(wt.join("cfg.txt")).unwrap(),
        "real\n"
    );
    assert!(inner.review_diff("main").unwrap().is_empty());
}

/// Git trusts an index entry's cached stats only when the file is older
/// than the index. A file rewritten with the same size in the same second
/// as the index was written is "racily clean" and must be read again; a
/// copy of the index must keep the original's time for that to work.
#[test]
fn index_copies_keep_racy_git_protection() {
    use std::time::{Duration, SystemTime};
    let f = Fixture::new();
    let wt = f.tmp.join("racy");
    f.repo().add_worktree(&wt, "maestro/racy", "main").unwrap();
    let past = SystemTime::now() - Duration::from_secs(30);
    let set = |p: &Path| {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(past)
            .unwrap()
    };
    // The entry caches `past`, and the index is from the same second.
    set(&wt.join("a.txt"));
    f.git_ok(&wt, &["update-index", "--refresh"]);
    let index = wt.join(f.git_ok(&wt, &["rev-parse", "--git-path", "index"]).trim());
    set(&index);
    // Same size, same time: only the contents say it changed.
    std::fs::write(wt.join("a.txt"), "ONE\ntwo\nthree\n").unwrap();
    set(&wt.join("a.txt"));
    // An untracked file makes the review diff use its index copy.
    write_in(&wt, "new.txt", "new\n");
    let repo = f.api().discover(&wt).unwrap().unwrap();
    let files = repo.review_diff("main").unwrap();
    assert_eq!(by_path(&files, "a.txt").change, FileChange::Modified);
    let snap = repo.work_snapshot().unwrap();
    assert_eq!(
        f.git_ok(&wt, &["show", &format!("{}:a.txt", snap.commit)]),
        "ONE\ntwo\nthree\n"
    );
}

#[test]
fn paths_touched_sees_changes_a_net_diff_hides() {
    let f = Fixture::new();
    let wt = agent_branch(&f, "maestro/t", "keep.txt", "k\n");
    // Changed and changed back: the net diff does not name a.txt.
    commit_in(&f, &wt, "a.txt", "changed\n", "change a");
    commit_in(&f, &wt, "a.txt", "one\ntwo\nthree\n", "restore a");
    f.git_ok(&wt, &["mv", "keep.txt", "moved name.txt"]);
    f.git_ok(&wt, &["commit", "-q", "-m", "move"]);
    let repo = f.api().discover(&wt).unwrap().unwrap();
    let net: Vec<_> = repo
        .review_diff("main")
        .unwrap()
        .iter()
        .filter_map(|d| d.path().cloned())
        .collect();
    assert_eq!(net, [PathBuf::from("moved name.txt")]);
    assert_eq!(
        repo.paths_touched("main", "HEAD").unwrap(),
        [
            PathBuf::from("a.txt"),
            PathBuf::from("keep.txt"),
            PathBuf::from("moved name.txt")
        ]
    );
    assert!(repo.paths_touched("HEAD", "HEAD").unwrap().is_empty());
    assert!(repo.paths_touched("-x", "HEAD").is_err());
}

#[test]
fn work_snapshots_hold_uncommitted_work_and_change_nothing() {
    let f = Fixture::new();
    f.write(".gitignore", "build/\n");
    f.commit_all("ignore build");
    let wt_a = agent_branch(&f, "maestro/a", "only-a.txt", "a\n");
    let wt_b = f.tmp.join("maestro-b");
    f.repo().add_worktree(&wt_b, "maestro/b", "main").unwrap();
    let api = f.api();
    let a = api.discover(&wt_a).unwrap().unwrap();
    let b = api.discover(&wt_b).unwrap().unwrap();

    // Uncommitted, untracked and ignored files in a.
    write_in(&wt_a, "a.txt", "one\nTWO from a\nthree\n");
    write_in(&wt_a, "new.txt", "untracked\n");
    write_in(&wt_a, "build/out.o", "ignored\n");
    let before = snapshot(&f, &wt_a);
    let snap = a.work_snapshot().unwrap();
    assert_eq!(
        snapshot(&f, &wt_a),
        before,
        "nothing in the worktree changes"
    );
    assert_eq!(
        f.git_ok(&wt_a, &["status", "--porcelain"]),
        " M a.txt\n?? new.txt\n"
    );
    let files = f.git_ok(&wt_a, &["ls-tree", "-r", "--name-only", &snap.commit]);
    assert_eq!(files, ".gitignore\na.txt\nnew.txt\nonly-a.txt\n");
    assert_eq!(
        f.git_ok(&wt_a, &["show", &format!("{}:a.txt", snap.commit)]),
        "one\nTWO from a\nthree\n"
    );
    assert_eq!(
        f.git_ok(&wt_a, &["rev-parse", &format!("{}^", snap.commit)])
            .trim(),
        rev(&f, &wt_a, "HEAD")
    );
    assert_eq!(
        f.git_ok(&wt_a, &["rev-parse", &format!("{}^{{tree}}", snap.commit)])
            .trim(),
        snap.tree
    );
    // No ref names it.
    assert_eq!(f.git(&["for-each-ref", "--contains", &snap.commit]), "");
    // The same state gives the same commit; a change gives another.
    assert_eq!(a.work_snapshot().unwrap(), snap);
    write_in(&wt_a, "new.txt", "changed\n");
    assert_ne!(a.work_snapshot().unwrap(), snap);
    write_in(&wt_a, "new.txt", "untracked\n");
    assert_eq!(a.work_snapshot().unwrap(), snap);

    // Dry runs between two worktrees' uncommitted states: compatible
    // edits merge, edits to the same line conflict.
    write_in(&wt_b, "a.txt", "one\ntwo\nthree\nfour from b\n");
    let sb = b.work_snapshot().unwrap();
    let main = f.repo();
    assert!(
        main.merge_check(&snap.commit, &sb.commit)
            .unwrap()
            .is_clean()
    );
    write_in(&wt_b, "a.txt", "one\nTWO from b\nthree\n");
    let sb = b.work_snapshot().unwrap();
    match main.merge_check(&snap.commit, &sb.commit).unwrap() {
        MergeCheck::Conflicts { files, .. } => assert_eq!(files, [PathBuf::from("a.txt")]),
        other => panic!("expected a conflict, got {other:?}"),
    }

    // commit_tree is deterministic and moves nothing.
    let head = rev(&f, &f.repo, "HEAD");
    let c1 = main.commit_tree(&snap.tree, &[&head], "x").unwrap();
    let c2 = main.commit_tree(&snap.tree, &[&head], "x").unwrap();
    assert_eq!(c1, c2);
    assert_eq!(rev(&f, &f.repo, "HEAD"), head);
    assert!(main.commit_tree("-x", &[], "x").is_err());
}

/// main gets `pre.txt`, an agent branch starts there and commits
/// `one.txt`, then main is moved back to the first commit: the agent's
/// base is no longer on main. Returns the worktree, the base and main.
fn base_off_main(f: &Fixture) -> (PathBuf, String, String) {
    f.write("pre.txt", "pre\n");
    f.commit_all("pre");
    let base = rev(f, &f.repo, "HEAD");
    let wt = agent_branch(f, "agent/r", "one.txt", "1\n");
    f.git(&["reset", "-q", "--hard", "HEAD~1"]);
    f.write("main.txt", "main\n");
    f.commit_all("main moves on");
    let main = rev(f, &f.repo, "HEAD");
    (wt, base, main)
}

#[test]
fn commits_between_names_what_the_base_has_and_main_lacks() {
    let f = Fixture::new();
    let (_wt, base, main) = base_off_main(&f);
    let c = f.repo().commits_between(&main, &base, 5).unwrap();
    assert_eq!(c.total, 1);
    assert_eq!(c.shown.len(), 1);
    assert_eq!(c.shown[0].id, base);
    assert_eq!(c.shown[0].subject, "pre");
    assert!(base.starts_with(&c.shown[0].short));
    let none = f.repo().commits_between(&base, &main, 0).unwrap();
    assert_eq!(none.total, 1);
    assert!(none.shown.is_empty());
}

#[test]
fn rebase_onto_moves_only_the_branchs_commits_and_keeps_the_old_tip() {
    let f = Fixture::new();
    let (wt, base, main) = base_off_main(&f);
    let repo = f.api().discover(&wt).unwrap().unwrap();
    let old_tip = rev(&f, &wt, "HEAD");
    // An untracked file stays where it is.
    write_in(&wt, "stray.txt", "stray\n");
    match repo.rebase_check(&main, &base, &old_tip, None).unwrap() {
        maestro_git::RebaseCheck::Clean { kept, .. } => {
            assert_eq!(kept, std::slice::from_ref(&old_tip))
        }
        other => panic!("{other:?}"),
    }
    let out = repo
        .rebase_onto(maestro_git::RebaseOnto {
            branch: "agent/r",
            upstream: &base,
            onto: &main,
            tip: &old_tip,
            keep_old_tip_as: "agent/r-before-rebase",
            undo_to: None,
        })
        .unwrap();
    assert_eq!(out.commits.len(), 1);
    assert_eq!(rev(&f, &wt, "HEAD"), out.new_tip);
    assert_eq!(rev(&f, &wt, "HEAD~1"), main);
    assert_eq!(rev(&f, &f.repo, "agent/r-before-rebase"), old_tip);
    assert!(
        !wt.join("pre.txt").exists(),
        "pre.txt came from the old base"
    );
    assert!(wt.join("one.txt").exists());
    assert!(wt.join("stray.txt").exists());
    // A second rebase may not reuse the old-tip name.
    let again = repo.rebase_onto(maestro_git::RebaseOnto {
        branch: "agent/r",
        upstream: &main,
        onto: &main,
        tip: &out.new_tip,
        keep_old_tip_as: "agent/r-before-rebase",
        undo_to: None,
    });
    assert!(
        matches!(again, Err(maestro_git::RebaseError::Refused(_))),
        "{again:?}"
    );
    assert_eq!(rev(&f, &wt, "HEAD"), out.new_tip);
}

#[test]
fn a_conflicting_rebase_is_aborted_and_the_branch_put_back() {
    let f = Fixture::new();
    let (wt, base, _) = base_off_main(&f);
    // main changes the line the agent changes.
    f.write("one.txt", "main's one\n");
    f.commit_all("main's one");
    let main = rev(&f, &f.repo, "HEAD");
    let repo = f.api().discover(&wt).unwrap().unwrap();
    let before_commit = rev(&f, &wt, "HEAD");
    // The caller's commit of uncommitted work, which a failure undoes.
    write_in(&wt, "two.txt", "2\n");
    f.git_ok(&wt, &["add", "-A"]);
    f.git_ok(&wt, &["commit", "-q", "-m", "work"]);
    let tip = rev(&f, &wt, "HEAD");
    match repo.rebase_check(&main, &base, &tip, None).unwrap() {
        maestro_git::RebaseCheck::Conflicts(files) => {
            assert_eq!(files, [PathBuf::from("one.txt")])
        }
        other => panic!("{other:?}"),
    }
    let r = repo.rebase_onto(maestro_git::RebaseOnto {
        branch: "agent/r",
        upstream: &base,
        onto: &main,
        tip: &tip,
        keep_old_tip_as: "agent/r-before-rebase",
        undo_to: Some(&before_commit),
    });
    match r {
        Err(maestro_git::RebaseError::Conflicts(files)) => {
            assert_eq!(files, [PathBuf::from("one.txt")])
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rev(&f, &wt, "HEAD"), before_commit);
    assert_eq!(repo.head().unwrap(), Head::Branch("agent/r".into()));
    assert_eq!(repo.operation().unwrap(), None);
    assert_eq!(std::fs::read_to_string(wt.join("one.txt")).unwrap(), "1\n");
    assert_eq!(std::fs::read_to_string(wt.join("two.txt")).unwrap(), "2\n");
    // The undone commit's file is staged, as before it.
    assert_eq!(f.git_ok(&wt, &["status", "--porcelain"]), "A  two.txt\n");
    assert!(
        !f.git_in(
            &f.repo,
            &["rev-parse", "--verify", "--quiet", "agent/r-before-rebase"]
        )
        .status
        .success(),
        "the old-tip branch is deleted again"
    );
}

#[test]
fn rebase_onto_refuses_a_dirty_worktree_or_a_moved_branch() {
    let f = Fixture::new();
    let (wt, base, main) = base_off_main(&f);
    let repo = f.api().discover(&wt).unwrap().unwrap();
    let tip = rev(&f, &wt, "HEAD");
    write_in(&wt, "one.txt", "changed\n");
    let r = repo.rebase_onto(maestro_git::RebaseOnto {
        branch: "agent/r",
        upstream: &base,
        onto: &main,
        tip: &tip,
        keep_old_tip_as: "agent/r-before-rebase",
        undo_to: None,
    });
    assert!(
        matches!(r, Err(maestro_git::RebaseError::Refused(ref m)) if m.contains("uncommitted")),
        "{r:?}"
    );
    f.git_ok(&wt, &["checkout", "-q", "--", "one.txt"]);
    let r = repo.rebase_onto(maestro_git::RebaseOnto {
        branch: "agent/r",
        upstream: &base,
        onto: &main,
        tip: &base,
        keep_old_tip_as: "agent/r-before-rebase",
        undo_to: None,
    });
    assert!(
        matches!(r, Err(maestro_git::RebaseError::Refused(_))),
        "{r:?}"
    );
    assert_eq!(rev(&f, &wt, "HEAD"), tip);
    assert!(
        !f.git_in(
            &f.repo,
            &["rev-parse", "--verify", "--quiet", "agent/r-before-rebase"]
        )
        .status
        .success()
    );
}

#[test]
fn rebase_onto_refuses_an_ignored_file_that_onto_tracks() {
    let f = Fixture::new();
    let (wt, base, _) = base_off_main(&f);
    // main starts tracking local.txt; the agent's worktree has its own,
    // ignored there, which a checkout would overwrite without asking.
    f.write("local.txt", "main's\n");
    f.commit_all("main tracks local.txt");
    let main = rev(&f, &f.repo, "HEAD");
    let info = PathBuf::from(
        f.git_ok(&wt, &["rev-parse", "--git-path", "info/exclude"])
            .trim(),
    );
    let info = if info.is_absolute() {
        info
    } else {
        wt.join(info)
    };
    std::fs::create_dir_all(info.parent().unwrap()).unwrap();
    std::fs::write(&info, "local.txt\n").unwrap();
    write_in(&wt, "local.txt", "only copy\n");
    let repo = f.api().discover(&wt).unwrap().unwrap();
    let tip = rev(&f, &wt, "HEAD");
    let r = repo.rebase_onto(maestro_git::RebaseOnto {
        branch: "agent/r",
        upstream: &base,
        onto: &main,
        tip: &tip,
        keep_old_tip_as: "agent/r-before-rebase",
        undo_to: None,
    });
    assert!(
        matches!(r, Err(maestro_git::RebaseError::Refused(ref m)) if m.contains("local.txt")),
        "{r:?}"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("local.txt")).unwrap(),
        "only copy\n"
    );
    assert_eq!(rev(&f, &wt, "HEAD"), tip);
}

#[test]
fn a_rebase_a_hook_refuses_is_put_back() {
    let f = Fixture::new();
    let (wt, base, main) = base_off_main(&f);
    let before_commit = rev(&f, &wt, "HEAD");
    write_in(&wt, "two.txt", "2\n");
    f.git_ok(&wt, &["add", "-A"]);
    f.git_ok(&wt, &["commit", "-q", "-m", "work"]);
    let tip = rev(&f, &wt, "HEAD");
    let hooks = f.repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-rebase");
    std::fs::write(&hook, "#!/bin/sh\necho no rebases here >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let repo = f.api().discover(&wt).unwrap().unwrap();
    let r = repo.rebase_onto(maestro_git::RebaseOnto {
        branch: "agent/r",
        upstream: &base,
        onto: &main,
        tip: &tip,
        keep_old_tip_as: "agent/r-before-rebase",
        undo_to: Some(&before_commit),
    });
    assert!(matches!(r, Err(maestro_git::RebaseError::Git(_))), "{r:?}");
    assert_eq!(rev(&f, &wt, "HEAD"), before_commit);
    assert_eq!(f.git_ok(&wt, &["status", "--porcelain"]), "A  two.txt\n");
    assert!(
        !f.git_in(
            &f.repo,
            &["rev-parse", "--verify", "--quiet", "agent/r-before-rebase"]
        )
        .status
        .success()
    );
}
