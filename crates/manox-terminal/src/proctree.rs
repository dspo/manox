//! Process-tree teardown and foreground-process introspection (unix only).
//!
//! Closing a terminal must not leak the shell's children — a `sleep 300 &`
//! outliving its tab is a leak the user cannot see. `PtyHandle::drop` cannot
//! block (it may run on a UI or runtime-worker thread), so the teardown moves onto a detached
//! thread: SIGTERM the target set, a short grace, SIGKILL the survivors,
//! reap the child.
//!
//! The target set is the shell, its whole descendant tree (a sysinfo
//! parent-chain walk — interactive shells give each job its own process
//! group, so a single group-kill misses background jobs), and the PTY's
//! current foreground process group (tcgetpgrp, captured by the caller).
//! The scan is the teardown's first action, before any signal or master
//! close — the caller moves the master onto the teardown thread so nothing
//! of ours can disturb the tree before it is recorded. Processes that
//! escaped the session (setsid / nohup) are deliberately left alone.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use portable_pty::{Child, ChildKiller};

/// Grace between SIGTERM and the SIGKILL escalation.
const TERM_GRACE: Duration = Duration::from_millis(100);
/// Poll interval while waiting for targets to die.
const POLL: Duration = Duration::from_millis(10);

/// Graceful-then-forceful teardown of a terminal's process tree. Blocks for
/// up to the grace window — run on a detached thread, never inline on a
/// UI or runtime-worker thread.
pub fn terminate(
    shell_pid: Option<libc::pid_t>,
    fg_pgid: Option<libc::pid_t>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    child: Option<Box<dyn Child + Send>>,
) {
    let mut pids: Vec<libc::pid_t> = shell_pid.into_iter().collect();
    if let Some(root) = shell_pid {
        pids.extend(descendant_pids(root));
    }
    if let Some(pgid) = fg_pgid {
        signal_group(pgid, libc::SIGTERM);
    }
    for &p in &pids {
        signal(p, libc::SIGTERM);
    }
    let deadline = Instant::now() + TERM_GRACE;
    while !all_gone(&pids, fg_pgid) && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    if let Some(pgid) = fg_pgid {
        signal_group(pgid, libc::SIGKILL);
    }
    for &p in &pids {
        signal(p, libc::SIGKILL);
    }
    // portable-pty's own killer is the final fallback for the direct child;
    // it targets the child pid even if the snapshot above was stale.
    if let Some(mut k) = killer {
        let _ = k.kill();
    }
    // Reap the child when `start` never moved it into the waiter thread.
    if let Some(mut c) = child {
        let _ = c.wait();
    }
}

/// Every descendant of `root` (children, grandchildren, …) from one sysinfo
/// snapshot. Processes that exit mid-walk simply vanish from the map and are
/// not followed.
pub fn descendant_pids(root: libc::pid_t) -> Vec<libc::pid_t> {
    let sys = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
    );
    let mut children_of: HashMap<libc::pid_t, Vec<libc::pid_t>> = HashMap::new();
    for (pid, proc_) in sys.processes() {
        if let Some(parent) = proc_.parent() {
            children_of
                .entry(parent.as_u32() as libc::pid_t)
                .or_default()
                .push(pid.as_u32() as libc::pid_t);
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if !seen.insert(p) {
            continue;
        }
        if let Some(kids) = children_of.get(&p) {
            for &k in kids {
                out.push(k);
                stack.push(k);
            }
        }
    }
    out
}

/// The comm name of a single process, resolved with a targeted refresh.
pub fn process_name(pid: libc::pid_t) -> Option<String> {
    let pid = sysinfo::Pid::from_u32(pid as u32);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing(),
    );
    sys.process(pid)
        .map(|p| p.name().to_string_lossy().into_owned())
}

fn signal(pid: libc::pid_t, sig: libc::c_int) {
    unsafe { libc::kill(pid, sig) };
}

/// Negative pid targets the whole process group (POSIX).
fn signal_group(pgid: libc::pid_t, sig: libc::c_int) {
    unsafe { libc::kill(-pgid, sig) };
}

/// kill(pid, 0) liveness: EPERM still means alive, ESRCH means gone. A
/// negative pid probes a whole process group. A zombie counts as gone — it
/// holds no resources and init reaps it; kill(pid, 0) would report it alive.
pub(crate) fn alive(pid: libc::pid_t) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        #[cfg(target_os = "linux")]
        if is_zombie(pid) {
            return false;
        }
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// /proc/<pid>/stat state field. The comm in parentheses may itself contain
/// spaces or parens, so the state is the byte after the last ") ".
#[cfg(target_os = "linux")]
fn is_zombie(pid: libc::pid_t) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(") ")
                .and_then(|(_, rest)| rest.as_bytes().first().copied())
        })
        == Some(b'Z')
}

fn all_gone(pids: &[libc::pid_t], fg_pgid: Option<libc::pid_t>) -> bool {
    pids.iter().all(|&p| !alive(p)) && fg_pgid.is_none_or(|g| !alive(-g))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendant_pids_finds_spawned_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let me = std::process::id() as libc::pid_t;
        let kid = child.id() as libc::pid_t;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if descendant_pids(me).contains(&kid) {
                break;
            }
            assert!(Instant::now() < deadline, "child never appeared in tree");
            std::thread::sleep(POLL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn descendant_pids_of_leaf_is_empty() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        assert!(descendant_pids(child.id() as libc::pid_t).is_empty());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn process_name_of_self_is_non_empty() {
        let me = std::process::id() as libc::pid_t;
        let name = process_name(me).expect("self must resolve");
        assert!(!name.is_empty());
    }
}
