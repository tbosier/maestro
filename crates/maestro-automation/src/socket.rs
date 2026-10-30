//! Live transport: a Unix socket served while the interactive editor runs.
//!
//! Threads never touch the editor. Each connection gets a reader thread
//! (started with [`Editor::spawn_background`], so the front end polls for
//! jobs quickly while a client is connected) that turns every line into a
//! job. The job runs on the editor's thread inside
//! [`Editor::poll_background`], handles the request and writes the
//! response to the socket directly, so responses and notifications go out
//! in order and a response is written before an `editor.quit` takes effect.
//! Writes time out after [`WRITE_TIMEOUT`] so a client that stops reading
//! cannot hang the editor; such a client is disconnected.
//!
//! The socket file is created with mode 0600 inside a private (0700)
//! directory and then renamed into place, so it never exists with looser
//! permissions. It is removed when the [`Listener`] is dropped.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use maestro_editor::Editor;
use maestro_editor::editor::JobSender;
use maestro_protocol::{self as p, ServerMessage};

use crate::server::{Server, Session};

/// How long a write to a client may block the editor before the client is
/// dropped.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How often subscribed connections check for changes made outside the API
/// (typing, commands).
pub const EVENT_POLL: Duration = Duration::from_millis(100);

struct Conn {
    session: Session,
    out: UnixStream,
    subscribed: Arc<AtomicBool>,
}

struct Hub {
    server: Server,
    conns: HashMap<u64, Conn>,
}

type SharedHub = Arc<Mutex<Hub>>;

fn lock(hub: &SharedHub) -> MutexGuard<'_, Hub> {
    hub.lock().unwrap_or_else(|e| e.into_inner())
}

/// A listening socket. Dropping it removes the socket file, if the file at
/// that path is still the one this listener created.
pub struct Listener {
    path: PathBuf,
    identity: (u64, u64),
}

impl Listener {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && (m.dev(), m.ino()) == self.identity
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// `$XDG_RUNTIME_DIR/maestro/<pid>.sock`. There is no fallback to `/tmp`,
/// which other users can write to.
pub fn default_socket_path() -> Result<PathBuf, String> {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => Ok(PathBuf::from(d)
            .join("maestro")
            .join(format!("{}.sock", std::process::id()))),
        _ => Err("XDG_RUNTIME_DIR is not set; pass an explicit socket path to --listen".into()),
    }
}

/// Binds `path` and serves automation clients until the process exits.
pub fn listen(editor: &Editor, path: &Path) -> io::Result<Listener> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if !parent.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&parent)?;
    }
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another process is listening on {}", path.display()),
            ));
        }
        // A socket nobody listens on: left behind by a crash.
        std::fs::remove_file(path)?;
    }

    // Bind inside a private directory, tighten the mode, then rename into
    // place, so the socket is never reachable with the umask's permissions.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let private = parent.join(format!(".m{:x}.{nonce:x}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&private)?;
    let staged = private.join("s");
    let bound = (|| {
        let l = UnixListener::bind(&staged)?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&staged, path)?;
        Ok::<_, io::Error>(l)
    })();
    let _ = std::fs::remove_file(&staged);
    let _ = std::fs::remove_dir(&private);
    let listener = bound?;
    let m = std::fs::symlink_metadata(path)?;
    let guard = Listener {
        path: path.to_path_buf(),
        identity: (m.dev(), m.ino()),
    };

    let hub: SharedHub = Arc::new(Mutex::new(Hub {
        server: Server::new(editor),
        conns: HashMap::new(),
    }));
    let idle = editor.idle_job_sender();
    std::thread::Builder::new()
        .name("maestro-automation-accept".into())
        .spawn(move || accept_loop(listener, idle, hub))?;
    Ok(guard)
}

fn accept_loop(listener: UnixListener, idle: JobSender, hub: SharedHub) {
    for (n, stream) in listener.incoming().enumerate() {
        let Ok(stream) = stream else { continue };
        let hub = hub.clone();
        let id = n as u64;
        if !idle.send(move |ed| start_connection(ed, hub, id, stream)) {
            return;
        }
    }
}

/// Runs on the editor thread: registers the connection and starts its
/// reader with a fast-poll [`JobSender`].
fn start_connection(ed: &mut Editor, hub: SharedHub, id: u64, stream: UnixStream) {
    let Ok(out) = stream.try_clone() else { return };
    if out.set_write_timeout(Some(WRITE_TIMEOUT)).is_err() {
        return;
    }
    let subscribed = Arc::new(AtomicBool::new(false));
    lock(&hub).conns.insert(
        id,
        Conn {
            session: Session::default(),
            out,
            subscribed: subscribed.clone(),
        },
    );
    let hub2 = hub.clone();
    let started = ed.spawn_background("automation-client", move |sender| {
        serve_connection(stream, sender, hub2, id, subscribed)
    });
    if started.is_err() {
        lock(&hub).conns.remove(&id);
    }
}

fn serve_connection(
    stream: UnixStream,
    sender: JobSender,
    hub: SharedHub,
    id: u64,
    subscribed: Arc<AtomicBool>,
) {
    let closed = Arc::new(AtomicBool::new(false));
    {
        // Checks for changes made outside the API while subscribed.
        let (sender, hub, closed) = (sender.clone(), hub.clone(), closed.clone());
        let _ = std::thread::Builder::new()
            .name("maestro-automation-events".into())
            .spawn(move || {
                while !closed.load(Ordering::SeqCst) {
                    std::thread::sleep(EVENT_POLL);
                    if subscribed.load(Ordering::SeqCst) {
                        let hub = hub.clone();
                        if !sender.send(move |ed| pump(ed, &mut lock(&hub))) {
                            return;
                        }
                    }
                }
            });
    }
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = match String::from_utf8(std::mem::take(&mut buf)) {
            Ok(l) if l.trim().is_empty() => continue,
            Ok(l) => Ok(l),
            Err(_) => Err(()),
        };
        let hub = hub.clone();
        let job = move |ed: &mut Editor| handle_on_editor(ed, &hub, id, line);
        if !sender.send(job) {
            break;
        }
    }
    closed.store(true, Ordering::SeqCst);
    let _ = sender.send(move |_| {
        lock(&hub).conns.remove(&id);
    });
}

fn write_line(out: &mut UnixStream, line: &str) -> io::Result<()> {
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

fn handle_on_editor(ed: &mut Editor, hub: &SharedHub, id: u64, line: Result<String, ()>) {
    let mut guard = lock(hub);
    let Hub { server, conns } = &mut *guard;
    let Some(conn) = conns.get_mut(&id) else {
        return;
    };
    let response = match line {
        Ok(l) => server.handle_line(ed, &mut conn.session, l.trim_end()),
        Err(()) => Some(Server::invalid_utf8_response()),
    };
    conn.subscribed
        .store(conn.session.is_subscribed(), Ordering::SeqCst);
    if let Some(r) = response
        && write_line(&mut conn.out, &r).is_err()
    {
        drop_conn(conns, id);
    }
    pump(ed, &mut guard);
}

fn drop_conn(conns: &mut HashMap<u64, Conn>, id: u64) {
    if let Some(c) = conns.remove(&id) {
        let _ = c.out.shutdown(std::net::Shutdown::Both);
    }
}

/// Sends pending notifications to every subscribed connection.
fn pump(ed: &mut Editor, hub: &mut Hub) {
    let events = hub.server.collect_events(ed);
    if events.is_empty() {
        return;
    }
    let lines: Vec<String> = events
        .iter()
        .filter_map(|n| serde_json::to_string(n).ok())
        .collect();
    let mut dead = Vec::new();
    for (id, c) in hub.conns.iter_mut() {
        if !c.session.is_subscribed() {
            continue;
        }
        if lines.iter().any(|l| write_line(&mut c.out, l).is_err()) {
            dead.push(*id);
        }
    }
    for id in dead {
        drop_conn(&mut hub.conns, id);
    }
}

/// How long `remote` waits for each response.
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(30);
const REMOTE_INIT_ID: &str = "maestro-remote-init";

/// The outcome of [`remote`].
#[derive(Debug)]
pub struct RemoteReply {
    /// The response line, or `None` when the request had no id.
    pub line: Option<String>,
    /// True when the response is an error.
    pub is_error: bool,
}

/// Connects to a listening editor, initializes (unless `request` is itself
/// an `initialize`), sends `request` and returns its response line.
/// Notifications that arrive meanwhile are skipped.
pub fn remote(path: &Path, request: &str) -> io::Result<RemoteReply> {
    let req: serde_json::Value = serde_json::from_str(request).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("request is not valid JSON: {e}"),
        )
    })?;
    let id = req.get("id").cloned().filter(|v| !v.is_null());
    let is_initialize = req.get("method").and_then(|m| m.as_str()) == Some(p::methods::INITIALIZE);
    let stream = UnixStream::connect(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    stream.set_read_timeout(Some(REMOTE_TIMEOUT))?;
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    if !is_initialize {
        let init = p::Request::new(
            REMOTE_INIT_ID,
            p::methods::INITIALIZE,
            p::InitializeParams {
                protocol_version: p::PROTOCOL_VERSION.to_string(),
                client_name: Some("maestro --remote".into()),
            },
        );
        write_line(
            &mut out,
            &serde_json::to_string(&init).map_err(io::Error::other)?,
        )?;
        let init_id = serde_json::Value::String(REMOTE_INIT_ID.into());
        let (line, is_error) = read_response(&mut reader, &init_id)?;
        if is_error {
            return Ok(RemoteReply {
                line: Some(line),
                is_error,
            });
        }
    }
    let compact = serde_json::to_string(&req).map_err(io::Error::other)?;
    write_line(&mut out, &compact)?;
    let Some(id) = id else {
        return Ok(RemoteReply {
            line: None,
            is_error: false,
        });
    };
    let (line, is_error) = read_response(&mut reader, &id)?;
    Ok(RemoteReply {
        line: Some(line),
        is_error,
    })
}

fn read_response(
    reader: &mut BufReader<UnixStream>,
    id: &serde_json::Value,
) -> io::Result<(String, bool)> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the editor closed the connection before responding",
            ));
        }
        let trimmed = line.trim_end();
        let Ok(ServerMessage::Response(r)) = ServerMessage::parse(trimmed) else {
            continue;
        };
        let rid = serde_json::to_value(&r.id).unwrap_or_default();
        if &rid == id || r.id.is_none() {
            return Ok((trimmed.to_string(), r.error().is_some()));
        }
    }
}
