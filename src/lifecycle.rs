//! Serialized autosave and restore protection for a compositor instance.
use crate::dispatch::{
    command_socket_path, reply_is_ok, resolve_dialect, send_raw_dispatch, Dialect, DialectChoice,
};
use crate::session::{LocalSession, Session};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

type Result<T> = hyprland::Result<T>;
fn error(message: impl ToString) -> hyprland::error::HyprError {
    hyprland::error::HyprError::Other(message.to_string())
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Control {
    instance: String,
    frozen: bool,
    restored: bool,
    restored_snapshot: Option<String>,
    restore_failed: bool,
    protected_snapshot: Option<String>,
}

pub struct Lifecycle {
    root: PathBuf,
    instance: String,
}

fn lock(path: &Path, nonblocking: bool) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let flags = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    if unsafe { libc::flock(file.as_raw_fd(), flags) } != 0 {
        let e = std::io::Error::last_os_error();
        if nonblocking && e.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(e.into());
    }
    Ok(Some(file))
}

impl Lifecycle {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            root,
            instance: std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default(),
        })
    }

    fn state_lock(&self) -> Result<File> {
        lock(&self.root.join("state.lock"), false)?
            .ok_or_else(|| error("Cannot lock session state"))
    }

    fn control(&self) -> Result<Control> {
        let mut state: Control = match fs::read(self.root.join("control.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(error)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Control::default(),
            Err(e) => return Err(e.into()),
        };
        if state.instance != self.instance && !state.restore_failed {
            state.instance = self.instance.clone();
            state.frozen = false;
            state.restored = false;
            state.restored_snapshot = None;
            // A failed or interrupted restore protects the snapshot across logins.
        }
        Ok(state)
    }

    fn write_control(&self, state: &Control) -> Result<()> {
        use std::io::Write;
        let temp = self.root.join("control.json.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&serde_json::to_vec(state).map_err(error)?)?;
        file.sync_all()?;
        fs::rename(temp, self.root.join("control.json"))?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    fn disabled(&self) -> bool {
        self.root.join("disabled").exists()
    }

    fn secure_snapshot(&self, name: &str) -> Result<()> {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(())
    }

    fn save_locked(&self, session: &LocalSession, name: &str, state: &Control) -> Result<bool> {
        if state.frozen || state.restore_failed || self.disabled() {
            return Ok(false);
        }
        self.secure_snapshot(name)?;
        session.save(name)?;
        self.secure_snapshot(name)?;
        Ok(true)
    }

    pub fn save(&self, session: &LocalSession, name: &str) -> Result<bool> {
        let _lock = self.state_lock()?;
        let state = self.control()?;
        self.save_locked(session, name, &state)
    }

    pub fn restore(
        &self,
        session: &mut LocalSession,
        name: &str,
        choice: DialectChoice,
        startup: bool,
    ) -> Result<()> {
        if session.simulate {
            session.dialect = resolve_dialect(choice, true).map_err(error)?.0;
            return session.load(name);
        }
        let _lock = self.state_lock()?;
        let mut state = self.control()?;
        // Root-wide save protection belongs to the snapshot whose restore failed.
        // Older state without its identity cannot safely authorize any retry.
        if state.restore_failed && state.protected_snapshot.as_deref() != Some(name) {
            return Err(error(
                "Previous restore protects a different or unknown snapshot; refusing retry",
            ));
        }
        // Explicit merge retries are safe in the same instance; automatic startup
        // retries still require a new compositor.
        let retry = state.restore_failed
            && (state.instance != self.instance || (!startup && session.merge));
        if state.restore_failed && !retry {
            return Err(error("Previous restore failed; snapshot remains protected"));
        }
        if retry {
            state.instance = self.instance.clone();
            state.frozen = false;
            state.restored = false;
            state.restored_snapshot = None;
        }
        if state.frozen
            || (startup && state.restored && state.restored_snapshot.as_deref() == Some(name))
        {
            return Ok(());
        }
        let scoped = session.workspace.is_some();
        // Persist before any IPC, dialect probing or process launch. A scoped
        // restore cannot authorize replacement of the complete session snapshot.
        state.restored = false;
        state.restored_snapshot = None;
        state.restore_failed = true;
        state.protected_snapshot = Some(name.to_owned());
        self.write_control(&state)?;
        let snapshot = self.root.join(name).join("clients.json");
        if self.disabled() && (snapshot.exists() || !startup) {
            return Err(error("Cannot restore while disabled"));
        }
        if startup && !snapshot.exists() {
            if retry {
                return Err(error(
                    "Cannot retry previous restore while disabled or without its snapshot",
                ));
            }
            state.restored = false;
            state.restore_failed = scoped;
            state.protected_snapshot = scoped.then(|| name.to_owned());
            return self.write_control(&state);
        }
        if startup {
            session.merge = true;
        }
        session.dialect = resolve_dialect(choice, false).map_err(error)?.0;
        session.load(name)?;
        state.restored = !scoped;
        state.restored_snapshot = (!scoped).then(|| name.to_owned());
        state.restore_failed = scoped;
        state.protected_snapshot = scoped.then(|| name.to_owned());
        self.write_control(&state)
    }

    pub fn daemon(
        &self,
        session: &mut LocalSession,
        name: &str,
        choice: DialectChoice,
        interval: u64,
        startup: bool,
    ) -> Result<()> {
        if self.instance.is_empty() {
            return Err(error("HYPRLAND_INSTANCE_SIGNATURE is not set"));
        }
        let mut key = String::with_capacity(12 + self.instance.len() * 2);
        key.push_str("daemon-");
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in self.instance.bytes() {
            key.push(HEX[(byte >> 4) as usize] as char);
            key.push(HEX[(byte & 15) as usize] as char);
        }
        key.push_str(".lock");
        let Some(_daemon) = lock(&self.root.join(key), true)? else {
            return Ok(());
        };
        if startup {
            self.restore(session, name, choice, true)?;
        }
        loop {
            if !self.save(session, name)? {
                return Ok(());
            }
            for _ in 0..interval {
                std::thread::sleep(Duration::from_secs(1));
                let _lock = self.state_lock()?;
                let state = self.control()?;
                if state.frozen || state.restore_failed || self.disabled() {
                    return Ok(());
                }
            }
        }
    }

    pub fn logout(&self, session: &LocalSession, name: &str, choice: DialectChoice) -> Result<()> {
        if session.simulate {
            return Ok(());
        }
        {
            let _lock = self.state_lock()?;
            let mut state = self.control()?;
            // Persist the freeze before IPC so interruption cannot re-enable autosave.
            let was_frozen = state.frozen;
            state.frozen = true;
            self.write_control(&state)?;
            state.frozen = was_frozen;
            self.save_locked(session, name, &state)?;
        }
        if std::env::var("HYPRTOROID_UWSM").as_deref() == Ok("1") {
            let status = std::process::Command::new("uwsm").arg("stop").status()?;
            if !status.success() {
                return Err(error(format!("uwsm stop failed: {status}")));
            }
            return Ok(());
        }
        let dialect = resolve_dialect(choice, session.simulate).map_err(error)?.0;
        let socket =
            command_socket_path().ok_or_else(|| error("HYPRLAND_INSTANCE_SIGNATURE is not set"))?;
        let command = if dialect == Dialect::Lua {
            "hl.dsp.exit()"
        } else {
            "exit"
        };
        let reply = send_raw_dispatch(&socket, command)?;
        if !reply_is_ok(&reply) {
            return Err(error(format!("Exit dispatch failed: {reply}")));
        }
        Ok(())
    }
}
