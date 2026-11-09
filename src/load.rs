//! Loading a session: parse the launch lines, launch them in layout order, and place
//! every saved window as the applications bring their windows up.
//!
//! All interaction with the compositor goes through `Host`, so the sequencing can be
//! tested against a fake one.

use crate::command_detection::{resolve_desktop_command, shell_join, shell_quote};
use crate::dispatch::{Dispatcher, Op};
use crate::dwindle;
use crate::ipc::{self, Client, Workspace};
use crate::queue::{ws_key, Held, PlacementQueue, WsKey};
use crate::resume::{
    program_identity, strip_wrapper, terminal_style, wrap_terminal_command, Relaunch, ResumeConfig,
};
use crate::session::{plan_adjustment, SavedClient};
use hyprland::data::FullscreenMode;
use hyprland::shared::{Address, HyprData};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Polls per second while waiting for windows
const TICKS_PER_SECOND: u64 = 4;
/// How long to wait for a launched program to show a window before moving on
const LAUNCH_WAIT_TICKS: u64 = 20 * TICKS_PER_SECOND;
/// How long a launch group's window count must hold still before it is considered settled
const SETTLE_TICKS: u64 = 3 * TICKS_PER_SECOND;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleWorkspace {
    Id(i32),
    /// Special workspace; `None` is the anonymous one
    Special(Option<String>),
    Named(String),
}

/// The placement rules written in front of a launch line in exec.conf
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecRules {
    pub monitor: Option<i128>,
    pub workspace: Option<RuleWorkspace>,
    pub float: bool,
    pub at: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
    pub pin: bool,
    pub fullscreen: Option<i32>,
    /// Rule words that were not recognised (ignored)
    pub unknown: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecEntry {
    pub rules: ExecRules,
    /// The bare command, with the `[rules]` prefix removed
    pub command: String,
    /// The saved windows this launch is responsible for (new-format snapshots only)
    pub group: Option<GroupInfo>,
}

/// What a launch is expected to produce
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupInfo {
    /// The recorded command, for reports
    pub label: String,
    /// Indices into `LoadPlan::saved` of the windows of this launch's process
    pub saved: Vec<usize>,
}

fn two_numbers(rule: &str, rest: &[&str]) -> Result<(i32, i32), String> {
    match rest {
        [a, b] => Ok((
            a.parse()
                .map_err(|_| format!("bad number in rule '{rule}'"))?,
            b.parse()
                .map_err(|_| format!("bad number in rule '{rule}'"))?,
        )),
        _ => Err(format!("rule '{rule}' needs two numbers")),
    }
}

fn parse_rules(text: &str) -> Result<ExecRules, String> {
    let mut rules = ExecRules::default();
    for rule in text.split(';').map(str::trim).filter(|r| !r.is_empty()) {
        let words: Vec<&str> = rule.split_whitespace().collect();
        let (name, rest) = (words[0], &words[1..]);
        match name {
            "monitor" => {
                rules.monitor = Some(
                    rest.first()
                        .and_then(|m| m.parse().ok())
                        .ok_or_else(|| format!("bad monitor in rule '{rule}'"))?,
                )
            }
            "workspace" => {
                let target = rest
                    .first()
                    .ok_or_else(|| format!("rule '{rule}' needs a workspace"))?;
                rules.workspace = Some(if *target == "special" {
                    RuleWorkspace::Special(None)
                } else if let Some(name) = target.strip_prefix("special:") {
                    RuleWorkspace::Special(Some(name.to_string()))
                } else {
                    RuleWorkspace::Id(
                        target
                            .parse()
                            .map_err(|_| format!("bad workspace in rule '{rule}'"))?,
                    )
                });
            }
            "float" => rules.float = true,
            "pin" => rules.pin = true,
            "move" => rules.at = Some(two_numbers(rule, rest)?),
            "size" => rules.size = Some(two_numbers(rule, rest)?),
            "fullscreenstate" => {
                rules.fullscreen = Some(
                    rest.first()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| format!("bad state in rule '{rule}'"))?,
                )
            }
            _ => rules.unknown.push(rule.to_string()),
        }
    }
    Ok(rules)
}

/// Parse one exec.conf line: `[rules] command`, or a bare command
pub fn parse_exec_line(line: &str) -> Result<ExecEntry, String> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix('[') {
        // Rules never contain ']', commands might, so split at the first one
        let (rules, command) = rest
            .split_once(']')
            .ok_or_else(|| format!("unterminated rules in line: {line}"))?;
        let command = command.trim();
        if command.is_empty() {
            return Err(format!("no command in line: {line}"));
        }
        Ok(ExecEntry {
            rules: parse_rules(rules)?,
            command: command.to_string(),
            group: None,
        })
    } else if line.is_empty() {
        Err("empty line".to_string())
    } else {
        Ok(ExecEntry {
            rules: ExecRules::default(),
            command: line.to_string(),
            group: None,
        })
    }
}

/// Parse the whole of exec.conf; blank lines are skipped, any bad line is an error
pub fn parse_exec_conf(text: &str) -> Result<Vec<ExecEntry>, String> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(n, l)| parse_exec_line(l).map_err(|e| format!("exec.conf line {}: {e}", n + 1)))
        .collect()
}

/// Placement rules equivalent to a saved window's state
fn rules_from_client(client: &Client) -> ExecRules {
    let workspace = if client.workspace.kind == crate::ipc::WorkspaceKind::Special {
        RuleWorkspace::Special(Some(
            client
                .workspace
                .address
                .strip_prefix("special:")
                .unwrap_or(&client.workspace.address)
                .to_owned(),
        ))
    } else if let Some(id) = client.workspace.id {
        RuleWorkspace::Id(id)
    } else {
        RuleWorkspace::Named(client.workspace.address.clone())
    };
    ExecRules {
        monitor: client.monitor,
        workspace: Some(workspace),
        float: client.floating,
        at: Some(client.at),
        size: Some(client.size),
        pin: client.pinned,
        fullscreen: match client.fullscreen {
            FullscreenMode::None => None,
            mode => Some(mode as i32),
        },
        unknown: vec![],
    }
}

/// The command line recorded for a saved window. A single-element argv is already a
/// complete command line (Chromium-family processes present /proc cmdline as one
/// space-joined string, quotes and all), so it is used verbatim; several elements are
/// shell-joined with quoting.
fn command_from_saved_argv(argv: &[String]) -> Option<String> {
    match argv {
        [] => None,
        [line] if line.trim().is_empty() => None,
        [line] => Some(line.clone()),
        argv => Some(shell_join(argv)),
    }
}

/// Whether the recorded program is the inside view of a Flatpak sandbox (`/app/...`),
/// which does not exist on the host
fn is_sandbox_internal(argv: &[String]) -> bool {
    argv.first()
        .and_then(|first| first.split_whitespace().next())
        .is_some_and(|program| program.starts_with("/app/"))
}

/// Launch entries for a new-format snapshot, built from the recorded argv. Windows are
/// grouped by command; each distinct pid in a group is one launch (identical commands
/// in separate processes are separate launches, windows sharing a process are one),
/// with rules taken from the group's first saved window. A window whose argv is
/// sandbox-internal launches through its desktop entry instead. Windows without argv
/// cannot be launched and are skipped (they are still placed if their app brings them
/// up).
pub fn launch_entries_from_saved(
    saved: &[SavedClient],
    xdg_map: &HashMap<String, String>,
) -> Vec<ExecEntry> {
    launch_entries_with(
        saved,
        xdg_map,
        &|dir| dir.is_dir(),
        &ResumeConfig::default(),
    )
}

/// Prefix a launch with `cd <cwd> &&` so it runs from its recorded directory. A
/// directory that no longer exists leaves the launch unwrapped, with a warning.
pub fn wrap_cwd(
    command: String,
    cwd: Option<&str>,
    window: &Client,
    dir_exists: &dyn Fn(&Path) -> bool,
) -> String {
    let Some(dir) = cwd else { return command };
    if dir_exists(Path::new(dir)) {
        format!("cd {} && {}", shell_quote(dir), command)
    } else {
        println!(
            "Warning: working directory '{dir}' of '{}' ({}) no longer exists; launching from the loader's directory",
            window.title, window.class
        );
        command
    }
}

/// A terminal with a recorded foreground program is launched running it (resume command,
/// or its argv replayed), in the program's own directory, and falls through to a shell
/// when it exits. When the program owns its own window, has no way to be run, or the policy
/// says so, the terminal comes back at a plain prompt.
fn terminal_launch(
    command: String,
    window: &SavedClient,
    resume: &ResumeConfig,
    dir_exists: &dyn Fn(&Path) -> bool,
) -> String {
    let Some(foreground) = window.foreground.as_ref() else {
        return command;
    };
    let title = &window.client.title;
    let (kind, program) = match resume.decide(foreground) {
        Relaunch::Linked => {
            println!("Terminal '{title}': its foreground program owns its own window; restoring at a clean prompt");
            return command;
        }
        Relaunch::CleanPrompt(reason) => {
            println!("Terminal '{title}': restoring at a clean prompt ({reason})");
            return command;
        }
        Relaunch::Table(program) => ("resume command", program),
        Relaunch::Replay(program) => ("recorded command", program),
    };
    let terminal = window
        .argv
        .as_deref()
        .and_then(program_identity)
        .unwrap_or_default();
    let Some(style) = terminal_style(&terminal) else {
        println!(
            "Warning: no verified way to run a command in '{terminal}'; terminal '{title}' restores at a clean prompt"
        );
        return command;
    };
    let cwd = match foreground.cwd.as_deref() {
        Some(dir) if dir_exists(Path::new(dir)) => dir.to_string(),
        Some(dir) => {
            println!("Warning: working directory '{dir}' of '{program}' in terminal '{title}' no longer exists");
            ".".to_string()
        }
        None => ".".to_string(),
    };
    println!("Terminal '{title}': running {kind} '{program}' in {cwd}");
    wrap_terminal_command(&command, style, &cwd, &program)
}

/// `launch_entries_from_saved` with the directory check injected
pub fn launch_entries_with(
    saved: &[SavedClient],
    xdg_map: &HashMap<String, String>,
    dir_exists: &dyn Fn(&Path) -> bool,
    resume: &ResumeConfig,
) -> Vec<ExecEntry> {
    let mut entries: Vec<ExecEntry> = vec![];
    let mut group_rules: HashMap<String, ExecRules> = HashMap::new();
    let mut launched: HashMap<(String, i32), usize> = HashMap::new();
    for (index, window) in saved.iter().enumerate() {
        let argv = window.argv.as_deref().map(|argv| {
            if let Some(style) = program_identity(argv).and_then(|program| terminal_style(&program))
            {
                std::borrow::Cow::Owned(strip_wrapper(argv, Some(style)))
            } else {
                std::borrow::Cow::Borrowed(argv)
            }
        });
        let sandboxed = argv.as_deref().is_some_and(is_sandbox_internal);
        let unavailable = argv.as_deref().is_some_and(|args| {
            let Some(program) = args.first() else {
                return false;
            };
            let executable = if args.len() == 1
                && program.contains(char::is_whitespace)
                && !Path::new(program).exists()
            {
                program.split_whitespace().next().unwrap_or(program)
            } else {
                program
            };
            let path = Path::new(executable);
            path.is_absolute() && !path.exists()
        });
        let command = if window
            .client
            .class
            .eq_ignore_ascii_case("dev.noctalia.Noctalia")
            && window.client.title == "Noctalia Settings"
        {
            // Its process argv starts the shell, not the settings window.
            Some("noctalia msg settings-open".to_owned())
        } else {
            let command = argv.as_deref().and_then(command_from_saved_argv);
            if command.is_none() || sandboxed || unavailable {
                resolve_desktop_command(&window.client, xdg_map).or(command)
            } else {
                command
            }
        };
        let Some(command) = command else {
            println!(
                "Warning: no launch command or desktop entry for '{}' ({})",
                window.client.title, window.client.class
            );
            continue;
        };
        let rules = group_rules
            .entry(command.clone())
            .or_insert_with(|| rules_from_client(&window.client))
            .clone();
        match launched.get(&(command.clone(), window.client.pid)) {
            Some(&entry) => {
                if let Some(group) = entries[entry].group.as_mut() {
                    group.saved.push(index);
                }
            }
            None => {
                launched.insert((command.clone(), window.client.pid), entries.len());
                let label = command.clone();
                let command = terminal_launch(command, window, resume, dir_exists);
                // A sandbox's cwd is a path inside the sandbox: not wrapped, not warned about
                let launch = if sandboxed {
                    command
                } else {
                    wrap_cwd(command, window.cwd.as_deref(), &window.client, dir_exists)
                };
                entries.push(ExecEntry {
                    rules,
                    command: launch,
                    group: Some(GroupInfo {
                        label,
                        saved: vec![index],
                    }),
                });
            }
        }
    }
    entries
}

/// Order launches so a tiling layout regrows as it was: workspace ascending, then
/// saved position (x, then y), floating windows last within each workspace
pub fn launch_order(mut entries: Vec<ExecEntry>) -> Vec<ExecEntry> {
    entries.sort_by_key(|e| {
        let workspace = match &e.rules.workspace {
            Some(RuleWorkspace::Id(id)) => *id as i64,
            Some(RuleWorkspace::Special(_)) => i64::MAX,
            Some(RuleWorkspace::Named(_)) => i64::MAX - 1,
            None => 0,
        };
        let (x, y) = e.rules.at.unwrap_or((0, 0));
        (workspace, e.rules.float, x, y)
    });
    entries
}

/// The state a freshly launched window should end up in according to its launch rules
pub fn desired_from_rules(live: &Client, rules: &ExecRules) -> Client {
    let mut wanted = live.clone();
    match &rules.workspace {
        Some(RuleWorkspace::Id(id)) => wanted.workspace = Workspace::numbered(*id),
        Some(RuleWorkspace::Named(name)) => wanted.workspace = Workspace::from_selector(name),
        Some(RuleWorkspace::Special(name)) => {
            wanted.workspace =
                Workspace::from_selector(&format!("special:{}", name.as_deref().unwrap_or("")))
        }
        None => {}
    }
    if let Some(monitor) = rules.monitor {
        wanted.monitor = Some(monitor);
    }
    wanted.floating = rules.float;
    wanted.pinned = rules.pin;
    if let Some(at) = rules.at {
        wanted.at = at;
    }
    if let Some(size) = rules.size {
        wanted.size = size;
    }
    wanted.fullscreen = match rules.fullscreen {
        Some(1) => FullscreenMode::Maximized,
        Some(2) => FullscreenMode::Fullscreen,
        Some(3) => FullscreenMode::MaximizedFullscreen,
        _ => FullscreenMode::None,
    };
    wanted
}

/// A stand-in for a window that does not exist yet (used to render simulated dispatches)
pub fn placeholder_client() -> Client {
    serde_json::from_value(serde_json::json!({
        "address": "0x<address>",
        "at": [0, 0],
        "size": [0, 0],
        "workspace": { "address": "0", "id": 0, "type": "normal", "name": "0" },
        "floating": false,
        "fullscreen": 0,
        "fullscreenClient": 0,
        "monitor": 0,
        "initialClass": "",
        "class": "",
        "initialTitle": "",
        "title": "",
        "pid": 0,
        "xwayland": false,
        "pinned": false,
        "grouped": [],
        "mapped": true,
        "swallowing": null,
        "focusHistoryID": 0
    }))
    .expect("placeholder client is valid")
}

struct Target {
    saved: Client,
    matched: Option<Address>,
}

/// Matches live windows to saved clients: by class, in saved order within the class,
/// with an exact title match among a class's remaining candidates winning the tie
pub struct Placer {
    targets: Vec<Target>,
}

impl Placer {
    pub fn new(saved: Vec<Client>) -> Self {
        Placer {
            targets: saved
                .into_iter()
                .map(|saved| Target {
                    saved,
                    matched: None,
                })
                .collect(),
        }
    }

    pub fn is_complete(&self) -> bool {
        self.targets.iter().all(|t| t.matched.is_some())
    }

    /// Addresses of the live windows that have claimed a saved client
    pub fn claimed_addresses(&self) -> HashSet<Address> {
        self.targets
            .iter()
            .filter_map(|t| t.matched.clone())
            .collect()
    }

    pub fn unmatched(&self) -> Vec<&Client> {
        self.targets
            .iter()
            .filter(|t| t.matched.is_none())
            .map(|t| &t.saved)
            .collect()
    }

    /// Claim a saved client for a live window; `None` when the window was already
    /// claimed or nothing saved fits it
    pub fn assign(&mut self, live: &Client) -> Option<Client> {
        self.assign_indexed(live).map(|(_, saved)| saved)
    }

    /// `assign`, also naming the claimed saved client's index in the plan
    pub fn assign_indexed(&mut self, live: &Client) -> Option<(usize, Client)> {
        self.assign_bound(live, &[], &HashSet::new())
    }

    /// Claim with launch identity. A window that a launch entry just spawned belongs to that
    /// entry's saved windows (`own`): they are tried first, whatever the window's transient
    /// title (a fresh terminal wears a generic title until its program retitles it, so
    /// identical titles cannot tell entries apart). Saves in `reserved` belong to other
    /// launches and are only taken when nothing else fits.
    pub fn assign_bound(
        &mut self,
        live: &Client,
        own: &[usize],
        reserved: &HashSet<usize>,
    ) -> Option<(usize, Client)> {
        if self
            .targets
            .iter()
            .any(|t| t.matched.as_ref() == Some(&live.address))
        {
            return None;
        }
        let same_class = |t: &Target| {
            t.matched.is_none()
                && (t.saved.class == live.class || t.saved.initial_class == live.initial_class)
        };
        let pick = |allowed: &dyn Fn(usize) -> bool| {
            let fits = |i: usize, t: &Target| allowed(i) && same_class(t);
            self.targets
                .iter()
                .enumerate()
                .position(|(i, t)| fits(i, t) && t.saved.title == live.title)
                .or_else(|| {
                    self.targets
                        .iter()
                        .enumerate()
                        .position(|(i, t)| fits(i, t))
                })
        };
        let index = pick(&|i| own.contains(&i))
            .or_else(|| pick(&|i| !reserved.contains(&i)))
            .or_else(|| pick(&|_| true))?;
        self.targets[index].matched = Some(live.address.clone());
        Some((index, self.targets[index].saved.clone()))
    }

    fn assign_proven(&mut self, live: &Client, own: &[usize]) -> Option<(usize, Client)> {
        if self
            .targets
            .iter()
            .any(|t| t.matched.as_ref() == Some(&live.address))
        {
            return None;
        }
        let available = |i: &&usize| self.targets.get(**i).is_some_and(|t| t.matched.is_none());
        let index = own
            .iter()
            .filter(available)
            .find(|&&i| self.targets[i].saved.title == live.title)
            .or_else(|| own.iter().find(available))
            .copied()?;
        self.targets[index].matched = Some(live.address.clone());
        Some((index, self.targets[index].saved.clone()))
    }

    /// Give a claim back (the claimed window vanished before it was placed)
    pub fn release(&mut self, index: usize) {
        self.targets[index].matched = None;
    }
}

/// Everything load needs to know, decided before anything is touched
#[derive(Debug)]
pub struct LoadPlan {
    /// Launch lines in launch order (empty when only adjusting)
    pub entries: Vec<ExecEntry>,
    /// Saved windows to place by class (new-format snapshots, or adjust-only)
    pub saved: Vec<Client>,
    /// Place by class from `saved`; otherwise place each launch's first new window by its rules
    pub place_by_class: bool,
    /// Polls to keep placing after the last launch
    pub tail_ticks: u64,
    /// Only this workspace's slice is being loaded
    pub scope: Option<String>,
    /// Workspace to focus once finished
    pub restore_workspace: Option<String>,
}

impl LoadPlan {
    pub fn tail_ticks_for(load_time: u64) -> u64 {
        load_time * TICKS_PER_SECOND
    }
}

#[derive(Debug, Default)]
pub struct LoadReport {
    pub placed: usize,
    pub unmatched: Vec<Client>,
    /// Live windows no saved client claimed (reported, never touched)
    pub surplus: Vec<Client>,
    /// Launch groups that produced fewer windows than were saved, even after re-invoking
    pub shortfalls: Vec<GroupShortfall>,
    /// One entry per workspace whose tiled windows were placed together
    pub flushes: Vec<FlushReport>,
    /// Per workspace, after startup-state finalization: windows at their saved state and geometry.
    pub verification: Vec<WorkspaceCheck>,
    pub incomplete: bool,
}

/// Final-state verification of one workspace's restored windows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceCheck {
    pub workspace: String,
    pub at_saved: usize,
    pub checked: usize,
    /// Titles of the windows that are not at their saved geometry
    pub off: Vec<String>,
}

/// How one workspace's queued tiled windows were ordered
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlushReport {
    pub workspace: String,
    pub windows: usize,
    /// `None`: placed in a computed dwindle insertion order (steered, or for the pointer as
    /// found); `Some(reason)`: saved reading order
    pub fallback: Option<String>,
    /// The loader steered the pointer to each split target (independent of where the user's pointer was)
    pub steered: bool,
    /// The pointer was somewhere else than the loader left it: the order may not have been reproduced
    pub pointer_moved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupShortfall {
    pub label: String,
    pub expected: usize,
    pub got: usize,
    pub reinvocations: usize,
}

/// A window's process as /proc shows it
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
}

/// The compositor as load sees it
pub trait Host {
    fn clients(&self) -> hyprland::Result<Vec<Client>>;
    fn send(&self, op: &Op) -> hyprland::Result<()>;
    fn active_workspace(&self) -> Option<String>;
    fn failed(&self) -> bool {
        false
    }
    fn merge_existing(&self) -> bool {
        false
    }
    /// Signal a process
    fn signal(&self, pid: i32, signal: Signal);
    /// Wait one poll interval
    fn pause(&self);
    /// The mouse pointer in layout coordinates (read only: the loader never moves it).
    /// Dwindle splits the leaf nearest the pointer, so the insertion order depends on it.
    fn cursor_pos(&self) -> Option<(i32, i32)> {
        None
    }
    /// What a window's process is running (argv, working directory): launch provenance
    fn process(&self, _pid: i32) -> Option<ProcessInfo> {
        None
    }
    /// Reveal a saved GUI from its verified tray process, without relaunching it.
    fn reveal_tray_window(&self, _entry: &ExecEntry) -> bool {
        false
    }
    /// Where the pointer was before the load touched anything (defaults to where it is now)
    fn pointer_origin(&self) -> Option<(i32, i32)> {
        self.cursor_pos()
    }
    /// A compositor option such as `general:layout`, as text
    fn option(&self, _key: &str) -> Option<String> {
        None
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

pub struct RealHost {
    pub dispatcher: Dispatcher,
    /// The pointer when the host was created: capture it before a load clears anything
    origin: Option<(i32, i32)>,
    failed: std::cell::Cell<bool>,
    merge: bool,
}

impl RealHost {
    pub fn new(dispatcher: Dispatcher) -> Self {
        let origin = hyprland::data::CursorPosition::get()
            .ok()
            .map(|c| (c.x as i32, c.y as i32));
        RealHost {
            dispatcher,
            origin,
            failed: std::cell::Cell::new(false),
            merge: false,
        }
    }

    pub fn merged(dispatcher: Dispatcher) -> Self {
        let mut host = Self::new(dispatcher);
        host.merge = true;
        host
    }
}

impl Host for RealHost {
    fn merge_existing(&self) -> bool {
        self.merge
    }
    fn clients(&self) -> hyprland::Result<Vec<Client>> {
        let result = ipc::clients();
        if result.is_err() {
            self.failed.set(true);
        }
        result
    }

    fn send(&self, op: &Op) -> hyprland::Result<()> {
        let result = self.dispatcher.send(op);
        if result.is_err() {
            self.failed.set(true);
        }
        result
    }

    fn active_workspace(&self) -> Option<String> {
        ipc::active_workspace()
            .ok()
            .map(|w| w.selector().to_owned())
    }

    fn failed(&self) -> bool {
        self.failed.get()
    }

    fn signal(&self, pid: i32, signal: Signal) {
        let flag = match signal {
            Signal::Term => "-TERM",
            Signal::Kill => "-KILL",
        };
        let _ = std::process::Command::new("kill")
            .arg(flag)
            .arg(pid.to_string())
            .output();
    }

    fn pause(&self) {
        std::thread::sleep(std::time::Duration::from_millis(1000 / TICKS_PER_SECOND));
    }

    fn pointer_origin(&self) -> Option<(i32, i32)> {
        self.origin
    }

    fn process(&self, pid: i32) -> Option<ProcessInfo> {
        Some(ProcessInfo {
            argv: crate::command_detection::read_argv(pid)?,
            cwd: crate::command_detection::read_cwd(pid),
        })
    }

    fn reveal_tray_window(&self, entry: &ExecEntry) -> bool {
        let Some(signature) = entry_signature(entry) else {
            return false;
        };
        let Some(executable) = executable_path(&signature.0[0], signature.1.as_deref()) else {
            return false;
        };
        let Ok(live) = self.clients() else {
            return false;
        };
        let mapped = live.iter().filter(|c| c.mapped).map(|c| c.pid).collect();
        crate::tray::activate_owned_window(&mapped, |pid| {
            if std::fs::canonicalize(format!("/proc/{pid}/exe"))
                .ok()
                .as_ref()
                != Some(&executable)
            {
                return false;
            }
            let Some(info) = self.process(pid) else {
                return false;
            };
            process_matches(&signature, &info.argv, info.cwd.as_deref())
                || (signature.0.len() > 1
                    && packed_process_argv(&info).is_some_and(|argv| {
                        process_matches(&signature, &argv, info.cwd.as_deref())
                    }))
        })
    }

    fn cursor_pos(&self) -> Option<(i32, i32)> {
        hyprland::data::CursorPosition::get()
            .ok()
            .map(|c| (c.x as i32, c.y as i32))
    }

    fn option(&self, key: &str) -> Option<String> {
        let value: serde_json::Value = crate::ipc::request(&format!("j/getoption {key}")).ok()?;
        ["str", "css", "int", "float"].iter().find_map(|field| {
            let value = value.get(field)?;
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_number().map(ToString::to_string))
        })
    }
}

/// How long each stage of clearing waits before escalating
pub struct ClearConfig {
    pub close_ticks: u64,
    pub term_ticks: u64,
    pub kill_ticks: u64,
}

impl Default for ClearConfig {
    fn default() -> Self {
        ClearConfig {
            close_ticks: 15 * TICKS_PER_SECOND,
            term_ticks: 10 * TICKS_PER_SECOND,
            kill_ticks: 5 * TICKS_PER_SECOND,
        }
    }
}

#[derive(Debug, Default)]
pub struct ClearReport {
    /// Titles of windows that went away after a graceful close
    pub closed: Vec<String>,
    pub termed: Vec<i32>,
    pub killed: Vec<i32>,
    /// Windows that refused a graceful close but share their process with a window outside
    /// the cleared scope, so were never signalled
    pub refused: Vec<String>,
}

/// Which windows a clear is allowed to touch
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    All,
    Workspace(String),
}

fn describe(clients: &[Client]) -> String {
    clients
        .iter()
        .map(|c| format!("'{}' ({}, pid {})", c.title, c.class, c.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Poll until `relevant` finds nothing left in the client list or the ticks run out;
/// returns what is left
fn wait_until(
    host: &dyn Host,
    ticks: u64,
    relevant: &dyn Fn(&[Client]) -> Vec<Client>,
) -> Result<Vec<Client>, String> {
    let mut last = None;
    for _ in 0..=ticks {
        if let Ok(live) = host.clients() {
            let left = relevant(&live);
            if left.is_empty() {
                return Ok(left);
            }
            last = Some(left);
        }
        if last.is_some() || ticks > 0 {
            host.pause();
        }
    }
    last.ok_or_else(|| "cannot read the client list from Hyprland".to_string())
}

/// Pids worth signalling: distinct, and never init or the kernel
fn signal_targets(clients: &[Client]) -> Vec<i32> {
    let mut pids: Vec<i32> = vec![];
    for c in clients {
        if c.pid > 1 && !pids.contains(&c.pid) {
            pids.push(c.pid);
        }
    }
    pids
}

/// Pids that have a live window outside the cleared scope: signalling one would take
/// down a window the scope has no business touching
pub fn shared_pids(live: &[Client], in_scope: &HashSet<Address>) -> HashSet<i32> {
    live.iter()
        .filter(|c| !in_scope.contains(&c.address))
        .map(|c| c.pid)
        .collect()
}

/// Close every window, escalating only for what stays: graceful close, then SIGTERM,
/// then SIGKILL, each followed by a bounded wait. Fails, naming the survivors, if
/// anything outlives SIGKILL.
pub fn clear_session(host: &dyn Host, config: &ClearConfig) -> Result<ClearReport, String> {
    clear_windows(host, config, Scope::All)
}

/// `clear_session` restricted to a scope. Signal stages only reach pids whose live
/// windows are **all** in scope: a window sharing its pid with another workspace's window
/// gets the graceful close only, and if it refuses it is reported, never killed.
pub fn clear_windows(
    host: &dyn Host,
    config: &ClearConfig,
    scope: Scope,
) -> Result<ClearReport, String> {
    let mut report = ClearReport::default();
    let all = host
        .clients()
        .map_err(|e| format!("cannot read the client list: {e}"))?;
    // Scope is fixed by address when the clear starts, so a window that moves mid-clear stays in or out
    let initial: Vec<Client> = all
        .iter()
        .filter(|c| match &scope {
            Scope::All => true,
            Scope::Workspace(id) => c.workspace.matches_selector(id),
        })
        .cloned()
        .collect();
    if initial.is_empty() {
        return Ok(report);
    }
    let in_scope: HashSet<Address> = initial.iter().map(|c| c.address.clone()).collect();
    // Windows still open that belong to the scope (everything, for Scope::All)
    let scoped = |live: &[Client]| -> Vec<Client> {
        live.iter()
            .filter(|c| scope == Scope::All || in_scope.contains(&c.address))
            .cloned()
            .collect()
    };
    // The part of that which may be signalled: not sharing a pid with an out-of-scope window
    let signalable = |live: &[Client]| -> Vec<Client> {
        let shared = shared_pids(live, &in_scope);
        scoped(live)
            .into_iter()
            .filter(|c| !shared.contains(&c.pid))
            .collect()
    };

    match &scope {
        Scope::All => println!("Closing {} windows gracefully", initial.len()),
        Scope::Workspace(id) => println!(
            "Closing {} windows on workspace {id} gracefully",
            initial.len()
        ),
    }
    for client in &initial {
        if host.send(&Op::Close(client.address.clone())).is_err() {
            println!("Warning: close request failed for '{}'", client.title);
        }
    }
    let mut left = wait_until(host, config.close_ticks, &scoped)?;
    let still_open: HashSet<&Address> = left.iter().map(|c| &c.address).collect();
    report.closed = initial
        .iter()
        .filter(|c| !still_open.contains(&c.address))
        .map(|c| c.title.clone())
        .collect();
    println!("Closed gracefully: {}", report.closed.len());
    for title in &report.closed {
        println!("  closed '{title}'");
    }

    let finish = |report: &mut ClearReport, host: &dyn Host| {
        // Whatever is still there only because a pid is shared is reported, not killed
        if let Ok(live) = host.clients() {
            let shared = shared_pids(&live, &in_scope);
            report.refused = scoped(&live)
                .iter()
                .filter(|c| shared.contains(&c.pid))
                .map(|c| format!("'{}' ({}, pid {})", c.title, c.class, c.pid))
                .collect();
        }
        for window in &report.refused {
            println!("Warning: {window} would not close and shares its process with another workspace; not signalled");
        }
    };

    if scoped(&left).is_empty() {
        return Ok(report);
    }
    for (stage, signal, ticks) in [
        ("graceful close", Signal::Term, config.term_ticks),
        ("SIGTERM", Signal::Kill, config.kill_ticks),
    ] {
        let live = host
            .clients()
            .map_err(|e| format!("cannot read the client list: {e}"))?;
        let stuck = signalable(&live);
        if stuck.is_empty() {
            finish(&mut report, host);
            return Ok(report);
        }
        let pids = signal_targets(&stuck);
        println!("Still open after {stage}: {}", describe(&stuck));
        println!(
            "Sending {} to pids {pids:?}",
            if signal == Signal::Term {
                "SIGTERM"
            } else {
                "SIGKILL"
            }
        );
        pids.iter().for_each(|&pid| host.signal(pid, signal));
        if signal == Signal::Term {
            report.termed = pids;
        } else {
            report.killed = pids;
        }
        left = wait_until(host, ticks, &signalable)?;
        if left.is_empty() {
            finish(&mut report, host);
            return Ok(report);
        }
    }

    Err(format!(
        "could not clear the desktop; these windows survived SIGKILL: {}",
        describe(&left)
    ))
}

fn apply(host: &dyn Host, live: &Client, wanted: &Client) {
    println!(
        "found '{}' on workspace: {}",
        live.title,
        live.workspace.selector()
    );
    for op in plan_adjustment(live, wanted) {
        if host.send(&op).is_err() {
            println!("Warning: Failed to apply {:?}: {}", op, live.title);
        }
    }
}

/// What a load carries from one poll to the next
struct LoadState<'a> {
    plan: &'a LoadPlan,
    placer: Placer,
    queue: PlacementQueue,
    /// In a scoped load, every window outside the loaded workspace: never claimed, moved or reported
    protected: HashSet<Address>,
    initial: HashMap<Address, usize>,
    foreign: HashSet<Address>,
    /// Where the user's pointer was when the load began (restored at the end if the loader moved it)
    pointer_home: Option<(i32, i32)>,
    pointer_moved_by_us: bool,
    /// The workspace that was visible when the load began: windows are born on it
    start_ws: Option<String>,
    /// Launch entries started so far
    launched: usize,
    /// What each entry's process looks like (argv, working directory), for launch provenance
    signatures: Vec<Option<(Vec<String>, Option<String>)>>,
    /// The entry whose process owns a window, established from /proc, not from timing
    owners: HashMap<Address, usize>,
    /// The compositor did not take a pointer warp: no more are attempted
    steering_refused: bool,
    /// Saved windows that a resume launch (a terminal under the hyprsession wrapper) will bring up
    reserved: HashSet<usize>,
    report: LoadReport,
}

/// The launch entry whose window(s) are being waited for: windows that were not there
/// before it ran are its own and bind to its saved windows
#[derive(Clone, Copy)]
struct Phase<'a> {
    before: &'a HashSet<Address>,
    own: &'a [usize],
}

/// The special workspace windows are parked on while the workspace they were born on is rebuilt
const STAGE: &str = "hyprsession-stage";

/// The process an entry's command starts: its argv and, for `cd <dir> && <command>`, its directory
fn entry_signature(entry: &ExecEntry) -> Option<(Vec<String>, Option<String>)> {
    let words = crate::command_detection::shell_split(&entry.command);
    let (cwd, rest) = match words.as_slice() {
        [cd, dir, and, rest @ ..] if cd == "cd" && and == "&&" => {
            (Some(dir.clone()), rest.to_vec())
        }
        _ => (None, words),
    };
    if rest.is_empty() {
        None
    } else {
        Some((rest, cwd))
    }
}

fn process_matches(
    signature: &(Vec<String>, Option<String>),
    actual: &[String],
    actual_cwd: Option<&str>,
) -> bool {
    let (argv, cwd) = signature;
    if argv.is_empty() || actual.len() != argv.len() || argv[0].is_empty() || actual[0].is_empty() {
        return false;
    }
    let same_executable = if argv[0].contains('/') && actual[0].contains('/') {
        argv[0] == actual[0]
    } else {
        argv[0].rsplit('/').next() == actual[0].rsplit('/').next()
    };
    same_executable
        && actual[1..] == argv[1..]
        && cwd
            .as_ref()
            .is_none_or(|dir| actual_cwd == Some(dir.as_str()))
}

fn executable_path(program: &str, cwd: Option<&str>) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let program = Path::new(program);
    let path = if program.is_absolute() {
        program.to_owned()
    } else if program.components().count() > 1 {
        cwd.map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok())?
            .join(program)
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(program))
            .find(|path| {
                path.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })?
    };
    path.canonicalize().ok()
}

fn packed_process_argv(info: &ProcessInfo) -> Option<Vec<String>> {
    let [line] = info.argv.as_slice() else {
        return None;
    };
    if !line.chars().any(char::is_whitespace) {
        return None;
    }
    let path = Path::new(line);
    let literal = if path.is_absolute() {
        path.is_file()
    } else {
        info.cwd
            .as_ref()
            .is_some_and(|cwd| Path::new(cwd).join(path).is_file())
    };
    // Preserve real executable paths containing spaces instead of inventing arguments.
    (!literal).then(|| crate::command_detection::shell_split(line))
}

fn merge_candidates(
    host: &dyn Host,
    plan: &LoadPlan,
    live: &[Client],
    protected: &HashSet<Address>,
    signatures: &[Option<(Vec<String>, Option<String>)>],
) -> HashMap<Address, usize> {
    let mut candidates = Vec::new();
    for window in live
        .iter()
        .filter(|c| c.mapped && !protected.contains(&c.address))
    {
        let info = host.process(window.pid);
        let packed = info.as_ref().and_then(packed_process_argv);
        let matching: Vec<_> = signatures
            .iter()
            .enumerate()
            .filter(|(_, signature)| {
                signature.as_ref().is_some_and(|signature| {
                    info.as_ref().is_some_and(|info| {
                        process_matches(signature, &info.argv, info.cwd.as_deref())
                            || packed.as_ref().is_some_and(|argv| {
                                process_matches(signature, argv, info.cwd.as_deref())
                            })
                    })
                })
            })
            .map(|(i, _)| i)
            .collect();
        let proven = if matching.len() == 1 {
            plan.entries[matching[0]]
                .group
                .as_ref()
                .map(|g| g.saved.as_slice())
                .unwrap_or_default()
        } else {
            &[]
        };
        let exact: Vec<_> = plan
            .saved
            .iter()
            .enumerate()
            .filter(|(_, saved)| {
                saved.title == window.title
                    && (saved.class == window.class || saved.initial_class == window.initial_class)
            })
            .map(|(i, _)| i)
            .collect();
        let titled: Vec<_> = proven
            .iter()
            .copied()
            .filter(|&i| plan.saved.get(i).is_some_and(|s| s.title == window.title))
            .collect();
        let index = if titled.len() == 1 {
            Some(titled[0])
        } else if proven.len() == 1 {
            Some(proven[0])
        } else if exact.len() == 1
            && ((matching.is_empty()
                && plan
                    .scope
                    .as_ref()
                    .is_none_or(|scope| window.workspace.matches_selector(scope)))
                || matching.iter().any(|&i| {
                    plan.entries[i]
                        .group
                        .as_ref()
                        .is_some_and(|group| group.saved.contains(&exact[0]))
                }))
        {
            Some(exact[0])
        } else {
            None
        };
        if let Some(index) = index {
            candidates.push((window.address.clone(), index));
        }
    }
    candidates
        .iter()
        .filter(|(_, index)| {
            candidates
                .iter()
                .filter(|(_, other)| other == index)
                .count()
                == 1
        })
        .cloned()
        .collect()
}

fn shared_workspace(live: &[Client], foreign: &HashSet<Address>, workspace: &str) -> bool {
    live.iter().any(|c| {
        c.mapped
            && !c.floating
            && foreign.contains(&c.address)
            && c.workspace.selector() == workspace
    })
}

/// A window belongs to the entry whose process it is; one that belongs to another entry does not
/// count for, and is not bound to, the entry being waited for
fn counts_for(st: &LoadState, address: &Address, entry: usize) -> bool {
    st.owners.get(address).is_none_or(|&owner| owner == entry)
}

/// Establish which launched entry owns each window not yet attributed. Only provable from /proc:
/// a window whose process matches no launched entry stays unattributed (timing and class decide).
fn attribute_owners(host: &dyn Host, st: &mut LoadState, live: &[Client]) {
    for window in live
        .iter()
        .filter(|c| c.mapped && !st.protected.contains(&c.address))
    {
        if st.owners.contains_key(&window.address) {
            continue;
        }
        let Some(info) = host.process(window.pid) else {
            continue;
        };
        let packed = packed_process_argv(&info);
        let mut matches = st.signatures.iter().enumerate().filter(|(_, signature)| {
            signature.as_ref().is_some_and(|signature| {
                process_matches(signature, &info.argv, info.cwd.as_deref())
                    || (signature.0.len() > 1
                        && packed.as_ref().is_some_and(|argv| {
                            process_matches(signature, argv, info.cwd.as_deref())
                        }))
            })
        });
        if let Some((index, _)) = matches.next() {
            if (host.merge_existing() || index < st.launched) && matches.next().is_none() {
                st.owners.insert(window.address.clone(), index);
            }
        }
    }
}

/// Entries that run a resumed program under the wrapper identify their window by what they
/// were asked to run, not by its title
fn is_resume_entry(entry: &ExecEntry) -> bool {
    entry.command.contains(crate::resume::WRAPPER_MARKER)
}

/// The saved windows a spawned window is bound to: only a resume launch binds by identity
fn own_saves(entry: &ExecEntry) -> &[usize] {
    match &entry.group {
        Some(group) if is_resume_entry(entry) => &group.saved,
        _ => &[],
    }
}

/// Read `key` as the first integer of its value ("12", "12,12,12,12" or "12 12 12 12")
fn option_number(host: &dyn Host, key: &str, default: f64) -> f64 {
    host.option(key)
        .and_then(|v| {
            v.split(|c: char| !c.is_ascii_digit() && c != '.')
                .find(|p| !p.is_empty())
                .map(str::to_string)
        })
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

/// The order in which a workspace's saved tiled windows (`rects`, in plan order) are to be
/// inserted: the computed dwindle insertion order when the compositor is in the configuration
/// the model was measured in, saved reading order (with the reason) otherwise. Returns the
/// pointer it computed for.
fn insertion_order(
    host: &dyn Host,
    rects: &[dwindle::Rect],
) -> (Vec<usize>, Option<String>, Option<(i32, i32)>) {
    let reading = || dwindle::reading_order(rects);
    let layout = host.option("general:layout");
    if layout.as_deref() != Some("dwindle") {
        return (
            reading(),
            Some(format!(
                "layout is {}, not dwindle",
                layout.as_deref().unwrap_or("unknown")
            )),
            None,
        );
    }
    let split = host.option("dwindle:force_split");
    if split.as_deref() != Some("2") {
        return (
            reading(),
            Some(format!(
                "dwindle:force_split is {}, not 2",
                split.as_deref().unwrap_or("unknown")
            )),
            None,
        );
    }
    let Some(pointer) = host.cursor_pos() else {
        return (
            reading(),
            Some("the pointer position is unknown".to_string()),
            None,
        );
    };
    let gaps_out = option_number(host, "general:gaps_out", dwindle::DEFAULT_GAPS_OUT);
    match dwindle::computed_order(
        rects,
        (pointer.0 as f64, pointer.1 as f64),
        gaps_out,
        dwindle::CUT_TOLERANCE,
    ) {
        Ok(order) => (order, None, Some(pointer)),
        Err(reason) => (reading(), Some(reason), Some(pointer)),
    }
}

/// How close (px) the pointer must read back to count as warped
const WARP_TOLERANCE: i32 = 3;
/// How close (px) a window must be to its saved geometry to count as reproduced; gaps and
/// rounding put saved and regrown rectangles a pixel or two apart
const GEOMETRY_TOLERANCE: i32 = 6;

fn window_at_saved_state(actual: &Client, saved: &Client, shared: bool) -> bool {
    let near =
        |a: i32, b: i32| (i64::from(a) - i64::from(b)).abs() <= i64::from(GEOMETRY_TOLERANCE);
    actual.mapped
        && (saved.pinned || actual.workspace.selector() == saved.workspace.selector())
        && actual.floating == saved.floating
        && actual.pinned == saved.pinned
        && actual.fullscreen == saved.fullscreen
        && (shared && !saved.floating
            || saved.fullscreen != FullscreenMode::None
            || (near(actual.at.0, saved.at.0)
                && near(actual.at.1, saved.at.1)
                && near(actual.size.0, saved.size.0)
                && near(actual.size.1, saved.size.1)))
}

/// Warp the pointer and read it back (the compositor applies dispatches in order, but the
/// read-back is what proves the warp was accepted at all)
fn warp_pointer(host: &dyn Host, x: i32, y: i32) -> bool {
    for _ in 0..3 {
        let _ = host.send(&Op::MoveCursor { x, y });
        if host.cursor_pos().is_some_and(|(px, py)| {
            (px - x).abs() <= WARP_TOLERANCE && (py - y).abs() <= WARP_TOLERANCE
        }) {
            return true;
        }
        host.pause();
    }
    false
}

/// Place a workspace's held windows together. Dwindle splits the leaf nearest the mouse
/// pointer when a window arrives, so with a working pointer warp the loader drives the
/// pointer into the leaf to split before each insertion and any saved tree regrows exactly,
/// whatever the user's pointer was (the pointer is restored when the load ends). Without it
/// the order is computed for the pointer as found, and failing that is the saved reading order.
fn flush_workspace(host: &dyn Host, st: &mut LoadState, key: &WsKey) {
    let mut held = st.queue.take(key);
    if held.is_empty() {
        return;
    }
    if host.merge_existing() {
        match host.clients() {
            Ok(live) => {
                let claimed = st.placer.claimed_addresses();
                st.foreign = live
                    .iter()
                    .filter(|c| c.mapped && !claimed.contains(&c.address))
                    .map(|c| c.address.clone())
                    .collect();
                if shared_workspace(&live, &st.foreign, key) {
                    for item in held {
                        if let Some(window) =
                            live.iter().find(|c| c.mapped && c.address == item.live)
                        {
                            apply(host, window, &item.saved);
                            st.report.placed += 1;
                        } else {
                            st.placer.release(item.saved_index);
                        }
                    }
                    return;
                }
            }
            Err(_) => {
                st.report.incomplete = true;
                for item in held {
                    st.placer.release(item.saved_index);
                }
                return;
            }
        }
    }
    // The tree is the workspace's whole saved tiled set, so a missing window leaves a hole
    // rather than reshaping the others' order
    let tiled: Vec<usize> = st
        .plan
        .saved
        .iter()
        .enumerate()
        .filter(|(_, c)| crate::queue::is_queued(c) && ws_key(c) == *key)
        .map(|(i, _)| i)
        .collect();
    let rects: Vec<dwindle::Rect> = tiled
        .iter()
        .map(|&i| {
            let c = &st.plan.saved[i];
            (
                c.at.0 as f64,
                c.at.1 as f64,
                f64::from(c.at.0) + f64::from(c.size.0),
                f64::from(c.at.1) + f64::from(c.size.1),
            )
        })
        .collect();

    let layout = host.option("general:layout");
    let split = host.option("dwindle:force_split");
    let mut why_not_steered = if layout.as_deref() != Some("dwindle") {
        Some(format!(
            "layout is {}, not dwindle",
            layout.as_deref().unwrap_or("unknown")
        ))
    } else if split.as_deref() != Some("2") {
        Some(format!(
            "dwindle:force_split is {}, not 2",
            split.as_deref().unwrap_or("unknown")
        ))
    } else if st.steering_refused {
        Some("the compositor did not accept a pointer warp".to_string())
    } else {
        None
    };

    // Steered: (window position in `tiled`, pointer to warp to first) in insertion order
    let mut schedule: Option<Vec<dwindle::SteerStep>> = None;
    if why_not_steered.is_none() && rects.len() >= 2 {
        // Native dwindle nodes already exclude gaps_out; clients exclude their borders too.
        let inset = option_number(host, "general:border_size", 1.0);
        match dwindle::steer_schedule(&rects, inset, dwindle::CUT_TOLERANCE) {
            Err(reason) => why_not_steered = Some(reason),
            Ok(steps) => {
                // Prove the warp works before the first window moves
                let (x, y) = steps[1].pointer.expect("second insertion has a pointer");
                if warp_pointer(host, x as i32, y as i32) {
                    st.pointer_moved_by_us = true;
                    schedule = Some(steps);
                } else {
                    st.steering_refused = true;
                    println!("Warning: the compositor did not take a pointer warp (wanted {x:.0},{y:.0}); not steering");
                    why_not_steered =
                        Some("the compositor did not accept a pointer warp".to_string());
                }
            }
        }
    }

    let rank: HashMap<usize, usize>;
    let mut fallback: Option<String> = None;
    let mut pointers: HashMap<usize, (i32, i32)> = HashMap::new();
    let mut reference_pointer = None;
    if let Some(steps) = &schedule {
        rank = steps
            .iter()
            .enumerate()
            .map(|(r, s)| (tiled[s.window], r))
            .collect();
        for step in steps {
            if let Some((x, y)) = step.pointer {
                pointers.insert(tiled[step.window], (x as i32, y as i32));
            }
        }
    } else {
        let (order, reason, pointer) = insertion_order(host, &rects);
        // a steering failure is the more useful reason when the as-found order is not computable
        fallback = reason.map(|r| match &why_not_steered {
            Some(w) => format!("{w}; {r}"),
            None => r,
        });
        reference_pointer = pointer;
        rank = order
            .iter()
            .enumerate()
            .map(|(r, &pos)| (tiled[pos], r))
            .collect();
    }
    held.sort_by_key(|h| {
        (
            rank.get(&h.saved_index).copied().unwrap_or(usize::MAX),
            h.saved_index,
        )
    });

    let steered = schedule.is_some();
    match (&fallback, steered) {
        (_, true) => println!("Workspace {}: placing {} tiled windows in steered insertion order (pointer-independent)", key, held.len()),
        (None, false) => println!("Workspace {}: placing {} tiled windows in computed insertion order", key, held.len()),
        (Some(reason), false) => {
            println!("Workspace {}: placing {} tiled windows in saved reading order ({reason})", key, held.len())
        }
    }
    // The workspace the load started on: its windows were born in its tree in arrival order, among
    // the staged windows of other workspaces that have since left. Moving them all off and back in
    // schedule order, with the pointer steered, rebuilds the tree they were saved in.
    if steered {
        let now = host.clients().unwrap_or_default();
        let here: Vec<&Client> = now
            .iter()
            .filter(|c| c.mapped && !c.floating && c.workspace.selector() == key.as_str())
            .collect();
        let mine: HashSet<&Address> = held.iter().map(|h| &h.live).collect();
        if here.iter().any(|c| !mine.contains(&c.address)) {
            println!("Warning: workspace {key} contains unrelated tiled windows; refusing to rebuild their tree");
            st.report.incomplete = true;
            for held in held {
                if let Some(window) = now.iter().find(|c| c.address == held.live) {
                    apply(host, window, &held.saved);
                    st.report.placed += 1;
                } else {
                    st.placer.release(held.saved_index);
                }
            }
            return;
        }
        let born_here = here.len();
        if born_here > 0 {
            println!(
                "Workspace {}: parking its {born_here} windows to rebuild their tree in order",
                key
            );
            for window in here {
                let _ = host.send(&Op::MoveToWorkspace {
                    window: window.address.clone(),
                    workspace: crate::dispatch::WorkspaceTarget::Special(STAGE.to_string()),
                });
            }
            host.pause();
        }
    }
    let live = host.clients().unwrap_or_default();
    let count = held.len();
    let mut last_pointer = None;
    let mut pointer_moved = false;
    for Held {
        saved_index,
        saved,
        live: address,
    } in held
    {
        match live.iter().find(|c| c.address == address) {
            Some(window) => {
                let split_step = schedule
                    .as_ref()
                    .and_then(|steps| rank.get(&saved_index).map(|&r| &steps[r]))
                    .and_then(|step| step.split.as_ref());
                let mut split_ready = true;
                if let Some(split) = split_step {
                    let anchor = st.placer.targets[tiled[split.anchor]].matched.as_ref();
                    let current = host.clients().unwrap_or_default();
                    if let Some(anchor) = anchor.filter(|a| {
                        current
                            .iter()
                            .any(|c| &c.address == *a && c.workspace.selector() == key)
                    }) {
                        if host
                            .send(&Op::FocusWorkspace(key.clone()))
                            .and_then(|_| host.send(&Op::FocusWindow(anchor.clone())))
                            .is_err()
                        {
                            st.report.incomplete = true;
                            split_ready = false;
                        }
                    } else {
                        st.report.incomplete = true;
                        split_ready = false;
                    }
                }
                if let Some(&(x, y)) = pointers.get(&saved_index).filter(|_| split_ready) {
                    if warp_pointer(host, x, y) {
                        last_pointer = Some((x, y));
                    } else {
                        println!(
                            "Warning: pointer warp to {x},{y} was not accepted while placing '{}'",
                            saved.title
                        );
                    }
                }
                apply(host, window, &saved);
                if let (Some((bx, by)), Some((ax, ay))) = (last_pointer, host.cursor_pos()) {
                    pointer_moved |=
                        (bx - ax).abs() > WARP_TOLERANCE || (by - ay).abs() > WARP_TOLERANCE;
                }
                if let Some(step) = schedule
                    .as_ref()
                    .filter(|_| split_ready)
                    .and_then(|steps| rank.get(&saved_index).map(|&r| &steps[r]))
                {
                    if let Some(split) = &step.split {
                        let anchor = st.placer.targets[tiled[split.anchor]].matched.as_ref();
                        let current = host.clients().unwrap_or_default();
                        let first = anchor.and_then(|a| current.iter().find(|c| &c.address == a));
                        let second = current.iter().find(|c| c.address == address);
                        if let (Some(first), Some(second)) = (first, second) {
                            let vertical = first.at.0 != second.at.0;
                            let result = host
                                .send(&Op::FocusWorkspace(key.clone()))
                                .and_then(|_| host.send(&Op::FocusWindow(address.clone())))
                                .and_then(|_| {
                                    if vertical != (split.orient == dwindle::Orient::V) {
                                        host.send(&Op::Layout("togglesplit".into()))?;
                                    }
                                    host.send(&Op::Layout(format!(
                                        "splitratio {} exact",
                                        split.ratio
                                    )))
                                });
                            if result.is_err() {
                                st.report.incomplete = true;
                            }
                            last_pointer = host.cursor_pos();
                        } else {
                            st.report.incomplete = true;
                        }
                    }
                }
                st.report.placed += 1;
            }
            None => {
                println!("Warning: '{}' vanished before it was placed", saved.title);
                st.placer.release(saved_index);
            }
        }
    }

    // The pointer is only suspect if something other than the loader moved it
    let pointer_moved = pointer_moved
        || match (last_pointer.or(reference_pointer), host.cursor_pos()) {
            (Some((bx, by)), Some((ax, ay))) => {
                (bx - ax).abs() > WARP_TOLERANCE || (by - ay).abs() > WARP_TOLERANCE
            }
            _ => false,
        };
    if pointer_moved {
        println!("Warning: the pointer moved while workspace {} was placed; its order may not have been reproduced", key);
    }

    st.report.flushes.push(FlushReport {
        workspace: key.clone(),
        windows: count,
        fallback,
        steered,
        pointer_moved,
    });
}

/// Flush every workspace whose queue is ready. The workspace the load started on waits for the
/// end: windows are born on it, so it can only be rebuilt once the other workspaces have taken
/// their staged windows away.
fn flush_ready(host: &dyn Host, st: &mut LoadState) {
    for key in st.queue.ready() {
        if !st.queue.is_launching() || Some(key.as_str()) != st.start_ws.as_deref() {
            flush_workspace(host, st, &key);
        }
    }
}

fn flush_all(host: &dyn Host, st: &mut LoadState) {
    let mut keys = st.queue.all_held();
    // the start workspace last
    keys.sort_by_key(|k| Some(k.as_str()) == st.start_ws.as_deref());
    for key in keys {
        flush_workspace(host, st, &key);
    }
}

/// Place every live window that claims a saved client; returns the windows claimed now.
/// Tiled claims are held and placed together once their workspace is ready.
fn place_live(
    host: &dyn Host,
    st: &mut LoadState,
    live: &[Client],
    phase: Option<Phase>,
) -> Vec<Address> {
    if host.merge_existing() {
        st.owners.clear();
        let mut excluded = st.protected.clone();
        excluded.extend(st.placer.claimed_addresses());
        st.initial = merge_candidates(host, st.plan, live, &excluded, &st.signatures);
    }
    attribute_owners(host, st, live);
    if host.merge_existing() {
        let claimed = st.placer.claimed_addresses();
        st.foreign = live
            .iter()
            .filter(|c| {
                c.mapped
                    && !claimed.contains(&c.address)
                    && !st.initial.contains_key(&c.address)
                    && !st.owners.contains_key(&c.address)
            })
            .map(|c| c.address.clone())
            .collect();
    }
    let mut claimed = vec![];
    for window in live
        .iter()
        .filter(|c| c.mapped && !st.protected.contains(&c.address))
    {
        // Bound by provenance when /proc proves whose window it is, else by the resume phase
        let owned: &[usize] = st
            .owners
            .get(&window.address)
            .and_then(|&i| st.plan.entries[i].group.as_ref())
            .map(|g| g.saved.as_slice())
            .unwrap_or_default();
        let assignment = if !owned.is_empty() {
            st.placer.assign_proven(window, owned)
        } else if let Some(&index) = st.initial.get(&window.address) {
            st.placer.assign_proven(window, &[index])
        } else if host.merge_existing() {
            None
        } else {
            let own = match phase {
                Some(p) if !p.before.contains(&window.address) => p.own,
                _ => &[],
            };
            st.placer.assign_bound(window, own, &st.reserved)
        };
        if let Some((saved_index, wanted)) = assignment {
            st.foreign.remove(&window.address);
            if st.queue.holds(&wanted)
                && !shared_workspace(live, &st.foreign, wanted.workspace.selector())
            {
                // Keep new arrivals out of every workspace's partially reconstructed tree.
                if host
                    .send(&Op::MoveToWorkspace {
                        window: window.address.clone(),
                        workspace: crate::dispatch::WorkspaceTarget::Special(STAGE.into()),
                    })
                    .is_err()
                {
                    st.report.incomplete = true;
                }
                st.queue.hold(Held {
                    saved_index,
                    saved: wanted,
                    live: window.address.clone(),
                });
            } else {
                apply(host, window, &wanted);
                st.report.placed += 1;
            }
            claimed.push(window.address.clone());
        }
    }
    flush_ready(host, st);
    claimed
}

/// Wait for a launch group's window count to hold still (or reach `expected`),
/// placing windows as they arrive; `claimed` counts the group's own new windows
fn settle_group(
    host: &dyn Host,
    st: &mut LoadState,
    before: &HashSet<Address>,
    own: &[usize],
    index: usize,
    expected: usize,
    claimed: &mut usize,
) {
    let mut stable = 0;
    while *claimed < expected && stable < SETTLE_TICKS {
        host.pause();
        stable += 1;
        let Ok(live) = host.clients() else { continue };
        let new = place_live(host, st, &live, Some(Phase { before, own }))
            .into_iter()
            .filter(|a| !before.contains(a) && counts_for(st, a, index))
            .count();
        if new > 0 {
            *claimed += new;
            stable = 0;
        }
    }
}

/// Make a launch group produce as many windows as it had saved. Once the group's windows
/// have settled, a shortfall is met by re-invoking the group's command, at most once per
/// missing window; the newcomers are placed through the Placer like any other window. A
/// group that produced nothing is a failed launch and is only reported, never re-invoked.
fn fulfil_group(
    host: &dyn Host,
    st: &mut LoadState,
    entry: &ExecEntry,
    index: usize,
    group: &GroupInfo,
    before: &HashSet<Address>,
    mut claimed: usize,
) {
    let expected = group.saved.len();
    let own = own_saves(entry);
    settle_group(host, st, before, own, index, expected, &mut claimed);
    let mut reinvocations = 0;
    if claimed > 0 {
        let bound = expected.saturating_sub(claimed);
        while reinvocations < bound && claimed < expected {
            reinvocations += 1;
            println!(
                "'{}': {claimed} of {expected} windows; launching it again",
                group.label
            );
            if host.send(&Op::Exec(entry.command.clone())).is_err() {
                break;
            }
            settle_group(host, st, before, own, index, expected, &mut claimed);
        }
    }
    if claimed < expected {
        println!(
            "Warning: '{}' produced {claimed} of {expected} saved windows after {reinvocations} extra launches",
            group.label
        );
        st.report.shortfalls.push(GroupShortfall {
            label: group.label.clone(),
            expected,
            got: claimed,
            reinvocations,
        });
    }
}

/// Independent GUI launches share one wait budget; a slow app cannot serialize the others.
fn launch_grouped(host: &dyn Host, st: &mut LoadState) {
    let plan = st.plan;
    #[derive(Default)]
    struct Progress {
        count: usize,
        stable: u64,
        reinvocations: usize,
        limit: Option<usize>,
        done: bool,
        tray_checked: bool,
    }
    let mut progress: Vec<Progress> = st
        .plan
        .entries
        .iter()
        .map(|_| Progress::default())
        .collect();
    for (index, entry) in plan.entries.iter().enumerate() {
        if let Ok(live) = host.clients() {
            place_live(host, st, &live, None);
        }
        st.launched = index + 1;
        let group = entry.group.as_ref().expect("grouped launches");
        if group
            .saved
            .iter()
            .all(|&i| st.placer.targets[i].matched.is_some())
        {
            progress[index].done = true;
            st.queue.entry_done(&st.plan.saved, entry);
        } else if let Err(error) = host.send(&Op::Exec(entry.command.clone())) {
            println!("Warning: failed to launch '{}': {error}", entry.command);
            progress[index].done = true;
            st.queue.entry_done(&st.plan.saved, entry);
        }
    }
    st.queue.launches_sent();
    for _ in 0..LAUNCH_WAIT_TICKS.max(st.plan.tail_ticks) {
        host.pause();
        if let Ok(live) = host.clients() {
            place_live(host, st, &live, None);
        }
        for (index, (entry, state)) in st.plan.entries.iter().zip(&mut progress).enumerate() {
            let group = entry.group.as_ref().expect("grouped launches");
            let count = group
                .saved
                .iter()
                .filter(|&&i| st.placer.targets[i].matched.is_some())
                .count();
            if count == group.saved.len() {
                if !state.done {
                    st.queue.entry_done(&st.plan.saved, entry);
                    state.done = true;
                }
            } else if !state.done {
                state.stable = if count == state.count {
                    state.stable + 1
                } else {
                    0
                };
                if count == 0 && state.stable >= SETTLE_TICKS && !state.tray_checked {
                    state.tray_checked = true;
                    if let Some(signature) = &st.signatures[index] {
                        let unique = st
                            .signatures
                            .iter()
                            .filter(|s| s.as_ref() == Some(signature))
                            .count()
                            == 1;
                        if unique && host.reveal_tray_window(entry) {
                            println!(
                                "Revealing saved GUI from verified tray process: '{}'",
                                group.label
                            );
                        }
                    }
                }
                if count > 0 && state.stable >= SETTLE_TICKS {
                    let limit = *state.limit.get_or_insert(group.saved.len() - count);
                    if state.reinvocations < limit {
                        let _ = host.send(&Op::Exec(entry.command.clone()));
                        state.reinvocations += 1;
                        state.stable = 0;
                    }
                }
            }
            state.count = count;
        }
        flush_ready(host, st);
        if st.placer.is_complete() {
            break;
        }
    }
    for (entry, state) in st.plan.entries.iter().zip(progress) {
        let group = entry.group.as_ref().expect("grouped launches");
        if state.count < group.saved.len() {
            st.report.shortfalls.push(GroupShortfall {
                label: group.label.clone(),
                expected: group.saved.len(),
                got: state.count,
                reinvocations: state.reinvocations,
            });
        }
    }
}

/// Run a load: launch each entry in order waiting for its window, claiming windows as
/// they appear, then keep claiming until everything saved is matched or time runs out.
/// Tiled windows are placed per workspace (see `PlacementQueue`); floating ones on claim.
pub fn run_load(host: &dyn Host, plan: &LoadPlan) -> LoadReport {
    let initial_live = if host.merge_existing() || plan.scope.is_some() {
        match host.clients() {
            Ok(clients) => clients,
            Err(_) => {
                return LoadReport {
                    unmatched: plan.saved.clone(),
                    incomplete: true,
                    ..LoadReport::default()
                }
            }
        }
    } else {
        Vec::new()
    };
    let protected: HashSet<Address> = initial_live
        .iter()
        .filter(|c| {
            plan.scope
                .as_ref()
                .is_some_and(|workspace| !c.workspace.matches_selector(workspace))
        })
        .map(|c| c.address.clone())
        .collect();
    let initial = HashMap::new();
    let foreign = HashSet::new();
    let mut st = LoadState {
        plan,
        placer: Placer::new(if plan.place_by_class {
            plan.saved.clone()
        } else {
            vec![]
        }),
        queue: if plan.place_by_class {
            PlacementQueue::new(&plan.saved, &plan.entries)
        } else {
            PlacementQueue::disabled()
        },
        protected,
        initial,
        foreign,
        pointer_home: if plan.place_by_class {
            host.pointer_origin()
        } else {
            None
        },
        pointer_moved_by_us: false,
        steering_refused: false,
        start_ws: if plan.place_by_class {
            host.active_workspace()
        } else {
            None
        },
        launched: 0,
        signatures: plan.entries.iter().map(entry_signature).collect(),
        owners: HashMap::new(),
        reserved: if plan.place_by_class {
            plan.entries
                .iter()
                .filter(|e| is_resume_entry(e))
                .filter_map(|e| e.group.as_ref())
                .flat_map(|g| g.saved.iter().copied())
                .collect()
        } else {
            HashSet::new()
        },
        report: LoadReport::default(),
    };
    if plan.place_by_class {
        if let Some(ws) = &st.start_ws {
            println!("Start workspace: {ws} (windows destined for it are born there and are rebuilt in order at the end)");
        }
        match st.pointer_home {
            Some((x, y)) => println!("Pointer at load start: {x},{y}"),
            None => println!("Pointer at load start: unknown (it will not be restored)"),
        }
    }
    if host.merge_existing() && plan.place_by_class {
        match host.clients() {
            Ok(live) => {
                place_live(host, &mut st, &live, None);
            }
            Err(_) => {
                return LoadReport {
                    unmatched: plan.saved.clone(),
                    incomplete: true,
                    ..LoadReport::default()
                };
            }
        }
    }

    let concurrent = plan.place_by_class
        && !plan.entries.is_empty()
        && plan
            .entries
            .iter()
            .all(|entry| entry.group.is_some() && !is_resume_entry(entry));
    if concurrent {
        launch_grouped(host, &mut st);
    } else {
        for (index, entry) in plan.entries.iter().enumerate() {
            if host.merge_existing() && plan.place_by_class {
                if let Ok(live) = host.clients() {
                    place_live(host, &mut st, &live, None);
                }
            }
            if host.merge_existing()
                && entry.group.as_ref().is_some_and(|group| {
                    group
                        .saved
                        .iter()
                        .all(|&i| st.placer.targets[i].matched.is_some())
                })
            {
                st.queue.entry_done(&plan.saved, entry);
                continue;
            }
            let before: HashSet<Address> = host
                .clients()
                .map(|c| c.into_iter().map(|c| c.address).collect())
                .unwrap_or_default();
            st.launched = index + 1;
            if let Err(e) = host.send(&Op::Exec(entry.command.clone())) {
                println!("Warning: failed to launch '{}': {e}", entry.command);
                st.queue.entry_done(&plan.saved, entry);
                flush_ready(host, &mut st);
                continue;
            }
            let mut claimed = if host.merge_existing() {
                entry.group.as_ref().map_or(0, |group| {
                    group
                        .saved
                        .iter()
                        .filter(|&&i| st.placer.targets[i].matched.is_some())
                        .count()
                })
            } else {
                0
            };
            for _ in 0..LAUNCH_WAIT_TICKS {
                host.pause();
                let Ok(live) = host.clients() else { continue };
                if plan.place_by_class {
                    let own = own_saves(entry);
                    claimed += place_live(
                        host,
                        &mut st,
                        &live,
                        Some(Phase {
                            before: &before,
                            own,
                        }),
                    )
                    .into_iter()
                    .filter(|a| !before.contains(a) && counts_for(&st, a, index))
                    .count();
                }
                // a window that /proc shows belongs to another entry is not the one being waited for
                let new_window = live.iter().find(|c| {
                    c.mapped && !before.contains(&c.address) && counts_for(&st, &c.address, index)
                });
                if let Some(window) = new_window {
                    if !plan.place_by_class {
                        apply(host, window, &desired_from_rules(window, &entry.rules));
                        st.report.placed += 1;
                    }
                    break;
                }
            }
            if let (true, Some(group)) = (plan.place_by_class, entry.group.as_ref()) {
                fulfil_group(host, &mut st, entry, index, group, &before, claimed);
            }
            st.queue.entry_done(&plan.saved, entry);
            flush_ready(host, &mut st);
        }
    }

    if plan.place_by_class {
        let mut ticks = 0;
        let tail_ticks = if concurrent { 0 } else { plan.tail_ticks };
        while !st.placer.is_complete() && ticks < tail_ticks {
            host.pause();
            ticks += 1;
            if let Ok(live) = host.clients() {
                place_live(host, &mut st, &live, None);
            }
        }
        // Nothing claimed may stay held, whatever the workspace's launches did
        flush_all(host, &mut st);
        st.report.unmatched = st.placer.unmatched().into_iter().cloned().collect();
        for client in &st.report.unmatched {
            println!(
                "Warning: no window appeared for saved client '{}' ({})",
                client.title, client.class
            );
        }
        if let Ok(live) = host.clients() {
            let claimed = st.placer.claimed_addresses();
            st.report.surplus = live
                .into_iter()
                .filter(|c| {
                    c.mapped && !claimed.contains(&c.address) && !st.protected.contains(&c.address)
                })
                .collect();
        }
    }

    // Focus first: changing workspace can itself move the pointer, so the pointer goes back last
    if let Some(workspace) = &plan.restore_workspace {
        let _ = host.send(&Op::FocusWorkspace(workspace.clone()));
    }
    if st.pointer_moved_by_us {
        match st.pointer_home {
            Some((x, y)) => {
                if warp_pointer(host, x, y) {
                    println!("Pointer restored to {x},{y}");
                } else {
                    println!(
                        "Warning: could not put the pointer back at {x},{y} (now at {:?})",
                        host.cursor_pos()
                    );
                }
            }
            None => {
                println!("Warning: the pointer was steered but its starting position was unknown")
            }
        }
    }
    verify_final_geometry(host, &mut st);
    st.report
}

/// Verify the final compositor state, not merely successful dispatch replies.
fn verify_final_geometry(host: &dyn Host, st: &mut LoadState) {
    if !st.plan.place_by_class || st.report.placed == 0 {
        return;
    }
    host.pause();
    host.pause();
    let Ok(mut after) = host.clients() else {
        st.report.incomplete = true;
        return;
    };
    let mut live: HashMap<_, _> = after.iter().map(|c| (&c.address, c)).collect();
    // Applications can deliver their initial maximize request after mapping and
    // placement. Finalize the saved state once those startup configures settle.
    let mut finalized = false;
    for target in &st.placer.targets {
        let Some(address) = &target.matched else {
            continue;
        };
        if let Some(actual) = live
            .get(address)
            .filter(|c| c.mapped && c.fullscreen != target.saved.fullscreen)
        {
            apply(host, actual, &target.saved);
            finalized = true;
        }
    }
    if finalized {
        host.pause();
        let Ok(current) = host.clients() else {
            st.report.incomplete = true;
            return;
        };
        drop(live);
        after = current;
        live = after.iter().map(|c| (&c.address, c)).collect();
    }
    // Await configure/animation completion, without relaunching or redispatching.
    for _ in 0..(2 * TICKS_PER_SECOND) {
        if host.merge_existing() {
            let claimed = st.placer.claimed_addresses();
            st.foreign = after
                .iter()
                .filter(|c| c.mapped && !claimed.contains(&c.address))
                .map(|c| c.address.clone())
                .collect();
        }
        if st.placer.targets.iter().all(|target| {
            target.matched.as_ref().is_none_or(|address| {
                live.get(address).is_some_and(|actual| {
                    window_at_saved_state(
                        actual,
                        &target.saved,
                        shared_workspace(&after, &st.foreign, target.saved.workspace.selector()),
                    )
                })
            })
        }) {
            break;
        }
        host.pause();
        let Ok(current) = host.clients() else {
            st.report.incomplete = true;
            return;
        };
        drop(live);
        after = current;
        live = after.iter().map(|c| (&c.address, c)).collect();
    }
    if host.merge_existing() {
        let claimed = st.placer.claimed_addresses();
        st.foreign = after
            .iter()
            .filter(|c| c.mapped && !claimed.contains(&c.address))
            .map(|c| c.address.clone())
            .collect();
    }
    let mut by_ws: std::collections::BTreeMap<&str, WorkspaceCheck> = Default::default();
    for target in &st.placer.targets {
        let Some(address) = &target.matched else {
            continue;
        };
        let saved = &target.saved;
        let check = by_ws
            .entry(saved.workspace.selector())
            .or_insert_with(|| WorkspaceCheck {
                workspace: saved.workspace.selector().to_owned(),
                at_saved: 0,
                checked: 0,
                off: vec![],
            });
        check.checked += 1;
        let ok = live.get(address).is_some_and(|actual| {
            window_at_saved_state(
                actual,
                saved,
                shared_workspace(&after, &st.foreign, saved.workspace.selector()),
            )
        });
        if ok {
            check.at_saved += 1;
        } else {
            check.off.push(saved.title.clone());
            st.report.incomplete = true;
        }
    }
    st.report.verification = by_ws.into_values().collect();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::client;
    use std::cell::{Cell, RefCell};

    #[test]
    fn test_parse_exec_line_rules_and_command() {
        let e = parse_exec_line(
            "[monitor 1;workspace 3 silent;float;move 10 20;size 800 600;pin;fullscreenstate 2] kitty --title 'a b'",
        ).unwrap();
        assert_eq!(e.command, "kitty --title 'a b'");
        assert_eq!(e.rules.monitor, Some(1));
        assert_eq!(e.rules.workspace, Some(RuleWorkspace::Id(3)));
        assert!(e.rules.float && e.rules.pin);
        assert_eq!(e.rules.at, Some((10, 20)));
        assert_eq!(e.rules.size, Some((800, 600)));
        assert_eq!(e.rules.fullscreen, Some(2));
    }

    #[test]
    fn test_parse_exec_line_edge_cases() {
        // Brackets inside the command are not mistaken for the end of the rules
        assert_eq!(
            parse_exec_line("[workspace 1 silent] sh -c 'echo [x]'")
                .unwrap()
                .command,
            "sh -c 'echo [x]'"
        );
        assert_eq!(
            parse_exec_line("[] true").unwrap().rules,
            ExecRules::default()
        );
        assert_eq!(parse_exec_line("true").unwrap().command, "true");
        assert_eq!(
            parse_exec_line("[workspace special silent] x")
                .unwrap()
                .rules
                .workspace,
            Some(RuleWorkspace::Special(None))
        );
        assert_eq!(
            parse_exec_line("[workspace special:scratch silent] x")
                .unwrap()
                .rules
                .workspace,
            Some(RuleWorkspace::Special(Some("scratch".to_string())))
        );
        assert_eq!(
            parse_exec_line("[someday 4] x").unwrap().rules.unknown,
            vec!["someday 4"]
        );
    }

    #[test]
    fn test_parse_exec_line_rejects_malformed() {
        assert!(parse_exec_line("[monitor ;workspace silent;move size] unknown_command").is_err());
        assert!(parse_exec_line("[workspace 1 silent kitty").is_err());
        assert!(parse_exec_line("[workspace 1 silent]").is_err());
        assert!(parse_exec_line("[move 1] x").is_err());
        let err = parse_exec_conf("[workspace 1 silent] ok\n\n[move a b] bad\n").unwrap_err();
        assert!(err.contains("line 3"), "{err}");
    }

    fn saved(
        class: &str,
        argv: Option<&[&str]>,
        ws: i32,
        floating: bool,
        at: (i32, i32),
    ) -> SavedClient {
        SavedClient {
            client: client(class, "t", ws, floating, at, (100, 50)),
            argv: argv.map(|a| a.iter().map(|s| s.to_string()).collect()),
            cwd: Some("/home/synthetic".to_string()),
            foreground: None,
        }
    }

    #[test]
    fn test_launch_entries_from_saved_argv() {
        let entries = launch_entries_from_saved_no_xdg(&[
            saved(
                "brave",
                Some(&["/opt/brave/brave", "--app=https://example.test/a b"]),
                5,
                false,
                (0, 0),
            ),
            // Another window of the same process: same command, not launched again
            saved(
                "brave",
                Some(&["/opt/brave/brave", "--app=https://example.test/a b"]),
                6,
                true,
                (9, 9),
            ),
            // A second instance with its own profile is a distinct launch
            saved(
                "brave",
                Some(&["/opt/brave/brave", "--user-data-dir=/home/x/p"]),
                1,
                false,
                (3, 4),
            ),
            // No argv: cannot be launched
            saved("ghost", None, 2, false, (0, 0)),
        ]);
        let commands: Vec<&str> = entries.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(
            commands,
            vec![
                "/opt/brave/brave '--app=https://example.test/a b'",
                "/opt/brave/brave --user-data-dir=/home/x/p",
            ]
        );
        // Rules come from the first window carrying each command
        assert_eq!(entries[0].rules.workspace, Some(RuleWorkspace::Id(5)));
        assert!(!entries[0].rules.float);
        assert_eq!(entries[1].rules.at, Some((3, 4)));
    }

    fn launch_entries_from_saved_no_xdg(saved: &[SavedClient]) -> Vec<ExecEntry> {
        launch_entries_from_saved(saved, &HashMap::new())
    }

    fn saved_pid(class: &str, argv: &[&str], pid: i32, at: (i32, i32)) -> SavedClient {
        let mut window = saved(class, Some(argv), 1, false, at);
        window.client.pid = pid;
        window
    }

    #[test]
    fn test_single_element_argv_is_used_verbatim() {
        // Chromium-family cmdline: one space-joined string with shell quotes embedded
        let blob = r#"/opt/electronmail/electron-mail --class=electron-mail --name="Electron Mail" --user-data-dir='/home/x/em data'"#;
        let entries =
            launch_entries_from_saved_no_xdg(&[saved_pid("electron-mail", &[blob], 7, (0, 0))]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, blob);
        // Multi-element argv is still shell-joined
        let entries = launch_entries_from_saved_no_xdg(&[saved_pid("x", &["a b", "c"], 8, (0, 0))]);
        assert_eq!(entries[0].command, "'a b' c");
        // A blank single element is not a command
        assert!(launch_entries_from_saved_no_xdg(&[saved_pid("x", &["  "], 9, (0, 0))]).is_empty());
    }

    #[test]
    fn test_same_command_in_separate_processes_is_launched_per_pid() {
        let entries = launch_entries_from_saved_no_xdg(&[
            saved_pid("foot", &["foot"], 101, (0, 0)),
            saved_pid("foot", &["foot"], 102, (10, 0)),
            saved_pid("foot", &["foot"], 103, (20, 0)),
        ]);
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.command == "foot"));
        // Rules come from the group's first window
        assert!(entries.iter().all(|e| e.rules.at == Some((0, 0))));
    }

    #[test]
    fn test_two_windows_of_one_pid_are_one_launch() {
        let entries = launch_entries_from_saved_no_xdg(&[
            saved_pid("brave", &["brave --profile-directory=Default"], 55, (0, 0)),
            saved_pid("brave", &["brave --profile-directory=Default"], 55, (30, 0)),
            saved_pid("brave", &["brave --profile-directory=Default"], 56, (60, 0)),
        ]);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_sandbox_internal_argv_launches_through_the_desktop_entry() {
        let mut xdg = HashMap::new();
        xdg.insert(
            crate::command_faker::xdg_id_key("dev.vencord.vesktop"),
            "/usr/bin/flatpak run --command=vesktop dev.vencord.Vesktop".to_string(),
        );
        let mut vesktop = saved_pid(
            "dev.vencord.Vesktop",
            &["/app/bin/vesktop/vesktop.bin --no-sandbox"],
            40,
            (0, 0),
        );
        vesktop.client.initial_class = "dev.vencord.Vesktop".to_string();
        let mut vesktop2 = vesktop.clone();
        vesktop2.client.pid = 41;
        let host_app = saved_pid("kitty", &["/usr/bin/kitty --single-instance"], 42, (0, 0));
        let entries = launch_entries_from_saved(&[vesktop.clone(), vesktop2, host_app], &xdg);
        let commands: Vec<&str> = entries.iter().map(|e| e.command.as_str()).collect();
        // The entry command replaces the blob; multiplicity is still per pid; the host app is untouched
        assert_eq!(
            commands,
            vec![
                "/usr/bin/flatpak run --command=vesktop dev.vencord.Vesktop",
                "/usr/bin/flatpak run --command=vesktop dev.vencord.Vesktop",
                "/usr/bin/kitty --single-instance",
            ]
        );
        // Multi-element sandbox argv is recognised too
        let multi = saved_pid(
            "dev.vencord.Vesktop",
            &["/app/bin/vesktop", "--x"],
            43,
            (0, 0),
        );
        let entries = launch_entries_from_saved(&[multi], &xdg);
        assert!(entries[0].command.starts_with("/usr/bin/flatpak run"));
        // With no desktop entry the recorded command is kept
        let entries = launch_entries_from_saved(&[vesktop], &HashMap::new());
        assert_eq!(
            entries[0].command,
            "/app/bin/vesktop/vesktop.bin --no-sandbox"
        );
    }

    fn saved_cwd(argv: &[&str], pid: i32, cwd: Option<&str>) -> SavedClient {
        let mut window = saved_pid("app", argv, pid, (0, 0));
        window.cwd = cwd.map(str::to_string);
        window
    }

    #[test]
    fn test_cwd_wraps_every_launch_with_a_recorded_directory() {
        let exists = |_: &Path| true;
        let entries = launch_entries_with(
            &[
                // The playcall pattern: a relative-path launch
                saved_cwd(
                    &["./playcall-gui"],
                    1,
                    Some("/home/synthetic/Work/playcall-gui"),
                ),
                // Not only relative launches (DEC-1), and paths needing quoting
                saved_cwd(
                    &["/usr/bin/kitty", "--single-instance"],
                    2,
                    Some("/home/synthetic/my dir"),
                ),
                // Legacy: no recorded cwd launches exactly as in v1
                saved_cwd(&["mpv"], 3, None),
            ],
            &HashMap::new(),
            &exists,
            &ResumeConfig::default(),
        );
        let commands: Vec<&str> = entries.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(
            commands,
            vec![
                "cd /home/synthetic/Work/playcall-gui && ./playcall-gui",
                "cd '/home/synthetic/my dir' && /usr/bin/kitty --single-instance",
                "mpv",
            ]
        );
    }

    #[test]
    fn test_missing_cwd_launches_unwrapped() {
        let gone = |_: &Path| false;
        let entries = launch_entries_with(
            &[saved_cwd(&["./playcall-gui"], 1, Some("/gone"))],
            &HashMap::new(),
            &gone,
            &ResumeConfig::default(),
        );
        assert_eq!(entries[0].command, "./playcall-gui");
    }

    #[test]
    fn test_sandbox_launches_skip_the_cwd_wrap_silently() {
        let mut xdg = HashMap::new();
        xdg.insert(
            crate::command_faker::xdg_id_key("dev.vencord.vesktop"),
            "/usr/bin/flatpak run dev.vencord.Vesktop".to_string(),
        );
        let mut vesktop = saved_cwd(&["/app/bin/vesktop"], 5, Some("/app/some/sandbox/dir"));
        vesktop.client.initial_class = "dev.vencord.Vesktop".to_string();
        let exists = |_: &Path| true;
        let entries = launch_entries_with(&[vesktop], &xdg, &exists, &ResumeConfig::default());
        assert_eq!(
            entries[0].command,
            "/usr/bin/flatpak run dev.vencord.Vesktop"
        );
    }

    #[test]
    fn test_wrapped_relative_launch_runs_from_its_directory() {
        // Real shell: a relative-path program only runs because of the cd
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let script = project.path().join("playcall-gui");
        std::fs::write(&script, "#!/bin/sh\npwd > ran-from\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let exists = |p: &Path| p.is_dir();
        let entries = launch_entries_with(
            &[saved_cwd(
                &["./playcall-gui"],
                1,
                Some(project.path().to_str().unwrap()),
            )],
            &HashMap::new(),
            &exists,
            &ResumeConfig::default(),
        );
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&entries[0].command)
            .current_dir(elsewhere.path())
            .status()
            .unwrap();
        assert!(status.success());
        let ran_from = std::fs::read_to_string(project.path().join("ran-from")).unwrap();
        assert_eq!(
            std::fs::canonicalize(ran_from.trim()).unwrap(),
            std::fs::canonicalize(project.path()).unwrap()
        );

        // Without the wrap the same launch fails from the loader's directory
        let bare = std::process::Command::new("sh")
            .arg("-c")
            .arg("./playcall-gui")
            .current_dir(elsewhere.path())
            .output()
            .unwrap();
        assert!(!bare.status.success());
    }

    #[test]
    fn test_launch_order() {
        let order = |lines: &[&str]| -> Vec<String> {
            launch_order(lines.iter().map(|l| parse_exec_line(l).unwrap()).collect())
                .into_iter()
                .map(|e| e.command)
                .collect()
        };
        assert_eq!(
            order(&[
                "[workspace 2 silent;move 0 0] b",
                "[workspace 1 silent;float;move 0 0] float1",
                "[workspace 1 silent;move 900 0] right",
                "[workspace 1 silent;move 0 500] lower",
                "[workspace 1 silent;move 0 0] left",
                "[workspace special silent] special",
            ]),
            vec!["left", "lower", "right", "float1", "b", "special"]
        );
    }

    #[test]
    fn test_placer_matches_by_class_then_order_with_title_tiebreak() {
        let mut placer = Placer::new(vec![
            client("brave", "one", 1, false, (0, 0), (1, 1)),
            client("brave", "two", 2, false, (0, 0), (1, 1)),
            client("kitty", "term", 3, false, (0, 0), (1, 1)),
        ]);
        // A brave window titled "two" claims the "two" client even though "one" is first
        let two = client("brave", "two", 9, false, (5, 0), (1, 1));
        assert_eq!(placer.assign(&two).unwrap().workspace.selector(), "2");
        // Re-presenting the same window claims nothing more
        assert!(placer.assign(&two).is_none());
        // A differently titled brave window takes the remaining one, in saved order
        let other = client("brave", "changed title", 9, false, (6, 0), (1, 1));
        assert_eq!(placer.assign(&other).unwrap().workspace.selector(), "1");
        // A third brave window has nothing left to claim
        assert!(placer
            .assign(&client("brave", "x", 9, false, (7, 0), (1, 1)))
            .is_none());
        assert!(!placer.is_complete());
        assert_eq!(placer.unmatched().len(), 1);
        assert!(placer
            .assign(&client("kitty", "whatever", 9, false, (8, 0), (1, 1)))
            .is_some());
        assert!(placer.is_complete());
    }

    #[test]
    fn test_same_title_windows_are_placed_in_order() {
        let mut placer = Placer::new(vec![
            client("kitty", "zsh", 1, false, (0, 0), (1, 1)),
            client("kitty", "zsh", 2, false, (0, 0), (1, 1)),
        ]);
        assert_eq!(
            placer
                .assign(&client("kitty", "zsh", 9, false, (1, 0), (1, 1)))
                .unwrap()
                .workspace
                .selector(),
            "1"
        );
        assert_eq!(
            placer
                .assign(&client("kitty", "zsh", 9, false, (2, 0), (1, 1)))
                .unwrap()
                .workspace
                .selector(),
            "2"
        );
    }

    /// A compositor whose launches make scripted windows appear after a delay in ticks
    struct FakeHost {
        tick: Cell<u64>,
        merge: bool,
        windows: RefCell<Vec<(u64, Client)>>,
        scripts: RefCell<std::collections::VecDeque<Vec<(u64, Client)>>>,
        sent: RefCell<Vec<Op>>,
        pauses: Cell<u64>,
        signals: RefCell<Vec<(i32, Signal)>>,
        /// pid -> how much it takes to remove its windows: 0 close, 1 SIGTERM, 2 SIGKILL, 3 never
        resist: RefCell<std::collections::HashMap<i32, u8>>,
        options: RefCell<std::collections::HashMap<String, String>>,
        cursor: Cell<Option<(i32, i32)>>,
        /// Where the pointer jumps to when the first window is moved to a workspace
        drift_to: Cell<Option<(i32, i32)>>,
        /// (number of Execs sent, address): the window disappears once that many launches happened
        hide: RefCell<Option<(usize, String)>>,
        /// Whether the compositor accepts a pointer warp
        warp: Cell<bool>,
        /// A dwindle layout simulation: moving a window onto a workspace splits the leaf nearest the pointer
        sim: Cell<bool>,
        /// A destination workspace carries its previous maximized state into arrivals.
        inherit_maximized: Cell<bool>,
        /// Geometry acknowledgements delivered after placement.
        geometry_updates: RefCell<Vec<(u64, Client)>>,
        title_updates: RefCell<Vec<(u64, Address, String)>>,
        tray_window: RefCell<Option<Client>>,
        /// Process info per window pid, for launch provenance
        procs: RefCell<std::collections::HashMap<i32, ProcessInfo>>,
        /// The workspace visible when the load starts
        start: Cell<i32>,
        /// Where the pointer was before the load touched anything (default: wherever it is)
        origin: Cell<Option<(i32, i32)>>,
        leaves: RefCell<std::collections::HashMap<String, Vec<(Address, crate::dwindle::Rect)>>>,
    }

    impl FakeHost {
        fn new(existing: Vec<Client>, scripts: Vec<Vec<(u64, Client)>>) -> Self {
            FakeHost {
                tick: Cell::new(0),
                merge: false,
                windows: RefCell::new(existing.into_iter().map(|c| (0, c)).collect()),
                scripts: RefCell::new(scripts.into()),
                sent: RefCell::new(vec![]),
                pauses: Cell::new(0),
                signals: RefCell::new(vec![]),
                resist: RefCell::new(Default::default()),
                options: RefCell::new(Default::default()),
                cursor: Cell::new(None),
                drift_to: Cell::new(None),
                hide: RefCell::new(None),
                warp: Cell::new(true),
                sim: Cell::new(false),
                inherit_maximized: Cell::new(false),
                geometry_updates: RefCell::new(vec![]),
                title_updates: RefCell::new(vec![]),
                tray_window: RefCell::new(None),
                procs: RefCell::new(Default::default()),
                start: Cell::new(1),
                origin: Cell::new(None),
                leaves: RefCell::new(Default::default()),
            }
        }

        /// A window leaves its workspace for a special one. Only whole-workspace evacuations are
        /// simulated (collapsing a partial tree is not modelled, so a partial removal panics)
        fn sim_park(&self, window: &Address) {
            let mut leaves = self.leaves.borrow_mut();
            for tree in leaves.values_mut() {
                if let Some(at) = tree.iter().position(|(a, _)| a == window) {
                    tree.remove(at);
                }
            }
            for (_, c) in self
                .windows
                .borrow_mut()
                .iter_mut()
                .filter(|(_, c)| c.address == *window)
            {
                c.workspace = Workspace::from_selector("special:hyprsession-stage");
            }
        }

        /// Dwindle as measured in spike #2: the new window splits the leaf nearest the pointer
        /// (ties: the oldest) along its longer side and becomes the second child
        fn sim_insert(&self, window: &Address, ws: &str) {
            const SCREEN: crate::dwindle::Rect = (0.0, 26.0, 1920.0, 1080.0);
            let (px, py) = self
                .cursor
                .get()
                .map(|(x, y)| (x as f64, y as f64))
                .unwrap_or((0.0, 0.0));
            let mut leaves = self.leaves.borrow_mut();
            let tree = leaves.entry(ws.to_owned()).or_default();
            if tree.is_empty() {
                tree.push((window.clone(), SCREEN));
            } else {
                let target = (0..tree.len())
                    .min_by(|&a, &b| {
                        let da = crate::dwindle::distance((px, py), tree[a].1);
                        let db = crate::dwindle::distance((px, py), tree[b].1);
                        da.partial_cmp(&db).unwrap()
                    })
                    .unwrap();
                let (_, first, second) = crate::dwindle::split_box(tree[target].1);
                tree[target].1 = first;
                tree.push((window.clone(), second));
            }
            // every window of the workspace takes the geometry of its leaf (12 px at the screen edge, 7 between)
            let inset = |edge: f64, at: f64| if (edge - at).abs() < 0.5 { 12.0 } else { 7.0 };
            for (address, b) in tree.iter() {
                let (x0, y0, x1, y1) = (
                    b.0 + inset(b.0, SCREEN.0),
                    b.1 + inset(b.1, SCREEN.1),
                    b.2 - inset(b.2, SCREEN.2),
                    b.3 - inset(b.3, SCREEN.3),
                );
                for (_, c) in self
                    .windows
                    .borrow_mut()
                    .iter_mut()
                    .filter(|(_, c)| c.address == *address)
                {
                    c.workspace = Workspace::from_selector(ws);
                    c.at = (x0 as i32, y0 as i32);
                    c.size = ((x1 - x0) as i32, (y1 - y0) as i32);
                }
            }
        }

        /// A compositor in the configuration the computed order is defined for, pointer bottom-left
        fn dwindle(self) -> Self {
            self.options
                .borrow_mut()
                .insert("general:layout".into(), "dwindle".into());
            self.options
                .borrow_mut()
                .insert("dwindle:force_split".into(), "2".into());
            self.cursor.set(Some((300, 900)));
            self
        }
    }

    impl Host for FakeHost {
        fn merge_existing(&self) -> bool {
            self.merge
        }
        fn clients(&self) -> hyprland::Result<Vec<Client>> {
            let hidden = self
                .hide
                .borrow()
                .as_ref()
                .filter(|(n, _)| {
                    self.sent
                        .borrow()
                        .iter()
                        .filter(|o| matches!(o, Op::Exec(_)))
                        .count()
                        >= *n
                })
                .map(|(_, a)| a.clone());
            Ok(self
                .windows
                .borrow()
                .iter()
                .filter(|(due, c)| {
                    *due <= self.tick.get()
                        && hidden.as_deref() != Some(c.address.to_string().as_str())
                })
                .map(|(_, c)| c.clone())
                .collect())
        }
        fn send(&self, op: &Op) -> hyprland::Result<()> {
            if let Op::Exec(_) = op {
                for (delay, c) in self.scripts.borrow_mut().pop_front().unwrap_or_default() {
                    self.windows.borrow_mut().push((self.tick.get() + delay, c));
                }
            }
            if let Op::Close(addr) = op {
                let resist = self.resist.borrow().clone();
                self.windows.borrow_mut().retain(|(_, c)| {
                    !(c.address == *addr && resist.get(&c.pid).copied().unwrap_or(0) == 0)
                });
            }
            if let (Op::MoveToWorkspace { window, workspace }, true) = (op, self.sim.get()) {
                match workspace {
                    crate::dispatch::WorkspaceTarget::Id(ws) => {
                        self.sim_insert(window, &ws.to_string())
                    }
                    crate::dispatch::WorkspaceTarget::Special(_) => self.sim_park(window),
                    crate::dispatch::WorkspaceTarget::Named(name) => self.sim_insert(window, name),
                }
            }
            if let (Op::MoveToWorkspace { window, workspace }, false) = (op, self.sim.get()) {
                let selector = match workspace {
                    crate::dispatch::WorkspaceTarget::Id(id) => id.to_string(),
                    crate::dispatch::WorkspaceTarget::Named(name) => format!("name:{name}"),
                    crate::dispatch::WorkspaceTarget::Special(name) => format!("special:{name}"),
                };
                for (_, client) in self
                    .windows
                    .borrow_mut()
                    .iter_mut()
                    .filter(|(_, c)| c.address == *window)
                {
                    client.workspace = Workspace::from_selector(&selector);
                    if self.inherit_maximized.get() && !selector.starts_with("special:") {
                        client.fullscreen = FullscreenMode::Maximized;
                    }
                }
            }
            if let Op::Fullscreen {
                window,
                maximized,
                enable,
            } = op
            {
                for (_, client) in self
                    .windows
                    .borrow_mut()
                    .iter_mut()
                    .filter(|(_, c)| c.address == *window)
                {
                    let mask = if *maximized { 1 } else { 2 };
                    let bits = client.fullscreen as i32;
                    client.fullscreen = match if *enable { bits | mask } else { bits & !mask } {
                        1 => FullscreenMode::Maximized,
                        2 => FullscreenMode::Fullscreen,
                        3 => FullscreenMode::MaximizedFullscreen,
                        _ => FullscreenMode::None,
                    };
                }
            }
            if let (Op::MoveCursor { x, y }, true) = (op, self.warp.get()) {
                self.cursor.set(Some((*x, *y)));
            }
            if let (Op::MoveToWorkspace { .. }, Some(to)) = (op, self.drift_to.get()) {
                self.cursor.set(Some(to));
            }
            self.sent.borrow_mut().push(op.clone());
            Ok(())
        }
        fn cursor_pos(&self) -> Option<(i32, i32)> {
            self.cursor.get()
        }
        fn option(&self, key: &str) -> Option<String> {
            self.options.borrow().get(key).cloned()
        }
        fn signal(&self, pid: i32, signal: Signal) {
            self.signals.borrow_mut().push((pid, signal));
            let needed = match signal {
                Signal::Term => 1,
                Signal::Kill => 2,
            };
            let resist = self.resist.borrow().get(&pid).copied().unwrap_or(0);
            if resist <= needed {
                self.windows.borrow_mut().retain(|(_, c)| c.pid != pid);
            }
        }
        fn active_workspace(&self) -> Option<String> {
            Some(self.start.get().to_string())
        }
        fn process(&self, pid: i32) -> Option<ProcessInfo> {
            self.procs.borrow().get(&pid).cloned()
        }
        fn reveal_tray_window(&self, _entry: &ExecEntry) -> bool {
            if let Some(window) = self.tray_window.borrow_mut().take() {
                self.windows.borrow_mut().push((self.tick.get(), window));
                true
            } else {
                false
            }
        }
        fn pointer_origin(&self) -> Option<(i32, i32)> {
            self.origin.get().or(self.cursor.get())
        }
        fn pause(&self) {
            self.tick.set(self.tick.get() + 1);
            self.pauses.set(self.pauses.get() + 1);
            self.title_updates
                .borrow_mut()
                .retain(|(due, address, title)| {
                    if *due > self.tick.get() {
                        return true;
                    }
                    for (_, live) in self
                        .windows
                        .borrow_mut()
                        .iter_mut()
                        .filter(|(_, c)| c.address == *address)
                    {
                        live.title = title.clone();
                    }
                    false
                });
            self.geometry_updates.borrow_mut().retain(|(due, saved)| {
                if *due > self.tick.get() {
                    return true;
                }
                for (_, live) in self
                    .windows
                    .borrow_mut()
                    .iter_mut()
                    .filter(|(_, c)| c.address == saved.address)
                {
                    live.at = saved.at;
                    live.size = saved.size;
                }
                false
            });
        }
    }

    fn entry(line: &str) -> ExecEntry {
        parse_exec_line(line).unwrap()
    }

    fn execs(host: &FakeHost) -> Vec<String> {
        host.sent
            .borrow()
            .iter()
            .filter_map(|o| match o {
                Op::Exec(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_old_format_places_each_launchs_first_new_window_by_rules() {
        let a = client("kitty", "a", 1, false, (0, 0), (50, 50));
        let b = client("mpv", "b", 1, false, (3, 0), (50, 50));
        let host = FakeHost::new(vec![], vec![vec![(2, a.clone())], vec![(3, b.clone())]]);
        let plan = LoadPlan {
            entries: launch_order(vec![
                entry("[workspace 4 silent;float;move 100 200;size 300 400] mpv"),
                entry("[workspace 2 silent;move 0 0] kitty"),
            ]),
            saved: vec![],
            place_by_class: false,
            tail_ticks: 0,
            scope: None,
            restore_workspace: Some("7".into()),
        };
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 2);
        // Workspace 2 launches before workspace 4
        assert_eq!(execs(&host), vec!["kitty", "mpv"]);
        let sent = host.sent.borrow();
        assert!(sent.contains(&Op::MoveToWorkspace {
            window: a.address.clone(),
            workspace: crate::dispatch::WorkspaceTarget::Id(2)
        }));
        assert!(sent.contains(&Op::MoveWindow {
            window: b.address.clone(),
            x: 100,
            y: 200
        }));
        assert!(sent.contains(&Op::ResizeWindow {
            window: b.address.clone(),
            w: 300,
            h: 400
        }));
        // The tiled window gets its workspace only
        assert!(!sent
            .iter()
            .any(|o| matches!(o, Op::MoveWindow { window, .. } if *window == a.address)));
        // Focus returns to the workspace that was active when load started
        assert_eq!(sent.last(), Some(&Op::FocusWorkspace("7".into())));
    }

    #[test]
    fn test_class_placement_handles_windows_that_arrive_late_and_reports_unmatched() {
        // Three saved brave windows; the app only recreates two of them
        let saved = vec![
            client("brave", "one", 1, false, (0, 0), (1, 1)),
            client("brave", "two", 2, false, (0, 0), (1, 1)),
            client("brave", "three", 3, false, (0, 0), (1, 1)),
        ];
        let live1 = client("brave", "one", 9, false, (1, 0), (1, 1));
        let live2 = client("brave", "two", 9, false, (2, 0), (1, 1));
        let host = FakeHost::new(vec![], vec![vec![(1, live1), (6, live2)]]);
        let plan = LoadPlan {
            entries: vec![entry("[workspace 1 silent] brave")],
            saved,
            place_by_class: true,
            tail_ticks: 10,
            scope: None,
            restore_workspace: None,
        };
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 2);
        assert_eq!(report.unmatched.len(), 1);
        assert_eq!(report.unmatched[0].title, "three");
    }

    #[test]
    fn test_load_ends_early_once_everything_is_matched() {
        let host = FakeHost::new(
            vec![],
            vec![vec![(1, client("kitty", "t", 9, false, (1, 0), (1, 1)))]],
        );
        let plan = LoadPlan {
            entries: vec![entry("[workspace 1 silent] kitty")],
            saved: vec![client("kitty", "t", 1, false, (0, 0), (1, 1))],
            place_by_class: true,
            tail_ticks: 600,
            scope: None,
            restore_workspace: None,
        };
        let report = run_load(&host, &plan);
        assert!(report.unmatched.is_empty());
        assert!(host.pauses.get() < 10, "waited {} ticks", host.pauses.get());
    }

    fn group_entry(label: &str, saved: Vec<usize>) -> ExecEntry {
        ExecEntry {
            rules: ExecRules::default(),
            command: label.to_string(),
            group: Some(GroupInfo {
                label: label.to_string(),
                saved,
            }),
        }
    }

    fn brave_saved(n: i32) -> Vec<Client> {
        (0..n)
            .map(|i| client("brave", &format!("w{i}"), 1 + i, false, (i, 0), (1, 1)))
            .collect()
    }

    fn live_brave(i: i32) -> Client {
        client("brave", &format!("live{i}"), 9, false, (50 + i, 0), (1, 1))
    }

    fn group_plan(entries: Vec<ExecEntry>, saved: Vec<Client>) -> LoadPlan {
        LoadPlan {
            entries,
            saved,
            place_by_class: true,
            tail_ticks: 0,
            scope: None,
            restore_workspace: None,
        }
    }

    #[test]
    fn test_group_with_a_missing_window_is_launched_again_and_both_are_placed() {
        // Two same-profile windows saved; the first launch brings up one, the second one more
        let host = FakeHost::new(
            vec![],
            vec![vec![(1, live_brave(0))], vec![(1, live_brave(1))]],
        );
        let plan = group_plan(
            vec![group_entry("brave --profile x", vec![0, 1])],
            brave_saved(2),
        );
        let report = run_load(&host, &plan);
        assert_eq!(execs(&host), vec!["brave --profile x", "brave --profile x"]);
        assert_eq!(report.placed, 2);
        assert!(report.shortfalls.is_empty() && report.unmatched.is_empty());
        // Each newcomer was moved to its saved workspace
        let sent = host.sent.borrow();
        for ws in [1, 2] {
            assert!(sent.iter().any(|o| matches!(o, Op::MoveToWorkspace {
                workspace: crate::dispatch::WorkspaceTarget::Id(id), ..
            } if *id == ws)));
        }
    }

    #[test]
    fn test_refused_mint_is_reported_and_the_load_completes() {
        let host = FakeHost::new(vec![], vec![vec![(1, live_brave(0))], vec![]]);
        let plan = group_plan(vec![group_entry("brave", vec![0, 1])], brave_saved(2));
        let report = run_load(&host, &plan);
        assert_eq!(
            report.shortfalls,
            vec![GroupShortfall {
                label: "brave".to_string(),
                expected: 2,
                got: 1,
                reinvocations: 1,
            }]
        );
        assert_eq!(execs(&host).len(), 2);
    }

    #[test]
    fn test_reinvocations_are_bounded_by_the_shortfall() {
        // Three saved, one produced: at most two more launches, even if nothing else appears
        let host = FakeHost::new(vec![], vec![vec![(1, live_brave(0))]]);
        let plan = group_plan(vec![group_entry("brave", vec![0, 1, 2])], brave_saved(3));
        let report = run_load(&host, &plan);
        assert_eq!(execs(&host).len(), 3);
        assert_eq!(report.shortfalls[0].reinvocations, 2);
    }

    #[test]
    fn test_complete_group_is_not_launched_again_and_does_not_wait() {
        let host = FakeHost::new(vec![], vec![vec![(1, live_brave(0)), (1, live_brave(1))]]);
        let plan = group_plan(vec![group_entry("brave", vec![0, 1])], brave_saved(2));
        let report = run_load(&host, &plan);
        assert_eq!(execs(&host).len(), 1);
        assert!(report.shortfalls.is_empty());
        assert!(
            host.pauses.get() < SETTLE_TICKS,
            "waited {} ticks",
            host.pauses.get()
        );
    }

    #[test]
    fn test_group_that_produced_nothing_is_reported_not_relaunched() {
        let host = FakeHost::new(vec![], vec![vec![]]);
        let plan = group_plan(vec![group_entry("ghost", vec![0, 1])], brave_saved(2));
        let report = run_load(&host, &plan);
        assert_eq!(execs(&host).len(), 1);
        assert_eq!(report.shortfalls[0].got, 0);
        assert_eq!(report.shortfalls[0].reinvocations, 0);
    }

    #[test]
    fn test_legacy_entries_without_a_group_are_never_relaunched() {
        let host = FakeHost::new(vec![], vec![vec![(1, live_brave(0))]]);
        let plan = group_plan(vec![entry("[workspace 1 silent] brave")], brave_saved(2));
        let report = run_load(&host, &plan);
        assert_eq!(execs(&host).len(), 1);
        assert!(report.shortfalls.is_empty());
    }

    #[test]
    fn test_launch_groups_carry_their_saved_windows() {
        let entries = launch_entries_from_saved_no_xdg(&[
            saved_pid("brave", &["brave --profile-directory=Default"], 55, (0, 0)),
            saved_pid("kitty", &["kitty"], 60, (5, 0)),
            saved_pid("brave", &["brave --profile-directory=Default"], 55, (30, 0)),
        ]);
        let groups: Vec<(&str, &[usize])> = entries
            .iter()
            .map(|e| {
                let g = e.group.as_ref().unwrap();
                (g.label.as_str(), g.saved.as_slice())
            })
            .collect();
        assert_eq!(
            groups,
            vec![
                ("brave --profile-directory=Default", &[0usize, 2][..]),
                ("kitty", &[1usize][..]),
            ]
        );
    }

    #[test]
    fn test_surplus_windows_are_reported_and_left_alone() {
        // Saved: one kitty. Live: that kitty, plus a window opened since the save
        let kitty = client("kitty", "t", 5, false, (1, 0), (1, 1));
        let mut stray = client("mpv", "opened later", 3, false, (9, 0), (1, 1));
        stray.address = hyprland::shared::Address::new("0xabc");
        let host = FakeHost::new(vec![kitty, stray.clone()], vec![]);
        let plan = LoadPlan {
            entries: vec![],
            saved: vec![client("kitty", "t", 2, false, (0, 0), (1, 1))],
            place_by_class: true,
            tail_ticks: 5,
            scope: None,
            restore_workspace: None,
        };
        let report = run_load(&host, &plan);
        assert_eq!(report.surplus.len(), 1);
        assert_eq!(report.surplus[0].address, stray.address);
        assert_eq!(report.surplus[0].class, "mpv");
        // Report only: nothing was dispatched for the stray window
        assert!(!host
            .sent
            .borrow()
            .iter()
            .any(|op| format!("{op:?}").contains("0xabc")));
    }

    #[test]
    fn test_surplus_includes_same_class_windows_beyond_the_saved_count() {
        let a = client("brave", "a", 1, false, (0, 0), (1, 1));
        let mut b = client("brave", "b", 1, false, (5, 0), (1, 1));
        b.address = hyprland::shared::Address::new("0xdef");
        let host = FakeHost::new(vec![a, b], vec![]);
        let plan = LoadPlan {
            entries: vec![],
            saved: vec![client("brave", "a", 1, false, (0, 0), (1, 1))],
            place_by_class: true,
            tail_ticks: 2,
            scope: None,
            restore_workspace: None,
        };
        assert_eq!(run_load(&host, &plan).surplus.len(), 1);
    }

    #[test]
    fn test_legacy_plans_report_no_surplus() {
        let host = FakeHost::new(
            vec![client("kitty", "t", 1, false, (0, 0), (1, 1))],
            vec![vec![]],
        );
        let plan = LoadPlan {
            entries: vec![entry("[workspace 1 silent] daemon")],
            saved: vec![],
            place_by_class: false,
            tail_ticks: 0,
            scope: None,
            restore_workspace: None,
        };
        assert!(run_load(&host, &plan).surplus.is_empty());
    }

    #[test]
    fn test_adjust_only_places_existing_windows_without_launching() {
        let live = client("kitty", "t", 5, false, (1, 0), (1, 1));
        let host = FakeHost::new(vec![live.clone()], vec![]);
        let plan = LoadPlan {
            entries: vec![],
            saved: vec![client("kitty", "t", 2, false, (0, 0), (1, 1))],
            place_by_class: true,
            tail_ticks: 5,
            scope: None,
            restore_workspace: None,
        };
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 1);
        assert!(execs(&host).is_empty());
    }

    #[test]
    fn test_launch_that_never_shows_a_window_does_not_stall_forever() {
        let host = FakeHost::new(vec![], vec![vec![]]);
        let plan = LoadPlan {
            entries: vec![entry("[workspace 1 silent] daemon")],
            saved: vec![],
            place_by_class: false,
            tail_ticks: 0,
            scope: None,
            restore_workspace: None,
        };
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 0);
        assert_eq!(host.pauses.get(), LAUNCH_WAIT_TICKS);
    }

    fn desktop(pids: &[(i32, &str)]) -> Vec<Client> {
        pids.iter()
            .enumerate()
            .map(|(i, (pid, class))| {
                let mut c = client(
                    class,
                    &format!("{class} {i}"),
                    1,
                    false,
                    (i as i32, 0),
                    (1, 1),
                );
                c.pid = *pid;
                c
            })
            .collect()
    }

    const FAST: ClearConfig = ClearConfig {
        close_ticks: 3,
        term_ticks: 3,
        kill_ticks: 3,
    };

    #[test]
    fn test_clear_on_empty_desktop_does_nothing() {
        let host = FakeHost::new(vec![], vec![]);
        let report = clear_session(&host, &FAST).unwrap();
        assert!(host.sent.borrow().is_empty() && host.signals.borrow().is_empty());
        assert!(report.closed.is_empty());
    }

    #[test]
    fn test_clear_closes_gracefully_without_any_signal() {
        let host = FakeHost::new(
            desktop(&[(10, "brave"), (20, "kitty"), (10, "brave")]),
            vec![],
        );
        let report = clear_session(&host, &FAST).unwrap();
        assert_eq!(report.closed.len(), 3);
        assert!(report.termed.is_empty() && report.killed.is_empty());
        assert!(host.signals.borrow().is_empty());
        let closes = host
            .sent
            .borrow()
            .iter()
            .filter(|o| matches!(o, Op::Close(_)))
            .count();
        assert_eq!(closes, 3);
    }

    #[test]
    fn test_clear_escalates_only_for_what_stays() {
        // kitty closes; spotify ignores close but not SIGTERM; stubborn needs SIGKILL
        let host = FakeHost::new(
            desktop(&[
                (20, "kitty"),
                (30, "spotify"),
                (30, "spotify"),
                (40, "stubborn"),
            ]),
            vec![],
        );
        host.resist.borrow_mut().insert(30, 1);
        host.resist.borrow_mut().insert(40, 2);
        let report = clear_session(&host, &FAST).unwrap();
        assert_eq!(report.closed.len(), 1);
        // The two spotify windows share a pid: one SIGTERM, covering both stragglers
        // from the close stage; the pid that was already gone is never signalled
        assert_eq!(report.termed, vec![30, 40]);
        assert_eq!(report.killed, vec![40]);
        assert_eq!(
            *host.signals.borrow(),
            vec![(30, Signal::Term), (40, Signal::Term), (40, Signal::Kill)]
        );
        // Each wait was bounded by its stage
        assert!(host.pauses.get() <= FAST.close_ticks + FAST.term_ticks + FAST.kill_ticks + 3);
    }

    #[test]
    fn test_clear_escalates_in_order_close_then_term_then_kill() {
        let host = FakeHost::new(desktop(&[(50, "stubborn")]), vec![]);
        host.resist.borrow_mut().insert(50, 2);
        clear_session(&host, &FAST).unwrap();
        // The close request went out before any signal
        assert!(matches!(host.sent.borrow().first(), Some(Op::Close(_))));
        assert_eq!(
            *host.signals.borrow(),
            vec![(50, Signal::Term), (50, Signal::Kill)]
        );
    }

    #[test]
    fn test_clear_aborts_naming_survivors_of_sigkill() {
        let host = FakeHost::new(desktop(&[(60, "zombie"), (70, "kitty")]), vec![]);
        host.resist.borrow_mut().insert(60, 3);
        let err = clear_session(&host, &FAST).unwrap_err();
        assert!(err.contains("zombie") && err.contains("pid 60"), "{err}");
        assert!(!err.contains("kitty"), "{err}");
        assert_eq!(host.signals.borrow().last(), Some(&(60, Signal::Kill)));
    }

    #[test]
    fn test_clear_never_signals_init() {
        let host = FakeHost::new(desktop(&[(1, "odd"), (0, "odd")]), vec![]);
        host.resist.borrow_mut().insert(1, 3);
        host.resist.borrow_mut().insert(0, 3);
        assert!(clear_session(&host, &FAST).is_err());
        assert!(host.signals.borrow().is_empty());
    }

    /// A window of `class` owned by `pid` on workspace `ws`; `n` keeps addresses distinct
    fn win(class: &str, pid: i32, ws: i32, n: i32) -> Client {
        let mut c = client(class, &format!("{class} {n}"), ws, false, (n, 0), (1, 1));
        c.pid = pid;
        c.address = hyprland::shared::Address::new(format!("0x{:x}{:02x}", ws, n));
        c
    }

    fn addresses(host: &FakeHost) -> Vec<String> {
        let mut a: Vec<String> = host
            .windows
            .borrow()
            .iter()
            .map(|(_, c)| c.address.to_string())
            .collect();
        a.sort();
        a
    }

    #[test]
    fn test_scoped_clear_leaves_other_workspaces_alone() {
        let host = FakeHost::new(
            vec![
                win("kitty", 10, 1, 1),
                win("brave", 20, 1, 2),
                win("mpv", 30, 2, 3),
                win("foot", 40, 2, 4),
            ],
            vec![],
        );
        let report = clear_windows(&host, &FAST, Scope::Workspace("1".into())).unwrap();
        assert_eq!(report.closed.len(), 2);
        // Only workspace 1 was asked to close; workspace 2's addresses are untouched
        let closes: Vec<_> = host
            .sent
            .borrow()
            .iter()
            .filter(|o| matches!(o, Op::Close(_)))
            .cloned()
            .collect();
        assert_eq!(closes.len(), 2);
        assert_eq!(
            addresses(&host),
            vec!["0x203".to_string(), "0x204".to_string()]
        );
        assert!(host.signals.borrow().is_empty());
    }

    #[test]
    fn test_scoped_clear_never_signals_a_pid_shared_with_another_workspace() {
        // pid 50 owns a window on workspace 1 and one on workspace 2; it ignores close and TERM
        let host = FakeHost::new(
            vec![
                win("brave", 50, 1, 1),
                win("brave", 50, 2, 2),
                win("kitty", 60, 1, 3),
            ],
            vec![],
        );
        host.resist.borrow_mut().insert(50, 2);
        let report = clear_windows(&host, &FAST, Scope::Workspace("1".into())).unwrap();
        // The refusing window is reported, no signal ever reached pid 50, both its windows survive
        assert_eq!(report.refused.len(), 1);
        assert!(report.refused[0].contains("pid 50"), "{:?}", report.refused);
        assert!(host.signals.borrow().iter().all(|(pid, _)| *pid != 50));
        assert_eq!(
            addresses(&host),
            vec!["0x101".to_string(), "0x202".to_string()]
        );
    }

    #[test]
    fn test_scoped_clear_still_escalates_for_pids_wholly_in_scope() {
        let host = FakeHost::new(
            vec![
                win("spotify", 70, 1, 1),
                win("spotify", 70, 1, 2),
                win("other", 80, 2, 3),
                win("shared", 90, 1, 4),
                win("shared", 90, 2, 5),
            ],
            vec![],
        );
        host.resist.borrow_mut().insert(70, 1);
        host.resist.borrow_mut().insert(90, 3);
        let report = clear_windows(&host, &FAST, Scope::Workspace("1".into())).unwrap();
        assert_eq!(*host.signals.borrow(), vec![(70, Signal::Term)]);
        assert_eq!(report.termed, vec![70]);
        assert_eq!(report.refused.len(), 1);
        assert_eq!(
            addresses(&host),
            vec![
                "0x104".to_string(),
                "0x203".to_string(),
                "0x205".to_string()
            ]
        );
    }

    #[test]
    fn test_scoped_clear_aborts_when_an_in_scope_pid_survives_sigkill() {
        let host = FakeHost::new(
            vec![win("zombie", 60, 1, 1), win("kitty", 70, 2, 2)],
            vec![],
        );
        host.resist.borrow_mut().insert(60, 3);
        let err = clear_windows(&host, &FAST, Scope::Workspace("1".into())).unwrap_err();
        assert!(err.contains("pid 60") && !err.contains("kitty"), "{err}");
    }

    #[test]
    fn test_scoped_clear_of_an_empty_workspace_does_nothing() {
        let host = FakeHost::new(vec![win("kitty", 10, 2, 1)], vec![]);
        clear_windows(&host, &FAST, Scope::Workspace("1".into())).unwrap();
        assert!(host.sent.borrow().is_empty());
    }

    #[test]
    fn test_scoped_load_never_claims_or_moves_other_workspaces_windows() {
        // Saved: a brave on workspace 1. Live: a brave already on workspace 2 (untouched
        // layout) and a brave that appears from the launch
        let other = win("brave", 11, 2, 1);
        let launched = win("brave", 12, 1, 2);
        let host = FakeHost::new(vec![other.clone()], vec![vec![(1, launched.clone())]]);
        let plan = LoadPlan {
            entries: vec![group_entry("brave", vec![0])],
            saved: vec![client("brave", "w", 1, false, (0, 0), (1, 1))],
            place_by_class: true,
            tail_ticks: 3,
            scope: Some("1".into()),
            restore_workspace: Some("2".into()),
        };
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 1);
        let sent = host.sent.borrow();
        let touches = |a: &hyprland::shared::Address| {
            sent.iter()
                .any(|o| format!("{o:?}").contains(&a.to_string()))
        };
        assert!(!touches(&other.address), "{sent:?}");
        // The only focus change is the v1 return-to-start
        let focuses: Vec<_> = sent
            .iter()
            .filter(|o| matches!(o, Op::FocusWorkspace(_) | Op::FocusWindow(_)))
            .collect();
        assert_eq!(focuses, vec![&Op::FocusWorkspace("2".into())]);
        // And it is not reported as surplus either
        assert!(report.surplus.is_empty());
    }

    #[test]
    fn test_placer_reports_the_claimed_index_and_gives_claims_back() {
        let saves = vec![
            client("kitty", "a", 1, false, (0, 0), (1, 1)),
            client("kitty", "b", 1, false, (0, 0), (1, 1)),
        ];
        let mut placer = Placer::new(saves);
        let live = client("kitty", "b", 9, false, (1, 0), (1, 1));
        let (index, claimed) = placer.assign_indexed(&live).unwrap();
        assert_eq!((index, claimed.title.as_str()), (1, "b"));
        assert!(
            placer.assign_indexed(&live).is_none(),
            "one live window claims once"
        );
        placer.release(index);
        assert_eq!(placer.unmatched().len(), 2);
        assert_eq!(placer.assign_indexed(&live).unwrap().0, 1);
    }

    // ---- placement queue: tiled windows are placed per workspace, in computed insertion order

    /// One tiled save per class on `ws`, at the given rectangles (x, y, w, h)
    fn tiled_saves(ws: i32, shape: &[(i32, i32, i32, i32)]) -> Vec<Client> {
        shape
            .iter()
            .enumerate()
            .map(|(i, &(x, y, w, h))| client(&format!("app{i}"), "t", ws, false, (x, y), (w, h)))
            .collect()
    }

    /// Spike #1 fixture "A" (insertion order = index order at a bottom-left pointer) and a
    /// reading order that differs from it: index 1 is the full-height window on the right
    const S4: [(i32, i32, i32, i32); 4] = [
        (12, 38, 941, 508),
        (967, 38, 941, 1030),
        (12, 560, 466, 508),
        (492, 560, 461, 508),
    ];

    fn live_window(class: &str, n: i32) -> Client {
        let mut c = client(class, "t", 9, false, (50 + n, 0), (1, 1));
        c.address = hyprland::shared::Address::new(format!("0xa{n:02x}"));
        c
    }

    /// Entries launching `launch_order` (indices into the saves), each bringing up its own window
    fn launching(
        saves: &[Client],
        launch_order: &[usize],
    ) -> (Vec<ExecEntry>, Vec<Vec<(u64, Client)>>) {
        let entries = launch_order
            .iter()
            .map(|&i| group_entry(&format!("app{i}"), vec![i]))
            .collect();
        let scripts = launch_order
            .iter()
            .map(|&i| vec![(1, live_window(&saves[i].class, i as i32))])
            .collect();
        (entries, scripts)
    }

    fn plan_for(entries: Vec<ExecEntry>, saved: Vec<Client>) -> LoadPlan {
        LoadPlan {
            entries,
            saved,
            place_by_class: true,
            tail_ticks: 0,
            scope: None,
            restore_workspace: None,
        }
    }

    #[test]
    fn test_workspace_flushes_at_the_end_of_the_phase_when_a_window_never_comes() {
        // Expected 3 on ws 2; the third app produces nothing
        let saves = tiled_saves(2, &S4[..3]);
        let (entries, mut scripts) = launching(&saves, &[0, 1, 2]);
        scripts[2] = vec![];
        let host = FakeHost::new(vec![], scripts).dwindle();
        let report = run_load(&host, &plan_for(entries, saves));
        assert_eq!(report.placed, 2);
        assert_eq!(report.unmatched.len(), 1);
    }

    #[test]
    fn test_window_arriving_after_the_flush_is_placed_on_claim() {
        // app1 maps after the 20 s launch wait and the settle: it is a late arrival
        let saves = tiled_saves(2, &S4[..2]);
        let (entries, mut scripts) = launching(&saves, &[0, 1]);
        scripts[1] = vec![(LAUNCH_WAIT_TICKS + SETTLE_TICKS + 5, live_window("app1", 1))];
        let host = FakeHost::new(vec![], scripts).dwindle();
        let mut plan = plan_for(entries, saves);
        plan.tail_ticks = 2 * LAUNCH_WAIT_TICKS;
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 2);
        assert!(report.unmatched.is_empty());
        // two flushes never happen for one workspace: the late window was placed on claim
        assert_eq!(report.flushes.len(), 1);
    }

    #[test]
    fn restored_window_does_not_inherit_destination_maximization() {
        let live = live_window("app", 1);
        let mut saved = live.clone();
        saved.workspace = Workspace::from_selector("7");
        let host = FakeHost::new(vec![], vec![vec![(1, live)]]);
        host.inherit_maximized.set(true);
        let report = run_load(
            &host,
            &plan_for(vec![group_entry("app", vec![0])], vec![saved]),
        );
        assert!(!report.incomplete);
        let clients = host.clients().unwrap();
        assert_eq!(clients[0].workspace.selector(), "7");
        assert_eq!(clients[0].fullscreen, FullscreenMode::None);
    }

    #[test]
    fn test_group_reinvocation_newcomers_join_the_same_ordered_flush() {
        // One app opens two windows, but the first launch brings up only one
        let saves = tiled_saves(2, &S4[..2])
            .into_iter()
            .map(|mut c| {
                c.class = "app".into();
                c.title = "t".into();
                c
            })
            .collect::<Vec<_>>();
        let mk = |n: i32| {
            let mut c = live_window("app", n);
            c.title = "t".into();
            c
        };
        let host = FakeHost::new(vec![], vec![vec![(1, mk(0))], vec![(1, mk(1))]]).dwindle();
        let plan = plan_for(vec![group_entry("app", vec![0, 1])], saves);
        let report = run_load(&host, &plan);
        assert_eq!(execs(&host).len(), 2);
        assert_eq!(report.placed, 2);
        assert_eq!(report.flushes.len(), 1);
        assert_eq!(report.flushes[0].windows, 2);
    }

    #[test]
    fn test_vanished_held_window_is_released_and_its_replacement_is_placed() {
        // app1's window is claimed and held; it is gone by the time ws 2 is complete (app2's
        // arrival). The app opens a replacement afterwards, which the tail claims and places.
        let saves = tiled_saves(2, &S4[..3]);
        let (entries, mut scripts) = launching(&saves, &[0, 1, 2]);
        scripts[2].push((4, live_window("app1", 7)));
        let host = FakeHost::new(vec![], scripts).dwindle();
        *host.hide.borrow_mut() = Some((3, "0xa01".to_string()));
        let mut plan = plan_for(entries, saves);
        plan.tail_ticks = 10;
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 3);
        assert!(report.unmatched.is_empty());
    }

    #[test]
    fn test_pointer_moving_during_a_flush_is_reported() {
        let saves = tiled_saves(2, &S4);
        let (entries, scripts) = launching(&saves, &[0, 1, 2, 3]);
        let host = FakeHost::new(vec![], scripts).dwindle();
        host.drift_to.set(Some((1500, 200)));
        let report = run_load(&host, &plan_for(entries, saves));
        assert!(report.flushes[0].pointer_moved);
    }

    #[test]
    fn test_nothing_claimed_stays_held_when_the_load_ends() {
        // a group-less entry is pending everywhere until it ends; the tail still flushes
        let saves = tiled_saves(2, &S4[..2]);
        let host = FakeHost::new(
            vec![],
            vec![vec![
                (1, live_window("app0", 0)),
                (2, live_window("app1", 1)),
            ]],
        )
        .dwindle();
        let entry = ExecEntry {
            rules: ExecRules::default(),
            command: "x".into(),
            group: None,
        };
        let mut plan = plan_for(vec![entry], saves);
        plan.tail_ticks = 5;
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 2);
    }

    // ---- claim binding: a resume window binds to the entry that spawned it

    /// Five saved terminals that all wear the generic title; the entries are resume launches
    fn identical_title_terminals(n: usize) -> (Vec<Client>, Vec<ExecEntry>) {
        let saves: Vec<Client> = (0..n)
            .map(|i| {
                client(
                    "foot",
                    "user@testhost:~",
                    1 + i as i32,
                    false,
                    (12, 38),
                    (1896, 1030),
                )
            })
            .collect();
        let entries = (0..n)
            .map(|i| {
                let mut e = group_entry(&format!("foot {i}"), vec![i]);
                e.command = format!("foot sh -c script hyprsession-resume /p{i} 'cmd{i}'");
                e
            })
            .collect();
        (saves, entries)
    }

    fn foot_window(n: i32) -> Client {
        let mut c = client("foot", "user@testhost:~", 9, false, (50 + n, 0), (1, 1));
        c.address = hyprland::shared::Address::new(format!("0xf{n:02x}"));
        c
    }

    /// (live address, workspace id) of every MoveToWorkspace sent
    fn moves_to(host: &FakeHost) -> Vec<(String, i32)> {
        host.sent
            .borrow()
            .iter()
            .filter_map(|o| match o {
                Op::MoveToWorkspace {
                    window,
                    workspace: crate::dispatch::WorkspaceTarget::Id(id),
                } => Some((window.to_string(), *id)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_resume_windows_with_identical_titles_bind_to_their_own_entries() {
        let (saves, mut entries) = identical_title_terminals(5);
        // launched against saved order, so title/order claiming alone would cross them over;
        // each launch brings up one generic-titled window, which must land on ITS entry's workspace
        entries.reverse();
        let scripts = (0..5).rev().map(|i| vec![(1, foot_window(i))]).collect();
        let host = FakeHost::new(vec![], scripts);
        let report = run_load(&host, &plan_for(entries, saves));
        assert_eq!(report.placed, 5);
        let mut got = moves_to(&host);
        got.sort();
        let want: Vec<(String, i32)> = (0..5).map(|i| (format!("0xf{i:02x}"), 1 + i)).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn test_a_plain_terminal_cannot_take_a_save_reserved_for_a_resume_launch() {
        // saved order: [resume terminal (ws 1), plain terminal (ws 2)], same title. The plain
        // terminal's entry is launched FIRST; by title alone it would take save 0 (the resume's).
        let saves = vec![
            client("foot", "user@testhost:~", 1, false, (12, 38), (1896, 1030)),
            client("foot", "user@testhost:~", 2, false, (12, 38), (1896, 1030)),
        ];
        let mut resume = group_entry("foot", vec![0]);
        resume.command = "foot sh -c script hyprsession-resume /p cmd".into();
        let plain = group_entry("foot", vec![1]);
        let host = FakeHost::new(
            vec![],
            vec![vec![(1, foot_window(1))], vec![(1, foot_window(0))]],
        );
        run_load(&host, &plan_for(vec![plain, resume], saves));
        let mut got = moves_to(&host);
        got.sort();
        // plain entry's window (0xf01) -> ws 2; resume entry's window (0xf00) -> ws 1
        assert_eq!(
            got,
            vec![("0xf00".to_string(), 1), ("0xf01".to_string(), 2)]
        );
    }

    #[test]
    fn test_spawn_binding_holds_when_a_late_retitle_would_have_split_the_titles() {
        // the resume window retitles itself after the claim: the binding must not care
        let mut saves = vec![client(
            "foot",
            "✳ Example project",
            1,
            false,
            (12, 38),
            (1896, 1030),
        )];
        saves.push(client(
            "foot",
            "user@testhost:~",
            8,
            false,
            (12, 38),
            (1896, 1030),
        ));
        let mut project = group_entry("foot", vec![0]);
        project.command =
            "foot sh -c script hyprsession-resume /home/user 'claude --continue'".into();
        let mut other = group_entry("foot", vec![1]);
        other.command = "foot sh -c script hyprsession-resume /p cmd".into();
        // The project window is still generically titled when it maps.
        let host = FakeHost::new(
            vec![],
            vec![vec![(1, foot_window(0))], vec![(1, foot_window(1))]],
        );
        run_load(&host, &plan_for(vec![project, other], saves));
        let mut got = moves_to(&host);
        got.sort();
        assert_eq!(
            got,
            vec![("0xf00".to_string(), 1), ("0xf01".to_string(), 8)]
        );
    }

    #[test]
    fn test_placer_bound_claims_prefer_own_then_unreserved_then_anything() {
        let saves = vec![
            client("foot", "t", 1, false, (0, 0), (1, 1)),
            client("foot", "t", 2, false, (0, 0), (1, 1)),
            client("foot", "t", 3, false, (0, 0), (1, 1)),
        ];
        let reserved: HashSet<usize> = [0, 1].into_iter().collect();
        let mut placer = Placer::new(saves);
        let w = |n| foot_window(n);
        // own wins over saved order; then unreserved; then reserved as the last resort
        assert_eq!(placer.assign_bound(&w(0), &[1], &reserved).unwrap().0, 1);
        assert_eq!(placer.assign_bound(&w(1), &[], &reserved).unwrap().0, 2);
        assert_eq!(placer.assign_bound(&w(2), &[], &reserved).unwrap().0, 0);
    }

    // ---- R2/R3: pointer-independent reproduction, one-generation convergence

    // The WS9 trees of playtest round 2 (window rectangles x0, y0, x1, y1): the same three windows
    const WS9_B: [(i32, i32, i32, i32); 3] = [
        (12, 38, 953, 546),
        (12, 560, 953, 1068),
        (967, 38, 1908, 1068),
    ];
    const WS9_C: [(i32, i32, i32, i32); 3] = [
        (12, 38, 953, 1068),
        (967, 38, 1908, 546),
        (967, 560, 1908, 1068),
    ];

    fn shape_saves(ws: i32, shape: &[(i32, i32, i32, i32)]) -> Vec<Client> {
        let rects: Vec<_> = shape
            .iter()
            .map(|&(x0, y0, x1, y1)| (x0, y0, x1 - x0, y1 - y0))
            .collect();
        tiled_saves(ws, &rects)
    }

    /// One load of `saves` onto the simulated compositor with the pointer at `pointer`; returns the
    /// "next save" (the saved clients with the geometry the compositor then reports), the report
    /// and the host
    fn simulated_load(
        saves: &[Client],
        pointer: (i32, i32),
        warp: bool,
    ) -> (Vec<Client>, LoadReport, FakeHost) {
        let launch: Vec<usize> = (0..saves.len()).collect();
        let (entries, scripts) = launching(saves, &launch);
        let host = FakeHost::new(vec![], scripts).dwindle();
        host.sim.set(true);
        host.warp.set(warp);
        host.cursor.set(Some(pointer));
        let report = run_load(&host, &plan_for(entries, saves.to_vec()));
        let windows = host.windows.borrow();
        let next = saves
            .iter()
            .enumerate()
            .map(|(i, saved)| {
                let live = &windows
                    .iter()
                    .find(|(_, c)| c.address.to_string() == format!("0xa{i:02x}"))
                    .unwrap()
                    .1;
                let mut next = saved.clone();
                next.at = live.at;
                next.size = live.size;
                next
            })
            .collect();
        drop(windows);
        (next, report, host)
    }

    fn geometry(clients: &[Client]) -> Vec<((i32, i32), (i32, i32))> {
        clients.iter().map(|c| (c.at, c.size)).collect()
    }

    #[test]
    fn test_untreeable_by_growth_shape_round_trips_whatever_the_pointer() {
        // WS9's tree (full-height left, stack right) cannot be grown at a left-hand pointer by
        // insertion order alone; steering makes the pointer irrelevant
        let pointers = [
            (300, 900),
            (561, 424),
            (1437, 553),
            (1500, 200),
            (960, 540),
            (100, 100),
        ];
        for shape in [&WS9_B[..], &WS9_C[..]] {
            let a = shape_saves(7, shape);
            for &pointer in &pointers {
                let (b, report, host) = simulated_load(&a, pointer, true);
                assert_eq!(geometry(&b), geometry(&a), "pointer {pointer:?}: load of A");
                assert!(report.flushes[0].steered);
                assert_eq!(
                    report.verification,
                    vec![WorkspaceCheck {
                        workspace: "7".into(),
                        at_saved: 3,
                        checked: 3,
                        off: vec![]
                    }]
                );
                // the pointer is where the user left it
                assert_eq!(host.cursor.get(), Some(pointer));
                // and the next generation, with the pointer somewhere else again, is identical
                let (c, _, _) = simulated_load(&b, (1437, 553), true);
                assert_eq!(geometry(&c), geometry(&b), "pointer {pointer:?}: load of B");
            }
        }
    }

    #[test]
    fn test_unsteered_load_cannot_reproduce_the_shape_which_is_what_steering_fixes() {
        // control: the same load with the pointer warp refused leaves the tree to the pointer
        let a = shape_saves(7, &WS9_C);
        let (b, report, _) = simulated_load(&a, (300, 900), false);
        assert_ne!(geometry(&b), geometry(&a));
        assert!(!report.flushes[0].steered);
        assert!(
            report.verification[0].at_saved < report.verification[0].checked,
            "the load notices"
        );
    }

    #[test]
    fn test_a_workspace_placed_by_fallback_saves_back_identically_next_generation() {
        // R3: generation N cannot steer (warp refused) and falls back; whatever it produced is a
        // tree the next generation reproduces exactly, at any pointer
        let a = shape_saves(7, &WS9_C);
        let (b, report, _) = simulated_load(&a, (300, 900), false);
        assert!(report.flushes[0].fallback.is_some());
        for pointer in [(300, 900), (1437, 553), (561, 424)] {
            let (c, report, _) = simulated_load(&b, pointer, true);
            assert_eq!(geometry(&c), geometry(&b), "pointer {pointer:?}");
            assert!(report.flushes[0].steered);
        }
    }

    #[test]
    fn test_larger_trees_round_trip_steered() {
        for n in 2..=6 {
            let a = shape_saves(10, &grown_shape(n, (1437, 553)));
            let (b, _, _) = simulated_load(&a, (100, 100), true);
            assert_eq!(geometry(&b), geometry(&a), "n={n}");
        }
    }

    /// Window rectangles of an n-window tree grown by the same pointer-nearest rule
    fn grown_shape(n: usize, pointer: (i32, i32)) -> Vec<(i32, i32, i32, i32)> {
        let saves = tiled_saves(10, &vec![(0, 0, 1, 1); n]);
        let launch: Vec<usize> = (0..n).collect();
        let (entries, scripts) = launching(&saves, &launch);
        let host = FakeHost::new(vec![], scripts).dwindle();
        host.sim.set(true);
        host.warp.set(false);
        host.cursor.set(Some(pointer));
        let mut plan = plan_for(entries, saves);
        plan.saved = (0..n)
            .map(|i| client(&format!("app{i}"), "t", 10, false, (0, 0), (1, 1)))
            .collect();
        run_load(&host, &plan);
        let out = host
            .windows
            .borrow()
            .iter()
            .map(|(_, c)| (c.at.0, c.at.1, c.at.0 + c.size.0, c.at.1 + c.size.1))
            .collect();
        out
    }

    #[test]
    fn test_the_pointer_is_restored_and_a_refused_warp_is_reported_once() {
        let a = shape_saves(7, &WS9_C);
        let (_, report, host) = simulated_load(&a, (123, 456), true);
        assert_eq!(host.cursor.get(), Some((123, 456)));
        assert!(!report.flushes[0].pointer_moved);
        // refused warp: nothing is attempted twice across workspaces, nothing is "restored"
        let (_, _, host) = simulated_load(&a, (123, 456), false);
        assert_eq!(host.cursor.get(), Some((123, 456)));
        let warps = host
            .sent
            .borrow()
            .iter()
            .filter(|o| matches!(o, Op::MoveCursor { .. }))
            .count();
        assert_eq!(
            warps, 3,
            "one refused warp is retried three times, then given up"
        );
    }

    // ---- round 4: provenance, final verification, the start workspace, the pointer's origin

    fn proc_for(host: &FakeHost, pid: i32, argv: &[&str], cwd: &str) {
        host.procs.borrow_mut().insert(
            pid,
            ProcessInfo {
                argv: argv.iter().map(|s| s.to_string()).collect(),
                cwd: Some(cwd.to_string()),
            },
        );
    }

    fn foot_with_pid(n: i32, pid: i32, ws: i32) -> Client {
        let mut c = foot_window(n);
        c.pid = pid;
        c.workspace = Workspace::numbered(ws);
        c.workspace.name = ws.to_string();
        c
    }

    /// Two plain terminals launched from different directories, saved on different workspaces
    /// with the same generic title. X (ws 3) is launched first and its window is SLOW; Y (ws 10)
    /// is quick. By timing and title alone Y's window is the first generic terminal seen and
    /// takes X's save.
    fn interleaved(with_provenance: bool) -> Vec<(String, i32)> {
        let saves = vec![
            client("foot", "user@testhost:~", 3, false, (12, 38), (941, 1030)),
            client("foot", "user@testhost:~", 10, false, (12, 38), (941, 1030)),
        ];
        let mut x = group_entry("foot /x", vec![0]);
        x.command = "cd /x && foot".into();
        let mut y = group_entry("foot /y", vec![1]);
        y.command = "cd /y && foot".into();
        let host = FakeHost::new(
            vec![],
            vec![
                vec![(LAUNCH_WAIT_TICKS + 20, foot_with_pid(0, 700, 9))],
                vec![(1, foot_with_pid(1, 701, 9))],
            ],
        );
        if with_provenance {
            proc_for(&host, 700, &["foot"], "/x");
            proc_for(&host, 701, &["/usr/bin/foot"], "/y");
        }
        let mut plan = plan_for(vec![x, y], saves);
        plan.tail_ticks = 3 * LAUNCH_WAIT_TICKS;
        run_load(&host, &plan);
        let mut got = moves_to(&host);
        got.sort();
        got
    }

    #[test]
    fn test_a_window_launched_for_one_entry_cannot_be_bound_to_another_entry_that_is_outstanding() {
        // X's window (0xf00) belongs on ws 3, Y's (0xf01) on ws 10, whichever arrives first
        assert_eq!(
            interleaved(true),
            vec![("0xf00".to_string(), 3), ("0xf01".to_string(), 10)]
        );
        // control: without /proc provenance the quick window takes the first generic save, crossing them
        assert_eq!(
            interleaved(false),
            vec![("0xf00".to_string(), 10), ("0xf01".to_string(), 3)]
        );
    }

    #[test]
    fn test_a_workspace_waits_for_its_late_window_instead_of_closing_its_phase() {
        // ws 3 has two tiled saves; the second app's window arrives long after its launch phase.
        // The workspace is not flushed half-built: both are placed together, in schedule order,
        // and the final check sees both at their saved geometry
        let saves = vec![
            client("app0", "t", 3, false, (12, 38), (941, 1030)),
            client("app1", "t", 3, false, (967, 38), (941, 1030)),
        ];
        let (entries, mut scripts) = launching(&saves, &[0, 1]);
        scripts[1] = vec![(
            LAUNCH_WAIT_TICKS + SETTLE_TICKS + 20,
            live_window("app1", 1),
        )];
        let host = FakeHost::new(vec![], scripts).dwindle();
        host.sim.set(true);
        let mut plan = plan_for(entries, saves);
        plan.tail_ticks = 3 * LAUNCH_WAIT_TICKS;
        let report = run_load(&host, &plan);
        assert_eq!(report.flushes.len(), 1, "one flush for the whole workspace");
        assert_eq!(report.flushes[0].windows, 2);
        assert_eq!(
            report.verification,
            vec![WorkspaceCheck {
                workspace: "3".into(),
                at_saved: 2,
                checked: 2,
                off: vec![]
            }]
        );
    }

    #[test]
    fn test_verification_is_read_once_at_the_end_and_counts_late_placements() {
        // a workspace that never completes is flushed at the end of the load, and a window placed
        // after it (on claim) is still part of the one final check
        let saves = tiled_saves(7, &S4[..3]);
        let (entries, mut scripts) = launching(&saves, &[0, 1, 2]);
        scripts[2] = vec![];
        let host = FakeHost::new(vec![], scripts).dwindle();
        let report = run_load(&host, &plan_for(entries, saves));
        // not simulating the compositor: the windows are not at their saved geometry, and it says so
        assert_eq!(report.verification.len(), 1);
        assert_eq!(
            (
                report.verification[0].workspace.as_str(),
                report.verification[0].checked
            ),
            ("7", 2)
        );
        assert_eq!(report.verification[0].off.len(), 2);
    }

    #[test]
    fn test_the_start_workspace_is_parked_and_rebuilt_in_schedule_order() {
        // windows destined for the workspace the load started on are born in its tree; they are
        // moved off to the stage and back in the steered order
        let saves = shape_saves(9, &WS9_C);
        let launch: Vec<usize> = (0..3).collect();
        let (entries, scripts) = launching(&saves, &launch);
        let host = FakeHost::new(vec![], scripts).dwindle();
        host.sim.set(true);
        host.start.set(9);
        host.cursor.set(Some((123, 456)));
        let report = run_load(&host, &plan_for(entries, saves));
        let sent = host.sent.borrow();
        let first_park = sent
            .iter()
            .position(|o| {
                matches!(
                    o,
                    Op::MoveToWorkspace {
                        workspace: crate::dispatch::WorkspaceTarget::Special(_),
                        ..
                    }
                )
            })
            .unwrap();
        let first_back = sent
            .iter()
            .position(|o| {
                matches!(
                    o,
                    Op::MoveToWorkspace {
                        workspace: crate::dispatch::WorkspaceTarget::Id(9),
                        ..
                    }
                )
            })
            .unwrap();
        let parks = sent
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    Op::MoveToWorkspace {
                        workspace: crate::dispatch::WorkspaceTarget::Special(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(parks, 3);
        assert!(first_park < first_back);
        drop(sent);
        assert_eq!(
            report.verification,
            vec![WorkspaceCheck {
                workspace: "9".into(),
                at_saved: 3,
                checked: 3,
                off: vec![]
            }]
        );
        assert!(report.flushes[0].steered);
    }

    #[test]
    fn test_the_start_workspace_is_left_alone_when_it_holds_windows_the_load_does_not_own() {
        let saves = shape_saves(9, &WS9_C);
        let (entries, scripts) = launching(&saves, &[0, 1, 2]);
        let stranger = foot_with_pid(40, 900, 9);
        let host = FakeHost::new(vec![stranger.clone()], scripts).dwindle();
        host.start.set(9);
        let report = run_load(&host, &plan_for(entries, saves));
        assert!(report.incomplete);
        assert_eq!(
            host.clients()
                .unwrap()
                .into_iter()
                .find(|c| c.address == stranger.address),
            Some(stranger)
        );
    }

    #[test]
    fn test_the_pointer_goes_back_to_where_it_was_before_anything_happened_and_after_the_focus() {
        // the pointer has moved by the time run_load starts (windows were closed first)
        let saves = shape_saves(7, &WS9_C);
        let (entries, scripts) = launching(&saves, &[0, 1, 2]);
        let host = FakeHost::new(vec![], scripts).dwindle();
        host.sim.set(true);
        host.origin.set(Some((111, 222)));
        host.cursor.set(Some((1866, 121)));
        let mut plan = plan_for(entries, saves);
        plan.restore_workspace = Some("4".into());
        run_load(&host, &plan);
        assert_eq!(host.cursor.get(), Some((111, 222)));
        let sent = host.sent.borrow();
        let focus = sent
            .iter()
            .position(|o| matches!(o, Op::FocusWorkspace(ws) if ws == "4"))
            .unwrap();
        let last_warp = sent
            .iter()
            .rposition(|o| matches!(o, Op::MoveCursor { .. }))
            .unwrap();
        assert!(
            focus < last_warp,
            "focusing can move the pointer, so it is restored after it"
        );
    }

    #[test]
    fn test_entry_signatures_and_process_matching() {
        let mut e = group_entry("x", vec![0]);
        e.command = "cd /home/m && foot --working-directory=/p".into();
        let sig = entry_signature(&e).unwrap();
        assert_eq!(
            sig,
            (
                vec!["foot".to_string(), "--working-directory=/p".to_string()],
                Some("/home/m".to_string())
            )
        );
        assert!(process_matches(
            &sig,
            &["/usr/bin/foot".into(), "--working-directory=/p".into()],
            Some("/home/m"),
        ));
        assert!(!process_matches(
            &sig,
            &["foot".into(), "--working-directory=/p".into()],
            Some("/elsewhere"),
        ));
        assert!(!process_matches(&sig, &["foot".into()], Some("/home/m")));
        e.command = "foot".into();
        assert_eq!(entry_signature(&e).unwrap().1, None);
    }

    // The round-3 fixtures (brief section 3), as geometry: WS4 = V(H(foot, playcall), Lions); WS10 = V(brave, H(foot, codex))
    const FIXTURE_WS4: [(i32, i32, i32, i32); 3] = [
        (12, 38, 953, 546),
        (967, 38, 1908, 1068),
        (12, 560, 953, 1068),
    ];
    const FIXTURE_WS10: [(i32, i32, i32, i32); 3] = [
        (12, 38, 953, 1068),
        (967, 38, 1908, 546),
        (967, 560, 1908, 1068),
    ];

    #[test]
    fn test_ws4_fixture_round_trips_even_when_the_first_window_of_the_tree_arrives_last() {
        // the occupant of the root's first leaf (the linked terminal) is the slow one; launches run
        // Lions, playcall-gui, then it. Nothing is placed until all three are there, then in schedule order.
        let saves = shape_saves(4, &FIXTURE_WS4);
        let (entries, mut scripts) = launching(&saves, &[1, 2, 0]);
        scripts[2] = vec![(
            LAUNCH_WAIT_TICKS + SETTLE_TICKS + 30,
            live_window("app0", 0),
        )];
        let host = FakeHost::new(vec![], scripts).dwindle();
        host.sim.set(true);
        host.cursor.set(Some((1866, 121)));
        let mut plan = plan_for(entries, saves.clone());
        plan.tail_ticks = 4 * LAUNCH_WAIT_TICKS;
        let report = run_load(&host, &plan);
        assert_eq!(report.flushes.len(), 1);
        assert_eq!(
            report.verification,
            vec![WorkspaceCheck {
                workspace: "4".into(),
                at_saved: 3,
                checked: 3,
                off: vec![]
            }]
        );
        // and the next generation, pointer elsewhere, is the same
        let (b, _, _) = simulated_load(&saves, (300, 900), true);
        assert_eq!(geometry(&b), geometry(&saves));
    }

    #[test]
    fn test_ws10_fixture_round_trips_when_it_is_the_start_workspace() {
        let saves = shape_saves(10, &FIXTURE_WS10);
        let (entries, scripts) = launching(&saves, &[0, 1, 2]);
        // the windows are born on ws 10 (the load was started there)
        let scripts: Vec<Vec<(u64, Client)>> = scripts
            .into_iter()
            .map(|v| {
                v.into_iter()
                    .map(|(d, mut c)| {
                        c.workspace = Workspace::numbered(10);
                        c.workspace.name = "10".into();
                        (d, c)
                    })
                    .collect()
            })
            .collect();
        let host = FakeHost::new(vec![], scripts).dwindle();
        host.sim.set(true);
        host.start.set(10);
        let report = run_load(&host, &plan_for(entries, saves));
        assert_eq!(
            report.verification,
            vec![WorkspaceCheck {
                workspace: "10".into(),
                at_saved: 3,
                checked: 3,
                off: vec![]
            }]
        );
    }
    #[test]
    fn scoped_adjustment_accepts_bare_named_workspace_alias() {
        let mut saved = client("notes", "notes", 1, false, (12, 38), (1896, 1030));
        saved.workspace = Workspace::from_selector("name:Work");
        let host = FakeHost::new(vec![saved.clone()], vec![]).dwindle();
        let mut plan = plan_for(vec![], vec![saved]);
        plan.scope = Some("Work".into());
        plan.tail_ticks = 1;
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 1);
        assert!(report.unmatched.is_empty());
    }

    #[test]
    fn process_identity_restores_window_after_its_app_id_changes() {
        let saved = client("browser-old", "document", 3, true, (30, 40), (640, 480));
        let mut live = live_window("browser-new", 0);
        live.pid = 700;
        let host = FakeHost::new(vec![], vec![vec![(1, live)]]);
        host.procs.borrow_mut().insert(
            700,
            ProcessInfo {
                argv: vec!["/usr/bin/browser".into()],
                cwd: None,
            },
        );
        let report = run_load(
            &host,
            &plan_for(vec![group_entry("/usr/bin/browser", vec![0])], vec![saved]),
        );
        assert!(
            report.unmatched.is_empty(),
            "a proven process must survive an app-id rename"
        );
        assert_eq!(host.clients().unwrap()[0].workspace.selector(), "3");
    }

    #[test]
    fn packed_process_argv_restores_a_renamed_application() {
        let saved = client("browser-old", "document", 3, false, (50, 0), (1, 1));
        let mut live = live_window("browser-new", 0);
        live.pid = 700;
        let host = FakeHost::new(vec![], vec![vec![(1, live)]]);
        proc_for(
            &host,
            700,
            &["/usr/bin/browser --profile 'two words'"],
            "/projects/a b",
        );
        let report = run_load(
            &host,
            &plan_for(
                vec![group_entry(
                    "cd '/projects/a b' && /usr/bin/browser --profile 'two words'",
                    vec![0],
                )],
                vec![saved],
            ),
        );
        assert!(report.unmatched.is_empty());
        assert_eq!(host.clients().unwrap()[0].workspace.selector(), "3");
    }

    #[test]
    fn renamed_application_requires_matching_arguments_and_directory() {
        for (argv, cwd) in [
            (
                vec!["/usr/bin/browser --profile 'other words'"],
                "/projects/a b",
            ),
            (
                vec!["/usr/bin/browser", "--profile two", "words"],
                "/projects/a b",
            ),
            (vec!["/usr/bin/browser --profile 'two words'"], "/elsewhere"),
            (
                vec!["/opt/other/browser", "--profile", "two words"],
                "/projects/a b",
            ),
            (vec!["", "--profile", "two words"], "/projects/a b"),
            (vec![], "/projects/a b"),
        ] {
            let saved = client("browser-old", "document", 3, false, (50, 0), (1, 1));
            let mut live = live_window("browser-new", 0);
            live.pid = 700;
            let host = FakeHost::new(vec![], vec![vec![(1, live)]]);
            proc_for(&host, 700, &argv, cwd);
            let report = run_load(
                &host,
                &plan_for(
                    vec![group_entry(
                        "cd '/projects/a b' && /usr/bin/browser --profile 'two words'",
                        vec![0],
                    )],
                    vec![saved],
                ),
            );
            assert_eq!(report.unmatched.len(), 1, "{argv:?} in {cwd}");
            assert_eq!(host.clients().unwrap()[0].workspace.selector(), "9");
        }
    }

    #[test]
    fn unrelated_slow_launch_does_not_delay_ready_applications() {
        let saves = vec![
            client("slow", "slow", 2, true, (10, 10), (640, 480)),
            client("fast", "fast", 3, true, (20, 20), (640, 480)),
        ];
        let host = FakeHost::new(
            vec![],
            vec![
                vec![(LAUNCH_WAIT_TICKS, live_window("slow", 0))],
                vec![(1, live_window("fast", 1))],
            ],
        );
        let report = run_load(
            &host,
            &plan_for(
                vec![group_entry("slow", vec![0]), group_entry("fast", vec![1])],
                saves,
            ),
        );
        assert!(report.unmatched.is_empty());
        let fast_due = host
            .windows
            .borrow()
            .iter()
            .find(|(_, c)| c.class == "fast")
            .unwrap()
            .0;
        assert!(
            fast_due < LAUNCH_WAIT_TICKS,
            "fast GUI was not launched until the slow one completed"
        );
    }

    #[test]
    fn foreign_tiled_window_does_not_strand_claims_in_stage() {
        let saves = tiled_saves(2, &[(12, 38, 941, 1030), (967, 38, 941, 1030)]);
        let (entries, scripts) = launching(&saves, &[0, 1]);
        let foreign = client("foreign", "foreign", 2, false, (0, 0), (100, 100));
        let foreign_address = foreign.address.clone();
        let host = FakeHost::new(vec![foreign], scripts).dwindle();
        let report = run_load(&host, &plan_for(entries, saves));
        assert!(report.incomplete);
        let live = host.clients().unwrap();
        assert!(live
            .iter()
            .filter(|c| c.class.starts_with("app"))
            .all(|c| c.workspace.selector() == "2"));
        assert!(!host.sent.borrow().iter().any(
            |op| matches!(op, Op::MoveToWorkspace { window, .. } if *window == foreign_address)
        ));
    }

    #[test]
    fn merge_shared_workspace_preserves_foreign_tiled_window() {
        let saves = tiled_saves(2, &[(12, 38, 941, 1030), (967, 38, 941, 1030)]);
        let (entries, scripts) = launching(&saves, &[0, 1]);
        let foreign = client("foreign", "foreign", 2, false, (0, 0), (100, 100));
        let mut host = FakeHost::new(vec![foreign.clone()], scripts).dwindle();
        host.merge = true;
        let report = run_load(&host, &plan_for(entries, saves));
        assert!(!report.incomplete, "{report:?}");
        assert!(report.unmatched.is_empty());
        let current = host.clients().unwrap();
        let foreign_after = current
            .iter()
            .find(|c| c.address == foreign.address)
            .unwrap();
        assert_eq!(foreign_after.workspace.selector(), "2");
        assert!(!foreign_after.floating);
        assert!(!host.sent.borrow().iter().any(
            |op| matches!(op, Op::MoveToWorkspace { window, .. } if *window == foreign.address)
        ));
    }

    #[test]
    fn merge_unshared_workspace_still_rejects_bad_geometry() {
        let saves = tiled_saves(3, &[(12, 38, 941, 1030)]);
        let (entries, scripts) = launching(&saves, &[0]);
        let foreign = client("foreign", "foreign", 2, false, (0, 0), (100, 100));
        let mut host = FakeHost::new(vec![foreign], scripts);
        host.merge = true;
        let report = run_load(&host, &plan_for(entries, saves));
        assert!(report.incomplete, "{report:?}");
        assert_eq!(report.verification[0].off.len(), 1);
    }

    #[test]
    fn merge_shared_workspace_still_rejects_bad_floating_geometry() {
        let saved = client("app", "t", 2, true, (200, 200), (640, 480));
        let mut launched = live_window("app", 0);
        launched.floating = true;
        let foreign = client("foreign", "foreign", 2, false, (0, 0), (100, 100));
        let mut host = FakeHost::new(vec![foreign], vec![vec![(1, launched)]]);
        host.merge = true;
        let report = run_load(
            &host,
            &plan_for(vec![group_entry("app", vec![0])], vec![saved]),
        );
        assert!(report.incomplete, "{report:?}");
        assert_eq!(report.verification[0].off.len(), 1);
    }

    #[test]
    fn merge_reuses_preexisting_renamed_application_with_packed_argv() {
        let saved = client("browser-old", "document", 3, false, (50, 0), (1, 1));
        let mut live = live_window("browser-new", 0);
        live.pid = 700;
        let duplicate = live_window("browser-new", 1);
        let mut host = FakeHost::new(vec![live.clone()], vec![vec![(1, duplicate)]]);
        host.merge = true;
        proc_for(
            &host,
            700,
            &["/usr/bin/browser --profile 'two words'"],
            "/projects/a b",
        );
        let report = run_load(
            &host,
            &plan_for(
                vec![group_entry(
                    "cd '/projects/a b' && /usr/bin/browser --profile 'two words'",
                    vec![0],
                )],
                vec![saved],
            ),
        );
        assert!(report.unmatched.is_empty(), "{report:?}");
        let current = host.clients().unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].address, live.address);
        assert_eq!(current[0].pid, 700);
        assert_eq!(current[0].workspace.selector(), "3");
    }

    #[test]
    fn merge_does_not_claim_conflicting_same_class_window() {
        let saved = client("browser", "saved document", 3, false, (50, 0), (1, 1));
        let mut foreign = live_window("browser", 0);
        foreign.title = "other document".into();
        foreign.pid = 700;
        let mut launched = live_window("browser", 1);
        launched.title = "saved document".into();
        let mut host = FakeHost::new(vec![foreign.clone()], vec![vec![(1, launched.clone())]]);
        host.merge = true;
        proc_for(&host, 700, &["/usr/bin/other-browser"], "/elsewhere");
        let report = run_load(
            &host,
            &plan_for(vec![group_entry("/usr/bin/browser", vec![0])], vec![saved]),
        );
        assert!(report.unmatched.is_empty(), "{report:?}");
        let current = host.clients().unwrap();
        assert_eq!(
            current
                .iter()
                .find(|c| c.address == foreign.address)
                .unwrap()
                .workspace
                .selector(),
            "9"
        );
        assert_eq!(
            current
                .iter()
                .find(|c| c.address == launched.address)
                .unwrap()
                .workspace
                .selector(),
            "3"
        );
    }

    #[test]
    fn merge_reuses_exact_class_and_title_without_matching_launcher() {
        let saved = client("appimage", "My application", 3, false, (50, 0), (1, 1));
        let mut live = live_window("appimage", 0);
        live.title = "My application".into();
        live.pid = 700;
        let duplicate = live_window("appimage", 1);
        let mut host = FakeHost::new(vec![live.clone()], vec![vec![(1, duplicate)]]);
        host.merge = true;
        proc_for(&host, 700, &["/tmp/.mount_app/usr/bin/app"], "/tmp");
        let report = run_load(
            &host,
            &plan_for(
                vec![group_entry("/home/user/app.AppImage", vec![0])],
                vec![saved],
            ),
        );
        assert!(report.unmatched.is_empty(), "{report:?}");
        let current = host.clients().unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].address, live.address);
        assert_eq!(current[0].pid, 700);
        assert_eq!(current[0].workspace.selector(), "3");
    }

    #[test]
    fn merge_late_conflicting_window_is_not_claimed() {
        let saved = client("browser", "saved", 3, false, (50, 0), (1, 1));
        let mut foreign = live_window("browser", 0);
        foreign.title = "other".into();
        foreign.pid = 700;
        let mut expected = live_window("browser", 1);
        expected.title = "saved".into();
        expected.pid = 701;
        let mut host = FakeHost::new(
            vec![],
            vec![vec![(1, foreign.clone()), (1, expected.clone())]],
        );
        host.merge = true;
        proc_for(&host, 700, &["/usr/bin/other"], "/");
        proc_for(&host, 701, &["/usr/bin/browser"], "/");
        let report = run_load(
            &host,
            &plan_for(vec![group_entry("/usr/bin/browser", vec![0])], vec![saved]),
        );
        assert!(report.unmatched.is_empty(), "{report:?}");
        let live = host.clients().unwrap();
        assert_eq!(
            live.iter()
                .find(|c| c.address == foreign.address)
                .unwrap()
                .workspace
                .selector(),
            "9"
        );
        assert_eq!(
            live.iter()
                .find(|c| c.address == expected.address)
                .unwrap()
                .workspace
                .selector(),
            "3"
        );
    }

    #[test]
    fn merge_late_foreign_tiled_window_shares_only_its_workspace() {
        let saved = client("app", "t", 2, false, (12, 38), (941, 1030));
        let foreign = client("foreign", "foreign", 2, false, (0, 0), (100, 100));
        let expected = live_window("app", 0);
        let mut host = FakeHost::new(
            vec![],
            vec![vec![(1, foreign.clone()), (1, expected.clone())]],
        );
        host.merge = true;
        let report = run_load(
            &host,
            &plan_for(vec![group_entry("app", vec![0])], vec![saved]),
        );
        assert!(!report.incomplete, "{report:?}");
        let live = host.clients().unwrap();
        assert_eq!(
            live.iter()
                .find(|c| c.address == foreign.address)
                .unwrap()
                .workspace
                .selector(),
            "2"
        );
        assert_eq!(
            live.iter()
                .find(|c| c.address == expected.address)
                .unwrap()
                .workspace
                .selector(),
            "2"
        );
    }

    #[test]
    fn merge_reuses_renamed_window_arriving_before_its_entry_launches() {
        let first = client("first", "t", 2, false, (50, 0), (1, 1));
        let second = client("old-id", "saved", 3, false, (51, 0), (1, 1));
        let first_live = live_window("first", 0);
        let mut existing = live_window("new-id", 1);
        existing.pid = 701;
        let duplicate = live_window("new-id", 2);
        let mut host = FakeHost::new(
            vec![],
            vec![
                vec![(0, existing.clone()), (1, first_live)],
                vec![(1, duplicate)],
            ],
        );
        host.merge = true;
        proc_for(&host, 701, &["/usr/bin/second"], "/");
        let report = run_load(
            &host,
            &plan_for(
                vec![
                    group_entry("first", vec![0]),
                    group_entry("/usr/bin/second", vec![1]),
                ],
                vec![first, second],
            ),
        );
        assert!(report.unmatched.is_empty(), "{report:?}");
        let live = host.clients().unwrap();
        assert_eq!(live.len(), 2);
        assert_eq!(
            live.iter()
                .find(|c| c.address == existing.address)
                .unwrap()
                .workspace
                .selector(),
            "3"
        );
    }

    #[test]
    fn merge_reconsiders_initial_window_after_title_changes() {
        let saved = client("app", "ready", 3, false, (50, 0), (1, 1));
        let mut existing = live_window("app", 0);
        existing.title = "starting".into();
        let mut host = FakeHost::new(vec![existing.clone()], vec![]);
        host.merge = true;
        host.title_updates
            .borrow_mut()
            .push((1, existing.address.clone(), "ready".into()));
        let report = run_load(
            &host,
            &plan_for(vec![group_entry("/usr/bin/app", vec![0])], vec![saved]),
        );
        assert!(report.unmatched.is_empty(), "{report:?}");
        let live = host.clients().unwrap();
        assert_eq!(live[0].address, existing.address);
        assert_eq!(live[0].workspace.selector(), "3");
    }

    #[test]
    fn merge_scoped_late_foreign_stays_outside_while_proven_app_moves_in() {
        let saved = client("app-old", "saved", 3, false, (51, 0), (1, 1));
        let mut expected = live_window("app-new", 1);
        expected.pid = 701;
        let mut foreign = live_window("app-old", 0);
        foreign.title = "other".into();
        foreign.pid = 700;
        let mut host = FakeHost::new(
            vec![],
            vec![vec![(1, foreign.clone()), (1, expected.clone())]],
        );
        host.merge = true;
        proc_for(&host, 700, &["/usr/bin/other"], "/");
        proc_for(&host, 701, &["/usr/bin/app"], "/");
        let mut plan = plan_for(vec![group_entry("/usr/bin/app", vec![0])], vec![saved]);
        plan.scope = Some("3".into());
        let report = run_load(&host, &plan);
        assert!(report.unmatched.is_empty(), "{report:?}");
        let live = host.clients().unwrap();
        assert_eq!(
            live.iter()
                .find(|c| c.address == foreign.address)
                .unwrap()
                .workspace
                .selector(),
            "9"
        );
        assert_eq!(
            live.iter()
                .find(|c| c.address == expected.address)
                .unwrap()
                .workspace
                .selector(),
            "3"
        );
    }

    #[test]
    fn merge_proven_group_cannot_claim_other_groups_matching_title() {
        let saves = vec![
            client("app", "first", 2, false, (50, 0), (1, 1)),
            client("app", "second", 2, false, (51, 0), (1, 1)),
            client("app", "other", 3, false, (52, 0), (1, 1)),
        ];
        let mut existing = live_window("app", 0);
        existing.title = "other".into();
        existing.pid = 700;
        let mut first = live_window("app", 1);
        first.title = "first".into();
        first.pid = 700;
        let mut other = live_window("app", 2);
        other.title = "other".into();
        other.pid = 701;
        let mut host = FakeHost::new(
            vec![existing.clone()],
            vec![vec![(1, first)], vec![(1, other.clone())]],
        );
        host.merge = true;
        proc_for(&host, 700, &["/usr/bin/a"], "/");
        proc_for(&host, 701, &["/usr/bin/b"], "/");
        let report = run_load(
            &host,
            &plan_for(
                vec![
                    group_entry("/usr/bin/a", vec![0, 1]),
                    group_entry("/usr/bin/b", vec![2]),
                ],
                saves,
            ),
        );
        assert!(report.unmatched.is_empty(), "{report:?}");
        let live = host.clients().unwrap();
        assert_ne!(
            live.iter()
                .find(|c| c.address == existing.address)
                .unwrap()
                .workspace
                .selector(),
            "3"
        );
        assert_eq!(
            live.iter()
                .find(|c| c.address == other.address)
                .unwrap()
                .workspace
                .selector(),
            "3"
        );
    }

    #[test]
    fn missing_steer_anchor_does_not_strand_survivor_in_stage() {
        let saves = tiled_saves(2, &[(12, 38, 941, 1030), (967, 38, 941, 1030)]);
        let rects: Vec<_> = saves
            .iter()
            .map(|c| {
                (
                    c.at.0 as f64,
                    c.at.1 as f64,
                    (c.at.0 + c.size.0) as f64,
                    (c.at.1 + c.size.1) as f64,
                )
            })
            .collect();
        let steps = dwindle::steer_schedule(&rects, 1.0, dwindle::CUT_TOLERANCE).unwrap();
        let survivor = steps[1].window;
        let (entries, mut scripts) = launching(&saves, &[0, 1]);
        scripts[steps[0].window].clear();
        let host = FakeHost::new(vec![], scripts).dwindle();
        let report = run_load(&host, &plan_for(entries, saves));
        assert!(report.incomplete);
        let live = host.clients().unwrap();
        assert_eq!(
            live.iter()
                .find(|c| c.class == format!("app{survivor}"))
                .unwrap()
                .workspace
                .selector(),
            "2"
        );
    }

    #[test]
    fn identical_process_signatures_match_saved_titles_in_reverse_arrival_order() {
        let saves = vec![
            client("app", "first", 2, true, (10, 10), (640, 480)),
            client("app", "second", 3, true, (10, 10), (640, 480)),
        ];
        let mut first = live_window("app", 0);
        first.title = "first".into();
        first.pid = 700;
        let mut second = live_window("app", 1);
        second.title = "second".into();
        second.pid = 701;
        let host = FakeHost::new(vec![], vec![vec![(2, first)], vec![(1, second)]]);
        for pid in [700, 701] {
            host.procs.borrow_mut().insert(
                pid,
                ProcessInfo {
                    argv: vec!["app".into()],
                    cwd: None,
                },
            );
        }
        let report = run_load(
            &host,
            &plan_for(
                vec![group_entry("app", vec![0]), group_entry("app", vec![1])],
                saves,
            ),
        );
        assert!(report.unmatched.is_empty());
        let mut moves = moves_to(&host);
        moves.sort();
        assert_eq!(moves, vec![("0xa00".into(), 2), ("0xa01".into(), 3)]);
    }

    #[test]
    fn restoration_waits_for_final_geometry_acknowledgements() {
        let live = live_window("app", 1);
        let mut saved = live.clone();
        saved.workspace = Workspace::from_selector("7");
        saved.size = (100, 100);
        let host = FakeHost::new(vec![], vec![vec![(1, live)]]);
        host.geometry_updates.borrow_mut().push((5, saved.clone()));
        let report = run_load(
            &host,
            &plan_for(vec![group_entry("app", vec![0])], vec![saved]),
        );
        assert!(!report.incomplete);
        assert_eq!(report.verification[0].at_saved, 1);
    }

    #[test]
    fn saved_tray_gui_is_revealed_and_placed() {
        let saved = client("tray-app", "document", 3, false, (50, 0), (1, 1));
        let host = FakeHost::new(vec![], vec![vec![]]);
        *host.tray_window.borrow_mut() = Some(live_window("tray-app", 0));
        let report = run_load(
            &host,
            &plan_for(vec![group_entry("/usr/bin/tray-app", vec![0])], vec![saved]),
        );
        assert!(report.unmatched.is_empty());
        assert!(!report.incomplete);
        let windows = host.clients().unwrap();
        assert_eq!(windows[0].workspace.selector(), "3");
    }

    #[test]
    fn mapped_gui_is_not_toggled_through_its_tray() {
        let saved = client("tray-app", "document", 3, false, (50, 0), (1, 1));
        let host = FakeHost::new(vec![], vec![vec![(1, live_window("tray-app", 0))]]);
        *host.tray_window.borrow_mut() = Some(live_window("extra", 1));
        let report = run_load(
            &host,
            &plan_for(vec![group_entry("/usr/bin/tray-app", vec![0])], vec![saved]),
        );
        assert!(report.unmatched.is_empty());
        let windows = host.clients().unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].class, "tray-app");
        assert_eq!(windows[0].workspace.selector(), "3");
    }

    #[test]
    fn literal_executable_path_with_spaces_is_not_packed_argv() {
        let dir = tempfile::tempdir().unwrap();
        let literal = dir.path().join("browser --profile work");
        std::fs::write(&literal, "#!/bin/sh\n").unwrap();
        let saved = client("browser-old", "document", 3, false, (50, 0), (1, 1));
        let mut live = live_window("browser-new", 0);
        live.pid = 700;
        let host = FakeHost::new(vec![], vec![vec![(1, live)]]);
        proc_for(
            &host,
            700,
            &[literal.to_str().unwrap()],
            dir.path().to_str().unwrap(),
        );
        let command = format!(
            "cd {} && {} --profile work",
            shell_quote(dir.path().to_str().unwrap()),
            shell_quote(dir.path().join("browser").to_str().unwrap())
        );
        let report = run_load(
            &host,
            &plan_for(vec![group_entry(&command, vec![0])], vec![saved]),
        );
        assert_eq!(report.unmatched.len(), 1);
        assert_eq!(host.clients().unwrap()[0].workspace.selector(), "9");
    }
}
