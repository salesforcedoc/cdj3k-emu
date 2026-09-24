//! Shared DJM-style four-channel virtual mixer viewport.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cdj3k_emu_platform::mixer::{MixerParameter, MixerShared, CHANNEL_COUNT};
use cdj3k_emu_storage::{
    InstanceSettings, MixerChannelSettings, MixerMasterSettings, MixerMidiExtra,
};
use egui::{Color32, Frame, Margin, RichText, Sense, Stroke, Vec2};

const SAVE_DEBOUNCE: Duration = Duration::from_millis(350);
const PANEL_BG: Color32 = Color32::from_rgb(12, 13, 15);
const STRIP_BG: Color32 = Color32::from_rgb(17, 18, 20);
const LABEL: Color32 = Color32::from_rgb(205, 207, 210);
const MUTED: Color32 = Color32::from_rgb(125, 129, 135);
const ACCENT: Color32 = Color32::from_rgb(236, 150, 45);
const CUE_BUTTON_FONT_SIZE: f32 = 9.0;
// The channel filter is the tallest section above the cue row. Keep the
// shared anchor close to its natural bottom so none of the strips gets a
// large, empty gap above its cue buttons.
const CUE_ROW_TOP: f32 = 406.0;

pub(super) struct MixerPanel {
    shared: Option<MixerShared>,
    error: Option<String>,
    dirty_since: [Option<Instant>; CHANNEL_COUNT],
    focused_after_open: bool,
    window_lock: Option<File>,
    last_shared_open: bool,
    last_master: MixerMasterSettings,
    master_dirty_since: Option<Instant>,
    fx_selector_position: f32,
}

impl MixerPanel {
    pub(super) fn new(instance_id: u32) -> Self {
        match MixerShared::open() {
            Ok(shared) => {
                // Each process seeds only its own channel. This restores that
                // player's persisted strip without overwriting live channels
                // owned by the other emulator processes.
                let settings = InstanceSettings::load_or_init(instance_id).mixer;
                let channel = instance_id.saturating_sub(1) as usize;
                set_channel_settings(&shared, channel, settings);
                let mut panel = Self {
                    shared: Some(shared),
                    error: None,
                    dirty_since: [None; CHANNEL_COUNT],
                    focused_after_open: false,
                    window_lock: None,
                    last_shared_open: false,
                    last_master: MixerMasterSettings::default(),
                    master_dirty_since: None,
                    fx_selector_position: 0.0,
                };
                panel.try_claim_window();
                panel
            }
            Err(err) => Self {
                shared: None,
                error: Some(format!("Could not open virtual mixer: {err}")),
                dirty_since: [None; CHANNEL_COUNT],
                focused_after_open: false,
                window_lock: None,
                last_shared_open: false,
                last_master: MixerMasterSettings::default(),
                master_dirty_since: None,
                fx_selector_position: 0.0,
            },
        }
    }

    pub(super) fn set_midi_channel(
        &mut self,
        channel: usize,
        parameter: MixerParameter,
        value: f32,
    ) {
        self.set(channel, parameter, value);
    }

    pub(super) fn apply_midi_extra(
        &mut self,
        channel: usize,
        extra: MixerMidiExtra,
        value: u8,
        is_note: bool,
    ) {
        let Some(shared) = &self.shared else { return };
        match extra {
            MixerMidiExtra::CueA if channel <= CHANNEL_COUNT => {
                let current = if channel < CHANNEL_COUNT {
                    shared.channel_cue(channel)
                } else {
                    shared.master_cue()
                };
                let enabled = if is_note { !current } else { value >= 64 };
                if channel < CHANNEL_COUNT {
                    shared.set_channel_cue(channel, enabled);
                } else {
                    shared.set_master_cue(enabled);
                }
            }
            MixerMidiExtra::CueB if channel <= CHANNEL_COUNT => {
                let current = if channel < CHANNEL_COUNT {
                    shared.channel_cue_b(channel)
                } else {
                    shared.master_cue_b()
                };
                let enabled = if is_note { !current } else { value >= 64 };
                if channel < CHANNEL_COUNT {
                    shared.set_channel_cue_b(channel, enabled);
                } else {
                    shared.set_master_cue_b(enabled);
                }
            }
            MixerMidiExtra::CrossAssign if channel < CHANNEL_COUNT => {
                shared.set_cross_assignment(channel, (value as u32 * 3 / 128).min(2));
                self.dirty_since[channel] = Some(Instant::now());
            }
            MixerMidiExtra::Crossfader if !is_note => {
                shared.set_crossfader(value as f32 / 127.0);
            }
            MixerMidiExtra::EqCurve => shared.set_eq_isolator(if is_note {
                !shared.eq_isolator()
            } else {
                value >= 64
            }),
            MixerMidiExtra::MasterLevel => shared.set_master_level(value as f32 / 127.0),
            MixerMidiExtra::CueALevel => shared.set_cue_level(0, value as f32 / 127.0),
            MixerMidiExtra::CueBLevel => shared.set_cue_level(1, value as f32 / 127.0),
            MixerMidiExtra::FxType => shared.set_fx_type((value as u32 * 4 / 128).min(3)),
            MixerMidiExtra::FxChannel => shared.set_fx_channel((value as u32 * 5 / 128).min(4)),
            MixerMidiExtra::FxOn => shared.set_fx_enabled(if is_note {
                !shared.fx_enabled()
            } else {
                value >= 64
            }),
            MixerMidiExtra::FxDepth => shared.set_fx_depth(value as f32 / 127.0),
            MixerMidiExtra::FxLow | MixerMidiExtra::FxMid | MixerMidiExtra::FxHigh => {
                let bit = match extra {
                    MixerMidiExtra::FxLow => 1,
                    MixerMidiExtra::FxMid => 2,
                    _ => 4,
                };
                let bands = shared.fx_bands();
                shared.set_fx_bands(if is_note {
                    bands ^ bit
                } else if value >= 64 {
                    bands | bit
                } else {
                    bands & !bit
                });
            }
            _ => {}
        }
    }

    pub(super) fn owns_shared_window(&mut self) -> bool {
        self.try_claim_window();
        self.window_lock.is_some()
    }

    pub(super) fn cue_led_states(&self) -> Option<[[bool; 2]; 5]> {
        let shared = self.shared.as_ref()?;
        Some(std::array::from_fn(|channel| {
            if channel < CHANNEL_COUNT {
                [shared.channel_cue(channel), shared.channel_cue_b(channel)]
            } else {
                [shared.master_cue(), shared.master_cue_b()]
            }
        }))
    }

    pub(super) fn fx_led_state(&self) -> Option<bool> {
        self.shared.as_ref().map(MixerShared::fx_enabled)
    }

    pub(super) fn request_midi_learn(&self, tab: u32) {
        if let Some(shared) = &self.shared {
            shared.request_midi_learn(tab);
        }
    }

    pub(super) fn midi_learn_state(&self) -> (bool, u32, u32, bool) {
        self.shared.as_ref().map_or((false, 0, 0, false), |shared| {
            (
                shared.midi_learn_open(),
                shared.midi_learn_tab(),
                shared.midi_learn_request_generation(),
                shared.midi_learn_active(),
            )
        })
    }

    pub(super) fn set_midi_learn_tab(&self, tab: u32) {
        if let Some(shared) = &self.shared {
            shared.set_midi_learn_tab(tab);
        }
    }

    pub(super) fn request_midi_device_action(&self, reset: bool) {
        if let Some(shared) = &self.shared {
            shared.request_midi_device_action(reset);
        }
    }

    pub(super) fn midi_device_action(&self) -> u32 {
        self.shared
            .as_ref()
            .map_or(0, MixerShared::midi_device_action)
    }

    pub(super) fn set_midi_learn_open(&self, open: bool) {
        if let Some(shared) = &self.shared {
            shared.set_midi_learn_open(open);
        }
    }

    pub(super) fn set_midi_learn_active(&self, active: bool) {
        if let Some(shared) = &self.shared {
            shared.set_midi_learn_active(active);
        }
    }

    /// Native/shared-ring values used by the debug viewport. Ring depth is a
    /// live sample and intentionally not persisted.
    pub(super) fn audio_debug_stats(
        &self,
        instance_id: u32,
    ) -> (u32, u32, usize, u32, u32, bool, (u32, u32, u32)) {
        let Some(shared) = &self.shared else {
            return (0, 0, 0, 0, 0, false, (0, 0, 0));
        };
        let channel = instance_id.saturating_sub(1) as usize;
        (
            shared.output_rate(),
            shared.source_rate(channel),
            shared.available_audio_frames(channel),
            shared.target_audio_frames(channel),
            shared.buffer_us(),
            shared.external_ring_active(channel),
            shared.host_latency_frames(),
        )
    }

    fn set(&mut self, channel: usize, parameter: MixerParameter, value: f32) {
        if let Some(shared) = &self.shared {
            shared.set(channel, parameter, value);
            if channel < CHANNEL_COUNT {
                self.dirty_since[channel] = Some(Instant::now());
            }
        }
    }

    pub(super) fn tick_persistence(&mut self) {
        let now = Instant::now();
        for channel in 0..CHANNEL_COUNT {
            if self.dirty_since[channel]
                .is_some_and(|changed| now.duration_since(changed) >= SAVE_DEBOUNCE)
            {
                self.persist_channel(channel);
                self.dirty_since[channel] = None;
            }
        }
        if self.window_lock.is_some() {
            if let Some(shared) = &self.shared {
                let current = master_settings(shared);
                if current != self.last_master {
                    self.last_master = current;
                    self.master_dirty_since = Some(now);
                }
            }
            if self
                .master_dirty_since
                .is_some_and(|changed| now.duration_since(changed) >= SAVE_DEBOUNCE)
            {
                if let Err(error) = cdj3k_emu_storage::save_mixer_master_settings(self.last_master)
                {
                    self.error = Some(format!("Could not save master mixer controls: {error}"));
                }
                self.master_dirty_since = None;
            }
        }
    }

    pub(super) fn flush(&mut self) {
        for channel in 0..CHANNEL_COUNT {
            if self.dirty_since[channel].is_some() {
                self.persist_channel(channel);
                self.dirty_since[channel] = None;
            }
        }
        if self.master_dirty_since.is_some() {
            let _ = cdj3k_emu_storage::save_mixer_master_settings(self.last_master);
            self.master_dirty_since = None;
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context, requested_open: bool) -> bool {
        self.try_claim_window();
        let Some(shared) = &self.shared else {
            return false;
        };
        if requested_open != self.last_shared_open {
            shared.set_window_open(requested_open);
        }
        let mut open = shared.window_open();
        self.last_shared_open = open;

        // Non-owner player processes only publish/observe the shared state.
        // The elected owner is the sole process that creates an egui viewport.
        if self.window_lock.is_none() {
            self.focused_after_open = false;
            return open;
        }
        if !open {
            self.focused_after_open = false;
            return false;
        }

        let close_flag = Arc::new(AtomicBool::new(false));
        let close_inner = Arc::clone(&close_flag);
        let need_focus = !self.focused_after_open;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("virtual_mixer"),
            egui::ViewportBuilder::default()
                .with_title("Virtual Mixer")
                .with_inner_size([740.0, 970.0])
                .with_min_inner_size([650.0, 950.0])
                .with_resizable(true)
                .with_active(true),
            |ctx, _class| {
                if ctx.input(|input| input.viewport().close_requested()) {
                    close_inner.store(true, Ordering::Relaxed);
                }
                if need_focus {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                egui::CentralPanel::default()
                    .frame(
                        Frame::default()
                            .fill(PANEL_BG)
                            .inner_margin(Margin::same(12.0)),
                    )
                    .show(ctx, |ui| self.draw(ui));
            },
        );
        self.focused_after_open = true;
        ctx.request_repaint_after(Duration::from_millis(33));
        if close_flag.load(Ordering::Relaxed) {
            open = false;
            if let Some(shared) = &self.shared {
                shared.set_window_open(false);
            }
        }
        self.last_shared_open = open;
        open
    }

    fn draw(&mut self, ui: &mut egui::Ui) {
        egui_extras::install_image_loaders(ui.ctx());
        let logo_width = ui.available_width().min(260.0);
        ui.vertical_centered(|ui| {
            ui.add(
                egui::Image::new(egui::include_image!(
                    "../../assets/phasim-virtual-mixer.svg"
                ))
                .fit_to_exact_size(Vec2::new(logo_width, logo_width / 13.0)),
            );
        });
        if let Some(error) = &self.error {
            ui.add_space(12.0);
            ui.colored_label(Color32::LIGHT_RED, error);
            return;
        }
        ui.add_space(7.0);

        let available = ui.available_size();
        const STRIP_GAP: f32 = 3.0;
        const MAX_STRIP_WIDTH: f32 = 132.0;
        const CROSSFADER_HEIGHT: f32 = 94.0;
        let total_gap = STRIP_GAP * CHANNEL_COUNT as f32;
        let strip_width =
            ((available.x - total_gap) / (CHANNEL_COUNT + 1) as f32).clamp(110.0, MAX_STRIP_WIDTH);
        let mixer_width = strip_width * (CHANNEL_COUNT + 1) as f32 + total_gap;
        let side_gutter = ((available.x - mixer_width) * 0.5).max(0.0);
        let strip_height = (available.y - CROSSFADER_HEIGHT - 5.0).max(0.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = STRIP_GAP;
            ui.add_space(side_gutter);
            for channel in 0..CHANNEL_COUNT {
                ui.allocate_ui_with_layout(
                    Vec2::new(strip_width, strip_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(strip_width);
                        ui.set_height(strip_height);
                        Frame::default()
                            .fill(STRIP_BG)
                            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(54, 57, 62)))
                            .rounding(egui::Rounding::same(1.0))
                            .inner_margin(Margin::symmetric(7.0, 6.0))
                            .show(ui, |ui| {
                                ui.set_width((strip_width - 14.0).max(0.0));
                                ui.set_height((strip_height - 12.0).max(0.0));
                                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                    self.draw_channel(ui, channel)
                                });
                            });
                    },
                );
            }
            ui.allocate_ui_with_layout(
                Vec2::new(strip_width, strip_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(strip_width);
                    ui.set_height(strip_height);
                    Frame::default()
                        .fill(STRIP_BG)
                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(54, 57, 62)))
                        .rounding(egui::Rounding::same(1.0))
                        .inner_margin(Margin::symmetric(7.0, 6.0))
                        .show(ui, |ui| {
                            ui.set_width((strip_width - 14.0).max(0.0));
                            ui.set_height((strip_height - 12.0).max(0.0));
                            self.draw_master(ui);
                        });
                },
            );
        });
        ui.add_space(5.0);
        // The actual travel runs from the center of channel 2 to the center
        // of channel 3. Add only the frame's 14 px margins around that range.
        const CROSSFADER_FRAME_MARGIN: f32 = 14.0;
        let crossfader_range_width = strip_width + STRIP_GAP;
        let crossfader_width = crossfader_range_width + CROSSFADER_FRAME_MARGIN * 2.0;
        let channel_two_center = side_gutter + strip_width * 1.5 + STRIP_GAP;
        let crossfader_gutter = channel_two_center - CROSSFADER_FRAME_MARGIN;
        let channel_fader_width = (strip_width - 22.0).max(76.0);
        let crossfader_cap_length = fader_cap_length(channel_fader_width);
        self.draw_crossfader(
            ui,
            crossfader_width,
            crossfader_gutter,
            crossfader_cap_length,
        );
        self.tick_persistence();
    }

    fn try_claim_window(&mut self) {
        if self.window_lock.is_some() {
            return;
        }
        let Ok(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(cdj3k_emu_platform::runtime_paths::mixer_window_lock_path())
        else {
            return;
        };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            self.window_lock = Some(file);
            if let Some(shared) = &self.shared {
                let settings = cdj3k_emu_storage::load_mixer_master_settings();
                set_master_settings(shared, settings);
                self.last_master = settings;
            }
        }
    }

    fn draw_channel(&mut self, ui: &mut egui::Ui, channel: usize) {
        if self.shared.is_none() {
            return;
        }
        ui.spacing_mut().item_spacing.y = 0.0;
        let strip_top = ui.cursor().top();
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(format!("{}", channel + 1))
                    .size(27.0)
                    .strong()
                    .color(Color32::WHITE),
            );
        });
        ui.add_space(5.0);

        // Like the hardware, the meter runs beside the gain and EQ stack
        // instead of consuming space next to the channel fader.
        const UPPER_HEIGHT: f32 = 264.0;
        const METER_WIDTH: f32 = 12.0;
        const METER_GAP: f32 = 5.0;
        let peaks = self.shared.as_ref().unwrap().peaks(channel);
        let knob_column_width = (ui.available_width() - (METER_WIDTH + METER_GAP) * 2.0).max(0.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            level_meter(ui, peaks, UPPER_HEIGHT);
            ui.add_space(METER_GAP);
            ui.allocate_ui_with_layout(
                Vec2::new(knob_column_width, UPPER_HEIGHT),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (parameter, label) in [
                        (MixerParameter::Trim, "TRIM"),
                        (MixerParameter::High, "HI"),
                        (MixerParameter::Mid, "MID"),
                        (MixerParameter::Low, "LOW"),
                    ] {
                        let mut value = self.shared.as_ref().unwrap().get(channel, parameter);
                        if knob(ui, label, &mut value, 0.5, parameter, false) {
                            self.set(channel, parameter, value);
                        }
                    }
                },
            );
            // Counterbalance the meter rail so the knob centerline remains
            // the exact centerline of the complete channel strip.
            ui.add_space(METER_WIDTH + METER_GAP);
        });

        ui.add_space(5.0);
        ui.separator();
        ui.add_space(3.0);
        let mut filter = self
            .shared
            .as_ref()
            .unwrap()
            .get(channel, MixerParameter::Filter);
        if knob(ui, "FILTER", &mut filter, 0.5, MixerParameter::Filter, true) {
            self.set(channel, MixerParameter::Filter, filter);
        }

        ui.add_space(3.0);
        ui.separator();
        align_cue_row(ui, strip_top);
        cue_buttons(ui, self.shared.as_ref().unwrap(), Some(channel));
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(4.0);

        let fader_height = (ui.available_height() - 58.0).max(95.0);
        let mut fader = self
            .shared
            .as_ref()
            .unwrap()
            .get(channel, MixerParameter::Fader);

        let fader_width = (ui.available_width() - 8.0).max(76.0);
        let leading_space = ((ui.available_width() - fader_width) * 0.5).max(0.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(leading_space);
            if channel_fader(ui, &mut fader, fader_height, fader_width) {
                self.set(channel, MixerParameter::Fader, fader);
            }
        });
        let displayed_fader = self
            .shared
            .as_ref()
            .unwrap()
            .get(channel, MixerParameter::Fader);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(format!("LEVEL {:>3}%", (displayed_fader * 100.0).round()))
                    .monospace()
                    .size(9.0)
                    .color(MUTED),
            );
        });
        ui.add_space(5.0);
        let mut assignment = self.shared.as_ref().unwrap().cross_assignment(channel);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("CROSSFADER ASSIGN").size(8.0).color(MUTED));
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let widths = [23.0, 42.0, 23.0];
            let group_width = widths.iter().sum::<f32>() + 4.0;
            ui.add_space(((ui.available_width() - group_width) * 0.5).max(0.0));
            for ((value, label), width) in [(0, "A"), (1, "THRU"), (2, "B")].into_iter().zip(widths)
            {
                if ui
                    .add_sized(
                        [width, 21.0],
                        egui::SelectableLabel::new(assignment == value, label),
                    )
                    .clicked()
                {
                    assignment = value;
                }
            }
        });
        if self.shared.as_ref().unwrap().cross_assignment(channel) != assignment {
            self.shared
                .as_ref()
                .unwrap()
                .set_cross_assignment(channel, assignment);
            self.dirty_since[channel] = Some(Instant::now());
        }
    }

    fn draw_master(&mut self, ui: &mut egui::Ui) {
        let Some(shared) = &self.shared else { return };
        ui.spacing_mut().item_spacing.y = 0.0;
        let strip_top = ui.cursor().top();
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("M").size(27.0).strong().color(Color32::WHITE));
        });
        ui.add_space(5.0);
        let mut level = shared.master_level();
        if knob(
            ui,
            "MASTER LEVEL",
            &mut level,
            0.5,
            MixerParameter::Fader,
            false,
        ) {
            shared.set_master_level(level);
        }
        let peaks = shared.master_peaks();
        let meter_height = 232.0;
        ui.vertical_centered(|ui| {
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 31.0).max(0.0) * 0.5);
                level_meter(ui, [peaks[0]; 2], meter_height);
                ui.add_space(7.0);
                level_meter(ui, [peaks[1]; 2], meter_height);
            });
            ui.label(RichText::new("L             R").size(8.0).color(MUTED));
        });
        ui.add_space(3.0);
        ui.separator();
        align_cue_row(ui, strip_top);
        cue_buttons(ui, shared, None);
        const FX_GROUP_PADDING: f32 = 7.0;
        const FX_LABEL_GAP: f32 = 3.0;
        ui.add_space(FX_GROUP_PADDING);
        master_group_label(ui, "EQ CURVE");
        let mut iso = shared.eq_isolator();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let group_width = 76.0;
            ui.add_space(((ui.available_width() - group_width) * 0.5).max(0.0));
            if ui
                .add_sized(
                    [36.0, 20.0],
                    egui::SelectableLabel::new(!iso, cue_button_text("EQ")),
                )
                .clicked()
            {
                iso = false;
            }
            if ui
                .add_sized(
                    [36.0, 20.0],
                    egui::SelectableLabel::new(iso, cue_button_text("ISO")),
                )
                .clicked()
            {
                iso = true;
            }
        });
        if iso != shared.eq_isolator() {
            shared.set_eq_isolator(iso);
        }
        ui.add_space(3.0);
        ui.separator();
        ui.add_space(3.0);
        ui.vertical_centered(|ui| ui.strong("BEAT FX"));
        ui.add_space(FX_GROUP_PADDING);
        let mut bands = shared.fx_bands();
        master_group_label(ui, "FX FREQUENCY");
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let width = ((ui.available_width() - 4.0) / 3.0).min(32.0);
            let group_width = width * 3.0 + 4.0;
            ui.add_space(((ui.available_width() - group_width) * 0.5).max(0.0));
            for (index, label) in ["LOW", "MID", "HI"].iter().enumerate() {
                let mask = 1_u32 << index;
                if ui
                    .add_sized(
                        [width, 21.0],
                        egui::SelectableLabel::new(bands & mask != 0, cue_button_text(label)),
                    )
                    .clicked()
                {
                    bands ^= mask;
                }
            }
        });
        if bands != shared.fx_bands() {
            shared.set_fx_bands(bands);
        }
        ui.add_space(FX_GROUP_PADDING);
        master_group_label(ui, "EFFECT SELECT");
        let effect = shared.fx_type();
        if !ui.input(|input| input.pointer.primary_down()) {
            self.fx_selector_position = effect as f32 / 3.0;
        }
        if knob(
            ui,
            "EFFECT SELECT",
            &mut self.fx_selector_position,
            0.0,
            MixerParameter::Fader,
            false,
        ) {
            shared.set_fx_type((self.fx_selector_position * 3.0).round() as u32);
        }
        ui.add_space(FX_GROUP_PADDING);
        let mut target = shared.fx_channel();
        master_group_label(ui, "CH SELECT");
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let width = ((ui.available_width() - 8.0) / 5.0).min(20.0);
            let group_width = width * 5.0 + 8.0;
            ui.add_space(((ui.available_width() - group_width) * 0.5).max(0.0));
            for (channel, label) in ["1", "2", "3", "4", "M"].iter().enumerate() {
                if ui
                    .add_sized(
                        [width, 21.0],
                        egui::SelectableLabel::new(
                            target == channel as u32,
                            cue_button_text(label),
                        ),
                    )
                    .clicked()
                {
                    target = channel as u32;
                }
            }
        });
        if target != shared.fx_channel() {
            shared.set_fx_channel(target);
        }
        ui.add_space(FX_GROUP_PADDING);
        master_group_label(ui, "LEVEL/DEPTH");
        let mut depth = shared.fx_depth();
        if knob(
            ui,
            "LEVEL/DEPTH",
            &mut depth,
            0.5,
            MixerParameter::Fader,
            false,
        ) {
            shared.set_fx_depth(depth);
        }
        const FX_POWER_DIAMETER: f32 = 34.0;
        // Keep the power switch tucked under Level/Depth. At the minimum
        // viewport height, another full group gap would extend this strip
        // below the four channel strips.
        let enabled = shared.fx_enabled();
        let mut clicked = false;
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("ON/OFF").size(8.0).color(MUTED));
            ui.add_space(FX_LABEL_GAP);
            let (rect, response) =
                ui.allocate_exact_size(Vec2::splat(FX_POWER_DIAMETER), Sense::click());
            clicked = response.clicked();
            let center = rect.center();
            let painter = ui.painter();
            if enabled {
                painter.circle_filled(
                    center,
                    FX_POWER_DIAMETER * 0.5,
                    Color32::from_rgb(18, 85, 145),
                );
                painter.circle_filled(
                    center,
                    FX_POWER_DIAMETER * 0.35,
                    Color32::from_rgb(40, 165, 255),
                );
            } else {
                painter.circle_filled(
                    center,
                    FX_POWER_DIAMETER * 0.5,
                    Color32::from_rgb(35, 38, 42),
                );
            }
            painter.circle_stroke(
                center,
                FX_POWER_DIAMETER * 0.5,
                Stroke::new(
                    1.5_f32,
                    if enabled {
                        Color32::from_rgb(106, 204, 255)
                    } else {
                        Color32::from_rgb(132, 136, 142)
                    },
                ),
            );
        });
        if clicked {
            shared.set_fx_enabled(!enabled);
        }
    }

    fn draw_crossfader(&mut self, ui: &mut egui::Ui, width: f32, gutter: f32, cap_length: f32) {
        let Some(shared) = &self.shared else { return };
        let mut value = shared.crossfader();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(gutter);
            Frame::default()
                .fill(STRIP_BG)
                .stroke(Stroke::new(1.0_f32, Color32::from_rgb(54, 57, 62)))
                .inner_margin(Margin::symmetric(14.0, 7.0))
                .show(ui, |ui| {
                    ui.set_width((width - 28.0).max(0.0));
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("A  ·  CROSSFADER  ·  B")
                                .size(9.0)
                                .strong()
                                .color(LABEL),
                        );
                        if crossfader(ui, &mut value, cap_length) {
                            if let Some(shared) = &self.shared {
                                shared.set_crossfader(value);
                            }
                        }
                    });
                });
        });
    }

    fn persist_channel(&mut self, channel: usize) {
        let Some(shared) = &self.shared else { return };
        let instance_id = channel as u32 + 1;
        let mut settings = InstanceSettings::load_or_init(instance_id);
        settings.mixer = channel_settings(shared, channel);
        if let Err(err) = settings.save(instance_id) {
            self.error = Some(format!("Could not save Player {instance_id} mixer: {err}"));
        }
    }
}

fn knob(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    neutral: f32,
    parameter: MixerParameter,
    emphasized: bool,
) -> bool {
    const LABEL_BAND: f32 = 12.0;
    let external_label = matches!(label, "EFFECT SELECT" | "LEVEL/DEPTH");
    let height =
        (if emphasized { 76.0 } else { 66.0 }) - if external_label { LABEL_BAND } else { 0.0 };
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), height),
        Sense::click_and_drag(),
    );
    let mut changed = false;
    if response.double_clicked() || response.secondary_clicked() {
        *value = neutral;
        changed = true;
    } else if response.dragged_by(egui::PointerButton::Primary) {
        let delta = ui.input(|input| input.pointer.delta().y);
        if delta != 0.0 {
            *value = (*value - delta * 0.005).clamp(0.0, 1.0);
            changed = true;
        }
    }
    response.on_hover_text("Drag vertically. Right-click or double-click to reset.");

    let painter = ui.painter();
    let radius = if emphasized { 22.0 } else { 17.0 };
    let tick_radius = radius + 4.0;
    // Keep the label above the tick arc and reserve the lower band for
    // filter mode or effect name when relevant.
    let center = egui::pos2(
        rect.center().x,
        rect.top() + if external_label { 0.0 } else { LABEL_BAND } + tick_radius,
    );
    for index in 0..=10 {
        let angle = std::f32::consts::PI * (0.75 + 1.5 * index as f32 / 10.0);
        let outer = center + Vec2::angled(angle) * tick_radius;
        let inner = center + Vec2::angled(angle) * (tick_radius - 2.5);
        painter.line_segment(
            [inner, outer],
            Stroke::new(1.0_f32, Color32::from_rgb(91, 94, 100)),
        );
    }
    if emphasized {
        painter.circle_stroke(center, radius + 2.5, Stroke::new(2.0_f32, ACCENT));
    }
    painter.circle_filled(center, radius, Color32::from_rgb(35, 38, 42));
    painter.circle_stroke(
        center,
        radius,
        Stroke::new(1.5_f32, Color32::from_rgb(132, 136, 142)),
    );
    let angle = std::f32::consts::PI * (0.75 + 1.5 * *value);
    let marker = center + Vec2::angled(angle) * (radius - 5.0);
    painter.line_segment([center, marker], Stroke::new(2.5_f32, Color32::WHITE));
    if !external_label {
        painter.text(
            egui::pos2(rect.center().x, rect.top()),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(10.0),
            LABEL,
        );
    }
    let display = match parameter {
        MixerParameter::Trim | MixerParameter::High | MixerParameter::Mid | MixerParameter::Low => {
            String::new()
        }
        MixerParameter::Filter => {
            if (*value - 0.5).abs() < 0.01 {
                "OFF".to_owned()
            } else if *value < 0.5 {
                "LPF".to_owned()
            } else {
                "HPF".to_owned()
            }
        }
        MixerParameter::Fader if label == "EFFECT SELECT" => {
            ["ECHO", "REVERB", "FLANGER", "DELAY"][(*value * 3.0).round() as usize].to_owned()
        }
        MixerParameter::Fader => String::new(),
    };
    painter.text(
        egui::pos2(
            rect.center().x,
            rect.bottom() - if label == "EFFECT SELECT" { 4.0 } else { 0.0 },
        ),
        egui::Align2::CENTER_BOTTOM,
        display,
        egui::FontId::monospace(9.0),
        MUTED,
    );
    changed
}

fn channel_fader(ui: &mut egui::Ui, value: &mut f32, height: f32, width: f32) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, height), Sense::click_and_drag());
    let track = rect.shrink2(Vec2::new(0.0, 10.0));
    let mut changed = false;
    if response.double_clicked() || response.secondary_clicked() {
        *value = 1.0;
        changed = true;
    } else if response.clicked_by(egui::PointerButton::Primary)
        || response.dragged_by(egui::PointerButton::Primary)
    {
        if let Some(pointer) = response.interact_pointer_pos() {
            *value = ((track.bottom() - pointer.y) / track.height()).clamp(0.0, 1.0);
            changed = true;
        }
    }
    response.on_hover_text("Drag the channel fader. Right-click or double-click for full level.");

    let painter = ui.painter();
    let x = rect.center().x;
    painter.line_segment(
        [egui::pos2(x, track.top()), egui::pos2(x, track.bottom())],
        Stroke::new(4.0_f32, Color32::from_rgb(5, 6, 7)),
    );
    painter.line_segment(
        [egui::pos2(x, track.top()), egui::pos2(x, track.bottom())],
        Stroke::new(1.0_f32, Color32::from_rgb(70, 73, 78)),
    );
    let marker_rail = rect.width() * 0.39;
    for index in 0..=10 {
        let amount = index as f32 / 10.0;
        let y = egui::lerp(track.bottom()..=track.top(), amount);
        let major = index % 2 == 0;
        let tick = if major { 15.0 } else { 9.0 };
        painter.line_segment(
            [
                egui::pos2(x - marker_rail, y),
                egui::pos2(x - marker_rail + tick, y),
            ],
            Stroke::new(1.0_f32, Color32::from_rgb(116, 119, 124)),
        );
        painter.line_segment(
            [
                egui::pos2(x + marker_rail - tick, y),
                egui::pos2(x + marker_rail, y),
            ],
            Stroke::new(1.0_f32, Color32::from_rgb(116, 119, 124)),
        );
        if major {
            painter.text(
                egui::pos2(x + marker_rail + 3.0, y),
                egui::Align2::LEFT_CENTER,
                format!("{}", index),
                egui::FontId::monospace(8.0),
                MUTED,
            );
        }
    }
    let cap_y = egui::lerp(track.bottom()..=track.top(), *value);
    let cap_width = fader_cap_length(rect.width());
    let cap = egui::Rect::from_center_size(egui::pos2(x, cap_y), Vec2::new(cap_width, 17.0));
    painter.rect_filled(cap, 2.0, Color32::from_rgb(61, 64, 69));
    painter.rect_stroke(
        cap,
        2.0,
        Stroke::new(1.0_f32, Color32::from_rgb(155, 158, 164)),
    );
    painter.line_segment(
        [
            egui::pos2(cap.left() + 4.0, cap.center().y),
            egui::pos2(cap.right() - 4.0, cap.center().y),
        ],
        Stroke::new(2.0_f32, Color32::WHITE),
    );
    changed
}

fn master_group_label(ui: &mut egui::Ui, label: &str) {
    ui.vertical_centered(|ui| ui.label(RichText::new(label).size(8.0).color(MUTED)));
    ui.add_space(3.0);
}

fn cue_button_text(label: &str) -> RichText {
    RichText::new(label).size(CUE_BUTTON_FONT_SIZE).strong()
}

fn cue_buttons(ui: &mut egui::Ui, shared: &MixerShared, channel: Option<usize>) {
    ui.vertical_centered(|ui| {
        const CUE_GROUP_WIDTH: f32 = 102.0;
        let leading_space = ((ui.available_width() - CUE_GROUP_WIDTH) * 0.5).max(0.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(leading_space);
            let button = |ui: &mut egui::Ui, side: &str, enabled: bool| {
                ui.add_sized(
                    [48.0, 27.0],
                    egui::Button::new(cue_button_text(&format!("{side}  CUE")))
                        .fill(if enabled {
                            ACCENT
                        } else {
                            Color32::from_rgb(55, 39, 18)
                        })
                        .stroke(Stroke::new(1.0_f32, ACCENT)),
                )
                .clicked()
            };
            let cue_a = channel.map_or_else(
                || shared.master_cue(),
                |channel| shared.channel_cue(channel),
            );
            if button(ui, "A", cue_a) {
                if let Some(channel) = channel {
                    shared.set_channel_cue(channel, !cue_a);
                } else {
                    shared.set_master_cue(!cue_a);
                }
            }
            ui.add_space(6.0);
            let cue_b = channel.map_or_else(
                || shared.master_cue_b(),
                |channel| shared.channel_cue_b(channel),
            );
            if button(ui, "B", cue_b) {
                if let Some(channel) = channel {
                    shared.set_channel_cue_b(channel, !cue_b);
                } else {
                    shared.set_master_cue_b(!cue_b);
                }
            }
        });
    });
}

fn fader_cap_length(fader_width: f32) -> f32 {
    (fader_width * 0.39 * 2.0 * 0.60).clamp(38.0, 70.0)
}

fn crossfader(ui: &mut egui::Ui, value: &mut f32, cap_length: f32) -> bool {
    let desired_width = ui.available_width().min(430.0);
    let height = (cap_length + 8.0).max(45.0);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(desired_width, height), Sense::click_and_drag());
    let track = rect.shrink2(Vec2::new(0.0, 9.0));
    let mut changed = false;
    if response.double_clicked() || response.secondary_clicked() {
        *value = 0.5;
        changed = true;
    } else if response.clicked_by(egui::PointerButton::Primary)
        || response.dragged_by(egui::PointerButton::Primary)
    {
        if let Some(pointer) = response.interact_pointer_pos() {
            *value = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0);
            changed = true;
        }
    }
    response.on_hover_text("Only channels assigned to A or B follow this fader; THRU bypasses it. Right-click or double-click to center.");

    let painter = ui.painter();
    let y = track.center().y;
    painter.line_segment(
        [egui::pos2(track.left(), y), egui::pos2(track.right(), y)],
        Stroke::new(5.0_f32, Color32::from_rgb(4, 5, 6)),
    );
    for index in 0..=10 {
        let x = egui::lerp(track.left()..=track.right(), index as f32 / 10.0);
        painter.line_segment(
            [egui::pos2(x, y - 12.0), egui::pos2(x, y + 12.0)],
            Stroke::new(1.0_f32, Color32::from_rgb(105, 108, 113)),
        );
    }
    let x = egui::lerp(track.left()..=track.right(), *value);
    // Same 17 px thickness and long dimension as the channel-fader cap,
    // rotated for horizontal travel.
    let cap = egui::Rect::from_center_size(egui::pos2(x, y), Vec2::new(17.0, cap_length));
    painter.rect_filled(cap, 2.0, Color32::from_rgb(65, 68, 73));
    painter.rect_stroke(
        cap,
        2.0,
        Stroke::new(1.0_f32, Color32::from_rgb(170, 173, 178)),
    );
    painter.line_segment(
        [
            egui::pos2(cap.center().x, cap.top() + 4.0),
            egui::pos2(cap.center().x, cap.bottom() - 4.0),
        ],
        Stroke::new(2.0_f32, Color32::WHITE),
    );
    changed
}

fn level_meter(ui: &mut egui::Ui, peaks: [f32; 2], height: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(12.0, height), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 1.0, Color32::from_rgb(6, 7, 8));
    painter.rect_stroke(
        rect,
        1.0,
        Stroke::new(1.0_f32, Color32::from_rgb(50, 53, 57)),
    );

    // 25 segments at 3 dB intervals: -60, -57, ... -3, 0, 3, 6, 9, 12.
    const SEGMENTS: usize = 25;
    let side_pad = 3.0;
    let column_width = rect.width() - side_pad * 2.0;
    let segment_gap = 2.0;
    let segment_height =
        (rect.height() - side_pad * 2.0 - segment_gap * (SEGMENTS.saturating_sub(1)) as f32)
            / SEGMENTS as f32;
    let peak = peaks[0].max(peaks[1]);
    let left = rect.left() + side_pad;
    for segment in 0..SEGMENTS {
        let segment_db = -60.0 + segment as f32 * 3.0;
        let threshold = ((segment_db + 60.0) / 72.0).clamp(0.0, 1.0);
        let bottom = rect.bottom() - side_pad - segment as f32 * (segment_height + segment_gap);
        let segment_rect = egui::Rect::from_min_max(
            egui::pos2(left, bottom - segment_height),
            egui::pos2(left + column_width, bottom),
        );
        let active = peak > 0.0 && peak >= threshold;
        let lit = if segment_db >= 12.0 {
            Color32::from_rgb(235, 62, 48)
        } else if segment_db >= 0.0 {
            Color32::from_rgb(235, 184, 40)
        } else {
            Color32::from_rgb(55, 205, 91)
        };
        let dark = Color32::from_rgb(lit.r() / 5, lit.g() / 5, lit.b() / 5);
        painter.rect_filled(segment_rect, 0.5, if active { lit } else { dark });
    }
}

fn channel_settings(shared: &MixerShared, channel: usize) -> MixerChannelSettings {
    MixerChannelSettings {
        trim: shared.get(channel, MixerParameter::Trim),
        high: shared.get(channel, MixerParameter::High),
        mid: shared.get(channel, MixerParameter::Mid),
        low: shared.get(channel, MixerParameter::Low),
        filter: shared.get(channel, MixerParameter::Filter),
        fader: shared.get(channel, MixerParameter::Fader),
        cross_assignment: Some(shared.cross_assignment(channel)),
        eq_isolator: shared.eq_isolator(),
    }
}

fn set_channel_settings(shared: &MixerShared, channel: usize, settings: MixerChannelSettings) {
    for (parameter, value) in [
        (MixerParameter::Trim, settings.trim),
        (MixerParameter::High, settings.high),
        (MixerParameter::Mid, settings.mid),
        (MixerParameter::Low, settings.low),
        (MixerParameter::Filter, settings.filter),
        (MixerParameter::Fader, settings.fader),
    ] {
        shared.set(channel, parameter, value);
    }
    shared.set_cross_assignment(
        channel,
        settings
            .cross_assignment
            .unwrap_or(if channel < 2 { 0 } else { 2 }),
    );
}

fn master_settings(shared: &MixerShared) -> MixerMasterSettings {
    MixerMasterSettings {
        level: shared.master_level(),
        cue_a_level: shared.cue_level(0),
        cue_b_level: shared.cue_level(1),
        eq_isolator: shared.eq_isolator(),
        fx_type: shared.fx_type(),
        fx_channel: shared.fx_channel(),
        fx_enabled: shared.fx_enabled(),
        fx_depth: shared.fx_depth(),
        fx_bands: shared.fx_bands(),
    }
}

fn set_master_settings(shared: &MixerShared, settings: MixerMasterSettings) {
    shared.set_master_level(settings.level);
    shared.set_cue_level(0, settings.cue_a_level);
    shared.set_cue_level(1, settings.cue_b_level);
    shared.set_eq_isolator(settings.eq_isolator);
    shared.set_fx_type(settings.fx_type);
    shared.set_fx_channel(settings.fx_channel);
    shared.set_fx_enabled(settings.fx_enabled);
    shared.set_fx_depth(settings.fx_depth);
    shared.set_fx_bands(settings.fx_bands);
}

fn align_cue_row(ui: &mut egui::Ui, strip_top: f32) {
    ui.add_space((strip_top + CUE_ROW_TOP - ui.cursor().top()).max(0.0));
}

#[cfg(test)]
mod tests {
    #[test]
    fn mixer_logo_svg_renders_at_header_size() {
        let image = egui_extras::image::load_svg_bytes_with_size(
            include_bytes!("../../assets/phasim-virtual-mixer.svg"),
            Some(egui::load::SizeHint::Width(260)),
        )
        .expect("mixer logo SVG must render");
        assert_eq!(image.size[0], 260);
        assert!(image.pixels.iter().any(|pixel| pixel.a() != 0));
    }
}
