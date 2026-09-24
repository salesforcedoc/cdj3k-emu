//! Per-player audio latency profile editor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cdj3k_emu_platform::menu_state::{self, AudioLatencyProfile, AudioLatencySettings};
use egui::{Color32, RichText};

pub(super) struct AudioLatencyPanel {
    open: bool,
    focused_after_open: bool,
    draft: AudioLatencySettings,
}

impl AudioLatencyPanel {
    pub(super) fn new(settings: AudioLatencySettings) -> Self {
        Self {
            open: false,
            focused_after_open: false,
            draft: settings,
        }
    }

    pub(super) fn open(&mut self, settings: AudioLatencySettings) {
        self.draft = settings;
        self.open = true;
        self.focused_after_open = false;
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            self.focused_after_open = false;
            return;
        }

        let close = Arc::new(AtomicBool::new(false));
        let close_inner = Arc::clone(&close);
        let need_focus = !self.focused_after_open;
        let this = &mut *self;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("audio_latency_profile"),
            egui::ViewportBuilder::default()
                .with_title("Audio Latency Profile")
                .with_inner_size([520.0, 510.0])
                .with_min_inner_size([480.0, 470.0])
                .with_resizable(true)
                .with_active(true),
            move |ctx, _class| {
                if ctx.input(|input| input.viewport().close_requested()) {
                    close_inner.store(true, Ordering::Relaxed);
                }
                if need_focus {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                egui::CentralPanel::default().show(ctx, |ui| this.draw(ui, &close_inner));
            },
        );
        self.focused_after_open = true;
        if close.load(Ordering::Relaxed) {
            self.open = false;
        }
    }

    fn draw(&mut self, ui: &mut egui::Ui, close: &AtomicBool) {
        let instance_id = menu_state::lock().current_instance_id;
        ui.heading(format!("Player {instance_id} Audio Latency"));
        ui.label(
            "Profiles tune CoreAudio buffering, startup prefill, and the guest's virtio-sound transfer size.",
        );
        ui.add_space(10.0);

        for (profile, detail) in [
            (
                AudioLatencyProfile::Safe,
                "More startup headroom for loopback, aggregate, Bluetooth, or busy systems.",
            ),
            (
                AudioLatencyProfile::Balanced,
                "Current proven defaults: 2,048-frame prefill and 1,024-frame guest transfers.",
            ),
            (
                AudioLatencyProfile::LowLatency,
                "Smaller 1,024-frame prefill and 512-frame transfers; best with a stable local output.",
            ),
            (
                AudioLatencyProfile::Custom,
                "Choose values within the validated operating range.",
            ),
        ] {
            ui.radio_value(&mut self.draft.profile, profile, profile.label());
            ui.indent(profile.label(), |ui| {
                ui.label(RichText::new(detail).small().color(Color32::GRAY));
            });
        }

        ui.add_space(10.0);
        ui.add_enabled_ui(self.draft.profile == AudioLatencyProfile::Custom, |ui| {
            ui.group(|ui| {
                ui.strong("Custom values");
                ui.horizontal(|ui| {
                    ui.label("Guest transfer frames");
                    ui.add(
                        egui::DragValue::new(&mut self.draft.custom.guest_transfer_frames)
                            .range(512..=2048)
                            .speed(64.0),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Startup prefill frames");
                    ui.add(
                        egui::DragValue::new(&mut self.draft.custom.startup_prefill_frames)
                            .range(1024..=8192)
                            .speed(64.0),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("CoreAudio buffer");
                    ui.add(
                        egui::DragValue::new(&mut self.draft.custom.coreaudio_buffer_us)
                            .range(3000..=30000)
                            .speed(500.0)
                            .suffix(" µs"),
                    );
                });
                ui.label(
                    RichText::new(
                        "Values are rounded/clamped on Apply: transfers use 64-frame steps and prefill covers at least two transfers.",
                    )
                    .small()
                    .color(Color32::GRAY),
                );
            });
        });

        let mut effective = self.draft.effective();
        if self.draft.profile == AudioLatencyProfile::Custom {
            effective = AudioLatencySettings::sanitize_custom(effective);
        }
        ui.add_space(10.0);
        ui.label(format!(
            "Effective: {} prefill frames · {} transfer frames · {} µs CoreAudio",
            effective.startup_prefill_frames,
            effective.guest_transfer_frames,
            effective.coreaudio_buffer_us
        ));
        ui.label(
            RichText::new(format!(
                "At 96 kHz: {:.1} ms prefill, {:.1} ms per guest transfer",
                frames_ms(effective.startup_prefill_frames, 96_000),
                frames_ms(effective.guest_transfer_frames, 96_000)
            ))
            .small()
            .color(Color32::GRAY),
        );

        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if ui.button("Apply & Restart This Player").clicked() {
                if self.draft.profile == AudioLatencyProfile::Custom {
                    self.draft.custom = AudioLatencySettings::sanitize_custom(self.draft.custom);
                }
                let mut state = menu_state::lock();
                state.audio_latency = self.draft;
                state.audio_latency_apply_requested = true;
                if state.audio_enabled && state.qemu_running {
                    state.shade_forced = true;
                }
                close.store(true, Ordering::Relaxed);
            }
            if ui.button("Cancel").clicked() {
                close.store(true, Ordering::Relaxed);
            }
        });
        ui.label(
            RichText::new(
                "The guest transfer size is fixed when ALSA opens, so changing profiles restarts this player. It does not restart other players.",
            )
            .small()
            .color(Color32::GRAY),
        );
    }
}

fn frames_ms(frames: u32, sample_rate: u32) -> f32 {
    frames as f32 * 1000.0 / sample_rate as f32
}
