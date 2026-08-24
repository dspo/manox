// PTY allocation and child spawn.
//
// cx owns the master side; the slave becomes the agent's controlling terminal
// (stdin/stdout/stderr). The slave handle is dropped in the parent right after
// spawn so the master sees EOF once the child exits.

use std::io::{Read, Write};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

use crate::LaunchSpec;

pub(crate) struct PtySession {
    pub(crate) master: Box<dyn portable_pty::MasterPty + Send>,
    pub(crate) child: Box<dyn portable_pty::Child + Send>,
    pub(crate) reader: Box<dyn Read + Send>,
    pub(crate) writer: Box<dyn Write + Send>,
}

/// Spawn `spec.program` with its args/env inside a freshly allocated PTY.
///
/// `extra_env` carries environment that is only known at relay time (e.g.
/// `CX_WARP_SESSION_ID`); it is applied after `env_remove` and `env` from the spec.
pub(crate) fn spawn_pty(spec: &LaunchSpec, extra_env: &[(&str, String)]) -> Result<PtySession> {
    let pty_system = NativePtySystem::default();
    // Initial size is a placeholder; the writer loop syncs the real terminal size
    // shortly after start via SIGWINCH handling.
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty 失败")?;

    let mut cmd = CommandBuilder::new(&spec.program);
    for arg in &spec.args {
        cmd.arg(arg);
    }
    // manox is the host terminal for every embedded agent PTY, regardless of how
    // manox itself was launched. A GUI launch (Finder/Dock/Spotlight) carries no
    // TERM/COLORTERM in its environment, and CommandBuilder snapshots the parent
    // env — so without these fallbacks the agent inherits a terminal-less
    // environment and TUIs (Claude Code) render monochrome and skip mouse capture.
    // `env_remove`, `env`, and `extra_env` apply after these and win, so explicit
    // spec values keep their precedence.
    cmd.env("TERM_PROGRAM", "manox");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    if std::env::var_os("TERM").is_none() {
        cmd.env("TERM", "xterm-256color");
    }
    if std::env::var_os("COLORTERM").is_none() {
        cmd.env("COLORTERM", "truecolor");
    }
    for key in &spec.env_remove {
        cmd.env_remove(key);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    if let Some(cwd) = spec.cwd.as_deref() {
        cmd.cwd(cwd);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("PTY spawn `{}` 失败", spec.program.display()))?;

    // Drop the slave handle in the parent so EOF propagates to the master reader
    // when the child exits or closes its stdio.
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .context("clone master reader 失败")?;
    let writer = pair
        .master
        .take_writer()
        .context("take master writer 失败")?;

    Ok(PtySession {
        master: pair.master,
        child,
        reader,
        writer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LaunchSpec;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::path::PathBuf;

    /// `/bin/cat` echoes input back via the PTY slave's line discipline, so a
    /// write to the master should be observable on the master reader.
    #[test]
    fn spawn_pty_round_trips_bytes() {
        let spec = LaunchSpec {
            program: PathBuf::from("/bin/cat"),
            args: vec![],
            env: BTreeMap::new(),
            summary: String::new(),
            detach: false,
            env_remove: vec![],
            agent_id: "test".into(),
            provider_name: "test".into(),
            model_id: None,
            pty: true,
            socket: None,
            cwd: None,
        };
        let pty = spawn_pty(&spec, &[]).expect("spawn_pty");
        let mut writer = pty.writer;
        let mut reader = pty.reader;
        let mut child = pty.child;

        writer.write_all(b"hello\n").expect("write");
        writer.flush().expect("flush");

        let mut buf = [0u8; 128];
        let n = reader.read(&mut buf).expect("read");
        let out = String::from_utf8_lossy(&buf[..n]);
        assert!(
            out.contains("hello"),
            "expected the PTY to echo 'hello', got: {out:?}"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// Run `/usr/bin/env` inside the PTY and collect its `KEY=VALUE` output.
    /// The child exits immediately, so the master reader reaches EOF once the
    /// output is drained. PTY output cooking turns `\n` into `\r\n`, so callers
    /// must strip `\r` before comparing lines.
    fn spawn_env_and_collect(
        spec_env: BTreeMap<String, String>,
        env_remove: Vec<String>,
    ) -> (Vec<String>, PtySession) {
        let spec = LaunchSpec {
            program: PathBuf::from("/usr/bin/env"),
            args: vec![],
            env: spec_env,
            summary: String::new(),
            detach: false,
            env_remove,
            agent_id: "test".into(),
            provider_name: "test".into(),
            model_id: None,
            pty: true,
            socket: None,
            cwd: None,
        };
        let mut pty = spawn_pty(&spec, &[]).expect("spawn_pty");
        let mut out = String::new();
        pty.reader.read_to_string(&mut out).expect("read env");
        let lines = out
            .lines()
            .map(|l| l.trim_end_matches('\r').to_string())
            .collect();
        (lines, pty)
    }

    /// The child always sees manox as its terminal host: `TERM_PROGRAM` is
    /// manox, and `TERM`/`COLORTERM` are non-empty whether inherited from a
    /// terminal-bearing parent env or supplied by the spawn_pty fallbacks.
    #[test]
    fn spawn_pty_always_reports_terminal_env() {
        let (lines, mut pty) = spawn_env_and_collect(BTreeMap::new(), vec![]);
        assert!(
            lines.iter().any(|l| l == "TERM_PROGRAM=manox"),
            "expected TERM_PROGRAM=manox in child env, got: {lines:?}"
        );
        for key in ["TERM", "COLORTERM"] {
            let prefix = format!("{key}=");
            let present = lines
                .iter()
                .any(|l| l.strip_prefix(&prefix).is_some_and(|v| !v.is_empty()));
            assert!(
                present,
                "expected a non-empty {key} in child env, got: {lines:?}"
            );
        }
        let _ = pty.child.kill();
        let _ = pty.child.wait();
    }

    /// Explicit `spec.env` wins over the fallbacks: a spec-supplied TERM reaches
    /// the child unchanged instead of being replaced by the xterm-256color
    /// fallback.
    #[test]
    fn spawn_pty_spec_env_overrides_terminal_fallbacks() {
        let mut env = BTreeMap::new();
        env.insert("TERM".to_string(), "dumb".to_string());
        let (lines, mut pty) = spawn_env_and_collect(env, vec![]);
        assert!(
            lines.iter().any(|l| l == "TERM=dumb"),
            "expected spec.env TERM=dumb to win, got: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l == "TERM=xterm-256color"),
            "fallback TERM leaked past the spec.env override: {lines:?}"
        );
        let _ = pty.child.kill();
        let _ = pty.child.wait();
    }

    /// `spec.env_remove` wins over the fallbacks too: removing TERM leaves the
    /// child with no TERM at all, whether the value would have been inherited
    /// from the parent env or supplied by the xterm-256color fallback. The
    /// assertion is robust to both test environments for that reason.
    #[test]
    fn spawn_pty_env_remove_strips_term() {
        let (lines, mut pty) = spawn_env_and_collect(BTreeMap::new(), vec!["TERM".to_string()]);
        assert!(
            !lines.iter().any(|l| l.starts_with("TERM=")),
            "env_remove should strip TERM from the child env, got: {lines:?}"
        );
        let _ = pty.child.kill();
        let _ = pty.child.wait();
    }
}
