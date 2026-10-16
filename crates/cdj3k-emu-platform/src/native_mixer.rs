//! Singleton host-side four-channel mixer.
//!
//! Every QEMU process publishes raw F32 stereo into its own lock-free mmap
//! ring. Exactly one application process wins an advisory lock and owns this
//! CoreAudio output unit; all channel DSP and summing happens in that single
//! render callback.

#![cfg(target_os = "macos")]

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::audio_capture::{CapturePoint, CaptureTap};
use crate::audio_devices;
use crate::mixer::{MixerParameter, MixerShared, CHANNEL_COUNT};
use crate::runtime_paths;

type OSStatus = i32;
type AudioComponent = *mut c_void;
type AudioUnit = *mut c_void;

const fn fourcc(value: &[u8; 4]) -> u32 {
    ((value[0] as u32) << 24)
        | ((value[1] as u32) << 16)
        | ((value[2] as u32) << 8)
        | value[3] as u32
}

const AUDIO_UNIT_TYPE_OUTPUT: u32 = fourcc(b"auou");
const AUDIO_UNIT_SUBTYPE_HAL_OUTPUT: u32 = fourcc(b"ahal");
const AUDIO_UNIT_MANUFACTURER_APPLE: u32 = fourcc(b"appl");
const AUDIO_FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm");
const AUDIO_FORMAT_FLAG_FLOAT_PACKED: u32 = 1 | 8;
const AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE: u32 = 2000;
const AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO: u32 = 2003;
const AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
const AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
const AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;
const AUDIO_UNIT_SCOPE_INPUT: u32 = 1;
const AUDIO_UNIT_SCOPE_OUTPUT: u32 = 2;
const MAX_RENDER_FRAMES: usize = 8192;
const MAX_SOURCE_FRAMES: usize = crate::mixer::RING_BYTES / 8;
const CUE_RING_FRAMES: usize = 131_072;

/// One producer (Master callback), one consumer (the cue device callback).
/// Full rings discard fresh cue frames instead of blocking either audio thread.
struct CueRing {
    samples: UnsafeCell<Box<[[f32; 2]]>>,
    head: AtomicUsize,
    tail: AtomicUsize,
    active: AtomicBool,
}

unsafe impl Sync for CueRing {}

impl CueRing {
    fn new() -> Self {
        Self {
            samples: UnsafeCell::new(vec![[0.0; 2]; CUE_RING_FRAMES].into_boxed_slice()),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            active: AtomicBool::new(false),
        }
    }

    fn push(&self, sample: [f32; 2]) {
        let head = self.head.load(Ordering::Relaxed);
        if head.wrapping_sub(self.tail.load(Ordering::Acquire)) >= CUE_RING_FRAMES {
            return;
        }
        unsafe {
            (*self.samples.get())[head % CUE_RING_FRAMES] = sample;
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
    }

    fn pop(&self) -> Option<[f32; 2]> {
        let mut tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        // Independent CoreAudio devices have independent clocks. Never let
        // cue latency grow without bound if its device runs a little slower.
        if head.wrapping_sub(tail) > 4096 {
            tail = head.wrapping_sub(2048);
        }
        let sample = unsafe { (*self.samples.get())[tail % CUE_RING_FRAMES] };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(sample)
    }

    fn depth(&self) -> usize {
        self.head
            .load(Ordering::Acquire)
            .wrapping_sub(self.tail.load(Ordering::Acquire))
    }
}

struct CueRenderState {
    ring: Arc<CueRing>,
    resampler: ChannelResampler,
}

impl CueRenderState {
    fn render(&mut self, output: &mut [f32]) {
        let depth_error = self.ring.depth() as f64 - 1024.0;
        let clock_correction = (1.0 + depth_error * 0.000_002).clamp(0.99, 1.01);
        let ratio = self.resampler.input_rate as f64 / self.resampler.output_rate.max(1) as f64
            * clock_correction;
        if !self.resampler.initialized {
            for _ in 0..4 {
                self.resampler.push(self.ring.pop().unwrap_or([0.0; 2]));
            }
            self.resampler.initialized = true;
        }
        for pair in output.chunks_exact_mut(2) {
            let t = self.resampler.phase as f32;
            let t2 = t * t;
            let t3 = t2 * t;
            let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
            let h10 = t3 - 2.0 * t2 + t;
            let h01 = -2.0 * t3 + 3.0 * t2;
            let h11 = t3 - t2;
            for side in 0..2 {
                let [y0, y1, y2, y3] = self.resampler.window.map(|frame| frame[side]);
                pair[side] = (h00 * y1 + h10 * 0.5 * (y2 - y0) + h01 * y2 + h11 * 0.5 * (y3 - y1))
                    .clamp(-1.0, 1.0);
            }
            self.resampler.phase += ratio;
            while self.resampler.phase >= 1.0 {
                self.resampler.phase -= 1.0;
                self.resampler.push(self.ring.pop().unwrap_or([0.0; 2]));
            }
        }
    }
}

#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

type RenderCallback = unsafe extern "C" fn(
    *mut c_void,
    *mut u32,
    *const c_void,
    u32,
    u32,
    *mut AudioBufferList,
) -> OSStatus;

#[repr(C)]
struct AudioUnitRenderCallback {
    callback: Option<RenderCallback>,
    refcon: *mut c_void,
}

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "AudioUnit", kind = "framework")]
extern "C" {
    fn AudioComponentFindNext(
        component: AudioComponent,
        description: *const AudioComponentDescription,
    ) -> AudioComponent;
    fn AudioComponentInstanceNew(component: AudioComponent, instance: *mut AudioUnit) -> OSStatus;
    fn AudioComponentInstanceDispose(instance: AudioUnit) -> OSStatus;
    fn AudioUnitSetProperty(
        unit: AudioUnit,
        property: u32,
        scope: u32,
        element: u32,
        data: *const c_void,
        data_size: u32,
    ) -> OSStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OSStatus;
    fn AudioOutputUnitStart(unit: AudioUnit) -> OSStatus;
    fn AudioOutputUnitStop(unit: AudioUnit) -> OSStatus;
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectSetPropertyData(
        object: u32,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: u32,
        data: *const c_void,
    ) -> OSStatus;
}

pub struct NativeMixerService {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl NativeMixerService {
    /// Start a background contender for the per-user mixer lock. If another
    /// player owns audio, this process waits and takes over automatically if
    /// that owner exits.
    pub fn start(
        device_uid: Option<String>,
        cue_a_uid: Option<String>,
        cue_b_uid: Option<String>,
        buffer_us: u32,
    ) -> io::Result<Self> {
        runtime_paths::ensure_runtime_base_dir()?;
        let readiness = MixerShared::open()?;
        let first_configuration = !runtime_paths::mixer_device_path().exists();
        // The persisted selection is application-wide. Refresh the runtime
        // request on every launch so a stale file from a previous session can
        // never disagree with the device shown in the menu.
        write_device_request(device_uid.as_deref())?;
        write_cue_device_request('A', cue_a_uid.as_deref())?;
        write_cue_device_request('B', cue_b_uid.as_deref())?;
        if first_configuration {
            readiness.request_buffer_us(buffer_us);
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(runtime_paths::mixer_lock_path())?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("native-mixer-owner".to_owned())
            .spawn(move || owner_loop(lock, device_uid, thread_stop))?;
        let service = Self {
            stop,
            thread: Some(thread),
        };
        // Give the elected owner time to publish readiness before the player
        // starts producing. Ring attachment itself is unconditional, but this
        // avoids filling the ring before a native consumer exists.
        for _ in 0..300 {
            if readiness.output_rate() != 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(service)
    }
}

/// Select the one output used by the shared mixer. This is intentionally
/// global: independently selecting an output per deck would recreate the
/// multiple-clock architecture this service replaces.
pub fn request_output_device(device_uid: Option<&str>) -> io::Result<()> {
    write_device_request(device_uid)?;
    MixerShared::open()?.request_device_reopen();
    Ok(())
}

/// Read the live application-wide output request so every player's menu can
/// mirror a device change made from any other player process.
pub fn requested_output_device() -> io::Result<Option<String>> {
    let value = std::fs::read_to_string(runtime_paths::mixer_device_path())?;
    let value = value.trim();
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

pub fn request_cue_output_device(bus: char, device_uid: Option<&str>) -> io::Result<()> {
    write_cue_device_request(bus, device_uid)?;
    MixerShared::open()?.request_device_reopen();
    Ok(())
}

pub fn requested_cue_output_device(bus: char) -> io::Result<Option<String>> {
    let value = std::fs::read_to_string(runtime_paths::mixer_cue_device_path(bus))?;
    let value = value.trim();
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

fn write_cue_device_request(bus: char, device_uid: Option<&str>) -> io::Result<()> {
    if !matches!(bus, 'A' | 'B') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid cue bus",
        ));
    }
    runtime_paths::ensure_runtime_base_dir()?;
    let destination = runtime_paths::mixer_cue_device_path(bus);
    let temporary = destination.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, device_uid.unwrap_or_default().as_bytes())?;
    std::fs::rename(temporary, destination)
}

pub fn request_audio_reset() -> io::Result<()> {
    MixerShared::open()?.request_audio_reset();
    Ok(())
}

pub fn request_latency(buffer_us: u32) -> io::Result<()> {
    MixerShared::open()?.request_buffer_us(buffer_us);
    Ok(())
}

fn write_device_request(device_uid: Option<&str>) -> io::Result<()> {
    runtime_paths::ensure_runtime_base_dir()?;
    let destination = runtime_paths::mixer_device_path();
    let temporary = destination.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, device_uid.unwrap_or_default().as_bytes())?;
    std::fs::rename(temporary, destination)
}

fn read_device_request(fallback: Option<&str>) -> Option<String> {
    match std::fs::read_to_string(runtime_paths::mixer_device_path()) {
        Ok(value) => {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_owned())
        }
        Err(_) => fallback.map(ToOwned::to_owned),
    }
}

impl Drop for NativeMixerService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn owner_loop(lock: File, device_uid: Option<String>, stop: Arc<AtomicBool>) {
    use std::os::fd::AsRawFd;
    while !stop.load(Ordering::Acquire) {
        let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !locked {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let shared = match MixerShared::open() {
            Ok(shared) => shared,
            Err(error) => {
                eprintln!("cdj3k-emu: native mixer shared state failed: {error}");
                unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let mut device_generation = shared.device_generation();
        let mut reset_generation = shared.reset_generation();
        while !stop.load(Ordering::Acquire) {
            let selected = read_device_request(device_uid.as_deref());
            // Clear readiness while the requested native device is reopened.
            shared.set_output_rate(0);
            match NativeMixerEngine::start(selected.as_deref(), shared.buffer_us()) {
                Ok(mut engine) => {
                    for (bus, index) in [('A', 0), ('B', 1)] {
                        if let Ok(Some(uid)) = requested_cue_output_device(bus) {
                            if let Err(error) = engine.start_cue(index, &uid, shared.buffer_us()) {
                                eprintln!("cdj3k-emu: Cue {bus} output unavailable: {error}");
                            }
                        }
                    }
                    eprintln!("cdj3k-emu: native mixer owns the shared CoreAudio output");
                    while !stop.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(250));
                        let new_device_generation = shared.device_generation();
                        let new_reset_generation = shared.reset_generation();
                        if new_device_generation != device_generation
                            || new_reset_generation != reset_generation
                        {
                            device_generation = new_device_generation;
                            reset_generation = new_reset_generation;
                            break;
                        }
                    }
                    drop(engine);
                }
                Err(error) => {
                    eprintln!("cdj3k-emu: native mixer start failed: {error}");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        if !stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

struct NativeMixerEngine {
    unit: AudioUnit,
    state: *mut MixerRenderState,
    cue: [Option<CueEngine>; 2],
}

unsafe impl Send for NativeMixerEngine {}

impl NativeMixerEngine {
    fn start(device_uid: Option<&str>, buffer_us: u32) -> io::Result<Self> {
        let (device, rate) = audio_devices::resolve_output_device(device_uid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no CoreAudio output device"))?;
        let description = AudioComponentDescription {
            component_type: AUDIO_UNIT_TYPE_OUTPUT,
            component_sub_type: AUDIO_UNIT_SUBTYPE_HAL_OUTPUT,
            component_manufacturer: AUDIO_UNIT_MANUFACTURER_APPLE,
            component_flags: 0,
            component_flags_mask: 0,
        };
        let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &description) };
        if component.is_null() {
            return Err(io::Error::other("AUHAL component unavailable"));
        }
        let mut unit = std::ptr::null_mut();
        status(
            unsafe { AudioComponentInstanceNew(component, &mut unit) },
            "create AUHAL",
        )?;

        request_device_buffer(device, rate, buffer_us);

        let result = (|| {
            let enabled: u32 = 1;
            set_property(
                unit,
                AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO,
                AUDIO_UNIT_SCOPE_OUTPUT,
                0,
                &enabled,
            )?;
            set_property(
                unit,
                AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE,
                AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &device,
            )?;
            let format = AudioStreamBasicDescription {
                sample_rate: rate as f64,
                format_id: AUDIO_FORMAT_LINEAR_PCM,
                format_flags: AUDIO_FORMAT_FLAG_FLOAT_PACKED,
                bytes_per_packet: 8,
                frames_per_packet: 1,
                bytes_per_frame: 8,
                channels_per_frame: 2,
                bits_per_channel: 32,
                reserved: 0,
            };
            set_property(
                unit,
                AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                AUDIO_UNIT_SCOPE_INPUT,
                0,
                &format,
            )?;

            let shared = MixerShared::open()?;
            let (buffer_frames, safety_frames, device_frames) =
                audio_devices::output_latency_frames(device);
            shared.set_host_latency_frames(buffer_frames, safety_frames, device_frames);
            let state = Box::new(MixerRenderState::new(shared, rate));
            let state_ptr = Box::into_raw(state);
            let callback = AudioUnitRenderCallback {
                callback: Some(render_callback),
                refcon: state_ptr.cast(),
            };
            if let Err(error) = set_property(
                unit,
                AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
                AUDIO_UNIT_SCOPE_INPUT,
                0,
                &callback,
            ) {
                unsafe { drop(Box::from_raw(state_ptr)) };
                return Err(error);
            }
            if let Err(error) = status(unsafe { AudioUnitInitialize(unit) }, "initialize AUHAL") {
                unsafe { drop(Box::from_raw(state_ptr)) };
                return Err(error);
            }
            if let Err(error) = status(unsafe { AudioOutputUnitStart(unit) }, "start AUHAL") {
                unsafe {
                    AudioUnitUninitialize(unit);
                    drop(Box::from_raw(state_ptr));
                }
                return Err(error);
            }
            // Publish readiness last. The guest producer can begin only once
            // the selected output is known to be draining the shared rings.
            unsafe { (*state_ptr).shared.set_output_rate(rate) };
            Ok(state_ptr)
        })();

        match result {
            Ok(state) => Ok(Self {
                unit,
                state,
                cue: [None, None],
            }),
            Err(error) => {
                unsafe { AudioComponentInstanceDispose(unit) };
                Err(error)
            }
        }
    }

    fn start_cue(&mut self, index: usize, uid: &str, buffer_us: u32) -> io::Result<()> {
        let source_rate = unsafe { (*self.state).rate };
        let ring = unsafe { Arc::clone(&(*self.state).cue_rings[index]) };
        self.cue[index] = Some(CueEngine::start(uid, buffer_us, source_rate, ring)?);
        Ok(())
    }
}

struct CueEngine {
    unit: AudioUnit,
    state: *mut CueRenderState,
    ring: Arc<CueRing>,
}

unsafe impl Send for CueEngine {}

impl CueEngine {
    fn start(uid: &str, buffer_us: u32, source_rate: f32, ring: Arc<CueRing>) -> io::Result<Self> {
        let (device, rate) = audio_devices::resolve_output_device(Some(uid)).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "cue output device not found")
        })?;
        let description = AudioComponentDescription {
            component_type: AUDIO_UNIT_TYPE_OUTPUT,
            component_sub_type: AUDIO_UNIT_SUBTYPE_HAL_OUTPUT,
            component_manufacturer: AUDIO_UNIT_MANUFACTURER_APPLE,
            component_flags: 0,
            component_flags_mask: 0,
        };
        let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &description) };
        if component.is_null() {
            return Err(io::Error::other("AUHAL component unavailable"));
        }
        let mut unit = std::ptr::null_mut();
        status(
            unsafe { AudioComponentInstanceNew(component, &mut unit) },
            "create cue AUHAL",
        )?;
        request_device_buffer(device, rate, buffer_us);
        let result = (|| {
            let enabled: u32 = 1;
            set_property(
                unit,
                AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO,
                AUDIO_UNIT_SCOPE_OUTPUT,
                0,
                &enabled,
            )?;
            set_property(
                unit,
                AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE,
                AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &device,
            )?;
            let format = AudioStreamBasicDescription {
                sample_rate: rate as f64,
                format_id: AUDIO_FORMAT_LINEAR_PCM,
                format_flags: AUDIO_FORMAT_FLAG_FLOAT_PACKED,
                bytes_per_packet: 8,
                frames_per_packet: 1,
                bytes_per_frame: 8,
                channels_per_frame: 2,
                bits_per_channel: 32,
                reserved: 0,
            };
            set_property(
                unit,
                AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                AUDIO_UNIT_SCOPE_INPUT,
                0,
                &format,
            )?;
            let mut resampler = ChannelResampler::default();
            resampler.configure(source_rate as u32, rate);
            let state = Box::into_raw(Box::new(CueRenderState { ring, resampler }));
            let callback = AudioUnitRenderCallback {
                callback: Some(cue_render_callback),
                refcon: state.cast(),
            };
            if let Err(error) = set_property(
                unit,
                AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
                AUDIO_UNIT_SCOPE_INPUT,
                0,
                &callback,
            ) {
                unsafe {
                    drop(Box::from_raw(state));
                }
                return Err(error);
            }
            if let Err(error) = status(unsafe { AudioUnitInitialize(unit) }, "initialize cue AUHAL")
            {
                unsafe {
                    drop(Box::from_raw(state));
                }
                return Err(error);
            }
            if let Err(error) = status(unsafe { AudioOutputUnitStart(unit) }, "start cue AUHAL") {
                unsafe {
                    AudioUnitUninitialize(unit);
                    drop(Box::from_raw(state));
                }
                return Err(error);
            }
            Ok(state)
        })();
        match result {
            Ok(state) => {
                let ring = unsafe { Arc::clone(&(*state).ring) };
                ring.active.store(true, Ordering::Release);
                Ok(Self { unit, state, ring })
            }
            Err(error) => {
                unsafe { AudioComponentInstanceDispose(unit) };
                Err(error)
            }
        }
    }
}

impl Drop for CueEngine {
    fn drop(&mut self) {
        self.ring.active.store(false, Ordering::Release);
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            drop(Box::from_raw(self.state));
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

impl Drop for NativeMixerEngine {
    fn drop(&mut self) {
        self.cue = [None, None];
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            if !self.state.is_null() {
                (*self.state).shared.set_output_rate(0);
                (*self.state).shared.set_host_latency_frames(0, 0, 0);
                drop(Box::from_raw(self.state));
            }
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

fn set_property<T>(
    unit: AudioUnit,
    property: u32,
    scope: u32,
    element: u32,
    value: &T,
) -> io::Result<()> {
    status(
        unsafe {
            AudioUnitSetProperty(
                unit,
                property,
                scope,
                element,
                (value as *const T).cast(),
                std::mem::size_of::<T>() as u32,
            )
        },
        "set AudioUnit property",
    )
}

fn status(value: OSStatus, operation: &str) -> io::Result<()> {
    if value == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!("{operation}: OSStatus {value}")))
    }
}

fn request_device_buffer(device: u32, rate: u32, buffer_us: u32) {
    let frames =
        (((rate as u64 * buffer_us as u64) + 500_000) / 1_000_000).clamp(1, u32::MAX as u64) as u32;
    let address = AudioObjectPropertyAddress {
        selector: fourcc(b"fsiz"),
        scope: fourcc(b"outp"),
        element: 0,
    };
    let result = unsafe {
        AudioObjectSetPropertyData(
            device,
            &address,
            0,
            std::ptr::null(),
            std::mem::size_of::<u32>() as u32,
            (&frames as *const u32).cast(),
        )
    };
    if result != 0 {
        eprintln!(
            "cdj3k-emu: CoreAudio device rejected requested {buffer_us} us buffer ({frames} frames): OSStatus {result}"
        );
    }
}

#[derive(Clone, Copy)]
struct ChannelResampler {
    window: [[f32; 2]; 4],
    phase: f64,
    initialized: bool,
    input_rate: u32,
    output_rate: u32,
    integrator: f64,
    biquad: [f32; 5],
    biquad_state: [[f32; 2]; 2],
}

impl Default for ChannelResampler {
    fn default() -> Self {
        Self {
            window: [[0.0; 2]; 4],
            phase: 0.0,
            initialized: false,
            input_rate: 0,
            output_rate: 0,
            integrator: 0.0,
            biquad: [0.0; 5],
            biquad_state: [[0.0; 2]; 2],
        }
    }
}

impl ChannelResampler {
    fn configure(&mut self, input_rate: u32, output_rate: u32) {
        if self.input_rate == input_rate && self.output_rate == output_rate {
            return;
        }
        *self = Self::default();
        self.input_rate = input_rate;
        self.output_rate = output_rate;
        if input_rate > output_rate && output_rate > 0 {
            let cutoff = 0.46 * output_rate as f64;
            let omega = 2.0 * std::f64::consts::PI * cutoff / input_rate as f64;
            let cos = omega.cos();
            let sin = omega.sin();
            let alpha = sin / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
            let a0 = 1.0 + alpha;
            self.biquad = [
                ((1.0 - cos) * 0.5 / a0) as f32,
                ((1.0 - cos) / a0) as f32,
                ((1.0 - cos) * 0.5 / a0) as f32,
                (-2.0 * cos / a0) as f32,
                ((1.0 - alpha) / a0) as f32,
            ];
        }
    }

    fn ratio(&mut self, ring_frames: usize, target_frames: u32) -> f64 {
        let target = target_frames.max(1) as f64;
        let error = (ring_frames as f64 - target) / target;
        self.integrator = (self.integrator + error * 1.0e-6).clamp(-0.005, 0.005);
        self.input_rate as f64 / self.output_rate.max(1) as f64 * (1.0 + self.integrator)
    }

    fn source_frames_needed(&self, output_frames: usize, ratio: f64) -> usize {
        usize::from(!self.initialized) * 4
            + (self.phase + ratio * output_frames as f64).floor() as usize
    }

    fn filter(&mut self, mut frame: [f32; 2]) -> [f32; 2] {
        if self.input_rate <= self.output_rate {
            return frame;
        }
        let [b0, b1, b2, a1, a2] = self.biquad;
        for side in 0..2 {
            let value = b0 * frame[side] + self.biquad_state[side][0];
            self.biquad_state[side][0] = b1 * frame[side] - a1 * value + self.biquad_state[side][1];
            self.biquad_state[side][1] = b2 * frame[side] - a2 * value;
            frame[side] = value;
        }
        frame
    }

    fn push(&mut self, frame: [f32; 2]) {
        self.window[0] = self.window[1];
        self.window[1] = self.window[2];
        self.window[2] = self.window[3];
        self.window[3] = self.filter(frame);
    }

    fn render(&mut self, source: &[f32], output: &mut [f32], frames: usize, ratio: f64) {
        let mut source_frame = 0;
        if !self.initialized {
            for _ in 0..4 {
                self.push([source[source_frame * 2], source[source_frame * 2 + 1]]);
                source_frame += 1;
            }
            self.initialized = true;
        }
        for frame in 0..frames {
            let t = self.phase as f32;
            let t2 = t * t;
            let t3 = t2 * t;
            let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
            let h10 = t3 - 2.0 * t2 + t;
            let h01 = -2.0 * t3 + 3.0 * t2;
            let h11 = t3 - t2;
            for side in 0..2 {
                let y0 = self.window[0][side];
                let y1 = self.window[1][side];
                let y2 = self.window[2][side];
                let y3 = self.window[3][side];
                output[frame * 2 + side] =
                    h00 * y1 + h10 * 0.5 * (y2 - y0) + h01 * y2 + h11 * 0.5 * (y3 - y1);
            }
            self.phase += ratio;
            while self.phase >= 1.0 {
                self.phase -= 1.0;
                self.push([source[source_frame * 2], source[source_frame * 2 + 1]]);
                source_frame += 1;
            }
        }
    }
}

#[derive(Clone, Copy)]
struct ChannelDsp {
    resampler: ChannelResampler,
    low_state: [f32; 2],
    high_state: [f32; 2],
    filter_state: [f32; 2],
    trim_gain: f32,
    high_gain: f32,
    mid_gain: f32,
    low_gain: f32,
    fader_gain: f32,
    crossfader_gain: f32,
    filter_alpha: f32,
    filter_mode: i8,
}

impl Default for ChannelDsp {
    fn default() -> Self {
        Self {
            resampler: ChannelResampler::default(),
            low_state: [0.0; 2],
            high_state: [0.0; 2],
            filter_state: [0.0; 2],
            trim_gain: 1.0,
            high_gain: 1.0,
            mid_gain: 1.0,
            low_gain: 1.0,
            fader_gain: 1.0,
            crossfader_gain: 1.0,
            filter_alpha: 1.0,
            filter_mode: 0,
        }
    }
}

struct MixerRenderState {
    shared: MixerShared,
    /// The guest's output, before the rate conversion and the DSP.
    capture_pre: Option<CaptureTap>,
    /// The mixer's own output, after all of it. Both together are what makes an
    /// artefact attributable: something wrong in the first is the deck's, and
    /// something wrong only in the second was made here.
    capture_post: Option<CaptureTap>,
    rate: f32,
    channels: [ChannelDsp; CHANNEL_COUNT],
    source_scratch: Vec<f32>,
    channel_scratch: Vec<f32>,
    fx: [FxDsp; CHANNEL_COUNT + 1],
    master_gain: f32,
    cue_rings: [Arc<CueRing>; 2],
    cue_scratch: [Vec<f32>; 2],
}

impl MixerRenderState {
    fn new(shared: MixerShared, rate: u32) -> Self {
        let capture_pre = CaptureTap::open(CapturePoint::PreMixer)
            .map_err(|error| eprintln!("cdj3k-emu: pre-mixer capture unavailable: {error}"))
            .ok();
        let capture_post = CaptureTap::open(CapturePoint::PostMixer)
            .map_err(|error| eprintln!("cdj3k-emu: post-mixer capture unavailable: {error}"))
            .ok();
        Self {
            shared,
            capture_pre,
            capture_post,
            rate: rate as f32,
            channels: [ChannelDsp::default(); CHANNEL_COUNT],
            source_scratch: vec![0.0; MAX_SOURCE_FRAMES * 2],
            channel_scratch: vec![0.0; MAX_RENDER_FRAMES * 2],
            fx: std::array::from_fn(|_| FxDsp::new(rate)),
            master_gain: 1.0,
            cue_rings: std::array::from_fn(|_| Arc::new(CueRing::new())),
            cue_scratch: std::array::from_fn(|_| vec![0.0; MAX_RENDER_FRAMES * 2]),
        }
    }

    fn render(&mut self, output: &mut [f32], frames: usize) {
        output.fill(0.0);
        if frames == 0 || frames > MAX_RENDER_FRAMES || output.len() < frames * 2 {
            return;
        }
        let cue_active = std::array::from_fn::<_, 2, _>(|bus| {
            self.cue_rings[bus].active.load(Ordering::Acquire)
        });
        for (bus, cue) in self.cue_scratch.iter_mut().enumerate() {
            if cue_active[bus] {
                cue[..frames * 2].fill(0.0);
            }
        }
        let crossfader = self.shared.crossfader();
        let fx_enabled = self.shared.fx_enabled();
        let fx_type = self.shared.fx_type();
        let fx_channel = self.shared.fx_channel();
        let fx_depth = self.shared.fx_depth();
        let fx_bands = self.shared.fx_bands();
        let mut processed = [false; CHANNEL_COUNT];
        for channel in 0..CHANNEL_COUNT {
            let source_rate = self.shared.source_rate(channel);
            if source_rate == 0 {
                self.shared.set_peaks(channel, [0.0, 0.0]);
                continue;
            }
            let resampler = &mut self.channels[channel].resampler;
            resampler.configure(source_rate, self.rate as u32);
            let ring_frames = self.shared.available_audio_frames(channel);
            let target_frames = self.shared.target_audio_frames(channel);
            let ratio = resampler.ratio(ring_frames, target_frames);
            let source_frames = resampler.source_frames_needed(frames, ratio);
            if source_frames == 0 || source_frames > MAX_SOURCE_FRAMES {
                self.shared.set_peaks(channel, [0.0, 0.0]);
                continue;
            }
            let source = &mut self.source_scratch[..source_frames * 2];
            if self.shared.read_audio(channel, source) != source_frames {
                self.shared.set_peaks(channel, [0.0, 0.0]);
                continue;
            }
            if let Some(capture) = &mut self.capture_pre {
                capture.push(channel, source, source_rate);
            }
            let input = &mut self.channel_scratch[..frames * 2];
            resampler.render(source, input, frames, ratio);
            let (cue_a, cue_b) = self.cue_scratch.split_at_mut(1);
            process_channel(
                &self.shared,
                channel,
                &mut self.channels[channel],
                self.rate,
                crossfader,
                if fx_channel == channel as u32 {
                    Some(&mut self.fx[channel])
                } else {
                    None
                },
                fx_type,
                fx_depth,
                fx_bands,
                fx_enabled,
                input,
                output,
                &mut cue_a[0],
                &mut cue_b[0],
                cue_active,
                frames,
            );
            processed[channel] = true;
        }
        if (fx_channel as usize) < CHANNEL_COUNT && !processed[fx_channel as usize] {
            let effect = &mut self.fx[fx_channel as usize];
            for frame in 0..frames {
                let wet = effect.process([0.0; 2], fx_type, fx_depth, fx_bands, self.rate, false);
                output[frame * 2] += wet[0];
                output[frame * 2 + 1] += wet[1];
            }
        }
        let target_master = master_level_gain(self.shared.master_level());
        let master_step = (target_master - self.master_gain) / frames as f32;
        let mut peaks = [0.0_f32; 2];
        for frame in 0..frames {
            let mut pair = [output[frame * 2], output[frame * 2 + 1]];
            if fx_channel == CHANNEL_COUNT as u32 {
                pair = self.fx[CHANNEL_COUNT]
                    .process(pair, fx_type, fx_depth, fx_bands, self.rate, fx_enabled);
            }
            if cue_active[0] && self.shared.master_cue() {
                self.cue_scratch[0][frame * 2] += pair[0];
                self.cue_scratch[0][frame * 2 + 1] += pair[1];
            }
            if cue_active[1] && self.shared.master_cue_b() {
                self.cue_scratch[1][frame * 2] += pair[0];
                self.cue_scratch[1][frame * 2 + 1] += pair[1];
            }
            self.master_gain += master_step;
            for side in 0..2 {
                let value = pair[side] * self.master_gain;
                peaks[side] = peaks[side].max(value.abs());
                output[frame * 2 + side] = value.clamp(-1.0, 1.0);
            }
        }
        /* The master mix, for whichever deck's Debug panel is recording the
         * post-mixer tap -- everything the mixer did to it is in these samples:
         * the rate conversion, the interpolator, trim, EQ, filter, fader and
         * master level, and the output clamp.
         *
         * It is one signal rather than one per channel, so it is offered to every
         * channel's post slot; a push to a slot that is not armed returns at once,
         * which makes this four checks per callback and a copy only for what is
         * actually being recorded. That is what lets whichever player's Debug
         * window armed it receive the mix, since the CoreAudio owner and the
         * player asking are not necessarily the same process. */
        if let Some(capture) = &mut self.capture_post {
            for channel in 0..CHANNEL_COUNT {
                capture.push(channel, &output[..frames * 2], self.rate as u32);
            }
        }
        self.master_gain = target_master;
        self.shared
            .set_master_peaks([peak_meter(peaks[0]), peak_meter(peaks[1])]);
        let cue_levels = [self.shared.cue_level(0), self.shared.cue_level(1)];
        for frame in 0..frames {
            for bus in 0..2 {
                if cue_active[bus] {
                    self.cue_rings[bus].push([
                        self.cue_scratch[bus][frame * 2] * cue_levels[bus],
                        self.cue_scratch[bus][frame * 2 + 1] * cue_levels[bus],
                    ]);
                }
            }
        }
    }
}

/// Preallocated delay line used by the native effect, never resized in the
/// CoreAudio callback. LOW/MID/HI select bands feeding the wet path.
struct FxDsp {
    delay: Vec<[f32; 2]>,
    cursor: usize,
    cursor_total: u64,
    generation_start: u64,
    was_enabled: bool,
    wet_gain: f32,
    band_low: [f32; 2],
    band_high: [f32; 2],
    phase: f32,
}

impl FxDsp {
    fn new(rate: u32) -> Self {
        Self {
            delay: vec![[0.0; 2]; (rate as usize * 2).max(2048)],
            cursor: 0,
            cursor_total: 0,
            generation_start: 0,
            was_enabled: false,
            wet_gain: 0.0,
            band_low: [0.0; 2],
            band_high: [0.0; 2],
            phase: 0.0,
        }
    }

    fn tap(&self, seconds: f32, rate: f32) -> [f32; 2] {
        let frames = ((seconds * rate) as usize).clamp(1, self.delay.len() - 1);
        if self.cursor_total < frames as u64
            || self.cursor_total - (frames as u64) < self.generation_start
        {
            return [0.0; 2];
        }
        self.delay[(self.cursor + self.delay.len() - frames) % self.delay.len()]
    }

    fn process(
        &mut self,
        dry: [f32; 2],
        kind: u32,
        depth: f32,
        bands: u32,
        rate: f32,
        send_enabled: bool,
    ) -> [f32; 2] {
        if send_enabled && !self.was_enabled {
            // Do not replay a delay line left behind by a previous ON cycle.
            self.generation_start = self.cursor_total;
            self.wet_gain = 1.0;
        }
        self.was_enabled = send_enabled;
        if !send_enabled {
            // A short wet-only release avoids a click without leaving FX
            // audible indefinitely after the user switches it off.
            self.wet_gain = (self.wet_gain - 1.0 / (rate * 0.03).max(1.0)).max(0.0);
        }
        let low_alpha = alpha(250.0, rate);
        let high_alpha = alpha(4_000.0, rate);
        let mut send = [0.0; 2];
        for side in 0..2 {
            self.band_low[side] += low_alpha * (dry[side] - self.band_low[side]);
            self.band_high[side] += high_alpha * (dry[side] - self.band_high[side]);
            if bands & 0b001 != 0 {
                send[side] += self.band_low[side];
            }
            if bands & 0b010 != 0 {
                send[side] += self.band_high[side] - self.band_low[side];
            }
            if bands & 0b100 != 0 {
                send[side] += dry[side] - self.band_high[side];
            }
        }
        let (wet, feedback, amount) = match kind {
            0 => (self.tap(0.34, rate), 0.32, 0.38), // echo
            1 => {
                let a = self.tap(0.071, rate);
                let b = self.tap(0.113, rate);
                let c = self.tap(0.179, rate);
                (
                    [
                        a[0] * 0.5 + b[0] * 0.3 + c[0] * 0.2,
                        a[1] * 0.5 + b[1] * 0.3 + c[1] * 0.2,
                    ],
                    0.18,
                    0.34,
                )
            }
            2 => {
                // Triangle LFO avoids a transcendental operation per sample
                // on the real-time output thread.
                let lfo = 1.0 - 4.0 * (self.phase - 0.5).abs();
                let seconds = 0.004 + 0.003 * lfo;
                self.phase += 0.35 / rate;
                if self.phase >= 1.0 {
                    self.phase -= 1.0;
                }
                (self.tap(seconds, rate), 0.0, 0.55)
            }
            3 => (self.tap(0.60, rate), 0.0, 0.5), // delay
            _ => unreachable!(),
        };
        let mut result = dry;
        let mut stored = if send_enabled { send } else { [0.0; 2] };
        for side in 0..2 {
            result[side] += wet[side] * amount * depth * self.wet_gain;
            if send_enabled {
                stored[side] += wet[side] * feedback;
            }
        }
        self.delay[self.cursor] = stored;
        self.cursor = (self.cursor + 1) % self.delay.len();
        self.cursor_total = self.cursor_total.wrapping_add(1);
        result
    }
}

unsafe extern "C" fn render_callback(
    refcon: *mut c_void,
    _flags: *mut u32,
    _timestamp: *const c_void,
    _bus: u32,
    frames: u32,
    buffers: *mut AudioBufferList,
) -> OSStatus {
    if refcon.is_null() || buffers.is_null() {
        return 0;
    }
    let state = &mut *refcon.cast::<MixerRenderState>();
    let list = &mut *buffers;
    if list.number_buffers == 0 || list.buffers[0].data.is_null() {
        return 0;
    }
    let requested = frames as usize * 2;
    let capacity = list.buffers[0].data_byte_size as usize / std::mem::size_of::<f32>();
    let samples =
        std::slice::from_raw_parts_mut(list.buffers[0].data.cast::<f32>(), requested.min(capacity));
    state.render(samples, frames as usize);
    0
}

unsafe extern "C" fn cue_render_callback(
    refcon: *mut c_void,
    _flags: *mut u32,
    _timestamp: *const c_void,
    _bus: u32,
    frames: u32,
    buffers: *mut AudioBufferList,
) -> OSStatus {
    if refcon.is_null() || buffers.is_null() {
        return 0;
    }
    let list = &mut *buffers;
    if list.number_buffers == 0 || list.buffers[0].data.is_null() {
        return 0;
    }
    let requested = frames as usize * 2;
    let capacity = list.buffers[0].data_byte_size as usize / std::mem::size_of::<f32>();
    let output =
        std::slice::from_raw_parts_mut(list.buffers[0].data.cast::<f32>(), requested.min(capacity));
    (*refcon.cast::<CueRenderState>()).render(output);
    0
}

#[allow(clippy::too_many_arguments)]
fn process_channel(
    shared: &MixerShared,
    channel: usize,
    dsp: &mut ChannelDsp,
    rate: f32,
    crossfader: f32,
    mut fx: Option<&mut FxDsp>,
    fx_type: u32,
    fx_depth: f32,
    fx_bands: u32,
    fx_enabled: bool,
    input: &[f32],
    mix: &mut [f32],
    cue_a: &mut [f32],
    cue_b: &mut [f32],
    cue_active: [bool; 2],
    frames: usize,
) {
    let trim = shared.get(channel, MixerParameter::Trim);
    let high = shared.get(channel, MixerParameter::High);
    let mid = shared.get(channel, MixerParameter::Mid);
    let low = shared.get(channel, MixerParameter::Low);
    let filter = shared.get(channel, MixerParameter::Filter);
    let fader = shared.get(channel, MixerParameter::Fader);
    let target_trim = 10.0_f32.powf(((trim - 0.5) * 24.0) / 20.0);
    let iso = shared.eq_isolator();
    let target_high = eq_gain(high, iso);
    let target_mid = eq_gain(mid, iso);
    let target_low = eq_gain(low, iso);
    let target_fader = fader * fader * fader;
    let target_crossfader = crossfader_gain(shared.cross_assignment(channel), crossfader);
    let cue_a_enabled = cue_active[0] && shared.channel_cue(channel);
    let cue_b_enabled = cue_active[1] && shared.channel_cue_b(channel);
    let (target_filter_mode, target_filter_alpha) = if filter < 0.48 {
        let position = filter / 0.48;
        (-1, alpha(80.0 * (18_000.0_f32 / 80.0).powf(position), rate))
    } else if filter > 0.52 {
        let position = (filter - 0.52) / 0.48;
        (1, alpha(20.0 * (10_000.0_f32 / 20.0).powf(position), rate))
    } else {
        (0, dsp.filter_alpha)
    };
    let inverse = 1.0 / frames as f32;
    let trim_step = (target_trim - dsp.trim_gain) * inverse;
    let high_step = (target_high - dsp.high_gain) * inverse;
    let mid_step = (target_mid - dsp.mid_gain) * inverse;
    let low_step = (target_low - dsp.low_gain) * inverse;
    let fader_step = (target_fader - dsp.fader_gain) * inverse;
    let cross_step = (target_crossfader - dsp.crossfader_gain) * inverse;
    let filter_step = (target_filter_alpha - dsp.filter_alpha) * inverse;
    let low_alpha = alpha(250.0, rate);
    let high_alpha = alpha(4000.0, rate);
    dsp.filter_mode = target_filter_mode;
    let mut peaks = [0.0_f32; 2];

    for frame in 0..frames {
        dsp.trim_gain += trim_step;
        dsp.high_gain += high_step;
        dsp.mid_gain += mid_step;
        dsp.low_gain += low_step;
        dsp.fader_gain += fader_step;
        dsp.crossfader_gain += cross_step;
        dsp.filter_alpha += filter_step;
        let mut pair = [0.0_f32; 2];
        for side in 0..2 {
            let index = frame * 2 + side;
            let source = input[index];
            dsp.low_state[side] += low_alpha * (source - dsp.low_state[side]);
            dsp.high_state[side] += high_alpha * (source - dsp.high_state[side]);
            let low_band = dsp.low_state[side];
            let mid_band = dsp.high_state[side] - low_band;
            let high_band = source - dsp.high_state[side];
            let mut sample =
                (low_band * dsp.low_gain + mid_band * dsp.mid_gain + high_band * dsp.high_gain)
                    * dsp.trim_gain;
            peaks[side] = peaks[side].max(sample.abs());
            if dsp.filter_mode == 0 {
                dsp.filter_state[side] = sample;
            } else {
                dsp.filter_state[side] += dsp.filter_alpha * (sample - dsp.filter_state[side]);
                sample = if dsp.filter_mode < 0 {
                    dsp.filter_state[side]
                } else {
                    sample - dsp.filter_state[side]
                };
            }
            if cue_a_enabled {
                cue_a[index] += sample;
            }
            if cue_b_enabled {
                cue_b[index] += sample;
            }
            pair[side] = sample * dsp.fader_gain * dsp.crossfader_gain;
        }
        if let Some(effect) = fx.as_deref_mut() {
            pair = effect.process(pair, fx_type, fx_depth, fx_bands, rate, fx_enabled);
        }
        mix[frame * 2] += pair[0];
        mix[frame * 2 + 1] += pair[1];
    }
    dsp.trim_gain = target_trim;
    dsp.high_gain = target_high;
    dsp.mid_gain = target_mid;
    dsp.low_gain = target_low;
    dsp.fader_gain = target_fader;
    dsp.crossfader_gain = target_crossfader;
    dsp.filter_alpha = target_filter_alpha;
    shared.set_peaks(channel, [peak_meter(peaks[0]), peak_meter(peaks[1])]);
}

fn eq_gain(value: f32, isolator: bool) -> f32 {
    let db = if value <= 0.5 {
        if isolator && value == 0.0 {
            return 0.0;
        }
        let min_db = if isolator { -80.0 } else { -26.0 };
        min_db + value * (-min_db * 2.0)
    } else {
        (value - 0.5) * 12.0
    };
    10.0_f32.powf(db / 20.0)
}

fn crossfader_gain(assignment: u32, position: f32) -> f32 {
    match assignment {
        0 => ((1.0 - position) * 2.0).min(1.0),
        2 => (position * 2.0).min(1.0),
        _ => 1.0,
    }
}

/// Twelve o'clock (0.5) is unity/0 dB; clockwise gives up to +6 dB.
fn master_level_gain(position: f32) -> f32 {
    let position = position.clamp(0.0, 1.0);
    if position <= 0.5 {
        (position * 2.0).powi(2)
    } else {
        10.0_f32.powf(((position - 0.5) * 12.0) / 20.0)
    }
}

fn alpha(cutoff: f32, rate: f32) -> f32 {
    if rate <= 0.0 {
        1.0
    } else {
        1.0 - (-2.0 * std::f32::consts::PI * cutoff / rate).exp()
    }
}

fn peak_meter(linear: f32) -> f32 {
    if linear <= 0.001 {
        0.0
    } else {
        ((20.0 * linear.log10()).clamp(-60.0, 12.0) + 60.0) / 72.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_scale_matches_djm_markings() {
        assert_eq!(peak_meter(0.0), 0.0);
        assert!((peak_meter(1.0) - (60.0 / 72.0)).abs() < 0.0001);
        assert_eq!(peak_meter(10.0), 1.0);
    }

    #[test]
    fn shared_fader_curve_is_unity_at_center() {
        let left_at_center = ((1.0_f32 - 0.5) * 2.0).min(1.0);
        let right_at_center = (0.5_f32 * 2.0).min(1.0);
        assert_eq!((left_at_center, right_at_center), (1.0, 1.0));
    }

    #[test]
    fn native_resampler_preserves_same_rate_linear_samples() {
        let mut resampler = ChannelResampler::default();
        resampler.configure(48_000, 48_000);
        let ratio = resampler.ratio(3_840, 3_840);
        let frames = 3;
        let needed = resampler.source_frames_needed(frames, ratio);
        assert_eq!(needed, 7);
        let source = (0..needed)
            .flat_map(|frame| [frame as f32, frame as f32])
            .collect::<Vec<_>>();
        let mut output = vec![0.0; frames * 2];
        resampler.render(&source, &mut output, frames, ratio);
        assert_eq!(output, vec![1.0, 1.0, 2.0, 2.0, 3.0, 3.0]);
    }

    #[test]
    fn native_resampler_uses_antialias_filter_when_downsampling() {
        let mut resampler = ChannelResampler::default();
        resampler.configure(96_000, 48_000);
        assert_ne!(resampler.biquad, [0.0; 5]);
        let ratio = resampler.ratio(7_680, 7_680);
        let frames = 8;
        let needed = resampler.source_frames_needed(frames, ratio);
        let source = vec![0.5; needed * 2];
        let mut output = vec![0.0; frames * 2];
        resampler.render(&source, &mut output, frames, ratio);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn ring_clock_controller_uses_the_profile_target() {
        let mut on_target = ChannelResampler::default();
        on_target.configure(96_000, 96_000);
        assert!((on_target.ratio(1_024, 1_024) - 1.0).abs() < f64::EPSILON);

        let mut below_target = ChannelResampler::default();
        below_target.configure(96_000, 96_000);
        assert!(below_target.ratio(512, 1_024) < 1.0);

        let mut above_target = ChannelResampler::default();
        above_target.configure(96_000, 96_000);
        assert!(above_target.ratio(2_048, 1_024) > 1.0);
    }

    #[test]
    fn cue_ring_and_rate_adapter_keep_stereo_bus_isolated() {
        let ring = Arc::new(CueRing::new());
        for frame in 0..12 {
            ring.push([frame as f32 / 12.0, -(frame as f32) / 12.0]);
        }
        let mut resampler = ChannelResampler::default();
        resampler.configure(48_000, 48_000);
        let mut adapter = CueRenderState { ring, resampler };
        let mut output = [0.0; 8];
        adapter.render(&mut output);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output[0] > 0.0);
        assert!(output[1] < 0.0);
        assert!(output[6] > output[0]);
    }

    #[test]
    fn crossfader_assignments_and_isolator_curve_affect_audio_gain() {
        assert_eq!(crossfader_gain(0, 1.0), 0.0);
        assert_eq!(crossfader_gain(1, 1.0), 1.0);
        assert_eq!(crossfader_gain(2, 0.0), 0.0);
        assert_eq!(eq_gain(0.0, true), 0.0);
        assert!(eq_gain(0.0, false) > 0.0);
        assert!((eq_gain(0.5, true) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn echo_and_reverb_have_finite_wet_output_without_cross_channel_state() {
        let mut echo = FxDsp::new(1_000);
        let mut reverb = FxDsp::new(1_000);
        let first = echo.process([1.0, 0.0], 0, 1.0, 0b111, 1_000.0, true);
        assert_eq!(first, [1.0, 0.0]);
        let mut tail = [0.0; 2];
        for _ in 0..340 {
            tail = echo.process([0.0; 2], 0, 1.0, 0b111, 1_000.0, true);
        }
        assert!(tail[0] > 0.0);
        assert_eq!(tail[1], 0.0);
        let mut switched_off = FxDsp::new(1_000);
        switched_off.process([1.0, 0.0], 0, 1.0, 0b111, 1_000.0, true);
        let mut silent_tail = [0.0; 2];
        for _ in 0..340 {
            silent_tail = switched_off.process([0.0; 2], 0, 1.0, 0b111, 1_000.0, false);
        }
        assert_eq!(silent_tail, [0.0; 2]);
        let mut flanger = FxDsp::new(1_000);
        for _ in 0..20 {
            flanger.process([1.0, 0.0], 2, 1.0, 0b111, 1_000.0, true);
        }
        let first_off = flanger.process([0.25, 0.0], 2, 1.0, 0b111, 1_000.0, false);
        assert!(first_off[0] > 0.25);
        for _ in 0..40 {
            flanger.process([0.25, 0.0], 2, 1.0, 0b111, 1_000.0, false);
        }
        assert_eq!(
            flanger.process([0.25, 0.0], 2, 1.0, 0b111, 1_000.0, false),
            [0.25, 0.0]
        );
        assert_eq!(
            flanger.process([0.0; 2], 2, 1.0, 0b111, 1_000.0, true),
            [0.0; 2]
        );
        assert_eq!(
            reverb.process([0.25, -0.25], 1, 1.0, 0b111, 1_000.0, false),
            [0.25, -0.25]
        );
        assert_eq!(
            reverb.process([0.0; 2], 1, 1.0, 0b111, 1_000.0, true),
            [0.0; 2]
        );
        assert!((master_level_gain(0.5) - 1.0).abs() < 1e-6);
        assert!(master_level_gain(0.25) < 1.0);
        assert!(master_level_gain(1.0) > 1.0);
        let mut muted_bands = FxDsp::new(1_000);
        muted_bands.process([1.0, 0.0], 3, 1.0, 0, 1_000.0, true);
        let mut delayed = [0.0; 2];
        for _ in 0..600 {
            delayed = muted_bands.process([0.0; 2], 3, 1.0, 0, 1_000.0, true);
        }
        assert_eq!(delayed, [0.0; 2]);
    }

    #[test]
    #[ignore = "opens the current macOS output device"]
    fn native_output_smoke_test() {
        let engine =
            NativeMixerEngine::start(None, 5_000).expect("native mixer should open CoreAudio");
        std::thread::sleep(Duration::from_millis(100));
        drop(engine);
    }
}
