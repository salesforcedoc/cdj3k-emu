//! Passive Pro DJ Link status capture from QEMU's network boundary.
//!
//! QEMU writes a pcap byte stream into a FIFO through `filter-dump`. Reading
//! there is deliberately preferable to binding UDP 50002: it cannot collide
//! with rekordbox or another emulator process, and it sees guest broadcasts
//! even when QEMU is using its user-mode NAT backend.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cdj3k_emu_platform::menu_state::ProlinkTrackStatus;

const PROLINK_MAGIC: &[u8; 10] = b"Qspt1WmJOL";
const PCAP_GLOBAL_HEADER_LEN: usize = 24;
const PCAP_PACKET_HEADER_LEN: usize = 16;
const MAX_CAPTURED_PACKET: usize = 65_536;

pub struct ProlinkMonitor {
    latest: Arc<Mutex<Option<ProlinkTrackStatus>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    fifo_path: PathBuf,
}

impl ProlinkMonitor {
    pub fn start(path: &Path, expected_player: u8, expected_mac: [u8; 6]) -> io::Result<Self> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Pro DJ Link FIFO path contains NUL",
            )
        })?;
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        // RDWR keeps the FIFO open across QEMU restarts, while O_NONBLOCK lets
        // the monitor observe its stop flag when no network packet is flowing.
        let fifo = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let latest_thread = Arc::clone(&latest);
        let stop_thread = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name(format!("prolink-status-{expected_player}"))
            .spawn(move || {
                capture_loop(
                    fifo,
                    expected_player,
                    expected_mac,
                    latest_thread,
                    stop_thread,
                )
            })?;

        Ok(Self {
            latest,
            stop,
            thread: Some(thread),
            fifo_path: path.to_path_buf(),
        })
    }

    pub fn latest(&self) -> Option<ProlinkTrackStatus> {
        self.latest.lock().ok()?.clone()
    }
}

impl Drop for ProlinkMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.fifo_path);
    }
}

#[derive(Clone, Copy)]
enum PcapEndian {
    Little,
    Big,
}

fn capture_loop(
    mut fifo: File,
    expected_player: u8,
    expected_mac: [u8; 6],
    latest: Arc<Mutex<Option<ProlinkTrackStatus>>>,
    stop: Arc<AtomicBool>,
) {
    let mut pending = Vec::<u8>::with_capacity(4096);
    let mut scratch = [0u8; 4096];
    let mut endian = None;

    while !stop.load(Ordering::Acquire) {
        match fifo.read(&mut scratch) {
            Ok(0) => std::thread::sleep(Duration::from_millis(10)),
            Ok(n) => {
                pending.extend_from_slice(&scratch[..n]);
                drain_pcap(
                    &mut pending,
                    &mut endian,
                    expected_player,
                    expected_mac,
                    &latest,
                );
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                eprintln!("cdj3k-emu: Pro DJ Link capture stopped: {error}");
                break;
            }
        }
    }
}

fn drain_pcap(
    pending: &mut Vec<u8>,
    endian: &mut Option<PcapEndian>,
    expected_player: u8,
    expected_mac: [u8; 6],
    latest: &Arc<Mutex<Option<ProlinkTrackStatus>>>,
) {
    if endian.is_none() {
        if pending.len() < PCAP_GLOBAL_HEADER_LEN {
            return;
        }
        *endian = match pending[..4] {
            [0xd4, 0xc3, 0xb2, 0xa1] | [0x4d, 0x3c, 0xb2, 0xa1] => Some(PcapEndian::Little),
            [0xa1, 0xb2, 0xc3, 0xd4] | [0xa1, 0xb2, 0x3c, 0x4d] => Some(PcapEndian::Big),
            _ => {
                // A malformed header should not allow an unbounded buffer.
                pending.clear();
                None
            }
        };
        if endian.is_some() {
            pending.drain(..PCAP_GLOBAL_HEADER_LEN);
        }
    }

    let Some(byte_order) = *endian else { return };
    loop {
        if pending.len() < PCAP_PACKET_HEADER_LEN {
            return;
        }
        let caplen = read_u32(&pending[8..12], byte_order) as usize;
        if caplen > MAX_CAPTURED_PACKET {
            pending.clear();
            *endian = None;
            return;
        }
        let record_len = PCAP_PACKET_HEADER_LEN + caplen;
        if pending.len() < record_len {
            return;
        }
        if let Some(status) = parse_frame(
            &pending[PCAP_PACKET_HEADER_LEN..record_len],
            expected_player,
            expected_mac,
        ) {
            if let Ok(mut value) = latest.lock() {
                *value = Some(status);
            }
        }
        pending.drain(..record_len);
    }
}

fn parse_frame(
    frame: &[u8],
    expected_player: u8,
    expected_mac: [u8; 6],
) -> Option<ProlinkTrackStatus> {
    if frame.len() < 14 || frame[6..12] != expected_mac {
        return None;
    }
    let mut network = 14;
    let mut ether_type = u16::from_be_bytes([frame[12], frame[13]]);
    while ether_type == 0x8100 || ether_type == 0x88a8 {
        if frame.len() < network + 4 {
            return None;
        }
        ether_type = u16::from_be_bytes([frame[network + 2], frame[network + 3]]);
        network += 4;
    }
    if ether_type != 0x0800 || frame.len() < network + 20 {
        return None;
    }
    let ip = &frame[network..];
    if ip[0] >> 4 != 4 || ip[9] != 17 {
        return None;
    }
    let ip_header_len = ((ip[0] & 0x0f) as usize) * 4;
    if ip_header_len < 20 || ip.len() < ip_header_len + 8 {
        return None;
    }
    let udp = &ip[ip_header_len..];
    let source_port = u16::from_be_bytes([udp[0], udp[1]]);
    let destination_port = u16::from_be_bytes([udp[2], udp[3]]);
    if source_port != 50002 && destination_port != 50002 {
        return None;
    }
    let payload = &udp[8..];
    if payload.len() <= 0xa6
        || &payload[..10] != PROLINK_MAGIC
        || payload[10] != 0x0a
        || payload[0x20] != 0x03
        || payload[0x21] != expected_player
        || payload[0x24] != expected_player
    {
        return None;
    }

    Some(ProlinkTrackStatus {
        received_at: Instant::now(),
        source_ip: format!("{}.{}.{}.{}", ip[12], ip[13], ip[14], ip[15]),
        device_name: ascii_field(&payload[0x0b..0x1f]),
        player_number: payload[0x21],
        source_player: payload[0x28],
        source_slot: payload[0x29],
        track_type: payload[0x2a],
        track_id: u32::from_be_bytes(payload[0x2c..0x30].try_into().ok()?),
        track_number: u16::from_be_bytes(payload[0x32..0x34].try_into().ok()?),
        play_state: payload[0x7b],
        firmware: ascii_field(&payload[0x7c..0x80]),
        state_flags: payload[0x89],
        pitch_raw: u32::from_be_bytes(payload[0x8c..0x90].try_into().ok()?),
        track_bpm_x100: u16::from_be_bytes(payload[0x92..0x94].try_into().ok()?),
        beat_number: u32::from_be_bytes(payload[0xa0..0xa4].try_into().ok()?),
        beat_in_bar: payload[0xa6],
    })
}

fn ascii_field(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn read_u32(bytes: &[u8], endian: PcapEndian) -> u32 {
    let bytes: [u8; 4] = bytes.try_into().expect("four-byte pcap field");
    match endian {
        PcapEndian::Little => u32::from_le_bytes(bytes),
        PcapEndian::Big => u32::from_be_bytes(bytes),
    }
}

pub fn parse_mac(value: &str) -> Option<[u8; 6]> {
    let mut result = [0u8; 6];
    let mut parts = value.split(':');
    for byte in &mut result {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_own_cdj_status_frame() {
        let mac = [0x0a, 0, 0, 0, 0, 2];
        let mut payload = vec![0u8; 0x124];
        payload[..10].copy_from_slice(PROLINK_MAGIC);
        payload[10] = 0x0a;
        payload[0x0b..0x13].copy_from_slice(b"CDJ-3000");
        payload[0x20] = 0x03;
        payload[0x21] = 2;
        payload[0x24] = 2;
        payload[0x28] = 1;
        payload[0x29] = 3;
        payload[0x2a] = 1;
        payload[0x2c..0x30].copy_from_slice(&42u32.to_be_bytes());
        payload[0x32..0x34].copy_from_slice(&7u16.to_be_bytes());
        payload[0x7b] = 3;
        payload[0x7c..0x80].copy_from_slice(b"3.14");
        payload[0x89] = 0xe0;
        payload[0x8c..0x90].copy_from_slice(&0x0010_0000u32.to_be_bytes());
        payload[0x92..0x94].copy_from_slice(&12_800u16.to_be_bytes());
        payload[0xa0..0xa4].copy_from_slice(&129u32.to_be_bytes());
        payload[0xa6] = 1;

        let mut frame = vec![0u8; 14 + 20 + 8 + payload.len()];
        frame[6..12].copy_from_slice(&mac);
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        let ip = 14;
        frame[ip] = 0x45;
        frame[ip + 9] = 17;
        frame[ip + 12..ip + 16].copy_from_slice(&[169, 254, 1, 2]);
        let udp = ip + 20;
        frame[udp..udp + 2].copy_from_slice(&50002u16.to_be_bytes());
        frame[udp + 2..udp + 4].copy_from_slice(&50002u16.to_be_bytes());
        frame[udp + 8..].copy_from_slice(&payload);

        let status = parse_frame(&frame, 2, mac).expect("status frame");
        assert_eq!(status.device_name, "CDJ-3000");
        assert_eq!(status.track_id, 42);
        assert_eq!(status.track_number, 7);
        assert_eq!(status.track_bpm_x100, 12_800);
        assert_eq!(status.beat_number, 129);
        assert_eq!(status.source_ip, "169.254.1.2");
    }

    #[test]
    fn rejects_another_ethernet_source() {
        let frame = vec![0u8; 256];
        assert!(parse_frame(&frame, 1, [1, 2, 3, 4, 5, 6]).is_none());
    }

    #[test]
    fn parses_mac_address() {
        assert_eq!(parse_mac("0a:00:00:00:00:04"), Some([10, 0, 0, 0, 0, 4]));
        assert_eq!(parse_mac("not-a-mac"), None);
    }
}
