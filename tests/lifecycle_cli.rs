use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn frozen_root() -> TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("default")).unwrap();
    fs::write(
        root.path().join("default/clients.json"),
        b"protected snapshot",
    )
    .unwrap();
    fs::write(root.path().join("control.json"),
        br#"{"instance":"isolated-lifecycle-test","frozen":true,"restored":false,"restore_failed":false}"#).unwrap();
    root
}

fn assert_frozen_mode(mode: &str) {
    let root = frozen_root();
    let before = fs::read(root.path().join("default/clients.json")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .arg(mode)
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{mode} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(root.path().join("default/clients.json")).unwrap(),
        before
    );
}

#[test]
fn successful_named_load_does_not_authorize_another_startup_snapshot() {
    use std::sync::atomic::Ordering;
    let root = frozen_root();
    fs::remove_file(root.path().join("control.json")).unwrap();
    fs::create_dir(root.path().join("work")).unwrap();
    fs::write(root.path().join("work/clients.json"), b"[]").unwrap();
    fs::create_dir(root.path().join("personal")).unwrap();
    fs::write(
        root.path().join("personal/clients.json"),
        b"invalid personal snapshot",
    )
    .unwrap();
    let (stop, server) = serve_foreign_client(&root);
    let load = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(["load", "work", "--merge", "--dialect", "lua"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert!(
        load.status.success(),
        "{}",
        String::from_utf8_lossy(&load.stderr)
    );
    let startup = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(["default", "personal", "--dialect", "lua"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(!startup.status.success());
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("control.json")).unwrap()).unwrap();
    assert_eq!(state["restore_failed"], true);
    assert_eq!(state["protected_snapshot"], "personal");
    assert_eq!(
        fs::read(root.path().join("personal/clients.json")).unwrap(),
        b"invalid personal snapshot"
    );
}

#[test]
fn disabled_existing_snapshot_stays_protected_after_reenable() {
    let root = frozen_root();
    fs::remove_file(root.path().join("control.json")).unwrap();
    fs::write(root.path().join("disabled"), b"").unwrap();
    let startup = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(["default", "--dialect", "lua"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(!startup.status.success());
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("control.json")).unwrap()).unwrap();
    assert_eq!(state["restore_failed"], true);
    assert_ne!(state["restored"], true);
    assert_eq!(state["protected_snapshot"], "default");
    fs::remove_file(root.path().join("disabled")).unwrap();
    let save = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .arg("save")
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(save.status.success());
    assert_eq!(
        fs::read(root.path().join("default/clients.json")).unwrap(),
        b"protected snapshot"
    );
}

#[test]
fn frozen_save_preserves_snapshot_without_ipc() {
    assert_frozen_mode("save");
}

#[test]
fn frozen_daemon_exits_without_touching_snapshot() {
    assert_frozen_mode("daemon");
}

#[test]
fn frozen_default_exits_without_touching_snapshot() {
    assert_frozen_mode("default");
}

#[test]
fn prior_failed_restore_blocks_save_across_instances() {
    let root = frozen_root();
    fs::write(root.path().join("control.json"),
        br#"{"instance":"previous-instance","frozen":false,"restored":false,"restore_failed":true}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .arg("save")
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(root.path().join("default/clients.json")).unwrap(),
        b"protected snapshot"
    );
}

#[test]
fn failed_restore_probe_persists_protection_before_ipc() {
    let root = frozen_root();
    fs::remove_file(root.path().join("control.json")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .arg("load")
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("control.json")).unwrap()).unwrap();
    assert_eq!(state["restore_failed"], true);
    assert_eq!(
        fs::read(root.path().join("default/clients.json")).unwrap(),
        b"protected snapshot"
    );
}

#[test]
fn fresh_instance_retries_failed_restore_without_authorizing_partial_apps() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};
    let root = frozen_root();
    fs::write(root.path().join("control.json"),
        br#"{"instance":"previous-instance","frozen":false,"restored":false,"restore_failed":true,"protected_snapshot":"default"}"#).unwrap();
    let socket_dir = root.path().join("hypr/isolated-lifecycle-test");
    fs::create_dir_all(&socket_dir).unwrap();
    let listener = UnixListener::bind(socket_dir.join(".socket.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut query = [0; 256];
                    let _ = stream.read(&mut query);
                    let _ = stream.write_all(br#"[{"address":"0x123","at":[0,0],"size":[640,480],"workspace":{"address":"1","id":1,"type":"normal","name":"1"},"monitor":0,"class":"kitty","initialClass":"kitty","title":"partial restore","initialTitle":"partial restore","pid":123,"mapped":true,"floating":false,"pinned":false,"fullscreen":0}]"#);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("fake IPC failed: {e}"),
            }
        }
    });
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(["default", "--dialect", "lua"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    stopped.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert!(!output.status.success());
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("control.json")).unwrap()).unwrap();
    assert_eq!(
        state["instance"], "isolated-lifecycle-test",
        "fresh login never attempted its protected retry"
    );
    assert_eq!(state["restore_failed"], true);
    assert_ne!(state["restored"], true);
    assert_eq!(
        fs::read(root.path().join("default/clients.json")).unwrap(),
        b"protected snapshot"
    );
}

#[test]
fn scoped_load_cannot_authorize_global_autosave() {
    let root = frozen_root();
    fs::remove_file(root.path().join("control.json")).unwrap();
    fs::write(root.path().join("disabled"), b"").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(["load", "--workspace", "2", "--dialect", "lua"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("control.json")).unwrap()).unwrap();
    assert_ne!(state["restored"], true);
    assert!(state["restore_failed"] == true || state["frozen"] == true);
}

#[test]
fn simulated_logout_does_not_mutate_snapshot_or_control() {
    let root = frozen_root();
    fs::write(root.path().join("control.json"),
        br#"{"instance":"isolated-lifecycle-test","frozen":false,"restored":true,"restore_failed":false}"#).unwrap();
    let before = fs::read(root.path().join("control.json")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(["logout", "--simulate", "--dialect", "lua"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(root.path().join("control.json")).unwrap(), before);
    assert_eq!(
        fs::read(root.path().join("default/clients.json")).unwrap(),
        b"protected snapshot"
    );
}

#[test]
fn failed_named_restore_rejects_other_snapshot_before_retry() {
    let root = frozen_root();
    fs::write(root.path().join("control.json"),
        br#"{"instance":"previous-instance","frozen":false,"restored":false,"restore_failed":true,"protected_snapshot":"work"}"#).unwrap();
    fs::create_dir(root.path().join("personal")).unwrap();
    fs::write(root.path().join("personal/clients.json"), b"[]").unwrap();
    let before = fs::read(root.path().join("control.json")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(["load", "personal", "--dialect", "lua"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        fs::read(root.path().join("control.json")).unwrap(),
        before,
        "retrying another snapshot must not change the failed snapshot protection"
    );
}

#[test]
fn failed_restore_without_snapshot_metadata_cannot_retry() {
    let root = frozen_root();
    fs::write(root.path().join("control.json"),
        br#"{"instance":"previous-instance","frozen":false,"restored":false,"restore_failed":true}"#).unwrap();
    let before = fs::read(root.path().join("control.json")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(["load", "--dialect", "lua"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        fs::read(root.path().join("control.json")).unwrap(),
        before,
        "unknown protected snapshot must not be guessed from the retry name"
    );
}

fn serve_clients_once(root: &TempDir, clients: serde_json::Value) -> std::thread::JoinHandle<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    let directory = root.path().join("hypr/isolated-lifecycle-test");
    fs::create_dir_all(&directory).unwrap();
    let listener = UnixListener::bind(directory.join(".socket.sock")).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut query = [0; 9];
        stream.read_exact(&mut query).unwrap();
        stream
            .write_all(&serde_json::to_vec(&clients).unwrap())
            .unwrap();
    })
}

fn mapped_client(class: &str) -> serde_json::Value {
    serde_json::json!({
        "address":"0x123", "at":[0,0], "size":[640,480],
        "workspace":{"address":"1","id":1,"type":"normal","name":"1"},
        "monitor":0, "class":class, "initialClass":class,
        "title":"startup window", "initialTitle":"startup window",
        "pid":std::process::id(), "mapped":true, "floating":false,
        "pinned":false, "fullscreen":0
    })
}

fn serve_foreign_client(
    root: &TempDir,
) -> (
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<()>,
) {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};
    let directory = root.path().join("hypr/isolated-lifecycle-test");
    fs::create_dir_all(&directory).unwrap();
    let listener = UnixListener::bind(directory.join(".socket.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !stopped.load(Ordering::Relaxed) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut query = [0; 256];
                    let n = stream.read(&mut query).unwrap();
                    let query = String::from_utf8_lossy(&query[..n]);
                    let reply = if query.contains("activeworkspace") {
                        serde_json::json!({"address":"1","id":1,"type":"normal","name":"1"})
                            .to_string()
                    } else if query.contains("clients") {
                        serde_json::json!([mapped_client("foreign")]).to_string()
                    } else if query.contains("cursorpos") {
                        r#"{"x":0,"y":0}"#.to_owned()
                    } else {
                        "ok".to_owned()
                    };
                    let _ = stream.write_all(reply.as_bytes());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("fake IPC failed: {e}"),
            }
        }
    });
    (stop, server)
}

fn incomplete_merge(root: &TempDir, args: &[&str]) {
    use std::sync::atomic::Ordering;
    let snapshot =
        serde_json::to_vec(&serde_json::json!([mapped_client("missing-saved-app")])).unwrap();
    fs::write(root.path().join("default/clients.json"), &snapshot).unwrap();
    let (stop, server) = serve_foreign_client(root);
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(args)
        .args(["--dialect", "lua", "--load-time", "1"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("saved windows not matched"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("control.json")).unwrap()).unwrap();
    assert_eq!(state["restore_failed"], true);
    assert_ne!(state["restored"], true);
    assert_eq!(state["protected_snapshot"], "default");
    let save = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .arg("save")
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(save.status.success());
    assert_eq!(
        fs::read(root.path().join("default/clients.json")).unwrap(),
        snapshot
    );
}

#[test]
fn startup_merges_foreign_window_and_protects_incomplete_restore() {
    let root = frozen_root();
    fs::remove_file(root.path().join("control.json")).unwrap();
    incomplete_merge(&root, &["default"]);
}

#[test]
fn explicit_merge_retries_same_instance_protected_snapshot() {
    let root = frozen_root();
    fs::write(root.path().join("control.json"), br#"{"instance":"isolated-lifecycle-test","frozen":false,"restored":false,"restore_failed":true,"protected_snapshot":"default"}"#).unwrap();
    incomplete_merge(&root, &["load", "--merge"]);
}

#[test]
fn same_instance_merge_cannot_retry_a_different_protected_snapshot() {
    let root = frozen_root();
    fs::write(root.path().join("control.json"), br#"{"instance":"isolated-lifecycle-test","frozen":false,"restored":false,"restore_failed":true,"protected_snapshot":"work"}"#).unwrap();
    let before = fs::read(root.path().join("control.json")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(["load", "--merge", "--dialect", "lua"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read(root.path().join("control.json")).unwrap(), before);
}

#[test]
fn manual_load_still_refuses_foreign_windows() {
    let root = frozen_root();
    fs::remove_file(root.path().join("control.json")).unwrap();
    fs::write(root.path().join("default/clients.json"), b"[]").unwrap();
    let (stop, server) = serve_foreign_client(&root);
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(["load", "--dialect", "lua"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Refusing to restore over existing windows"));
}

#[test]
fn merge_option_rejects_non_load_and_adjust_only() {
    let root = frozen_root();
    for args in [
        ["save", "--merge", "--dialect", "lua"],
        ["load", "--merge", "--adjust-clients-only", ""],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
            .args(args.into_iter().filter(|s| !s.is_empty()))
            .env("HYPRSESSION_PATH", root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
}

#[test]
fn saving_keeps_settings_but_not_the_wallet_unlock_agent() {
    let root = frozen_root();
    fs::remove_file(root.path().join("control.json")).unwrap();
    let server = serve_clients_once(
        &root,
        serde_json::json!([
            mapped_client("org.kde.ksecretd"),
            mapped_client("dev.noctalia.Noctalia")
        ]),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .arg("save")
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("XDG_RUNTIME_DIR", root.path())
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(root.path().join("default/clients.json")).unwrap())
            .unwrap();
    assert_eq!(
        saved
            .iter()
            .map(|c| c["class"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["dev.noctalia.Noctalia"]
    );
}

#[test]
fn failed_uwsm_stop_keeps_the_final_snapshot_frozen() {
    use std::os::unix::fs::PermissionsExt;
    let root = frozen_root();
    fs::write(root.path().join("control.json"),
        br#"{"instance":"isolated-lifecycle-test","frozen":false,"restored":true,"restore_failed":false}"#).unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("uwsm"), "#!/bin/sh\nexit 23\n").unwrap();
    fs::set_permissions(bin.join("uwsm"), fs::Permissions::from_mode(0o755)).unwrap();
    let server = serve_clients_once(
        &root,
        serde_json::json!([mapped_client("dev.noctalia.Noctalia")]),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_hyprsession"))
        .args(["logout", "--dialect", "lua"])
        .env("HYPRSESSION_PATH", root.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "isolated-lifecycle-test")
        .env("HYPRTOROID_UWSM", "1")
        .env("XDG_RUNTIME_DIR", root.path())
        .env("PATH", bin)
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("uwsm stop failed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("control.json")).unwrap()).unwrap();
    assert_eq!(state["frozen"], true);
    let saved: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(root.path().join("default/clients.json")).unwrap())
            .unwrap();
    assert_eq!(saved[0]["class"], "dev.noctalia.Noctalia");
}
