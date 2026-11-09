//! Restoring what ran inside a terminal: which programs have a verified resume command,
//! which terminals can be told to run a command, and how the relaunch is put together.

use crate::command_detection::{shell_join, shell_quote};
use crate::session::Foreground;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Built-in resume commands, keyed by program basename. Values are complete replacement
/// command lines. Each entry was verified against the program's own documentation;
/// do not add one that has not been, and do not invent a generic fallback.
pub const BUILTIN_RESUME: &[(&str, &str)] = &[
    ("claude", "claude --continue"),
    ("muse", "muse resume --last"),
    ("codex", "codex resume --last"),
    ("gemini", "gemini --resume"),
    ("aider", "aider --restore-chat-history"),
    ("opencode", "opencode --continue"),
    ("copilot", "copilot --continue"),
    ("goose", "goose session --resume"),
    ("qwen", "qwen --continue"),
];

/// How a terminal is told to run a command instead of its shell
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExecStyle {
    /// The command and its arguments are simply appended to the terminal's command line
    Positional,
    /// The command and its arguments follow this flag, which must come last
    Flag(&'static str),
}

/// Terminals known to run a command given on their command line. An entry is enabled only
/// with a vendor-documentation citation for its form:
///  * foot: foot.1 — "*foot* [OPTIONS] <command> [COMMAND OPTIONS]. All trailing
///    (non-option) arguments are treated as a command" (doc/foot.1.scd, codeberg.org/dnkl/foot)
///  * kitty: usage `kitty [options] [program-to-run ...]`, e.g. `kitty --hold sh -c "..."`
///    (kitty/cli.py, github.com/kovidgoyal/kitty)
///  * alacritty: alacritty(1) "-e, --command <COMMAND>... Command and args to execute
///    (must be last argument)" (extra/man/alacritty.1.scd, github.com/alacritty/alacritty)
pub const TERMINAL_PROFILES: &[(&str, ExecStyle)] = &[
    ("foot", ExecStyle::Positional),
    ("kitty", ExecStyle::Positional),
    ("alacritty", ExecStyle::Flag("-e")),
];

/// Marks the wrapper hyprsession puts around a resumed program, so a later save can see
/// through it
pub const WRAPPER_MARKER: &str = "hyprsession-resume";

/// Runs in the terminal: go to the program's directory, run the command, then become the
/// user's shell so the window stays open at a prompt. Data arrives as $1 and $2, never
/// spliced into the script.
const WRAPPER_SCRIPT: &str = r#"cd "$1" 2>/dev/null; eval "$2"; exec "${SHELL:-/bin/sh}""#;

/// What to do with programs that have no resume entry (DEC-2)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unlisted {
    /// Replay the recorded argv verbatim
    Replay,
    /// Restore the terminal at a clean prompt
    #[default]
    Prompt,
}

#[derive(Clone, Debug)]
pub struct ResumeConfig {
    table: HashMap<String, String>,
    pub unlisted: Unlisted,
    /// Terminals the user added: detected, but with no verified way to run a command in them
    extra_terminals: HashSet<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeFile {
    settings: Option<SettingsSection>,
    resume: Option<BTreeMap<String, String>>,
    terminals: Option<TerminalsSection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsSection {
    unlisted: Option<Unlisted>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalsSection {
    #[serde(default)]
    extra: Vec<String>,
}

/// What load does with a terminal's recorded foreground program
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Relaunch {
    /// The program owns its own window; relaunch nothing here
    Linked,
    /// Run this resume-table command line
    Table(String),
    /// Run the recorded argv again
    Replay(String),
    /// Leave the terminal at a prompt
    CleanPrompt(String),
}

impl Default for ResumeConfig {
    fn default() -> Self {
        ResumeConfig {
            table: BUILTIN_RESUME
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            unlisted: Unlisted::default(),
            extra_terminals: HashSet::new(),
        }
    }
}

/// `$XDG_CONFIG_HOME/hyprsession/resume.toml`, else `~/.config/hyprsession/resume.toml`
pub fn default_resume_path() -> Option<PathBuf> {
    resume_path_from(
        std::env::var("XDG_CONFIG_HOME").ok(),
        std::env::var("HOME").ok(),
    )
}

fn resume_path_from(xdg_config_home: Option<String>, home: Option<String>) -> Option<PathBuf> {
    let base = match xdg_config_home {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(home?).join(".config"),
    };
    Some(base.join("hyprsession").join("resume.toml"))
}

impl ResumeConfig {
    /// The built-in table extended and overridden by the user's file, if there is one
    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, &path.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: cannot read: {e}", path.display())),
        }
    }

    /// Parse a resume file; errors name the file, line and problem
    pub fn parse(text: &str, source: &str) -> Result<Self, String> {
        let file: ResumeFile = toml::from_str(text).map_err(|e| {
            let line = e
                .span()
                .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1);
            match line {
                Some(line) => format!("{source}:{line}: {}", e.message()),
                None => format!("{source}: {}", e.message()),
            }
        })?;
        let mut config = Self::default();
        if let Some(unlisted) = file.settings.and_then(|s| s.unlisted) {
            config.unlisted = unlisted;
        }
        for (program, command) in file.resume.unwrap_or_default() {
            config.table.insert(program.to_lowercase(), command);
        }
        config.extra_terminals = file
            .terminals
            .map(|t| t.extra.into_iter().map(|n| n.to_lowercase()).collect())
            .unwrap_or_default();
        Ok(config)
    }

    /// Whether a program name is a terminal emulator worth looking inside
    pub fn is_terminal(&self, name: &str) -> bool {
        terminal_style(name).is_some() || self.extra_terminals.contains(&name.to_lowercase())
    }

    /// Decide what load does with a recorded foreground program
    pub fn decide(&self, foreground: &Foreground) -> Relaunch {
        if foreground.linked {
            return Relaunch::Linked;
        }
        let identity = program_identity(&foreground.argv);
        if let Some(command) = identity.as_ref().and_then(|name| self.table.get(name)) {
            return if command.trim().is_empty() {
                Relaunch::CleanPrompt(format!(
                    "'{}' is set to resume nothing",
                    identity.unwrap_or_default()
                ))
            } else {
                Relaunch::Table(command.clone())
            };
        }
        match self.unlisted {
            Unlisted::Replay if !foreground.argv.is_empty() => {
                Relaunch::Replay(shell_join(&foreground.argv))
            }
            Unlisted::Replay => Relaunch::CleanPrompt("no recorded command".to_string()),
            Unlisted::Prompt => {
                Relaunch::CleanPrompt("no resume entry and unlisted = \"prompt\"".to_string())
            }
        }
    }
}

pub fn terminal_style(name: &str) -> Option<ExecStyle> {
    let name = name.to_lowercase();
    TERMINAL_PROFILES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, style)| *style)
}

/// Words that hand the real program to the next argument; an interpreter's program is
/// the script it runs
const INTERPRETERS: &[&str] = &[
    "node", "nodejs", "bun", "deno", "ruby", "perl", "sh", "bash", "env", "python",
];

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_interpreter(name: &str) -> bool {
    INTERPRETERS.contains(&name)
        || name
            .strip_prefix("python")
            .is_some_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// The program a process is running: the basename of argv[0] (lower-cased), or, for an
/// interpreter (`node /…/codex`, `python3 /…/aider`, `env KEY=v prog`), the script or
/// program it was given. Option words are skipped, and `KEY=value` words after `env`.
pub fn program_identity(argv: &[String]) -> Option<String> {
    let first = basename(argv.first()?.trim_start_matches('-')).to_lowercase();
    if !is_interpreter(&first) {
        return Some(first).filter(|n| !n.is_empty());
    }
    let skip_assignments = first == "env";
    let program = argv.iter().skip(1).find(|arg| {
        !arg.starts_with('-') && !(skip_assignments && arg.contains('=') && !arg.contains('/'))
    })?;
    let name = basename(program).to_lowercase();
    let name = [".js", ".mjs", ".cjs", ".py", ".rb", ".pl"]
        .iter()
        .find_map(|ext| name.strip_suffix(ext))
        .map(str::to_string)
        .unwrap_or(name);
    Some(name).filter(|n| !n.is_empty())
}

/// The terminal's own command with the program wrapper appended
pub fn wrap_terminal_command(
    terminal_command: &str,
    style: ExecStyle,
    program_cwd: &str,
    program_command: &str,
) -> String {
    let mut parts = vec![terminal_command.to_string()];
    if let ExecStyle::Flag(flag) = style {
        parts.push(flag.to_string());
    }
    parts.extend(
        [
            "sh",
            "-c",
            WRAPPER_SCRIPT,
            WRAPPER_MARKER,
            program_cwd,
            program_command,
        ]
        .iter()
        .map(|word| shell_quote(word)),
    );
    parts.join(" ")
}

/// Recover only terminal options, never its original program or our resume wrapper.
/// Explicit replay is handled by the foreground policy, not by terminal argv.
pub fn strip_wrapper(argv: &[String], style: Option<ExecStyle>) -> Vec<String> {
    let Some(style) = style else {
        return argv.to_vec();
    };
    if let ExecStyle::Flag(flag) = style {
        let end = argv
            .iter()
            .position(|word| {
                word == flag
                    || word == "--command"
                    || word == "--"
                    || word.starts_with("--command=")
                    || (word.starts_with(flag) && word.len() > flag.len())
            })
            .unwrap_or(argv.len());
        return argv[..end].to_vec();
    }
    let mut end = argv.len();
    let mut i = 1;
    while i < argv.len() {
        let word = argv[i].as_str();
        if word == "--" || word == "-e" || word == "--command" {
            end = i;
            break;
        }
        if !word.starts_with('-') {
            if style == ExecStyle::Positional {
                end = i;
            }
            break;
        }
        // Options taking a separate argument in the supported terminal profiles.
        // Equals-form options need no special treatment.
        let takes_value = matches!(
            word,
            "--title"
                | "-T"
                | "--app-id"
                | "--class"
                | "--name"
                | "--working-directory"
                | "--directory"
                | "-D"
                | "--config"
                | "-c"
                | "--override"
                | "--option"
                | "-o"
                | "--listen-on"
                | "--session"
                | "--start-as"
                | "--os-window-tag"
                | "--config-file"
                | "--socket"
                | "--font"
                | "--font-size"
                | "--geometry"
                | "--resize-delay"
                | "--hold-at-exit"
                | "--log-level"
                | "--embed"
                | "--dimensions"
                | "--position"
        );
        i += if takes_value { 2 } else { 1 };
    }
    argv[..end].to_vec()
}

/// The (directory, command) a wrapper process was asked to run: the two words after the
/// marker in `sh -c <script> MARKER <cwd> <command>`. `None` for any other argv.
pub fn wrapper_program(argv: &[String]) -> Option<(String, String)> {
    let marker = argv.iter().position(|a| a == WRAPPER_MARKER)?;
    let start = marker.checked_sub(3)?;
    if argv[start] != "sh" || argv[start + 1] != "-c" {
        return None;
    }
    Some((argv.get(marker + 1)?.clone(), argv.get(marker + 2)?.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fg(argv: &[&str]) -> Foreground {
        Foreground {
            pid: 7,
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: Some("/home/synthetic/proj".to_string()),
            linked: false,
        }
    }

    fn args(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_builtin_commands_replace_recorded_arguments() {
        let config = ResumeConfig::default();
        assert_eq!(
            config.decide(&fg(&["claude"])),
            Relaunch::Table("claude --continue".into())
        );
        assert_eq!(
            config.decide(&fg(&["goose", "session"])),
            Relaunch::Table("goose session --resume".into())
        );
        assert_eq!(
            config.decide(&fg(&["qwen"])),
            Relaunch::Table("qwen --continue".into())
        );
        // The replacement is complete: extra recorded flags are dropped
        assert_eq!(
            config.decide(&fg(&["/usr/bin/claude", "--model", "x"])),
            Relaunch::Table("claude --continue".into())
        );
    }

    #[test]
    fn test_unlisted_program_follows_the_policy() {
        let replay = ResumeConfig::parse("[settings]\nunlisted = \"replay\"\n", "t").unwrap();
        assert_eq!(
            replay.decide(&fg(&["htop", "-d", "5"])),
            Relaunch::Replay("htop -d 5".into())
        );
        // Verbatim means quoted so it round-trips
        assert_eq!(
            replay.decide(&fg(&["vim", "my file.txt"])),
            Relaunch::Replay("vim 'my file.txt'".into())
        );
        let prompt = ResumeConfig::parse("[settings]\nunlisted = \"prompt\"\n", "t").unwrap();
        assert!(matches!(
            prompt.decide(&fg(&["htop"])),
            Relaunch::CleanPrompt(_)
        ));
        // The setting does not touch listed programs
        assert_eq!(
            prompt.decide(&fg(&["codex"])),
            Relaunch::Table("codex resume --last".into())
        );
    }

    #[test]
    fn default_unlisted_foreground_restores_a_clean_prompt() {
        assert!(matches!(
            ResumeConfig::default().decide(&fg(&["sh", "-c", "dangerous-command"])),
            Relaunch::CleanPrompt(_)
        ));
    }

    #[test]
    fn terminal_argv_does_not_replay_an_explicit_program() {
        for (original, expected, style) in [
            (
                args(&[
                    "kitty",
                    "--title",
                    "work",
                    "--",
                    "sh",
                    "-c",
                    "dangerous-command",
                ]),
                args(&["kitty", "--title", "work"]),
                ExecStyle::Positional,
            ),
            (
                args(&["kitty", "sh", "-c", "dangerous-command"]),
                args(&["kitty"]),
                ExecStyle::Positional,
            ),
            (
                args(&[
                    "alacritty",
                    "--title",
                    "work",
                    "-e",
                    "sh",
                    "-c",
                    "dangerous-command",
                ]),
                args(&["alacritty", "--title", "work"]),
                ExecStyle::Flag("-e"),
            ),
        ] {
            assert_eq!(strip_wrapper(&original, Some(style)), expected);
        }
    }

    #[test]
    fn attached_terminal_command_arguments_are_not_replayed() {
        for command in ["--command=sh", "-esh"] {
            let original = args(&[
                "alacritty",
                "--title",
                "work",
                command,
                "-c",
                "dangerous-command",
            ]);
            assert_eq!(
                strip_wrapper(&original, Some(ExecStyle::Flag("-e"))),
                args(&["alacritty", "--title", "work"])
            );
        }
    }

    #[test]
    fn test_user_file_extends_and_overrides_per_key() {
        let config = ResumeConfig::parse(
            "[resume]\nclaude = \"claude --continue --model sonnet\"\nmytool = \"mytool --last\"\naider = \"\"\n",
            "t",
        ).unwrap();
        assert_eq!(
            config.decide(&fg(&["claude"])),
            Relaunch::Table("claude --continue --model sonnet".into())
        );
        assert_eq!(
            config.decide(&fg(&["mytool", "x"])),
            Relaunch::Table("mytool --last".into())
        );
        // An empty override resumes nothing
        assert!(matches!(
            config.decide(&fg(&["aider"])),
            Relaunch::CleanPrompt(_)
        ));
        // Other built-ins survive
        assert_eq!(
            config.decide(&fg(&["opencode"])),
            Relaunch::Table("opencode --continue".into())
        );
    }

    #[test]
    fn test_linked_foreground_relaunches_nothing() {
        let mut linked = fg(&["claude"]);
        linked.linked = true;
        assert_eq!(ResumeConfig::default().decide(&linked), Relaunch::Linked);
    }

    #[test]
    fn test_resume_file_errors_name_file_line_and_problem() {
        let err = ResumeConfig::parse("[settings]\nunlisted = \"sometimes\"\n", "/x/resume.toml")
            .unwrap_err();
        assert!(err.starts_with("/x/resume.toml:2:"), "{err}");
        assert!(
            err.contains("sometimes") || err.contains("variant"),
            "{err}"
        );
        let err = ResumeConfig::parse("[resume]\nclaude = 5\n", "r.toml").unwrap_err();
        assert!(err.starts_with("r.toml:2:"), "{err}");
        let err = ResumeConfig::parse("[resume\nx = 1", "r.toml").unwrap_err();
        assert!(err.starts_with("r.toml:"), "{err}");
        // Unknown sections are errors too: a typo must not silently drop overrides
        let err = ResumeConfig::parse("[resumee]\nclaude = \"x\"\n", "r.toml").unwrap_err();
        assert!(err.starts_with("r.toml:1:"), "{err}");
    }

    #[test]
    fn test_missing_file_means_builtins_and_a_bad_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = ResumeConfig::load(Some(&dir.path().join("nope.toml"))).unwrap();
        assert!(matches!(
            config.decide(&fg(&["claude"])),
            Relaunch::Table(_)
        ));
        assert!(ResumeConfig::load(None).is_ok());
        let bad = dir.path().join("resume.toml");
        std::fs::write(&bad, "[resume]\nclaude = [\n").unwrap();
        let err = ResumeConfig::load(Some(&bad)).unwrap_err();
        assert!(err.contains("resume.toml"), "{err}");
    }

    #[test]
    fn test_resume_path_follows_xdg() {
        assert_eq!(
            resume_path_from(Some("/x/cfg".into()), Some("/home/synthetic".into())).unwrap(),
            PathBuf::from("/x/cfg/hyprsession/resume.toml")
        );
        for unset in [None, Some(String::new())] {
            assert_eq!(
                resume_path_from(unset, Some("/home/synthetic".into())).unwrap(),
                PathBuf::from("/home/synthetic/.config/hyprsession/resume.toml")
            );
        }
        assert!(resume_path_from(None, None).is_none());
    }

    #[test]
    fn test_program_identity_sees_through_interpreters() {
        let id = |argv: &[&str]| program_identity(&args(argv));
        assert_eq!(id(&["/usr/bin/claude"]).as_deref(), Some("claude"));
        assert_eq!(id(&["-zsh"]).as_deref(), Some("zsh"));
        assert_eq!(
            id(&[
                "node",
                "/usr/lib/node_modules/@openai/codex/bin/codex.js",
                "x"
            ])
            .as_deref(),
            Some("codex")
        );
        assert_eq!(
            id(&["node", "--no-warnings", "/opt/gemini"]).as_deref(),
            Some("gemini")
        );
        assert_eq!(
            id(&["python3.12", "/home/u/.local/bin/aider"]).as_deref(),
            Some("aider")
        );
        assert_eq!(
            id(&["env", "FOO=1", "BAR=two", "opencode"]).as_deref(),
            Some("opencode")
        );
        assert_eq!(
            id(&["env", "-i", "FOO=1", "/opt/bin/qwen"]).as_deref(),
            Some("qwen")
        );
        // A program name that merely starts like an interpreter is not one
        assert_eq!(id(&["pythonista"]).as_deref(), Some("pythonista"));
        assert_eq!(id(&["node"]), None);
        assert_eq!(id(&[]), None);
    }

    #[test]
    fn test_node_shipped_cli_hits_the_table() {
        let config = ResumeConfig::default();
        assert_eq!(
            config.decide(&fg(&[
                "node",
                "/usr/lib/node_modules/@google/gemini-cli/bin/gemini"
            ])),
            Relaunch::Table("gemini --resume".into())
        );
    }

    #[test]
    fn test_terminal_detection() {
        let config = ResumeConfig::parse("[terminals]\nextra = [\"MyTerm\"]\n", "t").unwrap();
        assert!(
            config.is_terminal("foot")
                && config.is_terminal("Kitty")
                && config.is_terminal("alacritty")
        );
        assert!(config.is_terminal("myterm"));
        assert!(!config.is_terminal("code") && !ResumeConfig::default().is_terminal("myterm"));
        // Only cited terminals have an exec style
        assert!(terminal_style("myterm").is_none() && terminal_style("foot").is_some());
        assert!(TERMINAL_PROFILES.iter().any(|(n, _)| *n == "foot"));
    }

    #[test]
    fn test_wrapper_shape_per_style() {
        let positional = wrap_terminal_command(
            "foot --title t",
            ExecStyle::Positional,
            "/p q",
            "claude --continue",
        );
        assert_eq!(
            positional,
            r#"foot --title t sh -c 'cd "$1" 2>/dev/null; eval "$2"; exec "${SHELL:-/bin/sh}"' hyprsession-resume '/p q' 'claude --continue'"#
        );
        let flagged = wrap_terminal_command("alacritty", ExecStyle::Flag("-e"), "/p", "htop");
        assert!(flagged.starts_with("alacritty -e sh -c "), "{flagged}");
        assert!(flagged.ends_with("hyprsession-resume /p htop"), "{flagged}");
    }

    #[test]
    fn test_strip_wrapper_recovers_the_terminal_argv() {
        let wrapped = args(&[
            "foot",
            "--title",
            "t",
            "sh",
            "-c",
            "script",
            "hyprsession-resume",
            "/p",
            "cmd",
        ]);
        assert_eq!(
            strip_wrapper(&wrapped, Some(ExecStyle::Positional)),
            args(&["foot", "--title", "t"])
        );
        let flagged = args(&[
            "alacritty",
            "-e",
            "sh",
            "-c",
            "script",
            "hyprsession-resume",
            "/p",
            "cmd",
        ]);
        assert_eq!(
            strip_wrapper(&flagged, Some(ExecStyle::Flag("-e"))),
            args(&["alacritty"])
        );
        // Nothing to strip
        let plain = args(&["foot", "--server"]);
        assert_eq!(strip_wrapper(&plain, Some(ExecStyle::Positional)), plain);
    }

    #[test]
    fn test_wrapper_runs_the_program_in_its_directory_then_becomes_the_shell() {
        // The same words a terminal would exec, run under a real shell
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj dir");
        std::fs::create_dir(&proj).unwrap();
        let log = dir.path().join("log");
        let shell = dir.path().join("fakeshell");
        std::fs::write(
            &shell,
            format!("#!/bin/sh\necho \"shell:$PWD\" >> '{}'\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let program = format!("echo \"prog:$PWD\" >> '{}'", log.display());
        let words: Vec<String> = [
            "sh",
            "-c",
            WRAPPER_SCRIPT,
            WRAPPER_MARKER,
            proj.to_str().unwrap(),
            &program,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let status = std::process::Command::new(&words[0])
            .args(&words[1..])
            .env("SHELL", &shell)
            .status()
            .unwrap();
        assert!(status.success());
        let out = std::fs::read_to_string(&log).unwrap();
        let canon = std::fs::canonicalize(&proj).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // Program first, then the shell, both in the program's directory
        assert_eq!(
            lines,
            vec![
                format!("prog:{}", canon.display()),
                format!("shell:{}", canon.display())
            ]
        );
    }

    #[test]
    fn test_wrapper_survives_program_failure_and_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let shell = dir.path().join("fakeshell");
        std::fs::write(
            &shell,
            format!("#!/bin/sh\necho shell >> '{}'\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let words = [
            "-c",
            WRAPPER_SCRIPT,
            WRAPPER_MARKER,
            "/definitely/not/here",
            "false",
        ];
        let status = std::process::Command::new("sh")
            .args(words)
            .env("SHELL", &shell)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "shell");
    }

    #[test]
    fn test_wrapper_replays_hostile_arguments_verbatim() {
        let argv = args(&["printf", "%s|", "it's", "a b", "$HOME", "`x`", ";"]);
        let command = shell_join(&argv);
        let dir = tempfile::tempdir().unwrap();
        let shell = dir.path().join("fakeshell");
        std::fs::write(&shell, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let out = std::process::Command::new("sh")
            .args(["-c", WRAPPER_SCRIPT, WRAPPER_MARKER, ".", &command])
            .env("SHELL", &shell)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "it's|a b|$HOME|`x`|;|"
        );
    }
}
