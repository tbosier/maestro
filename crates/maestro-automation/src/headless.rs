//! Headless transport: newline-delimited JSON on a reader and a writer
//! (stdin and stdout for `maestro --headless`). One request per line, one
//! response per line, in order. Notifications follow the response of the
//! request that caused them, once the client has called `events.subscribe`.
//!
//! The loop ends on end of input or once the editor wants to quit
//! (`editor.quit`, or a `:q` through `editor.execute`).

use std::io::{self, BufRead, Write};

use maestro_editor::Editor;

use crate::server::{Server, Session};

/// Serves requests from `input` until it ends or the editor quits.
pub fn serve<R: BufRead, W: Write>(
    editor: &mut Editor,
    mut input: R,
    mut out: W,
) -> io::Result<()> {
    let mut server = Server::new(editor);
    let mut session = Session::default();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if input.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        // Results of background work started by earlier requests land
        // before this one reads state.
        editor.poll_background();
        let response = match std::str::from_utf8(&buf) {
            Ok(line) if line.trim().is_empty() => continue,
            Ok(line) => server.handle_line(editor, &mut session, line.trim_end()),
            Err(_) => Some(Server::invalid_utf8_response()),
        };
        if let Some(r) = response {
            writeln!(out, "{r}")?;
        }
        let events = server.collect_events(editor);
        if session.is_subscribed() {
            for n in events {
                let line = serde_json::to_string(&n).map_err(io::Error::other)?;
                writeln!(out, "{line}")?;
            }
        }
        out.flush()?;
        if editor.should_quit {
            return Ok(());
        }
    }
}

/// Serves on the process's stdin and stdout.
pub fn serve_stdio(editor: &mut Editor) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve(editor, stdin.lock(), stdout.lock())
}
