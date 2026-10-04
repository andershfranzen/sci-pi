//! A persistent shell per session. It lives on the daemon like tmux does: clients attach,
//! get the recent scrollback, and detach without killing it.

use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

const SCROLLBACK: usize = 512 * 1024;

pub struct Terminal {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    /// Scrollback and the live channel share a lock so attach never misses or repeats output.
    output: Mutex<(VecDeque<u8>, broadcast::Sender<Arc<[u8]>>)>,
    alive: Arc<AtomicBool>,
}

impl Terminal {
    pub fn spawn(cwd: &Path) -> Result<Arc<Self>> {
        let pair = native_pty_system().openpty(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 })?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let mut cmd = CommandBuilder::new(shell);
        cmd.arg("-l");
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let (tx, _) = broadcast::channel(1024);
        let term = Arc::new(Terminal {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            output: Mutex::new((VecDeque::new(), tx)),
            alive: Arc::new(AtomicBool::new(true)),
        });
        let t = term.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut out = t.output.lock().unwrap();
                        out.0.extend(&buf[..n]);
                        let excess = out.0.len().saturating_sub(SCROLLBACK);
                        out.0.drain(..excess);
                        let _ = out.1.send(Arc::from(&buf[..n]));
                    }
                }
            }
            t.alive.store(false, Ordering::SeqCst);
            // Dropping every sender would need the struct gone; an empty chunk signals exit.
            let _ = t.output.lock().unwrap().1.send(Arc::from(&[][..]));
        });
        Ok(term)
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Current scrollback plus a receiver for everything after it.
    pub fn attach(&self) -> (Vec<u8>, broadcast::Receiver<Arc<[u8]>>) {
        let out = self.output.lock().unwrap();
        (out.0.iter().copied().collect(), out.1.subscribe())
    }

    pub fn write(&self, data: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(data);
        let _ = w.flush();
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if cols > 0 && rows > 0 {
            let _ = self.master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        }
    }

    pub fn kill(&self) {
        let _ = self.child.lock().unwrap().kill();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.kill();
    }
}
