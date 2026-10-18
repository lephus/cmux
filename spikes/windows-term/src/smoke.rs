//! `--smoke`: headless check of the same Session the window uses. Starts an
//! interactive PowerShell on ConPTY, types a command through the same input
//! path as the window (committed text + libghostty-vt-encoded Enter), waits
//! for the computed result in the libghostty-vt grid, resizes, and exits.
//! Exit code 0 = pass. No window is opened.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};

use crate::session::{Session, Spawned, default_shell, spawn_reader};

const TIMEOUT: Duration = Duration::from_secs(45);

struct Pump {
    rx: mpsc::Receiver<Vec<u8>>,
    closed: bool,
}

impl Pump {
    /// Feed output until `done(lines)` holds or the deadline passes.
    fn until(
        &mut self,
        session: &mut Session,
        what: &str,
        mut done: impl FnMut(&[String]) -> bool,
    ) -> Result<Vec<String>> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let lines = session.text_lines()?;
            if done(&lines) {
                return Ok(lines);
            }
            if Instant::now() > deadline {
                dump(&lines);
                bail!("timed out waiting for {what}");
            }
            if self.closed {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(bytes) => {
                    session.feed(&bytes);
                    while let Ok(more) = self.rx.try_recv() {
                        session.feed(&more);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => self.closed = true,
            }
        }
    }
}

fn dump(lines: &[String]) {
    eprintln!("---- grid ----");
    for line in lines.iter().filter(|l| !l.is_empty()) {
        eprintln!("{line}");
    }
    eprintln!("--------------");
}

fn contains(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|line| line.contains(needle))
}

pub fn run() -> Result<()> {
    let started = Instant::now();
    let (program, args) = default_shell();
    // `-NoProfile` keeps the runner's profile out of the result.
    let mut full_args = args.clone();
    if program.to_ascii_lowercase().contains("powershell") || program.contains("pwsh") {
        full_args.push("-NoProfile");
    }
    let Spawned {
        mut session,
        reader,
    } = Session::spawn(&program, &full_args, 100, 30)?;
    let (tx, rx) = mpsc::channel();
    spawn_reader(reader, move |bytes| tx.send(bytes).is_ok());
    let mut pump = Pump { rx, closed: false };

    // 1. Key encoding through libghostty-vt (what the window sends).
    ensure!(
        session.encode_chord("enter").as_deref() == Some(b"\r".as_slice()),
        "enter != CR"
    );
    ensure!(
        session.encode_chord("ctrl+c").as_deref() == Some(b"\x03".as_slice()),
        "ctrl+c"
    );
    ensure!(
        session.encode_chord("backspace").as_deref() == Some(b"\x7f".as_slice()),
        "bs"
    );
    ensure!(
        session.encode_chord("up").is_some(),
        "up arrow not encodable"
    );
    println!("smoke: key encoding ok");

    // 2. Shell prompt reaches the grid.
    pump.until(&mut session, "the PowerShell prompt", |lines| {
        lines
            .iter()
            .any(|l| l.trim_end().ends_with('>') || l.contains("PS "))
    })?;
    println!("smoke: prompt after {:?}", started.elapsed());

    // 3. Type a command the way the window does and read the result back.
    //    The marker is computed by the shell, so finding it proves the
    //    command ran (the typed text alone never contains it).
    session.send_text("Write-Output ('shellx-ok-' + (6*7))");
    ensure!(session.send_chord("enter"), "enter not sent");
    pump.until(&mut session, "shellx-ok-42", |lines| {
        contains(lines, "shellx-ok-42")
    })?;
    println!(
        "smoke: echo ok (shellx-ok-42 in grid) after {:?}",
        started.elapsed()
    );

    // 4. Vietnamese committed text round-trips through ConPTY into the grid
    //    (what an IME commit sends). Informational: a miss is a warning.
    session.send_text("Write-Output 'ti\u{1ebf}ng Vi\u{1ec7}t'");
    session.send_chord("enter");
    match pump.until(&mut session, "Vietnamese text", |lines| {
        lines
            .iter()
            .filter(|l| l.contains("ti\u{1ebf}ng Vi\u{1ec7}t"))
            .count()
            >= 2
    }) {
        Ok(_) => println!("smoke: unicode ok (tiếng Việt echoed by the shell)"),
        Err(e) => println!("smoke: WARN unicode round-trip: {e:#}"),
    }

    // 5. Resize VT grid + pseudoconsole, then confirm the shell sees it.
    session.resize(120, 40, 8, 16)?;
    ensure!(
        session.vt_cols_rows() == (120, 40),
        "VT grid did not resize"
    );
    session.send_text("Write-Output ('cols-' + $Host.UI.RawUI.WindowSize.Width)");
    session.send_chord("enter");
    match pump.until(&mut session, "cols-120", |lines| {
        contains(lines, "cols-120")
    }) {
        Ok(_) => println!("smoke: resize ok (shell reports 120 columns)"),
        Err(e) => println!("smoke: WARN resize not seen by shell: {e:#}"),
    }

    let lines = session.text_lines()?;
    dump(&lines);
    session.send_text("exit");
    session.send_chord("enter");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !session.child_exited() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("smoke: PASS in {:?}", started.elapsed());
    Ok(())
}
