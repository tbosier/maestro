//! Starts a real ACP agent through `AcpClient` in a fresh temporary git
//! repository, sends one small prompt and prints every event it receives.
//!
//! ```text
//! cargo run -p maestro-agent --example acp_smoke -- gemini --acp
//! cargo run -p maestro-agent --example acp_smoke -- opencode acp
//! ```
//!
//! The prompt defaults to asking for the single word "ok" without tools;
//! set `ACP_SMOKE_PROMPT` to change it. A permission request is answered as
//! cancelled. This uses the agent's own account and network access.

use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use maestro_agent::{AcpClient, AcpEvent, ProviderKind, ProviderSpec};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(program) = args.next() else {
        eprintln!("usage: acp_smoke <program> [args...]");
        std::process::exit(2);
    };
    let spec = ProviderSpec {
        name: program.clone(),
        kind: ProviderKind::Acp,
        program,
        args: args.collect(),
        env: Vec::new(),
    };
    let prompt = std::env::var("ACP_SMOKE_PROMPT")
        .unwrap_or_else(|_| "Reply with exactly the word ok. Do not use any tools.".to_string());

    let dir = std::env::temp_dir().join(format!("maestro-acp-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the temporary repository");
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status()
        .expect("run git init");
    assert!(git.success(), "git init failed");
    println!("repository: {}", dir.display());

    let (tx, rx) = mpsc::channel();
    let start = Instant::now();
    let client = match AcpClient::start(&spec, &dir, move |e| drop(tx.send(e))) {
        Ok(c) => c,
        Err(e) => {
            println!("start failed: {e}");
            std::process::exit(1);
        }
    };
    let print = |e: &AcpEvent| {
        let text = format!("{e:?}");
        let text: String = text.chars().take(400).collect();
        println!("[{:>6.2}s] {text}", start.elapsed().as_secs_f64());
    };

    let wait = |limit: Duration, done: &dyn Fn(&AcpEvent) -> bool| -> Option<AcpEvent> {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(e) = rx.recv_timeout(left) else {
                println!("timed out after {limit:?}");
                return None;
            };
            print(&e);
            if let AcpEvent::PermissionRequested(req) = &e {
                println!("answering the permission request as cancelled");
                let _ = client.answer_permission(req.request_id.clone(), None);
            }
            if done(&e) || matches!(e, AcpEvent::Exited(_)) {
                return Some(e);
            }
        }
    };

    let started = wait(Duration::from_secs(90), &|e| {
        matches!(e, AcpEvent::SessionStarted { .. })
    });
    if matches!(started, Some(AcpEvent::SessionStarted { .. })) {
        println!("prompt: {prompt}");
        match client.prompt(&prompt) {
            Ok(()) => {
                wait(Duration::from_secs(180), &|e| {
                    matches!(e, AcpEvent::TurnEnded { .. })
                });
            }
            Err(e) => println!("prompt failed: {e}"),
        }
    }
    client.shutdown();
    wait(Duration::from_secs(5), &|_| false);
    let _ = std::fs::remove_dir_all(&dir);
}
