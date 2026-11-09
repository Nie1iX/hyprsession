use std::fs;
use std::process::{Command, Output};
use tempfile::TempDir;

fn run(root: &TempDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(args)
        .env("HYPRSESSION_PATH", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_RUNTIME_DIR", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-integration-test")
        .env("HYPRSESSION_NO_DETACH", "1")
        .output()
        .unwrap()
}

#[test]
fn deleting_one_session_preserves_the_other_snapshot() {
    let root = tempfile::tempdir().unwrap();
    for name in ["work", "personal"] {
        fs::create_dir(root.path().join(name)).unwrap();
        fs::write(
            root.path().join(name).join("clients.json"),
            b"protected snapshot",
        )
        .unwrap();
    }
    let deleted = run(&root, &["delete", "work"]);
    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    assert!(!root.path().join("work").exists());
    assert_eq!(
        fs::read(root.path().join("personal/clients.json")).unwrap(),
        b"protected snapshot"
    );
}

#[test]
fn invalid_snapshot_is_not_replaced_after_failed_load_or_save() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("default")).unwrap();
    let path = root.path().join("default/clients.json");
    fs::write(&path, b"{not valid JSON").unwrap();
    assert!(!run(&root, &["load", "--dialect", "lua"]).status.success());
    assert!(run(&root, &["save"]).status.success());
    assert_eq!(fs::read(&path).unwrap(), b"{not valid JSON");
}

#[test]
fn workspace_scope_is_rejected_outside_load_mode() {
    let root = tempfile::tempdir().unwrap();
    for mode in ["save", "clear", "list", "delete"] {
        assert!(
            !run(&root, &[mode, "--workspace", "special:notes", "--simulate"])
                .status
                .success(),
            "{mode}"
        );
    }
}

#[test]
fn bad_resume_policy_preserves_the_snapshot() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("config/hyprsession")).unwrap();
    fs::create_dir(root.path().join("default")).unwrap();
    fs::write(root.path().join("default/clients.json"), b"[]").unwrap();
    fs::write(
        root.path().join("config/hyprsession/resume.toml"),
        "[settings]\nunlisted = \"sometimes\"\n",
    )
    .unwrap();
    let out = run(&root, &["load", "--simulate", "--dialect", "lua"]);
    assert!(!out.status.success());
    let error = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(error.contains("resume.toml:2"), "{error}");
    assert_eq!(
        fs::read(root.path().join("default/clients.json")).unwrap(),
        b"[]"
    );
}

#[test]
fn packed_argv_preserves_arguments_instead_of_using_desktop_defaults() {
    use hyprsession::command_detection::shell_quote;
    use hyprsession::load::{launch_entries_from_saved, placeholder_client};
    use hyprsession::session::SavedClient;
    let root = tempfile::tempdir().unwrap();
    let result = root.path().join("result");
    let command = |value| {
        format!(
            "/bin/sh -c {}",
            shell_quote(&format!(
                "printf {value} > {}",
                shell_quote(result.to_str().unwrap())
            ))
        )
    };
    let mut client = placeholder_client();
    client.class = "packed-command".into();
    client.initial_class = client.class.clone();
    let saved = SavedClient {
        client,
        argv: Some(vec![command("original")]),
        cwd: None,
        foreground: None,
    };
    let desktop = std::collections::HashMap::from([("packed-command".into(), command("desktop"))]);
    let entries = launch_entries_from_saved(&[saved], &desktop);
    assert_eq!(entries.len(), 1);
    assert!(Command::new("/bin/sh")
        .args(["-c", &entries[0].command])
        .status()
        .unwrap()
        .success());
    assert_eq!(fs::read_to_string(result).unwrap(), "original");
}
