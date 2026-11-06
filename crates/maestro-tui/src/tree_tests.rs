//! Drawing tests for the file tree (`tree.rs`) and its diff, against a
//! real repository made with the git CLI under a temporary HOME (the
//! user's config is never read).

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maestro_config::Config;
use maestro_editor::git::{Git, GitState};
use maestro_editor::workspaces::tree::Changes;
use maestro_editor::workspaces::tree::diff::DiffState;
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect};

use crate::render::{render, sidebar_area, split_screen, tree_area};
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
    /// `main` with `a.txt`, `old.txt` and `src/lib.rs`; branch `feature`
    /// commits an edit to `src/lib.rs` and deletes `old.txt`, then edits
    /// `a.txt` without committing and adds an untracked `notes.md`.
    fn feature() -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("maestro-tui-tree-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let root = tmp.join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let r = Repo { tmp, root };
        r.git(&["init", "-q"]);
        r.write("a.txt", "one\ntwo\nthree\n");
        r.write("old.txt", "old\n");
        r.write("src/lib.rs", "pub fn f() {}\n");
        r.git(&["add", "-A"]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.git(&["switch", "-q", "-c", "feature"]);
        r.write("src/lib.rs", "pub fn f() {}\npub fn g() {}\n");
        r.git(&["rm", "-q", "old.txt"]);
        r.git(&["commit", "-q", "-am", "work"]);
        r.write("a.txt", "one\nTWO\nthree\n");
        r.write("notes.md", "new\n");
        r
    }

    fn git(&self, args: &[&str]) {
        let mut cmd = Command::new("git");
        for c in CONFIG {
            cmd.arg("-c").arg(c);
        }
        cmd.args(args)
            .current_dir(&self.root)
            .env("HOME", self.tmp.join("home"))
            .env("XDG_CONFIG_HOME", self.tmp.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.tmp.join("home/.gitconfig"))
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.root.join(rel), text).unwrap();
    }

    /// An editor in the repository with the tree opened by `space t` and
    /// settled, laid out for a `w` x `h` screen.
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
        ed.handle_key(KeyEvent::char(' '));
        ed.handle_key(KeyEvent::char('t'));
        assert!(ed.workspaces.tree.has_focus());
        let (_, views, _) = split_screen(w, h, &ed);
        ed.resize(views);
        poll_until(&mut ed, "the tree", |ed| {
            !matches!(ed.workspaces.tree.changes, Changes::Loading) && !ed.has_background_work()
        });
        ed
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

fn draw(ed: &Editor, w: u16, h: u16) -> Surface {
    let mut s = Surface::new(w, h, ed.theme.ui.text);
    render(ed, &mut s);
    s
}

fn row(s: &Surface, y: u16) -> String {
    s.row(y).iter().map(|c| c.symbol.as_str()).collect()
}

/// Column `x0` onwards of row `y`, trailing spaces dropped.
fn tail(s: &Surface, y: u16, x0: u16) -> String {
    let r: String = s.row(y)[x0 as usize..]
        .iter()
        .map(|c| c.symbol.as_str())
        .collect();
    r.trim_end().to_string()
}

#[test]
fn the_tree_draws_change_marks_right_of_the_views() {
    let repo = Repo::feature();
    let mut ed = repo.editor(100, 20);
    ed.handle_key(KeyEvent::char('j'));
    let s = draw(&ed, 100, 20);
    // 32 columns at the right, the divider just left of them.
    let area = tree_area(100, 20, &ed).unwrap();
    assert_eq!((area.x, area.width), (68, 32));
    assert_eq!(ed.area.right(), 67);
    assert_eq!(s.cell(67, 4).unwrap().symbol, "│");
    let text: Vec<String> = (1..19).map(|y| tail(&s, y, 68)).collect();
    let want = [
        " files: feature",
        " 4 changed vs main (merge-base…",
        " > src/ (1)",
        " M a.txt  modified",
        " ? notes.md",
        " D old.txt",
    ];
    assert_eq!(&text[..want.len()], want, "{text:#?}");
    // The letter carries the change; its color is the Git sign color.
    let ui = &ed.theme.ui;
    let d = &ed.theme.diff;
    assert_eq!(s.cell(69, 5).unwrap().style.fg, d.git_add.fg);
    assert_eq!(s.cell(69, 6).unwrap().style.fg, d.git_removed.fg);
    assert_eq!(s.cell(71, 3).unwrap().style.fg, ui.file_dir.fg);
    // The selected row is in the picker's selection colors while the tree
    // has the keys.
    assert_eq!(s.cell(71, 4).unwrap().style, ui.popup_selected);
    assert_eq!(s.cell(71, 5).unwrap().style.fg, ui.text.fg);
    assert_eq!(tail(&s, 18, 68), " ret open  d diff  c changed");
}

#[test]
fn changed_only_and_the_diff_draw_over_the_views() {
    let repo = Repo::feature();
    let mut ed = repo.editor(100, 20);
    ed.handle_key(KeyEvent::char('c'));
    let s = draw(&ed, 100, 20);
    let text: Vec<String> = (1..8).map(|y| tail(&s, y, 68)).collect();
    assert_eq!(
        text,
        [
            " changed files: feature",
            " 4 changed vs main (merge-base…",
            " v src/ (1)",
            "   M lib.rs",
            " M a.txt",
            " ? notes.md",
            " D old.txt",
        ]
    );
    // d on a.txt: the diff covers the views, the tree stays beside it.
    for _ in 0..2 {
        ed.handle_key(KeyEvent::char('j'));
    }
    ed.handle_key(KeyEvent::char('d'));
    poll_until(&mut ed, "the diff", |ed| {
        ed.workspaces
            .tree
            .diff
            .as_ref()
            .is_some_and(|d| d.state != DiffState::Loading)
    });
    let s = draw(&ed, 100, 20);
    let header = row(&s, 1);
    assert!(
        header.contains("diff: a.txt  modified vs main (merge-base"),
        "{header:?}"
    );
    assert!(header.contains("KEYS: TREE DIFF"), "{header:?}");
    let body: Vec<String> = (2..8)
        .map(|y| row(&s, y)[..60].trim_end().to_string())
        .collect();
    assert!(body[0].contains("@@ -1,3 +1,3 @@"), "{body:#?}");
    assert!(body[2].contains("2      -two"), "{body:#?}");
    assert!(body[3].contains("     2 +TWO"), "{body:#?}");
    assert!(row(&s, 18).contains("q back to the tree"));
    // The cursor (on the hunk header) is a mark in the first column.
    assert_eq!(s.cell(0, 2).unwrap().symbol, ">");
    // On the removed line it keeps its changed-word background.
    ed.handle_key(KeyEvent::char('j'));
    ed.handle_key(KeyEvent::char('j'));
    let s = draw(&ed, 100, 20);
    assert_eq!(s.cell(0, 4).unwrap().symbol, ">");
    let at = row(&s, 4).find("two").unwrap() as u16;
    assert_eq!(
        s.cell(at, 4).unwrap().style.bg,
        Some(ed.theme.diff.delete_text_bg)
    );
    assert_eq!(tail(&s, 3, 68), " v src/ (1)");
    ed.handle_key(KeyEvent::plain(KeyCode::Esc));
    assert!(ed.workspaces.tree.diff.is_none());
}

#[test]
fn both_columns_leave_the_views_room_and_narrow_screens_drop_the_tree() {
    let repo = Repo::feature();
    let mut ed = repo.editor(120, 30);
    ed.handle_key(KeyEvent::char(' '));
    ed.handle_key(KeyEvent::char('s'));
    assert!(ed.workspaces.sidebar.has_focus());
    // At 120 columns: the sidebar's 36, the tree's 32, and 50 for the
    // views between the two dividers.
    let side = sidebar_area(120, 30, &ed).unwrap();
    let tree = tree_area(120, 30, &ed).unwrap();
    let (_, views, _) = split_screen(120, 30, &ed);
    assert_eq!(
        (side.width, tree.width, views.x, views.width),
        (36, 32, 37, 50)
    );
    assert_eq!(views.right() + 1, tree.x);
    // At 80, the tree takes half of what the sidebar leaves, short of the
    // views' 20 columns.
    let tree = tree_area(80, 30, &ed).unwrap();
    let (_, views, _) = split_screen(80, 30, &ed);
    assert_eq!((tree.width, views.width), (21, 21));
    // At 60 there is no room for it next to the sidebar.
    assert_eq!(tree_area(60, 30, &ed), None);
    assert_eq!(split_screen(60, 30, &ed).1.right(), 60);
    // Every size draws without a panic, with one column or both.
    for both in [true, false] {
        ed.workspaces.sidebar.open = both;
        for (w, h) in (1..=70u16).flat_map(|w| (1..=12u16).map(move |h| (w, h))) {
            let (_, views, _) = split_screen(w, h, &ed);
            ed.resize(views);
            ed.workspaces.tree.too_narrow = crate::render::tree_room(w, h, &ed).is_none();
            let mut s = Surface::new(w, h, ed.theme.ui.text);
            render(&ed, &mut s);
        }
    }
}
