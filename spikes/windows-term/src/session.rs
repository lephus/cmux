//! One terminal session: a ConPTY (via cmux-pty / portable-pty) running a
//! shell, fed into a libghostty-vt `Terminal`. No UI here, so the headless
//! `--smoke` mode and the GPUI window drive exactly the same code.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use cmux_pty::{Child, ChildKiller as _, MasterPty, PtyCommand, PtySize};
use ghostty_vt::{Callbacks, KeyEncoder, RenderFrame, RenderState, Rgb, Terminal};

/// ShellX palette (soft, primary #60a5fa for the cursor).
pub const DEFAULT_BG: Rgb = Rgb {
    r: 0x11,
    g: 0x13,
    b: 0x18,
};
pub const DEFAULT_FG: Rgb = Rgb {
    r: 0xd4,
    g: 0xd7,
    b: 0xdd,
};
pub const CURSOR: Rgb = Rgb {
    r: 0x60,
    g: 0xa5,
    b: 0xfa,
};

const SCROLLBACK_BYTES: usize = 10 * 1024 * 1024;

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub struct Session {
    terminal: Terminal,
    render: RenderState,
    encoder: KeyEncoder,
    writer: SharedWriter,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    cols: u16,
    rows: u16,
}

pub struct Spawned {
    pub session: Session,
    pub reader: Box<dyn Read + Send>,
}

impl Session {
    pub fn spawn(program: &str, args: &[&str], cols: u16, rows: u16) -> Result<Spawned> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = cmux_pty::open(size).context("failed to open the pseudoconsole")?;
        let mut command = PtyCommand::new(program);
        command.args(args.iter().copied());
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            command.cwd(std::path::PathBuf::from(home));
        }
        let spawned = pair
            .spawn(command)
            .with_context(|| format!("failed to start {program}"))?;
        let reader = spawned
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer: SharedWriter = Arc::new(Mutex::new(
            spawned
                .master
                .take_writer()
                .context("failed to take PTY writer")?,
        ));

        // Query replies (DSR, DA, ...) go straight back to the PTY. ConPTY
        // asks for the cursor position at startup and waits for the answer.
        let reply = writer.clone();
        let callbacks = Callbacks {
            on_pty_write: Some(Box::new(move |bytes: &[u8]| {
                if let Ok(mut writer) = reply.lock() {
                    let _ = writer.write_all(bytes);
                    let _ = writer.flush();
                }
            })),
            ..Default::default()
        };
        let mut terminal = Terminal::new(cols, rows, SCROLLBACK_BYTES, callbacks)
            .context("failed to create libghostty-vt terminal")?;
        terminal.set_default_colors(Some(DEFAULT_FG), Some(DEFAULT_BG), Some(CURSOR));
        let render = RenderState::new().context("failed to create render state")?;
        let encoder = KeyEncoder::new().context("failed to create key encoder")?;

        Ok(Spawned {
            session: Session {
                terminal,
                render,
                encoder,
                writer,
                master: spawned.master,
                child: spawned.child,
                cols,
                rows,
            },
            reader,
        })
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Feed PTY output into the VT parser.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    fn write_pty(&self, bytes: &[u8]) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    /// Committed text (typed characters, IME results, paste).
    pub fn send_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.terminal.scroll_to_bottom();
        self.write_pty(text.as_bytes());
    }

    /// Encode a key chord ("enter", "ctrl+c", "shift+tab", "alt+x", "up")
    /// with libghostty-vt's key encoder, honoring the terminal's current
    /// keyboard modes, and write it. Returns false when the chord is not
    /// encodable (the caller then lets the platform deliver it as text).
    pub fn send_chord(&mut self, chord: &str) -> bool {
        let Some(bytes) = self.encode_chord(chord) else {
            return false;
        };
        self.terminal.scroll_to_bottom();
        self.write_pty(&bytes);
        true
    }

    pub fn encode_chord(&mut self, chord: &str) -> Option<Vec<u8>> {
        let input = ghostty_vt::key_input_from_chord(chord)?;
        self.encoder.sync_from_terminal(&self.terminal);
        let mut out = Vec::new();
        self.encoder.encode(&input, &mut out).ok()?;
        (!out.is_empty()).then_some(out)
    }

    /// Resize both the VT grid and the pseudoconsole.
    pub fn resize(&mut self, cols: u16, rows: u16, cell_w_px: u32, cell_h_px: u32) -> Result<()> {
        let (cols, rows) = (cols.max(2), rows.max(1));
        if (cols, rows) == (self.cols, self.rows) {
            return Ok(());
        }
        self.terminal
            .resize(cols, rows, cell_w_px, cell_h_px)
            .context("VT resize failed")?;
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: (u32::from(cols) * cell_w_px).min(u32::from(u16::MAX)) as u16,
                pixel_height: (u32::from(rows) * cell_h_px).min(u32::from(u16::MAX)) as u16,
            })
            .context("ConPTY resize failed")?;
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    pub fn scroll_lines(&mut self, delta: isize) {
        self.terminal.scroll_delta(delta);
    }

    pub fn vt_cols_rows(&self) -> (u16, u16) {
        (self.terminal.cols(), self.terminal.rows())
    }

    pub fn title(&self) -> Option<String> {
        self.terminal.title()
    }

    pub fn frame(&mut self) -> Result<RenderFrame> {
        self.render
            .update(&mut self.terminal)
            .context("render update failed")?;
        Ok(self.render.build_frame()?)
    }

    pub fn text_lines(&mut self) -> Result<Vec<String>> {
        self.render
            .update(&mut self.terminal)
            .context("render update failed")?;
        Ok(self.render.text_lines()?)
    }

    pub fn child_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Pump PTY output on a dedicated thread. `sink` returns false to stop.
pub fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    mut sink: impl FnMut(Vec<u8>) -> bool + Send + 'static,
) {
    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if !sink(buf[..n].to_vec()) {
                            break;
                        }
                    }
                }
            }
        })
        .expect("failed to spawn the PTY reader thread");
}

/// PowerShell on Windows (override with SHELLX_SPIKE_SHELL), sh elsewhere.
pub fn default_shell() -> (String, Vec<&'static str>) {
    if let Ok(shell) = std::env::var("SHELLX_SPIKE_SHELL")
        && !shell.trim().is_empty()
    {
        return (shell, Vec::new());
    }
    if cfg!(windows) {
        ("powershell.exe".to_string(), vec!["-NoLogo"])
    } else {
        ("/bin/sh".to_string(), Vec::new())
    }
}
