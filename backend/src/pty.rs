use std::io::{Read, Write};
use std::path::Path;

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

/// A shell running in a pty, bridged to async code. portable-pty I/O is
/// blocking, so a reader thread pumps output into a tokio channel and a
/// writer thread drains input from a std channel.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    pub output: tokio::sync::mpsc::Receiver<Vec<u8>>,
    input: std::sync::mpsc::Sender<Vec<u8>>,
}

impl PtySession {
    pub fn spawn(shell: &str, cwd: &Path, rows: u16, cols: u16) -> anyhow::Result<Self> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow::anyhow!("openpty: {e}"))?;

        let mut cmd = CommandBuilder::new(shell);
        cmd.arg("-l"); // login shell so the user's PATH (nvm, cargo, ...) loads
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("spawn shell: {e}"))?;
        let killer = child.clone_killer();
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("pty reader: {e}"))?;
        let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // EOF: shell exited
                    Ok(n) => {
                        if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
            // Reap the child so no zombie sticks around after exit.
            let _ = child.wait();
        });

        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("pty writer: {e}"))?;
        let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            while let Ok(data) = in_rx.recv() {
                if writer.write_all(&data).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            master: pair.master,
            killer,
            output: out_rx,
            input: in_tx,
        })
    }

    pub fn write(&self, data: Vec<u8>) {
        let _ = self.input.send(data);
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    pub fn kill(&mut self) {
        let _ = self.killer.kill();
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.kill();
    }
}
