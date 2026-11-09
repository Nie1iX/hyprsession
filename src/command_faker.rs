use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub fn fake_command(name: &str, command: &str) -> Result<(), Box<dyn std::error::Error>> {
    let home_dir = std::env::var("HOME")?;
    let bin_path = PathBuf::from(home_dir).join(".local/bin");

    // Create directory if it doesn't exist
    fs::create_dir_all(&bin_path)?;

    let file_path = bin_path.join(name);
    if !command.is_empty() {
        // Write the command to the file with shebang
        let content = format!("#!/bin/sh\n{}", command);
        fs::write(&file_path, content)?;

        // Make the file executable
        let mut perms = fs::metadata(&file_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&file_path, perms)?;
    } else {
        // Remove the file if command is empty
        if file_path.exists() {
            fs::remove_file(&file_path)?;
        }
    }
    Ok(())
}

/// Tokens a Flatpak export's Exec line carries for the launching desktop environment
/// (`flatpak run ... @@u %U @@`); they are never valid in a shell command line
const LAUNCHER_MARKERS: [&str; 3] = ["@@", "@@u", "@@f"];

/// Parse a .desktop file and extract the application name and command
fn parse_desktop_file(path: &PathBuf) -> Option<(String, String)> {
    let content = fs::read_to_string(path).ok()?;
    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut in_desktop_entry = false;

    for line in content.lines() {
        let line = line.trim();

        if line == "[Desktop Entry]" {
            in_desktop_entry = true;
            continue;
        } else if line.starts_with('[') {
            in_desktop_entry = false;
            continue;
        }

        if !in_desktop_entry {
            continue;
        }

        if let Some(value) = line.strip_prefix("Name=") {
            name = Some(value.to_lowercase().to_string());
        } else if let Some(value) = line.strip_prefix("Exec=") {
            // Remove field codes like %f, %F, %u, %U, etc.
            let cleaned = value
                .split_whitespace()
                .filter(|s| !s.starts_with('%') && !LAUNCHER_MARKERS.contains(s))
                .collect::<Vec<_>>()
                .join(" ");
            exec = Some(cleaned);
        }

        // Stop early if we have both
        if name.is_some() && exec.is_some() {
            break;
        }
    }

    match (name, exec) {
        (Some(n), Some(e)) if !e.is_empty() => Some((n, e)),
        _ => None,
    }
}

/// Get XDG desktop file directories in priority order
fn get_desktop_file_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // User-specific directory (highest priority)
    if let Ok(data_home) = std::env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("applications"));
    } else if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }

    let user_data = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")));

    // System directories
    if let Ok(data_dirs) = std::env::var("XDG_DATA_DIRS") {
        for dir in data_dirs.split(':') {
            if !dir.is_empty() {
                dirs.push(PathBuf::from(dir).join("applications"));
            }
        }
    } else {
        // Default system directories
        dirs.push(PathBuf::from("/usr/local/share/applications"));
        dirs.push(PathBuf::from("/usr/share/applications"));
    }

    // Flatpak's exported entries, last so user and system entries win. They are only on
    // XDG_DATA_DIRS when the environment happens to include them.
    for dir in flatpak_export_dirs(user_data.ok().as_deref()) {
        if dir.exists() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }

    dirs
}

/// Where Flatpak exports desktop entries: the user installation, then the system one
fn flatpak_export_dirs(user_data_home: Option<&std::path::Path>) -> Vec<PathBuf> {
    let mut dirs = vec![];
    if let Some(data_home) = user_data_home {
        dirs.push(data_home.join("flatpak/exports/share/applications"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    dirs
}

/// Prefix marking map keys derived from a desktop file id (filename stem), so
/// that id lookups never shadow or get confused with name-keyed entries.
const APP_ID_KEY_PREFIX: &str = "id:";

/// Key under which an application's desktop file id is indexed in the xdg map
pub fn xdg_id_key(app_id: &str) -> String {
    format!("{}{}", APP_ID_KEY_PREFIX, app_id.to_lowercase())
}

/// Create a map between application names and their commands from XDG .desktop files
/// Returns a HashMap where keys are application names (lowercased) and values are the
/// commands to run them. Each entry is also indexed by its desktop file id (filename
/// stem, lowercased) under `xdg_id_key`.
pub fn build_xdg_command_map() -> HashMap<String, String> {
    build_xdg_command_map_from(&get_desktop_file_dirs())
}

/// As `build_xdg_command_map`, but reading from the given directories (highest priority first)
pub fn build_xdg_command_map_from(dirs: &[PathBuf]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut ids: Vec<(String, String)> = Vec::new();

    for dir in dirs {
        if !dir.exists() {
            continue;
        }

        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();

                // Only process .desktop files
                if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
                    continue;
                }

                if let Some((name, command)) = parse_desktop_file(&path) {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        ids.push((xdg_id_key(stem), command.clone()));
                    }
                    // Only add if not already present (respects priority order)
                    map.entry(name).or_insert(command);
                }
            }
        }
    }

    for (key, command) in ids {
        map.entry(key).or_insert(command);
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_desktop_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("test.desktop");

        let content = r#"[Desktop Entry]
Name=Test Application
Exec=test-app --flag %f
Type=Application
Icon=test-icon
"#;
        fs::write(&test_file, content).unwrap();

        let result = parse_desktop_file(&test_file);
        let (name, command) = result.unwrap();
        assert_eq!(name, "test application");
        assert_eq!(command, "test-app --flag");
    }

    #[test]
    fn test_xdg_map_indexes_by_desktop_id() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("dev.vencord.Vesktop.desktop"),
            "[Desktop Entry]\nName=Vesktop\nExec=vesktop-launcher %U\n",
        )
        .unwrap();
        // A stale entry whose Name collides with what the title step would look up
        fs::write(
            dir.path().join("discord-webapp.desktop"),
            "[Desktop Entry]\nName=Discord\nExec=chromium --app=https://discord.com\n",
        )
        .unwrap();

        let map = build_xdg_command_map_from(&[dir.path().to_path_buf()]);
        assert_eq!(
            map.get(&xdg_id_key("dev.vencord.vesktop"))
                .map(String::as_str),
            Some("vesktop-launcher")
        );
        assert_eq!(
            map.get("vesktop").map(String::as_str),
            Some("vesktop-launcher")
        );
        assert_eq!(
            map.get("discord").map(String::as_str),
            Some("chromium --app=https://discord.com")
        );
        // Id keys never appear as plain name keys
        assert!(!map.contains_key("dev.vencord.vesktop"));
    }

    #[test]
    fn test_xdg_name_keys_win_over_id_keys() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.desktop"),
            "[Desktop Entry]\nName=b\nExec=one\n",
        )
        .unwrap();
        let map = build_xdg_command_map_from(&[dir.path().to_path_buf()]);
        assert_eq!(map.get("b").map(String::as_str), Some("one"));
        assert_eq!(map.get(&xdg_id_key("A")).map(String::as_str), Some("one"));
    }

    #[test]
    fn test_flatpak_export_entry_resolves_with_markers_stripped() {
        let user = tempfile::tempdir().unwrap();
        let export = tempfile::tempdir().unwrap();
        fs::write(
            export.path().join("dev.vencord.Vesktop.desktop"),
            "[Desktop Entry]\nName=Vesktop\nExec=/usr/bin/flatpak run --branch=stable --arch=x86_64 --command=vesktop --file-forwarding dev.vencord.Vesktop @@u %U @@\n",
        ).unwrap();
        // A user entry and an export entry with the same id: the user one wins
        fs::write(
            user.path().join("org.example.App.desktop"),
            "[Desktop Entry]\nName=App\nExec=user-app\n",
        )
        .unwrap();
        fs::write(
            export.path().join("org.example.App.desktop"),
            "[Desktop Entry]\nName=App\nExec=export-app @@f %f @@\n",
        )
        .unwrap();

        let map =
            build_xdg_command_map_from(&[user.path().to_path_buf(), export.path().to_path_buf()]);
        assert_eq!(
            map.get(&xdg_id_key("dev.vencord.vesktop")).map(String::as_str),
            Some("/usr/bin/flatpak run --branch=stable --arch=x86_64 --command=vesktop --file-forwarding dev.vencord.Vesktop")
        );
        assert_eq!(
            map.get(&xdg_id_key("org.example.app")).map(String::as_str),
            Some("user-app")
        );
        assert_eq!(map.get("app").map(String::as_str), Some("user-app"));
    }

    #[test]
    fn test_launcher_markers_are_dropped_from_exec() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            "[Desktop Entry]\nName=X\nExec=run x @@u %U @@ @@f --keep\n",
        )
        .unwrap();
        let (_, command) = parse_desktop_file(&file.path().to_path_buf()).unwrap();
        assert_eq!(command, "run x --keep");
    }

    #[test]
    fn test_flatpak_export_dirs_are_user_then_system() {
        let dirs = flatpak_export_dirs(Some(std::path::Path::new("/home/u/.local/share")));
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/u/.local/share/flatpak/exports/share/applications"),
                PathBuf::from("/var/lib/flatpak/exports/share/applications"),
            ]
        );
    }
}
