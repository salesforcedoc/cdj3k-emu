//! Cross-process virtual mixer controls and meters.
//!
//! The UI processes and their QEMU children mmap one small per-user file. All
//! values are aligned 32-bit atomics, so CoreAudio can read controls and write
//! meters without a lock or allocation in its real-time callback.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, Ordering};

use memmap2::{MmapMut, MmapOptions};

use crate::runtime_paths;

pub const CHANNEL_COUNT: usize = 4;
pub const MIXER_MAGIC: u32 = u32::from_le_bytes(*b"CMX1");
pub const MIXER_VERSION: u32 = 4;
pub const HEADER_BYTES: usize = 64;
pub const CHANNEL_BYTES: usize = 64;
pub const RING_BYTES: usize = 128 * 1024;
pub const RING_BASE: usize = HEADER_BYTES + CHANNEL_COUNT * CHANNEL_BYTES;
pub const MIXER_BYTES: usize = RING_BASE + CHANNEL_COUNT * RING_BYTES;
// Native-only controls live after the original QEMU transport region. QEMU
// continues mapping MIXER_BYTES; no guest ABI or ring offset changes.
const EXTRA_BASE: usize = MIXER_BYTES;
const MIXER_MAP_BYTES: usize = MIXER_BYTES + 128;
const EXTRA_MAGIC: u32 = u32::from_le_bytes(*b"MEX1");
const MASTER_LEVEL: usize = EXTRA_BASE + 4;
const MASTER_PEAK_LEFT: usize = EXTRA_BASE + 8;
const MASTER_PEAK_RIGHT: usize = EXTRA_BASE + 12;
const MASTER_CUE: usize = EXTRA_BASE + 16;
const FX_TYPE: usize = EXTRA_BASE + 20;
const FX_CHANNEL: usize = EXTRA_BASE + 24;
const FX_ENABLED: usize = EXTRA_BASE + 28;
const FX_DEPTH: usize = EXTRA_BASE + 32;
const ASSIGN_BASE: usize = EXTRA_BASE + 36;
const ISO_BASE: usize = EXTRA_BASE + 52;
const CHANNEL_CUE_BASE: usize = EXTRA_BASE + 68;
const GLOBAL_EQ_CURVE: usize = EXTRA_BASE + 84;
const FX_BANDS: usize = EXTRA_BASE + 88;
const EXTRA_V2_MARKER: usize = EXTRA_BASE + 92;
const EXTRA_V2_MAGIC: u32 = u32::from_le_bytes(*b"MEX2");
const CHANNEL_CUE_B_BASE: usize = EXTRA_BASE + 96;
const MASTER_CUE_B: usize = EXTRA_BASE + 112;
const CUE_LEVEL_A: usize = EXTRA_BASE + 116;
const CUE_LEVEL_B: usize = EXTRA_BASE + 120;
const CUE_LEVEL_MAGIC_OFFSET: usize = EXTRA_BASE + 124;
const CUE_LEVEL_MAGIC: u32 = u32::from_le_bytes(*b"CLV1");

const MAGIC_OFFSET: usize = 0;
const VERSION_OFFSET: usize = 4;
const CROSSFADER_OFFSET: usize = 8;
const OUTPUT_RATE_OFFSET: usize = 12;
const DEVICE_GENERATION_OFFSET: usize = 16;
const RESET_GENERATION_OFFSET: usize = 20;
const BUFFER_US_OFFSET: usize = 24;
const WINDOW_OPEN_OFFSET: usize = 28;
const HOST_BUFFER_FRAMES_OFFSET: usize = 32;
const HOST_SAFETY_FRAMES_OFFSET: usize = 36;
const HOST_DEVICE_FRAMES_OFFSET: usize = 40;
// Previously unused header words; keeping the mixer layout version allows
// older audio processes to coexist while newer UIs coordinate MIDI Learn.
const MIDI_LEARN_OPEN_OFFSET: usize = 44;
const MIDI_LEARN_TAB_OFFSET: usize = 48;
const MIDI_LEARN_REQUEST_OFFSET: usize = 52;
const MIDI_LEARN_ACTIVE_OFFSET: usize = 56;
const MIDI_DEVICE_ACTION_OFFSET: usize = 60;
const PEAK_LEFT_OFFSET: usize = 24;
const PEAK_RIGHT_OFFSET: usize = 28;
const RING_HEAD_OFFSET: usize = 32;
const RING_TAIL_OFFSET: usize = 40;
const RING_ACTIVE_OFFSET: usize = 48;
const SOURCE_RATE_OFFSET: usize = 52;
const RING_TARGET_FRAMES_OFFSET: usize = 56;
const DEFAULT_RING_TARGET_FRAMES: u32 = 2048;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerParameter {
    Trim,
    High,
    Mid,
    Low,
    Filter,
    Fader,
}

impl MixerParameter {
    pub const ALL: [Self; 6] = [
        Self::Trim,
        Self::High,
        Self::Mid,
        Self::Low,
        Self::Filter,
        Self::Fader,
    ];

    const fn offset(self) -> usize {
        match self {
            Self::Trim => 0,
            Self::High => 4,
            Self::Mid => 8,
            Self::Low => 12,
            Self::Filter => 16,
            Self::Fader => 20,
        }
    }
}

/// Live shared mixer mapping. `MixerShared` is cheap to access and should be
/// retained for the lifetime of the app rather than reopened per frame.
pub struct MixerShared {
    mmap: MmapMut,
}

impl MixerShared {
    pub fn open() -> io::Result<Self> {
        runtime_paths::ensure_runtime_base_dir()?;
        let path = runtime_paths::mixer_shm_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        // Serialize first-time sizing/initialization between simultaneously
        // launched player processes. The lock is released before returning.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        file.set_len(MIXER_MAP_BYTES as u64)?;
        let mut mmap = unsafe { MmapOptions::new().len(MIXER_MAP_BYTES).map_mut(&file)? };
        let initialized = read_u32(&mmap, MAGIC_OFFSET) == MIXER_MAGIC
            && read_u32(&mmap, VERSION_OFFSET) == MIXER_VERSION;
        if !initialized {
            mmap.fill(0);
            write_u32(&mut mmap, VERSION_OFFSET, MIXER_VERSION);
            write_u32(&mut mmap, CROSSFADER_OFFSET, normalized_to_raw(0.5));
            write_u32(&mut mmap, OUTPUT_RATE_OFFSET, 0);
            write_u32(&mut mmap, DEVICE_GENERATION_OFFSET, 0);
            write_u32(&mut mmap, RESET_GENERATION_OFFSET, 0);
            write_u32(&mut mmap, BUFFER_US_OFFSET, 5_000);
            write_u32(&mut mmap, WINDOW_OPEN_OFFSET, 0);
            for channel in 0..CHANNEL_COUNT {
                for parameter in MixerParameter::ALL {
                    let default = match parameter {
                        MixerParameter::Fader => 1.0,
                        _ => 0.5,
                    };
                    write_u32(
                        &mut mmap,
                        channel_offset(channel) + parameter.offset(),
                        normalized_to_raw(default),
                    );
                }
                write_u32(
                    &mut mmap,
                    channel_offset(channel) + RING_TARGET_FRAMES_OFFSET,
                    DEFAULT_RING_TARGET_FRAMES,
                );
            }
            // Publish magic last so a QEMU callback never accepts a partly
            // initialized layout.
            write_u32(&mut mmap, MAGIC_OFFSET, MIXER_MAGIC);
            mmap.flush_async()?;
        }
        if read_u32(&mmap, EXTRA_BASE) != EXTRA_MAGIC {
            mmap[EXTRA_BASE..MIXER_MAP_BYTES].fill(0);
            write_u32(&mut mmap, MASTER_LEVEL, normalized_to_raw(1.0));
            write_u32(&mut mmap, FX_DEPTH, normalized_to_raw(0.5));
            for channel in 0..CHANNEL_COUNT {
                // 0=A, 1=THRU, 2=B. Preserve the old 1-2/A, 3-4/B routing.
                write_u32(
                    &mut mmap,
                    ASSIGN_BASE + channel * 4,
                    if channel < 2 { 0 } else { 2 },
                );
            }
            write_u32(&mut mmap, EXTRA_BASE, EXTRA_MAGIC);
            mmap.flush_async()?;
        }
        if read_u32(&mmap, EXTRA_V2_MARKER) != EXTRA_V2_MAGIC {
            let old_eq_curve = read_u32(&mmap, ISO_BASE).min(1);
            write_u32(&mut mmap, GLOBAL_EQ_CURVE, old_eq_curve);
            write_u32(&mut mmap, FX_BANDS, 0b111);
            write_u32(&mut mmap, EXTRA_V2_MARKER, EXTRA_V2_MAGIC);
            mmap.flush_async()?;
        }
        if read_u32(&mmap, CUE_LEVEL_MAGIC_OFFSET) != CUE_LEVEL_MAGIC {
            write_u32(&mut mmap, CUE_LEVEL_A, normalized_to_raw(1.0));
            write_u32(&mut mmap, CUE_LEVEL_B, normalized_to_raw(1.0));
            write_u32(&mut mmap, CUE_LEVEL_MAGIC_OFFSET, CUE_LEVEL_MAGIC);
            mmap.flush_async()?;
        }
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        Ok(Self { mmap })
    }

    pub fn path() -> std::path::PathBuf {
        runtime_paths::mixer_shm_path()
    }

    pub fn get(&self, channel: usize, parameter: MixerParameter) -> f32 {
        let Some(offset) = parameter_offset(channel, parameter) else {
            return 0.0;
        };
        raw_to_normalized(self.atomic(offset).load(Ordering::Relaxed))
    }

    pub fn set(&self, channel: usize, parameter: MixerParameter, value: f32) {
        let Some(offset) = parameter_offset(channel, parameter) else {
            return;
        };
        self.atomic(offset)
            .store(normalized_to_raw(value), Ordering::Relaxed);
    }

    /// Shared crossfader position. Channels 1-2 are assigned to A and
    /// channels 3-4 to B; 0.5 leaves both sides at unity.
    pub fn crossfader(&self) -> f32 {
        raw_to_normalized(self.atomic(CROSSFADER_OFFSET).load(Ordering::Relaxed))
    }

    pub fn set_crossfader(&self, value: f32) {
        self.atomic(CROSSFADER_OFFSET)
            .store(normalized_to_raw(value), Ordering::Relaxed);
    }

    pub fn master_level(&self) -> f32 {
        raw_to_normalized(self.atomic(MASTER_LEVEL).load(Ordering::Relaxed))
    }
    pub fn set_master_level(&self, value: f32) {
        self.atomic(MASTER_LEVEL)
            .store(normalized_to_raw(value), Ordering::Relaxed);
    }
    pub fn master_peaks(&self) -> [f32; 2] {
        [
            raw_to_normalized(self.atomic(MASTER_PEAK_LEFT).load(Ordering::Relaxed)),
            raw_to_normalized(self.atomic(MASTER_PEAK_RIGHT).load(Ordering::Relaxed)),
        ]
    }
    pub fn set_master_peaks(&self, peaks: [f32; 2]) {
        self.atomic(MASTER_PEAK_LEFT)
            .store(normalized_to_raw(peaks[0]), Ordering::Relaxed);
        self.atomic(MASTER_PEAK_RIGHT)
            .store(normalized_to_raw(peaks[1]), Ordering::Relaxed);
    }
    pub fn cross_assignment(&self, channel: usize) -> u32 {
        if channel >= CHANNEL_COUNT {
            return 1;
        }
        self.atomic(ASSIGN_BASE + channel * 4)
            .load(Ordering::Relaxed)
            .min(2)
    }
    pub fn set_cross_assignment(&self, channel: usize, assignment: u32) {
        if channel < CHANNEL_COUNT {
            self.atomic(ASSIGN_BASE + channel * 4)
                .store(assignment.min(2), Ordering::Relaxed);
        }
    }
    pub fn eq_isolator(&self) -> bool {
        self.atomic(GLOBAL_EQ_CURVE).load(Ordering::Relaxed) != 0
    }
    pub fn set_eq_isolator(&self, enabled: bool) {
        self.atomic(GLOBAL_EQ_CURVE)
            .store(u32::from(enabled), Ordering::Relaxed);
    }
    pub fn channel_cue(&self, channel: usize) -> bool {
        channel < CHANNEL_COUNT
            && self
                .atomic(CHANNEL_CUE_BASE + channel * 4)
                .load(Ordering::Relaxed)
                != 0
    }
    pub fn set_channel_cue(&self, channel: usize, enabled: bool) {
        if channel < CHANNEL_COUNT {
            self.atomic(CHANNEL_CUE_BASE + channel * 4)
                .store(u32::from(enabled), Ordering::Relaxed);
        }
    }
    pub fn channel_cue_b(&self, channel: usize) -> bool {
        channel < CHANNEL_COUNT
            && self
                .atomic(CHANNEL_CUE_B_BASE + channel * 4)
                .load(Ordering::Relaxed)
                != 0
    }
    pub fn set_channel_cue_b(&self, channel: usize, enabled: bool) {
        if channel < CHANNEL_COUNT {
            self.atomic(CHANNEL_CUE_B_BASE + channel * 4)
                .store(u32::from(enabled), Ordering::Relaxed);
        }
    }
    pub fn master_cue(&self) -> bool {
        self.atomic(MASTER_CUE).load(Ordering::Relaxed) != 0
    }
    pub fn set_master_cue(&self, enabled: bool) {
        self.atomic(MASTER_CUE)
            .store(u32::from(enabled), Ordering::Relaxed);
    }
    pub fn master_cue_b(&self) -> bool {
        self.atomic(MASTER_CUE_B).load(Ordering::Relaxed) != 0
    }
    pub fn set_master_cue_b(&self, enabled: bool) {
        self.atomic(MASTER_CUE_B)
            .store(u32::from(enabled), Ordering::Relaxed);
    }
    pub fn cue_level(&self, bus: usize) -> f32 {
        let offset = if bus == 0 { CUE_LEVEL_A } else { CUE_LEVEL_B };
        raw_to_normalized(self.atomic(offset).load(Ordering::Relaxed))
    }
    pub fn set_cue_level(&self, bus: usize, level: f32) {
        if bus < 2 {
            let offset = if bus == 0 { CUE_LEVEL_A } else { CUE_LEVEL_B };
            self.atomic(offset)
                .store(normalized_to_raw(level), Ordering::Relaxed);
        }
    }
    pub fn fx_type(&self) -> u32 {
        self.atomic(FX_TYPE).load(Ordering::Relaxed).min(3)
    }
    pub fn set_fx_type(&self, effect: u32) {
        self.atomic(FX_TYPE).store(effect.min(3), Ordering::Relaxed);
    }
    pub fn fx_channel(&self) -> u32 {
        self.atomic(FX_CHANNEL)
            .load(Ordering::Relaxed)
            .min(CHANNEL_COUNT as u32)
    }
    pub fn set_fx_channel(&self, channel: u32) {
        self.atomic(FX_CHANNEL)
            .store(channel.min(CHANNEL_COUNT as u32), Ordering::Relaxed);
    }
    pub fn fx_enabled(&self) -> bool {
        self.atomic(FX_ENABLED).load(Ordering::Relaxed) != 0
    }
    pub fn set_fx_enabled(&self, enabled: bool) {
        self.atomic(FX_ENABLED)
            .store(u32::from(enabled), Ordering::Relaxed);
    }
    pub fn fx_depth(&self) -> f32 {
        raw_to_normalized(self.atomic(FX_DEPTH).load(Ordering::Relaxed))
    }
    pub fn set_fx_depth(&self, value: f32) {
        self.atomic(FX_DEPTH)
            .store(normalized_to_raw(value), Ordering::Relaxed);
    }
    pub fn fx_bands(&self) -> u32 {
        self.atomic(FX_BANDS).load(Ordering::Relaxed) & 0b111
    }
    pub fn set_fx_bands(&self, bands: u32) {
        self.atomic(FX_BANDS)
            .store(bands & 0b111, Ordering::Relaxed);
    }

    pub fn output_rate(&self) -> u32 {
        self.atomic(OUTPUT_RATE_OFFSET).load(Ordering::Acquire)
    }

    pub fn set_output_rate(&self, rate: u32) {
        let previous = self.output_rate();

        self.atomic(OUTPUT_RATE_OFFSET)
            .store(rate, Ordering::Release);

        /* THE GUEST ARRIVING IS WHAT MAKES THE ENGINE WORK, and until now only the
         * DJ could tell it so.
         *
         * NativeMixer::start elects an owner and starts the engine from
         * owner_loop -- which happens BEFORE QEMU is spawned, in every launch, and
         * starts it with the rate still unknown (set_output_rate(0)). The real rate
         * comes from the guest's virtio-snd once it boots. So the engine binds while
         * the rate is zero and sits silent, and the cure is any reset afterwards --
         * which is what the menu's Audio -> Reset audio does, by hand, every launch,
         * because the ordering is fixed rather than racy.
         *
         * So do it here instead: the first time a real rate arrives, ask for the same
         * reset the menu asks for. */
        /* ONCE, not "whenever the rate rises from zero": owner_loop zeroes the rate
         * before every engine start, so a plain 0 -> real test re-arms itself on each
         * restart and resets forever. Measured -- the first version of this did exactly
         * that, and the app log filled with engine starts until it was killed. */
        static ANNOUNCED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);

        if previous == 0 && rate != 0
            && !ANNOUNCED.swap(true, Ordering::AcqRel)
        {
            self.request_audio_reset();
        }
    }

    pub fn device_generation(&self) -> u32 {
        self.atomic(DEVICE_GENERATION_OFFSET)
            .load(Ordering::Acquire)
    }

    pub fn reset_generation(&self) -> u32 {
        self.atomic(RESET_GENERATION_OFFSET).load(Ordering::Acquire)
    }

    pub fn request_device_reopen(&self) {
        self.atomic(DEVICE_GENERATION_OFFSET)
            .fetch_add(1, Ordering::AcqRel);
    }

    pub fn request_audio_reset(&self) {
        self.atomic(RESET_GENERATION_OFFSET)
            .fetch_add(1, Ordering::AcqRel);
    }

    pub fn buffer_us(&self) -> u32 {
        self.atomic(BUFFER_US_OFFSET).load(Ordering::Acquire)
    }

    pub fn request_buffer_us(&self, value: u32) {
        self.atomic(BUFFER_US_OFFSET)
            .store(value.clamp(3_000, 30_000), Ordering::Release);
        self.request_audio_reset();
    }

    pub fn window_open(&self) -> bool {
        self.atomic(WINDOW_OPEN_OFFSET).load(Ordering::Acquire) != 0
    }

    pub fn set_window_open(&self, open: bool) {
        self.atomic(WINDOW_OPEN_OFFSET)
            .store(u32::from(open), Ordering::Release);
    }

    pub fn midi_learn_open(&self) -> bool {
        self.atomic(MIDI_LEARN_OPEN_OFFSET).load(Ordering::Acquire) != 0
    }

    /// Open or refocus the singleton MIDI Learn viewport on the requested
    /// player tab. Tab 0-3 is a player; tab 4 is the shared mixer.
    pub fn request_midi_learn(&self, tab: u32) {
        self.atomic(MIDI_LEARN_TAB_OFFSET)
            .store(tab.min(CHANNEL_COUNT as u32), Ordering::Release);
        self.atomic(MIDI_LEARN_OPEN_OFFSET)
            .store(1, Ordering::Release);
        self.atomic(MIDI_LEARN_REQUEST_OFFSET)
            .fetch_add(1, Ordering::AcqRel);
    }

    pub fn midi_learn_tab(&self) -> u32 {
        self.atomic(MIDI_LEARN_TAB_OFFSET).load(Ordering::Acquire)
    }

    pub fn set_midi_learn_tab(&self, tab: u32) {
        self.atomic(MIDI_LEARN_TAB_OFFSET)
            .store(tab.min(CHANNEL_COUNT as u32), Ordering::Release);
    }

    pub fn midi_learn_request_generation(&self) -> u32 {
        self.atomic(MIDI_LEARN_REQUEST_OFFSET)
            .load(Ordering::Acquire)
    }

    pub fn set_midi_learn_open(&self, open: bool) {
        self.atomic(MIDI_LEARN_OPEN_OFFSET)
            .store(u32::from(open), Ordering::Release);
        if !open {
            self.set_midi_learn_active(false);
        }
    }

    pub fn midi_learn_active(&self) -> bool {
        self.atomic(MIDI_LEARN_ACTIVE_OFFSET)
            .load(Ordering::Acquire)
            != 0
    }

    pub fn set_midi_learn_active(&self, active: bool) {
        self.atomic(MIDI_LEARN_ACTIVE_OFFSET)
            .store(u32::from(active), Ordering::Release);
    }

    /// Cross-process rescan/reset request. Low two bits encode action:
    /// 1 = rescan all inputs, 2 = reset CoreMIDI clients before reconnecting.
    pub fn request_midi_device_action(&self, reset: bool) {
        let current = self
            .atomic(MIDI_DEVICE_ACTION_OFFSET)
            .load(Ordering::Acquire);
        let next_generation = (current >> 2).wrapping_add(1);
        let action = if reset { 2 } else { 1 };
        self.atomic(MIDI_DEVICE_ACTION_OFFSET)
            .store((next_generation << 2) | action, Ordering::Release);
    }

    pub fn midi_device_action(&self) -> u32 {
        self.atomic(MIDI_DEVICE_ACTION_OFFSET)
            .load(Ordering::Acquire)
    }

    pub fn set_host_latency_frames(&self, buffer: u32, safety: u32, device: u32) {
        self.atomic(HOST_BUFFER_FRAMES_OFFSET)
            .store(buffer, Ordering::Release);
        self.atomic(HOST_SAFETY_FRAMES_OFFSET)
            .store(safety, Ordering::Release);
        self.atomic(HOST_DEVICE_FRAMES_OFFSET)
            .store(device, Ordering::Release);
    }

    pub fn host_latency_frames(&self) -> (u32, u32, u32) {
        (
            self.atomic(HOST_BUFFER_FRAMES_OFFSET)
                .load(Ordering::Acquire),
            self.atomic(HOST_SAFETY_FRAMES_OFFSET)
                .load(Ordering::Acquire),
            self.atomic(HOST_DEVICE_FRAMES_OFFSET)
                .load(Ordering::Acquire),
        )
    }

    pub fn source_rate(&self, channel: usize) -> u32 {
        if channel >= CHANNEL_COUNT {
            return 0;
        }
        self.atomic(channel_offset(channel) + SOURCE_RATE_OFFSET)
            .load(Ordering::Acquire)
    }

    /// Desired steady-state queue depth for this channel. The owning player
    /// publishes its latency profile before QEMU starts; the native resampler
    /// uses it as the clock-drift controller's set point.
    pub fn target_audio_frames(&self, channel: usize) -> u32 {
        if channel >= CHANNEL_COUNT {
            return DEFAULT_RING_TARGET_FRAMES;
        }
        let configured = self
            .atomic(channel_offset(channel) + RING_TARGET_FRAMES_OFFSET)
            .load(Ordering::Acquire);
        if configured == 0 {
            DEFAULT_RING_TARGET_FRAMES
        } else {
            configured
        }
    }

    pub fn set_target_audio_frames(&self, channel: usize, frames: u32) {
        if channel >= CHANNEL_COUNT {
            return;
        }
        self.atomic(channel_offset(channel) + RING_TARGET_FRAMES_OFFSET)
            .store(frames.clamp(512, RING_BYTES as u32 / 16), Ordering::Release);
    }

    /// Read interleaved float32 stereo frames from one QEMU producer ring.
    /// Called only from the native CoreAudio render callback; performs no
    /// allocation, locking, or syscalls.
    pub fn read_audio(&self, channel: usize, output: &mut [f32]) -> usize {
        if channel >= CHANNEL_COUNT || output.len() < 2 {
            return 0;
        }
        let frames_requested = output.len() / 2;
        let bytes_requested = frames_requested * 8;
        let base = channel_offset(channel);
        if self
            .atomic(base + RING_ACTIVE_OFFSET)
            .load(Ordering::Acquire)
            == 0
        {
            return 0;
        }
        let head = self
            .atomic_u64(base + RING_HEAD_OFFSET)
            .load(Ordering::Acquire);
        let tail_atomic = self.atomic_u64(base + RING_TAIL_OFFSET);
        let tail = tail_atomic.load(Ordering::Relaxed);
        let available = head.saturating_sub(tail) as usize;
        if available < bytes_requested {
            return 0;
        }
        let ring = ring_offset(channel);
        let offset = (tail as usize) & (RING_BYTES - 1);
        let first = bytes_requested.min(RING_BYTES - offset);
        // SAFETY: both source regions are within the live mmap; output is a
        // correctly aligned f32 slice and the producer publishes head only
        // after completing its byte copies.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.mmap.as_ptr().add(ring + offset),
                output.as_mut_ptr().cast::<u8>(),
                first,
            );
            if bytes_requested > first {
                std::ptr::copy_nonoverlapping(
                    self.mmap.as_ptr().add(ring),
                    output.as_mut_ptr().cast::<u8>().add(first),
                    bytes_requested - first,
                );
            }
        }
        tail_atomic.store(tail + bytes_requested as u64, Ordering::Release);
        frames_requested
    }

    pub fn available_audio_frames(&self, channel: usize) -> usize {
        if channel >= CHANNEL_COUNT {
            return 0;
        }
        let base = channel_offset(channel);
        let head = self
            .atomic_u64(base + RING_HEAD_OFFSET)
            .load(Ordering::Acquire);
        let tail = self
            .atomic_u64(base + RING_TAIL_OFFSET)
            .load(Ordering::Relaxed);
        head.saturating_sub(tail) as usize / 8
    }

    /// True when this player's QEMU process attached its virtio-sound writer
    /// directly to the shared native-mixer ring.
    pub fn external_ring_active(&self, channel: usize) -> bool {
        if channel >= CHANNEL_COUNT {
            return false;
        }
        self.atomic(channel_offset(channel) + RING_ACTIVE_OFFSET)
            .load(Ordering::Acquire)
            != 0
    }

    pub fn set_peaks(&self, channel: usize, peaks: [f32; 2]) {
        if channel >= CHANNEL_COUNT {
            return;
        }
        let base = channel_offset(channel);
        self.atomic(base + PEAK_LEFT_OFFSET)
            .store(normalized_to_raw(peaks[0]), Ordering::Relaxed);
        self.atomic(base + PEAK_RIGHT_OFFSET)
            .store(normalized_to_raw(peaks[1]), Ordering::Relaxed);
    }

    pub fn peaks(&self, channel: usize) -> [f32; 2] {
        if channel >= CHANNEL_COUNT {
            return [0.0, 0.0];
        }
        let base = channel_offset(channel);
        [
            raw_to_normalized(self.atomic(base + PEAK_LEFT_OFFSET).load(Ordering::Relaxed)),
            raw_to_normalized(
                self.atomic(base + PEAK_RIGHT_OFFSET)
                    .load(Ordering::Relaxed),
            ),
        ]
    }

    fn atomic(&self, offset: usize) -> &AtomicU32 {
        debug_assert_eq!(offset % std::mem::align_of::<AtomicU32>(), 0);
        debug_assert!(offset + std::mem::size_of::<AtomicU32>() <= self.mmap.len());
        // SAFETY: mmap base addresses are page-aligned, all offsets are four-
        // byte aligned and inside a live mapping. Every cross-process access
        // to these words uses a 32-bit atomic in both Rust and C.
        unsafe { &*(self.mmap.as_ptr().add(offset).cast::<AtomicU32>()) }
    }

    fn atomic_u64(&self, offset: usize) -> &std::sync::atomic::AtomicU64 {
        debug_assert_eq!(
            offset % std::mem::align_of::<std::sync::atomic::AtomicU64>(),
            0
        );
        debug_assert!(
            offset + std::mem::size_of::<std::sync::atomic::AtomicU64>() <= self.mmap.len()
        );
        // SAFETY: mmap is page-aligned and all u64 fields are eight-byte
        // aligned in the cross-language layout.
        unsafe {
            &*(self
                .mmap
                .as_ptr()
                .add(offset)
                .cast::<std::sync::atomic::AtomicU64>())
        }
    }
}

const fn channel_offset(channel: usize) -> usize {
    HEADER_BYTES + channel * CHANNEL_BYTES
}

const fn ring_offset(channel: usize) -> usize {
    RING_BASE + channel * RING_BYTES
}

fn parameter_offset(channel: usize, parameter: MixerParameter) -> Option<usize> {
    (channel < CHANNEL_COUNT).then(|| channel_offset(channel) + parameter.offset())
}

fn normalized_to_raw(value: f32) -> u32 {
    (value.clamp(0.0, 1.0) * 1000.0).round() as u32
}

fn raw_to_normalized(value: u32) -> f32 {
    value.min(1000) as f32 / 1000.0
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_the_qemu_contract() {
        assert_eq!(MIXER_BYTES, 524_608);
        assert_eq!(MIXER_MAP_BYTES, MIXER_BYTES + 128);
        assert_eq!(channel_offset(3), 256);
        assert_eq!(parameter_offset(3, MixerParameter::Fader), Some(276));
        assert_eq!(CROSSFADER_OFFSET, 8);
        assert_eq!(WINDOW_OPEN_OFFSET, 28);
        assert_eq!(SOURCE_RATE_OFFSET, 52);
        assert_eq!(RING_TARGET_FRAMES_OFFSET, 56);
        assert_eq!(ring_offset(0), 320);
        assert_eq!(ring_offset(3), 393_536);
    }

    #[test]
    fn normalized_values_are_clamped() {
        assert_eq!(normalized_to_raw(-1.0), 0);
        assert_eq!(normalized_to_raw(0.5), 500);
        assert_eq!(normalized_to_raw(2.0), 1000);
        assert_eq!(raw_to_normalized(1200), 1.0);
    }
}
