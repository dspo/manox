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
    for key in &spec.env_remove {
        cmd.env_remove(key);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
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
}
