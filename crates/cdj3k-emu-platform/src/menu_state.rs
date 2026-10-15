//! Cross-platform UI/runtime state shared between the native macOS NSMenu
//! implementation, the egui UI, and the runtime worker thread.
//!
//! Previously a soup of ~28 atomic globals + 3 ad-hoc `Mutex<...>`s. Folded
//! into a single `Mutex<AppState>` so:
//!
//!   * lock acquisition order is trivial (only one lock).
//!   * the menu can snapshot the whole state with one `lock()` instead of 20+
//!     individual atomic loads.
//!   * adding new fields no longer means picking a fresh atomic type.
//!
//! The lock is never held across I/O. Hot loops (menu sync, runtime poll)
//! snapshot what they need, drop the guard, then perform work. Audio thread
//! and DRM-stream threads do NOT touch this state.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

/// A host keyboard action waiting to be delivered to QEMU's virtual USB
/// keyboard. Printable text is kept as Unicode here; the runtime drops
/// characters that cannot be represented by the guest's US keyboard layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestKeyboardInput {
    Text(char),
    Backspace,
    Delete,
    Enter,
    Escape,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

/// Bound pending keyboard work so an accidental multi-megabyte paste cannot
/// make the runtime worker spend seconds replaying stale input.
pub const MAX_GUEST_KEYBOARD_QUEUE: usize = 1024;

/// Maximum instance count (slots are 1..=MAX_INSTANCES).
pub const MAX_INSTANCES: u32 = 4;

/// Sentinel value for [`AppState::usb_phys_mounted_idx`] and
/// `usb_phys_toggle_idx` meaning "no physical disk".  `i32` because the
/// menu uses signed indices to make the "none" state representable
/// without an `Option`.
pub const NO_USB_MOUNTED: i32 = -1;

/// Sentinel value for [`AppState::selected_interface`] meaning "no network
/// interface picked" (QEMU runs with user-mode NAT, the menu's "None (NAT)"
/// entry).
pub const NET_SEL_NONE: u32 = u32::MAX;

/// Sentinel value for [`AppState::selected_interface`] meaning "vmnet host-only
/// network" (menu's "Host-only (link-local)" entry). All instances selecting this
/// attach to the same `socket_vmnet --vmnet-mode=host` daemon, sharing a
/// host-side `bridgeN` interface that vmnet.framework creates. Pure L2,
/// host-sniffable in Wireshark with no encapsulation. The daemon runs with
/// `--vmnet-network-identifier`, so the segment has no DHCP server: guests
/// self-assign 169.254/16 link-local via avahi-autoipd, like real gear.
pub const NET_SEL_VMNET_HOST: u32 = u32::MAX - 1;

/// Token written to `InstanceSettings::net_iface` to persist the vmnet-host
/// mode across launches. Not a valid BSD ifname, so it can never collide with
/// a real interface returned by `getifaddrs`.
pub const NET_IFACE_VMNET_HOST_TOKEN: &str = "__vmnet_host__";

/// User-facing audio latency preset. The selected preset is stored per player
/// and applied the next time that player's QEMU process starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioLatencyProfile {
    Safe,
    Balanced,
    LowLatency,
    Custom,
}

impl AudioLatencyProfile {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Safe => "Safe",
            Self::Balanced => "Balanced",
            Self::LowLatency => "Low Latency",
            Self::Custom => "Custom",
        }
    }
}

/// The concrete knobs used by QEMU and the guest virtio-sound driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioLatencyTuning {
    /// Silent stereo F32 frames queued before the CoreAudio reader starts.
    pub startup_prefill_frames: u32,
    /// Frames accumulated by the guest into one virtio-sound TX request.
    pub guest_transfer_frames: u32,
    /// CoreAudio device buffer request, in microseconds.
    pub coreaudio_buffer_us: u32,
}

/// Per-player latency selection plus the values retained for Custom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioLatencySettings {
    pub profile: AudioLatencyProfile,
    pub custom: AudioLatencyTuning,
}

impl Default for AudioLatencySettings {
    fn default() -> Self {
        Self {
            // Matches the proven settings used before profiles were exposed.
            profile: AudioLatencyProfile::Balanced,
            custom: Self::BALANCED,
        }
    }
}

impl AudioLatencySettings {
    pub const SAFE: AudioLatencyTuning = AudioLatencyTuning {
        startup_prefill_frames: 4096,
        guest_transfer_frames: 1024,
        coreaudio_buffer_us: 10_000,
    };
    pub const BALANCED: AudioLatencyTuning = AudioLatencyTuning {
        startup_prefill_frames: 2048,
        guest_transfer_frames: 1024,
        coreaudio_buffer_us: 5_000,
    };
    pub const LOW_LATENCY: AudioLatencyTuning = AudioLatencyTuning {
        startup_prefill_frames: 1024,
        guest_transfer_frames: 512,
        coreaudio_buffer_us: 3_000,
    };

    pub const fn effective(self) -> AudioLatencyTuning {
        match self.profile {
            AudioLatencyProfile::Safe => Self::SAFE,
            AudioLatencyProfile::Balanced => Self::BALANCED,
            AudioLatencyProfile::LowLatency => Self::LOW_LATENCY,
            AudioLatencyProfile::Custom => self.custom,
        }
    }

    /// Clamp Custom to combinations the 32-buffer guest pool can sustain.
    /// Transfers remain multiples of the fixed 64-frame ALSA period, and the
    /// startup prefill always covers at least two complete transfers.
    pub fn sanitize_custom(mut tuning: AudioLatencyTuning) -> AudioLatencyTuning {
        tuning.guest_transfer_frames = tuning.guest_transfer_frames.clamp(512, 2048);
        tuning.guest_transfer_frames = (tuning.guest_transfer_frames / 64).max(8) * 64;
        tuning.startup_prefill_frames = tuning
            .startup_prefill_frames
            .clamp(tuning.guest_transfer_frames * 2, 8192);
        tuning.startup_prefill_frames = (tuning.startup_prefill_frames / 64) * 64;
        tuning.coreaudio_buffer_us = tuning.coreaudio_buffer_us.clamp(3_000, 30_000);
        tuning
    }
}

/// Latched on shutdown.  **Async-signal-safe** so the SIGTERM/SIGINT handler
/// can set it without taking the mutex (which would deadlock if a signal
/// arrives while another thread already holds `AppState`).  Every other
/// shutdown-related state lives in [`AppState`].
pub static APP_SHUTDOWN: AtomicBool = AtomicBool::new(false);

// ── Domain types ──────────────────────────────────────────────────────────────

/// A single discovered network interface.
#[derive(Clone, Debug, PartialEq)]
pub struct NetIf {
    /// e.g. "en0"
    pub name: String,
    /// IPv4 address, e.g. "192.168.1.42"
    pub addr: String,
    /// CIDR prefix length, e.g. 24
    pub prefix_len: u8,
}

impl NetIf {
    /// Menu label: "192.168.1.42/24 (en0)"
    pub fn label(&self) -> String {
        format!("{}/{} ({})", self.addr, self.prefix_len, self.name)
    }
}

/// A removable physical disk entry (mirrors cdj3k_emu_runtime::usb::PhysicalDisk).
#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalDisk {
    pub bsd_name: String,
    pub label: String,
}

/// What the guest last reported about USB Save (mirrors
/// cdj3k_emu_runtime::cfg::UsbSaveState).
///
/// Only the armed bit: the copy keeps the source's own container, so there is no
/// format to choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbSaveState {
    pub armed: bool,
}

/// A CoreAudio output device exposed in the per-instance "Audio Output" picker.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioOutDevice {
    pub uid: String,
    pub name: String,
    pub is_default: bool,
    /// Current nominal sample rate in Hz, or 0 if unavailable.
    pub sample_rate_hz: u32,
}

/// Latest CDJ status decoded passively from this player's Pro DJ Link traffic.
/// Text metadata (title/artist/album) is not part of the recurring UDP status
/// packet; the source/slot/track id tuple is the key for a later DB-server
/// metadata query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProlinkTrackStatus {
    pub received_at: Instant,
    pub source_ip: String,
    pub device_name: String,
    pub player_number: u8,
    pub source_player: u8,
    pub source_slot: u8,
    pub track_type: u8,
    pub track_id: u32,
    pub track_number: u16,
    pub play_state: u8,
    pub state_flags: u8,
    pub pitch_raw: u32,
    pub track_bpm_x100: u16,
    pub beat_number: u32,
    pub beat_in_bar: u8,
    pub firmware: String,
}

// ── Aggregate state ───────────────────────────────────────────────────────────

/// All cross-thread UI / runtime state. Access via [`lock()`].
pub struct AppState {
    // ── Instance ────────────────────────────────────────────────────────────
    /// 1..=MAX_INSTANCES. Set once at startup from main.rs (`--instance N`).
    pub current_instance_id: u32,

    // ── View ────────────────────────────────────────────────────────────────
    pub jog_screen_popped: bool,
    pub main_screen_popped: bool,
    pub debug_screen_popped: bool,
    pub mixer_open: bool,

    // ── Emulation lifecycle ─────────────────────────────────────────────────
    /// Fires once; consumer clears after acting.
    pub stop_requested: bool,
    /// Set to `true` while QEMU is running; updated by the runtime worker.
    pub qemu_running: bool,
    /// Host keystrokes queued by the active player's egui window. The runtime
    /// worker drains these through the QMP-owned virtual USB keyboard.
    pub guest_keyboard_input: Vec<GuestKeyboardInput>,
    /// Fires once to open the firmware install wizard.
    pub firmware_wizard_requested: bool,
    /// Fires once to open the shared MIDI assignment window on this player's tab.
    pub midi_learn_requested: bool,
    /// Fires once after provisioning completes to (re)start QEMU.
    pub qemu_boot_requested: bool,
    /// Fires once to stop and immediately restart QEMU.
    pub restart_requested: bool,
    /// Set by menu actions that require a restart so the boot shade engages
    /// immediately. Cleared by the runtime worker once QEMU has respawned.
    pub shade_forced: bool,
    /// Set by the runtime on graceful shutdown; UI injects a `set_power(false)`
    /// MISO stimuli and clears.
    pub power_off_stimuli_requested: bool,
    pub service_mode: bool,

    /// "EP122 Mods" toggle - whether the cdj3k-mods linked into
    /// ep122_shim.so install themselves inside EP122.  Off puts
    /// `ep122_no_mods` on the kernel cmdline (guest patch 13 turns it into
    /// `EP122_NO_MODS=1` in EP122's environment).  Persisted in
    /// InstanceSettings; a QEMU restart applies it.
    pub mods_enabled: bool,
    /// One-shot: set when the user toggles the menu item; the runtime worker
    /// persists `mods_enabled` to InstanceSettings.
    pub mods_toggle_requested: bool,

    // ── Audio toggles ───────────────────────────────────────────────────────
    /// Mirror of the per-instance `audio_enabled` setting. Toggled by the
    /// "Enable audio" menu item; `audio_toggle_requested` fires the runtime
    /// worker to persist + restart QEMU.
    pub audio_enabled: bool,
    pub audio_toggle_requested: bool,
    /// One-shot request to tear down and reopen the running player's host
    /// audio path without restarting QEMU or the guest firmware.
    pub audio_reset_requested: bool,
    /// Opens the per-player audio latency profile editor.
    pub audio_latency_requested: bool,
    /// Current per-player latency selection mirrored for the UI/runtime.
    pub audio_latency: AudioLatencySettings,
    /// One-shot: persist the new latency selection and restart if audio is live.
    pub audio_latency_apply_requested: bool,
    /// Set by the runtime worker when a live audio reset fails. The menu
    /// thread consumes this and presents a one-shot error dialog.
    pub audio_error_message: Option<String>,

    // ── Audio output device ─────────────────────────────────────────────────
    /// Selected CoreAudio device UID, or `None` for "system default output".
    /// Persisted once in the application-wide settings. Every player and the
    /// singleton native mixer use the same selection.
    pub audio_device_uid: Option<String>,
    /// Cached enumeration of host output devices; refreshed by the menu
    /// thread on a 5 s tick via [`refresh_audio_devices`].
    pub audio_devices: Vec<AudioOutDevice>,
    /// Bumped when [`audio_devices`] changes; menu compares vs a local copy
    /// to rebuild the radio list.
    pub audio_device_list_version: u32,
    /// One-shot: set when the user picks a different device. The runtime
    /// Worker persists the new UID and switches the native mixer output live.
    pub audio_device_toggle_requested: bool,
    /// Dedicated headphone cue output UIDs; `None` means bus disabled.
    pub cue_a_device_uid: Option<String>,
    pub cue_b_device_uid: Option<String>,
    pub cue_a_device_toggle_requested: bool,
    pub cue_b_device_toggle_requested: bool,

    /// "Enable ALC (Experimental)" toggle. Mirrors the guest's
    /// `audio_sync_enabled` sysfs param.
    pub alc_enabled: bool,
    pub alc_toggle_requested: bool,

    /// "Trackpad Haptics" toggle. Gates the Force Touch detent clicks
    /// emitted as the jog wheel crosses detents.  No QEMU/guest side
    /// effect - read at the haptic-actuate call site only.
    pub haptic_enabled: bool,
    /// One-shot: set when the user toggles the menu item; the runtime worker
    /// consumes it and persists `haptic_enabled` to InstanceSettings.
    pub haptic_toggle_requested: bool,

    /// Latest audio pipeline depth, pushed by the guest cfg daemon every 3 s.
    /// Packed `(total << 32) | (guest << 16) | host`, all ms.  `u64::MAX` means
    /// "no data yet" and the menu shows `--` instead.
    pub latency_packed: u64,

    /// Latest status this player published over Pro DJ Link. Captured at the
    /// QEMU network boundary, so it is also available when the selected
    /// backend is user-mode NAT.
    pub prolink_track_status: Option<ProlinkTrackStatus>,

    // ── Network ─────────────────────────────────────────────────────────────
    /// Index into [`net_ifaces`], or one of [`NET_SEL_NONE`] / [`NET_SEL_MCAST`].
    pub selected_interface: u32,
    /// Cached interface list; updated by [`refresh_net_interfaces`].
    pub net_ifaces: Vec<NetIf>,
    /// Bumped whenever [`net_ifaces`] changes; consumers compare against a
    /// local copy to rebuild.
    pub net_list_version: u32,
    /// Set by the runtime worker when network setup (vmnet / tap bridge)
    /// fails.  Consumed by the menu thread on next sync: shows an `rfd`
    /// error popup and clears.
    pub net_error_message: Option<String>,

    // ── Storage / virtual USB ───────────────────────────────────────────────
    /// Path of the user-selected virtual USB image.
    pub usb_virtual_img: Option<PathBuf>,
    /// True while the virtual USB image is mounted in the guest.
    pub usb_virtual_mounted: bool,
    /// Menu→worker one-shot requests.
    pub usb_virtual_mount_req: bool,
    pub usb_create_req: bool,
    /// Unified eject for both virtual and physical mounts.
    pub usb_eject_req: bool,

    // ── Storage / physical USB ──────────────────────────────────────────────
    pub usb_phys_disks: Vec<PhysicalDisk>,
    /// Index of the physical disk currently mounted in the guest, or
    /// [`NO_USB_MOUNTED`] when nothing is mounted.
    pub usb_phys_mounted_idx: i32,
    /// Disk index whose toggle was requested by the menu, or [`NO_USB_MOUNTED`].
    pub usb_phys_toggle_idx: i32,
    /// Bumped by the USB worker whenever the disk list changes.
    pub usb_phys_list_version: u32,
    /// Set by the runtime when attach_physical fails with PermissionDenied;
    /// consumed by the menu to show a one-shot alert.
    pub usb_phys_perm_denied: bool,
    /// Set by the alert "Retry" button; runtime worker re-fires the toggle.
    pub usb_phys_retry_req: bool,

    // ── USB Save ────────────────────────────────────────────────────────────
    /// The host's switch, as the Debug panel shows it.
    pub usbsave_armed: bool,
    /// One-shot: raised by the panel when the armed bit changes, and consumed by
    /// the runtime worker, which pushes it to the guest.
    pub usbsave_send_req: bool,
    /// What the guest says it is doing, which can differ from what was asked:
    /// with the mods off, nothing in the guest reads the flag at all.
    pub usbsave_reported: Option<UsbSaveState>,
    /// The last save the mods reported, as cfgd relayed it: the fields written
    /// into the file. Shown in the panel so a save can be checked without mounting
    /// the live USB image.
    pub usbsave_last: Option<String>,
}

impl AppState {
    pub const fn new() -> Self {
        Self {
            current_instance_id: 1,
            jog_screen_popped: false,
            main_screen_popped: false,
            debug_screen_popped: false,
            mixer_open: false,
            stop_requested: false,
            qemu_running: false,
            guest_keyboard_input: Vec::new(),
            firmware_wizard_requested: false,
            midi_learn_requested: false,
            qemu_boot_requested: false,
            restart_requested: false,
            shade_forced: false,
            power_off_stimuli_requested: false,
            service_mode: false,
            mods_enabled: false,
            mods_toggle_requested: false,
            audio_enabled: false,
            audio_toggle_requested: false,
            audio_reset_requested: false,
            audio_latency_requested: false,
            audio_latency: AudioLatencySettings {
                profile: AudioLatencyProfile::Balanced,
                custom: AudioLatencySettings::BALANCED,
            },
            audio_latency_apply_requested: false,
            audio_error_message: None,
            audio_device_uid: None,
            audio_devices: Vec::new(),
            audio_device_list_version: 0,
            audio_device_toggle_requested: false,
            cue_a_device_uid: None,
            cue_b_device_uid: None,
            cue_a_device_toggle_requested: false,
            cue_b_device_toggle_requested: false,
            alc_enabled: false,
            alc_toggle_requested: false,
            haptic_enabled: true,
            haptic_toggle_requested: false,
            latency_packed: u64::MAX,
            prolink_track_status: None,
            selected_interface: NET_SEL_NONE,
            net_ifaces: Vec::new(),
            net_list_version: 0,
            net_error_message: None,
            usb_virtual_img: None,
            usb_virtual_mounted: false,
            usb_virtual_mount_req: false,
            usb_create_req: false,
            usb_eject_req: false,
            usb_phys_disks: Vec::new(),
            usb_phys_mounted_idx: NO_USB_MOUNTED,
            usb_phys_toggle_idx: NO_USB_MOUNTED,
            usb_phys_list_version: 0,
            usb_phys_perm_denied: false,
            usb_phys_retry_req: false,
            usbsave_armed: false,
            usbsave_send_req: false,
            usbsave_reported: None,
            usbsave_last: None,
        }
    }
}

static APP_STATE: Mutex<AppState> = Mutex::new(AppState::new());

/// Acquire the global state lock. Held only for the duration of struct field
/// accesses - never across I/O.
pub fn lock() -> MutexGuard<'static, AppState> {
    APP_STATE.lock().unwrap()
}

// ── Latency packing helpers ───────────────────────────────────────────────────

/// Pack a (total, guest, host) ms triple into the [`AppState::latency_packed`] encoding.
pub fn pack_latency(total_ms: u32, guest_ms: u32, host_ms: u32) -> u64 {
    ((total_ms as u64) << 32) | (((guest_ms as u64) & 0xFFFF) << 16) | ((host_ms as u64) & 0xFFFF)
}

/// Decode [`AppState::latency_packed`].  Returns `None` if no sample has been published yet.
pub fn unpack_latency(packed: u64) -> Option<(u32, u32, u32)> {
    if packed == u64::MAX {
        return None;
    }
    let total = (packed >> 32) as u32;
    let guest = ((packed >> 16) & 0xFFFF) as u32;
    let host = (packed & 0xFFFF) as u32;
    Some((total, guest, host))
}

// ── Network helpers ───────────────────────────────────────────────────────────

/// Returns true if the interface is something we can actually plug QEMU into.
/// Two valid backends, one allowlist prefix each:
///   en*  - real Ethernet / Wi-Fi; bridged via Apple's vmnet.framework.
///   tap* - user-mode TAP; bridged in userspace by `TapBridge::setup`.
/// Everything else (utun, awdl, gif, stf, ipsec, bridge, bond, vlan, lo, …)
/// is either Layer 3 or has no upstream link and can't be bridged.
fn is_bridgeable(name: &str) -> bool {
    name.starts_with("en") || name.starts_with("tap")
}

/// Enumerate all non-loopback IPv4 interfaces via `getifaddrs`.
pub fn enumerate_interfaces() -> Vec<NetIf> {
    let mut out = Vec::new();

    #[cfg(unix)]
    unsafe {
        let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut addrs) != 0 {
            return out;
        }

        let mut cur = addrs;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;

            let sa = ifa.ifa_addr;
            if sa.is_null() {
                continue;
            }
            if (*sa).sa_family as i32 != libc::AF_INET {
                continue;
            }
            if (ifa.ifa_flags & libc::IFF_LOOPBACK as u32) != 0 {
                continue;
            }
            if (ifa.ifa_flags & libc::IFF_UP as u32) == 0 {
                continue;
            }

            let sin = &*(sa as *const libc::sockaddr_in);
            let ip = u32::from_be(sin.sin_addr.s_addr);
            let a = (ip >> 24) as u8;
            let b = (ip >> 16) as u8;
            let c = (ip >> 8) as u8;
            let d = ip as u8;

            let mask_sa = ifa.ifa_netmask;
            let prefix_len = if !mask_sa.is_null() && (*mask_sa).sa_family as i32 == libc::AF_INET {
                let mask_sin = &*(mask_sa as *const libc::sockaddr_in);
                u32::from_be(mask_sin.sin_addr.s_addr).count_ones() as u8
            } else {
                0
            };

            let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                .to_string_lossy()
                .into_owned();

            if !is_bridgeable(&name) {
                continue;
            }

            out.push(NetIf {
                name,
                addr: format!("{a}.{b}.{c}.{d}"),
                prefix_len,
            });
        }

        libc::freeifaddrs(addrs);
    }

    out
}

/// Re-enumerate interfaces and update the cached list.
/// Bumps `net_list_version` only when the list actually changed.
pub fn refresh_net_interfaces() {
    let fresh = enumerate_interfaces();
    let mut s = lock();
    let changed = s.net_ifaces.len() != fresh.len()
        || s.net_ifaces
            .iter()
            .zip(fresh.iter())
            .any(|(a, b)| a.name != b.name || a.addr != b.addr);
    if changed {
        s.net_ifaces = fresh;
        s.net_list_version = s.net_list_version.wrapping_add(1);
    }
}

// ── Audio device helpers ──────────────────────────────────────────────────────

/// Re-enumerate CoreAudio output devices and update the cached list.
/// Bumps `audio_device_list_version` only when the list actually changed.
/// No-op on non-macOS targets.
pub fn refresh_audio_devices() {
    #[cfg(target_os = "macos")]
    {
        let fresh: Vec<AudioOutDevice> = crate::audio_devices::enumerate_output_devices()
            .into_iter()
            .map(|d| AudioOutDevice {
                uid: d.uid,
                name: d.name,
                is_default: d.is_default,
                sample_rate_hz: d.sample_rate_hz,
            })
            .collect();
        let mut s = lock();
        let changed = s.audio_devices.len() != fresh.len()
            || s.audio_devices.iter().zip(fresh.iter()).any(|(a, b)| {
                a.uid != b.uid
                    || a.name != b.name
                    || a.is_default != b.is_default
                    || a.sample_rate_hz != b.sample_rate_hz
            });
        if changed {
            s.audio_devices = fresh;
            s.audio_device_list_version = s.audio_device_list_version.wrapping_add(1);
        }
    }
}
