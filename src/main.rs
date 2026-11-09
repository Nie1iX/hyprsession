use clap::{Parser, ValueEnum};
use std::env;
use std::path::Path;
use std::process::exit;

use hyprsession::command_detection::command_exists_in_path;
use hyprsession::command_faker::fake_command;
use hyprsession::dispatch::{resolve_dialect, Dialect, DialectChoice};
use hyprsession::lifecycle::Lifecycle;
use hyprsession::session::*;

#[derive(Copy, Clone, PartialEq, ValueEnum)]
enum Mode {
    /// Default mode: Load session and then save the session at intervals
    Default,

    /// Save the session
    Save,

    /// Save periodically without restoring
    Daemon,

    /// Save and freeze the snapshot, then exit Hyprland
    Logout,

    /// List available sessions
    List,

    /// Load a session then exit
    Load,

    /// Clear the current session
    Clear,

    /// Delete a session
    Delete,

    /// Create a command to fake applications
    Command,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Which mode to run the program in (default: default)
    #[arg(value_enum, default_value_t = Mode::Default)]
    mode: Mode,

    /// Name of the session to switch to or delete
    #[arg(default_value_t = String::from("default"))]
    name: String,

    /// Command to run (for Command mode)
    #[arg(default_value_t = String::from(""))]
    command: String,

    /// Interval between saving sessions (default: 60)
    #[arg(short = 'i', long, default_value_t = 60)]
    save_interval: u64,

    /// Time to wait before considering the session loaded (default: 60)
    #[arg(short = 'l', long, default_value_t = 60)]
    load_time: u64,

    /// Only simulate calls to Hyprland (supresses loading of session)
    #[arg(long, default_value_t = false)]
    simulate: bool,

    /// Dispatch dialect: auto probes the running Hyprland (default: auto)
    #[arg(long, value_enum, default_value_t = DialectChoice::Auto)]
    dialect: DialectChoice,

    /// Only adjust exiting clients without launching new programs
    #[arg(long, default_value_t = false)]
    adjust_clients_only: bool,

    /// Restore alongside existing windows (load mode only)
    #[arg(long, conflicts_with = "adjust_clients_only")]
    merge: bool,

    /// Only load this workspace of the session, leaving the others untouched (load mode only)
    #[arg(long)]
    workspace: Option<String>,
}

fn main() -> hyprland::Result<()> {
    // Snapshot argv, cwd and foreground commands are private session data.
    unsafe {
        libc::umask(0o077);
    }
    let args = Args::parse();
    if args.save_interval == 0 {
        eprintln!("Save interval needs to be greater than 0");
        exit(1);
    }
    if args.name.is_empty() || args.name == "." || args.name == ".." || args.name.contains('/') {
        eprintln!("Session name must be a single directory name");
        exit(1);
    }
    if args.workspace.is_some() && args.mode != Mode::Load {
        eprintln!("--workspace can only be used with load");
        exit(1);
    }
    if args.merge && args.mode != Mode::Load {
        eprintln!("--merge can only be used with load");
        exit(1);
    }
    let root = env::var("HYPRSESSION_PATH").unwrap_or_else(|_| {
        format!(
            "{}/.local/share/hyprsession",
            env::var("HOME").unwrap_or_default()
        )
    });
    let lifecycle = Lifecycle::new(Path::new(&root))?;
    let mut session = LocalSession {
        base_path: root,
        load_time: args.load_time,
        adjust_clients_only: args.adjust_clients_only,
        merge: args.merge,
        simulate: args.simulate,
        save_duplicate_pids: false,
        dialect: Dialect::Classic,
        workspace: args.workspace,
    };
    match args.mode {
        Mode::Default | Mode::Daemon => lifecycle.daemon(
            &mut session,
            &args.name,
            args.dialect,
            args.save_interval,
            args.mode == Mode::Default,
        )?,
        Mode::Save => {
            lifecycle.save(&session, &args.name)?;
        }
        Mode::Logout => lifecycle.logout(&session, &args.name, args.dialect)?,
        Mode::Load => lifecycle.restore(&mut session, &args.name, args.dialect, false)?,
        Mode::Clear => {
            session.dialect = resolve_dialect(args.dialect, args.simulate)
                .map_err(hyprland::error::HyprError::Other)?
                .0;
            session.clear()?;
        }
        Mode::List => {
            println!("Available sessions:");
            for name in session.list() {
                println!(" - {name}");
            }
        }
        Mode::Delete => session.delete(&args.name),
        Mode::Command => {
            if command_exists_in_path(&args.name) {
                eprintln!("Command already exists in PATH: {}", args.name);
                exit(1);
            }
            fake_command(&args.name, &args.command)
                .map_err(|e| hyprland::error::HyprError::Other(e.to_string()))?;
        }
    }
    Ok(())
}
