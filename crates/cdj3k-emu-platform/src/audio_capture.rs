//! Non-blocking, cross-process capture of each native-mixer input.
//!
//! The CoreAudio owner and the player whose Debug window starts a recording
//! are not necessarily the same process. A dedicated mmap ring per channel
//! tees the guest's raw stereo input before resampling and before
//! trim/EQ/filter/fader processing.
//! The real-time callback only copies into the ring; a normal-priority writer
//! thread drains it into a 32-bit float WAV file.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use memmap2::{MmapMut, MmapOptions};

use crate::mixer::{MixerShared, CHANNEL_COUNT};
use crate::runtime_paths;

const CAPTURE_MAGIC: u32 = u32::from_le_bytes(*b"CAP1");
/// 2: a slot is now (point, channel) rather than a channel alone, so a mapping
/// left by an older version is reinitialised rather than read as the new layout.
const CAPTURE_VERSION: u32 = 2;
const HEADER_BYTES: usize = 64;
const CHANNEL_BYTES: usize = 64;
// About 2.7 seconds of float32 stereo at 96 kHz. This absorbs filesystem
// scheduling hiccups without adding playback latency.
const RING_BYTES: usize = 2 * 1024 * 1024;

/// Which side of the mixer a capture is taken from.
///
/// PRE is what the guest sent -- the deck's own output, before anything the host
/// does to it. POST is what the mixer produced, after the rate conversion and
/// after trim, EQ, filter, fader and master level.
///
/// Both taps exist because one alone cannot attribute an artefact: audio that is
/// wrong in the guest's output was made by the deck, and audio that is wrong only
/// in the mixer's output was made here. Reading one tap and guessing which of
/// those it is has already cost a long afternoon on this project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturePoint {
    PreMixer = 0,
    PostMixer = 1,
}

const POINTS: usize = 2;
const SLOTS: usize = POINTS * CHANNEL_COUNT;

/// One ring per (point, channel). The taps are separate regions, so both can
/// record at once -- which is what makes them comparable sample for sample --
/// and the mixer never has to know which of them is armed.
const fn slot_of(point: CapturePoint, channel: usize) -> usize {
    point as usize * CHANNEL_COUNT + channel
}

const RING_BASE: usize = HEADER_BYTES + SLOTS * CHANNEL_BYTES;
const CAPTURE_BYTES: usize = RING_BASE + SLOTS * RING_BYTES;

const MAGIC_OFFSET: usize = 0;
const VERSION_OFFSET: usize = 4;
const ACTIVE_OFFSET: usize = 0;
const SAMPLE_RATE_OFFSET: usize = 4;
const HEAD_OFFSET: usize = 8;
const TAIL_OFFSET: usize = 16;
const DROPPED_OFFSET: usize = 24;

/// Lightweight mapping retained by the native audio callback.
pub struct CaptureTap {
    shared: CaptureShared,
    point: CapturePoint,
}

impl CaptureTap {
    pub fn open(point: CapturePoint) -> io::Result<Self> {
        Ok(Self {
            shared: CaptureShared::open()?,
            point,
        })
    }

    /// Copy a raw stereo block into the ring for this tap's point.
    /// Never waits, allocates, locks, or performs a syscall. If the writer
    /// cannot keep up, the whole block is dropped and reported in the UI.
    pub fn push(&mut self, channel: usize, samples: &[f32], sample_rate: u32) {
        self.shared
            .push(slot_of(self.point, channel), samples, sample_rate);
    }
}

#[derive(Clone, Debug, Default)]
pub struct AudioCaptureStatus {
    pub active: bool,
    pub path: Option<PathBuf>,
    pub sample_rate: u32,
    pub frames_written: u64,
    pub dropped_frames: u64,
    pub error: Option<String>,
}

/// UI-side controller for one deck's recording session, at one tap point.
#[derive(Clone)]
pub struct AudioCaptureControl {
    channel: usize,
    point: CapturePoint,
    status: Arc<Mutex<AudioCaptureStatus>>,
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
    stop_requested: Arc<AtomicBool>,
}

impl AudioCaptureControl {
    pub fn new(instance_id: u32, point: CapturePoint) -> Self {
        Self {
            channel: instance_id.saturating_sub(1) as usize,
            point,
            status: Arc::new(Mutex::new(AudioCaptureStatus::default())),
            worker: Arc::new(Mutex::new(None)),
            stop_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The ring this tap records into.
    fn slot(&self) -> usize {
        slot_of(self.point, self.channel)
    }

    /// What this tap is called, for the dialog and the panel.
    pub fn point_label(&self) -> &'static str {
        match self.point {
            CapturePoint::PreMixer => "pre-mixer",
            CapturePoint::PostMixer => "post-mixer",
        }
    }

    pub fn status(&self) -> AudioCaptureStatus {
        self.reap_finished();
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Show a save dialog and begin a WAV recording if the user chooses a
    /// destination. Returns `Ok(false)` when the dialog is cancelled.
    pub fn choose_and_start(&self) -> Result<bool, String> {
        let suggested = format!(
            "player-{}-{}-capture.wav",
            self.channel + 1,
            self.point_label()
        );
        let Some(path) = rfd::FileDialog::new()
            .set_title(&format!("Save {} audio capture", self.point_label()))
            .set_file_name(&suggested)
            .add_filter("WAV audio", &["wav"])
            .save_file()
        else {
            return Ok(false);
        };
        let path = if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
        {
            path
        } else {
            path.with_extension("wav")
        };
        if let Err(error) = self.start(path) {
            if let Ok(mut status) = self.status.lock() {
                status.active = false;
                status.error = Some(error.clone());
            }
            return Err(error);
        }
        Ok(true)
    }

    pub fn start(&self, path: PathBuf) -> Result<(), String> {
        if self.channel >= CHANNEL_COUNT {
            return Err("this player has no native-mixer channel".to_owned());
        }
        self.reap_finished();
        if self.status().active {
            return Err("capture is already running".to_owned());
        }
        let mut worker_slot = self
            .worker
            .lock()
            .map_err(|_| "capture worker lock failed")?;

        let mut shared = CaptureShared::open().map_err(|e| e.to_string())?;
        // Quiesce an abandoned or previous writer before reusing its ring.
        if shared.set_active(self.slot(), false) {
            std::thread::sleep(Duration::from_millis(50));
        }
        shared.reset(self.slot());
        self.stop_requested.store(false, Ordering::Release);

        let file = File::create(&path).map_err(|e| format!("could not create capture: {e}"))?;
        let fallback_rate = MixerShared::open()
            .map(|m| m.source_rate(self.channel))
            .unwrap_or(0);
        let status = Arc::clone(&self.status);
        if let Ok(mut current) = status.lock() {
            *current = AudioCaptureStatus {
                active: true,
                path: Some(path.clone()),
                ..AudioCaptureStatus::default()
            };
        }
        shared.set_active(self.slot(), true);

        let slot = self.slot();
        let point_label = self.point_label();
        let channel = self.channel;
        let failure_status = Arc::clone(&self.status);
        let stop_requested = Arc::clone(&self.stop_requested);
        let thread = std::thread::Builder::new()
            .name(format!("audio-capture-{point_label}-player-{}", channel + 1))
            .spawn(move || {
                let result = write_capture(
                    &mut shared,
                    slot,
                    file,
                    fallback_rate,
                    &status,
                    &stop_requested,
                );
                shared.set_active(slot, false);
                if let Ok(mut current) = status.lock() {
                    current.active = false;
                    current.dropped_frames = shared.dropped_frames(slot);
                    if let Err(error) = result {
                        current.error = Some(error.to_string());
                    }
                }
            })
            .map_err(|e| {
                let _ = CaptureShared::open().map(|s| s.set_active(self.channel, false));
                let error = format!("could not start capture writer: {e}");
                if let Ok(mut current) = failure_status.lock() {
                    current.active = false;
                    current.error = Some(error.clone());
                }
                error
            })?;
        *worker_slot = Some(thread);
        Ok(())
    }

    pub fn stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
        if let Ok(shared) = CaptureShared::open() {
            shared.set_active(self.slot(), false);
        }
        if let Ok(mut worker) = self.worker.lock() {
            if let Some(thread) = worker.take() {
                let _ = thread.join();
            }
        }
    }

    fn reap_finished(&self) {
        if let Ok(mut worker) = self.worker.lock() {
            if worker.as_ref().is_some_and(|thread| thread.is_finished()) {
                if let Some(thread) = worker.take() {
                    let _ = thread.join();
                }
            }
        }
    }
}

fn write_capture(
    shared: &mut CaptureShared,
    slot: usize,
    file: File,
    fallback_rate: u32,
    status: &Mutex<AudioCaptureStatus>,
    stop_requested: &AtomicBool,
) -> io::Result<()> {
    let mut file = Some(file);
    let mut wav: Option<WavWriter> = None;
    let mut scratch = vec![0.0_f32; 8192];

    loop {
        if stop_requested.load(Ordering::Acquire) {
            shared.set_active(slot, false);
        }
        let active = shared.active(slot);
        let available = shared.available_frames(slot);
        if !active && available == 0 {
            break;
        }
        let rate = shared.sample_rate(slot);
        if wav.is_none() && rate != 0 {
            wav = Some(WavWriter::begin(file.take().unwrap(), rate)?);
        }
        if let Some(writer) = &wav {
            if rate != 0 && rate != writer.sample_rate {
                return Err(io::Error::other(format!(
                    "output sample rate changed from {} Hz to {rate} Hz during capture",
                    writer.sample_rate
                )));
            }
        }

        let frames = shared.read(slot, &mut scratch);
        if frames == 0 {
            std::thread::sleep(Duration::from_millis(4));
            continue;
        }
        let writer = wav
            .as_mut()
            .expect("capture rate is published before samples");
        writer.write_samples(&scratch[..frames * 2])?;
        if let Ok(mut current) = status.lock() {
            current.sample_rate = writer.sample_rate;
            current.frames_written = writer.frames_written;
            current.dropped_frames = shared.dropped_frames(slot);
        }
    }

    let mut writer = match wav {
        Some(writer) => writer,
        None => WavWriter::begin(file.take().unwrap(), fallback_rate.max(48_000))?,
    };
    writer.finish()?;
    if let Ok(mut current) = status.lock() {
        current.sample_rate = writer.sample_rate;
        current.frames_written = writer.frames_written;
    }
    Ok(())
}

struct WavWriter {
    file: File,
    sample_rate: u32,
    frames_written: u64,
}

impl WavWriter {
    fn begin(mut file: File, sample_rate: u32) -> io::Result<Self> {
        write_wav_header(&mut file, sample_rate, 0)?;
        Ok(Self {
            file,
            sample_rate,
            frames_written: 0,
        })
    }

    fn write_samples(&mut self, samples: &[f32]) -> io::Result<()> {
        let new_frames = samples.len() as u64 / 2;
        let new_total = self.frames_written.saturating_add(new_frames);
        if new_total.saturating_mul(8) > u32::MAX as u64 {
            return Err(io::Error::other(
                "WAV capture reached the 4 GiB format limit",
            ));
        }
        let mut bytes = [0_u8; 4096];
        for chunk in samples.chunks(bytes.len() / 4) {
            for (sample, destination) in chunk.iter().zip(bytes.chunks_exact_mut(4)) {
                destination.copy_from_slice(&sample.to_le_bytes());
            }
            self.file.write_all(&bytes[..chunk.len() * 4])?;
        }
        self.frames_written = new_total;
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        write_wav_header(&mut self.file, self.sample_rate, self.frames_written)?;
        self.file.flush()
    }
}

fn write_wav_header<W: Write + Seek>(
    file: &mut W,
    sample_rate: u32,
    frames: u64,
) -> io::Result<()> {
    let data_bytes = u32::try_from(frames.saturating_mul(8))
        .map_err(|_| io::Error::other("WAV capture reached the 4 GiB format limit"))?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(b"RIFF")?;
    file.write_all(&(36_u32.saturating_add(data_bytes)).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16_u32.to_le_bytes())?;
    file.write_all(&3_u16.to_le_bytes())?; // WAVE_FORMAT_IEEE_FLOAT
    file.write_all(&2_u16.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&sample_rate.saturating_mul(8).to_le_bytes())?;
    file.write_all(&8_u16.to_le_bytes())?;
    file.write_all(&32_u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())
}

struct CaptureShared {
    mmap: MmapMut,
}

impl CaptureShared {
    fn open() -> io::Result<Self> {
        runtime_paths::ensure_runtime_base_dir()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(runtime_paths::capture_shm_path())?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        file.set_len(CAPTURE_BYTES as u64)?;
        let mut mmap = unsafe { MmapOptions::new().len(CAPTURE_BYTES).map_mut(&file)? };
        let initialized = read_u32(&mmap, MAGIC_OFFSET) == CAPTURE_MAGIC
            && read_u32(&mmap, VERSION_OFFSET) == CAPTURE_VERSION;
        if !initialized {
            mmap.fill(0);
            write_u32(&mut mmap, VERSION_OFFSET, CAPTURE_VERSION);
            write_u32(&mut mmap, MAGIC_OFFSET, CAPTURE_MAGIC);
            mmap.flush_async()?;
        }
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        Ok(Self { mmap })
    }

    fn active(&self, slot: usize) -> bool {
        slot < SLOTS
            && self
                .atomic_u32(slot_header_offset(slot) + ACTIVE_OFFSET)
                .load(Ordering::Acquire)
                != 0
    }

    /// Returns the previous active state.
    fn set_active(&self, slot: usize, active: bool) -> bool {
        if slot >= SLOTS {
            return false;
        }
        self.atomic_u32(slot_header_offset(slot) + ACTIVE_OFFSET)
            .swap(u32::from(active), Ordering::AcqRel)
            != 0
    }

    fn reset(&mut self, slot: usize) {
        if slot >= SLOTS {
            return;
        }
        let base = slot_header_offset(slot);
        self.atomic_u32(base + SAMPLE_RATE_OFFSET)
            .store(0, Ordering::Release);
        self.atomic_u64(base + HEAD_OFFSET)
            .store(0, Ordering::Release);
        self.atomic_u64(base + TAIL_OFFSET)
            .store(0, Ordering::Release);
        self.atomic_u64(base + DROPPED_OFFSET)
            .store(0, Ordering::Release);
    }

    fn sample_rate(&self, slot: usize) -> u32 {
        if slot >= SLOTS {
            return 0;
        }
        self.atomic_u32(slot_header_offset(slot) + SAMPLE_RATE_OFFSET)
            .load(Ordering::Acquire)
    }

    fn available_frames(&self, slot: usize) -> usize {
        if slot >= SLOTS {
            return 0;
        }
        let base = slot_header_offset(slot);
        let head = self.atomic_u64(base + HEAD_OFFSET).load(Ordering::Acquire);
        let tail = self.atomic_u64(base + TAIL_OFFSET).load(Ordering::Relaxed);
        head.saturating_sub(tail) as usize / 8
    }

    fn dropped_frames(&self, slot: usize) -> u64 {
        if slot >= SLOTS {
            return 0;
        }
        self.atomic_u64(slot_header_offset(slot) + DROPPED_OFFSET)
            .load(Ordering::Acquire)
    }

    fn push(&mut self, slot: usize, samples: &[f32], sample_rate: u32) {
        if slot >= SLOTS || samples.len() < 2 || !self.active(slot) {
            return;
        }
        let frames = samples.len() / 2;
        let bytes_requested = frames * 8;
        if bytes_requested > RING_BYTES {
            self.atomic_u64(slot_header_offset(slot) + DROPPED_OFFSET)
                .fetch_add(frames as u64, Ordering::Relaxed);
            return;
        }
        let base = slot_header_offset(slot);
        let head = self.atomic_u64(base + HEAD_OFFSET).load(Ordering::Relaxed);
        let tail = self.atomic_u64(base + TAIL_OFFSET).load(Ordering::Acquire);
        if head.saturating_sub(tail) as usize + bytes_requested > RING_BYTES {
            self.atomic_u64(base + DROPPED_OFFSET)
                .fetch_add(frames as u64, Ordering::Relaxed);
            return;
        }
        let ring = slot_ring_offset(slot);
        let offset = (head as usize) & (RING_BYTES - 1);
        let first = bytes_requested.min(RING_BYTES - offset);
        // SAFETY: the mapping is writable, both ring regions are in bounds,
        // and this channel has exactly one producer (the native callback).
        unsafe {
            std::ptr::copy_nonoverlapping(
                samples.as_ptr().cast::<u8>(),
                self.mmap.as_ptr().cast_mut().add(ring + offset),
                first,
            );
            if bytes_requested > first {
                std::ptr::copy_nonoverlapping(
                    samples.as_ptr().cast::<u8>().add(first),
                    self.mmap.as_ptr().cast_mut().add(ring),
                    bytes_requested - first,
                );
            }
        }
        self.atomic_u32(base + SAMPLE_RATE_OFFSET)
            .store(sample_rate, Ordering::Release);
        self.atomic_u64(base + HEAD_OFFSET)
            .store(head + bytes_requested as u64, Ordering::Release);
    }

    fn read(&mut self, slot: usize, output: &mut [f32]) -> usize {
        if slot >= SLOTS || output.len() < 2 {
            return 0;
        }
        let available = self.available_frames(slot);
        let frames = available.min(output.len() / 2);
        if frames == 0 {
            return 0;
        }
        let bytes_requested = frames * 8;
        let base = slot_header_offset(slot);
        let tail_atomic = self.atomic_u64(base + TAIL_OFFSET);
        let tail = tail_atomic.load(Ordering::Relaxed);
        let ring = slot_ring_offset(slot);
        let offset = (tail as usize) & (RING_BYTES - 1);
        let first = bytes_requested.min(RING_BYTES - offset);
        // SAFETY: the producer publishes head only after writing, and these
        // copies are bounded by the available byte count and output slice.
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
        frames
    }

    fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        debug_assert_eq!(offset % std::mem::align_of::<AtomicU32>(), 0);
        unsafe { &*(self.mmap.as_ptr().add(offset).cast::<AtomicU32>()) }
    }

    fn atomic_u64(&self, offset: usize) -> &AtomicU64 {
        debug_assert_eq!(offset % std::mem::align_of::<AtomicU64>(), 0);
        unsafe { &*(self.mmap.as_ptr().add(offset).cast::<AtomicU64>()) }
    }
}

const fn slot_header_offset(slot: usize) -> usize {
    HEADER_BYTES + slot * CHANNEL_BYTES
}

const fn slot_ring_offset(slot: usize) -> usize {
    RING_BASE + slot * RING_BYTES
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
    use std::io::Cursor;

    #[test]
    fn wav_header_describes_float32_stereo_audio() {
        let mut bytes = Cursor::new(Vec::new());
        write_wav_header(&mut bytes, 96_000, 960).unwrap();
        let bytes = bytes.into_inner();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 3);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            96_000
        );
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 32);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 7_680);
    }
}
