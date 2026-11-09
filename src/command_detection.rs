use crate::command_faker::xdg_id_key;
use crate::ipc::Client;
use std::collections::HashMap;
use std::fs;
use std::process::Command;

/// Check if a command exists in PATH using 'which'
pub fn command_exists_in_path(command: &str) -> bool {
    if command.is_empty() {
        return false;
    }

    Command::new("which")
        .arg(command.split_whitespace().next().unwrap())
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Split raw `/proc/<pid>/cmdline` bytes into argv (NUL separated, NUL terminated)
pub fn parse_cmdline(raw: &[u8]) -> Vec<String> {
    let raw = raw.strip_suffix(&[0]).unwrap_or(raw);
    if raw.is_empty() {
        return vec![];
    }
    raw.split(|&b| b == 0)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

/// The process argv, split on NUL before any name extraction
pub fn read_argv(pid: i32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
    let argv = parse_cmdline(&raw);
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

/// The process working directory
pub fn read_cwd(pid: i32) -> Option<String> {
    fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Quote one argument for a POSIX shell; arguments of safe characters stay bare
pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Split a command line into words as a POSIX shell would for plain words: whitespace
/// separates, single quotes are literal, double quotes allow backslash escapes, a bare
/// backslash escapes the next character. The inverse of `shell_join`. No expansion.
pub fn shell_split(line: &str) -> Vec<String> {
    let mut words = vec![];
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(e @ ('"' | '\\' | '$' | '`')) => word.push(e),
                            Some(e) => {
                                word.push('\\');
                                word.push(e);
                            }
                            None => word.push('\\'),
                        },
                        q => word.push(q),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(e) = chars.next() {
                    word.push(e);
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Join argv into a shell command line, quoting where needed
pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build a launch command from argv: the binary name (path dropped) then quoted arguments
pub fn command_from_argv(argv: &[String]) -> Option<String> {
    let first = argv.first()?;
    let binary = first.rsplit('/').next().unwrap_or(first);
    let mut parts = vec![binary.to_string()];
    parts.extend(argv.iter().skip(1).cloned());
    Some(shell_join(&parts))
}

fn handle_proc_cmdline(client: &Client) -> Result<String, std::io::Error> {
    read_argv(client.pid)
        .and_then(|argv| command_from_argv(&argv))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "Cmdline not found"))
}

fn handle_proc_exe(client: &Client) -> Result<String, std::io::Error> {
    let exe_path = format!("/proc/{}/exe", client.pid);
    if let Ok(exe_target) = fs::read_link(&exe_path) {
        if let Some(exe_name) = exe_target.file_name() {
            return Ok(exe_name.to_string_lossy().to_string());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Exe not found",
    ))
}

fn handle_initial_class(client: &Client) -> Result<String, std::io::Error> {
    Ok(client.initial_class.to_lowercase())
}

/// Look up a window class as a desktop file id (e.g. `dev.vencord.vesktop`)
fn resolve_class_by_app_id(class: &str, xdg_map: &HashMap<String, String>) -> Option<String> {
    if class.is_empty() {
        return None;
    }
    xdg_map.get(&xdg_id_key(class)).cloned()
}

/// The desktop-entry command for a window, resolved the way `fetch_command` resolves
/// it: desktop file id by class, then entry name by class, then entry name by title
pub fn resolve_desktop_command(
    client: &Client,
    xdg_map: &HashMap<String, String>,
) -> Option<String> {
    resolve_class_by_app_id(&client.initial_class, xdg_map)
        .or_else(|| resolve_class_by_app_id(&client.class, xdg_map))
        .or_else(|| xdg_map.get(&client.initial_class.to_lowercase()).cloned())
        .or_else(|| xdg_map.get(&client.initial_title.to_lowercase()).cloned())
}

fn handle_initial_title(client: &Client) -> Result<String, std::io::Error> {
    Ok(client.initial_title.to_lowercase())
}

/// Fetch command for a Hyprland client using multiple detection methods
pub fn fetch_command(
    client: &Client,
    xdg_map: &HashMap<String, String>,
) -> Result<String, std::io::Error> {
    type Handler = fn(&Client) -> Result<String, std::io::Error>;
    // (handler, whether the step also consults the desktop file id index)
    let handlers: [(Handler, bool); 4] = [
        (handle_proc_cmdline, false),
        (handle_proc_exe, false),
        (handle_initial_class, true),
        (handle_initial_title, false),
    ];

    for (handler, by_app_id) in handlers {
        if by_app_id {
            if let Some(command) = resolve_class_by_app_id(&client.initial_class, xdg_map) {
                return Ok(command);
            }
        }
        if let Ok(command) = handler(client) {
            if command_exists_in_path(&command) {
                return Ok(command);
            } else {
                // Fallback: check in xdg_map
                if let Some(xdg_command) = xdg_map.get(&command) {
                    return Ok(xdg_command.clone());
                }
            }
        }
    }

    // Fallback to cmdline even if not in PATH
    handle_proc_cmdline(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_split_inverts_shell_join() {
        let argv: Vec<String> = ["claude", "--continue", "it's a \"q\"", "", "a b", "$x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(shell_split(&shell_join(&argv)), argv);
        assert_eq!(
            shell_split("  claude   --continue "),
            vec!["claude", "--continue"]
        );
        assert_eq!(
            shell_split(r#"a "b \"c\"" d\ e"#),
            vec!["a", "b \"c\"", "d e"]
        );
        assert!(shell_split("   ").is_empty());
    }

    #[test]
    fn test_parse_cmdline_splits_on_nul() {
        assert_eq!(
            parse_cmdline(b"omawrite\0Job search activity.md\0"),
            vec!["omawrite".to_string(), "Job search activity.md".to_string()]
        );
        assert_eq!(
            parse_cmdline(b"a\0\0b\0"),
            vec!["a".to_string(), "".to_string(), "b".to_string()]
        );
        assert!(parse_cmdline(b"").is_empty());
    }

    #[test]
    fn test_argv_round_trips_through_shell_quoting() {
        let argv: Vec<String> = [
            "/usr/bin/omawrite",
            "Job search activity.md",
            "it's",
            "a\"b",
            "plain-1.x",
            "",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let cmd = command_from_argv(&argv).unwrap();
        assert_eq!(
            cmd,
            r#"omawrite 'Job search activity.md' 'it'\''s' 'a"b' plain-1.x ''"#
        );

        // The quoted command line parses back to the original argv under a real shell
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\0' {}", cmd.split_once(' ').unwrap().1))
            .output()
            .unwrap();
        assert_eq!(parse_cmdline(&out.stdout), argv[1..].to_vec());
    }

    #[test]
    fn test_shell_quote() {
        assert_eq!(shell_quote("simple"), "simple");
        assert_eq!(shell_quote("--flag=/a/b"), "--flag=/a/b");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn test_read_own_argv_and_cwd() {
        let pid = std::process::id() as i32;
        let argv = read_argv(pid).unwrap();
        assert!(!argv.is_empty());
        assert_eq!(
            read_cwd(pid).unwrap(),
            std::env::current_dir().unwrap().to_string_lossy()
        );
    }

    #[test]
    fn test_class_resolves_to_own_desktop_entry() {
        // Vesktop-style: class is the desktop file id, title is a stale webapp's name
        let mut map = HashMap::new();
        map.insert(xdg_id_key("dev.vencord.vesktop"), "vesktop".to_string());
        map.insert(
            "discord".to_string(),
            "chromium --app=https://discord.com".to_string(),
        );

        assert_eq!(
            resolve_class_by_app_id("dev.vencord.Vesktop", &map),
            Some("vesktop".to_string())
        );
        assert_eq!(resolve_class_by_app_id("unknown.app", &map), None);
        assert_eq!(resolve_class_by_app_id("", &map), None);
        // Name-keyed entries are not reachable through the class/id index
        assert_eq!(resolve_class_by_app_id("discord", &map), None);
    }
}
