pub(super) mod draw_cache;
mod draw_chassis;
mod draw_jog;
mod draw_left;
mod draw_primitives;
mod draw_right;
pub(crate) use draw_right::{NAV_DETENT_COUNT, NAV_SCROLL_PX_PER_TICK};
mod draw_top;
pub(crate) mod layout;

use egui::{Color32, Pos2, Rect, Vec2};

use super::CdjApp;

pub(in crate::app) use cdj3k_emu_platform::app_meta::DEVICE_NAME;

pub(super) const COL_BLACK: Color32 = Color32::from_rgb(20, 20, 26);
pub(super) const COL_DARK: Color32 = Color32::from_rgb(30, 30, 36);
pub(super) const COL_BTN: Color32 = Color32::from_rgb(48, 48, 55);
pub(super) const COL_BLUE: Color32 = Color32::from_rgb(0, 102, 255);
pub(super) const COL_GREEN: Color32 = Color32::from_rgb(140, 200, 0);
pub(super) const COL_AMBER: Color32 = Color32::from_rgb(220, 200, 0);
pub(super) const COL_SILVER: Color32 = Color32::from_rgb(100, 100, 110);
pub(super) const COL_YELLOW: Color32 = Color32::from_rgb(255, 240, 44);
pub(super) const COL_RED: Color32 = Color32::from_rgb(238, 69, 85);
pub(super) const COL_DARK_RED: Color32 = Color32::from_rgb(130, 20, 20);
pub(super) const COL_WHITE: Color32 = Color32::from_rgb(222, 222, 222);
pub(super) const COL_BTN_OUTLINED_YELLOW: Color32 = Color32::from_rgb(255, 237, 120);
pub(super) const COL_BTN_OUTLINED_WHITE: Color32 = Color32::from_rgb(242, 242, 242);
pub(super) const COL_BTN_CUE: Color32 = Color32::from_rgb(255, 150, 0);
pub(super) const COL_BTN_PLAY: Color32 = Color32::from_rgb(0, 200, 100);
pub(super) const COL_BTN_HOT: Color32 = Color32::from_rgb(70, 70, 80);
pub(super) const COL_BTN_TEXT: Color32 = Color32::from_rgb(200, 200, 210);
pub(super) const COL_BTN_WHITE: Color32 = Color32::from_rgb(255, 255, 255);
pub(super) const COL_LCD_BG: Color32 = Color32::from_rgb(0, 0, 0);
pub(super) const COL_JOG_BODY: Color32 = Color32::from_rgb(35, 35, 40);

#[cfg(debug_assertions)]
const DEBUG_ALIGN_SQUARE_SIZE_PX: f32 = 87.0;
#[cfg(debug_assertions)]
const DEBUG_ALIGN_SQUARE_COLOR: Color32 = Color32::RED;

pub(super) fn panel_bg() -> Color32 {
    COL_BLACK
}

pub(super) use draw_primitives::{
    collect_arc_quad_button, collect_back_double_circle_border, collect_bordered_rect_section,
    collect_button, collect_circle_button, draw_bordered_rect_section, draw_rotary_control,
    draw_rotary_control_collect, paint_double_circle_ring, paint_double_circle_ring_collect,
    ArcDecorSpec, ArcNotchSpec, ButtonType, DoubleBorderSpec, RotaryControlSpec, RotaryGearSpec,
    RotaryIndicatorSpec, RotaryTickSpec, StrokeSpec,
};

pub(super) use cdj3k_emu_platform::desktop::{LAYOUT_REF_H, LAYOUT_REF_W};

pub(super) struct UiScale {
    ox: f32,
    oy: f32,
    scale: f32,
}

impl UiScale {
    /// Returns the `(ox, oy, scale)` triple used as a cache invalidation key.
    pub(super) fn cache_key(&self) -> (f32, f32, f32) {
        (self.ox, self.oy, self.scale)
    }
}

impl UiScale {
    pub(super) fn sp(&self, x: f32, y: f32) -> Pos2 {
        Pos2::new(self.ox + x * self.scale, self.oy + y * self.scale)
    }

    /// Point inside `region` using normalized coordinates `u`, `v` in 0..1
    /// (`region` is in unscaled layout-reference space, same as `sp` / `sr`).
    /// Returns a point inside `region` using normalized coordinates (`horizontal_pct`, `vertical_pct`) in 0..1.
    /// E.g., (0.5, 0.0) is the top-center, (1.0, 1.0) is the bottom-right.
    pub(super) fn sp_in_rect(&self, region: Rect, horizontal_pct: f32, vertical_pct: f32) -> Pos2 {
        let x = region.left() + region.width() * horizontal_pct;
        let y = region.top() + region.height() * vertical_pct;
        self.sp(x, y)
    }

    pub(super) fn sc(&self, value: f32) -> f32 {
        value * self.scale
    }

    pub(super) fn sr(&self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(self.sp(x, y), Vec2::new(w * self.scale, h * self.scale))
    }

    pub(super) fn ar2rect(&self, left: f32, top: f32, aspect_ratio: f32, size: f32) -> Rect {
        if aspect_ratio >= 1.0 {
            self.sr(left, top, size, size / aspect_ratio)
        } else {
            self.sr(left, top, size * aspect_ratio, size)
        }
    }
}

/// Reference-space widths for [`CdjApp::btn`] when the caller does not pass `border_override`.
const DEFAULT_RECT_BTN_BORDER_INNER_REF: f32 = 3.0;
const DEFAULT_RECT_BTN_BORDER_OUTER_REF: f32 = 2.0;
const DEFAULT_RECT_BTN_BORDER_GAP_REF: f32 = 2.0;

/// Reference-space widths for [`CdjApp::circle_btn`] when `btn_border` is `None`.
const DEFAULT_CIRCLE_BTN_BORDER_INNER_REF: f32 = 2.0;
const DEFAULT_CIRCLE_BTN_BORDER_OUTER_REF: f32 = 2.0;
const DEFAULT_CIRCLE_BTN_BORDER_GAP_REF: f32 = 2.0;

#[inline]
pub(super) fn default_rect_btn_border(layout: &UiScale) -> DoubleBorderSpec {
    DoubleBorderSpec::from_strokes_with_gap(
        StrokeSpec {
            width: layout.sc(DEFAULT_RECT_BTN_BORDER_INNER_REF),
            color: COL_SILVER,
        },
        StrokeSpec {
            width: layout.sc(DEFAULT_RECT_BTN_BORDER_OUTER_REF),
            color: COL_DARK,
        },
        layout.sc(DEFAULT_RECT_BTN_BORDER_GAP_REF),
    )
}

#[inline]
pub(super) fn default_circle_btn_border(layout: &UiScale) -> DoubleBorderSpec {
    DoubleBorderSpec::from_strokes_with_gap(
        StrokeSpec {
            width: layout.sc(DEFAULT_CIRCLE_BTN_BORDER_INNER_REF),
            color: COL_SILVER,
        },
        StrokeSpec {
            width: layout.sc(DEFAULT_CIRCLE_BTN_BORDER_OUTER_REF),
            color: COL_DARK,
        },
        layout.sc(DEFAULT_CIRCLE_BTN_BORDER_GAP_REF),
    )
}

/// Snapshot of the parts of [`CdjApp`] state that the debug viewport reads.
///
/// The debug viewport runs as a *deferred* viewport with its own update cycle,
/// so we cannot borrow `&CdjApp` from inside its closure (different thread of
/// execution as far as egui is concerned). Each main `update()` writes a fresh
/// snapshot into a shared `Arc<Mutex<DebugSnapshot>>`; the deferred closure
/// reads from it. This decouples the debug window's repaint cadence from the
/// main window - its FPS counter measures *its own* render rate.
#[derive(Clone)]
pub struct DebugSnapshot {
    pub status: String,
    /// Main viewport fps (so the debug pane can show both its own and the
    /// main viewport's frame rate independently).
    pub main_fps: f32,
    pub main_shape_count: u64,
    pub jog_dbg_lines: [String; 3],
    pub midi_jog_dbg_lines: [String; 4],
    pub lcd_touch: Option<(u16, u16)>,
    pub last_miso: [u8; cdj3k_emu_subucom::miso_frame::MISO_SIZE],
    pub led_frame: [u8; 64],
    pub audio: AudioDebugSnapshot,
    pub prolink: Option<cdj3k_emu_platform::menu_state::ProlinkTrackStatus>,
    /// USB Save control state, as the host has it.
    pub usbsave_armed: bool,
    /// What the guest last said, which is `None` until it answers at all.
    pub usbsave_reported: Option<cdj3k_emu_platform::menu_state::UsbSaveState>,
    /// The last save the mods reported, relayed by cfgd: the fields written into
    /// the file. `None` until something has been saved this boot.
    pub usbsave_last: Option<String>,
}

#[derive(Clone, Copy, Default)]
pub struct AudioDebugSnapshot {
    pub reported_latency_ms: Option<(u32, u32, u32)>,
    pub qemu_running: bool,
    pub audio_enabled: bool,
    pub external_ring_active: bool,
    pub guest_rate_hz: u32,
    pub transfer_frames: u32,
    pub startup_prefill_frames: u32,
    pub output_rate_hz: u32,
    pub shared_ring_frames: usize,
    pub shared_ring_target_frames: u32,
    pub coreaudio_buffer_us: u32,
    pub host_buffer_frames: u32,
    pub host_safety_frames: u32,
    pub host_device_frames: u32,
}

impl Default for DebugSnapshot {
    fn default() -> Self {
        Self {
            status: String::new(),
            main_fps: 60.0,
            main_shape_count: 0,
            jog_dbg_lines: [String::new(), String::new(), String::new()],
            midi_jog_dbg_lines: [String::new(), String::new(), String::new(), String::new()],
            lcd_touch: None,
            last_miso: [0u8; cdj3k_emu_subucom::miso_frame::MISO_SIZE],
            led_frame: [0u8; 64],
            audio: AudioDebugSnapshot::default(),
            prolink: None,
            usbsave_armed: false,
            usbsave_reported: None,
            usbsave_last: None,
        }
    }
}

/// Render the debug pane from a snapshot. `local_fps` is the fps measured
/// inside the debug viewport's own update cycle (independent of `main_fps`).
pub(super) fn draw_debug_content(
    ui: &mut egui::Ui,
    snap: &DebugSnapshot,
    local_fps: f32,
    capture: &cdj3k_emu_platform::audio_capture::AudioCaptureControl,
) {
    let mono = egui::FontId::monospace(11.0);

    // ---- Status / FPS / shape count ----
    ui.label(egui::RichText::new(&snap.status).font(mono.clone()));
    ui.label(
        egui::RichText::new(format!(
            "fps: {:.1} (debug)  {:.1} (main)  shapes/f: {}",
            local_fps, snap.main_fps, snap.main_shape_count
        ))
        .font(mono.clone()),
    );
    ui.separator();

    // ---- Pre-DSP audio capture ----
    let capture_status = capture.status();
    ui.horizontal(|ui| {
        if capture_status.active {
            if ui.button("Stop Capture").clicked() {
                capture.stop();
            }
        } else if ui.button("Capture").clicked() {
            if let Err(error) = capture.choose_and_start() {
                eprintln!("cdj3k-emu: audio capture failed: {error}");
            }
        }
        ui.label(
            egui::RichText::new("deck input · pre trim / EQ / filter / fader")
                .font(mono.clone())
                .color(egui::Color32::GRAY),
        );
    });
    if let Some(path) = &capture_status.path {
        let state = if capture_status.active {
            "recording"
        } else {
            "saved"
        };
        ui.label(
            egui::RichText::new(format!(
                "capture: {state}  {} Hz  {:.1}s  dropped={}  {}",
                capture_status.sample_rate,
                if capture_status.sample_rate == 0 {
                    0.0
                } else {
                    capture_status.frames_written as f64 / capture_status.sample_rate as f64
                },
                capture_status.dropped_frames,
                path.display(),
            ))
            .font(mono.clone()),
        );
    }
    if let Some(error) = &capture_status.error {
        ui.colored_label(egui::Color32::LIGHT_RED, format!("capture error: {error}"));
    }
    ui.separator();

    // ---- USB Save ----
    // The switch is the host's intent; the line under it is the guest's answer.
    // They are shown apart on purpose: with the mods off, nothing in the guest
    // reads the flag at all, so an armed toggle on its own would be a claim the
    // host cannot make.
    {
        ui.label(
            egui::RichText::new("USB Save")
                .font(mono.clone())
                .strong(),
        );
        let mut armed = snap.usbsave_armed;
        let mut changed = false;
        ui.add_enabled_ui(snap.audio.qemu_running, |ui| {
            changed |= ui
                .checkbox(&mut armed, "Write each loaded track to USB_SAVE/")
                .changed();
        });
        match snap.usbsave_reported {
            Some(reported) if reported.armed => {
                ui.label(
                    egui::RichText::new("guest: armed - writing on every load")
                        .font(mono.clone())
                        .color(egui::Color32::LIGHT_GREEN),
                );
            }
            Some(_) => {
                ui.label(egui::RichText::new("guest: off").font(mono.clone()));
            }
            None => {
                ui.label(
                    egui::RichText::new("guest: no reply - EP122 Mods off?")
                        .font(mono.clone())
                        .color(egui::Color32::GRAY),
                );
            }
        }
        // What the mods last wrote, relayed by cfgd. The host cannot read the
        // stick without mounting the live image, so this is how a save is checked.
        ui.label(
            egui::RichText::new("last save")
                .font(mono.clone())
                .strong(),
        );
        match &snap.usbsave_last {
            Some(line) => {
                ui.label(
                    egui::RichText::new(line)
                        .font(mono.clone())
                        .color(egui::Color32::LIGHT_GREEN),
                );
            }
            None => {
                ui.label(
                    egui::RichText::new("none yet this boot")
                        .font(mono.clone())
                        .color(egui::Color32::GRAY),
                );
            }
        }
        if changed {
            let mut state = cdj3k_emu_platform::menu_state::lock();
            state.usbsave_armed = armed;
            state.usbsave_send_req = true;
        }
    }
    ui.separator();

    // ---- Track state published by this player over Pro DJ Link ----
    ui.label(
        egui::RichText::new("track (Pro DJ Link)")
            .font(mono.clone())
            .strong(),
    );
    if let Some(track) = &snap.prolink {
        let age_ms = track.received_at.elapsed().as_millis();
        let freshness = if age_ms <= 1_000 { "live" } else { "stale" };
        ui.label(
            egui::RichText::new(format!(
                "publisher: {}  player={}  ip={}  age={} ms ({})  firmware={}",
                track.device_name,
                track.player_number,
                track.source_ip,
                age_ms,
                freshness,
                track.firmware,
            ))
            .font(mono.clone()),
        );
        if track.source_player == 0 || track.source_slot == 0 || track.track_type == 0 {
            ui.label(egui::RichText::new("loaded: none").font(mono.clone()));
        } else {
            ui.label(
                egui::RichText::new(format!(
                    "loaded: source-player={}  slot={}  type={}  rekordbox/track-id={}  list-track={}",
                    track.source_player,
                    prolink_slot_name(track.source_slot),
                    prolink_track_type_name(track.track_type),
                    track.track_id,
                    track.track_number,
                ))
                .font(mono.clone()),
            );
        }
        let pitch_ratio = track.pitch_raw as f64 / 0x10_0000 as f64;
        let pitch_percent = (pitch_ratio - 1.0) * 100.0;
        let bpm = (track.track_bpm_x100 != u16::MAX).then_some(track.track_bpm_x100 as f64 / 100.0);
        let bpm_text = bpm
            .map(|value| format!("{value:.2}"))
            .unwrap_or_else(|| "--".to_string());
        let effective_bpm_text = bpm
            .map(|value| format!("{:.2}", value * pitch_ratio))
            .unwrap_or_else(|| "--".to_string());
        ui.label(
            egui::RichText::new(format!(
                "transport: {}  track-bpm={}  pitch={:+.3}%  effective-bpm={}  beat={}  beat-in-bar={}",
                prolink_play_state_name(track.play_state),
                bpm_text,
                pitch_percent,
                effective_bpm_text,
                track.beat_number,
                track.beat_in_bar,
            ))
            .font(mono.clone()),
        );
        ui.label(
            egui::RichText::new(format!(
                "flags: play={}  master={}  sync={}  on-air={}  raw=0x{:02x}",
                yes_no(track.state_flags & 0x40 != 0),
                yes_no(track.state_flags & 0x20 != 0),
                yes_no(track.state_flags & 0x10 != 0),
                yes_no(track.state_flags & 0x08 != 0),
                track.state_flags,
            ))
            .font(mono.clone()),
        );
        ui.label(
            egui::RichText::new(
                "note: title/artist/album are DB-server replies keyed by source + slot + track id; they are not present in the recurring status packet",
            )
            .font(mono.clone())
            .color(egui::Color32::GRAY),
        );
    } else {
        ui.label(
            egui::RichText::new("waiting for this player to publish a CDJ status packet")
                .font(mono.clone()),
        );
    }
    ui.separator();

    // ---- Audio pipeline latency ----
    ui.label(
        egui::RichText::new("audio latency")
            .font(mono.clone())
            .strong(),
    );
    let (transport, transport_color) = if !snap.audio.audio_enabled {
        ("DISABLED", egui::Color32::GRAY)
    } else if !snap.audio.qemu_running {
        ("EMULATION STOPPED", egui::Color32::GRAY)
    } else if snap.audio.external_ring_active {
        ("SHARED NATIVE MIXER", egui::Color32::LIGHT_GREEN)
    } else {
        ("SHARED TRANSPORT UNAVAILABLE", egui::Color32::LIGHT_RED)
    };
    ui.label(
        egui::RichText::new(format!("transport: {transport}"))
            .font(mono.clone())
            .strong()
            .color(transport_color),
    );
    match snap.audio.reported_latency_ms {
        Some((total, guest, host)) => ui.label(
            egui::RichText::new(format!(
                "reported: total={total} ms  guest-in-flight={guest} ms  host-extra={host} ms"
            ))
            .font(mono.clone()),
        ),
        None => ui
            .label(egui::RichText::new("reported: waiting for guest telemetry").font(mono.clone())),
    };
    let guest_rate = snap.audio.guest_rate_hz.max(1);
    ui.label(
        egui::RichText::new(format!(
            "guest: rate={} Hz  transfer={} f ({:.2} ms)  startup-prefill={} f ({:.2} ms)",
            snap.audio.guest_rate_hz,
            snap.audio.transfer_frames,
            frames_to_ms(snap.audio.transfer_frames as usize, guest_rate),
            snap.audio.startup_prefill_frames,
            frames_to_ms(snap.audio.startup_prefill_frames as usize, guest_rate),
        ))
        .font(mono.clone()),
    );
    if snap.audio.output_rate_hz > 0 {
        let requested_frames = (snap.audio.output_rate_hz as u64
            * snap.audio.coreaudio_buffer_us as u64
            / 1_000_000) as usize;
        ui.label(
            egui::RichText::new(format!(
                "host: output={} Hz  shared-ring={} source-f ({:.2} ms), target={} f ({:.2} ms)  CoreAudio-request={} us (~{} f)",
                snap.audio.output_rate_hz,
                snap.audio.shared_ring_frames,
                frames_to_ms(snap.audio.shared_ring_frames, snap.audio.guest_rate_hz),
                snap.audio.shared_ring_target_frames,
                frames_to_ms(
                    snap.audio.shared_ring_target_frames as usize,
                    snap.audio.guest_rate_hz,
                ),
                snap.audio.coreaudio_buffer_us,
                requested_frames,
            ))
            .font(mono.clone()),
        );
    } else {
        ui.label(egui::RichText::new("host: native output is not running").font(mono.clone()));
    }
    let native_frames = snap.audio.host_buffer_frames
        + snap.audio.host_safety_frames
        + snap.audio.host_device_frames;
    ui.label(
        egui::RichText::new(format!(
            "native HAL: buffer={} f  safety={} f  device={} f  total={} f ({:.2} ms)",
            snap.audio.host_buffer_frames,
            snap.audio.host_safety_frames,
            snap.audio.host_device_frames,
            native_frames,
            frames_to_ms(native_frames as usize, snap.audio.output_rate_hz),
        ))
        .font(mono.clone()),
    );
    ui.label(
        egui::RichText::new(
            "note: shared-ring depth is host-resident but overlaps guest-in-flight",
        )
        .font(mono.clone())
        .color(egui::Color32::GRAY),
    );
    ui.separator();

    // ---- Jog kinematics ----
    ui.label(egui::RichText::new("jog").font(mono.clone()).strong());
    for line in &snap.jog_dbg_lines {
        ui.label(egui::RichText::new(line).font(mono.clone()));
    }
    ui.label(
        egui::RichText::new("midi jog inputs")
            .font(mono.clone())
            .strong(),
    );
    for line in &snap.midi_jog_dbg_lines {
        ui.label(egui::RichText::new(line).font(mono.clone()));
    }
    ui.separator();

    // ---- LCD touch ----
    ui.label(egui::RichText::new("lcd touch").font(mono.clone()).strong());
    let touch_str = match snap.lcd_touch {
        Some((x, y)) => format!("x={:4}  y={:4}", x, y),
        None => "none".to_string(),
    };
    ui.label(egui::RichText::new(touch_str).font(mono.clone()));
    ui.separator();

    // ---- MISO frame (64 bytes, 4 rows of 16) ----
    ui.label(
        egui::RichText::new("miso (ctrl)")
            .font(mono.clone())
            .strong(),
    );
    for row in 0..4 {
        let start = row * 16;
        let hex: String = snap.last_miso[start..start + 16]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" ");
        ui.label(egui::RichText::new(format!("b{:02}  {}", start, hex)).font(mono.clone()));
    }
    ui.separator();

    // ---- MOSI LED frame (64 bytes, 4 rows of 16) ----
    ui.label(
        egui::RichText::new("mosi (led)")
            .font(mono.clone())
            .strong(),
    );
    let led = &snap.led_frame;
    for row in 0..4 {
        let start = row * 16;
        let hex: String = led[start..start + 16]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" ");
        ui.label(egui::RichText::new(format!("b{:02}  {}", start, hex)).font(mono.clone()));
    }

    // ---- MOSI decoded ----
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("mosi decoded")
            .font(mono.clone())
            .strong(),
    );
    let bitfield: String = (2..=11)
        .map(|i| format!("b{:02}:{:08b}", i, led[i]))
        .collect::<Vec<_>>()
        .join("  ");
    ui.label(egui::RichText::new(bitfield).font(mono.clone()));
    let pads: String = (0..8usize)
        .map(|p| {
            let base = 12 + p * 3;
            format!(
                "{}:{:02x}/{:02x}/{:02x}",
                (b'A' + p as u8) as char,
                led[base],
                led[base + 1],
                led[base + 2]
            )
        })
        .collect::<Vec<_>>()
        .join("  ");
    ui.label(egui::RichText::new(pads).font(mono.clone()));
    let inds = format!(
        "SD:{:02x}/{:02x}/{:02x}  USB:{:02x}/{:02x}/{:02x}  ONAIR:{:02x}/{:02x}/{:02x}",
        led[36], led[37], led[38], led[39], led[40], led[41], led[42], led[43], led[44],
    );
    ui.label(egui::RichText::new(inds).font(mono));
}

impl CdjApp {
    /// Builds a fresh [`DebugSnapshot`] from the current app state.
    pub(super) fn debug_snapshot(&self) -> DebugSnapshot {
        let (instance_id, reported_latency_ms, tuning, qemu_running, audio_enabled, prolink,
             usbsave_armed, usbsave_reported, usbsave_last) = {
            let state = cdj3k_emu_platform::menu_state::lock();
            (
                state.current_instance_id,
                cdj3k_emu_platform::menu_state::unpack_latency(state.latency_packed),
                state.audio_latency.effective(),
                state.qemu_running,
                state.audio_enabled,
                state.prolink_track_status.clone(),
                state.usbsave_armed,
                state.usbsave_reported,
                state.usbsave_last.clone(),
            )
        };
        let (
            output_rate_hz,
            guest_rate_hz,
            shared_ring_frames,
            shared_ring_target_frames,
            coreaudio_buffer_us,
            external_ring_active,
            (host_buffer_frames, host_safety_frames, host_device_frames),
        ) = self.mixer.audio_debug_stats(instance_id);
        DebugSnapshot {
            status: self.status.clone(),
            main_fps: self.fps_smooth,
            main_shape_count: self.frame_shape_count,
            jog_dbg_lines: self.jog_dbg_lines.clone(),
            midi_jog_dbg_lines: self.midi_jog_debug_lines(),
            lcd_touch: self.lcd_touch,
            last_miso: self.last_miso,
            led_frame: self.led_state.frame,
            prolink,
            usbsave_armed,
            usbsave_reported,
            usbsave_last,
            audio: AudioDebugSnapshot {
                reported_latency_ms,
                qemu_running,
                audio_enabled,
                external_ring_active,
                guest_rate_hz,
                transfer_frames: tuning.guest_transfer_frames,
                startup_prefill_frames: tuning.startup_prefill_frames,
                output_rate_hz,
                shared_ring_frames,
                shared_ring_target_frames,
                coreaudio_buffer_us,
                host_buffer_frames,
                host_safety_frames,
                host_device_frames,
            },
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn prolink_slot_name(slot: u8) -> &'static str {
    match slot {
        0 => "none",
        1 => "CD",
        2 => "SD",
        3 => "USB",
        4 => "rekordbox",
        _ => "unknown",
    }
}

fn prolink_track_type_name(track_type: u8) -> &'static str {
    match track_type {
        0 => "none",
        1 => "rekordbox",
        2 => "unanalyzed",
        5 => "audio-CD",
        _ => "unknown",
    }
}

fn prolink_play_state_name(state: u8) -> &'static str {
    match state {
        0x00 => "no track",
        0x02 => "loading",
        0x03 => "playing",
        0x04 => "looping",
        0x05 => "paused",
        0x06 => "cue paused",
        0x07 => "cue play",
        0x08 => "cue scratch",
        0x09 => "searching",
        0x0e => "CD idle",
        0x11 => "ended",
        _ => "unknown",
    }
}

fn frames_to_ms(frames: usize, rate_hz: u32) -> f32 {
    if rate_hz == 0 {
        0.0
    } else {
        frames as f32 * 1000.0 / rate_hz as f32
    }
}

impl CdjApp {
    pub(super) fn draw_ui(&mut self, ui: &mut egui::Ui) {
        puffin::profile_function!();
        self.frame_shape_count = 0;
        let avail = ui.clip_rect().shrink(2.0);
        let scale = (avail.width() / LAYOUT_REF_W).min(avail.height() / LAYOUT_REF_H);
        let ox = avail.left() + (avail.width() - LAYOUT_REF_W * scale) * 0.5;
        let oy = avail.top() + (avail.height() - LAYOUT_REF_H * scale) * 0.5;
        let layout = UiScale { ox, oy, scale };
        let p = ui.painter().clone();

        // Chassis - fully static, rebuilt only on resize.
        let ppp = ui.ctx().pixels_per_point();
        let bg_shapes = self
            .chassis_bg_cache
            .get_or_build(ox, oy, scale, ppp, |list| {
                draw_chassis::collect_chassis(list, &layout);
            });
        self.frame_shape_count += bg_shapes.len() as u64;
        p.extend(bg_shapes.iter().cloned());
        // LCD overlay on top of all sections - fully static, rebuilt only on resize.
        let overlay_shapes =
            self.chassis_lcd_overlay_cache
                .get_or_build(ox, oy, scale, ppp, |list| {
                    draw_chassis::collect_chassis_lcd_overlay(list, &layout);
                });
        self.frame_shape_count += overlay_shapes.len() as u64;
        p.extend(overlay_shapes.iter().cloned());

        {
            puffin::profile_scope!("draw_top");
            self.draw_top_section(ui, &p, &layout);
        }
        {
            puffin::profile_scope!("draw_left");
            self.draw_left_section(ui, &p, &layout);
        }
        {
            puffin::profile_scope!("draw_jog");
            self.draw_jog_wheel_section(ui, &p, &layout);
        }
        {
            puffin::profile_scope!("draw_right");
            self.draw_right_sections(ui, &p, &layout);
        }

        #[cfg(debug_assertions)]
        {
            let s = DEBUG_ALIGN_SQUARE_SIZE_PX;
            for (x, y) in [
                (0.0, 0.0),
                (LAYOUT_REF_W - s, 0.0),
                (0.0, LAYOUT_REF_H - s),
                (LAYOUT_REF_W - s, LAYOUT_REF_H - s),
            ] {
                p.rect_filled(layout.sr(x, y, s, s), 0.0, DEBUG_ALIGN_SQUARE_COLOR);
            }
        }
    }
}
