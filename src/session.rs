use crate::command_detection::fetch_command;
use crate::dispatch::{Dialect, Dispatcher, Op, WorkspaceTarget};
use crate::ipc::{self, Client, WorkspaceKind};
use crate::load::{
    clear_windows, desired_from_rules, launch_entries_with, launch_order, placeholder_client,
    run_load, ClearConfig, Host, LoadPlan, RealHost, Scope,
};
use crate::procfs::{capture_terminal, ProcFs, ProcTable, RealProc};
use crate::resume::{
    default_resume_path, program_identity, strip_wrapper, terminal_style, Relaunch, ResumeConfig,
};
use hyprland::data::FullscreenMode;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const EXEC_NAME: &str = "exec.conf";
const CLIENTS_PATH_NAME: &str = "clients.json";

pub trait Session {
    fn save(&self, name: &str) -> hyprland::Result<()>;
    fn load(&self, name: &str) -> hyprland::Result<()>;
    fn clear(&self) -> hyprland::Result<()>;
    fn list(&self) -> impl Iterator<Item = String>;
    fn delete(&self, name: &str);
}

macro_rules! iif {
    ($prop:expr, $val:expr) => {
        if $prop {
            $val
        } else {
            ""
        }
    };
    ($prop:expr, $val:expr, $alt:expr) => {
        if $prop {
            $val
        } else {
            $alt
        }
    };
}

macro_rules! to_base_dir {
    ($base_path:expr, $name:expr) => {
        if $name.is_empty() {
            PathBuf::from($base_path)
        } else {
            [$base_path, $name].iter().collect::<PathBuf>()
        }
    };
}

/// A client as stored in clients.json: the live client data plus what is needed to
/// relaunch it. The extra fields are optional because older snapshots lack them.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(try_from = "serde_json::Value", into = "serde_json::Value")]
pub struct SavedClient {
    pub client: Client,
    /// Process argv, split from the raw /proc/<pid>/cmdline
    pub argv: Option<Vec<String>>,
    /// Process working directory, the directory the window is launched from on load
    pub cwd: Option<String>,
    /// For terminal windows: the program running in the foreground of the terminal's tty
    pub foreground: Option<Foreground>,
}

/// The non-shell program that was in the foreground of a terminal when it was saved
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Foreground {
    /// Pid of the foreground process group leader at save time
    pub pid: i32,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    /// The program owns another saved client's window (by pid): relaunch nothing in the terminal
    #[serde(default)]
    pub linked: bool,
}

// The client fields and the extras share one JSON object. serde's flatten can't be
// used for that: it buffers values and the crate's monitor id is an i128.
impl From<SavedClient> for serde_json::Value {
    fn from(saved: SavedClient) -> Self {
        let mut value = serde_json::to_value(&saved.client).expect("Client serializes to JSON");
        let object = value
            .as_object_mut()
            .expect("Client serializes to an object");
        if let Some(argv) = saved.argv {
            object.insert("argv".to_string(), serde_json::json!(argv));
        }
        if let Some(cwd) = saved.cwd {
            object.insert("cwd".to_string(), serde_json::json!(cwd));
        }
        if let Some(foreground) = saved.foreground {
            object.insert("foreground".to_string(), serde_json::json!(foreground));
        }
        value
    }
}

impl TryFrom<serde_json::Value> for SavedClient {
    type Error = serde_json::Error;

    fn try_from(mut value: serde_json::Value) -> Result<Self, Self::Error> {
        // Both extras are optional: snapshots from before they existed lack them
        let (argv, cwd, foreground) = match value.as_object_mut() {
            Some(object) => (
                object
                    .remove("argv")
                    .and_then(|v| serde_json::from_value(v).ok()),
                object
                    .remove("cwd")
                    .and_then(|v| serde_json::from_value(v).ok()),
                object
                    .remove("foreground")
                    .and_then(|v| serde_json::from_value(v).ok()),
            ),
            None => (None, None, None),
        };
        Ok(SavedClient {
            client: serde_json::from_value(value)?,
            argv,
            cwd,
            foreground,
        })
    }
}

impl SavedClient {
    /// Record every window of a snapshot. Terminal windows also get the program in the
    /// foreground of their tty, linked when that program owns another saved window.
    pub fn capture_all(
        clients: &[Client],
        proc: &dyn ProcFs,
        resume: &ResumeConfig,
    ) -> Vec<SavedClient> {
        let mut table: Option<ProcTable> = None;
        let mut captured: Vec<(SavedClient, Vec<i32>)> = vec![];
        for client in clients {
            let mut argv = proc.argv(client.pid);
            let mut cwd = proc.cwd(client.pid);
            let mut foreground = None;
            let mut related_pids = vec![];
            let name = argv
                .as_deref()
                .and_then(program_identity)
                .filter(|n| resume.is_terminal(n))
                .or_else(|| {
                    proc.exe_name(client.pid)
                        .map(|n| n.to_lowercase())
                        .filter(|n| resume.is_terminal(n))
                });
            if let Some(name) = name {
                if clients.iter().filter(|c| c.pid == client.pid).count() > 1 {
                    println!(
                        "Warning: terminal '{}' shares its process with other windows; its foreground program is not recorded",
                        client.title
                    );
                } else {
                    let table = table.get_or_insert_with(|| ProcTable::read(proc));
                    let found = capture_terminal(proc, table, client.pid);
                    if found.ambiguous {
                        println!(
                            "Warning: terminal '{}' runs several shells in one process; its foreground program is not recorded",
                            client.title
                        );
                    }
                    // The shell may have cd'd since the terminal was launched
                    if found.shell_cwd.is_some() {
                        cwd = found.shell_cwd;
                    }
                    if let Some(fg) = found.foreground {
                        related_pids = fg
                            .group_pids
                            .iter()
                            .chain(&fg.descendant_pids)
                            .copied()
                            .collect();
                        foreground = Some(Foreground {
                            pid: fg.pid,
                            argv: fg.argv,
                            cwd: fg.cwd,
                            linked: false,
                        });
                    }
                }
                // A terminal relaunched by hyprsession carries its wrapper: record the terminal itself
                if let Some(argv) = argv.as_mut() {
                    *argv = strip_wrapper(argv, terminal_style(&name));
                }
            }
            captured.push((
                SavedClient {
                    client: client.clone(),
                    argv,
                    cwd,
                    foreground,
                },
                related_pids,
            ));
        }

        // A foreground program that is (or whose process group or descendants include) another
        // saved window's process owns that window: it returns through its own entry
        let pids: HashSet<i32> = clients.iter().map(|c| c.pid).collect();
        captured
            .into_iter()
            .map(|(mut saved, related)| {
                if let Some(fg) = saved.foreground.as_mut() {
                    fg.linked = related
                        .iter()
                        .any(|pid| *pid != saved.client.pid && pids.contains(pid));
                }
                saved
            })
            .collect()
    }
}

/// Order-independent description of a snapshot for comparing two saves: one sorted
/// entry per window of (class, workspace, floating, position, size, argv). Addresses,
/// pids and ordering are deliberately left out.
pub fn fingerprint(clients: &[SavedClient]) -> Vec<String> {
    fingerprint_with(clients, &ResumeConfig::default())
}

/// `fingerprint` for a given resume configuration. A terminal's foreground program counts
/// as what load would run for it (not its raw argv, which a resume command rewrites), and
/// its directory.
pub fn fingerprint_with(clients: &[SavedClient], resume: &ResumeConfig) -> Vec<String> {
    let mut entries: Vec<String> = clients
        .iter()
        .map(|c| {
            let k = &c.client;
            let foreground = match &c.foreground {
                None => "-".to_string(),
                Some(fg) => match resume.decide(fg) {
                    Relaunch::Linked => "linked".to_string(),
                    Relaunch::Table(cmd) | Relaunch::Replay(cmd) => format!("{cmd} @ {:?}", fg.cwd),
                    Relaunch::CleanPrompt(_) => "prompt".to_string(),
                },
            };
            format!(
                "{}|{}|{}|{:?}|{:?}|{:?}|{}",
                k.class,
                k.workspace.selector(),
                k.floating,
                k.at,
                k.size,
                c.argv,
                foreground
            )
        })
        .collect();
    entries.sort();
    entries
}

/// Write a file by writing a sibling temp file then renaming it over the target,
/// so an interrupted save never leaves a truncated snapshot behind
fn write_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Write both snapshot files for a session directory
fn write_snapshot(
    base_dir: &std::path::Path,
    exec_lines: &[String],
    saved: &[SavedClient],
) -> std::io::Result<()> {
    let mut exec = String::new();
    for line in exec_lines {
        exec.push_str(line);
        exec.push('\n');
    }
    write_atomic(&base_dir.join(EXEC_NAME), exec.as_bytes())?;
    write_atomic(
        &base_dir.join(CLIENTS_PATH_NAME),
        &serde_json::to_vec(saved)?,
    )
}

pub struct LocalSession {
    pub base_path: String,
    pub simulate: bool,
    pub load_time: u64,
    pub adjust_clients_only: bool,
    pub merge: bool,
    pub save_duplicate_pids: bool,
    pub dialect: Dialect,
    /// Load only this workspace's slice of the snapshot
    pub workspace: Option<String>,
}

/// Work out which dispatches bring a live window in line with its saved client
pub fn plan_adjustment(real_client: &Client, session_client: &Client) -> Vec<Op> {
    let window = real_client.address.clone();
    let mut ops = vec![];
    if real_client.pinned && !session_client.pinned {
        ops.push(Op::Pin {
            window: window.clone(),
            enable: false,
        });
    }
    let shape_changed = real_client.floating != session_client.floating
        || (session_client.floating
            && (real_client.at != session_client.at || real_client.size != session_client.size));
    let clear_fullscreen = real_client.fullscreen != FullscreenMode::None
        && (shape_changed || real_client.fullscreen != session_client.fullscreen);
    if clear_fullscreen {
        ops.push(Op::Fullscreen {
            window: window.clone(),
            maximized: real_client.fullscreen == FullscreenMode::Maximized,
            enable: false,
        });
    }
    if real_client.workspace.selector() != session_client.workspace.selector() {
        let workspace = WorkspaceTarget::from_workspace(&session_client.workspace);
        ops.push(Op::MoveToWorkspace {
            window: window.clone(),
            workspace,
        });
    }
    if real_client.monitor != session_client.monitor
        && session_client.workspace.kind == WorkspaceKind::Normal
    {
        ops.push(Op::MoveWorkspaceToMonitor {
            workspace: session_client.workspace.selector().to_owned(),
            monitor: session_client.monitor.unwrap_or(0),
        });
    }
    if real_client.floating != session_client.floating {
        ops.push(Op::Float {
            window: window.clone(),
            enable: session_client.floating,
        });
    }
    if session_client.floating && session_client.fullscreen == FullscreenMode::None {
        // Resize preserves the window centre in Hyprland; set its position last.
        ops.push(Op::ResizeWindow {
            window: window.clone(),
            w: session_client.size.0,
            h: session_client.size.1,
        });
        ops.push(Op::MoveWindow {
            window: window.clone(),
            x: session_client.at.0,
            y: session_client.at.1,
        });
    }
    if session_client.pinned && !real_client.pinned {
        ops.push(Op::Pin {
            window: window.clone(),
            enable: true,
        });
    }
    if session_client.fullscreen != FullscreenMode::None
        && (clear_fullscreen || real_client.fullscreen != session_client.fullscreen)
    {
        ops.push(Op::Fullscreen {
            window,
            maximized: session_client.fullscreen == FullscreenMode::Maximized,
            enable: true,
        });
    }
    ops
}

impl LocalSession {
    fn clear_scope(&self, scope: Scope) -> hyprland::Result<()> {
        if !self.simulate {
            let host = RealHost::new(Dispatcher::new(self.dialect, false));
            let report = clear_windows(&host, &ClearConfig::default(), scope.clone())
                .map_err(hyprland::error::HyprError::Other)?;
            if !report.refused.is_empty() {
                println!(
                    "Not closed (shared with another workspace): {}",
                    report.refused.join(", ")
                );
            }
        }

        match scope {
            Scope::All => println!("Cleared existing session"),
            Scope::Workspace(id) => println!("Cleared workspace {id}"),
        }
        Ok(())
    }

    /// Read and validate a snapshot, returning what load will do
    fn prepare_load(&self, base_dir: &std::path::Path) -> hyprland::Result<LoadPlan> {
        // A bad resume file fails here, before anything is touched (and under --simulate)
        let resume = ResumeConfig::load(default_resume_path().as_deref())
            .map_err(hyprland::error::HyprError::Other)?;
        self.prepare_load_with(base_dir, &resume)
    }

    fn prepare_load_with(
        &self,
        base_dir: &std::path::Path,
        resume: &ResumeConfig,
    ) -> hyprland::Result<LoadPlan> {
        let data = std::fs::read(base_dir.join(CLIENTS_PATH_NAME))?;
        let mut saved: Vec<SavedClient> = serde_json::from_slice(&data)?;
        if let Some(workspace) = &self.workspace {
            saved.retain(|c| c.client.workspace.matches_selector(workspace));
            if saved.is_empty() {
                return Err(hyprland::error::HyprError::Other(format!(
                    "No saved windows on workspace {workspace}"
                )));
            }
        }
        let entries = if self.adjust_clients_only {
            vec![]
        } else {
            launch_entries_with(
                &saved,
                &crate::command_faker::build_xdg_command_map(),
                &|dir| dir.is_dir(),
                resume,
            )
        };
        Ok(LoadPlan {
            entries: launch_order(entries),
            saved: saved.into_iter().map(|c| c.client).collect(),
            place_by_class: true,
            tail_ticks: LoadPlan::tail_ticks_for(self.load_time),
            scope: self.workspace.clone(),
            restore_workspace: if self.simulate {
                None
            } else {
                Some(ipc::active_workspace()?.selector().to_owned())
            },
        })
    }
}

/// Print what a load would send. Windows to place do not exist yet, so their
/// dispatches are shown against a placeholder address.
fn simulate_load(dispatcher: &Dispatcher, plan: &LoadPlan) {
    if let Some(workspace) = &plan.scope {
        println!(
            "Scoped to workspace {workspace}: refusing existing windows and launching {} programs",
            plan.entries.len()
        );
    }
    for entry in &plan.entries {
        let _ = dispatcher.send(&Op::Exec(entry.command.clone()));
        if let Some(group) = entry.group.as_ref().filter(|g| g.saved.len() > 1) {
            println!(
                "Expecting {} windows from '{}' (would launch it again up to {} times if short)",
                group.saved.len(),
                group.label,
                group.saved.len() - 1
            );
        }
    }
    if plan.place_by_class {
        println!(
            "After placement, windows matched by no saved client will be listed (not touched)"
        );
        print_placement_order(&plan.saved);
        for wanted in &plan.saved {
            for op in plan_adjustment(&placeholder_client(), wanted) {
                println!(
                    "Would send once the window exists: dispatch {}",
                    dispatcher.render(&op)
                );
            }
        }
    } else {
        for entry in &plan.entries {
            let blank = placeholder_client();
            for op in plan_adjustment(&blank, &desired_from_rules(&blank, &entry.rules)) {
                println!(
                    "Would send once the window exists: dispatch {}",
                    dispatcher.render(&op)
                );
            }
        }
    }
}

/// Say how each workspace's tiled windows will be placed. The computed insertion order depends
/// on the live compositor (layout options and the pointer), so a simulation can only promise the
/// fallback: saved reading order.
fn print_placement_order(saved: &[Client]) {
    let mut workspaces: Vec<&str> = vec![];
    for client in saved.iter().filter(|c| crate::queue::is_queued(c)) {
        let key = crate::queue::ws_key(client);
        if !workspaces.contains(&key) {
            workspaces.push(key);
        }
    }
    workspaces.sort();
    for key in workspaces {
        let rects: Vec<crate::dwindle::Rect> = saved
            .iter()
            .filter(|c| crate::queue::is_queued(c) && crate::queue::ws_key(c) == key)
            .map(|c| {
                (
                    c.at.0 as f64,
                    c.at.1 as f64,
                    f64::from(c.at.0) + f64::from(c.size.0),
                    f64::from(c.at.1) + f64::from(c.size.1),
                )
            })
            .collect();
        let titles: Vec<&str> = saved
            .iter()
            .filter(|c| crate::queue::is_queued(c) && crate::queue::ws_key(c) == key)
            .map(|c| c.title.as_str())
            .collect();
        let reading: Vec<String> = crate::dwindle::reading_order(&rects)
            .into_iter()
            .map(|i| format!("'{}'", titles[i]))
            .collect();
        println!(
            "Workspace {}: its {} tiled windows are placed together once claimed, in the computed dwindle \
             insertion order (decided at load time from the pointer; saved reading order if the tree is \
             unreachable): reading order would be {}",
            key,
            rects.len(),
            reading.join(", ")
        );
    }
}

/// Report how each workspace's tiled windows were ordered, and why any fell back to reading order
fn print_flushes(flushes: &[crate::load::FlushReport]) {
    for flush in flushes {
        let how = match (&flush.fallback, flush.steered) {
            (_, true) => "steered insertion order (pointer-independent)".to_string(),
            (None, false) => "computed insertion order".to_string(),
            (Some(reason), false) => {
                format!("saved reading order ({reason}); the layout may not reproduce")
            }
        };
        println!(
            "Workspace {}: {} tiled windows placed in {how}",
            flush.workspace, flush.windows
        );
        if flush.pointer_moved {
            println!(
                "  the pointer moved during placement; the order may not have been reproduced"
            );
        }
    }
}

/// The final geometry check, one line per workspace, read after every placement was done
fn print_verification(checks: &[crate::load::WorkspaceCheck]) {
    for check in checks {
        if check.at_saved == check.checked {
            println!(
                "Workspace {}: all {} windows match the saved state and geometry",
                check.workspace, check.checked
            );
        } else {
            println!(
                "Warning: workspace {}: only {} of {} windows match the saved state and geometry (off: {})",
                check.workspace,
                check.at_saved,
                check.checked,
                check.off.join(", ")
            );
        }
    }
}

/// Report launch groups that stayed short of their saved window count, with the saves left unmatched
fn print_shortfalls(shortfalls: &[crate::load::GroupShortfall], unmatched: &[Client]) {
    if shortfalls.is_empty() {
        return;
    }
    println!(
        "Launch groups short of their saved window count: {}",
        shortfalls.len()
    );
    for short in shortfalls {
        println!(
            "  '{}': {} of {} windows after {} extra launches",
            short.label, short.got, short.expected, short.reinvocations
        );
    }
    for client in unmatched {
        println!(
            "  unmatched save: '{}' ({}) workspace={}",
            client.title,
            client.class,
            client.workspace.selector()
        );
    }
}

/// List live windows no saved client claimed. They are not touched.
fn print_surplus(surplus: &[Client]) {
    if surplus.is_empty() {
        return;
    }
    println!("Windows not matched to any saved client: {}", surplus.len());
    for window in surplus {
        println!(
            "  surplus {} class='{}' title='{}' workspace={}",
            window.address,
            window.class,
            window.title,
            window.workspace.selector()
        );
    }
}

impl Session for LocalSession {
    fn save(&self, name: &str) -> hyprland::Result<()> {
        println!("Saving session: {}", name);
        let base_dir: PathBuf = to_base_dir!(self.base_path.clone(), name.to_string());
        std::fs::create_dir_all(&base_dir)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base_dir, std::fs::Permissions::from_mode(0o700))?;

        let props = [
            |info: &Client| format!("monitor {:?}", info.monitor.unwrap_or(0)),
            |info: &Client| format!("workspace {} silent", info.workspace.selector()),
            |info: &Client| iif!(info.floating, "float").to_string(),
            |info: &Client| format!("move {} {}", info.at.0, info.at.1),
            |info: &Client| format!("size {} {}", info.size.0, info.size.1),
            |info: &Client| iif!(info.pinned, "pin").to_string(),
            |info: &Client| format!("fullscreenstate {}", info.fullscreen as i32),
        ];

        let mut client_info = ipc::clients()?;
        client_info.retain(ipc::restorable);
        client_info.reverse();

        let mut pids: Vec<i32> = vec![];
        let mut saved_clients: Vec<SavedClient> = vec![];
        let mut exec_lines: Vec<String> = vec![];
        let xdg_map: HashMap<String, String> = crate::command_faker::build_xdg_command_map();

        let resume = ResumeConfig::load(default_resume_path().as_deref()).unwrap_or_else(|e| {
            println!("Warning: {e}; using the built-in resume table");
            ResumeConfig::default()
        });
        let ordered = client_info;
        let captured = SavedClient::capture_all(&ordered, &RealProc, &resume);

        for (info, saved) in ordered.iter().zip(captured) {
            // Every window is recorded in clients.json, whatever happens to its command
            saved_clients.push(saved);
            // exec.conf holds one launch line per distinct process
            if !self.save_duplicate_pids && pids.contains(&info.pid) {
                continue;
            }
            let cmd = fetch_command(info, &xdg_map);
            if !cmd.is_ok() {
                continue;
            }
            pids.push(info.pid);

            let exec_opts: Vec<String> = props
                .iter()
                .map(|opt| opt(info))
                .filter(|opt| !opt.is_empty())
                .collect();
            exec_lines.push(format!("[{}] {}", exec_opts.join(";"), cmd.unwrap()));
        }

        write_snapshot(&base_dir, &exec_lines, &saved_clients)?;
        println!("Session saved");
        Ok(())
    }

    fn load(&self, name: &str) -> hyprland::Result<()> {
        println!("Loading session: {}", name);
        let base_dir: PathBuf = to_base_dir!(self.base_path.clone(), name.to_string());
        let dispatcher = Dispatcher::new(self.dialect, self.simulate);

        // Everything is read and checked before anything is touched: a bad snapshot
        // must fail here, not after clear() has killed the desktop
        let plan = self.prepare_load(&base_dir)?;

        if self.simulate {
            simulate_load(&dispatcher, &plan);
        } else {
            let host = if self.merge {
                RealHost::merged(dispatcher)
            } else {
                RealHost::new(dispatcher)
            };
            if !self.merge
                && !self.adjust_clients_only
                && host.clients()?.iter().any(|c| {
                    ipc::restorable(c)
                        && self
                            .workspace
                            .as_ref()
                            .is_none_or(|ws| c.workspace.matches_selector(ws))
                })
            {
                return Err(hyprland::error::HyprError::Other(
                    "Refusing to restore over existing windows; nothing was closed".into(),
                ));
            }
            let report = run_load(&host, &plan);
            println!(
                "Placed {} windows, {} saved windows not matched",
                report.placed,
                report.unmatched.len()
            );
            print_flushes(&report.flushes);
            print_verification(&report.verification);
            print_shortfalls(&report.shortfalls, &report.unmatched);
            print_surplus(&report.surplus);
            if !report.unmatched.is_empty() || report.incomplete || host.failed() {
                return Err(hyprland::error::HyprError::Other(
                    "Restore was incomplete; preserving the saved snapshot".into(),
                ));
            }
        }

        println!("Finished loading session");
        Ok(())
    }

    fn clear(&self) -> hyprland::Result<()> {
        self.clear_scope(Scope::All)
    }

    fn list(&self) -> impl Iterator<Item = String> {
        let paths = std::fs::read_dir(self.base_path.clone()).unwrap();

        paths
            .filter_map(|path| {
                let entry = path.unwrap();
                if entry.path().is_dir() {
                    Some(entry.file_name())
                } else {
                    None
                }
            })
            .map(|os_str| os_str.to_string_lossy().into_owned())
    }

    fn delete(&self, name: &str) {
        let full_path = format!("{}/{}", self.base_path, name);
        if std::fs::remove_dir_all(&full_path).is_ok() {
            println!("Deleted session: {}", name);
        } else {
            println!("Failed to delete session: {}", name);
        }
    }
}
#[cfg(test)]
pub mod test_support {
    use crate::ipc::Client;

    /// Build a synthetic client; only the fields sessions care about are parameters
    pub fn client(
        class: &str,
        title: &str,
        ws: i32,
        floating: bool,
        at: (i32, i32),
        size: (i32, i32),
    ) -> Client {
        serde_json::from_value(serde_json::json!({
            "address": format!("0x{:x}", (class.len() * 1000 + title.len() * 7 + at.0 as usize) as u32),
            "at": [at.0, at.1],
            "size": [size.0, size.1],
            "workspace": { "address": ws.to_string(), "id": ws, "type": "normal", "name": ws.to_string() },
            "floating": floating,
            "fullscreen": 0,
            "fullscreenClient": 0,
            "monitor": 0,
            "initialClass": class,
            "class": class,
            "initialTitle": title,
            "title": title,
            "pid": 100,
            "xwayland": false,
            "pinned": false,
            "grouped": [],
            "mapped": true,
            "swallowing": null,
            "focusHistoryID": 0
        })).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::client;
    use super::*;
    use crate::ipc::Workspace;

    #[test]
    fn modern_special_snapshot_preserves_address_without_numeric_id() {
        let mut value = serde_json::to_value(SavedClient {
            client: client("kitty", "scratchpad", 1, true, (20, 30), (640, 400)),
            argv: Some(vec!["kitty".into()]),
            cwd: None,
            foreground: None,
        })
        .unwrap();
        value["workspace"] = serde_json::json!({
            "address": "special:scratchpad",
            "type": "special",
            "name": "Notes",
        });
        let decoded = serde_json::from_value::<SavedClient>(value);
        assert!(decoded.is_ok(), "modern workspace rejected: {decoded:?}");
        let saved = serde_json::to_value(decoded.unwrap()).unwrap();
        assert_eq!(saved["workspace"]["address"], "special:scratchpad");
    }

    #[test]
    fn test_saved_client_round_trip_with_argv_and_cwd() {
        let saved = SavedClient {
            client: client("brave", "tab", 2, false, (0, 0), (10, 10)),
            argv: Some(vec!["brave".into(), "--user-data-dir=/x y".into()]),
            cwd: Some("/home/synthetic".into()),
            foreground: None,
        };
        let json = serde_json::to_string(&vec![saved.clone()]).unwrap();
        let back: Vec<SavedClient> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].argv, saved.argv);
        assert_eq!(back[0].cwd, saved.cwd);
        assert_eq!(back[0].client, saved.client);
        // Still readable as a plain client list by older consumers
        let plain: Vec<Client> = serde_json::from_str(&json).unwrap();
        assert_eq!(plain[0], saved.client);
    }

    fn sim_session(dir: &std::path::Path) -> LocalSession {
        LocalSession {
            base_path: dir.to_str().unwrap().to_string(),
            simulate: true,
            load_time: 1,
            adjust_clients_only: false,
            merge: false,
            save_duplicate_pids: false,
            dialect: Dialect::Lua,
            workspace: None,
        }
    }

    fn scoped_session(dir: &std::path::Path, workspace: i32) -> LocalSession {
        LocalSession {
            workspace: Some(workspace.to_string()),
            ..sim_session(dir)
        }
    }

    fn write_clients(dir: &std::path::Path, clients: &[SavedClient]) {
        std::fs::write(
            dir.join(CLIENTS_PATH_NAME),
            serde_json::to_vec(clients).unwrap(),
        )
        .unwrap();
    }

    fn with_argv(class: &str, argv: &[&str], ws: i32, at: (i32, i32)) -> SavedClient {
        SavedClient {
            client: client(class, "t", ws, false, at, (10, 10)),
            argv: Some(argv.iter().map(|s| s.to_string()).collect()),
            cwd: None,
            foreground: None,
        }
    }

    #[test]
    fn test_new_format_launches_every_distinct_argv_despite_stale_exec_conf() {
        let dir = tempfile::tempdir().unwrap();
        // exec.conf is stale and degraded: one bare 'brave', nothing else
        std::fs::write(dir.path().join(EXEC_NAME), "[workspace 1 silent] brave\n").unwrap();
        write_clients(
            dir.path(),
            &[
                with_argv(
                    "brave",
                    &["/opt/brave/brave", "--app=https://example.test"],
                    5,
                    (0, 0),
                ),
                with_argv(
                    "brave",
                    &["/opt/brave/brave", "--user-data-dir=/home/x/profile two"],
                    1,
                    (0, 0),
                ),
                with_argv(
                    "brave",
                    &["/opt/brave/brave", "--user-data-dir=/home/x/profile two"],
                    1,
                    (50, 0),
                ),
                with_argv("kitty", &["kitty"], 2, (0, 0)),
            ],
        );
        let plan = sim_session(dir.path()).prepare_load(dir.path()).unwrap();
        assert!(plan.place_by_class);
        let commands: Vec<&str> = plan.entries.iter().map(|e| e.command.as_str()).collect();
        // Launch order: workspace ascending
        assert_eq!(
            commands,
            vec![
                "/opt/brave/brave '--user-data-dir=/home/x/profile two'",
                "kitty",
                "/opt/brave/brave --app=https://example.test",
            ]
        );
        assert!(!commands.contains(&"brave"));
    }

    #[test]
    fn test_fingerprint_ignores_address_pid_and_order_but_not_geometry() {
        let saved = |class: &str, at: (i32, i32), pid: i32| {
            let mut client = client(class, "t", 1, false, at, (10, 10));
            client.pid = pid;
            SavedClient {
                client,
                argv: Some(vec![class.to_string()]),
                cwd: None,
                foreground: None,
            }
        };
        let before = vec![
            saved("brave", (0, 0), 10),
            saved("brave", (5, 0), 10),
            saved("kitty", (9, 9), 20),
        ];
        // After a reload: new pids, new addresses, different order
        let mut after = vec![
            saved("kitty", (9, 9), 77),
            saved("brave", (5, 0), 55),
            saved("brave", (0, 0), 55),
        ];
        for (i, c) in after.iter_mut().enumerate() {
            c.client.address = hyprland::shared::Address::new(format!("0xf{i}"));
        }
        assert_eq!(fingerprint(&before), fingerprint(&after));

        after[1].client.at = (6, 0);
        assert_ne!(fingerprint(&before), fingerprint(&after));
    }

    #[test]
    fn test_old_snapshot_without_new_fields_still_reads() {
        let old =
            serde_json::to_string(&vec![client("kitty", "t", 1, false, (1, 2), (3, 4))]).unwrap();
        let back: Vec<SavedClient> = serde_json::from_str(&old).unwrap();
        assert_eq!(back.len(), 1);
        assert!(back[0].argv.is_none() && back[0].cwd.is_none());
        // And absent fields are not written back out
        assert!(!serde_json::to_string(&back).unwrap().contains("argv"));
    }

    #[test]
    fn test_write_snapshot_is_atomic_and_complete() {
        let dir = tempfile::tempdir().unwrap();
        let clients: Vec<SavedClient> = (0..3)
            .map(|i| SavedClient {
                client: client("brave", &format!("w{i}"), 1, false, (i, 0), (10, 10)),
                argv: Some(vec!["brave".into()]),
                cwd: None,
                foreground: None,
            })
            .collect();
        // Three windows of one process: one launch line, three saved windows
        write_snapshot(
            dir.path(),
            &["[workspace 1 silent] brave".to_string()],
            &clients,
        )
        .unwrap();
        let back: Vec<SavedClient> = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(CLIENTS_PATH_NAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(EXEC_NAME)).unwrap(),
            "[workspace 1 silent] brave\n"
        );
        assert!(!dir.path().join("exec.conf.tmp").exists());
    }

    #[test]
    fn test_plan_adjustment_nothing_to_do_when_tiled_and_in_place() {
        let real = client("kitty", "term", 3, false, (0, 0), (500, 500));
        let saved = client("kitty", "term", 3, false, (5, 5), (100, 100));
        assert!(plan_adjustment(&real, &saved).is_empty());
    }

    #[test]
    fn test_scoped_load_plans_only_the_requested_workspaces_slice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(EXEC_NAME), "[workspace 1 silent] kitty\n").unwrap();
        let mut brave = with_argv(
            "brave",
            &["/opt/brave/brave", "--profile-directory=P"],
            2,
            (0, 0),
        );
        brave.client.pid = 21;
        let mut brave_other = with_argv(
            "brave",
            &["/opt/brave/brave", "--profile-directory=P"],
            3,
            (0, 0),
        );
        brave_other.client.pid = 21;
        write_clients(
            dir.path(),
            &[
                with_argv("kitty", &["kitty"], 1, (0, 0)),
                brave,
                brave_other,
            ],
        );
        let plan = scoped_session(dir.path(), 2)
            .prepare_load(dir.path())
            .unwrap();
        assert_eq!(plan.scope.as_deref(), Some("2"));
        assert_eq!(plan.saved.len(), 1);
        assert_eq!(plan.saved[0].workspace.selector(), "2");
        // Only the process behind workspace 2's window is launched, expecting one window
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].group.as_ref().unwrap().saved, vec![0]);
        assert!(plan.entries[0].command.contains("brave"));
    }

    #[test]
    fn test_scoped_load_of_a_workspace_with_no_saved_windows_fails_before_anything_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(EXEC_NAME), "[workspace 1 silent] kitty\n").unwrap();
        write_clients(dir.path(), &[with_argv("kitty", &["kitty"], 1, (0, 0))]);
        let err = scoped_session(dir.path(), 4)
            .prepare_load(dir.path())
            .unwrap_err();
        assert!(err.to_string().contains("workspace 4"), "{err}");
    }

    #[test]
    fn test_scoped_simulate_load_succeeds_and_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(EXEC_NAME), "[workspace 1 silent] kitty\n").unwrap();
        write_clients(dir.path(), &[with_argv("kitty", &["kitty"], 1, (0, 0))]);
        assert!(scoped_session(dir.path(), 1).load("").is_ok());
    }

    // ---- V2: inside-terminal capture, planning, fixed point ----

    use crate::procfs::fake::{run_in_foreground, terminal_with_shell, FakeProc};

    fn terminal_window(pid: i32, workspace: i32) -> Client {
        let mut c = client("foot", "term", workspace, false, (workspace, 0), (10, 10));
        c.pid = pid;
        c
    }

    fn gui_window(class: &str, pid: i32) -> Client {
        let mut c = client(class, class, 1, false, (7, 7), (10, 10));
        c.pid = pid;
        c
    }

    fn capture(p: &FakeProc, clients: &[Client]) -> Vec<SavedClient> {
        SavedClient::capture_all(clients, p, &ResumeConfig::default())
    }

    #[test]
    fn test_terminal_running_claude_records_the_foreground_program() {
        let mut p = terminal_with_shell();
        run_in_foreground(&mut p, 200, &["claude"], "/home/synthetic/proj");
        let saved = capture(&p, &[terminal_window(100, 1)]);
        let fg = saved[0].foreground.as_ref().unwrap();
        assert_eq!(
            (fg.pid, fg.argv.clone(), fg.cwd.as_deref(), fg.linked),
            (
                200,
                vec!["claude".to_string()],
                Some("/home/synthetic/proj"),
                false
            )
        );
    }

    #[test]
    fn test_terminal_client_cwd_is_the_shells_cwd_not_the_terminal_process_cwd() {
        let p = terminal_with_shell();
        let saved = capture(&p, &[terminal_window(100, 1)]);
        assert_eq!(saved[0].cwd.as_deref(), Some("/home/synthetic/shell-dir"));
        // Nothing but the shell running: no foreground record
        assert!(saved[0].foreground.is_none());
    }

    #[test]
    fn test_saving_from_a_terminal_does_not_record_the_save_command_as_its_foreground() {
        let mut p = terminal_with_shell();
        run_in_foreground(
            &mut p,
            200,
            &["hyprsession", "save"],
            "/home/synthetic/proj",
        );
        p.own = 200;
        let saved = capture(&p, &[terminal_window(100, 1)]);
        assert!(saved[0].foreground.is_none());
        assert_eq!(saved[0].cwd.as_deref(), Some("/home/synthetic/shell-dir"));
    }

    #[test]
    fn test_non_terminal_windows_get_no_foreground_and_keep_their_cwd() {
        let mut p = terminal_with_shell();
        p.add(
            900,
            1,
            900,
            900,
            0,
            -1,
            &["/usr/bin/mpv"],
            "/home/synthetic/videos",
        );
        let saved = capture(&p, &[gui_window("mpv", 900)]);
        assert!(saved[0].foreground.is_none());
        assert_eq!(saved[0].cwd.as_deref(), Some("/home/synthetic/videos"));
    }

    #[test]
    fn test_gui_app_started_from_a_terminal_is_linked() {
        // The playcall pattern: the terminal's foreground program IS the GUI window's process
        let mut p = terminal_with_shell();
        run_in_foreground(
            &mut p,
            200,
            &["./playcall-gui"],
            "/home/synthetic/Work/playcall-gui",
        );
        let saved = capture(
            &p,
            &[terminal_window(100, 1), gui_window("playcall-gui", 200)],
        );
        assert!(saved[0].foreground.as_ref().unwrap().linked);
        // And the GUI window carries no foreground of its own
        assert!(saved[1].foreground.is_none());
    }

    #[test]
    fn test_wrapper_script_launching_the_gui_is_linked_through_its_descendant() {
        // The foreground leader is a shell script (pid 200); the GUI window's process (pid 201) is its child
        let mut p = terminal_with_shell();
        run_in_foreground(&mut p, 200, &["sh", "./run.sh"], "/home/synthetic/proj");
        p.add(
            201,
            200,
            200,
            101,
            5,
            200,
            &["./playcall-gui"],
            "/home/synthetic/proj",
        );
        let saved = capture(
            &p,
            &[terminal_window(100, 1), gui_window("playcall-gui", 201)],
        );
        assert!(saved[0].foreground.as_ref().unwrap().linked);
    }

    #[test]
    fn test_a_group_member_that_is_a_saved_window_links() {
        let mut p = terminal_with_shell();
        run_in_foreground(&mut p, 200, &["sh", "-c", "a | b"], "/tmp");
        p.add(202, 101, 200, 101, 5, 200, &["b"], "/tmp");
        let saved = capture(&p, &[terminal_window(100, 1), gui_window("b", 202)]);
        assert!(saved[0].foreground.as_ref().unwrap().linked);
    }

    #[test]
    fn test_linkage_is_pid_identity_not_command_matching() {
        // A saved GUI window runs the same command as the terminal's foreground program, but is another process
        let mut p = terminal_with_shell();
        run_in_foreground(&mut p, 200, &["playcall-gui"], "/tmp");
        p.add(300, 1, 300, 300, 0, -1, &["playcall-gui"], "/tmp");
        let saved = capture(
            &p,
            &[terminal_window(100, 1), gui_window("playcall-gui", 300)],
        );
        assert!(!saved[0].foreground.as_ref().unwrap().linked);
    }

    #[test]
    fn test_terminals_sharing_a_process_record_no_foreground() {
        let mut p = terminal_with_shell();
        run_in_foreground(&mut p, 200, &["claude"], "/tmp");
        let saved = capture(&p, &[terminal_window(100, 1), terminal_window(100, 2)]);
        assert!(saved.iter().all(|s| s.foreground.is_none()));
    }

    #[test]
    fn test_foreground_record_round_trips_and_old_readers_ignore_it() {
        let mut p = terminal_with_shell();
        run_in_foreground(&mut p, 200, &["claude"], "/home/synthetic/proj");
        let saved = capture(&p, &[terminal_window(100, 1)]);
        let json = serde_json::to_string(&saved).unwrap();
        let back: Vec<SavedClient> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].foreground, saved[0].foreground);
        let plain: Vec<Client> = serde_json::from_str(&json).unwrap();
        assert_eq!(plain[0], saved[0].client);
        // A malformed record is treated as absent
        let broken = json.replace("\"linked\":false", "\"linked\":\"x\"");
        assert_ne!(broken, json);
        assert!(
            serde_json::from_str::<Vec<SavedClient>>(&broken).unwrap()[0]
                .foreground
                .is_none()
        );
    }

    fn terminal_snapshot(fg_argv: &[&str], linked: bool) -> SavedClient {
        let mut saved = SavedClient {
            client: terminal_window(100, 1),
            argv: Some(vec!["foot".to_string()]),
            cwd: Some("/home/synthetic/shell-dir".to_string()),
            foreground: Some(Foreground {
                pid: 200,
                argv: fg_argv.iter().map(|s| s.to_string()).collect(),
                cwd: Some("/home/synthetic/proj".to_string()),
                linked,
            }),
        };
        saved.client.class = "foot".to_string();
        saved
    }

    fn plan_for(
        dir: &std::path::Path,
        clients: &[SavedClient],
        resume: &ResumeConfig,
        exists: bool,
    ) -> Vec<String> {
        write_clients(dir, clients);
        std::fs::write(dir.join(EXEC_NAME), "[workspace 1 silent] foot\n").unwrap();
        let _ = exists;
        sim_session(dir)
            .prepare_load_with(dir, resume)
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.command)
            .collect()
    }

    #[test]
    fn test_plan_resumes_claude_in_its_directory_with_a_prompt_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        // The recorded directories are synthetic and absent here, so use real ones
        let proj = dir.path().join("proj");
        std::fs::create_dir(&proj).unwrap();
        let mut term = terminal_snapshot(&["claude"], false);
        term.foreground.as_mut().unwrap().cwd = Some(proj.to_str().unwrap().to_string());
        term.cwd = Some(dir.path().to_str().unwrap().to_string());
        let commands = plan_for(dir.path(), &[term], &ResumeConfig::default(), true);
        assert_eq!(commands.len(), 1);
        let command = &commands[0];
        assert!(
            command.starts_with(&format!("cd {} && foot sh -c ", dir.path().display())),
            "{command}"
        );
        assert!(
            command.ends_with(&format!(
                "hyprsession-resume {} 'claude --continue'",
                proj.display()
            )),
            "{command}"
        );
    }

    #[test]
    fn test_plan_for_a_linked_terminal_is_a_plain_terminal_launch_and_the_app_launches_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut gui = SavedClient {
            client: gui_window("playcall-gui", 200),
            argv: Some(vec!["./playcall-gui".into()]),
            cwd: None,
            foreground: None,
        };
        gui.client.workspace = Workspace::numbered(1);
        let mut term = terminal_snapshot(&["./playcall-gui"], true);
        term.cwd = None;
        let commands = plan_for(dir.path(), &[term, gui], &ResumeConfig::default(), true);
        let mut sorted = commands.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec!["./playcall-gui".to_string(), "foot".to_string()]
        );
    }

    #[test]
    fn test_plan_degrades_an_unverified_terminal_to_a_clean_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = terminal_snapshot(&["claude"], false);
        t.argv = Some(vec!["myterm".into()]);
        t.cwd = None;
        let config = ResumeConfig::parse("[terminals]\nextra = [\"myterm\"]\n", "t").unwrap();
        assert_eq!(
            plan_for(dir.path(), &[t], &config, true),
            vec!["myterm".to_string()]
        );
    }

    #[test]
    fn test_legacy_snapshots_without_foreground_plan_exactly_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let mut plain = terminal_snapshot(&["x"], false);
        plain.foreground = None;
        plain.cwd = None;
        assert_eq!(
            plan_for(dir.path(), &[plain], &ResumeConfig::default(), true),
            vec!["foot".to_string()]
        );
    }

    /// The process tree the compositor shows for a terminal launched with `command` (a planned
    /// wrapped launch): the terminal's argv carries the wrapper, and the wrapper `sh` is the
    /// terminal's shell, running the program as a child *in its own process group* (a
    /// non-interactive shell has no job control, so nothing becomes the tty's foreground group)
    fn wrapped_terminal_tree(command: &str, cwd: &str, program: &[&str]) -> FakeProc {
        // the launch may be prefixed with `cd <dir> &&`; the process tree starts at the terminal
        let words = shell_words(&command[command.find("foot").unwrap()..]);
        let terminal_argv: Vec<&str> = words
            .iter()
            .skip_while(|w| *w != "foot")
            .map(String::as_str)
            .collect();
        let mut p = FakeProc::default();
        p.add(1, 0, 1, 1, 0, -1, &["init"], "/");
        p.add(
            500,
            1,
            500,
            500,
            0,
            -1,
            &terminal_argv,
            "/home/synthetic/shell-dir",
        );
        p.add(501, 500, 501, 501, 9, 501, &terminal_argv[1..], cwd);
        p.add(502, 501, 501, 501, 9, 501, program, cwd);
        p
    }

    #[test]
    fn test_wrapper_terminal_records_its_resume_and_survives_a_second_generation() {
        let resume = ResumeConfig::default();
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("playcall-gui");
        std::fs::create_dir(&proj).unwrap();
        let proj = proj.to_str().unwrap();

        // Generation 1: a load launched the terminal with the resume wrapper
        let mut seed = terminal_snapshot(&["claude"], false);
        seed.cwd = None;
        seed.foreground.as_mut().unwrap().cwd = Some(proj.to_string());
        let command1 = plan_for(dir.path(), &[seed], &resume, true).remove(0);
        let tree = wrapped_terminal_tree(&command1, proj, &["claude", "--continue"]);

        // Save: the probe sees only "the shell" (no foreground group), yet the resume is recorded
        let save = SavedClient::capture_all(&[terminal_window(500, 1)], &tree, &resume);
        assert_eq!(
            save[0].argv,
            Some(vec!["foot".to_string()]),
            "the terminal itself, never the wrapper"
        );
        let fg = save[0]
            .foreground
            .as_ref()
            .expect("wrapper terminal must record a foreground");
        assert_eq!(fg.argv, vec!["claude", "--continue"]);
        assert_eq!(fg.cwd.as_deref(), Some(proj));
        assert!(!fg.linked);

        // ...so the next load dispatches the same resume in the same directory
        let command2 = plan_for(dir.path(), &save, &resume, true).remove(0);
        let wrapper = |c: &str| c[c.find("foot sh -c").unwrap()..].to_string();
        assert_eq!(wrapper(&command2), wrapper(&command1));

        // Second generation: that load's terminal records identically at the next save
        let tree2 = wrapped_terminal_tree(&command2, proj, &["claude", "--continue"]);
        let save2 = SavedClient::capture_all(&[terminal_window(500, 1)], &tree2, &resume);
        assert_eq!(save2[0].foreground, save[0].foreground);
        assert_eq!(fingerprint(&save2), fingerprint(&save));
    }

    #[test]
    fn test_wrapper_terminal_whose_program_has_ended_records_no_resume() {
        // The wrapper exec'd the user's shell once the program exited: no marker left in the shell's argv
        let resume = ResumeConfig::default();
        let mut p = FakeProc::default();
        p.add(1, 0, 1, 1, 0, -1, &["init"], "/");
        p.add(
            500,
            1,
            500,
            500,
            0,
            -1,
            &[
                "foot",
                "sh",
                "-c",
                "s",
                "hyprsession-resume",
                "/p",
                "claude --continue",
            ],
            "/",
        );
        p.add(501, 500, 501, 501, 9, 501, &["-zsh"], "/p");
        let save = SavedClient::capture_all(&[terminal_window(500, 1)], &p, &resume);
        assert_eq!(save[0].foreground, None);
        assert_eq!(save[0].argv, Some(vec!["foot".to_string()]));
    }

    #[test]
    fn test_wrapper_terminal_keeps_the_guards() {
        let resume = ResumeConfig::default();
        // hyprsession itself running under the wrapper shell is not a program to bring back
        let mut p = FakeProc::default();
        p.add(1, 0, 1, 1, 0, -1, &["init"], "/");
        p.add(500, 1, 500, 500, 0, -1, &["foot"], "/");
        p.add(
            501,
            500,
            501,
            501,
            9,
            501,
            &[
                "sh",
                "-c",
                "s",
                "hyprsession-resume",
                "/p",
                "hyprsession save x",
            ],
            "/p",
        );
        p.add(
            502,
            501,
            501,
            501,
            9,
            501,
            &["hyprsession", "save", "x"],
            "/p",
        );
        p.own = 502;
        assert_eq!(
            SavedClient::capture_all(&[terminal_window(500, 1)], &p, &resume)[0].foreground,
            None
        );
        // two shells in one terminal process: ambiguous, nothing recorded
        let mut p = FakeProc::default();
        p.add(1, 0, 1, 1, 0, -1, &["init"], "/");
        p.add(500, 1, 500, 500, 0, -1, &["foot"], "/");
        p.add(
            501,
            500,
            501,
            501,
            9,
            501,
            &["sh", "-c", "s", "hyprsession-resume", "/p", "claude"],
            "/p",
        );
        p.add(503, 500, 503, 503, 10, 503, &["-zsh"], "/p");
        assert_eq!(
            SavedClient::capture_all(&[terminal_window(500, 1)], &p, &resume)[0].foreground,
            None
        );
        // a resumed program that owns another saved window is linked, not relaunched in the terminal
        let mut p =
            wrapped_terminal_tree("foot sh -c s hyprsession-resume /p brave", "/p", &["brave"]);
        p.procs.get_mut(&502).unwrap().stat.pgrp = 501;
        let mut browser = terminal_window(502, 2);
        browser.class = "brave".into();
        browser.initial_class = "brave".into();
        let save = SavedClient::capture_all(&[terminal_window(500, 1), browser], &p, &resume);
        assert!(save[0].foreground.as_ref().unwrap().linked);
    }

    #[test]
    fn test_save_load_save_fixed_point_with_a_resumed_foreground_program() {
        let resume = ResumeConfig::default();
        // Save 1: foot running claude in a project directory, shell cd'd elsewhere
        let mut before_proc = terminal_with_shell();
        run_in_foreground(&mut before_proc, 200, &["claude"], "/home/synthetic/proj");
        let save1 = SavedClient::capture_all(&[terminal_window(100, 1)], &before_proc, &resume);

        // Load: what the planner would launch for the terminal...
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir(&proj).unwrap();
        let mut planned = save1.clone();
        planned[0].foreground.as_mut().unwrap().cwd = Some(proj.to_str().unwrap().to_string());
        let command = plan_for(dir.path(), &planned, &resume, true).remove(0);
        // ...and the process tree the compositor then shows: the terminal's argv carries the wrapper,
        // and the program runs as its resume command
        let words: Vec<String> = shell_words(&command);
        let mut after_proc = FakeProc::default();
        after_proc.add(1, 0, 1, 1, 0, -1, &["init"], "/");
        let terminal_argv: Vec<&str> = words
            .iter()
            .skip_while(|w| *w != "foot")
            .map(String::as_str)
            .collect();
        after_proc.add(
            500,
            1,
            500,
            500,
            0,
            -1,
            &terminal_argv,
            "/home/synthetic/shell-dir",
        );
        after_proc.add(501, 500, 501, 501, 9, 502, &["sh"], proj.to_str().unwrap());
        after_proc.add(
            502,
            501,
            502,
            501,
            9,
            502,
            &["claude", "--continue"],
            proj.to_str().unwrap(),
        );
        let save2 = SavedClient::capture_all(&[terminal_window(500, 1)], &after_proc, &resume);

        // The terminal's recorded argv is its own again, never the wrapper
        assert_eq!(save2[0].argv, Some(vec!["foot".to_string()]));
        // Same layout, argv and relaunch plan, even though the program's argv changed
        let mut expected = save1.clone();
        expected[0].foreground.as_mut().unwrap().cwd = Some(proj.to_str().unwrap().to_string());
        assert_eq!(fingerprint(&save2), fingerprint(&expected));
        // The relaunch plan is the same too; the only drift is the terminal's own directory, which
        // is now the shell's (the program's directory) rather than where the first shell had cd'd
        let command2 = plan_for(dir.path(), &save2, &resume, true).remove(0);
        let wrapper = |c: &str| c[c.find("foot sh -c").unwrap()..].to_string();
        assert_eq!(wrapper(&command2), wrapper(&command));
    }

    /// Split a shell command line into words (single quotes only, as shell_quote produces)
    fn shell_words(line: &str) -> Vec<String> {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("for a in {line}; do printf '%s\\0' \"$a\"; done"))
            .output()
            .unwrap();
        crate::command_detection::parse_cmdline(&out.stdout)
    }
}
