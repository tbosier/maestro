//! Task contracts: what an agent task may change, how it is verified and
//! what the reviewer checks. See `docs/agent-model.md`.
//!
//! A contract is attached when a task is created, either typed in the task
//! input as `@` lines ([`parse_task`]) or read from a TOML file the user
//! names with `@contract <path>` ([`parse_toml`]). Nothing reads a contract
//! from the repository on its own.
//!
//! The contract is sent to the agent with the task, as a request. What
//! Maestro enforces is narrower and listed in the docs: file writes the
//! agent asks Maestro to do (ACP `fs/write_text_file`) outside the allowed
//! paths are refused, the review marks every changed file outside them,
//! integrating such files needs a key of its own, `t` runs the contract's
//! verification command, and integrate warns when that did not pass on
//! the current files.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Largest contract file read, in bytes.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;
/// Most patterns per list and most acceptance criteria.
pub const MAX_ITEMS: usize = 200;

/// What an agent task may change and how its work is checked.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Contract {
    /// Path patterns, relative to the repository root, the task may
    /// change. Empty means any path not forbidden.
    #[serde(default)]
    pub allowed: Vec<String>,
    /// Path patterns the task must not change, whatever `allowed` says.
    #[serde(default)]
    pub forbidden: Vec<String>,
    /// The verification command `t` runs, as a program and its arguments.
    /// Overrides `[agents] test-command` for this task.
    #[serde(default)]
    pub verify: Option<Vec<String>>,
    /// Plain text for the reviewer, one criterion per entry.
    #[serde(default)]
    pub acceptance: Vec<String>,
    /// The file the contract was read from, as the user named it; `None`
    /// when it was typed.
    #[serde(default)]
    pub source: Option<String>,
}

/// A contract file, as written by the user. Unknown keys are refused.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContractFile {
    #[serde(default)]
    allowed: Vec<String>,
    #[serde(default)]
    forbidden: Vec<String>,
    #[serde(default)]
    verify: Option<Words>,
    #[serde(default)]
    acceptance: Option<Words>,
}

/// A string or a list of strings.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Words {
    One(String),
    Many(Vec<String>),
}

impl Contract {
    /// True when nothing in it limits or checks anything.
    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
            && self.forbidden.is_empty()
            && self.verify.is_none()
            && self.acceptance.is_empty()
    }

    /// Why `path` (relative to the repository root) is outside the
    /// contract, or `None` when the task may change it.
    pub fn violation(&self, path: &Path) -> Option<String> {
        let shown = path.display();
        // Patterns are text. A name that is not valid UTF-8 could only be
        // matched through a lossy conversion that makes different names
        // equal, so with any pattern it counts as outside.
        if (!self.allowed.is_empty() || !self.forbidden.is_empty()) && path.to_str().is_none() {
            return Some(format!(
                "{shown} is not a valid UTF-8 path, which no pattern can name"
            ));
        }
        if let Some(p) = self.forbidden.iter().find(|p| matches_path(p, path)) {
            return Some(format!("{shown} matches the forbidden pattern {p}"));
        }
        if !self.allowed.is_empty() && !self.allowed.iter().any(|p| matches_path(p, path)) {
            return Some(format!("{shown} matches no allowed pattern"));
        }
        None
    }

    /// Checks the patterns and the command. Used for typed contracts,
    /// files and saved records alike.
    pub fn validate(&self) -> Result<(), String> {
        for (what, list) in [("allowed", &self.allowed), ("forbidden", &self.forbidden)] {
            if list.len() > MAX_ITEMS {
                return Err(format!("more than {MAX_ITEMS} {what} patterns"));
            }
            for p in list {
                check_pattern(p).map_err(|e| format!("{what} pattern {p:?}: {e}"))?;
            }
        }
        if self.acceptance.len() > MAX_ITEMS {
            return Err(format!("more than {MAX_ITEMS} acceptance criteria"));
        }
        if let Some(v) = &self.verify {
            match v.first() {
                None => return Err("the verification command is empty".into()),
                Some(p) if p.trim().is_empty() => {
                    return Err("the verification command has no program".into());
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// The verification command as shown to the user: every word quoted
    /// for a shell where needed, control and direction-changing characters
    /// escaped, so a contract file cannot hide part of what `t` runs.
    pub fn verify_shown(&self) -> Option<String> {
        let v = self.verify.as_ref()?;
        let mut words = v.iter();
        let program = crate::proposals::quote_program(words.next()?);
        Some(
            std::iter::once(program)
                .chain(words.map(|w| crate::proposals::shell_quote(w)))
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    /// One line for lists: `2 allowed, 1 forbidden, verify: ..., 3 criteria`.
    pub fn brief(&self) -> String {
        let mut parts = Vec::new();
        if self.allowed.is_empty() {
            parts.push("any path".to_string());
        } else {
            parts.push(format!("allowed {}", self.allowed.join(" ")));
        }
        if !self.forbidden.is_empty() {
            parts.push(format!("forbidden {}", self.forbidden.join(" ")));
        }
        if let Some(v) = self.verify_shown() {
            parts.push(format!("verify: {v}"));
        }
        if !self.acceptance.is_empty() {
            parts.push(format!("{} acceptance criteria", self.acceptance.len()));
        }
        parts.join("; ")
    }

    /// What the user sees before the agent starts and in dialogs.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(match &self.source {
            Some(s) => format!("Contract from {s}."),
            None => "Contract typed with the task.".to_string(),
        });
        out.push(if self.allowed.is_empty() {
            "Allowed paths: any not forbidden.".to_string()
        } else {
            format!("Allowed paths: {}", self.allowed.join(", "))
        });
        if !self.forbidden.is_empty() {
            out.push(format!("Forbidden paths: {}", self.forbidden.join(", ")));
        }
        match self.verify_shown() {
            Some(v) => out.push(format!(
                "Verification command (t runs it in the worktree, instead of [agents] test-command): {v}"
            )),
            None => out.push("Verification command: [agents] test-command, as usual.".into()),
        }
        for a in &self.acceptance {
            out.push(format!("Acceptance: {a}"));
        }
        out
    }

    /// The text sent to the agent with the task.
    pub fn agent_text(&self) -> String {
        let mut s = String::from(
            "Task contract from the user (Maestro refuses file writes it is asked to make outside \
             the allowed paths and shows the reviewer every file changed outside them):\n",
        );
        if self.allowed.is_empty() {
            s.push_str("- You may change any path that is not forbidden.\n");
        } else {
            s.push_str(&format!(
                "- Change only paths matching: {}\n",
                self.allowed.join(", ")
            ));
        }
        if !self.forbidden.is_empty() {
            s.push_str(&format!(
                "- Do not change paths matching: {}\n",
                self.forbidden.join(", ")
            ));
        }
        s.push_str(
            "- Patterns are relative to the repository root; * stays within one directory, \
             ** spans directories, and a pattern naming a directory covers what is in it.\n",
        );
        if let Some(v) = &self.verify {
            s.push_str(&format!("- Verification command: {}\n", v.join(" ")));
        }
        if !self.acceptance.is_empty() {
            s.push_str("- Acceptance criteria:\n");
            for a in &self.acceptance {
                s.push_str(&format!("  - {a}\n"));
            }
        }
        s.trim_end().to_string()
    }
}

/// A task with its contract text appended, as sent to the agent.
pub fn task_with_contract(task: &str, contract: Option<&Contract>) -> String {
    match contract {
        Some(c) => format!("{}\n\n{}", task.trim_end(), c.agent_text()),
        None => task.to_string(),
    }
}

/// A pattern must be relative, without `..`, `.` or empty segments and
/// without `!`, so it can only name paths inside the repository.
fn check_pattern(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Err("empty".into());
    }
    if p.starts_with('/') {
        return Err("must be relative to the repository root".into());
    }
    if p.starts_with('!') {
        return Err("negation is not supported; use forbidden".into());
    }
    if p.contains('\\') {
        return Err("use / between directories".into());
    }
    if p.chars().any(char::is_control) {
        return Err("has a control character".into());
    }
    if p.len() > MAX_PATTERN_BYTES {
        return Err(format!("longer than {MAX_PATTERN_BYTES} bytes"));
    }
    // One trailing slash is allowed (`src/`); anything else empty is not.
    let mut deep = 0;
    let mut last = "";
    for seg in p.strip_suffix('/').unwrap_or(p).split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err("has an empty, . or .. segment".into());
        }
        if seg.contains("**") && seg != "**" {
            return Err("** must be a whole segment".into());
        }
        if seg == "**" {
            // Each ** multiplies the matching work; a few are plenty.
            if last == "**" {
                return Err("** twice in a row".into());
            }
            deep += 1;
            if deep > MAX_DEEP {
                return Err(format!("more than {MAX_DEEP} ** segments"));
            }
        }
        if seg.matches('*').count() > MAX_STARS {
            return Err(format!("more than {MAX_STARS} * in one segment"));
        }
        last = seg;
    }
    Ok(())
}

/// Longest pattern accepted.
const MAX_PATTERN_BYTES: usize = 512;
/// Most `**` segments in one pattern.
const MAX_DEEP: usize = 3;
/// Most `*` in one segment.
const MAX_STARS: usize = 8;

/// True when `pattern` matches `path` or one of its parent directories.
/// `*` matches within one segment, `?` one character, `**` any number of
/// segments; matching is anchored at the repository root.
pub fn matches_path(pattern: &str, path: &Path) -> bool {
    let segs: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let pat: Vec<&str> = pattern
        .strip_suffix('/')
        .unwrap_or(pattern)
        .split('/')
        .collect();
    (1..=segs.len()).any(|n| match_segments(&pat, &segs[..n]))
}

fn match_segments(pat: &[&str], path: &[String]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|i| match_segments(rest, &path[i..])),
        Some((p, rest)) => match path.split_first() {
            Some((s, tail)) => wildcard(p, s) && match_segments(rest, tail),
            None => false,
        },
    }
}

/// `*` and `?` within one segment.
fn wildcard(p: &str, s: &str) -> bool {
    let p: Vec<char> = p.chars().collect();
    let s: Vec<char> = s.chars().collect();
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Splits a typed command into words: spaces separate words, single and
/// double quotes group them (no escapes inside single quotes; `\"` and
/// `\\` inside double quotes).
pub fn split_command(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err("unclosed ' in the command".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') if matches!(chars.peek(), Some('"' | '\\')) => {
                            cur.push(chars.next().unwrap_or('\\'));
                        }
                        Some(c) => cur.push(c),
                        None => return Err("unclosed \" in the command".into()),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

/// The directives a task line can start with.
const DIRECTIVES: &[&str] = &["@allow", "@forbid", "@verify", "@accept", "@contract"];

/// A task as typed, split into its text and its contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTask {
    pub task: String,
    pub contract: Option<Contract>,
}

/// Splits the typed task into the task text and a contract. A line whose
/// first word is `@allow`, `@forbid`, `@verify`, `@accept` or `@contract`
/// is a contract line and is taken out of the task; any other line stays.
///
/// - `@allow <pattern>...` and `@forbid <pattern>...` add patterns,
///   separated by spaces;
/// - `@verify <command>` sets the verification command (quotes group words);
/// - `@accept <text>` adds one acceptance criterion;
/// - `@contract <path>` reads the contract from a TOML file, relative to
///   `cwd`; it cannot be combined with the other lines.
pub fn parse_task(text: &str, cwd: &Path) -> Result<ParsedTask, String> {
    let mut task = Vec::new();
    let mut c = Contract::default();
    let mut typed = false;
    let mut file: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        let (word, rest) = trimmed
            .split_once(char::is_whitespace)
            .map_or((trimmed, ""), |(w, r)| (w, r.trim()));
        if !DIRECTIVES.contains(&word) {
            task.push(line);
            continue;
        }
        if rest.is_empty() {
            return Err(format!("{word} needs a value"));
        }
        match word {
            "@allow" => c.allowed.extend(rest.split_whitespace().map(String::from)),
            "@forbid" => c
                .forbidden
                .extend(rest.split_whitespace().map(String::from)),
            "@verify" => {
                if c.verify.is_some() {
                    return Err("only one @verify line".into());
                }
                c.verify = Some(split_command(rest)?);
            }
            "@accept" => c.acceptance.push(rest.to_string()),
            _ => {
                if file.is_some() {
                    return Err("only one @contract line".into());
                }
                file = Some(rest.to_string());
                continue;
            }
        }
        typed = true;
    }
    let task = task.join("\n").trim().to_string();
    let contract = match (file, typed) {
        (Some(_), true) => {
            return Err(
                "a contract comes from @contract or from typed @ lines, not both; \
                 put everything in the file"
                    .into(),
            );
        }
        (Some(path), false) => Some(load_file(&path, cwd)?),
        (None, true) => {
            c.validate()?;
            Some(c)
        }
        (None, false) => None,
    };
    Ok(ParsedTask { task, contract })
}

/// Reads a contract file the user named. Only a regular file of at most
/// [`MAX_FILE_BYTES`] is read (a FIFO could block the editor), and its
/// text is only parsed: nothing in it runs until the user presses `t`.
pub fn load_file(path: &str, cwd: &Path) -> Result<Contract, String> {
    let full: PathBuf = {
        let p = crate::editor::expand_tilde(Path::new(path));
        if p.is_absolute() { p } else { cwd.join(p) }
    };
    let shown = crate::navigation::display_path(&full, cwd);
    match std::fs::metadata(&full) {
        Ok(m) if !m.is_file() => return Err(format!("{shown} is not a regular file")),
        Ok(m) if m.len() > MAX_FILE_BYTES => {
            return Err(format!(
                "{shown} is larger than {} KiB",
                MAX_FILE_BYTES / 1024
            ));
        }
        Ok(_) => {}
        Err(e) => return Err(format!("contract {shown}: {e}")),
    }
    // Checked again on the open handle, and read with a limit, so a file
    // swapped or grown after the check above is still refused.
    let text = (|| -> std::io::Result<String> {
        use std::io::Read;
        let file = std::fs::File::open(&full)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            return Err(std::io::Error::other(
                "not a regular file of at most 64 KiB when opened",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(std::io::Error::other("grew past 64 KiB while being read"));
        }
        String::from_utf8(bytes).map_err(|_| std::io::Error::other("not UTF-8 text"))
    })()
    .map_err(|e| format!("contract {shown}: {e}"))?;
    let mut c = parse_toml(&text).map_err(|e| format!("contract {shown}: {e}"))?;
    c.source = Some(path.to_string());
    Ok(c)
}

/// Parses a contract file:
///
/// ```toml
/// allowed = ["calc.py", "tests/**"]
/// forbidden = ["secrets/**"]
/// verify = ["python3", "test_calc.py"]   # or one string, split like @verify
/// acceptance = ["add(2, 3) returns 5"]   # or one string
/// ```
pub fn parse_toml(text: &str) -> Result<Contract, String> {
    let f: ContractFile = toml::from_str(text).map_err(|e| e.message().to_string())?;
    let verify = match f.verify {
        None => None,
        Some(Words::One(s)) => Some(split_command(&s)?),
        Some(Words::Many(v)) => Some(v),
    };
    let acceptance = match f.acceptance {
        None => Vec::new(),
        Some(Words::One(s)) => vec![s],
        Some(Words::Many(v)) => v,
    };
    let c = Contract {
        allowed: f.allowed,
        forbidden: f.forbidden,
        verify,
        acceptance,
        source: None,
    };
    c.validate()?;
    if c.is_empty() {
        return Err("the contract is empty".into());
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, path: &str) -> bool {
        matches_path(p, Path::new(path))
    }

    #[test]
    fn patterns_are_anchored_and_cover_directories() {
        assert!(m("calc.py", "calc.py"));
        assert!(!m("calc.py", "sub/calc.py"));
        assert!(m("*.py", "calc.py"));
        assert!(!m("*.py", "sub/calc.py"));
        assert!(m("**/*.py", "sub/deep/calc.py"));
        assert!(m("**/*.py", "calc.py"));
        assert!(m("src", "src/a/b.rs"));
        assert!(m("src/", "src/a/b.rs"));
        assert!(m("src/**", "src/a/b.rs"));
        assert!(!m("src", "srcs/a.rs"));
        assert!(m("test_?.py", "test_a.py"));
        assert!(!m("test_?.py", "test_ab.py"));
        assert!(m("a*b*c", "aXXbYYc"));
        assert!(!m("a*b*c", "aXXbYY"));
        assert!(m("docs/**/index.md", "docs/index.md"));
        assert!(m("docs/**/index.md", "docs/x/y/index.md"));
    }

    #[test]
    fn violations_name_the_rule() {
        let c = Contract {
            allowed: vec!["src/**".into(), "README.md".into()],
            forbidden: vec!["src/secret.rs".into()],
            ..Default::default()
        };
        assert_eq!(c.violation(Path::new("src/lib.rs")), None);
        assert_eq!(c.violation(Path::new("README.md")), None);
        assert_eq!(
            c.violation(Path::new("src/secret.rs")).unwrap(),
            "src/secret.rs matches the forbidden pattern src/secret.rs"
        );
        assert_eq!(
            c.violation(Path::new("Cargo.toml")).unwrap(),
            "Cargo.toml matches no allowed pattern"
        );
        let open = Contract {
            forbidden: vec!["*.lock".into()],
            ..Default::default()
        };
        assert_eq!(open.violation(Path::new("deep/x.rs")), None);
        assert!(open.violation(Path::new("Cargo.lock")).is_some());
    }

    #[test]
    fn typed_lines_become_the_contract() {
        let p = parse_task(
            "Add mul to calc.py\n@allow calc.py test_*.py\n@forbid secrets\n\
             @verify sh -c \"python3 test_calc.py && echo ok\"\n@accept mul(3, 4) is 12\n\
             @decorator stays in the task",
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(p.task, "Add mul to calc.py\n@decorator stays in the task");
        let c = p.contract.unwrap();
        assert_eq!(c.allowed, ["calc.py", "test_*.py"]);
        assert_eq!(c.forbidden, ["secrets"]);
        assert_eq!(
            c.verify.as_deref().unwrap(),
            ["sh", "-c", "python3 test_calc.py && echo ok"]
        );
        assert_eq!(c.acceptance, ["mul(3, 4) is 12"]);
        assert_eq!(c.source, None);
        // No directive, no contract.
        let p = parse_task("just a task", Path::new("/")).unwrap();
        assert_eq!(p.contract, None);
        // Bad patterns and values are refused.
        assert!(parse_task("x\n@allow ../up", Path::new("/")).is_err());
        assert!(parse_task("x\n@allow /abs", Path::new("/")).is_err());
        assert!(parse_task("x\n@verify", Path::new("/")).is_err());
        assert!(parse_task("x\n@verify 'open", Path::new("/")).is_err());
        assert!(parse_task("x\n@verify a\n@verify b", Path::new("/")).is_err());
    }

    #[test]
    fn files_are_parsed_strictly() {
        let c = parse_toml(
            "allowed = [\"calc.py\"]\nverify = \"python3 t.py\"\nacceptance = \"works\"\n",
        )
        .unwrap();
        assert_eq!(c.verify.as_deref().unwrap(), ["python3", "t.py"]);
        assert_eq!(c.acceptance, ["works"]);
        assert!(parse_toml("allowed = [\"x\"]\nrun = \"rm -rf /\"\n").is_err());
        assert!(parse_toml("").is_err());
        assert!(parse_toml("verify = []").is_err());
        assert!(parse_toml("forbidden = [\"a/../b\"]").is_err());
    }

    #[test]
    fn the_command_is_shown_quoted() {
        let c = Contract {
            verify: Some(vec!["sh".into(), "-c".into(), "echo hi\nrm x".into()]),
            ..Default::default()
        };
        assert_eq!(c.verify_shown().unwrap(), "sh -c $'echo hi\\nrm x'");
    }

    #[test]
    fn costly_and_odd_patterns_are_refused() {
        let ok = |p: &str| check_pattern(p).is_ok();
        assert!(ok("src/") && ok("a/**/b/**/c/**/d"));
        assert!(!ok("src//") && !ok("src////") && !ok("a//b"));
        assert!(!ok("**/**/x"), "** twice in a row");
        assert!(!ok("a/**/b/**/c/**/d/**/e"), "four **");
        assert!(!ok(&"*".repeat(9)));
        assert!(!ok(&"a".repeat(600)));
        // The worst accepted pattern against a deep path stays cheap.
        let deep: String = (0..40).map(|i| format!("d{i}/")).collect::<String>() + "f";
        let t = std::time::Instant::now();
        assert!(!m("**/x*y*z/**/q*w/**/nope", &deep));
        assert!(t.elapsed() < std::time::Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn names_that_are_not_utf8_are_outside_any_pattern() {
        use std::os::unix::ffi::OsStrExt;
        let raw = Path::new(std::ffi::OsStr::from_bytes(b"\xff.txt"));
        let c = Contract {
            allowed: vec!["\u{FFFD}.txt".into(), "**".into()],
            ..Default::default()
        };
        assert!(c.violation(raw).is_some());
        assert!(Contract::default().violation(raw).is_none(), "no patterns");
    }
}
