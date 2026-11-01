//! Commits built from chosen hunks, against real repositories built with
//! the git CLI. Each test has its own HOME and config, as in `repos.rs`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use maestro_git::{DiffFile, DiffLineKind, Git, HunkPick, HunksError, Repository};

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
    /// A repository whose first commit holds `files`.
    fn new(files: &[(&str, &str)]) -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("maestro-hunks-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let f = Fixture { tmp, repo };
        f.git(&f.repo, &["init", "-q"]);
        for (p, t) in files {
            write(&f.repo, p, t);
        }
        f.git(&f.repo, &["add", "-A"]);
        f.git(&f.repo, &["commit", "-q", "-m", "initial"]);
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

    fn open(&self, dir: &Path) -> Repository {
        self.api().discover(dir).unwrap().expect("repo")
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        for c in CONFIG {
            cmd.arg("-c").arg(c);
        }
        let out = cmd
            .args(args)
            .current_dir(dir)
            .env("HOME", self.tmp.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.tmp.join("home/.gitconfig"))
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
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A worktree on a new branch from `HEAD`.
    fn worktree(&self, name: &str) -> PathBuf {
        let path = self.tmp.join(name);
        self.git(
            &self.repo,
            &["worktree", "add", "-q", "-b", name, path.to_str().unwrap()],
        );
        path
    }

    fn head(&self) -> String {
        self.git(&self.repo, &["rev-parse", "HEAD"])
            .trim()
            .to_string()
    }

    /// The bytes of `path` in `rev`, or `None` when it has no such file.
    fn show(&self, rev: &str, path: &str) -> Option<Vec<u8>> {
        let mut cmd = Command::new("git");
        let out = cmd
            .args(["cat-file", "blob", &format!("{rev}:{path}")])
            .current_dir(&self.repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", self.tmp.join("home"))
            .output()
            .unwrap();
        out.status.success().then_some(out.stdout)
    }
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

fn file<'a>(files: &'a [DiffFile], path: &str) -> &'a DiffFile {
    files
        .iter()
        .find(|f| f.path().is_some_and(|p| p == Path::new(path)))
        .unwrap_or_else(|| panic!("no {path} in {files:?}"))
}

fn whole(f: &DiffFile, i: usize) -> HunkPick {
    HunkPick::new("agent", f, i, None).unwrap()
}

/// The pick of lines of hunk `i` whose text is in `texts` (added or
/// removed lines only).
fn lines_of(f: &DiffFile, i: usize, texts: &[&str]) -> HunkPick {
    let h = &f.hunks[i];
    let sel: BTreeSet<usize> = h
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind != DiffLineKind::Context && texts.contains(&l.text.as_str()))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(sel.len(), texts.len(), "{h:?}");
    HunkPick::new("agent", f, i, Some(sel)).unwrap()
}

/// Builds a commit of `picks` on `base` and returns it.
fn commit(repo: &Repository, base: &str, picks: &[HunkPick]) -> String {
    let tree = repo.tree_from_hunks(base, picks).unwrap();
    repo.commit_on(&tree, base, "part").unwrap()
}

fn refusals(err: HunksError) -> Vec<(usize, String)> {
    match err {
        HunksError::DoNotApply(f) => f.into_iter().map(|f| (f.pick, f.reason)).collect(),
        other => panic!("expected a refusal, got {other}"),
    }
}

#[test]
fn whole_hunks_added_and_deleted_files_make_a_subset_commit() {
    let f = Fixture::new(&[("a.txt", &numbered(30)), ("gone.txt", "bye\n")]);
    let base = f.head();
    let wt = f.worktree("agent");
    // Two hunks in a.txt (lines 2 and 28), an added file, a deleted one;
    // some of it committed, some not, one file untracked.
    let text = numbered(30).replace("line 2\n", "line two\n");
    write(&wt, "a.txt", &text);
    f.git(&wt, &["commit", "-qam", "agent work"]);
    write(
        &wt,
        "a.txt",
        &text.replace("line 28\n", "line twenty-eight\n"),
    );
    write(&wt, "dir/new.txt", "fresh\nfile\n");
    std::fs::remove_file(wt.join("gone.txt")).unwrap();
    let agent = f.open(&wt);
    let status_before = f.git(&wt, &["status", "--porcelain"]);
    let files = agent.review_diff(&base).unwrap();
    let a = file(&files, "a.txt");
    assert_eq!(a.hunks.len(), 2, "{a:?}");
    let picks = [
        whole(a, 0),
        whole(file(&files, "dir/new.txt"), 0),
        whole(file(&files, "gone.txt"), 0),
    ];
    let main = f.open(&f.repo);
    let c = commit(&main, &base, &picks);
    // Exactly the picked hunks: line 2 changed, line 28 not.
    let want = numbered(30).replace("line 2\n", "line two\n");
    assert_eq!(f.show(&c, "a.txt").unwrap(), want.as_bytes());
    assert_eq!(f.show(&c, "dir/new.txt").unwrap(), b"fresh\nfile\n");
    assert_eq!(f.show(&c, "gone.txt"), None);
    let parent = f.git(&f.repo, &["rev-parse", &format!("{c}^")]);
    assert_eq!(parent.trim(), base);
    let author = f.git(&f.repo, &["log", "-1", "--format=%an <%ae>", &c]);
    assert_eq!(author.trim(), "Maestro Test <test@example.invalid>");
    // The same picks give the same tree; nothing in either worktree moved.
    assert_eq!(
        main.tree_from_hunks(&base, &picks).unwrap(),
        f.git(&f.repo, &["rev-parse", &format!("{c}^{{tree}}")])
            .trim()
    );
    assert_eq!(f.git(&wt, &["status", "--porcelain"]), status_before);
    assert_eq!(f.git(&f.repo, &["status", "--porcelain"]), "");
    // A branch names it; an existing branch is never moved.
    main.create_branch("maestro/agent-part-1", &c).unwrap();
    assert!(main.branch_exists("maestro/agent-part-1").unwrap());
    let err = main
        .create_branch("maestro/agent-part-1", &base)
        .unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    let at = f.git(&f.repo, &["rev-parse", "maestro/agent-part-1"]);
    assert_eq!(at.trim(), c);
}

#[test]
fn line_ranges_take_some_lines_of_a_hunk() {
    let f = Fixture::new(&[("a.txt", "x1\nx2\nx3\n"), ("old.txt", "o1\no2\no3\n")]);
    let base = f.head();
    let wt = f.worktree("agent");
    write(&wt, "a.txt", "x1\nX2\nnew\nx3\n");
    write(&wt, "add.txt", "n1\nn2\nn3\n");
    std::fs::remove_file(wt.join("old.txt")).unwrap();
    let files = f.open(&wt).review_diff(&base).unwrap();
    let main = f.open(&f.repo);
    let a = file(&files, "a.txt");
    // Only the inserted line: the replaced line stays as it was.
    let c = commit(&main, &base, &[lines_of(a, 0, &["new"])]);
    assert_eq!(f.show(&c, "a.txt").unwrap(), b"x1\nx2\nnew\nx3\n");
    // Only the replacement, not the insertion.
    let c = commit(&main, &base, &[lines_of(a, 0, &["x2", "X2"])]);
    assert_eq!(f.show(&c, "a.txt").unwrap(), b"x1\nX2\nx3\n");
    // Only the removal: the line goes and nothing replaces it.
    let c = commit(&main, &base, &[lines_of(a, 0, &["x2"])]);
    assert_eq!(f.show(&c, "a.txt").unwrap(), b"x1\nx3\n");
    // Part of an added file adds the file with those lines.
    let c = commit(
        &main,
        &base,
        &[lines_of(file(&files, "add.txt"), 0, &["n1", "n3"])],
    );
    assert_eq!(f.show(&c, "add.txt").unwrap(), b"n1\nn3\n");
    // Part of a deleted file keeps the file without those lines.
    let c = commit(
        &main,
        &base,
        &[lines_of(file(&files, "old.txt"), 0, &["o2"])],
    );
    assert_eq!(f.show(&c, "old.txt").unwrap(), b"o1\no3\n");
    // A label says how much of the hunk is taken.
    let label = lines_of(a, 0, &["new"]).label();
    assert_eq!(label, "a.txt -1,3 +1,4 (1 of 3 changed lines)");
    assert_eq!(whole(a, 0).label(), "a.txt -1,3 +1,4");
    // A selection of a context line is refused.
    let bad = HunkPick::new("agent", a, 0, Some(BTreeSet::from([0]))).unwrap();
    let r = refusals(main.tree_from_hunks(&base, &[bad]).unwrap_err());
    assert!(r[0].1.contains("not an added or removed line"), "{r:?}");
}

#[test]
fn a_hunk_that_needs_an_unpicked_one_is_refused_by_name() {
    // The agent turns the file `lib` into a directory: adding `lib/mod.rs`
    // needs the removal of the file `lib`.
    let f = Fixture::new(&[("lib", "a file\n"), ("b.txt", "b\n")]);
    let base = f.head();
    let wt = f.worktree("agent");
    std::fs::remove_file(wt.join("lib")).unwrap();
    write(&wt, "lib/mod.rs", "pub fn x() {}\n");
    write(&wt, "b.txt", "B\n");
    let files = f.open(&wt).review_diff(&base).unwrap();
    let main = f.open(&f.repo);
    let add = whole(file(&files, "lib/mod.rs"), 0);
    let b = whole(file(&files, "b.txt"), 0);
    let r = refusals(
        main.tree_from_hunks(&base, &[b.clone(), add.clone()])
            .unwrap_err(),
    );
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].0, 1, "the add is named, not b.txt");
    assert!(r[0].1.contains("lib is a file at the base"), "{r:?}");
    assert!(r[0].1.contains("removal of lib"), "{r:?}");
    // With the removal it applies.
    let rm = whole(file(&files, "lib"), 0);
    let c = commit(&main, &base, &[b, add, rm]);
    assert_eq!(f.show(&c, "lib/mod.rs").unwrap(), b"pub fn x() {}\n");
    assert_eq!(f.show(&c, "b.txt").unwrap(), b"B\n");

    // A hunk made against other text (here: after a change on main that
    // it does not know) is refused, not applied at a shifted place.
    f.git(&f.repo, &["rm", "-q", "b.txt"]);
    write(&f.repo, "b.txt", "changed on main\n");
    f.git(&f.repo, &["add", "-A"]);
    f.git(&f.repo, &["commit", "-qm", "main moves"]);
    let moved = f.head();
    let b = whole(file(&files, "b.txt"), 0);
    let r = refusals(main.tree_from_hunks(&moved, &[b]).unwrap_err());
    assert!(
        r[0].1.contains("line 1 of b.txt at the base is not what"),
        "{r:?}"
    );
}

#[test]
fn picks_from_two_worktrees_conflict_only_where_they_touch() {
    let base_text = numbered(30);
    let f = Fixture::new(&[("a.txt", &base_text)]);
    let base = f.head();
    let one = f.worktree("one");
    let two = f.worktree("two");
    write(&one, "a.txt", &base_text.replace("line 3\n", "ONE 3\n"));
    write(
        &two,
        "a.txt",
        &base_text
            .replace("line 4\n", "TWO 4\n")
            .replace("line 25\n", "TWO 25\n"),
    );
    let f1 = f.open(&one).review_diff(&base).unwrap();
    let f2 = f.open(&two).review_diff(&base).unwrap();
    let pick = |source: &str, files: &[DiffFile], i| {
        HunkPick::new(source, file(files, "a.txt"), i, None).unwrap()
    };
    let main = f.open(&f.repo);
    // Lines 3 and 4 are neighbors: both picks are named, each with the other.
    let r = refusals(
        main.tree_from_hunks(&base, &[pick("one", &f1, 0), pick("two", &f2, 0)])
            .unwrap_err(),
    );
    assert_eq!(r.len(), 2, "{r:?}");
    assert!(
        r[0].1.contains("conflicts with a.txt -1,7 +1,7 from two"),
        "{r:?}"
    );
    assert!(r[1].1.contains("from one"), "{r:?}");
    // Line 3 from one and line 25 from two compose.
    let c = commit(&main, &base, &[pick("one", &f1, 0), pick("two", &f2, 1)]);
    let want = base_text
        .replace("line 3\n", "ONE 3\n")
        .replace("line 25\n", "TWO 25\n");
    assert_eq!(f.show(&c, "a.txt").unwrap(), want.as_bytes());
}

#[test]
fn files_without_a_final_newline() {
    let f = Fixture::new(&[("a.txt", "a\nb"), ("c.txt", "c\nd\n")]);
    let base = f.head();
    let wt = f.worktree("agent");
    write(&wt, "a.txt", "a\nb\nc");
    write(&wt, "c.txt", "c\nd");
    let files = f.open(&wt).review_diff(&base).unwrap();
    let main = f.open(&f.repo);
    let a = file(&files, "a.txt");
    // The whole hunk: the old last line gets its newline, a new one comes.
    let c = commit(&main, &base, &[whole(a, 0)]);
    assert_eq!(f.show(&c, "a.txt").unwrap(), b"a\nb\nc");
    // Only the line after it: it would follow a line with no newline.
    let r = refusals(
        main.tree_from_hunks(&base, &[lines_of(a, 0, &["c"])])
            .unwrap_err(),
    );
    assert!(r[0].1.contains("no newline at its end"), "{r:?}");
    // Only the newline fix of the old last line.
    let fix = HunkPick::new(
        "agent",
        a,
        0,
        Some(
            a.hunks[0]
                .lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.text == "b" && l.kind != DiffLineKind::Context)
                .map(|(i, _)| i)
                .collect(),
        ),
    )
    .unwrap();
    let c = commit(&main, &base, &[fix]);
    assert_eq!(f.show(&c, "a.txt").unwrap(), b"a\nb\n");
    // Dropping the final newline.
    let c = commit(&main, &base, &[whole(file(&files, "c.txt"), 0)]);
    assert_eq!(f.show(&c, "c.txt").unwrap(), b"c\nd");
}

#[test]
fn crlf_lines_keep_their_carriage_returns() {
    let f = Fixture::new(&[("w.txt", "one\r\ntwo\r\nthree\r\n")]);
    let base = f.head();
    let wt = f.worktree("agent");
    write(&wt, "w.txt", "one\r\nTWO\r\nthree\r\n");
    let files = f.open(&wt).review_diff(&base).unwrap();
    let main = f.open(&f.repo);
    let c = commit(&main, &base, &[whole(file(&files, "w.txt"), 0)]);
    assert_eq!(f.show(&c, "w.txt").unwrap(), b"one\r\nTWO\r\nthree\r\n");
}

#[test]
fn a_file_turned_into_a_symlink_needs_both_its_hunks() {
    let f = Fixture::new(&[("link", "was a file\n"), ("target.txt", "t\n")]);
    let base = f.head();
    let wt = f.worktree("agent");
    std::fs::remove_file(wt.join("link")).unwrap();
    std::os::unix::fs::symlink("target.txt", wt.join("link")).unwrap();
    let files = f.open(&wt).review_diff(&base).unwrap();
    // Git shows the type change as a deletion and an addition of `link`.
    let gone = files.iter().position(|d| d.new_path.is_none()).unwrap();
    let new = files.iter().position(|d| d.old_path.is_none()).unwrap();
    let main = f.open(&f.repo);
    let add = whole(&files[new], 0);
    let r = refusals(
        main.tree_from_hunks(&base, std::slice::from_ref(&add))
            .unwrap_err(),
    );
    assert!(
        r[0].1.contains("it adds link, which the base already has"),
        "{r:?}"
    );
    let c = commit(&main, &base, &[whole(&files[gone], 0), add]);
    let ls = f.git(&f.repo, &["ls-tree", &c, "link"]);
    assert!(ls.starts_with("120000 blob "), "{ls}");
    assert_eq!(f.show(&c, "link").unwrap(), b"target.txt");
}
