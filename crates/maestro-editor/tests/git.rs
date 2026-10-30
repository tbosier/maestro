//! Git behavior through the editor: gutter signs, branch, pickers.
//!
//! Repositories are built with the git CLI under a temporary HOME and
//! global config file, with `GIT_CONFIG_NOSYSTEM=1` and no inherited `-c`
//! settings, so the user's config is never read or written.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maestro_config::Config;
use maestro_editor::git::{GitState, Sign};
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect};
use maestro_git::Git;

static NEXT: AtomicUsize = AtomicUsize::new(0);

const CONFIG: &[&str] = &[
    "user.name=Maestro Test",
    "user.email=test@example.invalid",
    "init.defaultBranch=main",
    "commit.gpgSign=false",
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
        let tmp = std::env::temp_dir().join(format!("maestro-edgit-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let f = Fixture { tmp, repo };
        f.git(&f.repo, &["init", "-q"]);
        f
    }

    fn git(&self, dir: &Path, args: &[&str]) {
        let mut cmd = Command::new("git");
        for c in CONFIG {
            cmd.arg("-c").arg(c);
        }
        let out = cmd
            .args(args)
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
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
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

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.repo.join(rel);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn editor(&self) -> Editor {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
        ed.git = GitState::new(self.api());
        ed.cwd = self.repo.clone();
        ed
    }
}

fn keys(script: &str) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let mut chars = script.chars();
    while let Some(c) = chars.next() {
        if c == '<' {
            let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
            out.push(name.parse().unwrap_or_else(|_| panic!("bad key {name}")));
        } else if c == '\n' {
            out.push(KeyEvent::plain(KeyCode::Enter));
        } else {
            out.push(KeyEvent::char(c));
        }
    }
    out
}

/// Polls background work until `done` holds, as the terminal loop does.
fn poll_until(ed: &mut Editor, what: &str, mut done: impl FnMut(&Editor) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        ed.poll_background();
        if done(ed) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn signs(ed: &Editor) -> Vec<(usize, Sign)> {
    let doc = ed.view().doc;
    ed.git
        .signs(doc)
        .map(|s| s.iter().collect())
        .unwrap_or_default()
}

#[test]
fn signs_appear_for_a_modified_tracked_file_and_follow_edits() {
    let f = Fixture::new();
    f.write("f.txt", "one\ntwo\nthree\nfour\nfive\n");
    f.git(&f.repo, &["add", "f.txt"]);
    f.git(&f.repo, &["commit", "-q", "-m", "init"]);
    // Change line 2, remove "four", add "six" at the end.
    let path = f.write("f.txt", "one\nTWO\nthree\nfive\nsix\n");

    let mut ed = f.editor();
    ed.open(&path).unwrap();
    let want = vec![(1, Sign::Changed), (2, Sign::Removed), (4, Sign::Added)];
    poll_until(&mut ed, "signs after open", |ed| signs(ed) == want);
    assert_eq!(ed.git.branch_label(ed.view().doc).as_deref(), Some("main"));
    assert_eq!(ed.git.sign_at(ed.view().doc, 0), None);

    // Add a line at the top: every sign moves down and line 0 is added.
    ed.feed_keys(&keys("ggOnew<esc>"));
    assert_eq!(
        ed.doc().text().to_string(),
        "new\none\nTWO\nthree\nfive\nsix\n"
    );
    let want = vec![
        (0, Sign::Added),
        (2, Sign::Changed),
        (3, Sign::Removed),
        (5, Sign::Added),
    ];
    poll_until(&mut ed, "signs after an edit", |ed| signs(ed) == want);

    // Undo to the saved text and the original signs come back.
    ed.feed_keys(&keys("u"));
    let want = vec![(1, Sign::Changed), (2, Sign::Removed), (4, Sign::Added)];
    poll_until(&mut ed, "signs after undo", |ed| signs(ed) == want);
}

#[test]
fn staging_outside_the_editor_clears_signs_on_tick() {
    let f = Fixture::new();
    f.write("f.txt", "a\nb\n");
    f.git(&f.repo, &["add", "f.txt"]);
    f.git(&f.repo, &["commit", "-q", "-m", "init"]);
    let path = f.write("f.txt", "a\nB\n");
    let mut ed = f.editor();
    ed.open(&path).unwrap();
    poll_until(&mut ed, "a changed line", |ed| {
        signs(ed) == vec![(1, Sign::Changed)]
    });
    // Signs compare with the index: staging the change removes them once
    // the editor notices the index changed.
    std::thread::sleep(Duration::from_millis(20));
    f.git(&f.repo, &["add", "f.txt"]);
    // Polling alone does not notice: nothing in the buffer changed.
    for _ in 0..20 {
        ed.poll_background();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(signs(&ed), vec![(1, Sign::Changed)]);
    // The once-a-second tick sees the index change.
    ed.tick(Instant::now());
    poll_until(&mut ed, "signs cleared after git add", |ed| {
        signs(ed).is_empty()
    });
}

#[test]
fn untracked_files_are_all_added_and_ignored_files_have_no_signs() {
    let f = Fixture::new();
    f.write(".gitignore", "*.log\n");
    f.git(&f.repo, &["add", ".gitignore"]);
    f.git(&f.repo, &["commit", "-q", "-m", "init"]);
    let new = f.write("new.txt", "x\ny\n");
    let log = f.write("out.log", "x\ny\n");
    let mut ed = f.editor();
    ed.open(&new).unwrap();
    poll_until(&mut ed, "untracked signs", |ed| {
        signs(ed) == vec![(0, Sign::Added), (1, Sign::Added)]
    });
    let log_id = ed.open(&log).unwrap();
    // The branch shows once the ignored file's lookup has finished.
    poll_until(&mut ed, "branch for the ignored file", |ed| {
        ed.git.branch_label(log_id).is_some()
    });
    assert!(signs(&ed).is_empty());
}

#[test]
fn git_status_picker_lists_changes_and_opens_the_file() {
    let f = Fixture::new();
    f.write("tracked.txt", "t\n");
    f.git(&f.repo, &["add", "tracked.txt"]);
    f.git(&f.repo, &["commit", "-q", "-m", "init"]);
    f.write("tracked.txt", "changed\n");
    let untracked = f.write("fresh.txt", "new\n");

    let mut ed = f.editor();
    ed.feed_keys(&keys(":git-status\n"));
    poll_until(&mut ed, "the status picker", |ed| {
        ed.picker.as_ref().is_some_and(|p| !p.loading)
    });
    let picker = ed.picker.as_ref().unwrap();
    assert!(
        picker.title.starts_with("git status (main"),
        "{}",
        picker.title
    );
    let rows: Vec<(String, String)> = picker
        .items()
        .iter()
        .map(|i| (i.label.clone(), i.detail.clone()))
        .collect();
    assert!(rows.contains(&("tracked.txt".into(), "unstaged modified".into())));
    assert!(rows.contains(&("fresh.txt".into(), "untracked".into())));

    ed.feed_keys(&keys("fresh\n"));
    assert!(ed.picker.is_none());
    assert_eq!(ed.doc().path(), Some(untracked.as_path()));
}

#[test]
fn worktrees_picker_shows_state_and_changes_directory() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.git(&f.repo, &["add", "a.txt"]);
    f.git(&f.repo, &["commit", "-q", "-m", "init"]);
    let wt = f.tmp.join("agent-wt");
    f.git(
        &f.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "agent/x",
            wt.to_str().unwrap(),
        ],
    );
    std::fs::write(wt.join("scratch.txt"), "s\n").unwrap();

    let mut ed = f.editor();
    ed.feed_keys(&keys(":worktrees\n"));
    poll_until(&mut ed, "the worktree picker", |ed| {
        ed.picker.as_ref().is_some_and(|p| !p.loading)
    });
    let picker = ed.picker.as_ref().unwrap();
    let rows: Vec<(String, String)> = picker
        .items()
        .iter()
        .map(|i| (i.label.clone(), i.detail.clone()))
        .collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.contains(&(
        f.repo.display().to_string(),
        "main  clean, main worktree".into()
    )));
    assert!(rows.contains(&(wt.display().to_string(), "agent/x  1 untracked".into())));

    ed.feed_keys(&keys("agent-wt\n"));
    assert!(ed.picker.is_none());
    assert_eq!(ed.cwd, wt);
}

#[test]
fn files_outside_a_repository_get_no_signs_and_no_branch() {
    let f = Fixture::new();
    let plain = f.tmp.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let path = plain.join("x.txt");
    std::fs::write(&path, "x\n").unwrap();
    let mut ed = f.editor();
    let id = ed.open(&path).unwrap();
    poll_until(&mut ed, "background work to finish", |ed| {
        !ed.has_background_work()
    });
    ed.poll_background();
    assert!(signs(&ed).is_empty());
    assert_eq!(ed.git.branch_label(id), None);

    ed.feed_keys(&keys(":git-status\n"));
    poll_until(&mut ed, "the status error", |ed| ed.picker.is_none());
    let (_, msg) = ed.status.clone().unwrap();
    assert!(msg.contains("not in a Git repository"), "{msg}");
}

#[test]
fn bom_files_compare_equal_and_the_base_can_switch_to_head() {
    let f = Fixture::new();
    f.write("bom.txt", "\u{feff}a\nb\n");
    f.git(&f.repo, &["add", "bom.txt"]);
    f.git(&f.repo, &["commit", "-q", "-m", "init"]);
    // Stage a change: against the index the file is clean.
    let path = f.write("bom.txt", "\u{feff}a\nB\n");
    f.git(&f.repo, &["add", "bom.txt"]);
    let mut ed = f.editor();
    let id = ed.open(&path).unwrap();
    poll_until(&mut ed, "the first result", |ed| {
        ed.git.branch_label(id).is_some() && !ed.has_background_work()
    });
    assert!(signs(&ed).is_empty(), "{:?}", signs(&ed));
    // Against HEAD the staged change shows.
    ed.git.base = maestro_editor::git::Base::Head;
    poll_until(&mut ed, "signs against HEAD", |ed| {
        signs(ed) == vec![(1, Sign::Changed)]
    });
}

/// Timing behind the numbers in ADR 0005. Run with
/// `cargo test --release -p maestro-editor measure_open_to_signs -- --ignored --nocapture`.
/// It opens this crate's own `src/editor.rs`, so it needs the Maestro
/// checkout to be a Git working tree.
#[test]
#[ignore = "measurement, prints timings"]
fn measure_open_to_signs() {
    let f = Fixture::new();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/editor.rs");
    let mut samples = Vec::new();
    for _ in 0..20 {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
        ed.git = GitState::new(f.api());
        let start = Instant::now();
        let id = ed.open(&path).unwrap();
        while ed.git.branch_label(id).is_none() {
            ed.poll_background();
            std::thread::sleep(Duration::from_micros(200));
        }
        samples.push(start.elapsed());
    }
    samples.sort();
    eprintln!(
        "open to signs and branch: median {:?}, max {:?}, n={}",
        samples[samples.len() / 2],
        samples[samples.len() - 1],
        samples.len()
    );
    let text = std::fs::read_to_string(&path).unwrap();
    let edited = text.replacen("fn ", "fn  ", 50);
    let start = Instant::now();
    for _ in 0..20 {
        let _ = maestro_git::SignMap::diff(&text, &edited);
    }
    eprintln!(
        "line diff of {} lines with 50 edits: {:?} each",
        text.lines().count(),
        start.elapsed() / 20
    );
}
