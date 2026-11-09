# Hyprsession
## Overview
Hyprland 0.57-compatible fork of [mjgreenjr/hyprsession](https://github.com/mjgreenjr/hyprsession), retaining its [fork baseline](https://github.com/mjgreenjr/hyprsession/commit/7b31b3a5e8be42a211eedd75fcbbd73d99d519f0) and the history of the original [joshurtree/hyprsession](https://github.com/joshurtree/hyprsession). It works through Hyprland IPC without the HyprToroid plugin. GPL-3.0; upstream credits and license are retained.

The authoritative snapshot is `clients.json`: modern workspace addresses/types, recorded argv, working directories and window state. GUI applications are not restricted to an allowlist. Launches use recorded argv or a matching desktop entry when the executable is unavailable. This relaunches applications, not live process memory.
Compositor previews/update dialogs are excluded as infrastructure. Ordinary GUI windows remain eligible. Noctalia Settings uses its supported `noctalia msg settings-open` activation: rerunning its shell process alone does not recreate that window.

## Installation and startup
Build this checkout, not the unported crates.io/AUR package:

```sh
git clone https://github.com/Nie1iX/hyprsession.git
cd hyprsession
cargo build --release --locked
```

An up-to-date Rust/Cargo toolchain is required. The HyprToroid Arch package builds this source and installs `/usr/lib/hyprtoroid/hyprsession`; the installed Lua profile launches that binary with `default --dialect lua --save-interval 5` and sets `HYPRSESSION_PATH` to `${XDG_STATE_HOME:-$HOME/.local/state}/hyprtoroid/hyprsession`. Nested previews do not start it. A standalone `cargo build` does not replace the installed binary.

`default` restores before autosaving; `daemon` only autosaves. A manual snapshot is `hyprsession save <session-name>`. Use `logout --dialect lua` for the final save/freeze/exit sequence. Snapshots and control files are private (`0600`, directories `0700`).
In HyprToroid UWSM profiles, `HYPRTOROID_UWSM=1` switches the final logout action to `uwsm stop`, after the same save/freeze sequence. A failed stop remains an error and leaves the snapshot frozen. Profiles without this flag retain native Hyprland exit.


This independent fork is maintained at [Nie1iX/hyprsession](https://github.com/Nie1iX/hyprsession). [HyprToroid](https://github.com/Nie1iX/hyprtoroid) packages it as a separate executable at a pinned source revision, with an optional `HYPRSESSION_SOURCE_DIR` override for local development. Only that integration sets the snapshot path above and `HYPRTOROID_UWSM`; standalone use retains the CLI's defaults. Upstream compatibility work can be checked manually at [PR #19](https://github.com/joshurtree/hyprsession/pull/19) and [issue #20](https://github.com/joshurtree/hyprsession/issues/20).

## Options
Various options can be used to modify the behavior of Hyprsession.

### [mode]
One of
* default - Merge the saved session into the startup desktop, then save at regular intervals after verified success.
* load - Restore a named snapshot. Existing mapped windows cause refusal without closing anything, unless `--merge` or `--adjust-clients-only` is explicitly selected.
* save - Save the current session unless its snapshot is protected.
* daemon - Save periodically without restoring; only one daemon per compositor instance.
* logout - Save and freeze the final snapshot, then stop UWSM when `HYPRTOROID_UWSM=1`, otherwise exit Hyprland.
* clear - Clear the current session (removes all windows)
* list - List all available sessions
* delete - Delete a session
* command - Create a command to deal with edge cases (see below)
 
### [name]
Name of the session or command

### [command]
Command to run (see below) 

### -l, --load-time <load_time>
Modern GUI launch groups start concurrently and share a wait budget of `max(load_time, 20 seconds)`. Loading ends early when all saved windows are placed. Resume-wrapper and legacy entries retain sequential launching and the post-launch tail wait. A process that stays running without opening its saved window can exhaust the budget; this remains an incomplete restore and protects the snapshot.

### -i, --save-interval <save_interval>
This sets the interval in seconds between session saves. The default is 60 seconds.

### --simulate
Only simulate loading and clearing of sessions

### --dialect <dialect>
Selects how dispatches are sent to Hyprland: `auto` (default), `lua` or `classic`.
`auto` probes the running compositor (under `--simulate` it guesses from the config files instead
and sends nothing). Use `lua` for Hyprland builds using the Lua config provider.

### --adjust-clients-only
Place existing clients to match the snapshot without launching or closing programs.

### --merge
Restore alongside existing windows without closing or moving unrelated applications off their workspaces. Valid only with `load`, and incompatible with `--adjust-clients-only`; startup `default` uses this policy automatically. Eligible existing saved windows are reused without another launch. Unambiguous process ownership takes priority over class/title matching and bounds a window to its own launch group; exact class/title can identify an existing window when its launcher has changed. Unrelated windows arriving during the restore are also preserved.

On a workspace shared with unrelated tiled windows, only the saved tiled rectangles may differ. Saved window counts, workspaces, floating/pin/fullscreen state, floating geometry and geometry on unshared workspaces still have to verify. A failed protected snapshot can be retried explicitly with `load <same-name> --merge` in the same compositor; automatic startup retries require a new compositor. `load` never starts an autosave daemon or writes a new snapshot.

### --workspace <workspace>
Only restore the selected workspace (`3`, `name:Work` or `special:scratchpad`; bare normal names also work). Existing windows outside the selected scope are left untouched. This is valid only with `load`. A scoped restore never authorizes replacement of the complete autosave snapshot.

### -h, --help
Display program help

### -v, --version
Display program version

## Command detection and bridge commands
The recorded argv is preferred, retaining application arguments and profiles. Desktop entries are resolved by class/application ID when argv is unavailable, points into a Flatpak sandbox or references a vanished executable such as an AppImage mount. Applications still need to support reopening their own documents/tabs/windows; missing windows are an incomplete restore, not permission to replace the original snapshot.
Placement waits for initial client configures before checking the final state. A late maximize request is reconciled once with the saved state; fullscreen dispatches disable layout-aware mode conversion. The loader then allows up to two seconds for configure/animation completion, ending early when all claimed windows match. It does not relaunch applications during this wait or suppress missing windows and geometry failures.
Process matching accepts a rewritten one-element command line only at the ownership boundary, preserving ordinary argv boundaries and existing singleton executable paths containing spaces. Different absolute executable paths cannot establish ownership merely through the same basename.

Modern GUI groups that have produced no windows after three seconds receive one standard StatusNotifierItem activation attempt. The item must have a unique verified owner: matching command/cwd and canonical `/proc/PID/exe`, with no mapped windows. Well-known D-Bus names are pinned to their unique connection before activation. This uses `busctl` without service autostart, never re-executes the application, and does not authorize autosave unless the saved windows actually appear and verify. Unsupported or unsuccessful tray activation remains an incomplete restore.

For an application without a usable argv or desktop entry, the explicit bridge-command utility remains available:

```sh
hyprsession command firefox "flatpak run org.mozilla.firefox"
```

It creates a launcher in `~/.local/bin`.

## Working directories
Each window is launched from the working directory it had when it was saved (`cd <dir> && <command>`), so programs
started with relative paths (`./my-app`) come back. If the directory no longer exists the program is launched from
the loader's directory and a warning names the window and the missing directory. Windows of sandboxed (Flatpak)
applications are launched through their desktop entry and are not wrapped.

## Programs running inside terminals
When a saved window is a terminal (currently `foot`, `kitty` and `alacritty`), hyprsession looks in `/proc` for the
program holding the terminal's foreground and records its command line and directory in `clients.json`
(the optional `foreground` field). On load the terminal is relaunched running that program, in its recorded
directory; when the program exits the terminal falls through to a shell prompt instead of closing. The terminal's own
recorded directory is that of its shell.

Which command is run is decided by program name:
* a program in the resume table is started with its resume command (below);
* other programs return at a clean prompt by default; replay requires explicit `unlisted = "replay"`;
* a program that owns its own window (for instance a GUI app started from the terminal, matched by process id)
  is not relaunched in the terminal: the terminal returns at a prompt and the program returns through its own window.

The built-in resume table (replacement command lines; any other flags that were recorded are dropped):

| Program | Resume command |
|---|---|
| claude | `claude --continue` |
| muse | `muse resume --last` |
| codex | `codex resume --last` |
| gemini | `gemini --resume` |
| aider | `aider --restore-chat-history` |
| opencode | `opencode --continue` |
| copilot | `copilot --continue` |
| goose | `goose session --resume` |
| qwen | `qwen --continue` |

`~/.config/hyprsession/resume.toml` (or `$XDG_CONFIG_HOME/hyprsession/resume.toml`) extends and overrides it:
```toml
[settings]
unlisted = "prompt"      # "replay" is an explicit opt-in

[resume]
claude = "claude --continue --model sonnet"
mytool = "mytool --last"
aider  = ""              # resume nothing: restore the terminal at a prompt
```
A bad resume file stops loading before any launch or adjustment, including under `--simulate`.

Note that the command lines of foreground programs are written to `clients.json`: do not keep secrets in
command-line arguments if snapshots are shared.

## Window counts and reports
Programs that own several saved windows are launched once. If fewer windows than were saved appear, the launch is
repeated (at most once per missing window); a shortfall that remains is reported with the saved windows left
unmatched, and the load fails while preserving the snapshot. After placement, live windows that no saved window claimed are listed
(address, class, title, workspace); they are never touched.

## Tiled window order
Dwindle builds its tree from insertion order and focus/pointer state. Placing each window as it maps would let
app start-up latency decide who sits where. A load
therefore claims tiled windows as they appear but places each workspace's tiled windows together, once every saved
tiled window of that workspace is claimed or its launches are over. For each workspace the loader rebuilds the saved
tree from the saved rectangles and inserts the windows in the order that regrows it. Before each insertion it focuses
the existing leaf and steers the pointer; immediately afterward it restores that split's orientation and exact ratio,
before either child gains descendants. The pointer is put back where it was when loading finishes. The final workspace,
float/pin/fullscreen state and geometry are read back from the compositor; mismatches fail the restore.
Floating windows are placed exactly as before.
If an unrelated tiled window occupies a destination, or a required split anchor is missing, surviving claimed windows
are placed normally instead of rebuilding that tree. In merge mode, shared-workspace tiled rectangles may differ;
otherwise a geometry mismatch keeps the restore incomplete and the snapshot protected.

If the compositor does not accept a pointer warp (checked by reading the pointer back), the order is computed for the
pointer as found; when that cannot regrow the tree (layout other than dwindle, `dwindle:force_split` other than 2, a
pointer exactly on a boundary) the workspace falls back to the saved reading order and the load says why. If the
result differs from the saved geometry, snapshot protection remains in place. Non-50/50 dividers are reconstructed;
grouped tabs are not. Manual split orientation requires the original orientation-preserving dwindle configuration.

A workspace is placed only when all its tiled windows have arrived (or at the end of the load), never half built, and
each window is bound to the launch that produced it by reading its process (`/proc`) rather than by arrival time or
title, so a slow window cannot be mistaken for another entry's. Existing matched windows are parked on a hidden stage
before rebuilding a workspace, including adjust-only restores; unrelated tiled windows prevent this rebuild.
The geometry check runs once, after everything is placed, and reports each workspace.
The pointer's position is captured before placement and restored after the final focus change.

## Snapshot protection
Loading never clears the desktop. Startup merges saved applications with the current desktop; unrelated windows
are preserved rather than treated as a reason to skip restoration. Wallet-unlock dialogs (`org.kde.ksecretd`) are
infrastructure, not saved applications. Failed/interrupted restoration keeps its protection across logins;
a different named snapshot or partial restore cannot clear it. Automatic retries require a new compositor;
explicit `load <same-name> --merge` can retry in place. Successful startup authorization is bound to the snapshot
name: restoring one snapshot cannot authorize skipping another. Disabling restore with an existing snapshot
never records success or authorizes autosave after re-enabling.
`logout` freezes writes before taking its final snapshot, so an autosaver cannot publish the shutdown's empty desktop.
A file named `disabled` in `HYPRSESSION_PATH` disables restore/autosave.

## Change log
### Unreleased
* Publish the independent fork with upstream attribution; remove obsolete release automation and the broken inherited VM harness
* Run CLI and library code from the same modules; keep isolated behavior tests and remove test-only implementation copies and host-dependent launcher tests
* Merge startup with unrelated autostart windows; explicit non-destructive `load --merge` recovery in the same compositor, process-bound reuse and shared-workspace tiled geometry checks
* Bind successful startup authorization to the snapshot name; keep disabled existing snapshots protected
* HyprToroid: Hyprland 0.57 workspace IPC and explicit Lua state setters, generic GUI relaunch, terminal prompt safety
* Place tiled windows per workspace so the saved tree regrows exactly: steered insertion order, pointer restored afterwards (computed order, then reading order, as fallbacks)
* Protect snapshots across failed/interrupted/scoped restores; final-save/freeze/logout and daemon locking
* Launch windows from their saved working directory
* Restore the program running in the foreground of a terminal (resume table, `resume.toml`)
* Launch again when a program produced fewer windows than were saved
* `--workspace` to load a single workspace
* Report live windows that match no saved window
### 0.1.1
* Changed --session-path option to point at base directory of session file
### 0.1.2
* Fixed bug which would crash program if no session file existed
### 0.1.3
* Fix fatal bug on Hyprland 0.4 that crashed the program while saving the session  
### 0.1.4
* Changed fullscreen dispatcher to use fullscreenmode and fixed FullscreenMode type issue (#2)
* Updated to latest alpha version of hyprland crate. Fixes panic on fetching clients (#1)
### 0.1.5
* Skip saving clients with duplicate pids (i.e. generated by the same application)
* Add option to retain clients with duplicate pids
### 0.1.6
* Handle special workspaces properly
* Handle wrapped programs in Nixos
### 0.2.0
* Breaking change: Remove the need to use '--mode' to specify what mode to launch the program in
* Introduce named sessions
* Improve method of finding originating command for clients
* Track client creation to try and better place them
* Introduce bridge command functionality to deal with programs complicated launch patterns (flatpak, snap etc.)
* Refactor code to allow potential future use as either standalone or as a client
### 0.2.1
* Fix faulty post client creation logic
* Cleanup and properly document parameters
* Add parameter documentation test
* Use .desktop files to help find correct command

## Development

Offline Rust tests cover snapshot compatibility, restore planning and placement, workspace addressing, terminal safety and lifecycle protection. See [tests/README.md](tests/README.md) for commands and isolation requirements.

For compositor smoke checks, use a separate nested Hyprland with private XDG/DBus state. Never run `clear`, `load` or `logout` tests against your working session. This standalone IPC tool is maintained independently of the HyprToroid plugin.

By default, sessions are saved to `~/.local/share/hyprsession`. Set `HYPRSESSION_PATH` to a disposable directory for experiments; this alone does not isolate compositor IPC or logout commands.

## Thanks

Thank you to the following people for helping to improve this project

* Tie C (ticia)
* Isaac Hesslegrave (HeadedBranch)
* Hornet (fsilly)