//! CfgClient - bidirectional bridge to the guest's `cdj3k-cfgd` daemon.
//!
//! One Unix socket (`{sock_dir}/cfg.sock`) carries USB attach/detach commands,
//! `set`/`get` for the whitelisted virtio_snd sysfs parameters, and an
//! unsolicited 3-second latency push.
//!
//! Wire protocol (line-based, ASCII, '\n'-terminated):
//!
//!   host → guest
//!     usb attach              - invoke /usr/sbin/usb-external-attach.sh
//!     usb detach              - no-op (EP122 handles unmount)
//!     set <name> <value>      - write to a whitelisted sysfs param
//!     get <name>              - request a `param` response
//!     usbsave on|off          - arm/disarm USB Save in the mods
//!
//!   guest → host
//!     usb_state <0|1>         - emitted by the in-guest USB hooks
//!     param <name> <value>    - response to `get` or unsolicited push
//!     latency <g>,<h>,<t>     - pushed every 3s by cdj3k-cfgd
//!     usbsave_state <on|off> <flac|wav> - the state cfgd actually holds
//!     usbsave_saved <line>    - the last save the mods made, relayed by cfgd
//!
//! The `usbsave` pair is a single bit, which is all USB Save needs from the host:
//! the audio never crosses this socket, because the guest writes it straight to
//! the stick. `usbsave_state` comes back so the UI can show what took effect
//! rather than what was merely asked for -- cfgd is the one that knows whether the
//! flag file was written. `usbsave_saved` runs the other way: it carries what the
//! mods wrote, so the panel can show a save without the host mounting the live USB
//! image. cfgd still names a container on `usbsave_state`; nothing reads it.
//!
//! Connection is lazy and self-healing: the reader thread reconnects on EOF
//! / connect failure, the writer methods retry briefly while QEMU is still
//! bringing the port up.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Backoff between reader-thread reconnect attempts. Long enough not to thrash
/// QEMU while it's spinning up its virtio-serial port, short enough that the
/// "first frame" UX (latency label, USB state) lights up promptly.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);
/// Cold-path writer: max attempts to open a one-shot connection while the
/// reader thread is still bringing up the long-lived writer. `RETRY_DELAY ×
/// RETRIES = 2 s`, matching the read-side timeout below.
const COLD_WRITER_RETRIES: u32 = 20;
const COLD_WRITER_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Write timeout for the cold-path one-shot connection.
const COLD_WRITER_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
pub struct Latency {
    pub guest_ms: u32,
    pub host_ms: u32,
    pub total_ms: u32,
    pub at: Instant,
}

/// What the guest reports it is actually doing, as opposed to what was asked.
///
/// Only the armed bit now: the copy keeps the source's own container, so there is
/// no format to choose and nothing for the guest to report about one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbSaveState {
    pub armed: bool,
}

#[derive(Default)]
struct Shared {
    /// Latest USB mount state from the guest (`true` = mounted).
    usb_state: Option<bool>,
    /// Latest latency push.
    latency: Option<Latency>,
    /// Last `usbsave_state` the guest reported.
    usbsave: Option<UsbSaveState>,
    /// Last `usbsave_saved` summary: what the mods actually wrote to the stick,
    /// relayed by cfgd. The host cannot read the medium itself without mounting
    /// the live image, so this is how the panel shows a save.
    usbsave_last: Option<String>,
    /// Most recent `param <name> <value>` response per name.
    params: HashMap<String, String>,
    /// Writer half - `None` while disconnected.
    writer: Option<UnixStream>,
}

#[derive(Clone)]
pub struct CfgClient {
    sock_path: PathBuf,
    shared: Arc<Mutex<Shared>>,
}

impl CfgClient {
    /// Spawn the background reader thread and return a handle.
    /// `sock_dir` is e.g. `/tmp/cdj3k-emu/instance-1`; the socket is `{sock_dir}/cfg.sock`.
    pub fn new(sock_dir: &std::path::Path) -> Self {
        let sock_path = sock_dir.join("cfg.sock");
        let shared = Arc::new(Mutex::new(Shared::default()));

        let path = sock_path.clone();
        let shared_clone = Arc::clone(&shared);
        thread::Builder::new()
            .name("cfg-stream".into())
            .spawn(move || reader_loop(path, shared_clone))
            .expect("spawn cfg-stream thread");

        Self { sock_path, shared }
    }

    /// Most recently received guest USB mount state (`Some(true/false)`),
    /// or `None` if no state has been observed yet.
    pub fn usb_state(&self) -> Option<bool> {
        self.shared.lock().ok()?.usb_state
    }

    /// Take the next pending USB-state transition (consumes the cached value
    /// so callers can use this in a poll loop just like the old watcher).
    pub fn poll_usb_state(&self) -> Option<bool> {
        let mut s = self.shared.lock().ok()?;
        s.usb_state.take()
    }

    /// Latest latency triple. `None` until the first push lands.
    pub fn latency(&self) -> Option<Latency> {
        self.shared.lock().ok()?.latency
    }

    /// Most recent `param <name> <value>` response, if any.
    pub fn param(&self, name: &str) -> Option<String> {
        self.shared.lock().ok()?.params.get(name).cloned()
    }

    /// Send `usb attach\n`. Retries briefly while the port is still coming up.
    pub fn usb_attach(&self) -> std::io::Result<()> {
        self.send_line("usb attach\n")
    }

    /// Send `usb detach\n`. No-op on the guest side (EP122 handles unmount).
    pub fn usb_detach(&self) -> std::io::Result<()> {
        self.send_line("usb detach\n")
    }

    /// Write a sysfs param. The guest will respond with a `param` line that
    /// updates [`Self::param`] asynchronously.
    pub fn set_param(&self, name: &str, value: &str) -> std::io::Result<()> {
        if name.contains(' ') || value.contains('\n') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "param name/value must not contain spaces / newlines",
            ));
        }
        self.send_line(&format!("set {} {}\n", name, value))
    }

    /// Request a sysfs param. The response arrives asynchronously and updates
    /// [`Self::param`].
    pub fn get_param(&self, name: &str) -> std::io::Result<()> {
        if name.contains(' ') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "param name must not contain spaces",
            ));
        }
        self.send_line(&format!("get {}\n", name))
    }

    /// Arm or disarm USB Save.
    ///
    /// The guest replies with `usbsave_state`, so this returning `Ok` means the
    /// line was written and not that the mods took it: the mods may be off, in
    /// which case nothing in the guest reads the flag at all.
    pub fn usbsave(&self, armed: bool) -> std::io::Result<()> {
        if armed {
            self.send_line("usbsave on\n")
        } else {
            self.send_line("usbsave off\n")
        }
    }

    /// What the guest last reported about USB Save, if anything.
    pub fn usbsave_state(&self) -> Option<UsbSaveState> {
        self.shared.lock().ok()?.usbsave
    }

    /// The last save the mods reported, as cfgd relayed it: the fields written
    /// into the file, `key="value"` pairs. `None` until a save has happened in
    /// this boot.
    pub fn usbsave_last(&self) -> Option<String> {
        self.shared.lock().ok()?.usbsave_last.clone()
    }

    fn send_line(&self, line: &str) -> std::io::Result<()> {
        // Fast path: writer already connected by the reader thread.
        if let Ok(mut s) = self.shared.lock() {
            if let Some(ref mut w) = s.writer {
                if w.write_all(line.as_bytes()).is_ok() && w.flush().is_ok() {
                    return Ok(());
                }
                // Write failed - drop and let the reader thread reconnect.
                s.writer = None;
            }
        }
        // Cold path: open a one-shot writer connection. We don't hold this
        // open because the reader thread owns the long-lived stream.
        let mut last_err: Option<std::io::Error> = None;
        for _ in 0..COLD_WRITER_RETRIES {
            match UnixStream::connect(&self.sock_path) {
                Ok(mut s) => {
                    s.set_write_timeout(Some(COLD_WRITER_WRITE_TIMEOUT))?;
                    s.write_all(line.as_bytes())?;
                    s.flush()?;
                    return Ok(());
                }
                Err(e) => {
                    last_err = Some(e);
                    thread::sleep(COLD_WRITER_RETRY_DELAY);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| std::io::Error::other("cfg socket unavailable")))
    }
}

fn reader_loop(sock_path: PathBuf, shared: Arc<Mutex<Shared>>) {
    loop {
        let stream = match UnixStream::connect(&sock_path) {
            Ok(s) => s,
            Err(_) => {
                thread::sleep(RECONNECT_DELAY);
                continue;
            }
        };

        // Stash a clone for writers.
        if let Ok(write_clone) = stream.try_clone() {
            if let Ok(mut s) = shared.lock() {
                s.writer = Some(write_clone);
            }
        }

        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            handle_line(&line, &shared);
        }

        // Disconnected - drop writer and try again.
        if let Ok(mut s) = shared.lock() {
            s.writer = None;
        }
        thread::sleep(RECONNECT_DELAY);
    }
}

fn handle_line(line: &str, shared: &Arc<Mutex<Shared>>) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }

    if let Some(rest) = line.strip_prefix("usb_state ") {
        let mounted = rest.trim() == "1";
        if let Ok(mut s) = shared.lock() {
            s.usb_state = Some(mounted);
        }
        return;
    }

    if let Some(rest) = line.strip_prefix("latency ") {
        let mut parts = rest.split(',');
        let g = parts.next().and_then(|x| x.trim().parse().ok());
        let h = parts.next().and_then(|x| x.trim().parse().ok());
        let t = parts.next().and_then(|x| x.trim().parse().ok());
        if let (Some(g), Some(h), Some(t)) = (g, h, t) {
            if let Ok(mut s) = shared.lock() {
                s.latency = Some(Latency {
                    guest_ms: g,
                    host_ms: h,
                    total_ms: t,
                    at: Instant::now(),
                });
            }
        }
        return;
    }

    if let Some(rest) = line.strip_prefix("usbsave_state ") {
        let armed = match rest.split_whitespace().next() {
            Some("on") => true,
            Some("off") => false,
            _ => return,
        };
        // Anything after the armed bit is ignored: cfgd still sends the container
        // name it holds, and may grow more fields besides.
        if let Ok(mut s) = shared.lock() {
            s.usbsave = Some(UsbSaveState { armed });
        }
        return;
    }

    if let Some(rest) = line.strip_prefix("usbsave_saved ") {
        // The whole remainder is the summary, spaces and all -- values are quoted
        // on the wire precisely so it can be shown as it stands.
        let text = rest.trim();
        if !text.is_empty() {
            if let Ok(mut s) = shared.lock() {
                s.usbsave_last = Some(text.to_string());
            }
        }
        return;
    }

    if let Some(rest) = line.strip_prefix("param ") {
        if let Some(space) = rest.find(' ') {
            let (name, value) = rest.split_at(space);
            let value = value.trim_start();
            if let Ok(mut s) = shared.lock() {
                s.params.insert(name.to_string(), value.to_string());
            }
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(lines: &[&str]) -> Shared {
        let shared = Arc::new(Mutex::new(Shared::default()));
        for line in lines {
            handle_line(line, &shared);
        }
        let out = shared.lock().unwrap();
        Shared {
            usb_state: out.usb_state,
            latency: out.latency,
            usbsave: out.usbsave,
            usbsave_last: out.usbsave_last.clone(),
            params: out.params.clone(),
            writer: None,
        }
    }

    #[test]
    fn usbsave_state_reads_the_armed_bit() {
        assert_eq!(
            feed(&["usbsave_state on flac"]).usbsave,
            Some(UsbSaveState { armed: true })
        );
        assert_eq!(
            feed(&["usbsave_state off"]).usbsave,
            Some(UsbSaveState { armed: false })
        );
    }

    #[test]
    fn usbsave_state_survives_trailing_detail() {
        // cfgd still names the container it holds, and may grow more fields; the
        // armed bit is the part the panel acts on and must survive either way.
        assert_eq!(
            feed(&["usbsave_state on wav written=3"]).usbsave,
            Some(UsbSaveState { armed: true })
        );
    }

    #[test]
    fn a_malformed_usbsave_state_is_ignored_rather_than_guessed() {
        assert_eq!(feed(&["usbsave_state maybe flac"]).usbsave, None);
        assert_eq!(feed(&["usbsave_state"]).usbsave, None);
    }

    #[test]
    fn usbsave_saved_keeps_the_whole_summary() {
        // Quoted values with spaces inside must arrive intact -- the panel shows
        // the line as it stands.
        let got = feed(&["usbsave_saved file=\"Cold Nights.m4a\" artist=\"Bluff\""]);
        assert_eq!(
            got.usbsave_last.as_deref(),
            Some("file=\"Cold Nights.m4a\" artist=\"Bluff\"")
        );
        // An empty remainder is not a save.
        assert_eq!(feed(&["usbsave_saved "]).usbsave_last, None);
        assert_eq!(feed(&["usbsave_saved"]).usbsave_last, None);
    }

    #[test]
    fn the_other_lines_still_parse() {
        let got = feed(&["usb_state 1", "latency 5,6,11", "param foo bar baz"]);
        assert_eq!(got.usb_state, Some(true));
        assert_eq!(got.latency.map(|l| l.total_ms), Some(11));
        assert_eq!(got.params.get("foo").map(String::as_str), Some("bar baz"));
    }
}
