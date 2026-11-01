//! Drawing tests for the Git status screen, the hunk and blame popups and
//! the blame column, against real repositories made with the git CLI under
//! a temporary HOME (the user's config is never read).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maestro_config::Config;
use maestro_editor::git::{Git, GitState};
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect};

use crate::surface::Surface;

static NEXT: AtomicUsize = AtomicUsize::new(0);

const CONFIG: &[&str] = &[
    "user.name=Maestro Test",
    "user.email=test@example.invalid",
    "init.defaultBranch=main",
    "commit.gpgSign=false",
];

struct Repo {
    tmp: PathBuf,
    root: PathBuf,
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

impl Repo {
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("maestro-tui-git-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let root = tmp.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let r = Repo { tmp, root };
        r.git(&["init", "-q"]);
        r
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env("HOME", self.tmp.join("home"))
            .env("XDG_CONFIG_HOME", self.tmp.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.tmp.join("home/.gitconfig"))
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
    }

    fn git(&self, args: &[&str]) {
        let mut cmd = Command::new("git");
        for c in CONFIG {
            cmd.arg("-c").arg(c);
        }
        cmd.args(args).current_dir(&self.root);
        self.env(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.root.join(rel);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn editor(&self, w: u16, h: u16) -> Editor {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 1, w, h.saturating_sub(2)));
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
        ed.git = GitState::new(g);
        ed.cwd = self.root.clone();
        ed
    }
}

fn twenty() -> String {
    (1..=20).map(|i| format!("line {i}\n")).collect()
}

fn feed(ed: &mut Editor, script: &str) {
    for c in script.chars() {
        let k = match c {
            '\t' => KeyEvent::plain(KeyCode::Tab),
            '\n' => KeyEvent::plain(KeyCode::Enter),
            '\x1b' => KeyEvent::plain(KeyCode::Esc),
            c => KeyEvent::char(c),
        };
        ed.handle_key(k);
    }
}

fn poll_until(ed: &mut Editor, what: &str, mut done: impl FnMut(&Editor) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(ed) {
        ed.poll_background();
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn settled(ed: &Editor) -> bool {
    ed.git
        .screen
        .as_ref()
        .is_some_and(|s| !s.loading && s.busy.is_none() && s.data.is_some())
}

fn draw(ed: &Editor, w: u16, h: u16) -> Vec<String> {
    let mut s = Surface::new(w, h, ed.theme.ui.text);
    crate::render::render(ed, &mut s);
    (0..h)
        .map(|y| s.row(y).iter().map(|c| c.symbol.as_str()).collect())
        .collect()
}

fn find(rows: &[String], text: &str) -> usize {
    rows.iter()
        .position(|r| r.contains(text))
        .unwrap_or_else(|| panic!("{text:?} not drawn:\n{}", rows.join("\n")))
}

#[test]
fn status_screen_draws_head_sections_hunks_and_hints() {
    let r = Repo::new();
    r.write("a.txt", &twenty());
    r.write("b.txt", "b\n");
    r.git(&["add", "-A"]);
    r.git(&["commit", "-q", "-m", "first commit"]);
    r.write("a.txt", &twenty().replace("line 2\n", "line two\n"));
    r.write("b.txt", "B\n");
    r.git(&["add", "b.txt"]);
    let (w, h) = (100, 30);
    let mut ed = r.editor(w, h);
    feed(&mut ed, " g");
    poll_until(&mut ed, "the screen", settled);
    feed(&mut ed, "\t");
    let rows = draw(&ed, w, h);
    let text = rows.join("\n");
    assert!(
        rows[1].contains(&format!(" git status: {} ", r.root.display())),
        "{}",
        rows[1]
    );
    assert!(rows[1].contains("KEYS: GIT"), "{}", rows[1]);
    assert!(rows[2].contains("Head        main  first commit"), "{text}");
    assert!(rows[3].contains("Upstream    none"), "{text}");
    let un = find(&rows, "Unstaged (1)");
    assert!(rows[un + 1].contains("modified   a.txt  +1 -1"), "{text}");
    // The cursor sits on the open file.
    assert!(rows[un + 1].starts_with('▌'), "{:?}", rows[un + 1]);
    assert!(rows[un + 2].contains("@@ -1,5 +1,5 @@"), "{text}");
    let minus = find(&rows, "-line 2");
    assert!(rows[minus + 1].contains("+line two"), "{text}");
    assert!(
        rows[minus].contains("   2      -line 2"),
        "{:?}",
        rows[minus]
    );
    let st = find(&rows, "Staged (1)");
    assert!(
        rows[st + 1].contains("modified   b.txt  +1 -1  (tab shows hunks)"),
        "{text}"
    );
    find(&rows, "Recent commits (1)");
    let hints = &rows[(h - 2) as usize];
    assert!(
        hints.contains("tab hunks") && hints.contains("s stage"),
        "{hints}"
    );

    // A line selection is drawn and the hints change.
    let target = ed
        .git
        .screen
        .as_ref()
        .unwrap()
        .rows
        .iter()
        .position(|r| matches!(r, maestro_editor::git_screen::Row::Line { row, .. } if row.text == "line 2"))
        .unwrap();
    while ed.git.screen.as_ref().unwrap().cursor < target {
        feed(&mut ed, "j");
    }
    feed(&mut ed, "vj");
    let rows = draw(&ed, w, h);
    let hints = &rows[(h - 2) as usize];
    assert!(hints.contains("s stage lines"), "{hints}");

    // The discard dialog.
    feed(&mut ed, "x");
    let rows = draw(&ed, w, h);
    let text = rows.join("\n");
    assert!(
        text.contains("discard 2 line(s) of hunk 1 of a.txt"),
        "{text}"
    );
    assert!(text.contains("Git cannot bring them back"), "{text}");
    assert!(text.contains("y discard"), "{text}");
    feed(&mut ed, "n");

    // Never panics at small sizes.
    for (w, h) in [(20, 5), (21, 6), (30, 8), (5, 3)] {
        let mut s = Surface::new(w, h, ed.theme.ui.text);
        ed.resize(Rect::new(0, 1, w, h.saturating_sub(2)));
        crate::render::render(&ed, &mut s);
    }
}

#[test]
fn log_page_branch_picker_and_commit_page_are_drawn() {
    let r = Repo::new();
    r.write("a.txt", "a\n");
    r.git(&["add", "-A"]);
    r.git(&["commit", "-q", "-m", "first"]);
    r.write("a.txt", "A\n");
    r.git(&["commit", "-q", "-am", "second"]);
    let (w, h) = (100, 24);
    let mut ed = r.editor(w, h);
    feed(&mut ed, " g");
    poll_until(&mut ed, "the screen", settled);
    feed(&mut ed, "b");
    poll_until(&mut ed, "the picker", |ed| {
        ed.git.screen.as_ref().is_some_and(|s| s.branches.is_some())
    });
    feed(&mut ed, "new");
    let rows = draw(&ed, w, h);
    let text = rows.join("\n");
    assert!(text.contains("switch or create a branch"), "{text}");
    assert!(text.contains("> new"), "{text}");
    assert!(text.contains("create branch new from HEAD"), "{text}");
    feed(&mut ed, "\x1b");

    feed(&mut ed, "l");
    poll_until(&mut ed, "the log", |ed| {
        ed.git.screen.as_ref().is_some_and(|s| !s.pages.is_empty())
    });
    let rows = draw(&ed, w, h);
    assert!(rows[1].contains("git log:"), "{}", rows[1]);
    let top = find(&rows, "second");
    assert!(rows[top].contains("* "), "{:?}", rows[top]);
    assert!(rows[top].contains("(HEAD -> main)"), "{:?}", rows[top]);
    assert!(rows[top].contains("Maestro Test"), "{:?}", rows[top]);
    find(&rows, "first");
    feed(&mut ed, "\n");
    poll_until(&mut ed, "the commit", |ed| {
        ed.git.screen.as_ref().is_some_and(|s| s.pages.len() == 2)
    });
    let rows = draw(&ed, w, h);
    let text = rows.join("\n");
    assert!(rows[1].contains("git commit "), "{}", rows[1]);
    assert!(
        text.contains("Author: Maestro Test <test@example.invalid>"),
        "{text}"
    );
    assert!(text.contains("    second"), "{text}");
    assert!(text.contains("modified   a.txt"), "{text}");
    assert!(text.contains("-a"), "{text}");
    assert!(text.contains("+A"), "{text}");
}

#[test]
fn blame_column_and_hunk_popup_in_a_buffer() {
    let r = Repo::new();
    r.write("a.txt", "one\ntwo\nthree\n");
    r.git(&["add", "-A"]);
    r.git(&["commit", "-q", "-m", "first"]);
    let path = r.write("a.txt", "one\nTWO\nthree\n");
    let (w, h) = (90, 12);
    let mut ed = r.editor(w, h);
    ed.open(Path::new(&path)).unwrap();
    let doc = ed.view().doc;
    poll_until(&mut ed, "signs", |ed| {
        ed.git.signs(doc).is_some_and(|s| !s.is_empty())
    });
    feed(&mut ed, "j hp");
    let rows = draw(&ed, w, h);
    let text = rows.join("\n");
    assert!(
        text.contains("change 1 of 1: staged line 2 -> buffer line 2"),
        "{text}"
    );
    let minus = find(&rows, "-two");
    assert!(rows[minus + 1].contains("+TWO"), "{text}");

    feed(&mut ed, " hB");
    poll_until(&mut ed, "the blame column", |ed| {
        maestro_editor::git_hunks::blame_cell(ed, doc, 0).is_some()
    });
    let rows = draw(&ed, w, h);
    let first = &rows[1];
    // Blame, then the sign column, then the line number, then the text.
    assert!(first.contains(" Maestro Tes 20"), "{first:?}");
    assert!(first.contains("   1 one"), "{first:?}");
    let second = &rows[2];
    assert!(second.starts_with("not committed yet"), "{second:?}");
    assert!(second.contains("~  2 TWO"), "{second:?}");
}
