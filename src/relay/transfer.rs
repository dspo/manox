// Bidirectional byte relay between the user terminal and the PTY master.
//
// Stdin is read as raw bytes (no crossterm event decoding) so that multibyte
// input, pastes, and Ctrl+C (0x03 under raw mode) pass through verbatim to the
// slave, whose line discipline turns 0x03 into SIGINT for the agent. A single
// writer thread serializes writes from stdin and IPC so they never interleave.

use std::io::{BufRead, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use crossterm::terminal;
use portable_pty::{MasterPty, PtySize};

/// A unit of work for the master writer thread.
///
/// `Bytes` carries stdin/IPC input to write to the master; `Resize` carries an
/// explicit window size (library API) to apply to the master's kernel winsize.
/// The relay tears down via `process::exit` (the agent's EOF ends the main
/// thread, which finalizes and exits), so there is no shutdown message — stdin
/// and IPC both send `Bytes` only.
pub(crate) enum WriteReq {
    Bytes(Vec<u8>),
    Resize(u16, u16),
}

/// Read raw stdin verbatim and forward to the writer channel.
///
/// Returns when stdin hits EOF (e.g. Ctrl+D) or the channel is closed.
pub(crate) fn stdin_forward(tx: mpsc::Sender<WriteReq>) {
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(WriteReq::Bytes(buf[..n].to_vec())).is_err() {
                        break; // writer thread gone
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// Consume the writer channel and write to the master, serializing stdin/IPC.
///
/// Uses `recv_timeout` so a blocked receive still wakes periodically to service
/// the SIGWINCH flag: when set, the master is resized to match the user terminal.
pub(crate) fn writer_loop(
    mut writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<WriteReq>,
    master: Box<dyn MasterPty + Send>,
    resize_flag: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(WriteReq::Bytes(bytes)) => {
                    if writer.write_all(&bytes).is_err() {
                        break; // master closed
                    }
                    let _ = writer.flush();
                }
                Ok(WriteReq::Resize(cols, rows)) => {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if resize_flag.swap(false, Ordering::Relaxed) {
                        if let Ok((cols, rows)) = terminal::size() {
                            let _ = master.resize(PtySize {
                                rows,
                                cols,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

/// Lines received over an IPC stream, decoded as `{"text":"..."}` JSON.
///
/// Each decoded `text` is forwarded as `text + '\n'` bytes — equivalent to the
/// user typing the line and pressing enter. Unparseable lines are silently
/// dropped so a peer can't crash the relay with malformed input.
pub(crate) fn forward_ipc_stream(
    stream: std::os::unix::net::UnixStream,
    tx: mpsc::Sender<WriteReq>,
) {
    thread::spawn(move || {
        let reader = std::io::BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            #[derive(serde::Deserialize)]
            struct InjectRequest {
                text: String,
            }
            if let Ok(req) = serde_json::from_str::<InjectRequest>(&line) {
                let mut bytes = req.text.into_bytes();
                bytes.push(b'\n');
                if tx.send(WriteReq::Bytes(bytes)).is_err() {
                    break; // relay tearing down
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn forward_ipc_stream_decodes_text_to_bytes() {
        let (mut a, b) = UnixStream::pair().expect("pair");
        let (tx, rx) = mpsc::channel();
        forward_ipc_stream(b, tx);
        a.write_all(b"{\"text\":\"hello world\"}\n").expect("write");
        a.flush().expect("flush");
        let req = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("expected a Bytes request");
        match req {
            WriteReq::Bytes(b) => assert_eq!(b, b"hello world\n"),
            WriteReq::Resize(..) => panic!("expected Bytes, got Resize"),
        }
    }

    #[test]
    fn forward_ipc_stream_skips_malformed_and_blank_lines() {
        let (mut a, b) = UnixStream::pair().expect("pair");
        let (tx, rx) = mpsc::channel();
        forward_ipc_stream(b, tx);
        // Malformed JSON and a blank line must be silently skipped, not crash.
        a.write_all(b"not json\n\n").expect("write");
        a.write_all(b"{\"text\":\"ok\"}\n").expect("write");
        a.flush().expect("flush");
        let req = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("expected the valid request after skipped lines");
        match req {
            WriteReq::Bytes(b) => assert_eq!(b, b"ok\n"),
            WriteReq::Resize(..) => panic!("expected Bytes, got Resize"),
        }
    }

    /// The writer loop must pump channel bytes onto the master so that the slave
    /// (here `/bin/cat`) echoes them back on the master reader — this is the
    /// IPC/stdin → agent path wired end-to-end (minus the socket layer).
    #[test]
    fn writer_loop_pumps_channel_bytes_to_master() {
        use crate::LaunchSpec;
        use crate::relay::pty::spawn_pty;
        use std::collections::BTreeMap;
        use std::io::Read;
        use std::path::PathBuf;
        use std::sync::atomic::AtomicBool;
        use std::time::Instant;

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
        let (tx, rx) = mpsc::channel();
        let flag = Arc::new(AtomicBool::new(false));
        writer_loop(pty.writer, rx, pty.master, flag);
        let mut reader = pty.reader;
        let mut child = pty.child;

        tx.send(WriteReq::Bytes(b"hello\n".to_vec())).expect("send");

        let mut buf = [0u8; 128];
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(n) = reader.read(&mut buf) {
                if n > 0 {
                    out.extend_from_slice(&buf[..n]);
                    if out.windows(5).any(|w| w == b"hello") {
                        break;
                    }
                }
            }
        }
        assert!(
            String::from_utf8_lossy(&out).contains("hello"),
            "expected cat to echo 'hello' via the writer loop, got: {:?}",
            String::from_utf8_lossy(&out)
        );

        let _ = child.kill();
        let _ = child.wait();
    }
}
