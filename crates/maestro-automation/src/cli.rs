//! Entry points for the `maestro` binary's automation flags, so `main.rs`
//! only parses arguments and calls these.

use std::path::Path;
use std::process::ExitCode;

use maestro_editor::Editor;

/// `maestro --headless`: serves requests on stdin/stdout until end of input
/// or `editor.quit`. Diagnostics go to stderr; stdout carries only protocol
/// messages.
pub fn headless_main(mut editor: Editor, startup_errors: &[String]) -> ExitCode {
    for e in startup_errors {
        eprintln!("maestro: {e}");
    }
    eprintln!(
        "maestro: headless automation server, protocol {}; send newline-delimited JSON-RPC on stdin",
        maestro_protocol::PROTOCOL_VERSION
    );
    match crate::headless::serve_stdio(&mut editor) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("maestro: headless I/O error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A running socket listener; dropping it removes the socket file.
#[cfg(unix)]
pub type ListenerGuard = crate::socket::Listener;
/// Placeholder on platforms without the Unix socket transport.
#[cfg(not(unix))]
pub struct ListenerGuard;

/// `maestro --listen <path|auto>`: starts serving automation clients on a
/// Unix socket. `auto` means `$XDG_RUNTIME_DIR/maestro/<pid>.sock`. The
/// socket path is shown in the status line.
#[cfg(unix)]
pub fn start_listener(editor: &mut Editor, spec: &str) -> Result<ListenerGuard, String> {
    let path = if spec == "auto" {
        crate::socket::default_socket_path()?
    } else {
        editor.resolve_path(Path::new(spec))
    };
    let guard = crate::socket::listen(editor, &path)
        .map_err(|e| format!("cannot listen on {}: {e}", path.display()))?;
    editor.info(format!("automation socket: {}", path.display()));
    Ok(guard)
}

// TODO(windows): serve the protocol on a named pipe restricted to the
// current user. Until then --listen and --remote are refused on Windows;
// --headless works everywhere.
#[cfg(not(unix))]
pub fn start_listener(_editor: &mut Editor, _spec: &str) -> Result<ListenerGuard, String> {
    Err(
        "--listen needs Unix domain sockets; this platform is not supported yet (use --headless)"
            .into(),
    )
}

/// `maestro --remote <socket> <json>`: sends one request to a listening
/// editor and prints the response. Exits 0 on a result, 1 on an error
/// response, 2 when the editor could not be reached.
#[cfg(unix)]
pub fn remote_main(socket: &Path, request: &str) -> ExitCode {
    match crate::socket::remote(socket, request) {
        Ok(reply) => {
            if let Some(line) = reply.line {
                println!("{line}");
            }
            if reply.is_error {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("maestro: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(unix))]
pub fn remote_main(_socket: &Path, _request: &str) -> ExitCode {
    eprintln!("maestro: --remote needs Unix domain sockets; this platform is not supported yet");
    ExitCode::from(2)
}
