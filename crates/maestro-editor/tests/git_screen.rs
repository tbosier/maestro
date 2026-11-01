//! The Git status screen and buffer hunk keys, driven through
//! `Editor::handle_key` against real repositories.
//!
//! Repositories live under a temporary HOME with `GIT_CONFIG_NOSYSTEM=1`
//! and no inherited `-c` settings, so the user's config is never read or
//! written. Remotes are local bare repositories.

// One-element lists of ranges are intraline marks, not mistyped ranges.
#![allow(clippy::single_range_in_vec_init)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maestro_config::Config;
use maestro_editor::git::{Git, GitState, Sign};
use maestro_editor::git_screen::{Page, Row, Screen};
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect, Severity};

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
        let tmp = std::env::temp_dir().join(format!("maestro-gscreen-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let f = Fixture { tmp, repo };
        f.git(&["init", "-q"]);
        f
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

    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.raw(dir, args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.repo, args)
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

    fn write(&self, rel: &str, text: impl AsRef<[u8]>) -> PathBuf {
        let p = self.repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.repo.join(rel)).unwrap()
    }

    fn commit_all(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
    }

    /// A bare repository as `origin`, with `main` pushed and tracked.
    fn with_remote(&self) -> PathBuf {
        let bare = self.tmp.join("origin.git");
        self.git_in(&self.tmp, &["init", "-q", "--bare", bare.to_str().unwrap()]);
        self.git(&["remote", "add", "origin", bare.to_str().unwrap()]);
        self.git(&["push", "-q", "-u", "origin", "main"]);
        bare
    }

    fn editor(&self) -> Editor {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 110, 34));
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

fn poll_until(ed: &mut Editor, what: &str, mut done: impl FnMut(&Editor) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        ed.poll_background();
        if done(ed) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; status {:?}",
            ed.status
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn screen(ed: &Editor) -> &Screen {
    ed.git.screen.as_ref().expect("the status screen is open")
}

fn settled(ed: &Editor) -> bool {
    ed.git
        .screen
        .as_ref()
        .is_some_and(|s| !s.loading && s.busy.is_none() && (s.data.is_some() || s.error.is_some()))
}

/// Feeds keys, then waits for the screen to finish any Git work.
fn press(ed: &mut Editor, script: &str) {
    ed.feed_keys(&keys(script));
    poll_until(ed, &format!("the screen after {script:?}"), settled);
}

fn open(f: &Fixture) -> Editor {
    let mut ed = f.editor();
    press(&mut ed, " g");
    ed
}

/// A row as plain text, for finding and checking rows.
fn row_text(sc: &Screen, i: usize) -> String {
    match &sc.rows[i] {
        Row::Head { label, value, .. } => format!("{label}: {value}"),
        Row::Blank => String::new(),
        Row::Section { section, count } => format!("{} ({count})", section.title()),
        Row::Untracked(i) => format!(
            "untracked {}",
            sc.data.as_ref().unwrap().untracked[*i].display()
        ),
        Row::File { section, file } => {
            maestro_editor::git_screen::file_label(sc.file(*section, *file).unwrap())
        }
        Row::Note(t) => t.clone(),
        Row::Hunk { header, .. } => header.clone(),
        Row::Line { row, .. } => {
            let sign = match row.kind {
                maestro_editor::agents::DiffLineKind::Added => '+',
                maestro_editor::agents::DiffLineKind::Removed => '-',
                maestro_editor::agents::DiffLineKind::Context => ' ',
            };
            format!("{sign}{}", row.text)
        }
        Row::Stash(i) => sc.data.as_ref().unwrap().stashes[*i].message.clone(),
        Row::Commit(i) => sc.data.as_ref().unwrap().recent[*i].subject.clone(),
    }
}

fn rows(ed: &Editor) -> Vec<String> {
    let sc = screen(ed);
    (0..sc.rows.len()).map(|i| row_text(sc, i)).collect()
}

/// Moves the cursor with `g` and `j` to the first row reading `want`.
fn goto(ed: &mut Editor, want: &str) {
    let all = rows(ed);
    let target = all
        .iter()
        .position(|t| t.trim_end() == want)
        .unwrap_or_else(|| panic!("no row {want:?} in {all:#?}"));
    ed.feed_keys(&keys("g"));
    let mut guard = 0;
    while screen(ed).cursor < target {
        ed.feed_keys(&keys("j"));
        guard += 1;
        assert!(guard < 500, "could not reach row {want:?}");
    }
    assert_eq!(screen(ed).cursor, target, "row {want:?}");
}

fn status_text(ed: &Editor) -> String {
    ed.status
        .as_ref()
        .map(|(_, m)| m.clone())
        .unwrap_or_default()
}

fn status_is(ed: &Editor, sev: Severity, part: &str) -> bool {
    ed.status
        .as_ref()
        .is_some_and(|(s, m)| *s == sev && m.contains(part))
}

fn file_state(p: &Path) -> (Vec<u8>, std::time::SystemTime) {
    (
        std::fs::read(p).unwrap(),
        std::fs::metadata(p).unwrap().modified().unwrap(),
    )
}

/// Twenty numbered lines, so changes at the ends land in separate hunks.
fn twenty() -> String {
    (1..=20).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn space_g_and_colon_git_show_head_sections_and_hunks() {
    let f = Fixture::new();
    f.write("a.txt", twenty());
    f.write("b.txt", "b\n");
    f.commit_all("first commit");
    f.with_remote();
    f.write("a.txt", twenty().replace("line 2\n", "line two\n"));
    f.write("b.txt", "B\n");
    f.git(&["add", "b.txt"]);
    f.write("new.txt", "new\n");

    let mut ed = open(&f);
    let all = rows(&ed);
    assert_eq!(all[0], "Head: main  first commit");
    assert_eq!(all[1], "Upstream: origin/main  ahead 0, behind 0");
    for want in [
        "Untracked (1)",
        "untracked new.txt",
        "Unstaged (1)",
        "modified   a.txt",
        "Staged (1)",
        "modified   b.txt",
        "Recent commits (1)",
        "first commit",
    ] {
        assert!(all.iter().any(|r| r == want), "{want:?} in {all:#?}");
    }
    // The cursor starts on the first file.
    assert_eq!(
        row_text(screen(&ed), screen(&ed).cursor),
        "untracked new.txt"
    );

    // Tab opens a file's hunks with changed words marked.
    goto(&mut ed, "modified   a.txt");
    ed.feed_keys(&keys("<tab>"));
    let all = rows(&ed);
    assert!(
        all.iter().any(|r| r.starts_with("@@ -1,5 +1,5 @@")),
        "{all:#?}"
    );
    let sc = screen(&ed);
    let marked = sc.rows.iter().find_map(|r| match r {
        Row::Line { row, .. } if row.text == "line two" => Some(row.marks.clone()),
        _ => None,
    });
    assert_eq!(marked, Some(vec![5..8]), "intraline mark on \"two\"");
    // Tab on a line closes the file again and returns to its row.
    goto(&mut ed, "+line two");
    ed.feed_keys(&keys("<tab>"));
    assert_eq!(
        row_text(screen(&ed), screen(&ed).cursor),
        "modified   a.txt"
    );
    assert!(!rows(&ed).iter().any(|r| r == "+line two"));
    // Tab on a section folds it.
    goto(&mut ed, "Staged (1)");
    ed.feed_keys(&keys("<tab>"));
    assert!(!rows(&ed).iter().any(|r| r == "modified   b.txt"));
    ed.feed_keys(&keys("<tab>"));
    assert!(rows(&ed).iter().any(|r| r == "modified   b.txt"));

    // q closes; :git opens again; Escape closes.
    ed.feed_keys(&keys("q"));
    assert!(ed.git.screen.is_none());
    press(&mut ed, ":git\n");
    assert!(rows(&ed).iter().any(|r| r == "Unstaged (1)"));
    ed.feed_keys(&keys("<esc>"));
    assert!(ed.git.screen.is_none());
}

#[test]
fn s_stages_a_hunk_and_then_selected_lines_without_touching_the_file() {
    let f = Fixture::new();
    f.write("a.txt", twenty());
    f.commit_all("init");
    let changed = twenty()
        .replace("line 2\n", "line two\n")
        .replace("line 18\n", "line 18\nadded one\nadded two\n")
        .replace("line 19\n", "");
    let path = f.write("a.txt", &changed);
    let before = file_state(&path);

    let mut ed = open(&f);
    goto(&mut ed, "modified   a.txt");
    ed.feed_keys(&keys("<tab>"));
    // The first hunk, from its header.
    goto(&mut ed, "@@ -1,5 +1,5 @@");
    press(&mut ed, "s");
    assert!(
        status_is(&ed, Severity::Info, "staged hunk 1 of a.txt"),
        "{:?}",
        ed.status
    );
    let cached = f.git(&["diff", "--cached", "-U0"]);
    assert!(cached.contains("-line 2\n+line two\n"), "{cached}");
    assert!(!cached.contains("added"), "{cached}");
    assert_eq!(file_state(&path), before, "the file on disk is untouched");

    // Two selected lines of the other hunk: the two added lines, not the
    // removal of "line 19" next to them.
    goto(&mut ed, "+added one");
    ed.feed_keys(&keys("vj"));
    assert_eq!(screen(&ed).selection().map(|(a, b)| b - a), Some(1));
    press(&mut ed, "s");
    assert!(
        status_is(&ed, Severity::Info, "staged 2 line(s) of hunk"),
        "{:?}",
        ed.status
    );
    let index = f.git(&["show", ":a.txt"]);
    assert_eq!(
        index,
        twenty()
            .replace("line 2\n", "line two\n")
            .replace("line 19\n", "line 19\nadded one\nadded two\n"),
        "the removal of line 19 stays unstaged (git lists it before the additions)"
    );
    assert_eq!(file_state(&path), before);
    let unstaged = f.git(&["diff", "-U0"]);
    assert!(unstaged.contains("-line 19\n"), "{unstaged}");
    assert!(!unstaged.contains("added"), "{unstaged}");

    // A selection that leaves the hunk is refused.
    goto(&mut ed, "Staged (1)");
    ed.feed_keys(&keys("j<tab>"));
    goto(&mut ed, "+added two");
    ed.feed_keys(&keys("vk"));
    ed.feed_keys(&keys("k"));
    ed.feed_keys(&keys("k"));
    ed.feed_keys(&keys("k"));
    ed.feed_keys(&keys("k"));
    ed.feed_keys(&keys("k"));
    ed.feed_keys(&keys("u"));
    assert!(
        status_is(&ed, Severity::Error, "select lines inside one hunk"),
        "{:?}",
        ed.status
    );
    ed.feed_keys(&keys("<esc>"));
    assert!(screen(&ed).mark.is_none());

    // u on one staged line unstages just it.
    goto(&mut ed, "+added two");
    press(&mut ed, "vu");
    let index = f.git(&["show", ":a.txt"]);
    assert!(index.contains("line 19\nadded one\nline 20\n"), "{index}");
    assert!(!index.contains("added two"), "{index}");
    assert_eq!(file_state(&path), before);

    // s on a staged row says so; u on an unstaged row says so.
    press(&mut ed, "s");
    assert!(
        status_text(&ed).contains("already staged"),
        "{:?}",
        ed.status
    );
    goto(&mut ed, "modified   a.txt");
    press(&mut ed, "u");
    assert!(status_text(&ed).contains("not staged"), "{:?}", ed.status);
}

#[test]
fn x_asks_says_what_is_lost_and_refuses_a_changed_file() {
    let f = Fixture::new();
    f.write("a.txt", twenty());
    f.commit_all("init");
    let changed = twenty()
        .replace("line 2\n", "line two\n")
        .replace("line 19\n", "line nineteen\n");
    let path = f.write("a.txt", &changed);
    f.write("junk.txt", "junk\n");

    let mut ed = open(&f);
    goto(&mut ed, "modified   a.txt");
    ed.feed_keys(&keys("<tab>"));
    goto(&mut ed, "@@ -1,5 +1,5 @@");
    ed.feed_keys(&keys("x"));
    let d = screen(&ed).dialog.as_ref().expect("x asks first");
    let text = d.lines.join(" ");
    assert!(
        text.contains(
            "Discards hunk 1 of a.txt: 1 added line(s) are removed and 1 removed line(s) come back"
        ),
        "{text}"
    );
    assert!(text.contains("Git cannot bring them back"), "{text}");
    // n cancels and changes nothing.
    ed.feed_keys(&keys("n"));
    assert!(screen(&ed).dialog.is_none());
    assert_eq!(f.read("a.txt"), changed);

    // y discards exactly that hunk.
    ed.feed_keys(&keys("x"));
    press(&mut ed, "y");
    assert!(
        status_is(&ed, Severity::Info, "discarded hunk 1 of a.txt"),
        "{:?}",
        ed.status
    );
    assert_eq!(
        f.read("a.txt"),
        twenty().replace("line 19\n", "line nineteen\n")
    );

    // The file changes after the screen was drawn: refused, nothing lost.
    goto(&mut ed, "+line nineteen");
    ed.feed_keys(&keys("x"));
    assert!(screen(&ed).dialog.is_some());
    let edited = twenty()
        .replace("line 19\n", "line nineteen\n")
        .replace("line 5\n", "line five\n");
    std::fs::write(&path, &edited).unwrap();
    press(&mut ed, "y");
    assert!(
        status_is(&ed, Severity::Error, "changed since the screen was drawn"),
        "{:?}",
        ed.status
    );
    assert_eq!(f.read("a.txt"), edited);

    // Untracked: deleted after saying Git never had it.
    goto(&mut ed, "untracked junk.txt");
    ed.feed_keys(&keys("x"));
    let text = screen(&ed).dialog.as_ref().unwrap().lines.join(" ");
    assert!(
        text.contains("Deletes the untracked file junk.txt (5 bytes)"),
        "{text}"
    );
    assert!(text.contains("Git never stored it"), "{text}");
    press(&mut ed, "y");
    assert!(!f.repo.join("junk.txt").exists());

    // A staged file with unstaged changes too is refused whole.
    f.git(&["add", "a.txt"]);
    std::fs::write(&path, twenty()).unwrap();
    press(&mut ed, "r");
    goto(&mut ed, "Staged (1)");
    ed.feed_keys(&keys("j"));
    ed.feed_keys(&keys("x"));
    assert!(
        status_is(&ed, Severity::Error, "also has unstaged changes"),
        "{:?}",
        ed.status
    );
    assert!(screen(&ed).dialog.is_none());
}

#[test]
fn destructive_keys_are_refused_during_a_merge() {
    let f = Fixture::new();
    f.write("a.txt", "base\n");
    f.commit_all("init");
    f.git(&["switch", "-q", "-c", "other"]);
    f.write("a.txt", "theirs\n");
    f.commit_all("theirs");
    f.git(&["switch", "-q", "main"]);
    f.write("a.txt", "ours\n");
    f.commit_all("ours");
    f.write("c.txt", "c\n");
    f.git(&["stash", "push", "-q", "--include-untracked"]);
    assert!(!f.raw(&f.repo, &["merge", "other"]).status.success());

    let mut ed = open(&f);
    let all = rows(&ed);
    assert!(
        all.iter()
            .any(|r| r
                == "In progress: merge: finish with git merge --continue or git merge --abort"),
        "{all:#?}"
    );
    assert!(all.iter().any(|r| r == "conflict   a.txt"), "{all:#?}");
    goto(&mut ed, "conflict   a.txt");
    for (k, what) in [
        ("x", "not discarding: a merge is in progress"),
        ("b", "not switching branches: a merge is in progress"),
        ("F", "not pulling: a merge is in progress"),
        ("z", "not stashing: a merge is in progress"),
        ("Z", "not popping a stash: a merge is in progress"),
        ("ca", "not amending: a merge is in progress"),
    ] {
        press(&mut ed, k);
        assert!(
            status_is(&ed, Severity::Error, what),
            "{k}: {:?}",
            ed.status
        );
        assert!(screen(&ed).dialog.is_none() && screen(&ed).branches.is_none());
    }
    // Staging a file that still has conflict markers is refused too.
    press(&mut ed, "s");
    assert!(
        status_is(&ed, Severity::Error, "conflict markers"),
        "{:?}",
        ed.status
    );
    goto(&mut ed, "Unstaged (1)");
    press(&mut ed, "s");
    assert!(
        status_is(&ed, Severity::Error, "a.txt still has conflict markers"),
        "{:?}",
        ed.status
    );
    assert!(
        f.git(&["diff", "--name-only", "--diff-filter=U"])
            .contains("a.txt")
    );
    assert!(
        f.raw(&f.repo, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .status
            .success()
    );
}

#[test]
fn c_c_commits_from_a_message_buffer_and_empty_aborts() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.commit_all("init");
    f.write("a.txt", "A\n");

    let mut ed = open(&f);
    // Nothing staged yet.
    press(&mut ed, "cc");
    assert!(
        status_is(&ed, Severity::Error, "nothing staged"),
        "{:?}",
        ed.status
    );

    goto(&mut ed, "modified   a.txt");
    press(&mut ed, "s");
    ed.feed_keys(&keys("cc"));
    poll_until(&mut ed, "the message buffer", |ed| {
        ed.git.commit_message.is_some()
    });
    assert!(
        ed.git.screen.is_none(),
        "the screen makes room for the buffer"
    );
    let text = ed.doc().text().to_string();
    assert!(
        text.starts_with("\n# Write the commit message above."),
        "{text}"
    );
    assert!(
        text.contains(maestro_editor::git_screen::SCISSORS),
        "{text}"
    );
    assert!(text.contains("a.txt | 2 +-"), "{text}");
    ed.feed_keys(&keys("iChange a to A<esc>:wq\n"));
    poll_until(&mut ed, "the commit", |ed| {
        status_is(ed, Severity::Info, "committed")
    });
    assert_eq!(f.git(&["log", "-1", "--format=%B"]), "Change a to A\n\n");
    assert!(
        status_text(&ed).contains(": Change a to A"),
        "{:?}",
        ed.status
    );
    poll_until(&mut ed, "the screen again", settled);
    assert_eq!(rows(&ed)[0], "Head: main  Change a to A");

    // An empty message aborts.
    f.write("a.txt", "AA\n");
    press(&mut ed, "r");
    goto(&mut ed, "modified   a.txt");
    press(&mut ed, "s");
    ed.feed_keys(&keys("cc"));
    poll_until(&mut ed, "the message buffer", |ed| {
        ed.git.commit_message.is_some()
    });
    ed.feed_keys(&keys(":wq\n"));
    poll_until(&mut ed, "the abort", |ed| {
        status_is(ed, Severity::Warning, "commit aborted: empty message")
    });
    assert_eq!(f.git(&["log", "--format=%s"]), "Change a to A\ninit\n");

    // Closing with unsaved edits aborts too.
    press(&mut ed, " g");
    ed.feed_keys(&keys("cc"));
    poll_until(&mut ed, "the message buffer", |ed| {
        ed.git.commit_message.is_some()
    });
    ed.feed_keys(&keys("iNot saved<esc>:q\n"));
    poll_until(&mut ed, "the abort", |ed| {
        status_is(ed, Severity::Warning, "closed without saving")
    });
    assert_eq!(f.git(&["log", "--format=%s"]), "Change a to A\ninit\n");
}

#[test]
fn c_a_amends_and_asks_again_when_head_is_on_the_remote() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.commit_all("init");
    f.write("a.txt", "b\n");
    f.commit_all("Local subject");

    // Not pushed: straight to the message, prefilled.
    let mut ed = open(&f);
    ed.feed_keys(&keys("ca"));
    poll_until(&mut ed, "the message buffer", |ed| {
        ed.git.commit_message.is_some()
    });
    assert!(
        ed.doc()
            .text()
            .to_string()
            .starts_with("Local subject\n# Amending HEAD")
    );
    ed.feed_keys(&keys("wcReworded <esc>:wq\n"));
    poll_until(&mut ed, "the amend", |ed| {
        status_is(ed, Severity::Info, "amended")
    });
    assert_eq!(f.git(&["log", "--format=%s"]), "Reworded subject\ninit\n");

    // Pushed: asked again first; n cancels.
    f.with_remote();
    press(&mut ed, "r");
    press(&mut ed, "ca");
    let d = screen(&ed).dialog.as_ref().expect("asks again");
    let text = d.lines.join(" ");
    assert!(text.contains("is already on origin/main"), "{text}");
    assert!(
        text.contains("would need a force push, which Maestro never does"),
        "{text}"
    );
    ed.feed_keys(&keys("n"));
    assert!(ed.git.commit_message.is_none());
    press(&mut ed, "ca");
    ed.feed_keys(&keys("y"));
    poll_until(&mut ed, "the message buffer", |ed| {
        ed.git.commit_message.is_some()
    });
    ed.feed_keys(&keys("A Again <esc>:wq\n"));
    poll_until(&mut ed, "the amend", |ed| {
        status_is(ed, Severity::Info, "amended")
    });
    assert_eq!(
        f.git(&["log", "-1", "--format=%s"]).trim(),
        "Reworded subject Again"
    );
}

#[test]
fn f_capital_f_and_p_show_the_exact_command_and_never_force() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.commit_all("init");
    let bare = f.with_remote();
    f.write("a.txt", "b\n");
    f.commit_all("second");

    let mut ed = open(&f);
    assert!(
        rows(&ed)
            .iter()
            .any(|r| r == "Upstream: origin/main  ahead 1, behind 0")
    );
    press(&mut ed, "P");
    let d = screen(&ed).dialog.as_ref().expect("P asks first");
    assert_eq!(
        d.lines[0],
        "Runs: git push origin refs/heads/main:refs/heads/main"
    );
    assert_eq!(d.lines[1], format!("Remote origin: {}", bare.display()));
    assert!(d.lines[2].contains("Never forces"));
    ed.feed_keys(&keys("n"));
    assert_eq!(
        f.git_in(&bare, &["log", "-1", "--format=%s", "main"]),
        "init\n"
    );
    press(&mut ed, "P");
    press(&mut ed, "y");
    assert!(
        status_is(&ed, Severity::Info, "git push origin"),
        "{:?}",
        ed.status
    );
    assert_eq!(
        f.git_in(&bare, &["log", "-1", "--format=%s", "main"]),
        "second\n"
    );
    assert!(
        rows(&ed)
            .iter()
            .any(|r| r == "Upstream: origin/main  ahead 0, behind 0")
    );

    // Someone else pushes; fetch then pull fast-forward.
    let other = f.tmp.join("other");
    f.git_in(
        &f.tmp,
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    std::fs::write(other.join("o.txt"), "o\n").unwrap();
    f.git_in(&other, &["add", "o.txt"]);
    f.git_in(&other, &["commit", "-q", "-m", "from other"]);
    f.git_in(&other, &["push", "-q", "origin", "main"]);
    press(&mut ed, "f");
    assert_eq!(
        screen(&ed).dialog.as_ref().unwrap().lines[0],
        "Runs: git fetch origin"
    );
    press(&mut ed, "y");
    assert!(
        rows(&ed)
            .iter()
            .any(|r| r == "Upstream: origin/main  ahead 0, behind 1")
    );
    press(&mut ed, "F");
    assert_eq!(
        screen(&ed).dialog.as_ref().unwrap().lines[0],
        "Runs: git pull --no-rebase --ff-only origin refs/heads/main"
    );
    press(&mut ed, "y");
    assert_eq!(f.read("o.txt"), "o\n");

    // Diverged: push is refused by git, nothing is forced.
    std::fs::write(other.join("o.txt"), "o2\n").unwrap();
    f.git_in(&other, &["commit", "-q", "-am", "other again"]);
    f.git_in(&other, &["push", "-q", "origin", "main"]);
    f.write("a.txt", "mine\n");
    f.commit_all("mine");
    press(&mut ed, "P");
    press(&mut ed, "y");
    assert!(
        status_is(&ed, Severity::Error, "rejected"),
        "{:?}",
        ed.status
    );
    assert_eq!(
        f.git_in(&bare, &["log", "-1", "--format=%s", "main"]),
        "other again\n"
    );
}

#[test]
fn b_switches_and_creates_branches_through_a_picker() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.commit_all("init");
    f.git(&["branch", "existing"]);
    let mut ed = open(&f);
    press(&mut ed, "b");
    let p = screen(&ed).branches.as_ref().expect("a picker");
    assert_eq!(p.matches.len(), 2);
    ed.feed_keys(&keys("topic"));
    assert!(screen(&ed).branches.as_ref().unwrap().offers_create());
    press(&mut ed, "\n");
    assert_eq!(f.git(&["branch", "--show-current"]).trim(), "topic");
    assert!(rows(&ed)[0].starts_with("Head: topic"));
    press(&mut ed, "b");
    ed.feed_keys(&keys("exi"));
    press(&mut ed, "\n");
    assert_eq!(f.git(&["branch", "--show-current"]).trim(), "existing");
    // Escape leaves it alone.
    press(&mut ed, "b");
    ed.feed_keys(&keys("<esc>"));
    assert!(screen(&ed).branches.is_none());
    assert_eq!(f.git(&["branch", "--show-current"]).trim(), "existing");
}

#[test]
fn z_stashes_and_capital_z_pops() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.commit_all("init");
    f.write("a.txt", "changed\n");
    let mut ed = open(&f);
    press(&mut ed, "z");
    assert!(status_is(&ed, Severity::Info, "stashed"), "{:?}", ed.status);
    assert_eq!(f.read("a.txt"), "a\n");
    let all = rows(&ed);
    assert!(all.iter().any(|r| r == "Stashes (1)"), "{all:#?}");
    assert!(all.iter().any(|r| r.starts_with("WIP on main")), "{all:#?}");
    press(&mut ed, "Z");
    assert!(
        status_is(&ed, Severity::Info, "popped stash@{0}"),
        "{:?}",
        ed.status
    );
    assert_eq!(f.read("a.txt"), "changed\n");
    assert!(!rows(&ed).iter().any(|r| r.starts_with("Stashes")));
    press(&mut ed, "Z");
    assert!(status_text(&ed).contains("no stash to pop"));
}

#[test]
fn l_shows_the_log_graph_and_enter_a_commit_diff() {
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
    let mut ed = open(&f);
    press(&mut ed, "l");
    let Some(Page::Log { entries, .. }) = screen(&ed).pages.last() else {
        panic!("the log page");
    };
    let subjects: Vec<&str> = entries
        .iter()
        .filter_map(|e| e.commit.as_ref().map(|c| c.subject.as_str()))
        .collect();
    assert_eq!(subjects.len(), 4, "{subjects:?}");
    assert_eq!(subjects[0], "merge side");
    assert_eq!(subjects[3], "first");
    assert!(entries.iter().any(|e| e.graph.contains('\\')), "a graph");
    // Down to "on main" and open it.
    let at = entries
        .iter()
        .position(|e| e.commit.as_ref().is_some_and(|c| c.subject == "on main"))
        .unwrap();
    for _ in 0..at {
        ed.feed_keys(&keys("j"));
    }
    press(&mut ed, "\n");
    let Some(Page::Commit { detail, rows, .. }) = screen(&ed).pages.last() else {
        panic!("the commit page");
    };
    assert_eq!(detail.message, "on main");
    assert!(
        rows.iter()
            .any(|r| matches!(r, maestro_editor::git_screen::DetailRow::Line(l) if l.text == "A"))
    );
    // q goes back a page at a time, then closes.
    ed.feed_keys(&keys("q"));
    assert!(matches!(screen(&ed).pages.last(), Some(Page::Log { .. })));
    ed.feed_keys(&keys("<esc>"));
    assert!(screen(&ed).pages.is_empty());
    // Enter on a recent commit shows it too.
    goto(&mut ed, "first");
    press(&mut ed, "\n");
    assert!(matches!(
        screen(&ed).pages.last(),
        Some(Page::Commit { .. })
    ));
    ed.feed_keys(&keys("qq"));
    assert!(ed.git.screen.is_none());
}

#[test]
fn r_refreshes_and_enter_opens_the_file_at_the_line() {
    let f = Fixture::new();
    f.write("a.txt", twenty());
    f.commit_all("init");
    let mut ed = open(&f);
    assert!(
        rows(&ed)
            .iter()
            .any(|r| r == "nothing to commit, working tree clean")
    );
    f.write("a.txt", twenty().replace("line 12\n", "line twelve\n"));
    press(&mut ed, "r");
    goto(&mut ed, "modified   a.txt");
    ed.feed_keys(&keys("<tab>"));
    goto(&mut ed, "+line twelve");
    ed.feed_keys(&keys("\n"));
    assert!(ed.git.screen.is_none());
    assert_eq!(ed.doc().path(), Some(f.repo.join("a.txt").as_path()));
    let text = ed.doc().text().clone();
    assert_eq!(text.char_to_line(ed.primary().head), 11);
}

// ----- buffers ------------------------------------------------------------

fn signs(ed: &Editor) -> Vec<(usize, Sign)> {
    let doc = ed.view().doc;
    ed.git
        .signs(doc)
        .map(|s| s.iter().collect())
        .unwrap_or_default()
}

fn cursor_line(ed: &Editor) -> usize {
    let t = ed.doc().text().clone();
    t.char_to_line(ed.primary().head)
}

#[test]
fn bracket_h_moves_between_changes_and_space_h_p_previews() {
    let f = Fixture::new();
    f.write("a.txt", twenty());
    f.commit_all("init");
    let path = f.write(
        "a.txt",
        twenty()
            .replace("line 3\n", "line three\n")
            .replace("line 10\n", "")
            .replace("line 16\n", "line 16\nextra\n"),
    );
    let mut ed = f.editor();
    ed.open(&path).unwrap();
    poll_until(&mut ed, "signs", |ed| signs(ed).len() == 3);
    ed.feed_keys(&keys("]h"));
    assert_eq!(cursor_line(&ed), 2);
    ed.feed_keys(&keys("]h"));
    assert_eq!(cursor_line(&ed), 8, "a removal is marked on the line above");
    ed.feed_keys(&keys("]h"));
    assert_eq!(cursor_line(&ed), 15);
    ed.feed_keys(&keys("]h"));
    assert_eq!(cursor_line(&ed), 15);
    assert!(status_text(&ed).contains("no next change"));
    ed.feed_keys(&keys("[h[h"));
    assert_eq!(cursor_line(&ed), 2);
    ed.feed_keys(&keys("2]h"));
    assert_eq!(cursor_line(&ed), 15);

    ed.feed_keys(&keys("gg]h"));
    ed.feed_keys(&keys(" hp"));
    let p = ed.git.popup.as_ref().expect("a popup");
    assert_eq!(p.title, "change 1 of 3: staged line 3 -> buffer line 3");
    let lines: Vec<String> = p.lines.iter().map(|l| l.text.clone()).collect();
    assert_eq!(lines, vec!["line 3", "line three"]);
    assert_eq!(p.lines[1].marks, vec![5..10]);
    // Any key closes it.
    ed.feed_keys(&keys("j"));
    assert!(ed.git.popup.is_none());
    ed.feed_keys(&keys("gg"));
    ed.feed_keys(&keys(" hp"));
    assert!(status_text(&ed).contains("no change at the cursor line"));
}

#[test]
fn space_h_s_stages_the_change_or_selected_lines_and_signs_follow() {
    let f = Fixture::new();
    f.write("a.txt", twenty());
    f.commit_all("init");
    let text = twenty()
        .replace("line 3\n", "line three\n")
        .replace("line 16\n", "line 16\nnew a\nnew b\n");
    let path = f.write("a.txt", &text);
    let before = file_state(&path);
    let mut ed = f.editor();
    ed.open(&path).unwrap();
    let want = vec![(2, Sign::Changed), (16, Sign::Added), (17, Sign::Added)];
    poll_until(&mut ed, "signs", |ed| signs(ed) == want);

    // The change under the cursor.
    ed.feed_keys(&keys("]h hs"));
    poll_until(&mut ed, "staging", |ed| {
        status_is(ed, Severity::Info, "staged the change in a.txt")
    });
    assert_eq!(
        f.git(&["show", ":a.txt"]),
        twenty().replace("line 3\n", "line three\n")
    );
    assert_eq!(file_state(&path), before);
    // Signs compare with the index, so that change's sign goes.
    poll_until(&mut ed, "signs after staging", |ed| {
        signs(ed) == vec![(16, Sign::Added), (17, Sign::Added)]
    });

    // One selected line of the other change.
    ed.feed_keys(&keys("17Gx hs"));
    poll_until(&mut ed, "line staging", |ed| {
        status_is(ed, Severity::Info, "staged lines 17-17")
    });
    assert_eq!(
        f.git(&["show", ":a.txt"]),
        twenty()
            .replace("line 3\n", "line three\n")
            .replace("line 16\n", "line 16\nnew a\n")
    );
    assert_eq!(file_state(&path), before);
    poll_until(&mut ed, "signs after line staging", |ed| {
        signs(ed) == vec![(17, Sign::Added)]
    });

    // Refused while the buffer has unsaved edits.
    ed.feed_keys(&keys("ggiX<esc>"));
    ed.feed_keys(&keys(" hs"));
    assert!(
        status_is(&ed, Severity::Error, "save the buffer first"),
        "{:?}",
        ed.status
    );
}

#[test]
fn space_h_r_resets_after_asking_and_u_undoes_it() {
    let f = Fixture::new();
    f.write("a.txt", "one\ntwo\nthree\n");
    f.commit_all("init");
    let path = f.write("a.txt", "one\nTWO\nthree\n");
    let mut ed = f.editor();
    ed.open(&path).unwrap();
    poll_until(&mut ed, "signs", |ed| signs(ed) == vec![(1, Sign::Changed)]);
    ed.feed_keys(&keys("j hr"));
    assert!(
        ed.pending_char_label()
            .unwrap()
            .contains("replace line 2 with the 1 staged line(s)")
    );
    ed.feed_keys(&keys("n"));
    assert_eq!(ed.doc().text().to_string(), "one\nTWO\nthree\n");
    ed.feed_keys(&keys(" hr"));
    ed.feed_keys(&keys("y"));
    assert_eq!(ed.doc().text().to_string(), "one\ntwo\nthree\n");
    assert_eq!(
        f.read("a.txt"),
        "one\nTWO\nthree\n",
        "the file waits for a save"
    );
    poll_until(&mut ed, "signs after reset", |ed| signs(ed).is_empty());
    ed.feed_keys(&keys("u"));
    assert_eq!(ed.doc().text().to_string(), "one\nTWO\nthree\n");
    poll_until(&mut ed, "signs after undo", |ed| {
        signs(ed) == vec![(1, Sign::Changed)]
    });
}

#[test]
fn space_h_b_blames_the_line_and_space_h_capital_b_shows_a_column() {
    let f = Fixture::new();
    f.write("a.txt", "one\ntwo\n");
    f.commit_all("first");
    f.write("a.txt", "one\ntwo\nthree\n");
    f.commit_all("second");
    let path = f.repo.join("a.txt");
    let mut ed = f.editor();
    ed.open(&path).unwrap();
    ed.feed_keys(&keys("G hb"));
    poll_until(&mut ed, "the blame popup", |ed| ed.git.popup.is_some());
    let p = ed.git.popup.as_ref().unwrap();
    assert_eq!(p.title, "blame line 3");
    let text: Vec<&str> = p.lines.iter().map(|l| l.text.as_str()).collect();
    assert!(text[0].starts_with("commit  "), "{text:?}");
    assert_eq!(text[1], "author  Maestro Test <test@example.invalid>");
    assert!(text[2].starts_with("date    20"), "{text:?}");
    assert_eq!(text[3], "summary second");

    let width = ed.gutter_width(ed.view());
    ed.feed_keys(&keys(" hB"));
    assert_eq!(
        ed.gutter_width(ed.view()),
        width + maestro_editor::git_hunks::BLAME_WIDTH
    );
    let doc = ed.view().doc;
    poll_until(&mut ed, "the blame column", |ed| {
        maestro_editor::git_hunks::blame_cell(ed, doc, 2).is_some()
    });
    let first = maestro_editor::git_hunks::blame_cell(&ed, doc, 0)
        .unwrap()
        .clone();
    let third = maestro_editor::git_hunks::blame_cell(&ed, doc, 2)
        .unwrap()
        .clone();
    assert_ne!(first.short, third.short);
    assert_eq!(first.author, "Maestro Test");
    // An edit makes the new line uncommitted once blame runs again.
    ed.feed_keys(&keys("ggOtop<esc>"));
    poll_until(&mut ed, "blame after an edit", |ed| {
        maestro_editor::git_hunks::blame_cell(ed, doc, 0).is_some_and(|c| c.uncommitted)
    });
    ed.feed_keys(&keys(" hB"));
    assert_eq!(ed.gutter_width(ed.view()), width);
}

// ----- regressions from the independent review ------------------------------

#[test]
fn closing_a_message_without_saving_aborts_even_a_prefilled_amend() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.commit_all("init");
    f.write("a.txt", "b\n");
    f.commit_all("Keep this subject");
    f.write("a.txt", "c\n");
    f.git(&["add", "a.txt"]);
    let head = f.git(&["rev-parse", "HEAD"]);
    let mut ed = open(&f);
    ed.feed_keys(&keys("ca"));
    poll_until(&mut ed, "the message buffer", |ed| {
        ed.git.commit_message.is_some()
    });
    let path = ed.doc().path().unwrap().to_path_buf();
    assert!(
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("MAESTRO_EDITMSG-")
    );
    // The prefilled message is on disk, but it was never saved from the
    // buffer: :q aborts.
    ed.feed_keys(&keys(":q\n"));
    poll_until(&mut ed, "the abort", |ed| {
        status_is(
            ed,
            Severity::Warning,
            "amend aborted: the message buffer was closed without saving",
        )
    });
    assert_eq!(f.git(&["rev-parse", "HEAD"]), head);
    assert!(!path.exists(), "the message file is removed");

    // Unsaved edits, then the buffer closed by force: aborted too.
    press(&mut ed, " g");
    ed.feed_keys(&keys("ca"));
    poll_until(&mut ed, "the message buffer", |ed| {
        ed.git.commit_message.is_some()
    });
    ed.feed_keys(&keys("ggiNew <esc>"));
    ed.poll_background();
    ed.feed_keys(&keys(":bc!\n"));
    poll_until(&mut ed, "the abort", |ed| {
        status_is(ed, Severity::Warning, "amend aborted")
    });
    assert_eq!(f.git(&["rev-parse", "HEAD"]), head);
}

#[test]
fn a_commit_is_refused_when_head_moved_while_the_message_was_open() {
    let f = Fixture::new();
    f.write("a.txt", "a\n");
    f.commit_all("init");
    f.write("a.txt", "b\n");
    f.git(&["add", "a.txt"]);
    let mut ed = open(&f);
    ed.feed_keys(&keys("cc"));
    poll_until(&mut ed, "the message buffer", |ed| {
        ed.git.commit_message.is_some()
    });
    let path = ed.doc().path().unwrap().to_path_buf();
    // Someone commits meanwhile.
    f.git(&["commit", "-q", "--allow-empty", "-m", "from elsewhere"]);
    ed.feed_keys(&keys("iMine<esc>:wq\n"));
    poll_until(&mut ed, "the refusal", |ed| {
        status_is(ed, Severity::Error, "not committed: HEAD moved")
    });
    assert_eq!(f.git(&["log", "--format=%s"]), "from elsewhere\ninit\n");
    assert!(path.exists(), "the message is kept");
    assert!(std::fs::read_to_string(&path).unwrap().starts_with("Mine"));
}

#[test]
fn discarding_a_staged_deletion_never_overwrites_a_recreated_file() {
    let f = Fixture::new();
    f.write("a.txt", "committed\n");
    f.commit_all("init");
    f.git(&["rm", "-q", "a.txt"]);
    let mut ed = open(&f);
    goto(&mut ed, "deleted    a.txt");
    ed.feed_keys(&keys("x"));
    assert!(screen(&ed).dialog.is_some());
    // The file is made again while the dialog is up.
    f.write("a.txt", "new work\n");
    press(&mut ed, "y");
    assert!(
        status_is(&ed, Severity::Error, "a.txt is an untracked file now"),
        "{:?}",
        ed.status
    );
    assert_eq!(f.read("a.txt"), "new work\n");
}

#[test]
fn unstaging_the_hunk_of_a_renamed_file_keeps_the_rename() {
    let f = Fixture::new();
    f.write("old.txt", twenty());
    f.commit_all("init");
    f.git(&["mv", "old.txt", "new.txt"]);
    f.write("new.txt", twenty().replace("line 2\n", "line two\n"));
    f.git(&["add", "new.txt"]);
    let mut ed = open(&f);
    goto(&mut ed, "renamed    old.txt -> new.txt");
    ed.feed_keys(&keys("<tab>"));
    goto(&mut ed, "@@ -1,5 +1,5 @@");
    press(&mut ed, "u");
    assert_eq!(
        f.git(&["diff", "--cached", "-M", "--name-status"]),
        "R100\told.txt\tnew.txt\n"
    );
    assert_eq!(f.git(&["show", ":new.txt"]), twenty());
}

#[test]
fn reset_counts_lines_the_way_git_does() {
    let f = Fixture::new();
    // A lone CR is not a line break for git; the rope would count it.
    f.write("cr.txt", "a\rb\nc\n");
    f.commit_all("init");
    let path = f.write("cr.txt", "A\rb\nc\n");
    let mut ed = f.editor();
    ed.open(&path).unwrap();
    poll_until(&mut ed, "signs", |ed| signs(ed) == vec![(0, Sign::Changed)]);
    ed.feed_keys(&keys(" hr"));
    ed.feed_keys(&keys("y"));
    assert_eq!(ed.doc().text().to_string(), "a\rb\nc\n");
}

#[test]
fn the_blame_column_follows_a_commit() {
    let f = Fixture::new();
    f.write("a.txt", "one\n");
    f.commit_all("first");
    let path = f.write("a.txt", "one\ntwo\n");
    let mut ed = f.editor();
    ed.open(&path).unwrap();
    let doc = ed.view().doc;
    ed.feed_keys(&keys(" hB"));
    poll_until(&mut ed, "blame", |ed| {
        maestro_editor::git_hunks::blame_cell(ed, doc, 1).is_some_and(|c| c.uncommitted)
    });
    std::thread::sleep(Duration::from_millis(20));
    f.commit_all("second");
    // The editor notices the commit on its tick, as the terminal loop
    // calls it.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !maestro_editor::git_hunks::blame_cell(&ed, doc, 1).is_some_and(|c| !c.uncommitted) {
        ed.tick(Instant::now());
        ed.poll_background();
        assert!(Instant::now() < deadline, "blame did not follow the commit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn hunk_and_screen_keys_are_bound_where_nothing_else_was() {
    use maestro_editor::keymap::{Keymaps, Lookup};
    let maps = Keymaps::for_config(&Config::default());
    for (k, c) in [
        ("] h", "goto_next_hunk"),
        ("[ h", "goto_prev_hunk"),
        ("space g", "git_screen"),
        ("space h s", "stage_hunk"),
        ("space h r", "reset_hunk"),
        ("space h p", "preview_hunk"),
        ("space h b", "blame_line"),
        ("space h B", "toggle_blame"),
    ] {
        let seq = maestro_editor::key::parse_sequence(k).unwrap();
        assert_eq!(
            maps.lookup(maestro_editor::Mode::Normal, &seq),
            Lookup::Command(c.into()),
            "{k}"
        );
        let n = maps
            .bindings(maestro_editor::Mode::Normal)
            .iter()
            .filter(|(_, cmd)| cmd == c)
            .count();
        assert_eq!(n, 1, "{c} is bound once");
    }
    // Everything under space h is ours.
    let h = maps
        .node(
            maestro_editor::Mode::Normal,
            &maestro_editor::key::parse_sequence("space h").unwrap(),
        )
        .unwrap();
    assert_eq!(h.children().unwrap().len(), 5);
}
