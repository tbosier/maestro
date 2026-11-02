//! Staging, patches, commits, log, blame, stash and remotes against real
//! repositories built with the git CLI. Same isolation as `repos.rs`: a
//! temporary HOME and global config, `GIT_CONFIG_NOSYSTEM=1`, no inherited
//! `-c` settings.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use maestro_git::{
    ApplyTo, DiffLineKind, DiffSpec, FileChange, FilePatch, Git, Operation, PatchError, Picks,
    RemoteOp, Side, TextPatch,
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
        let tmp = std::env::temp_dir().join(format!("maestro-stage-{}-{n}", std::process::id()));
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

    fn repo(&self) -> maestro_git::Repository {
        self.api().discover(&self.repo).unwrap().expect("repo")
    }

    fn raw(&self, dir: &Path, args: &[&str]) -> std::process::Output {
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

    fn git_in(&self, dir: &Path, args: &[&str]) -> Vec<u8> {
        let out = self.raw(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    fn git(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.git_in(&self.repo, args)).into_owned()
    }

    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let p = self.repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn commit_all(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
    }

    /// The staged bytes of `rel`.
    fn index(&self, rel: &str) -> Vec<u8> {
        self.git_in(&self.repo, &["show", &format!(":{rel}")])
    }

    fn unstaged(&self, rel: &str) -> FilePatch {
        let mut v = self
            .repo()
            .file_patches(&DiffSpec::Unstaged, &[PathBuf::from(rel)])
            .unwrap();
        assert_eq!(v.len(), 1, "one file in the unstaged diff");
        v.remove(0)
    }

    /// The staged diff of `rel`, from a diff of every file so renames
    /// are found (a path filter hides the other side of a rename).
    fn staged(&self, rel: &str) -> FilePatch {
        self.repo()
            .file_patches(&DiffSpec::Staged, &[])
            .unwrap()
            .into_iter()
            .find(|p| p.target_path() == Some(&PathBuf::from(rel)))
            .expect("the file is in the staged diff")
    }
}

/// Index of the first line of `kind` with `text` in hunk `h`.
fn line(p: &FilePatch, h: usize, kind: DiffLineKind, text: &str) -> usize {
    p.file.hunks[h]
        .lines
        .iter()
        .position(|l| l.kind == kind && l.text.trim_end_matches('\r') == text)
        .unwrap_or_else(|| panic!("no {kind:?} line {text:?} in {:#?}", p.file.hunks[h]))
}

fn file_state(p: &Path) -> (Vec<u8>, std::time::SystemTime) {
    (
        std::fs::read(p).unwrap(),
        std::fs::metadata(p).unwrap().modified().unwrap(),
    )
}

#[test]
fn line_staging_with_additions_next_to_deletions_leaves_the_file_alone() {
    let f = Fixture::new();
    f.write("f.txt", "one\ntwo\nthree\nfour\n");
    f.commit_all("init");
    f.write("f.txt", "one\nTWO\nthree\nnew\nfour\n");
    let path = f.repo.join("f.txt");
    let before = file_state(&path);
    let repo = f.repo();

    // Only the added "TWO", not the removal of "two" beside it.
    let p = f.unstaged("f.txt");
    let picks = Picks::new().lines(0, [line(&p, 0, DiffLineKind::Added, "TWO")]);
    let patch = p.build(&picks, Side::Forward).unwrap();
    repo.apply_patch(&patch, ApplyTo::Index, false).unwrap();
    assert_eq!(f.index("f.txt"), b"one\ntwo\nTWO\nthree\nfour\n");
    assert_eq!(
        file_state(&path),
        before,
        "the working tree file is untouched"
    );

    // Then the removal of "two" and "new", from a fresh diff.
    let p = f.unstaged("f.txt");
    let picks = Picks::new().lines(
        0,
        [
            line(&p, 0, DiffLineKind::Removed, "two"),
            line(&p, 0, DiffLineKind::Added, "new"),
        ],
    );
    repo.apply_patch(
        &p.build(&picks, Side::Forward).unwrap(),
        ApplyTo::Index,
        false,
    )
    .unwrap();
    assert_eq!(f.index("f.txt"), b"one\nTWO\nthree\nnew\nfour\n");
    assert_eq!(file_state(&path), before);
    assert_eq!(f.git(&["diff", "--name-only"]), "");

    // Unstage just "new": the index loses it, the file keeps it.
    let s = f.staged("f.txt");
    let picks = Picks::new().lines(0, [line(&s, 0, DiffLineKind::Added, "new")]);
    repo.apply_patch(
        &s.build(&picks, Side::Reverse).unwrap(),
        ApplyTo::Index,
        true,
    )
    .unwrap();
    assert_eq!(f.index("f.txt"), b"one\nTWO\nthree\nfour\n");
    assert_eq!(file_state(&path), before);

    // Discard the unstaged "new" from the working tree.
    let p = f.unstaged("f.txt");
    let picks = Picks::new().hunk(0);
    repo.apply_patch(
        &p.build(&picks, Side::Reverse).unwrap(),
        ApplyTo::Worktree,
        true,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"one\nTWO\nthree\nfour\n",
        "only the discarded line went"
    );
}

#[test]
fn picking_one_of_two_replaced_lines_keeps_the_order() {
    let f = Fixture::new();
    f.write("r.txt", "one\ntwo\nthree\nfour\n");
    f.commit_all("init");
    f.write("r.txt", "one\nTWO\nTHREE\nfour\n");
    let repo = f.repo();
    // The first replacement only.
    let p = f.unstaged("r.txt");
    let picks = Picks::new().lines(
        0,
        [
            line(&p, 0, DiffLineKind::Removed, "two"),
            line(&p, 0, DiffLineKind::Added, "TWO"),
        ],
    );
    repo.apply_patch(
        &p.build(&picks, Side::Forward).unwrap(),
        ApplyTo::Index,
        false,
    )
    .unwrap();
    assert_eq!(f.index("r.txt"), b"one\nTWO\nthree\nfour\n");
    // Unstage it again, then stage the second replacement only.
    let s = f.staged("r.txt");
    repo.apply_patch(
        &s.build(&Picks::new().hunk(0), Side::Reverse).unwrap(),
        ApplyTo::Index,
        true,
    )
    .unwrap();
    let p = f.unstaged("r.txt");
    let picks = Picks::new().lines(
        0,
        [
            line(&p, 0, DiffLineKind::Removed, "three"),
            line(&p, 0, DiffLineKind::Added, "THREE"),
        ],
    );
    repo.apply_patch(
        &p.build(&picks, Side::Forward).unwrap(),
        ApplyTo::Index,
        false,
    )
    .unwrap();
    assert_eq!(f.index("r.txt"), b"one\ntwo\nTHREE\nfour\n");
    // Unstaging the first of two staged replacements keeps the second.
    f.git(&["add", "r.txt"]);
    let s = f.staged("r.txt");
    let picks = Picks::new().lines(
        0,
        [
            line(&s, 0, DiffLineKind::Removed, "two"),
            line(&s, 0, DiffLineKind::Added, "TWO"),
        ],
    );
    repo.apply_patch(
        &s.build(&picks, Side::Reverse).unwrap(),
        ApplyTo::Index,
        true,
    )
    .unwrap();
    assert_eq!(f.index("r.txt"), b"one\ntwo\nTHREE\nfour\n");
}

#[test]
fn a_file_without_a_trailing_newline() {
    let f = Fixture::new();
    f.write("n.txt", "a\nb\nc");
    f.commit_all("init");
    f.write("n.txt", "a\nB\nc");
    let repo = f.repo();
    let p = f.unstaged("n.txt");
    let picks = Picks::new().lines(
        0,
        [
            line(&p, 0, DiffLineKind::Removed, "b"),
            line(&p, 0, DiffLineKind::Added, "B"),
        ],
    );
    repo.apply_patch(
        &p.build(&picks, Side::Forward).unwrap(),
        ApplyTo::Index,
        false,
    )
    .unwrap();
    assert_eq!(f.index("n.txt"), b"a\nB\nc");

    // Appending after a last line without a newline changes that line too.
    f.commit_all("B");
    f.write("n.txt", "a\nB\nc\nd");
    let p = f.unstaged("n.txt");
    let d = line(&p, 0, DiffLineKind::Added, "d");
    assert_eq!(
        p.build(&Picks::new().lines(0, [d]), Side::Forward),
        Err(PatchError::JoinsLines)
    );
    // Giving "c" its newline alone works and leaves "d" unstaged.
    let picks = Picks::new().lines(
        0,
        [
            line(&p, 0, DiffLineKind::Removed, "c"),
            line(&p, 0, DiffLineKind::Added, "c"),
        ],
    );
    repo.apply_patch(
        &p.build(&picks, Side::Forward).unwrap(),
        ApplyTo::Index,
        false,
    )
    .unwrap();
    assert_eq!(f.index("n.txt"), b"a\nB\nc\n");
    assert_eq!(std::fs::read(f.repo.join("n.txt")).unwrap(), b"a\nB\nc\nd");
}

#[test]
fn crlf_lines_are_staged_byte_for_byte() {
    let f = Fixture::new();
    f.write("w.txt", "a\r\nb\r\nc\r\n");
    f.commit_all("init");
    f.write("w.txt", "a\r\nB\r\nc\r\nd\r\n");
    let repo = f.repo();
    let p = f.unstaged("w.txt");
    let picks = Picks::new().lines(
        0,
        [
            line(&p, 0, DiffLineKind::Removed, "b"),
            line(&p, 0, DiffLineKind::Added, "B"),
        ],
    );
    repo.apply_patch(
        &p.build(&picks, Side::Forward).unwrap(),
        ApplyTo::Index,
        false,
    )
    .unwrap();
    assert_eq!(f.index("w.txt"), b"a\r\nB\r\nc\r\n");
    assert_eq!(
        std::fs::read(f.repo.join("w.txt")).unwrap(),
        b"a\r\nB\r\nc\r\nd\r\n"
    );
}

#[test]
fn unstaging_lines_of_a_renamed_file_keeps_the_rename() {
    let f = Fixture::new();
    let body: String = (1..=12).map(|i| format!("line {i}\n")).collect();
    f.write("old.txt", &body);
    f.commit_all("init");
    f.git(&["mv", "old.txt", "new.txt"]);
    f.write("new.txt", body.replace("line 2\n", "LINE 2\n"));
    f.git(&["add", "new.txt"]);
    let s = f.staged("new.txt");
    assert!(
        matches!(s.file.change, FileChange::Renamed { .. }),
        "{:?}",
        s.file.change
    );
    let picks = Picks::new().lines(
        0,
        [
            line(&s, 0, DiffLineKind::Removed, "line 2"),
            line(&s, 0, DiffLineKind::Added, "LINE 2"),
        ],
    );
    f.repo()
        .apply_patch(
            &s.build(&picks, Side::Reverse).unwrap(),
            ApplyTo::Index,
            true,
        )
        .unwrap();
    assert_eq!(f.index("new.txt"), body.as_bytes());
    let staged = f.git(&["diff", "--cached", "--name-status", "-M"]);
    assert!(staged.starts_with("R100\told.txt\tnew.txt"), "{staged}");
    assert!(
        std::fs::read_to_string(f.repo.join("new.txt"))
            .unwrap()
            .contains("LINE 2")
    );
}

#[test]
fn binary_files_are_refused_for_line_staging_but_stage_whole() {
    let f = Fixture::new();
    f.write("img.bin", [0u8, 1, 2, 3, 0, 255]);
    f.commit_all("init");
    f.write("img.bin", [0u8, 9, 9, 0, 1]);
    let p = f.unstaged("img.bin");
    assert!(p.file.binary);
    assert_eq!(
        p.build(&Picks::new().hunk(0), Side::Forward),
        Err(PatchError::Binary)
    );
    f.repo().stage_paths(&[Path::new("img.bin")]).unwrap();
    assert_eq!(f.index("img.bin"), [0u8, 9, 9, 0, 1]);
}

#[test]
fn whole_file_stage_unstage_and_discard() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.write("b.txt", "b\n");
    f.commit_all("init");
    f.write("a.txt", "A\n");
    f.write("new.txt", "new\n");
    std::fs::remove_file(f.repo.join("b.txt")).unwrap();
    let repo = f.repo();
    repo.stage_paths(&[Path::new("a.txt"), Path::new("b.txt"), Path::new("new.txt")])
        .unwrap();
    assert_eq!(
        f.git(&["diff", "--cached", "--name-status"]),
        "M\ta.txt\nD\tb.txt\nA\tnew.txt\n"
    );
    repo.unstage_paths(&[Path::new("a.txt"), Path::new("b.txt")])
        .unwrap();
    assert_eq!(
        f.git(&["diff", "--cached", "--name-status"]),
        "A\tnew.txt\n"
    );
    // Discarding staged new.txt removes it from index and disk; discarding
    // the unstaged a.txt and b.txt brings back the committed text.
    repo.discard_staged_paths(&[Path::new("new.txt")]).unwrap();
    assert!(!f.repo.join("new.txt").exists());
    repo.discard_paths(&[Path::new("a.txt"), Path::new("b.txt")])
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "a\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.repo.join("b.txt")).unwrap(),
        "b\n"
    );

    // Untracked files: deleted only when git calls them untracked.
    f.write("junk.txt", "x\n");
    assert!(repo.remove_untracked(Path::new("a.txt")).is_err());
    assert!(repo.remove_untracked(Path::new("../escape")).is_err());
    repo.remove_untracked(Path::new("junk.txt")).unwrap();
    assert!(!f.repo.join("junk.txt").exists());
}

#[test]
fn text_patches_stage_one_sign_change_from_a_file() {
    let f = Fixture::new();
    let body: String = (1..=20).map(|i| format!("{i}\n")).collect();
    f.write("t.txt", &body);
    f.commit_all("init");
    let new = body.replace("2\n", "TWO\n").replace("19\n", "NINETEEN\n");
    f.write("t.txt", &new);
    let tp = TextPatch::new(Path::new("t.txt"), body.as_bytes(), new.as_bytes()).unwrap();
    let change = tp.change_at(18).unwrap();
    let patch = tp
        .patch
        .build(&tp.pick_change(change), Side::Forward)
        .unwrap();
    f.repo().apply_patch(&patch, ApplyTo::Index, false).unwrap();
    assert_eq!(
        f.index("t.txt"),
        body.replace("19\n", "NINETEEN\n").as_bytes()
    );
}

#[test]
fn commit_amend_and_remote_containment() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.commit_all("init");
    let repo = f.repo();
    f.write("a.txt", "b\n");
    repo.stage_paths(&[Path::new("a.txt")]).unwrap();
    assert_eq!(
        repo.staged_stat().unwrap().trim(),
        "a.txt | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)"
    );
    let id = repo.commit("Change a\n\nWith a body.\n", false).unwrap();
    assert_eq!(f.git(&["log", "-1", "--format=%H"]).trim(), id);
    assert_eq!(repo.head_message().unwrap(), "Change a\n\nWith a body.\n");
    assert!(repo.commit("  \n", false).is_err());
    let amended = repo.commit("Changed a\n", true).unwrap();
    assert_ne!(amended, id);
    assert_eq!(f.git(&["log", "--format=%s"]), "Changed a\ninit\n");
    assert!(repo.head_on_remotes().unwrap().is_empty());

    // A bare remote that has HEAD.
    let bare = f.tmp.join("bare.git");
    f.git_in(&f.tmp, &["init", "-q", "--bare", bare.to_str().unwrap()]);
    f.git(&["remote", "add", "origin", bare.to_str().unwrap()]);
    let push = repo.remote_command(RemoteOp::Push).unwrap();
    assert_eq!(
        push.display(),
        "git push --set-upstream origin refs/heads/main:refs/heads/main"
    );
    repo.run_remote(&push).unwrap();
    assert_eq!(
        f.git_in(&bare, &["log", "--format=%s", "main"]),
        b"Changed a\ninit\n"
    );
    assert_eq!(
        repo.head_on_remotes().unwrap(),
        vec!["origin/main".to_string()]
    );
    // With an upstream now, push names it and does not set it again.
    let push = repo.remote_command(RemoteOp::Push).unwrap();
    assert_eq!(
        push.display(),
        "git push origin refs/heads/main:refs/heads/main"
    );
    assert_eq!(
        repo.remote_command(RemoteOp::Fetch).unwrap().display(),
        "git fetch origin"
    );
    assert_eq!(
        repo.remote_command(RemoteOp::Pull).unwrap().display(),
        "git pull --no-rebase --ff-only origin refs/heads/main"
    );
    // A push shows the push URL when one is set.
    assert_eq!(push.url, bare.to_str().unwrap());
    let elsewhere = f.tmp.join("push-target.git");
    f.git(&[
        "remote",
        "set-url",
        "--push",
        "origin",
        elsewhere.to_str().unwrap(),
    ]);
    assert_eq!(
        repo.remote_command(RemoteOp::Push).unwrap().url,
        elsewhere.to_str().unwrap()
    );
    assert_eq!(
        repo.remote_command(RemoteOp::Fetch).unwrap().url,
        bare.to_str().unwrap()
    );
    f.git(&["config", "--unset", "remote.origin.pushurl"]);
    // A command planned on main does not run once another branch is out.
    let pull = repo.remote_command(RemoteOp::Pull).unwrap();
    assert_eq!(pull.branch.as_deref(), Some("main"));
    f.git(&["switch", "-q", "-c", "elsewhere"]);
    let err = repo.run_remote(&pull).unwrap_err().to_string();
    assert!(
        err.contains("elsewhere is checked out now, not main"),
        "{err}"
    );
    f.git(&["switch", "-q", "main"]);
    // A hand-made forcing command is refused before git runs.
    let mut forced = push.clone();
    forced.args.insert(1, "--force".into());
    assert!(repo.run_remote(&forced).is_err());
    let mut plus = push;
    plus.args[2] = "+refs/heads/main:refs/heads/main".into();
    assert!(repo.run_remote(&plus).is_err());
}

#[test]
fn log_with_graph_and_commit_detail() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.commit_all("first");
    f.git(&["switch", "-q", "-c", "side"]);
    f.write("s.txt", "s\n");
    f.commit_all("on side");
    f.git(&["switch", "-q", "main"]);
    f.write("a.txt", "A\n");
    f.commit_all("on main");
    f.git(&["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
    let repo = f.repo();
    let log = repo.log(50, true).unwrap();
    let commits: Vec<&str> = log
        .iter()
        .filter_map(|e| e.commit.as_ref().map(|c| c.subject.as_str()))
        .collect();
    assert_eq!(commits.len(), 4, "{log:#?}");
    assert_eq!(commits[0], "merge side");
    assert_eq!(commits[3], "first");
    assert!(log.iter().any(|e| e.commit.is_none()), "graph-only rows");
    assert!(log[0].graph.starts_with('*'));
    assert!(
        log[0]
            .commit
            .as_ref()
            .unwrap()
            .refs
            .contains("HEAD -> main")
    );

    let first = log
        .iter()
        .filter_map(|e| e.commit.as_ref())
        .find(|c| c.subject == "on main")
        .unwrap();
    let d = repo.commit_detail(&first.id).unwrap();
    assert_eq!(d.message, "on main");
    assert_eq!(d.author, "Maestro Test <test@example.invalid>");
    assert_eq!(d.files.len(), 1);
    assert_eq!(d.files[0].file.hunks[0].lines.len(), 2);
    // A merge shows its diff against the first parent.
    let m = repo.commit_detail("HEAD").unwrap();
    assert_eq!(m.parents.len(), 2);
    assert_eq!(m.files.len(), 1);
    assert_eq!(m.files[0].file.new_path, Some(PathBuf::from("s.txt")));

    let flat = repo.log(10, false).unwrap();
    assert!(flat.iter().all(|e| e.commit.is_some()));
}

#[test]
fn blame_from_disk_and_from_buffer_contents() {
    let f = Fixture::new();
    f.write("b.txt", "one\ntwo\n");
    f.commit_all("first");
    f.write("b.txt", "one\ntwo\nthree\n");
    f.commit_all("second");
    let repo = f.repo();
    let lines = repo.blame(&f.repo.join("b.txt"), None, None).unwrap();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].commit.summary, "first");
    assert_eq!(lines[2].commit.summary, "second");
    assert_eq!(lines[2].commit.author, "Maestro Test");
    assert_eq!(lines[2].commit.date().len(), 10);
    // Unsaved buffer text: the edited line is not committed yet.
    let lines = repo
        .blame(Path::new("b.txt"), Some(b"one\nTWO\nthree\n"), Some((2, 2)))
        .unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].final_line, 2);
    assert!(lines[0].commit.uncommitted());
}

#[test]
fn stash_push_list_and_pop_by_id() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.commit_all("init");
    let repo = f.repo();
    assert!(!repo.stash_push(None).unwrap(), "nothing to stash");
    f.write("a.txt", "changed\n");
    assert!(repo.stash_push(Some("my work")).unwrap());
    assert_eq!(
        std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "a\n"
    );
    let list = repo.stashes().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].message, "On main: my work");
    assert!(repo.stash_pop(0, "0123456789").is_err(), "wrong id refused");
    assert_eq!(repo.stashes().unwrap().len(), 1);
    repo.stash_pop(0, &list[0].id).unwrap();
    assert!(repo.stashes().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "changed\n"
    );
}

#[test]
fn operations_in_progress_are_named() {
    let f = Fixture::new();
    f.write("a.txt", "base\n");
    f.commit_all("init");
    let repo = f.repo();
    assert_eq!(repo.operation().unwrap(), None);
    f.git(&["switch", "-q", "-c", "other"]);
    f.write("a.txt", "theirs\n");
    f.commit_all("theirs");
    f.git(&["switch", "-q", "main"]);
    f.write("a.txt", "ours\n");
    f.commit_all("ours");
    let out = f.raw(&f.repo, &["merge", "other"]);
    assert!(!out.status.success());
    assert_eq!(repo.operation().unwrap(), Some(Operation::Merge));
    f.git(&["merge", "--abort"]);
    let out = f.raw(&f.repo, &["rebase", "other"]);
    assert!(!out.status.success());
    assert_eq!(repo.operation().unwrap(), Some(Operation::Rebase));
    f.git(&["rebase", "--abort"]);
    f.git(&["bisect", "start"]);
    assert_eq!(repo.operation().unwrap(), Some(Operation::Bisect));
    f.git(&["bisect", "reset"]);
    assert_eq!(repo.operation().unwrap(), None);
}

#[test]
fn branches_switch_and_create() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.commit_all("init");
    let repo = f.repo();
    repo.create_branch("topic").unwrap();
    assert_eq!(f.git(&["branch", "--show-current"]).trim(), "topic");
    let b = repo.branches().unwrap();
    assert_eq!(b.len(), 2);
    assert!(b.iter().any(|b| b.name == "topic" && b.current));
    repo.switch_branch("main").unwrap();
    assert_eq!(f.git(&["branch", "--show-current"]).trim(), "main");
    assert!(repo.switch_branch("nope").is_err());
    assert!(repo.switch_branch("-f").is_err());
    assert!(repo.create_branch("bad..name").is_err());
}
