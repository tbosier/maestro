//! Tests for the proposal review drawing (`proposals.rs`).

use maestro_config::Config;
use maestro_core::{Change, ChangeSet, Document, DocumentVersion};
use maestro_editor::proposals::{self, Actor, NewProposal};
use maestro_editor::{Editor, KeyEvent, Rect};

use crate::render::CursorShape;
use crate::surface::Surface;

fn row(s: &Surface, y: u16) -> String {
    s.row(y).iter().map(|c| c.symbol.as_str()).collect()
}

fn screen(s: &Surface) -> Vec<String> {
    (0..s.height).map(|y| row(s, y)).collect()
}

fn editor_with_proposal(w: u16, h: u16) -> Editor {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, w, h.saturating_sub(2)));
    let mut doc = Document::from_text("hello world\n");
    doc.set_path(std::env::temp_dir().join("maestro-tui-proposal.txt"));
    let id = ed.add_document(doc);
    ed.switch_to(id);
    let changes = ChangeSet::from_changes(12, [Change::replace(6, 11, "there")]).unwrap();
    proposals::submit(
        &mut ed,
        NewProposal {
            doc: id,
            base_version: DocumentVersion(0),
            changes,
            edit_count: 1,
            actor: Actor {
                name: "agent:draw".into(),
                agent_id: Some("draw-1".into()),
            },
            description: "greet someone closer".into(),
            verify: Some(("sh".into(), vec!["-c".into(), "grep -q there f".into()])),
        },
    );
    ed
}

#[test]
fn pending_proposals_show_in_the_status_line_and_review() {
    let mut ed = editor_with_proposal(100, 30);
    let mut s = Surface::new(100, 30, ed.theme.ui.text);
    crate::render::render(&ed, &mut s);
    let text = screen(&s).join("\n");
    assert!(text.contains("1 proposal waiting: space e"), "{text}");

    ed.handle_key(KeyEvent::char(' '));
    ed.handle_key(KeyEvent::char('e'));
    let mut s = Surface::new(100, 30, ed.theme.ui.text);
    crate::render::render(&ed, &mut s);
    let rows = screen(&s);
    let text = rows.join("\n");
    assert!(
        rows[1].contains("edit proposals: 1 pending"),
        "{:?}",
        rows[1]
    );
    assert!(text.contains("#1 pending agent:draw"), "{text}");
    assert!(text.contains("agent:draw (agent draw-1)"), "{text}");
    assert!(text.contains("greet someone closer"), "{text}");
    assert!(text.contains("base version 0, document at 0"), "{text}");
    assert!(text.contains("-hello world"), "{text}");
    assert!(text.contains("+hello there"), "{text}");
    assert!(
        text.contains("verify after saving: sh -c 'grep -q there f'"),
        "{text}"
    );
    assert!(rows[28].contains("a approve"), "{:?}", rows[28]);
    assert!(rows[28].contains("x reject"), "{:?}", rows[28]);

    // The approval dialog spells out the command.
    ed.handle_key(KeyEvent::char('a'));
    let mut s = Surface::new(100, 30, ed.theme.ui.text);
    crate::render::render(&ed, &mut s);
    let text = screen(&s).join("\n");
    assert!(text.contains("approve proposal 1"), "{text}");
    assert!(text.contains("sh -c 'grep -q there f'"), "{text}");
    assert!(text.contains("y approve"), "{text}");

    // Rejecting asks for a reason in a box with a cursor.
    ed.handle_key(KeyEvent::char('n'));
    ed.handle_key(KeyEvent::char('x'));
    ed.paste_text("not now");
    let mut s = Surface::new(100, 30, ed.theme.ui.text);
    let cursor = crate::render::render(&ed, &mut s);
    let text = screen(&s).join("\n");
    assert!(text.contains("reason for rejecting proposal 1"), "{text}");
    assert!(text.contains("not now"), "{text}");
    assert!(matches!(cursor, CursorShape::Bar(..)));
}

#[test]
fn review_never_panics_at_small_sizes() {
    for (w, h) in (1..=60u16).flat_map(|w| (1..=14u16).map(move |h| (w, h))) {
        let mut ed = editor_with_proposal(w, h);
        ed.handle_key(KeyEvent::char(' '));
        ed.handle_key(KeyEvent::char('e'));
        ed.handle_key(KeyEvent::char('a'));
        let mut s = Surface::new(w, h, ed.theme.ui.text);
        crate::render::render(&ed, &mut s);
    }
}

#[test]
fn text_past_the_right_edge_is_marked_and_reachable() {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 100, 28));
    let id = ed.add_document(Document::from_text("x\n"));
    ed.switch_to(id);
    let long = format!("y{}touch marker", " ".repeat(300));
    let changes = ChangeSet::from_changes(2, [Change::replace(0, 1, long.as_str())]).unwrap();
    proposals::submit(
        &mut ed,
        NewProposal {
            doc: id,
            base_version: DocumentVersion(0),
            changes,
            edit_count: 1,
            actor: Actor {
                name: "agent:wide".into(),
                agent_id: None,
            },
            description: "looks small".into(),
            verify: None,
        },
    );
    ed.handle_key(KeyEvent::char(' '));
    ed.handle_key(KeyEvent::char('e'));
    let mut s = Surface::new(100, 30, ed.theme.ui.text);
    crate::render::render(&ed, &mut s);
    let rows = screen(&s);
    let added = rows
        .iter()
        .find(|r| r.contains("+y"))
        .expect("the added line is drawn");
    assert!(added.trim_end().ends_with('>'), "{added:?}");
    assert!(!rows.join("\n").contains("touch marker"));
    // The dialog warns before approval.
    ed.handle_key(KeyEvent::char('a'));
    let mut s = Surface::new(100, 30, ed.theme.ui.text);
    crate::render::render(&ed, &mut s);
    let text = screen(&s).join("\n");
    assert!(text.contains("wider than the review shows"), "{text}");
    ed.handle_key(KeyEvent::char('n'));
    // Scrolling right brings the rest into view, marked as scrolled.
    for _ in 0..12 {
        ed.handle_key(KeyEvent::char('l'));
        let mut s = Surface::new(100, 30, ed.theme.ui.text);
        crate::render::render(&ed, &mut s);
        if let Some(row) = screen(&s).into_iter().find(|r| r.contains("touch marker")) {
            assert!(row.contains('<'), "{row:?}");
            return;
        }
    }
    panic!("scrolling right never showed the end of the line");
}

#[test]
fn a_rebased_intent_edit_says_so_in_the_summary_and_above_the_diff() {
    use maestro_editor::KeyCode;
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 120, 28));
    let mut doc = Document::from_text("pub fn a() -> Result<(), E> {\n    Ok(())\n}\n");
    doc.set_path(std::env::temp_dir().join("maestro-tui-intent.rs"));
    let id = ed.add_document(doc);
    ed.switch_to(id);
    let pipe = "select-nodes pub-fn | insert-above #[must_use]";
    let run = maestro_editor::structural::run_intent(&ed, id, pipe, Some(1)).unwrap();
    proposals::submit_intent(
        &mut ed,
        NewProposal {
            doc: id,
            base_version: DocumentVersion(0),
            changes: run.changes.clone(),
            edit_count: run.edit_count,
            actor: Actor {
                name: "agent:intent".into(),
                agent_id: None,
            },
            description: "mark it".into(),
            verify: None,
        },
        proposals::Intent::new(pipe.into(), Some(1), &run),
    );
    // The user types a line at the top before reviewing.
    ed.handle_key(KeyEvent::char('i'));
    for c in "// note".chars() {
        ed.handle_key(KeyEvent::char(c));
    }
    ed.handle_key(KeyEvent::plain(KeyCode::Enter));
    ed.handle_key(KeyEvent::plain(KeyCode::Esc));
    let v = ed.doc().version().0;
    ed.handle_key(KeyEvent::char(' '));
    ed.handle_key(KeyEvent::char('e'));
    let mut s = Surface::new(120, 30, ed.theme.ui.text);
    crate::render::render(&ed, &mut s);
    let text = screen(&s).join("\n");
    assert!(
        text.contains(&format!("base version 0, document at {v}")),
        "{text}"
    );
    assert!(
        text.contains(&format!("rebased onto version {v}")),
        "{text}"
    );
    assert!(!text.contains("stale: approving will refuse it"), "{text}");
    assert!(text.contains(&format!("intent edit: {pipe}")), "{text}");
    assert!(
        text.contains(&format!(
            "rebased onto version {v}: changed since version 0, which agent:intent looked at"
        )),
        "{text}"
    );
    assert!(text.contains("+#[must_use]"), "{text}");
}
