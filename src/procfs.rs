//! Reading what is running inside a terminal from /proc: the shell on the terminal's
//! pty, and the program holding the pty's foreground process group.
//!
//! All /proc access goes through `ProcFs` so the walk can be tested against a
//! synthetic process tree.

use std::collections::{HashMap, HashSet};
use std::fs;

/// The fields of /proc/<pid>/stat the walk needs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcStat {
    pub ppid: i32,
    pub pgrp: i32,
    pub session: i32,
    /// Controlling terminal device number; 0 for none
    pub tty_nr: i32,
    /// Foreground process group of the controlling terminal; -1 for none
    pub tpgid: i32,
}

/// Parse /proc/<pid>/stat. `comm` is parenthesised and may itself contain spaces and
/// parentheses, so the fields are located after the *last* `)`.
pub fn parse_stat(text: &str) -> Option<ProcStat> {
    let rest = &text[text.rfind(')')? + 1..];
    // state ppid pgrp session tty_nr tpgid ...
    let mut fields = rest.split_whitespace().skip(1);
    let mut next = || fields.next()?.parse::<i32>().ok();
    Some(ProcStat {
        ppid: next()?,
        pgrp: next()?,
        session: next()?,
        tty_nr: next()?,
        tpgid: next()?,
    })
}

pub trait ProcFs {
    /// The pid of the process doing the capturing
    fn own_pid(&self) -> i32;
    fn pids(&self) -> Vec<i32>;
    fn stat(&self, pid: i32) -> Option<ProcStat>;
    fn argv(&self, pid: i32) -> Option<Vec<String>>;
    fn cwd(&self, pid: i32) -> Option<String>;
    /// Basename of the executable
    fn exe_name(&self, pid: i32) -> Option<String>;
}

pub struct RealProc;

impl ProcFs for RealProc {
    fn own_pid(&self) -> i32 {
        std::process::id() as i32
    }

    fn pids(&self) -> Vec<i32> {
        fs::read_dir("/proc")
            .map(|dir| {
                dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn stat(&self, pid: i32) -> Option<ProcStat> {
        parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    fn argv(&self, pid: i32) -> Option<Vec<String>> {
        crate::command_detection::read_argv(pid)
    }

    fn cwd(&self, pid: i32) -> Option<String> {
        crate::command_detection::read_cwd(pid)
    }

    fn exe_name(&self, pid: i32) -> Option<String> {
        let exe = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        Some(exe.file_name()?.to_string_lossy().into_owned())
    }
}

/// The program in the foreground of a terminal's tty
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForegroundCapture {
    /// Leader of the foreground process group
    pub pid: i32,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    /// Every pid in the foreground process group
    pub group_pids: Vec<i32>,
    /// Every descendant of the leader
    pub descendant_pids: Vec<i32>,
}

/// What looking inside a terminal process found
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalCapture {
    /// The terminal runs several shells in one process: they cannot be told apart by
    /// pid, so nothing is recorded
    pub ambiguous: bool,
    /// Working directory of the terminal's shell (it may have `cd`'d since launch)
    pub shell_cwd: Option<String>,
    /// Set only when something other than the shell holds the foreground
    pub foreground: Option<ForegroundCapture>,
}

/// Bound on the walk down from the terminal process to its shell
const MAX_DEPTH: usize = 8;

/// All processes' stat, read once for a whole save
pub struct ProcTable {
    stats: HashMap<i32, ProcStat>,
    children: HashMap<i32, Vec<i32>>,
}

impl ProcTable {
    pub fn read(proc: &dyn ProcFs) -> Self {
        let mut stats = HashMap::new();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        let mut pids = proc.pids();
        pids.sort_unstable();
        for pid in pids {
            if let Some(stat) = proc.stat(pid) {
                children.entry(stat.ppid).or_default().push(pid);
                stats.insert(pid, stat);
            }
        }
        ProcTable { stats, children }
    }

    fn descendants(&self, root: i32) -> Vec<i32> {
        let mut found = vec![];
        let mut queue = vec![root];
        let mut seen: HashSet<i32> = HashSet::from([root]);
        while let Some(pid) = queue.pop() {
            for &child in self.children.get(&pid).into_iter().flatten() {
                if seen.insert(child) {
                    found.push(child);
                    queue.push(child);
                }
            }
        }
        found.sort_unstable();
        found
    }

    /// Shells on the terminal's ptys: the first session leader with a controlling tty on
    /// each path down from the terminal process
    fn shells(&self, terminal: i32) -> Vec<i32> {
        let mut shells = vec![];
        let mut level = vec![terminal];
        for _ in 0..MAX_DEPTH {
            let mut next = vec![];
            for pid in level {
                for &child in self.children.get(&pid).into_iter().flatten() {
                    match self.stats.get(&child) {
                        Some(s) if s.session == child && s.tty_nr != 0 => shells.push(child),
                        Some(_) => next.push(child),
                        None => {}
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            level = next;
        }
        shells
    }
}

/// Look inside a terminal process: find its shell, and the foreground program if any
pub fn capture_terminal(
    proc: &dyn ProcFs,
    table: &ProcTable,
    terminal_pid: i32,
) -> TerminalCapture {
    let shells = table.shells(terminal_pid);
    let shell = match shells.as_slice() {
        [] => return TerminalCapture::default(),
        [shell] => *shell,
        _ => {
            return TerminalCapture {
                ambiguous: true,
                ..Default::default()
            }
        }
    };
    let mut capture = TerminalCapture {
        shell_cwd: proc.cwd(shell),
        ..Default::default()
    };
    let Some(shell_stat) = table.stats.get(&shell) else {
        return capture;
    };

    // A terminal relaunched by hyprsession runs its resumed program under a non-interactive
    // `sh -c` wrapper without job control: the program shares the wrapper's process group, so
    // the foreground probe below sees "the shell". While the wrapper is still the terminal's
    // shell the program is running, and the wrapper's own arguments name it. (Once the program
    // ends the wrapper execs the user's shell, whose argv carries no marker: nothing recorded.)
    if let Some((cwd, command)) = proc
        .argv(shell)
        .as_deref()
        .and_then(crate::resume::wrapper_program)
    {
        let descendants = table.descendants(shell);
        if descendants.contains(&proc.own_pid()) {
            return capture;
        }
        let mut group_pids = vec![shell];
        group_pids.extend(descendants.iter().copied().filter(|p| {
            table
                .stats
                .get(p)
                .is_some_and(|s| s.pgrp == shell_stat.pgrp)
        }));
        capture.foreground = Some(ForegroundCapture {
            pid: shell,
            argv: crate::command_detection::shell_split(&command),
            cwd: Some(cwd),
            descendant_pids: descendants,
            group_pids,
        });
        return capture;
    }

    // The shell holding the foreground itself means nothing else is running
    let fg_group = shell_stat.tpgid;
    if fg_group <= 0 || fg_group == shell_stat.pgrp {
        return capture;
    }
    let mut group: Vec<i32> = table
        .stats
        .iter()
        .filter(|(_, s)| s.pgrp == fg_group && s.tty_nr == shell_stat.tty_nr)
        .map(|(&pid, _)| pid)
        .collect();
    group.sort_unstable();
    // Normally the group leader; if it has exited (a pipeline's first stage), the lowest pid left
    let leader = if group.contains(&fg_group) {
        fg_group
    } else {
        match group.first() {
            Some(&pid) => pid,
            None => return capture,
        }
    };
    // When hyprsession itself was run from the terminal (`hyprsession save`), it is the
    // terminal's foreground job. That is not a program to bring back: treat the shell as foreground.
    let descendants = table.descendants(leader);
    let own = proc.own_pid();
    if group.contains(&own) || descendants.contains(&own) {
        return capture;
    }
    let Some(argv) = proc.argv(leader) else {
        return capture;
    };
    capture.foreground = Some(ForegroundCapture {
        pid: leader,
        argv,
        cwd: proc.cwd(leader),
        descendant_pids: descendants,
        group_pids: group,
    });
    capture
}

#[cfg(test)]
pub mod fake {
    use super::*;

    /// A synthetic process tree
    #[derive(Default)]
    pub struct FakeProc {
        pub procs: HashMap<i32, FakeEntry>,
        /// The pid the capture claims to be running as (0: none of the fake processes)
        pub own: i32,
    }

    #[derive(Clone)]
    pub struct FakeEntry {
        pub stat: ProcStat,
        pub argv: Vec<String>,
        pub cwd: Option<String>,
    }

    impl FakeProc {
        pub fn add(
            &mut self,
            pid: i32,
            ppid: i32,
            pgrp: i32,
            session: i32,
            tty_nr: i32,
            tpgid: i32,
            argv: &[&str],
            cwd: &str,
        ) {
            self.procs.insert(
                pid,
                FakeEntry {
                    stat: ProcStat {
                        ppid,
                        pgrp,
                        session,
                        tty_nr,
                        tpgid,
                    },
                    argv: argv.iter().map(|s| s.to_string()).collect(),
                    cwd: Some(cwd.to_string()),
                },
            );
        }
    }

    impl ProcFs for FakeProc {
        fn own_pid(&self) -> i32 {
            self.own
        }
        fn pids(&self) -> Vec<i32> {
            self.procs.keys().copied().collect()
        }
        fn stat(&self, pid: i32) -> Option<ProcStat> {
            self.procs.get(&pid).map(|p| p.stat)
        }
        fn argv(&self, pid: i32) -> Option<Vec<String>> {
            self.procs
                .get(&pid)
                .map(|p| p.argv.clone())
                .filter(|a| !a.is_empty())
        }
        fn cwd(&self, pid: i32) -> Option<String> {
            self.procs.get(&pid).and_then(|p| p.cwd.clone())
        }
        fn exe_name(&self, pid: i32) -> Option<String> {
            self.procs
                .get(&pid)?
                .argv
                .first()
                .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
        }
    }

    /// A terminal (pid 100) -> shell (pid 101, session leader on tty 5) tree. The shell is
    /// in the foreground unless a program is added with `run_in_foreground`.
    pub fn terminal_with_shell() -> FakeProc {
        let mut p = FakeProc::default();
        p.add(1, 0, 1, 1, 0, -1, &["init"], "/");
        p.add(100, 1, 100, 100, 0, -1, &["foot"], "/home/synthetic");
        p.add(
            101,
            100,
            101,
            101,
            5,
            101,
            &["-zsh"],
            "/home/synthetic/shell-dir",
        );
        p
    }

    /// Put a program in the shell's tty foreground (process group = its pid)
    pub fn run_in_foreground(p: &mut FakeProc, pid: i32, argv: &[&str], cwd: &str) {
        p.add(pid, 101, pid, 101, 5, pid, argv, cwd);
        p.procs.get_mut(&101).unwrap().stat.tpgid = pid;
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    fn capture(p: &FakeProc, terminal: i32) -> TerminalCapture {
        capture_terminal(p, &ProcTable::read(p), terminal)
    }

    #[test]
    fn test_parse_stat_handles_awkward_comm() {
        let line = "4242 (my (weird) prog) S 100 4242 4242 34817 4242 4194304 1 0";
        let s = parse_stat(line).unwrap();
        assert_eq!(
            s,
            ProcStat {
                ppid: 100,
                pgrp: 4242,
                session: 4242,
                tty_nr: 34817,
                tpgid: 4242
            }
        );
        assert!(parse_stat("garbage").is_none());
        assert!(parse_stat("1 (x) S 1 2").is_none());
    }

    #[test]
    fn test_parse_stat_of_a_real_process() {
        let own = std::fs::read_to_string("/proc/self/stat").unwrap();
        let s = parse_stat(&own).unwrap();
        assert_eq!(s.ppid, std::os::unix::process::parent_id() as i32);
    }

    #[test]
    fn test_a_shell_in_the_foreground_records_nothing() {
        let p = terminal_with_shell();
        let c = capture(&p, 100);
        assert_eq!(c.foreground, None);
        // But the shell's own cwd is known
        assert_eq!(c.shell_cwd.as_deref(), Some("/home/synthetic/shell-dir"));
        assert!(!c.ambiguous);
    }

    #[test]
    fn test_foreground_program_is_captured_with_its_own_argv_and_cwd() {
        let mut p = terminal_with_shell();
        run_in_foreground(&mut p, 200, &["claude"], "/home/synthetic/proj");
        let fg = capture(&p, 100).foreground.unwrap();
        assert_eq!(fg.pid, 200);
        assert_eq!(fg.argv, vec!["claude"]);
        assert_eq!(fg.cwd.as_deref(), Some("/home/synthetic/proj"));
        assert_eq!(fg.group_pids, vec![200]);
    }

    #[test]
    fn test_shell_cwd_differs_from_the_terminal_process_cwd() {
        // The terminal process was launched from /home/synthetic; the shell has cd'd
        let p = terminal_with_shell();
        assert_eq!(p.cwd(100).as_deref(), Some("/home/synthetic"));
        assert_eq!(
            capture(&p, 100).shell_cwd.as_deref(),
            Some("/home/synthetic/shell-dir")
        );
    }

    #[test]
    fn test_pipeline_whose_leader_exited_records_the_lowest_surviving_pid() {
        let mut p = terminal_with_shell();
        // Group 300 lost its leader; two stages remain
        p.add(302, 101, 300, 101, 5, 300, &["grep", "x"], "/tmp");
        p.add(301, 101, 300, 101, 5, 300, &["tail", "-f", "log"], "/tmp");
        p.procs.get_mut(&101).unwrap().stat.tpgid = 300;
        let fg = capture(&p, 100).foreground.unwrap();
        assert_eq!(fg.pid, 301);
        assert_eq!(fg.argv, vec!["tail", "-f", "log"]);
        assert_eq!(fg.group_pids, vec![301, 302]);
    }

    #[test]
    fn test_unreadable_foreground_argv_records_nothing() {
        let mut p = terminal_with_shell();
        run_in_foreground(&mut p, 200, &[], "/tmp");
        let c = capture(&p, 100);
        assert_eq!(c.foreground, None);
        assert!(c.shell_cwd.is_some());
    }

    #[test]
    fn test_the_capturing_process_as_foreground_leader_records_nothing() {
        // `hyprsession save` run from the terminal is the terminal's foreground job
        let mut p = terminal_with_shell();
        run_in_foreground(
            &mut p,
            200,
            &["hyprsession", "save"],
            "/home/synthetic/proj",
        );
        p.own = 200;
        let c = capture(&p, 100);
        assert_eq!(c.foreground, None);
        // The shell's cwd is still recorded
        assert_eq!(c.shell_cwd.as_deref(), Some("/home/synthetic/shell-dir"));
    }

    #[test]
    fn test_the_capturing_process_below_a_wrapper_leader_records_nothing() {
        // e.g. `cargo run -- save`: the leader is a wrapper, hyprsession is its descendant
        let mut p = terminal_with_shell();
        run_in_foreground(&mut p, 200, &["cargo", "run"], "/home/synthetic/proj");
        p.add(
            201,
            200,
            201,
            101,
            5,
            200,
            &["hyprsession", "save"],
            "/home/synthetic/proj",
        );
        p.own = 201;
        let c = capture(&p, 100);
        assert_eq!(c.foreground, None);
        assert!(c.shell_cwd.is_some());
    }

    #[test]
    fn test_the_capturing_process_elsewhere_in_the_foreground_group_records_nothing() {
        let mut p = terminal_with_shell();
        run_in_foreground(
            &mut p,
            200,
            &["sh", "-c", "hyprsession save | tee log"],
            "/tmp",
        );
        p.add(202, 101, 200, 101, 5, 200, &["hyprsession", "save"], "/tmp");
        p.own = 202;
        assert_eq!(capture(&p, 100).foreground, None);
    }

    #[test]
    fn test_an_unrelated_foreground_program_is_still_recorded_when_saving_elsewhere() {
        let mut p = terminal_with_shell();
        run_in_foreground(&mut p, 200, &["claude"], "/home/synthetic/proj");
        // hyprsession runs as a daemon outside this terminal
        p.add(900, 1, 900, 900, 0, -1, &["hyprsession"], "/");
        p.own = 900;
        let fg = capture(&p, 100).foreground.unwrap();
        assert_eq!(fg.argv, vec!["claude"]);
    }

    #[test]
    fn test_descendants_of_the_leader_are_collected() {
        let mut p = terminal_with_shell();
        run_in_foreground(
            &mut p,
            200,
            &["sh", "./playcall-gui"],
            "/home/synthetic/proj",
        );
        // The script's child, in the same pty session but another group
        p.add(
            201,
            200,
            201,
            101,
            5,
            200,
            &["playcall-gui"],
            "/home/synthetic/proj",
        );
        p.add(202, 201, 201, 101, 5, 200, &["helper"], "/");
        let fg = capture(&p, 100).foreground.unwrap();
        assert_eq!(fg.descendant_pids, vec![201, 202]);
    }

    #[test]
    fn test_two_shells_in_one_terminal_process_are_ambiguous() {
        let mut p = terminal_with_shell();
        p.add(110, 100, 110, 110, 6, 110, &["-zsh"], "/tmp");
        let c = capture(&p, 100);
        assert!(c.ambiguous);
        assert_eq!(c.foreground, None);
        assert_eq!(c.shell_cwd, None);
    }

    #[test]
    fn test_no_pty_under_the_process_records_nothing() {
        let mut p = FakeProc::default();
        p.add(100, 1, 100, 100, 0, -1, &["foot"], "/");
        assert_eq!(capture(&p, 100), TerminalCapture::default());
    }

    #[test]
    fn test_shell_is_found_below_an_intermediate_process() {
        // Some terminals fork a helper between themselves and the shell
        let mut p = FakeProc::default();
        p.add(100, 1, 100, 100, 0, -1, &["alacritty"], "/");
        p.add(150, 100, 100, 100, 0, -1, &["helper"], "/");
        p.add(151, 150, 151, 151, 7, 151, &["bash"], "/home/synthetic");
        assert_eq!(
            capture(&p, 100).shell_cwd.as_deref(),
            Some("/home/synthetic")
        );
    }
}

#[cfg(test)]
mod real_proc_tests {
    use super::*;

    /// A real pty: `script` stands in for the terminal emulator, its child shell for the
    /// terminal's shell, and `sleep` for the foreground program. Skipped without `script`.
    #[test]
    fn test_real_proc_finds_a_foreground_program_on_a_real_pty() {
        let Ok(mut terminal) = std::process::Command::new("script")
            .args(["-q", "-e", "-c", "bash --norc --noprofile -i", "/dev/null"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return;
        };
        let pid = terminal.id() as i32;
        // An interactive shell puts each job in its own process group, as a user's shell does
        std::io::Write::write_all(terminal.stdin.as_mut().unwrap(), b"sleep 30\n").unwrap();
        let mut found = None;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let table = ProcTable::read(&RealProc);
            let capture = capture_terminal(&RealProc, &table, pid);
            if capture.foreground.is_some() {
                found = Some(capture);
                break;
            }
        }
        let _ = terminal.kill();
        let _ = terminal.wait();
        let capture = found.expect("foreground program was not found");
        let foreground = capture.foreground.unwrap();
        // Don't leave the sleep behind
        let _ = std::process::Command::new("kill")
            .arg(foreground.pid.to_string())
            .status();
        assert_eq!(foreground.argv, vec!["sleep".to_string(), "30".to_string()]);
        assert!(capture.shell_cwd.is_some());
    }
}
