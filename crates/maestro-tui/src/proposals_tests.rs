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
