use std::borrow::Cow;
use std::collections::HashSet;
use std::process::Command;

fn busctl(args: &[&str]) -> Option<serde_json::Value> {
    let output = Command::new("/usr/bin/busctl")
        .args([
            "--user",
            "--auto-start=no",
            "--timeout=2",
            "--json=short",
            "--",
        ])
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

pub(crate) fn activate_owned_window(
    mapped_pids: &HashSet<i32>,
    mut owns: impl FnMut(i32) -> bool,
) -> bool {
    let Some(items) = busctl(&[
        "get-property",
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
        "RegisteredStatusNotifierItems",
    ]) else {
        return false;
    };
    let Some(items) = items.get("data").and_then(|data| data.as_array()) else {
        return false;
    };

    let mut candidate: Option<(Cow<'_, str>, String)> = None;
    for item in items {
        let Some(item) = item.as_str() else {
            return false;
        };
        let (service, path) = match item.split_once('/') {
            Some((service, path)) => (service, format!("/{path}")),
            None => (item, "/StatusNotifierItem".to_string()),
        };
        if service.is_empty() {
            return false;
        }
        // Pin well-known names to their current connection before checking its PID.
        let service = if service.starts_with(':') {
            Cow::Borrowed(service)
        } else {
            let Some(owner) = busctl(&[
                "call",
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                "s",
                service,
            ])
            .and_then(|reply| reply.get("data")?.get(0)?.as_str().map(str::to_owned)) else {
                return false;
            };
            Cow::Owned(owner)
        };
        let Some(pid) = busctl(&[
            "call",
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetConnectionUnixProcessID",
            "s",
            &service,
        ])
        .and_then(|reply| reply.get("data")?.get(0)?.as_u64())
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0) else {
            return false;
        };
        if mapped_pids.contains(&pid) || !owns(pid) {
            continue;
        }
        if let Some((previous_service, previous_path)) = &candidate {
            if *previous_service != service || *previous_path != path {
                return false;
            }
        } else {
            candidate = Some((service, path));
        }
    }

    let Some((service, path)) = candidate else {
        return false;
    };
    Command::new("/usr/bin/busctl")
        .args(["--user", "--auto-start=no", "--timeout=2", "--"])
        .args([
            "call",
            &service,
            &path,
            "org.kde.StatusNotifierItem",
            "Activate",
            "ii",
            "0",
            "0",
        ])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}
