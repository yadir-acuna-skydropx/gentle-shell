//! Minimal PTY wrapper for the session server (S34). Unlike the partitioner's
//! `pane.rs` this does NO vt100 emulation: the server relays the master's raw
//! bytes straight to the attached client, so every escape sequence passes
//! through unchanged. Reuses portable-pty like the rest of the partitioner.

use std::io::{Read, Write};

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
}

fn clamp(v: u16) -> u16 {
    v.max(1)
}

impl Pty {
    /// Spawns `cmd` in a fresh PTY of `rows`x`cols`.
    pub fn spawn(cmd: CommandBuilder, rows: u16, cols: u16) -> Result<Self> {
        let (rows, cols) = (clamp(rows), clamp(cols));
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let writer = pair.master.take_writer()?;
        Ok(Self { master: pair.master, writer, child, size: (rows, cols) })
    }

    /// A clone of the master read side, for the relay thread.
    pub fn reader(&self) -> Result<Box<dyn Read + Send>> {
        Ok(self.master.try_clone_reader()?)
    }

    pub fn write_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes).and_then(|_| self.writer.flush());
    }

    /// Resizes the PTY; a no-op when unchanged. Resizing raises SIGWINCH in the
    /// child's foreground process group.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (clamp(rows), clamp(cols));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }

    /// Forces the child to repaint by nudging the size by one column and back,
    /// guaranteeing a SIGWINCH even when the new client size equals the current
    /// one (S34: a reattached TUI must fully repaint).
    pub fn force_redraw(&mut self) {
        let (rows, cols) = self.size;
        let nudged = if cols > 1 { cols - 1 } else { cols + 1 };
        let _ = self.master.resize(PtySize { rows, cols: nudged, pixel_width: 0, pixel_height: 0 });
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }

    /// The child's process id, if known.
    pub fn child_pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Blocks until the child exits and returns its exit code.
    pub fn wait(&mut self) -> i32 {
        match self.child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => -1,
        }
    }
}
