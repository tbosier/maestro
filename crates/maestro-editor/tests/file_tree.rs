//! The file tree (`space t`): keys in through `Editor::handle_key`, real
//! Git repositories made with the git CLI under a temporary HOME (the
//! user's config is never read), and the background listing and
//! comparison applied with `poll_background`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maestro_config::Config;
use maestro_editor::git::{Git, GitState};
use maestro_editor::workspaces::tree::diff::DiffState;
use maestro_editor::workspaces::tree::{self, Changes, Listing, RowKind};
use maestro_editor::{Editor, KeyEvent, Mode, MouseKind, Rect};

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
    /// An empty directory `repo` (not yet a repository).
    fn bare_dir() -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("maestro-tree-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        Fixture { tmp, repo }
    }

    /// A repository whose `main` has `a.txt`, `b.txt`, `old.txt`,
    /// `src/lib.rs` and `docs/guide.md`, ignoring `target/`.
    fn on_main() -> Self {
        let f = Self::bare_dir();
        f.git(&["init", "-q"]);
        f.write("a.txt", "one\ntwo\nthree\n");
        f.write("b.txt", "bee\n");
        f.write("old.txt", "old\n");
        f.write("src/lib.rs", "pub fn f() {}\n");
        f.write("docs/guide.md", "# guide\n");
        f.write(".gitignore", "target/\n");
        f.git(&["add", "-A"]);
        f.git(&["commit", "-q", "-m", "base"]);
        f.write("target/out.bin", "built\n");
        f
    }

    /// [`Fixture::on_main`] on branch `feature`, which committed an edit
    /// to `src/lib.rs` and the deletion of `old.txt`, then has an
    /// uncommitted edit to `a.txt` and an untracked `notes.md`.
    fn feature() -> Self {
        let f = Self::on_main();
        f.git(&["switch", "-q", "-c", "feature"]);
        f.write("src/lib.rs", "pub fn f() {}\npub fn g() {}\n");
        f.git(&["rm", "-q", "old.txt"]);
        f.git(&["commit", "-q", "-am", "feature work"]);
        f.write("a.txt", "one\nTWO\nthree\n");
        f.write("notes.md", "untracked\n");
        f
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

    /// An editor in the repository whose views end at column 80, as if the
    /// front end had put a tree right of them.
    fn editor(&self) -> Editor {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 80, 28));
        ed.git = GitState::new(self.api());
        ed.cwd = self.repo.clone();
        ed
    }
}

fn run(ed: &mut Editor, script: &str) {
    for c in script.chars() {
        let k = match c {
            '\n' => KeyEvent::plain(maestro_editor::KeyCode::Enter),
            '\x1b' => KeyEvent::plain(maestro_editor::KeyCode::Esc),
            c => KeyEvent::char(c),
        };
        ed.handle_key(k);
    }
}

fn status_text(ed: &Editor) -> String {
    ed.status
        .as_ref()
        .map(|(_, m)| m.clone())
        .unwrap_or_default()
}

fn poll_until(ed: &mut Editor, what: &str, mut done: impl FnMut(&Editor) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        ed.poll_background();
        if done(ed) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; tree {:?}",
            ed.workspaces.tree.changes
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until the comparison has finished and every listing asked for
/// has arrived.
fn settle(ed: &mut Editor) {
    poll_until(ed, "the tree", |ed| {
        let t = &ed.workspaces.tree;
        !matches!(t.changes, Changes::Loading)
            && t.dirs.values().all(|l| *l != Listing::Loading)
            && !ed.has_background_work()
    });
}

/// The rows as text: indent, then the mark (`>`/`v` for directories, the
/// change letter or a space for files), the name and a directory's count.
fn shown(ed: &Editor) -> Vec<String> {
    tree::rows(ed)
        .iter()
        .map(|r| {
            let pad = "  ".repeat(r.depth);
            match &r.kind {
                RowKind::Dir { expanded, changed } => {
                    let m = if *expanded { 'v' } else { '>' };
                    if *changed > 0 {
                        format!("{pad}{m} {}/ ({changed})", r.name)
                    } else {
                        format!("{pad}{m} {}/", r.name)
                    }
                }
                RowKind::File { change } => {
                    let m = change.as_ref().map_or(' ', |c| c.letter());
                    format!("{pad}{m} {}", r.name)
                }
                RowKind::Note(t) => format!("{pad}({t})"),
            }
        })
        .collect()
}

fn selected(ed: &Editor) -> Option<PathBuf> {
    let rows = tree::rows(ed);
    tree::selected_index(ed, &rows).map(|i| rows[i].path.clone())
}

/// Moves the selection to `path` with `g` and `j`.
fn select(ed: &mut Editor, path: &str) {
    run(ed, "g");
    for _ in 0..100 {
        if selected(ed).as_deref() == Some(Path::new(path)) {
            return;
        }
        run(ed, "j");
    }
    panic!("no row {path} in {:#?}", shown(ed));
}

fn open_tree(ed: &mut Editor) {
    run(ed, " t");
    assert!(ed.workspaces.tree.has_focus());
    settle(ed);
}

#[test]
fn space_t_opens_focuses_and_hides_the_tree() {
    let f = Fixture::on_main();
    let mut ed = f.editor();
    run(&mut ed, ":e a.txt\n");
    open_tree(&mut ed);
    assert!(ed.workspaces.tree.open && ed.workspaces.tree.focused);
    // Keys go to the tree, not the buffer.
    run(&mut ed, "i");
    assert_eq!(ed.mode, Mode::Normal);
    ed.paste_text("pasted");
    assert!(!ed.doc().is_modified());
    assert!(status_text(&ed).contains("file tree has the keys"));
    // esc gives the keys back; the tree stays.
    run(&mut ed, "\x1b");
    assert!(ed.workspaces.tree.open && !ed.workspaces.tree.focused);
    run(&mut ed, "i");
    assert_eq!(ed.mode, Mode::Insert);
    run(&mut ed, "\x1b");
    // space t focuses it again, then hides it.
    run(&mut ed, " t");
    assert!(ed.workspaces.tree.has_focus());
    run(&mut ed, " t");
    assert!(!ed.workspaces.tree.open);
    // q hides it too.
    run(&mut ed, " t");
    run(&mut ed, "q");
    assert!(!ed.workspaces.tree.open);
    // :tree does what space t does.
    run(&mut ed, ":tree\n");
    assert!(ed.workspaces.tree.has_focus());
    // A space command from the tree acts on the editor.
    run(&mut ed, " f");
    assert!(ed.picker.is_some());
    assert!(ed.workspaces.tree.open && !ed.workspaces.tree.focused);
    run(&mut ed, "\x1b");
    // The sidebar and the tree take turns with the keys.
    run(&mut ed, " t");
    run(&mut ed, " s");
    assert!(ed.workspaces.sidebar.has_focus());
    assert!(ed.workspaces.tree.open && !ed.workspaces.tree.focused);
    run(&mut ed, " t");
    assert!(ed.workspaces.tree.has_focus());
    assert!(ed.workspaces.sidebar.open && !ed.workspaces.sidebar.focused);
    settle(&mut ed);
}

#[test]
fn the_tree_marks_what_the_branch_changed_since_main() {
    let f = Fixture::feature();
    // A tag with the branch's name must not stand in for it.
    f.git(&["tag", "main", "HEAD"]);
    let mut ed = f.editor();
    open_tree(&mut ed);
    // Directories first and folded; ignored and hidden unchanged files
    // left out; the deleted file still listed.
    assert_eq!(
        shown(&ed),
        [
            "> docs/",
            "> src/ (1)",
            "M a.txt",
            "  b.txt",
            "? notes.md",
            "D old.txt",
        ]
    );
    let main = f.git(&["rev-parse", "--short", "refs/heads/main"]);
    let [title, detail] = tree::header(&ed);
    assert_eq!(title, "files: feature");
    assert_eq!(detail, format!("4 changed vs main (merge-base {main})"));

    // Only changed files, as a pruned tree with its directories open.
    run(&mut ed, "c");
    assert_eq!(
        shown(&ed),
        [
            "v src/ (1)",
            "  M lib.rs",
            "M a.txt",
            "? notes.md",
            "D old.txt"
        ]
    );
    assert_eq!(tree::header(&ed)[0], "changed files: feature");
    // h folds a directory there too.
    select(&mut ed, "src");
    run(&mut ed, "h");
    assert_eq!(shown(&ed)[..2], ["> src/ (1)", "M a.txt"]);
    run(&mut ed, "c");
    assert_eq!(shown(&ed).len(), 6);

    // A save is noticed on the next tick and the marks follow.
    run(&mut ed, "\x1b");
    run(&mut ed, ":e b.txt\n");
    ed.paste_text("x");
    run(&mut ed, ":w\n");
    ed.tick(Instant::now());
    poll_until(&mut ed, "b.txt marked", |ed| {
        shown(ed).contains(&"M b.txt".to_string())
    });
    assert!(tree::header(&ed)[1].starts_with("5 changed vs main"));
}

#[test]
fn directories_fold_and_files_open_in_the_focused_view() {
    let f = Fixture::feature();
    let mut ed = f.editor();
    open_tree(&mut ed);
    select(&mut ed, "src");
    run(&mut ed, "l");
    settle(&mut ed);
    assert_eq!(shown(&ed)[1..3], ["v src/ (1)", "  M lib.rs"]);
    // l on an open directory steps into it; h on a file folds its
    // directory and selects it.
    run(&mut ed, "l");
    assert_eq!(selected(&ed).as_deref(), Some(Path::new("src/lib.rs")));
    run(&mut ed, "h");
    assert_eq!(selected(&ed).as_deref(), Some(Path::new("src")));
    assert_eq!(shown(&ed)[1], "> src/ (1)");
    // ret toggles a directory.
    run(&mut ed, "\n");
    settle(&mut ed);
    assert_eq!(shown(&ed)[1], "v src/ (1)");
    run(&mut ed, "\n");
    assert_eq!(shown(&ed)[1], "> src/ (1)");

    // ret on a file opens it and gives the keys back.
    select(&mut ed, "a.txt");
    run(&mut ed, "\n");
    assert_eq!(ed.doc().path(), Some(f.repo.join("a.txt").as_path()));
    assert!(ed.workspaces.tree.open && !ed.workspaces.tree.focused);

    // A deleted file cannot be opened; it says so.
    run(&mut ed, " t");
    select(&mut ed, "old.txt");
    run(&mut ed, "\n");
    assert!(status_text(&ed).contains("deleted on this branch"));
    assert_eq!(ed.doc().path(), Some(f.repo.join("a.txt").as_path()));
    assert!(ed.workspaces.tree.has_focus());
}

#[test]
fn d_shows_a_files_diff_against_the_base() {
    let f = Fixture::feature();
    let mut ed = f.editor();
    open_tree(&mut ed);
    let diff_texts = |ed: &mut Editor| -> Vec<String> {
        poll_until(ed, "the diff", |ed| {
            ed.workspaces
                .tree
                .diff
                .as_ref()
                .is_some_and(|d| d.state != DiffState::Loading)
        });
        let d = ed.workspaces.tree.diff.as_ref().unwrap();
        match &d.state {
            DiffState::Ready { file, .. } => file
                .hunks
                .iter()
                .flat_map(|h| &h.lines)
                .map(|l| format!("{:?} {}", l.kind, l.text))
                .collect(),
            other => panic!("{other:?}"),
        }
    };

    // A file edited in a commit on the branch.
    select(&mut ed, "src");
    run(&mut ed, "l");
    settle(&mut ed);
    select(&mut ed, "src/lib.rs");
    run(&mut ed, "d");
    assert!(ed.workspaces.tree.diff_shown());
    assert_eq!(
        diff_texts(&mut ed),
        ["Context pub fn f() {}", "Added pub fn g() {}"]
    );
    let title = ed.workspaces.tree.diff.as_ref().unwrap().title.clone();
    assert!(
        title.starts_with("src/lib.rs  modified vs main (merge-base "),
        "{title}"
    );
    // q goes back to the tree, keys and all.
    run(&mut ed, "q");
    assert!(ed.workspaces.tree.diff.is_none() && ed.workspaces.tree.has_focus());

    // An uncommitted edit, a deletion and an untracked file.
    select(&mut ed, "a.txt");
    run(&mut ed, "d");
    assert_eq!(
        diff_texts(&mut ed),
        ["Context one", "Removed two", "Added TWO", "Context three"]
    );
    // s switches to side by side; ret opens the file at the line.
    run(&mut ed, "s");
    assert!(ed.workspaces.tree.diff.as_ref().unwrap().side_by_side);
    // The hunk header, one, then two beside TWO: the file's line 2.
    run(&mut ed, "jj\n");
    assert!(ed.workspaces.tree.diff.is_none() && !ed.workspaces.tree.focused);
    assert_eq!(ed.doc().path(), Some(f.repo.join("a.txt").as_path()));
    let cursor = ed.view().selection.primary().head;
    assert_eq!(ed.doc().text().char_to_line(cursor), 1);

    run(&mut ed, " t");
    select(&mut ed, "old.txt");
    run(&mut ed, "d");
    assert_eq!(diff_texts(&mut ed), ["Removed old"]);
    run(&mut ed, "\x1b");
    select(&mut ed, "notes.md");
    run(&mut ed, "d");
    assert_eq!(diff_texts(&mut ed), ["Added untracked"]);
    run(&mut ed, "\x1b");

    // An unchanged file has nothing to show.
    select(&mut ed, "b.txt");
    run(&mut ed, "d");
    assert!(ed.workspaces.tree.diff.is_none());
    assert!(status_text(&ed).contains("b.txt is the same as main"));
}

#[test]
fn on_main_the_tree_compares_with_head() {
    let f = Fixture::on_main();
    let mut ed = f.editor();
    open_tree(&mut ed);
    assert_eq!(tree::header(&ed)[1], "no uncommitted changes (on main)");
    run(&mut ed, "c");
    assert_eq!(shown(&ed), ["(no changes vs HEAD)"]);
    f.write("a.txt", "changed\n");
    run(&mut ed, "r");
    settle(&mut ed);
    assert_eq!(tree::header(&ed)[1], "1 uncommitted vs HEAD (on main)");
    assert_eq!(shown(&ed), ["M a.txt"]);
}

#[test]
fn outside_a_repository_the_tree_lists_files_without_marks() {
    let f = Fixture::bare_dir();
    f.write("kept.txt", "x\n");
    f.write("sub/inner.txt", "y\n");
    f.write(".gitignore", "*.log\n");
    f.write("noise.log", "z\n");
    let mut ed = f.editor();
    open_tree(&mut ed);
    // .gitignore applies even outside a repository, as in the finder.
    assert_eq!(shown(&ed), ["> sub/", "  kept.txt"]);
    assert_eq!(
        tree::header(&ed)[1],
        "no change marks: not a Git repository"
    );
    run(&mut ed, "c");
    assert_eq!(shown(&ed), ["(not a Git repository)"]);
    run(&mut ed, "d");
    assert!(ed.workspaces.tree.diff.is_none());
    run(&mut ed, "c");
    select(&mut ed, "kept.txt");
    run(&mut ed, "d");
    assert!(status_text(&ed).contains("no diff: not a Git repository"));
}

#[test]
fn clicks_select_open_and_fold_and_the_wheel_scrolls() {
    let f = Fixture::feature();
    for i in 0..40 {
        f.write(&format!("many/f{i:02}.txt"), "x\n");
    }
    let mut ed = f.editor();
    open_tree(&mut ed);
    run(&mut ed, "\x1b");
    // Views end at column 80 and start at row 1; the tree's list starts
    // under its two header rows, at row 3.
    let (x, list) = (85, 1 + tree::HEADER_ROWS);
    assert_eq!(shown(&ed)[1], "> many/ (40)");
    // A click on a directory focuses the tree and unfolds it.
    ed.handle_mouse(MouseKind::Down, x, list + 1);
    assert!(ed.workspaces.tree.has_focus());
    settle(&mut ed);
    assert_eq!(shown(&ed)[1], "v many/ (40)");
    assert_eq!(selected(&ed).as_deref(), Some(Path::new("many")));
    // The wheel scrolls the list without moving the selection.
    ed.handle_mouse(MouseKind::ScrollDown, x, list + 5);
    assert_eq!(ed.workspaces.tree.top, 3);
    // A click after scrolling picks the row drawn there: list row 0 is
    // now the fourth row, many/f01.txt.
    ed.handle_mouse(MouseKind::Down, x, list);
    assert_eq!(selected(&ed).as_deref(), Some(Path::new("many/f01.txt")));
    assert_eq!(ed.doc().path(), Some(f.repo.join("many/f01.txt").as_path()));
    run(&mut ed, " t");
    select(&mut ed, "many");
    ed.handle_mouse(MouseKind::ScrollUp, x, list + 5);
    assert_eq!(ed.workspaces.tree.top, 0);
    // A click on a file opens it and gives the keys back.
    ed.handle_mouse(MouseKind::Down, x, list + 2);
    assert_eq!(ed.doc().path(), Some(f.repo.join("many/f00.txt").as_path()));
    assert!(!ed.workspaces.tree.focused);
    // A click in the views leaves the tree to them.
    ed.handle_mouse(MouseKind::Down, x, list);
    assert!(ed.workspaces.tree.has_focus());
    ed.handle_mouse(MouseKind::Down, 10, 5);
    assert!(!ed.workspaces.tree.focused);

    // A half-typed `r` in the buffer is dropped by a click on the tree:
    // the next key moves the tree, it does not replace a character.
    let before = ed.doc().text().to_string();
    run(&mut ed, "r");
    ed.handle_mouse(MouseKind::Down, x, 1);
    let at = selected(&ed);
    run(&mut ed, "j");
    assert_eq!(ed.doc().text().to_string(), before);
    assert_ne!(selected(&ed), at);

    // Opened behind a screen that covers the views, the file is there and
    // the status line says so.
    run(&mut ed, "\x1b:git\n");
    assert!(ed.git.screen_open());
    // The Git screen has the keys; a click gives them to the tree.
    ed.handle_mouse(MouseKind::Down, x, 1);
    assert!(ed.workspaces.tree.has_focus());
    settle(&mut ed);
    select(&mut ed, "a.txt");
    run(&mut ed, "\n");
    assert_eq!(ed.doc().path(), Some(f.repo.join("a.txt").as_path()));
    assert!(
        status_text(&ed).contains("behind the Git screen"),
        "{}",
        status_text(&ed)
    );
    settle(&mut ed);
}

#[test]
fn a_too_narrow_tree_takes_no_keys() {
    let f = Fixture::on_main();
    let mut ed = f.editor();
    run(&mut ed, ":e a.txt\n");
    open_tree(&mut ed);
    // The front end found no room for it.
    ed.workspaces.tree.too_narrow = true;
    run(&mut ed, "i");
    assert_eq!(ed.mode, Mode::Insert);
    run(&mut ed, "\x1b");
    ed.handle_mouse(MouseKind::Down, 85, 4);
    assert!(!ed.workspaces.tree.has_focus());
}

#[test]
fn odd_names_and_a_directory_turned_into_a_file_keep_their_own_rows() {
    let f = Fixture::on_main();
    f.write("thing/old.txt", "old\n");
    f.write("tools/keep.sh", "x\n");
    f.write(".ignore", "gen/\n");
    f.git(&["add", "-A"]);
    f.git(&["commit", "-q", "-m", "thing"]);
    f.git(&["switch", "-q", "-c", "feature"]);
    // The directory `thing` becomes a file.
    f.git(&["rm", "-q", "-r", "thing"]);
    f.write("thing", "now a file\n");
    f.git(&["add", "thing"]);
    f.git(&["commit", "-q", "-m", "flatten"]);
    // Untracked output that only `.ignore` hides.
    f.write("gen/out.txt", "generated\n");
    // A name that is not UTF-8, where the file system allows one (macOS
    // refuses it with "Illegal byte sequence"; that part is then skipped).
    #[cfg(unix)]
    let odd = {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
        std::fs::write(f.repo.join(name), "x\n")
            .ok()
            .map(|()| PathBuf::from(name))
    };
    let mut ed = f.editor();
    open_tree(&mut ed);
    let rows = shown(&ed);
    assert!(rows.contains(&"> thing/ (1)".to_string()), "{rows:#?}");
    assert!(rows.contains(&"A thing".to_string()), "{rows:#?}");
    assert!(!rows.iter().any(|r| r.contains("gen")), "{rows:#?}");
    // The file named `thing` can be selected, diffed and opened.
    run(&mut ed, "g");
    for _ in 0..20 {
        let r = tree::rows(&ed);
        let i = tree::selected_index(&ed, &r).unwrap();
        if r[i].name == "thing" && matches!(r[i].kind, RowKind::File { .. }) {
            break;
        }
        run(&mut ed, "j");
    }
    run(&mut ed, "d");
    poll_until(&mut ed, "the diff", |ed| {
        ed.workspaces
            .tree
            .diff
            .as_ref()
            .is_some_and(|d| d.state != DiffState::Loading)
    });
    assert!(matches!(
        ed.workspaces.tree.diff.as_ref().unwrap().state,
        DiffState::Ready { .. }
    ));
    run(&mut ed, "q\n");
    assert_eq!(ed.doc().path(), Some(f.repo.join("thing").as_path()));
    #[cfg(unix)]
    if let Some(odd) = odd {
        // The untracked file with a non-UTF-8 name is marked and opens
        // under its own name.
        run(&mut ed, " t");
        run(&mut ed, "g");
        for _ in 0..20 {
            if selected(&ed).as_deref() == Some(odd.as_path()) {
                break;
            }
            run(&mut ed, "j");
        }
        let r = tree::rows(&ed);
        let i = tree::selected_index(&ed, &r).unwrap();
        assert_eq!(r[i].path, odd);
        assert!(matches!(
            r[i].kind,
            RowKind::File {
                change: Some(tree::PathChangeKind::Untracked)
            }
        ));
        run(&mut ed, "\n");
        assert_eq!(ed.doc().path(), Some(f.repo.join(&odd).as_path()));
    }
}

#[test]
fn the_tree_follows_the_working_directory_to_another_repository() {
    let f = Fixture::feature();
    let g = Fixture::on_main();
    let mut ed = f.editor();
    open_tree(&mut ed);
    select(&mut ed, "src");
    run(&mut ed, "l");
    settle(&mut ed);
    // Move on while the first repository is being read again: its results
    // arrive after the switch and are dropped.
    run(&mut ed, "r");
    ed.cwd = g.repo.clone();
    ed.tick(Instant::now());
    settle(&mut ed);
    assert!(!shown(&ed).iter().any(|r| r.contains("notes.md")));
    assert_eq!(ed.workspaces.tree.root.as_deref(), Some(g.repo.as_path()));
    assert_eq!(tree::header(&ed)[0], "files: main");
    // Everything starts folded again.
    assert_eq!(shown(&ed)[..2], ["> docs/", "> src/"]);
}
