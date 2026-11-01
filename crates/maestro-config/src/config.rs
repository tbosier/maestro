//! User configuration, read from `$XDG_CONFIG_HOME/maestro/config.toml`
//! (or the platform equivalent). Every field has a default, so an empty or
//! missing file is a complete configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    /// Selection-first hybrid: Kakoune/Helix selections with Vim-familiar
    /// motions and commands.
    #[default]
    Maestro,
    /// Vim operator-pending grammar where implemented (see docs/editor-model.md).
    Vim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LineNumbers {
    #[default]
    Absolute,
    Relative,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IndentStyle {
    #[default]
    Spaces,
    Tabs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IconMode {
    /// Plain text markers. Works in every terminal and font.
    #[default]
    Text,
    /// Nerd Font glyphs. Only enable if your terminal font has them.
    Nerd,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct EditorConfig {
    pub tab_width: usize,
    pub indent: IndentStyle,
    pub indent_width: usize,
    pub line_numbers: LineNumbers,
    pub cursorline: bool,
    /// Lines kept visible above and below the cursor when scrolling.
    pub scrolloff: usize,
    pub icons: IconMode,
    /// Seconds of idle time before unsaved changes are written to the
    /// recovery directory. 0 disables recovery files.
    pub recovery_interval_secs: u64,
    pub mouse: bool,
    /// Reopen the files from the last session in this directory when
    /// started without file arguments.
    pub restore_session: bool,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            tab_width: 4,
            indent: IndentStyle::Spaces,
            indent_width: 4,
            line_numbers: LineNumbers::Absolute,
            cursorline: true,
            scrolloff: 5,
            icons: IconMode::Text,
            recovery_interval_secs: 2,
            mouse: true,
            restore_session: true,
        }
    }
}

impl EditorConfig {
    pub fn indent_unit(&self) -> String {
        match self.indent {
            IndentStyle::Tabs => "\t".to_string(),
            IndentStyle::Spaces => " ".repeat(self.indent_width.max(1)),
        }
    }
}

/// A language server for one language: `[language-server.rust]`.
///
/// Servers are run by name from this config or the built-in defaults, never
/// from files inside a repository (see docs/security.md).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LanguageServerConfig {
    /// Program name looked up on `PATH`, or a path.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// `false` turns the language's server off.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl LanguageServerConfig {
    pub fn new(command: &str, args: &[&str]) -> Self {
        Self {
            command: command.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            enabled: true,
        }
    }
}

/// Built-in servers per language, in order of preference. The first one
/// found on `PATH` is used.
pub fn default_language_servers(language: &str) -> Vec<LanguageServerConfig> {
    let ts = || LanguageServerConfig::new("typescript-language-server", &["--stdio"]);
    match language {
        "rust" => vec![LanguageServerConfig::new("rust-analyzer", &[])],
        "python" => vec![
            LanguageServerConfig::new("pyright-langserver", &["--stdio"]),
            LanguageServerConfig::new("pylsp", &[]),
        ],
        "typescript" | "javascript" => vec![ts()],
        "c" | "cpp" => vec![LanguageServerConfig::new("clangd", &[])],
        _ => Vec::new(),
    }
}

/// How Maestro talks to an agent program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentProviderKind {
    /// Agent Client Protocol on stdin and stdout: structured messages,
    /// tool calls, permission requests and file access through the editor.
    Acp,
    /// Any command-line agent, run in a terminal. Maestro sees only the
    /// terminal screen and the files it changes.
    Terminal,
}

/// An agent program: `[agents.provider.gemini]`.
///
/// Like language servers, providers come from this config or the built-in
/// list, never from files inside a repository (see docs/security.md).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct AgentProviderConfig {
    pub kind: AgentProviderKind,
    /// Program name looked up on `PATH`, or a path.
    pub command: String,
    /// Arguments. For a terminal agent, an argument containing `{task}` has
    /// it replaced by the task text; with none, the task is typed into the
    /// terminal once it starts.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `false` hides a built-in provider of the same name.
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// `[agents]`: where agent worktrees go and how their work is checked.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct AgentsConfig {
    /// Directory for agent worktrees. Unset means a sibling of the
    /// repository named `<repository>-worktrees`. `~` expands to the home
    /// directory.
    pub worktree_dir: Option<String>,
    /// The verification command run in an agent's worktree, as program and
    /// arguments. Empty means `cargo test` when the worktree has a
    /// `Cargo.toml`, else no command.
    pub test_command: Vec<String>,
    /// Providers that add to or replace the built-in ones, by name.
    pub provider: BTreeMap<String, AgentProviderConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    pub theme: String,
    pub profile: Profile,
    pub editor: EditorConfig,
    /// Extra key bindings per mode: `[keys.normal] "space w" = "write"`.
    pub keys: BTreeMap<String, BTreeMap<String, String>>,
    /// Language servers that replace the built-in choice for a language.
    pub language_server: BTreeMap<String, LanguageServerConfig>,
    pub agents: AgentsConfig,
    pub sidebar: SidebarConfig,
    pub projects: ProjectsConfig,
    pub notify: NotifyConfig,
    pub lsp: LspConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "duskfox".into(),
            profile: Profile::default(),
            editor: EditorConfig::default(),
            keys: BTreeMap::new(),
            language_server: BTreeMap::new(),
            agents: AgentsConfig::default(),
            sidebar: SidebarConfig::default(),
            projects: ProjectsConfig::default(),
            notify: NotifyConfig::default(),
            lsp: LspConfig::default(),
        }
    }
}

/// `[sidebar]`: the projects and worktrees column (`space s`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct SidebarConfig {
    /// Columns the sidebar takes when open. It never takes more than half
    /// the screen.
    pub width: u16,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self { width: 36 }
    }
}

/// `[projects]`: Git repositories the sidebar and the jump palette list,
/// besides the directories of earlier sessions.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProjectsConfig {
    /// Directories inside Git repositories. `~` expands to the home
    /// directory.
    pub paths: Vec<String>,
}

/// How Maestro tells the terminal that an agent needs you.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NotifyMethod {
    /// The terminal bell (BEL).
    #[default]
    Bell,
    /// An OSC 9 desktop notification, for terminals that show them.
    Osc,
    /// Only the status line message.
    Off,
}

/// `[notify]`: what happens, besides the status line message, when an
/// agent starts needing you.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct NotifyConfig {
    pub method: NotifyMethod,
}

/// `[lsp]`: when language servers for worktrees you left are stopped.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct LspConfig {
    /// Seconds a server outside the current worktree may run with no open
    /// buffer before it is stopped. 0 never stops one for that reason.
    pub stop_unused_after_secs: u64,
    /// Most workspace roots with running servers. Beyond it, servers of
    /// other roots with no open buffer are stopped, least recently used
    /// first. 0 means no limit.
    pub max_live_roots: usize,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            stop_unused_after_secs: 300,
            max_live_roots: 3,
        }
    }
}

impl Config {
    /// Server candidates for `language`: the user's entry if there is one
    /// (none if it is disabled), else the built-in defaults.
    pub fn language_servers(&self, language: &str) -> Vec<LanguageServerConfig> {
        match self.language_server.get(language) {
            Some(c) if c.enabled => vec![c.clone()],
            Some(_) => Vec::new(),
            None => default_language_servers(language),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config in {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
}

impl Config {
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(e),
        })
    }

    /// Loads the user config. A missing file is not an error.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

/// `$XDG_CONFIG_HOME/maestro`, falling back to `~/.config/maestro`, or
/// `%APPDATA%\maestro` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("maestro"));
    }
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("maestro"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("maestro"))
}

/// `$XDG_STATE_HOME/maestro`, falling back to `~/.local/state/maestro`, or
/// `%LOCALAPPDATA%\maestro` on Windows. Holds sessions and recovery files.
pub fn state_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("maestro"));
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("maestro"));
    }
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".local")
            .join("state")
            .join("maestro")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_default() {
        assert_eq!(
            Config::parse("", Path::new("x")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn parses_agent_settings() {
        let c = Config::parse(
            r#"
[agents]
worktree-dir = "~/worktrees"
test-command = ["cargo", "test", "--workspace"]
[agents.provider.local]
kind = "terminal"
command = "my-agent"
args = ["--task", "{task}"]
env = { MODE = "fast" }
[agents.provider.gemini]
kind = "acp"
command = "gemini"
enabled = false
"#,
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(c.agents.worktree_dir.as_deref(), Some("~/worktrees"));
        assert_eq!(c.agents.test_command, ["cargo", "test", "--workspace"]);
        let local = &c.agents.provider["local"];
        assert_eq!(local.kind, AgentProviderKind::Terminal);
        assert_eq!(local.args, ["--task", "{task}"]);
        assert_eq!(local.env["MODE"], "fast");
        assert!(local.enabled);
        assert!(!c.agents.provider["gemini"].enabled);
        assert!(
            Config::parse(
                "[agents]
test-cmd = []
",
                Path::new("x")
            )
            .is_err()
        );
        assert!(
            Config::parse(
                "[agents.provider.x]
kind = \"pty\"\ncommand = \"x\"\n",
                Path::new("x")
            )
            .is_err()
        );
    }

    #[test]
    fn parses_fields_and_rejects_typos() {
        let c = Config::parse(
            r#"
profile = "vim"
[editor]
tab-width = 8
indent = "tabs"
line-numbers = "relative"
[keys.normal]
"space w" = "write"
"#,
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(c.profile, Profile::Vim);
        assert_eq!(c.editor.tab_width, 8);
        assert_eq!(c.editor.indent_unit(), "\t");
        assert_eq!(c.keys["normal"]["space w"], "write");
        assert!(Config::parse("[editor]\ntab-widht = 2\n", Path::new("x")).is_err());
    }

    #[test]
    fn parses_workspace_settings() {
        let c = Config::parse(
            r#"
[sidebar]
width = 40
[projects]
paths = ["~/src/a", "/work/b"]
[notify]
method = "osc"
[lsp]
stop-unused-after-secs = 60
max-live-roots = 2
"#,
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(c.sidebar.width, 40);
        assert_eq!(c.projects.paths, ["~/src/a", "/work/b"]);
        assert_eq!(c.notify.method, NotifyMethod::Osc);
        assert_eq!(c.lsp.stop_unused_after_secs, 60);
        assert_eq!(c.lsp.max_live_roots, 2);
        let d = Config::default();
        assert_eq!(d.notify.method, NotifyMethod::Bell);
        assert_eq!(d.lsp.stop_unused_after_secs, 300);
        assert_eq!(d.sidebar.width, 36);
        assert!(Config::parse("[notify]\nmethod = \"loud\"\n", Path::new("x")).is_err());
        assert!(Config::parse("[sidebar]\nwidht = 3\n", Path::new("x")).is_err());
    }

    #[test]
    fn language_servers_override_defaults_per_language() {
        let c = Config::parse(
            r#"
[language-server.rust]
command = "/opt/ra/rust-analyzer"
args = ["--log", "x"]
[language-server.python]
command = "pylsp"
enabled = false
"#,
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(
            c.language_servers("rust"),
            vec![LanguageServerConfig::new(
                "/opt/ra/rust-analyzer",
                &["--log", "x"]
            )]
        );
        assert!(c.language_servers("python").is_empty());
        // Untouched languages keep the defaults.
        assert_eq!(c.language_servers("c")[0].command, "clangd");
        assert_eq!(
            Config::default().language_servers("python")[1].command,
            "pylsp"
        );
        assert!(c.language_servers("cobol").is_empty());
        assert!(Config::parse("[language-server.rust]\ncmd = \"x\"\n", Path::new("x")).is_err());
    }
}
