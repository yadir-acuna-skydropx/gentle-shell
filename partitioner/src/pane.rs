//! A partition backed by its own pseudo-terminal (S3): gentle-shell runs
//! unchanged inside it (S1). A reader thread feeds the vt100 emulator and
//! wakes the UI only when output arrives (S2, S11).
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

pub const SCROLLBACK: usize = 2000;

pub struct PtyPane {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    alive: Arc<AtomicBool>,
    size: (u16, u16),
}

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap()
}

impl PtyPane {
    /// Spawns `cmd` in a PTY of `rows` x `cols`; `notify` runs on the reader
    /// thread after every chunk of output and once when the child exits.
    pub fn spawn(cmd: CommandBuilder, rows: u16, cols: u16, notify: impl Fn() + Send + 'static) -> Result<Self> {
        let (rows, cols) = (rows.max(1), cols.max(1));
        let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(nz(rows), nz(cols), SCROLLBACK)));
        let alive = Arc::new(AtomicBool::new(true));
        let (p, a) = (parser.clone(), alive.clone());
        std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        p.lock().unwrap().process(&chunk[..n]);
                        notify();
                    }
                }
            }
            a.store(false, Ordering::Release);
            notify();
        })?;
        Ok(Self { parser, writer, master: pair.master, child, alive, size: (rows, cols) })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.writer.write_all(bytes).and_then(|_| self.writer.flush());
        }
    }

    /// Resizes PTY and emulator together; a no-op when the size is unchanged.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        self.parser.lock().unwrap().set_size(nz(rows), nz(cols));
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        f(self.parser.lock().unwrap().screen())
    }

    /// Scrolls the chat scrollback; positive `delta` reveals older output.
    /// vt100 clamps the target to the available scrollback.
    pub fn scroll_chat(&mut self, delta: isize) {
        let mut parser = self.parser.lock().unwrap();
        let current = parser.screen().scrollback() as isize;
        let target = (current + delta).max(0) as usize;
        parser.screen_mut().set_scrollback(target);
    }

    /// Jumps back to the live bottom of the scrollback (used on fresh input).
    pub fn reset_scroll(&mut self) {
        self.parser.lock().unwrap().screen_mut().set_scrollback(0);
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for PtyPane {
    fn drop(&mut self) {
        self.kill();
    }
}

pub fn convert_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Copies the emulator screen into `buf` at `area`, clipping to both sizes.
pub fn render_screen(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) {
    let area = area.intersection(buf.area);
    for row in 0..area.height {
        for col in 0..area.width {
            let Some(cell) = screen.cell(row, col) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut modifier = Modifier::empty();
            for (on, m) in [
                (cell.bold(), Modifier::BOLD),
                (cell.italic(), Modifier::ITALIC),
                (cell.underline(), Modifier::UNDERLINED),
                (cell.inverse(), Modifier::REVERSED),
            ] {
                if on {
                    modifier |= m;
                }
            }
            let style = Style::default()
                .fg(convert_color(cell.fgcolor()))
                .bg(convert_color(cell.bgcolor()))
                .add_modifier(modifier);
            let symbol = if cell.has_contents() { cell.contents() } else { " " };
            buf[(area.x + col, area.y + row)].set_symbol(symbol).set_style(style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn screen_of(bytes: &[u8], rows: u16, cols: u16) -> vt100::Parser {
        let mut p = vt100::Parser::new(nz(rows), nz(cols), 0);
        p.process(bytes);
        p
    }

    #[test]
    fn colors_map_to_terminal_palette() {
        assert_eq!(convert_color(vt100::Color::Default), Color::Reset);
        assert_eq!(convert_color(vt100::Color::Idx(1)), Color::Indexed(1));
        assert_eq!(convert_color(vt100::Color::Rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
    }

    #[test]
    fn render_places_text_with_styles_at_area_offset() {
        let p = screen_of(b"\x1b[1;31mhi\x1b[0m \x1b[7mX", 3, 10);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        render_screen(p.screen(), Rect::new(5, 1, 10, 3), &mut buf);
        let h = &buf[(5, 1)];
        assert_eq!(h.symbol(), "h");
        assert_eq!(h.fg, Color::Indexed(1));
        assert!(h.modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(6, 1)].symbol(), "i");
        assert_eq!(buf[(7, 1)].symbol(), " ");
        assert!(buf[(8, 1)].modifier.contains(Modifier::REVERSED));
        // outside the area stays untouched
        assert_eq!(buf[(4, 1)].symbol(), " ");
        assert_eq!(buf[(4, 1)].fg, Color::Reset);
    }

    #[test]
    fn render_handles_wide_chars_and_clips() {
        let p = screen_of("界a".as_bytes(), 2, 4);
        let mut buf = Buffer::empty(Rect::new(0, 0, 3, 1));
        render_screen(p.screen(), Rect::new(0, 0, 3, 1), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "界");
        assert_eq!(buf[(2, 0)].symbol(), "a");
    }

    #[cfg(unix)]
    #[test]
    fn spawned_child_output_reaches_the_screen_and_exit_is_detected() {
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "printf 'ready'; sleep 0.2"]);
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        let pane = PtyPane::spawn(cmd, 5, 20, move || {
            h.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pane.with_screen(|s| s.contents().contains("ready")) {
            assert!(Instant::now() < deadline, "no output");
            std::thread::sleep(Duration::from_millis(20));
        }
        while pane.is_alive() {
            assert!(Instant::now() < deadline, "exit not detected");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(hits.load(Ordering::SeqCst) >= 2, "notify on output and on exit");
    }

    #[cfg(unix)]
    #[test]
    fn scroll_chat_moves_scrollback_and_reset_returns_to_bottom() {
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "for i in $(seq 1 100); do echo line$i; done; sleep 0.3"]);
        let mut pane = PtyPane::spawn(cmd, 5, 20, || {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pane.with_screen(|s| s.contents().contains("line100")) {
            assert!(Instant::now() < deadline, "no output");
            std::thread::sleep(Duration::from_millis(20));
        }
        pane.scroll_chat(10);
        assert!(pane.with_screen(|s| s.scrollback()) > 0, "scrollback moved up");
        pane.reset_scroll();
        assert_eq!(pane.with_screen(|s| s.scrollback()), 0, "reset returns to the live bottom");
    }

    #[cfg(unix)]
    #[test]
    fn resize_updates_emulator_and_input_reaches_child() {
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "read line; printf \"got:%s\" \"$line\"; sleep 0.2"]);
        let mut pane = PtyPane::spawn(cmd, 5, 20, || {}).unwrap();
        pane.resize(8, 30);
        assert_eq!(pane.with_screen(|s| s.size()), (nz(8), nz(30)));
        pane.write(b"abc\r");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pane.with_screen(|s| s.contents().contains("got:abc")) {
            assert!(Instant::now() < deadline, "input not echoed");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
