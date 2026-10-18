//! W0 spike (ShellX branch `shellx/w0-bridge`, never merged): a "virtual PTY"
//! whose far end is a libghostty surface owned by the macOS app.
//!
//! The Mux keeps its usual `PtyRuntime::Local` surface and `ghostty-vt`
//! mirror; only the PTY is replaced. Output comes from the app's PTY tee,
//! input goes back into the app's real PTY, and the real grid size is pushed
//! by the app (the Mac owns the PTY, so it is the sizing host).
//!
//! Frame: `[type u8][len u32 BE][payload]`, one Unix-socket connection per
//! terminal. Types (see `Sources/Mobile/ShellXBridgeSpike.swift`):
//! daemon -> app `H` hello JSON, `I` input bytes, `R` resize wish (cols,rows
//! u16 BE), `K` kill; app -> daemon `A` ack JSON, `O` output bytes, `S` real
//! grid (cols,rows u16 BE), `E` exit.
//!
//! Enabled when the daemon runs with `CMUX_SHELLX_BRIDGE_SOCKET=<path>`: every
//! default-shell terminal it creates becomes a Mac terminal (the first one
//! projects the Mac's focused terminal, later ones open a new Mac tab).

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use cmux_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize, SpawnedPty};

pub(crate) const BRIDGE_SOCKET_ENV: &str = "CMUX_SHELLX_BRIDGE_SOCKET";

const HELLO: u8 = b'H';
const INPUT: u8 = b'I';
const RESIZE: u8 = b'R';
const KILL: u8 = b'K';
const ACK: u8 = b'A';
const OUTPUT: u8 = b'O';
const SIZE: u8 = b'S';
const EXIT: u8 = b'E';

pub(crate) struct VirtualPtySpec {
    socket: PathBuf,
}

/// Default-shell terminals become virtual when the bridge socket is set.
/// Terminals with an explicit command still run locally in the daemon.
pub(crate) fn spec_for(command: Option<&Vec<String>>) -> Option<VirtualPtySpec> {
    if command.is_some_and(|argv| !argv.is_empty()) {
        return None;
    }
    let socket = std::env::var_os(BRIDGE_SOCKET_ENV)?;
    if socket.is_empty() {
        return None;
    }
    Some(VirtualPtySpec { socket: socket.into() })
}

type HostSizeHandler = Box<dyn Fn(u16, u16) + Send + Sync>;

struct Shared {
    writer: Mutex<UnixStream>,
    size: Mutex<PtySize>,
    /// Last real grid the app reported. A Mux resize to exactly this size is
    /// the echo of our own host-size apply and is not sent back.
    host_size: Mutex<Option<(u16, u16)>>,
    on_host_size: Mutex<Option<HostSizeHandler>>,
    exited: Mutex<bool>,
    exited_cv: Condvar,
    ack: serde_json::Value,
}

impl Shared {
    fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 5);
        frame.push(kind);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        self.writer.lock().unwrap().write_all(&frame)
    }

    fn mark_exited(&self) {
        *self.exited.lock().unwrap() = true;
        self.exited_cv.notify_all();
    }
}

/// Handle surface.rs keeps to route app-reported sizes into the Mux surface.
pub(crate) struct VirtualPtyHandle {
    shared: Arc<Shared>,
}

impl VirtualPtyHandle {
    pub(crate) fn initial_size(&self) -> Option<(u16, u16)> {
        *self.shared.host_size.lock().unwrap()
    }

    pub(crate) fn ack(&self) -> &serde_json::Value {
        &self.shared.ack
    }

    /// Installs the host-size callback (the Mac's real PTY grid changed).
    pub(crate) fn set_host_size_handler(&self, handler: HostSizeHandler) {
        *self.shared.on_host_size.lock().unwrap() = Some(handler);
    }
}

fn read_frame(stream: &mut UnixStream) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > 16 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bridge frame too large"));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

fn parse_size(payload: &[u8]) -> Option<(u16, u16)> {
    (payload.len() == 4).then(|| {
        (u16::from_be_bytes([payload[0], payload[1]]), u16::from_be_bytes([payload[2], payload[3]]))
    })
}

/// Connects to the app, binds (or creates) one Mac terminal and returns a
/// master/child pair the surface code uses exactly like a real PTY.
pub(crate) fn open(
    spec: &VirtualPtySpec,
    size: PtySize,
    cwd: Option<&str>,
) -> anyhow::Result<(SpawnedPty, VirtualPtyHandle)> {
    let mut stream = UnixStream::connect(&spec.socket).map_err(|error| {
        anyhow::anyhow!("shellx bridge: connect {}: {error}", spec.socket.display())
    })?;
    let hello = serde_json::json!({ "cols": size.cols, "rows": size.rows, "cwd": cwd });
    {
        let payload = serde_json::to_vec(&hello)?;
        let mut frame = vec![HELLO];
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        stream.write_all(&frame)?;
    }
    // The app answers with A (bound surface), then S (its real grid), then
    // the screen snapshot as O frames.
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (kind, payload) = read_frame(&mut stream)?;
    if kind != ACK {
        anyhow::bail!("shellx bridge: app refused the terminal (frame {:?})", kind as char);
    }
    let ack: serde_json::Value = serde_json::from_slice(&payload).unwrap_or_default();
    let mut host_size = None;
    let mut early_output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    while host_size.is_none() && Instant::now() < deadline {
        match read_frame(&mut stream) {
            Ok((SIZE, payload)) => host_size = parse_size(&payload),
            Ok((OUTPUT, payload)) => early_output.extend_from_slice(&payload),
            Ok(_) => {}
            Err(error)
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(error) => return Err(error.into()),
        }
    }
    stream.set_read_timeout(None)?;

    let (reader, mut pipe_writer) = UnixStream::pair()?;
    let initial = host_size
        .map(|(cols, rows)| PtySize { cols, rows, ..size })
        .unwrap_or(size);
    let shared = Arc::new(Shared {
        writer: Mutex::new(stream.try_clone()?),
        size: Mutex::new(initial),
        host_size: Mutex::new(host_size),
        on_host_size: Mutex::new(None),
        exited: Mutex::new(false),
        exited_cv: Condvar::new(),
        ack,
    });
    if !early_output.is_empty() {
        pipe_writer.write_all(&early_output)?;
    }

    std::thread::Builder::new().name("shellx-bridge-demux".into()).spawn({
        let shared = shared.clone();
        let mut stream = stream;
        move || {
            loop {
                match read_frame(&mut stream) {
                    Ok((OUTPUT, payload)) => {
                        if pipe_writer.write_all(&payload).is_err() {
                            break;
                        }
                    }
                    Ok((SIZE, payload)) => {
                        let Some((cols, rows)) = parse_size(&payload) else { continue };
                        *shared.host_size.lock().unwrap() = Some((cols, rows));
                        if let Some(handler) = shared.on_host_size.lock().unwrap().as_ref() {
                            handler(cols, rows);
                        }
                    }
                    Ok((EXIT, _)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            drop(pipe_writer);
            shared.mark_exited();
        }
    })?;

    let master = VirtualMaster { shared: shared.clone(), reader };
    let child = VirtualChild { shared: shared.clone() };
    Ok((
        SpawnedPty { master: Box::new(master), child: Box::new(child) },
        VirtualPtyHandle { shared },
    ))
}

struct VirtualMaster {
    shared: Arc<Shared>,
    reader: UnixStream,
}

impl MasterPty for VirtualMaster {
    fn resize(&self, size: PtySize) -> Result<(), anyhow::Error> {
        *self.shared.size.lock().unwrap() = size;
        if *self.shared.host_size.lock().unwrap() == Some((size.cols, size.rows)) {
            return Ok(());
        }
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&size.cols.to_be_bytes());
        payload.extend_from_slice(&size.rows.to_be_bytes());
        self.shared.send(RESIZE, &payload)?;
        Ok(())
    }

    fn get_size(&self) -> Result<PtySize, anyhow::Error> {
        Ok(*self.shared.size.lock().unwrap())
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, anyhow::Error> {
        Ok(Box::new(self.reader.try_clone()?))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, anyhow::Error> {
        Ok(Box::new(VirtualWriter { shared: self.shared.clone() }))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        None
    }

    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    fn tty_name(&self) -> Option<PathBuf> {
        None
    }
}

struct VirtualWriter {
    shared: Arc<Shared>,
}

impl Write for VirtualWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.shared.send(INPUT, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct VirtualChild {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for VirtualChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualChild").finish()
    }
}

impl ChildKiller for VirtualChild {
    fn kill(&mut self) -> io::Result<()> {
        let _ = self.shared.send(KILL, &[]);
        let _ = self.shared.writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

impl Child for VirtualChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok((*self.shared.exited.lock().unwrap()).then(|| ExitStatus::with_exit_code(0)))
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        let mut exited = self.shared.exited.lock().unwrap();
        while !*exited {
            exited = self.shared.exited_cv.wait(exited).unwrap();
        }
        Ok(ExitStatus::with_exit_code(0))
    }

    fn process_id(&self) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// A fake app: acks, reports a 100x30 grid, sends recorded output, then
    /// echoes every input frame back as output.
    #[test]
    fn virtual_pty_round_trips_output_input_and_sizes() {
        let dir = std::env::temp_dir().join(format!("vpty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("b.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let app = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let (kind, _hello) = read_frame(&mut conn).unwrap();
            assert_eq!(kind, HELLO);
            conn.write_all(&frame(ACK, br#"{"surface_id":"x","mode":"attach"}"#)).unwrap();
            conn.write_all(&frame(SIZE, &[0, 100, 0, 30])).unwrap();
            conn.write_all(&frame(OUTPUT, b"\x1b[31mred\x1b[0m\r\n")).unwrap();
            let mut seen_resize = None;
            loop {
                match read_frame(&mut conn) {
                    Ok((INPUT, bytes)) => conn.write_all(&frame(OUTPUT, &bytes)).unwrap(),
                    Ok((RESIZE, size)) => seen_resize = parse_size(&size),
                    Ok((KILL, _)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            seen_resize
        });
        let spec = VirtualPtySpec { socket: path.clone() };
        let size = PtySize { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 };
        let (spawned, handle) = open(&spec, size, None).unwrap();
        assert_eq!(handle.initial_size(), Some((100, 30)));
        assert_eq!(handle.ack()["mode"], "attach");
        let mut reader = spawned.master.try_clone_reader().unwrap();
        let mut writer = spawned.master.take_writer().unwrap();
        let mut got = vec![0u8; 64];
        let n = reader.read(&mut got).unwrap();
        assert_eq!(&got[..n], b"\x1b[31mred\x1b[0m\r\n");
        writer.write_all(b"ls\r").unwrap();
        let n = reader.read(&mut got).unwrap();
        assert_eq!(&got[..n], b"ls\r");
        // An echo of the host size is not sent back; a real wish is.
        spawned.master.resize(PtySize { cols: 100, rows: 30, ..size }).unwrap();
        spawned.master.resize(PtySize { cols: 60, rows: 20, ..size }).unwrap();
        let mut child = spawned.child;
        child.kill().unwrap();
        assert_eq!(app.join().unwrap(), Some((60, 20)));
        assert!(child.wait().unwrap().success());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
