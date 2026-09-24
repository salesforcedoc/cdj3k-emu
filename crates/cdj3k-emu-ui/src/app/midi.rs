//! Per-player host MIDI input and shared assignment UI.
//!
//! Every emulator instance owns a persisted [`MidiAssignments`] value. The
//! input thread connects to all CoreMIDI sources, while the UI thread filters
//! messages by this player's source/channel/message bindings before injecting
//! them into the emulated SPI frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cdj3k_emu_platform::menu_state;
use cdj3k_emu_platform::mixer::MixerParameter;
use cdj3k_emu_storage::{
    MidiAssignments, MidiBinding, MidiJogMode, MidiMessageKind, MidiMixerAssignments,
    MixerMidiExtra, MIDI_EXTRA_BUTTON_KEYS,
};
use cdj3k_emu_streams::RepaintGate;
use cdj3k_emu_subucom::{miso_frame, Direction};
use egui::{Color32, ComboBox, Frame, Grid, Margin, RichText, ScrollArea};
use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput};

use super::CdjApp;

/// Polling is a fallback for CoreMIDI devices/drivers that do not leave a
/// usable connection after hot-plug. Discovery is cheap and reconciliation
/// only touches endpoints whose stable IDs changed.
const PORT_SCAN_INTERVAL: Duration = Duration::from_millis(500);
/// The guest keeps the latest injected control frame. A controller can send
/// Note On and Note Off before its next read, so keep short presses visible
/// long enough for the firmware to observe a press edge.
const MIN_MIDI_BUTTON_PRESS: Duration = Duration::from_millis(60);
/// Keep a recent movement visibly engaged across the debug pane's 60 Hz tick.
const MIDI_JOG_ACTIVE_WINDOW: Duration = Duration::from_millis(250);
/// First jog event (or one after a long pause) gets one display frame of
/// movement instead of integrating the entire time since the previous event.
const JOG_INITIAL_SAMPLE_SEC: f32 = 1.0 / 60.0;
const JOG_MAX_SAMPLE_SEC: f32 = 0.05;
/// Default movement represented by one relative encoder count.
const DEFAULT_JOG_RELATIVE_STEPS_PER_REV: u16 = 128;

#[derive(Clone, Debug, Default, PartialEq)]
struct MidiLedDesired {
    /// Only the focused deck drives its FLX4 pad/transport bank.
    deck: Option<(u8, [bool; 11])>,
    /// The one shared-mixer owner drives learned Cue A/B and FX button lights.
    mixer: Vec<([u8; 2], bool)>,
}

fn deck_led_messages(bank: u8, states: [bool; 11]) -> [[u8; 3]; 11] {
    std::array::from_fn(|index| {
        let (status, number) = if index < 8 {
            (if bank == 0 { 0x97 } else { 0x99 }, index as u8)
        } else {
            (
                if bank == 0 { 0x90 } else { 0x91 },
                match index {
                    8 => 0x10,
                    9 => 0x11,
                    _ => 0x58,
                },
            )
        };
        [status, number, if states[index] { 0x7f } else { 0 }]
    })
}

/// Empty CDJ hot-cue pads can have a dim neutral-white RGB drive. It is a
/// backlight state, not an occupied cue. The firmware's cue/loop colors have
/// chroma; use that distinction instead of treating any PWM as occupied.
fn hot_cue_led_has_assignment(rgb: (u8, u8, u8)) -> bool {
    let (r, g, b) = rgb;
    let brightest = r.max(g).max(b) as u16;
    let darkest = r.min(g).min(b) as u16;
    let chroma = brightest - darkest;
    chroma >= 2 && chroma * 5 >= brightest
}

fn midi_led_output_loop(
    desired: Arc<Mutex<MidiLedDesired>>,
    stop: Arc<AtomicBool>,
    reconnect: Arc<AtomicUsize>,
) {
    let mut connection = None;
    let mut port_name = String::new();
    let mut sent = MidiLedDesired::default();
    let mut last_reconnect = reconnect.load(Ordering::Relaxed);
    let mut last_refresh = Instant::now();
    let mut next_scan = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let generation = reconnect.load(Ordering::Relaxed);
        if generation != last_reconnect {
            connection = None;
            sent = MidiLedDesired::default();
            last_reconnect = generation;
        }
        if connection.is_none() && Instant::now() >= next_scan {
            next_scan = Instant::now() + PORT_SCAN_INTERVAL;
            if let Ok(output) = MidiOutput::new("cdj3k-emu LEDs") {
                if let Some(port) = output
                    .ports()
                    .into_iter()
                    .find(|port| output.port_name(port).is_ok_and(|name| is_ddj_flx4(&name)))
                {
                    port_name = output.port_name(&port).unwrap_or_default();
                    if let Ok(open) = output.connect(&port, "cdj3k-emu FLX4 LEDs") {
                        connection = Some(open);
                        sent = MidiLedDesired::default();
                    }
                }
            }
        }
        if let Some(open) = &mut connection {
            let current = desired
                .lock()
                .map(|state| state.clone())
                .unwrap_or_default();
            // Refresh periodically: this also detects a stale CoreMIDI output
            // connection after unplug/replug even if no LED state changed.
            let refresh = last_refresh.elapsed() >= Duration::from_secs(2);
            let mut failed = false;
            if let Some((bank, states)) = current.deck {
                if refresh || sent.deck != current.deck {
                    for message in deck_led_messages(bank, states) {
                        if open.send(&message).is_err() {
                            failed = true;
                            break;
                        }
                    }
                }
            }
            if !failed && (refresh || sent.mixer != current.mixer) {
                for (address, _) in &sent.mixer {
                    if !current.mixer.iter().any(|(next, _)| next == address) {
                        if open.send(&[address[0], address[1], 0]).is_err() {
                            failed = true;
                            break;
                        }
                    }
                }
                if !failed {
                    for (address, lit) in &current.mixer {
                        if open
                            .send(&[address[0], address[1], if *lit { 0x7f } else { 0 }])
                            .is_err()
                        {
                            failed = true;
                            break;
                        }
                    }
                }
            }
            if failed {
                eprintln!("[midi] FLX4 LED output disconnected: {port_name}; retrying");
                connection = None;
            } else {
                sent = current;
            }
            if refresh {
                last_refresh = Instant::now();
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct MidiButtonId {
    source: u64,
    channel: u8,
    kind: MidiMessageKind,
    number: u8,
    button: (usize, u8),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct MidiHeldButton {
    pub(super) button: (usize, u8),
    pressed_at: Instant,
    release_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MidiJogActivity {
    last_at: Option<Instant>,
    last_value: Option<u8>,
    last_delta_rad: f32,
    events: u64,
}

impl MidiJogActivity {
    fn record(&mut self, value: u8, delta_rad: f32, at: Instant) {
        self.last_at = Some(at);
        self.last_value = Some(value);
        self.last_delta_rad = delta_rad;
        self.events = self.events.saturating_add(1);
    }

    fn moving(&self, now: Instant) -> bool {
        self.last_delta_rad != 0.0
            && self.last_at.is_some_and(|at| {
                now.checked_duration_since(at)
                    .is_some_and(|age| age <= MIDI_JOG_ACTIVE_WINDOW)
            })
    }
}

fn update_midi_button(
    held: &mut HashMap<MidiButtonId, MidiHeldButton>,
    id: MidiButtonId,
    pressed: bool,
    now: Instant,
) -> bool {
    if pressed {
        if let Some(existing) = held.get_mut(&id) {
            existing.release_at = None;
            false
        } else {
            held.insert(
                id,
                MidiHeldButton {
                    button: id.button,
                    pressed_at: now,
                    release_at: None,
                },
            );
            true
        }
    } else if let Some(existing) = held.get_mut(&id) {
        let deadline = existing.pressed_at + MIN_MIDI_BUTTON_PRESS;
        if now >= deadline {
            held.remove(&id);
            true
        } else {
            existing.release_at = Some(deadline);
            false
        }
    } else {
        false
    }
}

fn expire_midi_buttons(held: &mut HashMap<MidiButtonId, MidiHeldButton>, now: Instant) -> bool {
    let before = held.len();
    held.retain(|_, button| button.release_at.is_none_or(|deadline| deadline > now));
    held.len() != before
}

fn release_unfocused_rotary_press(
    held: &mut HashMap<MidiButtonId, MidiHeldButton>,
    deck_window_focused: bool,
) -> bool {
    if deck_window_focused {
        return false;
    }
    let before = held.len();
    held.retain(|id, _| id.button != miso_frame::BTN_ROTARY_PRESS);
    held.len() != before
}

fn button_allowed_for_focus(button: (usize, u8), deck_window_focused: bool) -> bool {
    button != miso_frame::BTN_ROTARY_PRESS || deck_window_focused
}

#[derive(Clone, Debug, PartialEq)]
enum MidiMessage {
    Note {
        channel: u8,
        note: u8,
        pressed: bool,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
}

impl MidiMessage {
    fn channel(&self) -> u8 {
        match *self {
            Self::Note { channel, .. } | Self::ControlChange { channel, .. } => channel,
        }
    }

    fn kind_and_number(&self) -> (MidiMessageKind, u8) {
        match *self {
            Self::Note { note, .. } => (MidiMessageKind::Note, note),
            Self::ControlChange { controller, .. } => (MidiMessageKind::ControlChange, controller),
        }
    }

    fn button_pressed(&self) -> bool {
        match *self {
            Self::Note { pressed, .. } => pressed,
            Self::ControlChange { value, .. } => value != 0,
        }
    }
}

fn activation_press_edge(
    held: &mut std::collections::HashSet<(u64, u8, MidiMessageKind, u8)>,
    binding: Option<&MidiBinding>,
    source: u64,
    port_name: &str,
    message: &MidiMessage,
) -> bool {
    let Some(binding) = binding else { return false };
    let channel = message.channel();
    let (kind, number) = message.kind_and_number();
    if !binding_matches(binding, port_name, channel, kind, number) {
        return false;
    }
    let id = (source, channel, kind, number);
    if message.button_pressed() {
        held.insert(id)
    } else {
        held.remove(&id);
        false
    }
}

#[derive(Clone, Debug)]
enum MidiEvent {
    Message {
        source: u64,
        port_name: Arc<str>,
        message: MidiMessage,
        at: Instant,
    },
    ResetButtons,
    ScanComplete {
        request: MidiScanRequest,
        discovered: usize,
        connected: usize,
        failures: Vec<String>,
    },
    ScanFailed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MidiScanRequest {
    /// Discover additions/removals while leaving healthy connections alone.
    Refresh,
    /// Recreate every endpoint, but connect replacements before dropping the
    /// old handles so the explicit Rescan button has no blind window.
    ReconnectAll,
    /// Close all CoreMIDI clients before discovering and reconnecting ports.
    /// This recovers handles left stale by a fast unplug/replug.
    Reset,
}

impl MidiScanRequest {
    fn priority(self) -> u8 {
        match self {
            Self::Refresh => 0,
            Self::ReconnectAll => 1,
            Self::Reset => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum MidiTarget {
    Play,
    Cue,
    HotCue(usize),
    Tempo,
    CallDelete,
    LoopIn,
    LoopOut,
    ReloopExit,
    BeatSync,
    MasterTempo,
    TempoRange,
    TempoReset,
    JogPlatter,
    JogRing,
    JogTouch,
    RotaryTurn,
    ActivateDeck,
    DirectionReverse,
    DirectionSlipReverse,
    Extra(usize),
    MixerTrim,
    MixerHigh,
    MixerMid,
    MixerLow,
    MixerFilter,
    MixerFader,
    MixerExtra(MixerMidiExtra),
}

const EXTRA_BUTTONS: [(&str, (usize, u8)); 33] = [
    ("Search ◀◀", miso_frame::BTN_SEARCH_PREV),
    ("Search ▶▶", miso_frame::BTN_SEARCH_NEXT),
    ("Track ◀", miso_frame::BTN_TRACK_PREV),
    ("Track ▶", miso_frame::BTN_TRACK_NEXT),
    ("Beat Jump ◀", miso_frame::BTN_BEATJUMP_PREV),
    ("Beat Jump ▶", miso_frame::BTN_BEATJUMP_NEXT),
    ("Auto Cue", miso_frame::BTN_AUTO_CUE),
    ("Key Sync", miso_frame::BTN_KEY_SYNC),
    ("Master", miso_frame::BTN_MASTER),
    ("Beat Loop ½", miso_frame::BTN_BEATLOOP_HALF),
    ("Beat Loop 2×", miso_frame::BTN_BEATLOOP_2X),
    ("Slip", miso_frame::BTN_SLIP),
    ("Memory", miso_frame::BTN_MEMORY),
    ("Delete", miso_frame::BTN_DELETE),
    ("Call ◀", miso_frame::BTN_CALL_PREV),
    ("Call ▶", miso_frame::BTN_CALL_NEXT),
    ("Source", miso_frame::BTN_SOURCE),
    ("Browse", miso_frame::BTN_BROWSE),
    ("Tag List", miso_frame::BTN_TAG_LIST),
    ("Playlist", miso_frame::BTN_PLAYLIST),
    ("Search Menu", miso_frame::BTN_SEARCH_MENU),
    ("Menu", miso_frame::BTN_MENU),
    ("Jog Mode", miso_frame::BTN_JOG_MODE),
    ("Back", miso_frame::BTN_BACK),
    ("Tag Track", miso_frame::BTN_TAG_TRACK),
    ("Track Filter", miso_frame::BTN_TRACK_FILTER),
    ("Shortcut", miso_frame::BTN_SHORTCUT),
    ("Rotary Press (active deck)", miso_frame::BTN_ROTARY_PRESS),
    ("Sleep", miso_frame::BTN_SLEEP),
    ("Time Mode", miso_frame::BTN_TIME_MODE),
    ("Quantize", miso_frame::BTN_QUANTIZE),
    ("USB Stop", miso_frame::BTN_USB_STOP),
    ("Power On", miso_frame::BTN_POWER_ON),
];
const _: [(); EXTRA_BUTTONS.len()] = [(); MIDI_EXTRA_BUTTON_KEYS.len()];

impl MidiTarget {
    const DECK: [Self; 26] = [
        Self::Play,
        Self::Cue,
        Self::HotCue(0),
        Self::HotCue(1),
        Self::HotCue(2),
        Self::HotCue(3),
        Self::HotCue(4),
        Self::HotCue(5),
        Self::HotCue(6),
        Self::HotCue(7),
        Self::Tempo,
        Self::CallDelete,
        Self::LoopIn,
        Self::LoopOut,
        Self::ReloopExit,
        Self::BeatSync,
        Self::MasterTempo,
        Self::TempoRange,
        Self::TempoReset,
        Self::JogPlatter,
        Self::JogRing,
        Self::JogTouch,
        Self::RotaryTurn,
        Self::ActivateDeck,
        Self::DirectionReverse,
        Self::DirectionSlipReverse,
    ];

    const MIXER: [Self; 6] = [
        Self::MixerTrim,
        Self::MixerHigh,
        Self::MixerMid,
        Self::MixerLow,
        Self::MixerFilter,
        Self::MixerFader,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Play => "Play/Pause",
            Self::Cue => "Cue",
            Self::HotCue(0) => "Hot Cue A",
            Self::HotCue(1) => "Hot Cue B",
            Self::HotCue(2) => "Hot Cue C",
            Self::HotCue(3) => "Hot Cue D",
            Self::HotCue(4) => "Hot Cue E",
            Self::HotCue(5) => "Hot Cue F",
            Self::HotCue(6) => "Hot Cue G",
            Self::HotCue(7) => "Hot Cue H",
            Self::HotCue(_) => "Hot Cue",
            Self::Tempo => "Pitch/Tempo",
            Self::CallDelete => "Hot Cue Call/Delete",
            Self::LoopIn => "Loop In",
            Self::LoopOut => "Loop Out",
            Self::ReloopExit => "Reloop/Exit",
            Self::BeatSync => "Beat Sync",
            Self::MasterTempo => "Master Tempo",
            Self::TempoRange => "Tempo Range",
            Self::TempoReset => "Tempo Reset",
            Self::JogPlatter => "Jog Platter",
            Self::JogRing => "Jog Outer Ring",
            Self::JogTouch => "Jog Touch",
            Self::RotaryTurn => "Rotary Turn (active deck)",
            Self::ActivateDeck => "Activate Deck Window",
            Self::DirectionReverse => "Reverse",
            Self::DirectionSlipReverse => "Slip Reverse",
            Self::Extra(index) => EXTRA_BUTTONS[index].0,
            Self::MixerTrim => "Mixer Trim",
            Self::MixerHigh => "Mixer High EQ",
            Self::MixerMid => "Mixer Mid EQ",
            Self::MixerLow => "Mixer Low EQ",
            Self::MixerFilter => "Mixer Filter",
            Self::MixerFader => "Mixer Channel Fader",
            Self::MixerExtra(extra) => match extra {
                MixerMidiExtra::CrossAssign => "Crossfader Assign",
                MixerMidiExtra::Crossfader => "Crossfader",
                MixerMidiExtra::CueA => "Cue A",
                MixerMidiExtra::CueB => "Cue B",
                MixerMidiExtra::EqCurve => "EQ/ISO Curve",
                MixerMidiExtra::MasterLevel => "Master Level",
                MixerMidiExtra::CueALevel => "Cue A Level",
                MixerMidiExtra::CueBLevel => "Cue B Level",
                MixerMidiExtra::FxType => "FX Selector",
                MixerMidiExtra::FxChannel => "CH Select",
                MixerMidiExtra::FxOn => "FX On/Off",
                MixerMidiExtra::FxDepth => "Level/Depth",
                MixerMidiExtra::FxLow => "FX Frequency Low",
                MixerMidiExtra::FxMid => "FX Frequency Mid",
                MixerMidiExtra::FxHigh => "FX Frequency Hi",
            },
        }
    }

    fn button(self) -> Option<(usize, u8)> {
        const HOT_CUES: [(usize, u8); 8] = [
            miso_frame::BTN_HOT_A,
            miso_frame::BTN_HOT_B,
            miso_frame::BTN_HOT_C,
            miso_frame::BTN_HOT_D,
            miso_frame::BTN_HOT_E,
            miso_frame::BTN_HOT_F,
            miso_frame::BTN_HOT_G,
            miso_frame::BTN_HOT_H,
        ];
        match self {
            Self::Play => Some(miso_frame::BTN_PLAY),
            Self::Cue => Some(miso_frame::BTN_CUE),
            Self::HotCue(index) => HOT_CUES.get(index).copied(),
            Self::Extra(index) => EXTRA_BUTTONS.get(index).map(|(_, button)| *button),
            Self::CallDelete => Some(miso_frame::BTN_CALL_DELETE),
            Self::LoopIn => Some(miso_frame::BTN_LOOP_IN),
            Self::LoopOut => Some(miso_frame::BTN_LOOP_OUT),
            Self::ReloopExit => Some(miso_frame::BTN_RELOOP),
            Self::BeatSync => Some(miso_frame::BTN_BEAT_SYNC),
            Self::MasterTempo => Some(miso_frame::BTN_MASTER_TEMPO),
            Self::TempoRange => Some(miso_frame::BTN_TEMPO_RANGE),
            Self::TempoReset => Some(miso_frame::BTN_TEMPO_RESET),
            Self::Tempo
            | Self::JogPlatter
            | Self::JogRing
            | Self::JogTouch
            | Self::RotaryTurn
            | Self::ActivateDeck
            | Self::DirectionReverse
            | Self::DirectionSlipReverse
            | Self::MixerTrim
            | Self::MixerHigh
            | Self::MixerMid
            | Self::MixerLow
            | Self::MixerFilter
            | Self::MixerFader => None,
            Self::MixerExtra(_) => None,
        }
    }

    fn binding(self, assignments: &MidiAssignments) -> &Option<MidiBinding> {
        match self {
            Self::Play => &assignments.play,
            Self::Cue => &assignments.cue,
            Self::HotCue(index) => &assignments.hot_cues[index],
            Self::Tempo => &assignments.tempo,
            Self::CallDelete => &assignments.call_delete,
            Self::LoopIn => &assignments.loop_in,
            Self::LoopOut => &assignments.loop_out,
            Self::ReloopExit => &assignments.reloop_exit,
            Self::BeatSync => &assignments.beat_sync,
            Self::MasterTempo => &assignments.master_tempo,
            Self::TempoRange => &assignments.tempo_range,
            Self::TempoReset => &assignments.tempo_reset,
            Self::JogPlatter => &assignments.jog_platter,
            Self::JogRing => &assignments.jog_ring,
            Self::JogTouch => &assignments.jog_touch,
            Self::RotaryTurn => &assignments.rotary_turn,
            Self::ActivateDeck => &assignments.activate_deck,
            Self::DirectionReverse => &assignments.direction_reverse,
            Self::DirectionSlipReverse => &assignments.direction_slip_reverse,
            Self::Extra(index) => &assignments.extra_buttons[index],
            Self::MixerTrim => &assignments.mixer_trim,
            Self::MixerHigh => &assignments.mixer_high,
            Self::MixerMid => &assignments.mixer_mid,
            Self::MixerLow => &assignments.mixer_low,
            Self::MixerFilter => &assignments.mixer_filter,
            Self::MixerFader => &assignments.mixer_fader,
            Self::MixerExtra(_) => unreachable!("mixer extras use shared assignments"),
        }
    }

    fn set_binding(self, assignments: &mut MidiAssignments, binding: Option<MidiBinding>) {
        match self {
            Self::Play => assignments.play = binding,
            Self::Cue => assignments.cue = binding,
            Self::HotCue(index) => assignments.hot_cues[index] = binding,
            Self::Tempo => assignments.tempo = binding,
            Self::CallDelete => assignments.call_delete = binding,
            Self::LoopIn => assignments.loop_in = binding,
            Self::LoopOut => assignments.loop_out = binding,
            Self::ReloopExit => assignments.reloop_exit = binding,
            Self::BeatSync => assignments.beat_sync = binding,
            Self::MasterTempo => assignments.master_tempo = binding,
            Self::TempoRange => assignments.tempo_range = binding,
            Self::TempoReset => assignments.tempo_reset = binding,
            Self::JogPlatter => assignments.jog_platter = binding,
            Self::JogRing => assignments.jog_ring = binding,
            Self::JogTouch => assignments.jog_touch = binding,
            Self::RotaryTurn => assignments.rotary_turn = binding,
            Self::ActivateDeck => assignments.activate_deck = binding,
            Self::DirectionReverse => assignments.direction_reverse = binding,
            Self::DirectionSlipReverse => assignments.direction_slip_reverse = binding,
            Self::Extra(index) => assignments.extra_buttons[index] = binding,
            Self::MixerTrim => assignments.mixer_trim = binding,
            Self::MixerHigh => assignments.mixer_high = binding,
            Self::MixerMid => assignments.mixer_mid = binding,
            Self::MixerLow => assignments.mixer_low = binding,
            Self::MixerFilter => assignments.mixer_filter = binding,
            Self::MixerFader => assignments.mixer_fader = binding,
            Self::MixerExtra(_) => unreachable!("mixer extras use shared assignments"),
        }
    }

    fn jog_mode(self, assignments: &MidiAssignments) -> Option<MidiJogMode> {
        match self {
            Self::JogPlatter => Some(assignments.jog_platter_mode),
            Self::JogRing => Some(assignments.jog_ring_mode),
            Self::RotaryTurn => Some(assignments.rotary_turn_mode),
            _ => None,
        }
    }

    fn set_jog_mode(self, assignments: &mut MidiAssignments, mode: MidiJogMode) {
        match self {
            Self::JogPlatter => assignments.jog_platter_mode = mode,
            Self::JogRing => assignments.jog_ring_mode = mode,
            Self::RotaryTurn => assignments.rotary_turn_mode = mode,
            _ => {}
        }
    }

    fn jog_steps_per_rev(self, assignments: &MidiAssignments) -> Option<u16> {
        match self {
            Self::JogPlatter => Some(assignments.jog_platter_steps_per_rev),
            Self::JogRing => Some(assignments.jog_ring_steps_per_rev),
            _ => None,
        }
    }

    fn set_jog_steps_per_rev(self, assignments: &mut MidiAssignments, steps: u16) {
        match self {
            Self::JogPlatter => assignments.jog_platter_steps_per_rev = steps.clamp(1, 8192),
            Self::JogRing => assignments.jog_ring_steps_per_rev = steps.clamp(1, 8192),
            _ => {}
        }
    }

    fn mixer_parameter(self) -> Option<MixerParameter> {
        match self {
            Self::MixerTrim => Some(MixerParameter::Trim),
            Self::MixerHigh => Some(MixerParameter::High),
            Self::MixerMid => Some(MixerParameter::Mid),
            Self::MixerLow => Some(MixerParameter::Low),
            Self::MixerFilter => Some(MixerParameter::Filter),
            Self::MixerFader => Some(MixerParameter::Fader),
            Self::MixerExtra(_) => None,
            _ => None,
        }
    }

    fn requires_cc(self) -> bool {
        matches!(
            self,
            Self::Tempo
                | Self::JogPlatter
                | Self::JogRing
                | Self::RotaryTurn
                | Self::MixerTrim
                | Self::MixerHigh
                | Self::MixerMid
                | Self::MixerLow
                | Self::MixerFilter
                | Self::MixerFader
                | Self::MixerExtra(
                    MixerMidiExtra::CrossAssign
                        | MixerMidiExtra::Crossfader
                        | MixerMidiExtra::MasterLevel
                        | MixerMidiExtra::CueALevel
                        | MixerMidiExtra::CueBLevel
                        | MixerMidiExtra::FxType
                        | MixerMidiExtra::FxChannel
                        | MixerMidiExtra::FxDepth
                )
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MidiTab {
    Slot(usize),
    Mixer,
}

impl MidiTab {
    fn from_shared(value: u32) -> Self {
        if value < 4 {
            Self::Slot(value as usize)
        } else {
            Self::Mixer
        }
    }

    fn shared_index(self) -> u32 {
        match self {
            Self::Slot(slot) => slot as u32,
            Self::Mixer => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MidiLearnSelection {
    Deck { slot: usize, target: MidiTarget },
    Mixer { channel: usize, target: MidiTarget },
}

impl MidiLearnSelection {
    fn target(self) -> MidiTarget {
        match self {
            Self::Deck { target, .. } | Self::Mixer { target, .. } => target,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Deck { slot, target } => format!("Slot {} {}", slot + 1, target.label()),
            Self::Mixer { channel, target } => {
                if channel == 4 {
                    format!("Mixer {}", target.label())
                } else {
                    format!("Mixer channel {} {}", channel + 1, target.label())
                }
            }
        }
    }
}

pub(super) struct MidiLearnState {
    pub(super) open: bool,
    tab: MidiTab,
    learning: Option<MidiLearnSelection>,
    notice: Option<String>,
    last_request_generation: u32,
    last_device_action: u32,
    next_assignments_refresh: Instant,
    suppression_until: Option<Instant>,
    slot_assignments: [MidiAssignments; 4],
    profile_name: String,
    profiles: Vec<String>,
    selected_profile: String,
}

impl MidiLearnState {
    pub(super) fn new() -> Self {
        Self {
            open: false,
            tab: MidiTab::Slot(0),
            learning: None,
            notice: None,
            last_request_generation: 0,
            last_device_action: 0,
            next_assignments_refresh: Instant::now(),
            suppression_until: None,
            slot_assignments: std::array::from_fn(|index| {
                MidiAssignments::for_instance(index as u32 + 1)
            }),
            profile_name: String::new(),
            profiles: Vec::new(),
            selected_profile: String::new(),
        }
    }
}

pub(super) struct MidiControls {
    events: Receiver<MidiEvent>,
    connected_port_count: Arc<AtomicUsize>,
    connected_port_names: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    rescan_tx: Sender<MidiScanRequest>,
    led_desired: Arc<Mutex<MidiLedDesired>>,
    led_reconnect: Arc<AtomicUsize>,
}

impl MidiControls {
    pub(super) fn new(repaint_gate: RepaintGate) -> Self {
        let (tx, events) = mpsc::channel();
        let connected_port_count = Arc::new(AtomicUsize::new(0));
        let connected_port_names = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_count = Arc::clone(&connected_port_count);
        let thread_names = Arc::clone(&connected_port_names);
        let thread_stop = Arc::clone(&stop);
        let (rescan_tx, rescan_rx) = mpsc::channel();
        let led_desired = Arc::new(Mutex::new(MidiLedDesired::default()));
        let led_reconnect = Arc::new(AtomicUsize::new(0));
        let output_desired = Arc::clone(&led_desired);
        let output_stop = Arc::clone(&stop);
        let output_reconnect = Arc::clone(&led_reconnect);
        if let Err(err) = thread::Builder::new()
            .name("midi-led-output".into())
            .spawn(move || midi_led_output_loop(output_desired, output_stop, output_reconnect))
        {
            eprintln!("[midi] failed to spawn LED output thread: {err}");
        }

        if let Err(err) = thread::Builder::new()
            .name("midi-input".into())
            .spawn(move || {
                midi_input_loop(
                    tx,
                    repaint_gate,
                    thread_count,
                    thread_names,
                    thread_stop,
                    rescan_rx,
                )
            })
        {
            eprintln!("[midi] failed to spawn input thread: {err}");
        }

        Self {
            events,
            connected_port_count,
            connected_port_names,
            stop,
            rescan_tx,
            led_desired,
            led_reconnect,
        }
    }

    fn try_recv(&self) -> Option<MidiEvent> {
        self.events.try_recv().ok()
    }

    pub(super) fn connected_port_count(&self) -> usize {
        self.connected_port_count.load(Ordering::Relaxed)
    }

    pub(super) fn connected_port_names(&self) -> Vec<String> {
        self.connected_port_names
            .lock()
            .map(|names| names.clone())
            .unwrap_or_default()
    }

    pub(super) fn request_refresh(&self) {
        let _ = self.rescan_tx.send(MidiScanRequest::Refresh);
    }

    pub(super) fn request_rescan(&self) {
        let _ = self.rescan_tx.send(MidiScanRequest::ReconnectAll);
        self.led_reconnect.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn request_reset(&self) {
        let _ = self.rescan_tx.send(MidiScanRequest::Reset);
        self.led_reconnect.fetch_add(1, Ordering::Relaxed);
    }

    fn set_led_feedback(&self, deck: Option<(u8, [bool; 11])>, mixer: Vec<([u8; 2], bool)>) {
        if let Ok(mut state) = self.led_desired.lock() {
            state.deck = deck;
            state.mixer = mixer;
        }
    }
}

impl Drop for MidiControls {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn deck_focus_from_viewports(root_focused: bool, main_focused: bool, main_open: bool) -> bool {
    root_focused || (main_open && main_focused)
}

fn deck_window_focused(ctx: &egui::Context, main_open: bool) -> bool {
    let focused = |viewport_id| {
        ctx.input_for(viewport_id, |input| {
            input.viewport().focused.unwrap_or(input.focused)
        })
    };
    let root_focused = focused(egui::ViewportId::ROOT);
    let main_focused = main_open && focused(super::viewports::main_screen_viewport_id());
    deck_focus_from_viewports(root_focused, main_focused, main_open)
}

impl CdjApp {
    pub(super) fn poll_midi(&mut self, ctx: &egui::Context) {
        let deck_window_focused = deck_window_focused(ctx, self.main_screen_popped);
        self.update_midi_led_feedback(deck_window_focused);
        if self.mixer.midi_learn_state().3 {
            self.midi_activate_deck_held.clear();
        }
        if release_unfocused_rotary_press(&mut self.midi_held_btns, deck_window_focused) {
            self.inject(self.build_current_frame().finalize());
        }
        let device_action = self.mixer.midi_device_action();
        if device_action != self.midi_learn.last_device_action {
            self.midi_learn.last_device_action = device_action;
            self.clear_midi_buttons();
            match device_action & 3 {
                1 => self.midi_controls.request_rescan(),
                2 => self.midi_controls.request_reset(),
                _ => {}
            }
        }
        let now = Instant::now();
        if now >= self.midi_learn.next_assignments_refresh {
            self.refresh_runtime_midi_assignments();
            self.midi_learn.next_assignments_refresh = now + Duration::from_millis(500);
        }
        if self
            .midi_learn
            .suppression_until
            .is_some_and(|deadline| now >= deadline)
        {
            self.midi_learn.suppression_until = None;
            self.mixer.set_midi_learn_active(false);
        }
        while let Some(event) = self.midi_controls.try_recv() {
            match event {
                MidiEvent::ResetButtons => self.clear_midi_buttons(),
                MidiEvent::ScanComplete {
                    request,
                    discovered,
                    connected,
                    failures,
                } => {
                    let action = if request == MidiScanRequest::Reset {
                        "MIDI reset"
                    } else {
                        "MIDI rescan"
                    };
                    self.midi_learn.notice = Some(if failures.is_empty() {
                        format!("{action} complete: {connected} input(s) connected")
                    } else {
                        format!(
                            "{action} found {discovered} input(s), connected to {connected}; {}. Retrying automatically…",
                            failures.join("; ")
                        )
                    });
                }
                MidiEvent::ScanFailed(error) => {
                    self.midi_learn.notice = Some(format!("CoreMIDI discovery failed: {error}"));
                }
                MidiEvent::Message {
                    source,
                    port_name,
                    message,
                    at,
                } => {
                    if let Some(selection) = self.midi_learn.learning {
                        if let Some(binding) =
                            learned_binding(selection.target(), &port_name, &message)
                        {
                            let description = binding_label(Some(&binding));
                            let save_result = self.replace_midi_binding(selection, Some(binding));
                            self.midi_learn.learning = None;
                            self.midi_learn.suppression_until =
                                Some(Instant::now() + Duration::from_millis(150));
                            self.midi_learn.notice = Some(match save_result {
                                Ok(()) => {
                                    format!("{} assigned to {description}", selection.label())
                                }
                                Err(err) => err,
                            });
                        }
                        // Suppress performance actions while waiting for a
                        // learn message so assigning a pad cannot start audio.
                        continue;
                    }

                    // Another player may own the shared Learn window. Never
                    // actuate an existing binding while it is learning one.
                    if self.mixer.midi_learn_state().3 {
                        continue;
                    }

                    if activation_press_edge(
                        &mut self.midi_activate_deck_held,
                        self.midi_assignments.activate_deck.as_ref(),
                        source,
                        &port_name,
                        &message,
                    ) {
                        // Focus targets this process's root deck viewport. On
                        // macOS, winit activates the process and orders its
                        // NSWindow in front; restore a minimized deck first.
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }

                    let changed = self.apply_midi_message(
                        source,
                        &port_name,
                        &message,
                        at,
                        deck_window_focused,
                    );
                    if changed {
                        self.inject(self.build_current_frame().finalize());
                    }
                }
            }
        }

        let now = Instant::now();
        if expire_midi_buttons(&mut self.midi_held_btns, now) {
            self.inject(self.build_current_frame().finalize());
        }
        if let Some(deadline) = self
            .midi_held_btns
            .values()
            .filter_map(|held| held.release_at)
            .min()
        {
            ctx.request_repaint_after(deadline.saturating_duration_since(now));
        }
        if self.debug_screen_popped {
            for activity in &self.midi_jog_activity {
                if activity.moving(now) {
                    if let Some(at) = activity.last_at {
                        ctx.request_repaint_after(
                            (at + MIDI_JOG_ACTIVE_WINDOW).saturating_duration_since(now),
                        );
                    }
                }
            }
        }
        if let Some(deadline) = self.midi_learn.suppression_until {
            ctx.request_repaint_after(deadline.saturating_duration_since(now));
        }
    }

    fn update_midi_led_feedback(&mut self, deck_window_focused: bool) {
        let deck = if deck_window_focused && self.midi_led_frame_seen {
            let leds = self.led_state.mosi();
            let mut states = [false; 11];
            for (pad, state) in states[..8].iter_mut().enumerate() {
                *state = self.midi_assignments.hot_cues[pad].is_some()
                    && hot_cue_led_has_assignment(leds.pad_rgb(pad));
            }
            states[8] = leds.led_bit(cdj3k_emu_subucom::mosi_frame::LED_LOOP_IN);
            states[9] = leds.led_bit(cdj3k_emu_subucom::mosi_frame::LED_LOOP_OUT);
            states[10] = leds.led_bit(cdj3k_emu_subucom::mosi_frame::LED_BEAT_SYNC);
            let slot = menu_state::lock().current_instance_id.saturating_sub(1) as u8;
            Some((slot % 2, states))
        } else {
            None
        };
        let mut mixer = Vec::new();
        if self.mixer.owns_shared_window() {
            if let Some(states) = self.mixer.cue_led_states() {
                for channel in 0..5 {
                    for bus in 0..2 {
                        let binding = if bus == 0 {
                            &self.mixer_midi_assignments.cue_a[channel]
                        } else {
                            &self.mixer_midi_assignments.cue_b[channel]
                        };
                        if let Some(address) = binding.as_ref().and_then(midi_led_address) {
                            push_midi_led_state(&mut mixer, address, states[channel][bus]);
                        }
                    }
                }
            }
            if let (Some(binding), Some(enabled)) = (
                self.mixer_midi_assignments.fx_on.as_ref(),
                self.mixer.fx_led_state(),
            ) {
                if let Some(address) = midi_led_address(binding) {
                    push_midi_led_state(&mut mixer, address, enabled);
                }
            }
        }
        self.midi_controls.set_led_feedback(deck, mixer);
    }

    fn refresh_runtime_midi_assignments(&mut self) {
        let instance_id = menu_state::lock().current_instance_id;
        let latest = cdj3k_emu_storage::load_midi_assignments(instance_id);
        if latest != self.midi_assignments {
            self.clear_midi_buttons();
            self.midi_assignments = latest;
        }
        let slot = instance_id.saturating_sub(1).min(3) as usize;
        self.midi_learn.slot_assignments[slot] = self.midi_assignments.clone();
        if self.mixer.owns_shared_window() {
            let latest = cdj3k_emu_storage::load_mixer_midi_assignments();
            if latest != self.mixer_midi_assignments {
                self.mixer_midi_assignments = latest;
            }
        }
    }

    fn apply_midi_message(
        &mut self,
        source: u64,
        port_name: &str,
        message: &MidiMessage,
        at: Instant,
        deck_window_focused: bool,
    ) -> bool {
        let channel = message.channel();
        let (kind, number) = message.kind_and_number();
        let mut changed = false;

        for target in MidiTarget::DECK
            .into_iter()
            .chain((0..EXTRA_BUTTONS.len()).map(MidiTarget::Extra))
        {
            let Some(button) = target.button() else {
                continue;
            };
            if !button_allowed_for_focus(button, deck_window_focused) {
                continue;
            }
            let Some(binding) = target.binding(&self.midi_assignments) else {
                continue;
            };
            if !binding_matches(binding, port_name, channel, kind, number) {
                continue;
            }

            let id = MidiButtonId {
                source,
                channel,
                kind,
                number,
                button,
            };
            changed |= update_midi_button(
                &mut self.midi_held_btns,
                id,
                message.button_pressed(),
                Instant::now(),
            );
        }

        if let MidiMessage::ControlChange { value, .. } = *message {
            if rotary_binding_matches(
                self.midi_assignments.rotary_turn.as_ref(),
                deck_window_focused,
                port_name,
                channel,
                kind,
                number,
            ) {
                let clicks = rotary_clicks(value, self.midi_assignments.rotary_turn_mode);
                if clicks != 0 {
                    self.rotary = self.rotary.wrapping_add(clicks as u16);
                    self.nav_angle +=
                        clicks as f32 * std::f32::consts::TAU / super::ui::NAV_DETENT_COUNT;
                    changed = true;
                }
            }
            if let Some(binding) = &self.midi_assignments.tempo {
                if binding_matches(binding, port_name, channel, kind, number) {
                    let tempo = tempo_from_midi(value);
                    if (self.tempo - tempo).abs() > f32::EPSILON {
                        self.tempo = tempo;
                        changed = true;
                    }
                }
            }

            for target in [MidiTarget::JogPlatter, MidiTarget::JogRing] {
                let Some(binding) = target.binding(&self.midi_assignments) else {
                    continue;
                };
                if !binding_matches(binding, port_name, channel, kind, number) {
                    continue;
                }
                let mode = target.jog_mode(&self.midi_assignments).unwrap();
                let activity = match target {
                    MidiTarget::JogPlatter => &mut self.midi_jog_activity[0],
                    MidiTarget::JogRing => &mut self.midi_jog_activity[1],
                    _ => unreachable!(),
                };
                let dt = jog_sample_dt(activity.last_at, at);
                let steps_per_rev = target
                    .jog_steps_per_rev(&self.midi_assignments)
                    .unwrap_or(DEFAULT_JOG_RELATIVE_STEPS_PER_REV);
                let delta = jog_delta_rad(
                    value,
                    dt,
                    mode,
                    target == MidiTarget::JogRing,
                    steps_per_rev,
                );
                activity.record(value, delta, at);
                if delta == 0.0 {
                    // Centered-speed CC 64 means the controller has stopped;
                    // an unrelated mouse drag may still be active, so only
                    // stop MIDI motion when this is the sole jog input.
                    if mode == MidiJogMode::SpeedCentered && !self.jog_is_dragging() {
                        self.jog_midi_stop();
                    }
                } else {
                    self.jog_midi_sample(delta, dt);
                }
            }

            if self.mixer.owns_shared_window() {
                for mixer_channel in 0..4 {
                    for target in MidiTarget::MIXER {
                        let parameter = target.mixer_parameter().unwrap();
                        let Some(binding) = self
                            .mixer_midi_assignments
                            .binding(mixer_channel, parameter)
                        else {
                            continue;
                        };
                        if binding_matches(binding, port_name, channel, kind, number) {
                            self.mixer.set_midi_channel(
                                mixer_channel,
                                parameter,
                                value as f32 / 127.0,
                            );
                        }
                    }
                }
            }
        }

        if self.mixer.owns_shared_window() {
            let (value, is_note, should_apply) = match *message {
                MidiMessage::ControlChange { value, .. } => (value, false, true),
                MidiMessage::Note { pressed, .. } => (127, true, pressed),
            };
            if should_apply {
                for mixer_channel in 0..=4 {
                    let extras: &[MixerMidiExtra] = if mixer_channel < 4 {
                        &[
                            MixerMidiExtra::CrossAssign,
                            MixerMidiExtra::CueA,
                            MixerMidiExtra::CueB,
                        ]
                    } else {
                        &[
                            MixerMidiExtra::Crossfader,
                            MixerMidiExtra::CueA,
                            MixerMidiExtra::CueB,
                            MixerMidiExtra::MasterLevel,
                            MixerMidiExtra::CueALevel,
                            MixerMidiExtra::CueBLevel,
                            MixerMidiExtra::FxType,
                            MixerMidiExtra::FxChannel,
                            MixerMidiExtra::FxOn,
                            MixerMidiExtra::EqCurve,
                            MixerMidiExtra::FxDepth,
                            MixerMidiExtra::FxLow,
                            MixerMidiExtra::FxMid,
                            MixerMidiExtra::FxHigh,
                        ]
                    };
                    for &extra in extras {
                        if self
                            .mixer_midi_assignments
                            .extra_binding(mixer_channel, extra)
                            .is_some_and(|binding| {
                                binding_matches(binding, port_name, channel, kind, number)
                            })
                        {
                            self.mixer
                                .apply_midi_extra(mixer_channel, extra, value, is_note);
                        }
                    }
                }
            }
        }

        if let Some(binding) = &self.midi_assignments.jog_touch {
            if binding_matches(binding, port_name, channel, kind, number) {
                let id = (source, channel, kind, number);
                if message.button_pressed() {
                    self.midi_jog_touch_held.insert(id);
                } else {
                    self.midi_jog_touch_held.remove(&id);
                }
            }
        }

        for (target, reverse) in [
            (MidiTarget::DirectionReverse, Direction::Reverse),
            (MidiTarget::DirectionSlipReverse, Direction::SlipReverse),
        ] {
            let Some(binding) = target.binding(&self.midi_assignments) else {
                continue;
            };
            if !binding_matches(binding, port_name, channel, kind, number) {
                continue;
            }
            match (reverse, message.button_pressed()) {
                (Direction::Reverse, true) => {
                    self.direction = if self.direction == Direction::Reverse {
                        Direction::Forward
                    } else {
                        Direction::Reverse
                    };
                    changed = true;
                }
                (Direction::SlipReverse, true) => {
                    self.direction = Direction::SlipReverse;
                    changed = true;
                }
                (Direction::SlipReverse, false) if self.direction == Direction::SlipReverse => {
                    self.direction = Direction::Forward;
                    changed = true;
                }
                _ => {}
            }
        }

        changed
    }

    pub(super) fn is_midi_button_pressed(&self, button: (usize, u8)) -> bool {
        self.midi_held_btns
            .values()
            .any(|held| held.button == button)
    }

    pub(super) fn midi_jog_debug_lines(&self) -> [String; 4] {
        let now = Instant::now();
        let describe = |label: &str, target: MidiTarget, activity: &MidiJogActivity| {
            let state = if activity.moving(now) {
                "ENGAGED"
            } else {
                "idle"
            };
            let value = activity
                .last_value
                .map_or_else(|| "--".to_owned(), |value| value.to_string());
            let age = activity.last_at.map_or_else(
                || "never".to_owned(),
                |at| format!("{} ms ago", now.saturating_duration_since(at).as_millis()),
            );
            format!(
                "{label}: {state} · CC={value} · Δ={:+.1}° · events={} · {age} · {} · {}",
                activity.last_delta_rad.to_degrees(),
                activity.events,
                jog_mode_label(target.jog_mode(&self.midi_assignments).unwrap()),
                binding_label(target.binding(&self.midi_assignments).as_ref()),
            )
        };
        let duplicate_binding = self.midi_assignments.jog_platter.is_some()
            && self.midi_assignments.jog_platter == self.midi_assignments.jog_ring;
        let flx4 = self.ddj_flx4_jog_source().is_some();
        let reversed_flx4 = flx4
            && self
                .midi_assignments
                .jog_platter
                .as_ref()
                .is_some_and(|binding| binding.number == 33)
            && self
                .midi_assignments
                .jog_ring
                .as_ref()
                .is_some_and(|binding| binding.number == 34);
        let wrong_flx4_mode = flx4
            && (self.midi_assignments.jog_platter_mode != MidiJogMode::RelativeOffset64
                || self.midi_assignments.jog_ring_mode != MidiJogMode::RelativeOffset64);
        [
            describe(
                "platter",
                MidiTarget::JogPlatter,
                &self.midi_jog_activity[0],
            ),
            describe(
                "outer ring",
                MidiTarget::JogRing,
                &self.midi_jog_activity[1],
            ),
            format!(
                "touch: {} · {} · duplicate movement binding: {}",
                if self.midi_jog_touch_held.is_empty() {
                    "released"
                } else {
                    "HELD"
                },
                binding_label(self.midi_assignments.jog_touch.as_ref()),
                if duplicate_binding { "YES" } else { "no" },
            ),
            if reversed_flx4 || wrong_flx4_mode {
                "DDJ-FLX4 jog setup needs correction; apply its mapping in MIDI Learn".to_owned()
            } else if flx4 {
                "DDJ-FLX4: platter CC34, outer ring CC33, touch Note54; use Relative (65 / 63)"
                    .to_owned()
            } else {
                "Move each jog surface separately to confirm its learned CC above".to_owned()
            },
        ]
    }

    fn ddj_flx4_jog_source(&self) -> Option<(String, u8)> {
        let slot = menu_state::lock().current_instance_id.saturating_sub(1) as usize;
        self.ddj_flx4_jog_source_for(slot.min(3))
    }

    fn ddj_flx4_jog_source_for(&self, slot: usize) -> Option<(String, u8)> {
        let assignments = &self.midi_learn.slot_assignments[slot];
        [
            assignments.jog_platter.as_ref(),
            assignments.jog_ring.as_ref(),
            assignments.jog_touch.as_ref(),
            assignments.play.as_ref(),
        ]
        .into_iter()
        .flatten()
        .find_map(|binding| {
            binding
                .port_name
                .as_deref()
                .filter(|name| is_ddj_flx4(name))
                .map(|name| (name.to_owned(), binding.channel))
        })
        .or_else(|| {
            let channel = slot as u8;
            self.midi_controls
                .connected_port_names()
                .into_iter()
                .find(|name| is_ddj_flx4(name))
                .map(|name| (name, channel))
        })
    }

    fn apply_ddj_flx4_jog_mapping(&mut self, slot: usize, port_name: String, channel: u8) {
        if self.is_current_slot(slot) {
            self.clear_midi_buttons();
        }
        let binding = |kind, number| {
            Some(MidiBinding {
                port_name: Some(port_name.clone()),
                channel,
                kind,
                number,
            })
        };
        let assignments = &mut self.midi_learn.slot_assignments[slot];
        assignments.jog_platter = binding(MidiMessageKind::ControlChange, 34);
        assignments.jog_ring = binding(MidiMessageKind::ControlChange, 33);
        assignments.jog_touch = binding(MidiMessageKind::Note, 54);
        assignments.jog_platter_mode = MidiJogMode::RelativeOffset64;
        assignments.jog_ring_mode = MidiJogMode::RelativeOffset64;
        assignments.jog_platter_steps_per_rev = DEFAULT_JOG_RELATIVE_STEPS_PER_REV;
        assignments.jog_ring_steps_per_rev = DEFAULT_JOG_RELATIVE_STEPS_PER_REV;
        self.midi_learn.learning = None;
        self.mixer.set_midi_learn_active(false);
        self.midi_learn.notice = Some(match self.persist_midi_assignments(slot) {
            Ok(()) => format!(
                "Slot {} DDJ-FLX4 jog mapping saved for {port_name} on channel {}",
                slot + 1,
                channel + 1
            ),
            Err(error) => error,
        });
    }

    fn replace_midi_binding(
        &mut self,
        selection: MidiLearnSelection,
        binding: Option<MidiBinding>,
    ) -> Result<(), String> {
        match selection {
            MidiLearnSelection::Deck { slot, target } => {
                if self.is_current_slot(slot) {
                    self.clear_midi_buttons();
                }
                let assignments = &mut self.midi_learn.slot_assignments[slot];
                if matches!(target, MidiTarget::JogPlatter | MidiTarget::JogRing)
                    && binding
                        .as_ref()
                        .and_then(|binding| binding.port_name.as_deref())
                        .is_some_and(is_ddj_flx4)
                {
                    target.set_jog_mode(assignments, MidiJogMode::RelativeOffset64);
                }
                target.set_binding(assignments, binding);
                if self.is_current_slot(slot) {
                    self.midi_jog_activity = [MidiJogActivity::default(); 2];
                }
                self.persist_midi_assignments(slot)
            }
            MidiLearnSelection::Mixer { channel, target } => {
                if let MidiTarget::MixerExtra(extra) = target {
                    self.mixer_midi_assignments
                        .set_extra_binding(channel, extra, binding);
                } else {
                    let parameter = target.mixer_parameter().unwrap();
                    self.mixer_midi_assignments
                        .set_binding(channel, parameter, binding);
                }
                self.persist_mixer_midi_assignments()
            }
        }
    }

    fn reset_midi_bindings(&mut self, slot: usize) {
        if self.is_current_slot(slot) {
            self.clear_midi_buttons();
        }
        self.midi_learn.slot_assignments[slot] = MidiAssignments::for_instance(slot as u32 + 1);
        self.midi_learn.learning = None;
        self.mixer.set_midi_learn_active(false);
        self.midi_learn.notice = Some(match self.persist_midi_assignments(slot) {
            Ok(()) => {
                format!(
                    "Restored Slot {} defaults on MIDI channel {}",
                    slot + 1,
                    slot + 1
                )
            }
            Err(err) => err,
        });
    }

    fn clear_midi_buttons(&mut self) {
        let had_midi_motion = self
            .midi_jog_activity
            .iter()
            .any(|activity| activity.last_at.is_some());
        self.midi_jog_touch_held.clear();
        self.midi_activate_deck_held.clear();
        self.midi_jog_activity = [MidiJogActivity::default(); 2];
        if had_midi_motion && !self.jog_is_dragging() {
            self.jog_midi_stop();
        }
        if !self.midi_held_btns.is_empty() {
            self.midi_held_btns.clear();
            self.inject(self.build_current_frame().finalize());
        }
    }

    fn is_current_slot(&self, slot: usize) -> bool {
        menu_state::lock().current_instance_id == slot as u32 + 1
    }

    fn persist_midi_assignments(&mut self, slot: usize) -> Result<(), String> {
        if self.is_current_slot(slot) {
            if self.midi_assignments != self.midi_learn.slot_assignments[slot] {
                self.clear_midi_buttons();
                self.midi_jog_activity = [MidiJogActivity::default(); 2];
            }
            self.midi_assignments = self.midi_learn.slot_assignments[slot].clone();
        }
        cdj3k_emu_storage::save_midi_assignments(
            slot as u32 + 1,
            &self.midi_learn.slot_assignments[slot],
        )
        .map_err(|err| format!("Could not save Slot {} MIDI assignments: {err}", slot + 1))
    }

    fn persist_mixer_midi_assignments(&self) -> Result<(), String> {
        cdj3k_emu_storage::save_mixer_midi_assignments(&self.mixer_midi_assignments)
            .map_err(|err| format!("Could not save mixer MIDI assignments: {err}"))
    }

    pub(super) fn show_midi_learn(&mut self, ctx: &egui::Context) {
        let (open, requested_tab, generation, _) = self.mixer.midi_learn_state();
        if !open {
            self.midi_learn.learning = None;
            self.midi_learn.open = false;
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        }
        if !self.mixer.owns_shared_window() {
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        }

        let need_focus =
            !self.midi_learn.open || self.midi_learn.last_request_generation != generation;
        if need_focus {
            self.midi_learn.open = true;
            self.midi_learn.last_request_generation = generation;
            self.midi_learn.tab = MidiTab::from_shared(requested_tab);
            self.midi_learn.learning = None;
            self.midi_learn.suppression_until = None;
            self.mixer.set_midi_learn_active(false);
            self.midi_learn.slot_assignments = std::array::from_fn(|slot| {
                cdj3k_emu_storage::load_midi_assignments(slot as u32 + 1)
            });
            self.mixer_midi_assignments = cdj3k_emu_storage::load_mixer_midi_assignments();
            self.midi_learn.selected_profile.clear();
            self.midi_learn.profile_name.clear();
            self.refresh_midi_profiles();
            self.midi_controls.request_refresh();
        }

        let close_flag = Arc::new(AtomicBool::new(false));
        let close_inner = Arc::clone(&close_flag);
        let this = &mut *self;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("midi_learn_shared"),
            egui::ViewportBuilder::default()
                .with_title("MIDI Learn")
                .with_inner_size([1040.0, 780.0])
                .with_resizable(true)
                .with_active(true),
            move |ctx, _class| {
                if ctx.input(|input| input.viewport().close_requested()) {
                    close_inner.store(true, Ordering::Relaxed);
                }
                if need_focus {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                egui::CentralPanel::default()
                    .frame(
                        Frame::default()
                            .fill(Color32::from_rgb(24, 25, 28))
                            .inner_margin(Margin::same(18.0)),
                    )
                    .show(ctx, |ui| this.draw_midi_learn(ui, &close_inner));
            },
        );
        if close_flag.load(Ordering::Relaxed) {
            // A fresh request from another player wins over a simultaneous
            // close of the old viewport.
            if self.mixer.midi_learn_state().2 == self.midi_learn.last_request_generation {
                self.midi_learn.open = false;
                self.midi_learn.learning = None;
                self.midi_learn.suppression_until = None;
                self.mixer.set_midi_learn_active(false);
                self.mixer.set_midi_learn_open(false);
            }
        }
        // A request may originate in another, otherwise idle player process.
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn select_midi_tab(&mut self, tab: MidiTab) {
        self.midi_learn.tab = tab;
        self.midi_learn.learning = None;
        self.midi_learn.suppression_until = None;
        self.midi_learn.notice = None;
        self.mixer.set_midi_learn_active(false);
        self.mixer.set_midi_learn_tab(tab.shared_index());
    }

    fn draw_midi_learn(&mut self, ui: &mut egui::Ui, close: &AtomicBool) {
        ui.label(
            RichText::new("MIDI Assignments")
                .size(20.0)
                .strong()
                .color(Color32::from_rgb(230, 230, 232)),
        );
        let connected_names = self.midi_controls.connected_port_names();
        let connected_description = if connected_names.is_empty() {
            "none".to_owned()
        } else {
            connected_names.join(", ")
        };
        ui.label(
            RichText::new(format!(
                "{} CoreMIDI input(s) connected here: {connected_description}. Slot and mixer assignments are shared across all instances.",
                self.midi_controls.connected_port_count(),
            ))
            .size(12.0)
            .color(Color32::from_rgb(155, 155, 160)),
        );
        ui.horizontal(|ui| {
            if ui.button("Rescan MIDI Devices").clicked() {
                self.mixer.request_midi_device_action(false);
                self.midi_learn.notice = Some("Rescanning CoreMIDI inputs on all slots…".to_owned());
            }
            if ui.button("Reset MIDI").clicked() {
                self.mixer.request_midi_device_action(true);
                self.midi_learn.notice = Some("Resetting CoreMIDI inputs on all slots…".to_owned());
            }
            ui.label(
                RichText::new(
                    "Reset closes and reopens MIDI inputs after a hot-plug; assignments and Learn stay active.",
                )
                .size(11.0)
                .color(Color32::from_rgb(135, 135, 140)),
            );
        });
        ui.add_space(10.0);
        self.draw_midi_profiles(ui);
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            for slot in 0..4 {
                let tab = MidiTab::Slot(slot);
                if ui
                    .selectable_label(self.midi_learn.tab == tab, format!("Slot {}", slot + 1))
                    .clicked()
                    && self.midi_learn.tab != tab
                {
                    self.select_midi_tab(tab);
                }
            }
            if ui
                .selectable_label(self.midi_learn.tab == MidiTab::Mixer, "Mixer")
                .clicked()
                && self.midi_learn.tab != MidiTab::Mixer
            {
                self.select_midi_tab(MidiTab::Mixer);
            }
        });
        ui.separator();
        if let MidiTab::Slot(slot) = self.midi_learn.tab {
            if let Some((port_name, channel)) = self.ddj_flx4_jog_source_for(slot) {
                ui.horizontal(|ui| {
                    if ui.button("Apply DDJ-FLX4 Jog Mapping").clicked() {
                        self.apply_ddj_flx4_jog_mapping(slot, port_name, channel);
                    }
                    ui.label(
                        RichText::new(
                            "Vinyl On: platter CC34, outer ring CC33, touch Note54; relative 65/63",
                        )
                        .size(11.0)
                        .color(Color32::from_rgb(135, 135, 140)),
                    );
                });
            }
        }
        ui.add_space(8.0);

        if let Some(target) = self.midi_learn.learning {
            Frame::default()
                .fill(Color32::from_rgb(58, 49, 24))
                .inner_margin(Margin::same(10.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new(format!(
                                "Learning {}: move or press the intended MIDI control…",
                                target.label()
                            ))
                            .color(Color32::from_rgb(245, 210, 110)),
                        );
                        if ui.button("Cancel").clicked() {
                            self.midi_learn.learning = None;
                            self.mixer.set_midi_learn_active(false);
                        }
                    });
                });
            ui.add_space(8.0);
        } else if let Some(notice) = &self.midi_learn.notice {
            ui.label(
                RichText::new(notice)
                    .size(12.0)
                    .color(Color32::from_rgb(130, 205, 155)),
            );
            ui.add_space(8.0);
        }

        match self.midi_learn.tab {
            MidiTab::Slot(slot) => self.draw_deck_midi_assignments(ui, slot),
            MidiTab::Mixer => self.draw_mixer_midi_assignments(ui),
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(
            RichText::new("Pitch, jog motion, rotary turns, and mixer controls learn CCs. Activate Deck Window brings its slot to the front on a MIDI press. Rotary Turn and Rotary Press respond only when that deck or its main-screen pop-out is focused; assign the same encoder on each Slot tab you want it to control. Move the platter top and wheel edge separately when learning jog controls. Jog Touch learns a Note or CC; Reloop/Exit is one physical button.")
                .size(11.0)
                .color(Color32::from_rgb(155, 155, 160)),
        );
        ui.horizontal(|ui| {
            match self.midi_learn.tab {
                MidiTab::Slot(slot) => {
                    if ui.button("Reset Slot Defaults").clicked() {
                        self.reset_midi_bindings(slot);
                    }
                    ui.label(
                        RichText::new(format!(
                            "Defaults: any input, MIDI channel {}, CDJ-3000 messages",
                            slot + 1
                        ))
                        .size(11.0)
                        .color(Color32::from_rgb(135, 135, 140)),
                    );
                }
                MidiTab::Mixer => {
                    if ui.button("Clear Mixer Assignments").clicked() {
                        self.mixer_midi_assignments = MidiMixerAssignments::default();
                        self.midi_learn.learning = None;
                        self.mixer.set_midi_learn_active(false);
                        self.midi_learn.notice =
                            Some(match self.persist_mixer_midi_assignments() {
                                Ok(()) => "Cleared all mixer MIDI assignments".to_owned(),
                                Err(err) => err,
                            });
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    close.store(true, Ordering::Relaxed);
                }
            });
        });
    }

    fn draw_deck_midi_assignments(&mut self, ui: &mut egui::Ui, slot: usize) {
        let mut clear_target = None;
        let mut mode_change = None;
        let mut steps_change = None;
        let mut learn_target = None;
        ScrollArea::vertical().max_height(485.0).show(ui, |ui| {
            Grid::new(("midi_assignment_grid", slot))
                .num_columns(6)
                .striped(true)
                .spacing([12.0, 7.0])
                .show(ui, |ui| {
                    ui.strong("Control");
                    ui.strong("Assignment");
                    ui.strong("CC format");
                    ui.strong("Steps/rev");
                    ui.strong("");
                    ui.strong("");
                    ui.end_row();

                    for target in MidiTarget::DECK
                        .into_iter()
                        .chain((0..EXTRA_BUTTONS.len()).map(MidiTarget::Extra))
                    {
                        let assignments = &self.midi_learn.slot_assignments[slot];
                        let binding = target.binding(assignments).clone();
                        ui.label(target.label());
                        ui.label(binding_label(binding.as_ref()));

                        if let Some(current_mode) = target.jog_mode(assignments) {
                            let mut mode = current_mode;
                            ComboBox::from_id_salt(("midi_jog_mode", slot, target))
                                .selected_text(jog_mode_label(mode))
                                .show_ui(ui, |ui| {
                                    if target != MidiTarget::RotaryTurn {
                                        ui.selectable_value(
                                            &mut mode,
                                            MidiJogMode::SpeedCentered,
                                            "Speed (64 = stop)",
                                        );
                                    }
                                    ui.selectable_value(
                                        &mut mode,
                                        MidiJogMode::RelativeTwosComplement,
                                        "Relative (1 / 127)",
                                    );
                                    ui.selectable_value(
                                        &mut mode,
                                        MidiJogMode::RelativeOffset64,
                                        "Relative (65 / 63)",
                                    );
                                });
                            if mode != current_mode {
                                mode_change = Some((target, mode));
                            }
                        } else {
                            ui.label("");
                        }

                        if let Some(current_steps) = target.jog_steps_per_rev(assignments) {
                            let mut steps = current_steps;
                            ui.add_enabled(
                                target.jog_mode(assignments) != Some(MidiJogMode::SpeedCentered),
                                egui::DragValue::new(&mut steps).range(1..=8192).speed(1.0),
                            ).on_hover_text("Relative jog encoder counts per full platter revolution; lower values move farther per count.");
                            if steps != current_steps {
                                steps_change = Some((target, steps));
                            }
                        } else {
                            ui.label("");
                        }

                        let selection = MidiLearnSelection::Deck { slot, target };
                        let listening = self.midi_learn.learning == Some(selection);
                        if ui
                            .add_enabled(
                                !listening,
                                egui::Button::new(if listening { "Listening…" } else { "Learn" }),
                            )
                            .clicked()
                        {
                            learn_target = Some(selection);
                        }
                        if ui
                            .add_enabled(binding.is_some(), egui::Button::new("Clear"))
                            .clicked()
                        {
                            clear_target = Some(selection);
                        }
                        ui.end_row();
                    }
                });
        });

        if let Some(selection) = learn_target {
            self.start_midi_learning(selection);
        }
        if let Some((target, mode)) = mode_change {
            if self.is_current_slot(slot) {
                self.clear_midi_buttons();
            }
            target.set_jog_mode(&mut self.midi_learn.slot_assignments[slot], mode);
            self.midi_learn.notice = Some(match self.persist_midi_assignments(slot) {
                Ok(()) => format!("{} set to {}", target.label(), jog_mode_label(mode)),
                Err(err) => err,
            });
        }

        if let Some((target, steps)) = steps_change {
            target.set_jog_steps_per_rev(&mut self.midi_learn.slot_assignments[slot], steps);
            self.midi_learn.notice = Some(match self.persist_midi_assignments(slot) {
                Ok(()) => format!("{} set to {steps} steps/rev", target.label()),
                Err(err) => err,
            });
        }

        if let Some(selection) = clear_target {
            let save_result = self.replace_midi_binding(selection, None);
            if self.midi_learn.learning == Some(selection) {
                self.midi_learn.learning = None;
                self.mixer.set_midi_learn_active(false);
            }
            self.midi_learn.notice = Some(match save_result {
                Ok(()) => format!("Cleared {}", selection.label()),
                Err(err) => err,
            });
        }
    }

    fn draw_mixer_midi_assignments(&mut self, ui: &mut egui::Ui) {
        let mut learn_target = None;
        let mut clear_target = None;
        ScrollArea::vertical().max_height(485.0).show(ui, |ui| {
            for channel in 0..4 {
                ui.strong(format!("Channel {}", channel + 1));
                Grid::new(("midi_mixer_grid", channel))
                    .num_columns(4)
                    .striped(true)
                    .spacing([12.0, 7.0])
                    .show(ui, |ui| {
                        for target in MidiTarget::MIXER.into_iter().chain([
                            MidiTarget::MixerExtra(MixerMidiExtra::CueA),
                            MidiTarget::MixerExtra(MixerMidiExtra::CueB),
                            MidiTarget::MixerExtra(MixerMidiExtra::CrossAssign),
                        ]) {
                            let selection = MidiLearnSelection::Mixer { channel, target };
                            let binding = match target {
                                MidiTarget::MixerExtra(extra) => {
                                    self.mixer_midi_assignments.extra_binding(channel, extra)
                                }
                                _ => self
                                    .mixer_midi_assignments
                                    .binding(channel, target.mixer_parameter().unwrap()),
                            };
                            ui.label(target.label().trim_start_matches("Mixer "));
                            ui.label(binding_label(binding));
                            let listening = self.midi_learn.learning == Some(selection);
                            if ui
                                .add_enabled(
                                    !listening,
                                    egui::Button::new(if listening {
                                        "Listening…"
                                    } else {
                                        "Learn"
                                    }),
                                )
                                .clicked()
                            {
                                learn_target = Some(selection);
                            }
                            if ui
                                .add_enabled(binding.is_some(), egui::Button::new("Clear"))
                                .clicked()
                            {
                                clear_target = Some(selection);
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(12.0);
                ui.separator();
            }
            ui.strong("Crossfader / Master / FX");
            Grid::new("midi_mixer_master_grid")
                .num_columns(4)
                .striped(true)
                .spacing([12.0, 7.0])
                .show(ui, |ui| {
                    for extra in [
                        MixerMidiExtra::Crossfader,
                        MixerMidiExtra::CueA,
                        MixerMidiExtra::CueB,
                        MixerMidiExtra::MasterLevel,
                        MixerMidiExtra::CueALevel,
                        MixerMidiExtra::CueBLevel,
                        MixerMidiExtra::FxType,
                        MixerMidiExtra::FxChannel,
                        MixerMidiExtra::FxOn,
                        MixerMidiExtra::EqCurve,
                        MixerMidiExtra::FxDepth,
                        MixerMidiExtra::FxLow,
                        MixerMidiExtra::FxMid,
                        MixerMidiExtra::FxHigh,
                    ] {
                        let target = MidiTarget::MixerExtra(extra);
                        let selection = MidiLearnSelection::Mixer { channel: 4, target };
                        let binding = self.mixer_midi_assignments.extra_binding(4, extra);
                        ui.label(target.label());
                        ui.label(binding_label(binding));
                        let listening = self.midi_learn.learning == Some(selection);
                        if ui
                            .add_enabled(
                                !listening,
                                egui::Button::new(if listening { "Listening…" } else { "Learn" }),
                            )
                            .clicked()
                        {
                            learn_target = Some(selection);
                        }
                        if ui
                            .add_enabled(binding.is_some(), egui::Button::new("Clear"))
                            .clicked()
                        {
                            clear_target = Some(selection);
                        }
                        ui.end_row();
                    }
                });
        });
        if let Some(selection) = learn_target {
            self.start_midi_learning(selection);
        }
        if let Some(selection) = clear_target {
            let result = self.replace_midi_binding(selection, None);
            if self.midi_learn.learning == Some(selection) {
                self.midi_learn.learning = None;
                self.mixer.set_midi_learn_active(false);
            }
            self.midi_learn.notice = Some(match result {
                Ok(()) => format!("Cleared {}", selection.label()),
                Err(err) => err,
            });
        }
    }

    fn start_midi_learning(&mut self, selection: MidiLearnSelection) {
        self.midi_learn.learning = Some(selection);
        self.midi_learn.notice = None;
        self.midi_learn.suppression_until = None;
        self.mixer.set_midi_learn_active(true);
        self.midi_controls.request_refresh();
    }

    fn refresh_midi_profiles(&mut self) {
        let result = cdj3k_emu_storage::list_all_midi_profiles();
        match result {
            Ok(profiles) => {
                self.midi_learn.profiles = profiles;
                if !self
                    .midi_learn
                    .profiles
                    .contains(&self.midi_learn.selected_profile)
                {
                    self.midi_learn.selected_profile.clear();
                }
            }
            Err(err) => {
                self.midi_learn.notice = Some(format!("Could not list MIDI profiles: {err}"));
            }
        }
    }

    fn draw_midi_profiles(&mut self, ui: &mut egui::Ui) {
        Frame::default()
            .fill(Color32::from_rgb(31, 33, 37))
            .inner_margin(Margin::same(10.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("MIDI Profile");
                    ui.label("Slots 1–4 + Mixer");
                    ComboBox::from_id_salt("midi_profile_all_tabs")
                        .selected_text(if self.midi_learn.selected_profile.is_empty() {
                            "Choose saved profile…"
                        } else {
                            &self.midi_learn.selected_profile
                        })
                        .show_ui(ui, |ui| {
                            for profile in &self.midi_learn.profiles {
                                if ui
                                    .selectable_value(
                                        &mut self.midi_learn.selected_profile,
                                        profile.clone(),
                                        profile,
                                    )
                                    .clicked()
                                {
                                    self.midi_learn.profile_name = profile.clone();
                                }
                            }
                        });
                    ui.add(
                        egui::TextEdit::singleline(&mut self.midi_learn.profile_name)
                            .hint_text("Profile name")
                            .desired_width(180.0),
                    );

                    if ui.button("Save").clicked() {
                        let name = self.midi_learn.profile_name.trim().to_owned();
                        let profile = cdj3k_emu_storage::MidiProfile {
                            slots: self.midi_learn.slot_assignments.clone(),
                            mixer: self.mixer_midi_assignments.clone(),
                        };
                        let result = cdj3k_emu_storage::save_all_midi_profile(&name, &profile);
                        self.midi_learn.notice = Some(match result {
                            Ok(()) => {
                                self.midi_learn.selected_profile = name.clone();
                                format!("Saved MIDI profile “{name}” for all slots and mixer")
                            }
                            Err(err) => format!("Could not save MIDI profile: {err}"),
                        });
                        self.refresh_midi_profiles();
                    }

                    let selected = self.midi_learn.selected_profile.clone();
                    if ui
                        .add_enabled(!selected.is_empty(), egui::Button::new("Load"))
                        .clicked()
                    {
                        let result =
                            cdj3k_emu_storage::load_all_midi_profile(&selected).map(|profile| {
                                // Import legacy per-tab files as a unified snapshot.
                                cdj3k_emu_storage::save_all_midi_profile(&selected, &profile)
                                    .map_err(|err| {
                                        format!("Could not import MIDI profile: {err}")
                                    })?;
                                self.midi_learn.slot_assignments = profile.slots;
                                self.mixer_midi_assignments = profile.mixer;
                                for slot in 0..4 {
                                    self.persist_midi_assignments(slot)?;
                                }
                                self.persist_mixer_midi_assignments()
                            });
                        self.midi_learn.learning = None;
                        self.mixer.set_midi_learn_active(false);
                        self.midi_learn.notice = Some(match result {
                            Ok(Ok(())) => {
                                format!("Loaded MIDI profile “{selected}” for all slots and mixer")
                            }
                            Ok(Err(err)) => err,
                            Err(err) => format!("Could not load MIDI profile: {err}"),
                        });
                        self.refresh_midi_profiles();
                    }
                    if ui
                        .add_enabled(!selected.is_empty(), egui::Button::new("Delete"))
                        .clicked()
                    {
                        let result = cdj3k_emu_storage::delete_all_midi_profile(&selected);
                        let deleted = result.is_ok();
                        self.midi_learn.notice = Some(match result {
                            Ok(()) => format!("Deleted MIDI profile “{selected}”"),
                            Err(err) => format!("Could not delete MIDI profile: {err}"),
                        });
                        if deleted {
                            self.midi_learn.selected_profile.clear();
                            self.midi_learn.profile_name.clear();
                        }
                        self.refresh_midi_profiles();
                    }
                });
            });
    }
}

fn learned_binding(
    target: MidiTarget,
    port_name: &str,
    message: &MidiMessage,
) -> Option<MidiBinding> {
    let (kind, number) = message.kind_and_number();
    let valid = match (target.requires_cc(), message) {
        (true, MidiMessage::ControlChange { .. }) => true,
        (true, _) => false,
        (false, MidiMessage::Note { pressed, .. }) => *pressed,
        (false, MidiMessage::ControlChange { value, .. }) => *value != 0,
    };
    valid.then(|| MidiBinding {
        port_name: Some(port_name.to_owned()),
        channel: message.channel(),
        kind,
        number,
    })
}

fn binding_matches(
    binding: &MidiBinding,
    port_name: &str,
    channel: u8,
    kind: MidiMessageKind,
    number: u8,
) -> bool {
    binding.channel == channel
        && binding.kind == kind
        && binding.number == number
        && binding
            .port_name
            .as_deref()
            .map_or(true, |assigned| assigned == port_name)
}

fn is_ddj_flx4(name: &str) -> bool {
    name.to_ascii_uppercase().contains("DDJ-FLX4")
}

fn midi_led_address(binding: &MidiBinding) -> Option<[u8; 2]> {
    if !binding.port_name.as_deref().is_some_and(is_ddj_flx4) {
        return None;
    }
    let status = match binding.kind {
        MidiMessageKind::Note => 0x90,
        MidiMessageKind::ControlChange => 0xb0,
    } | binding.channel;
    Some([status, binding.number])
}

fn push_midi_led_state(states: &mut Vec<([u8; 2], bool)>, address: [u8; 2], lit: bool) {
    if let Some((_, existing)) = states.iter_mut().find(|(key, _)| *key == address) {
        *existing |= lit;
    } else {
        states.push((address, lit));
    }
}

fn binding_label(binding: Option<&MidiBinding>) -> String {
    let Some(binding) = binding else {
        return "Not assigned".to_owned();
    };
    let port = binding.port_name.as_deref().unwrap_or("Any input");
    let message = match binding.kind {
        MidiMessageKind::Note => "Note",
        MidiMessageKind::ControlChange => "CC",
    };
    format!(
        "{port} · Ch {} · {message} {}",
        binding.channel + 1,
        binding.number
    )
}

fn jog_mode_label(mode: MidiJogMode) -> &'static str {
    match mode {
        MidiJogMode::SpeedCentered => "Speed (64 = stop)",
        MidiJogMode::RelativeTwosComplement => "Relative (1 / 127)",
        MidiJogMode::RelativeOffset64 => "Relative (65 / 63)",
    }
}

/// Convert one relative encoder message into signed menu detents. Unlike a
/// jog platter, each count is one discrete firmware rotary click.
fn rotary_clicks(value: u8, mode: MidiJogMode) -> i16 {
    match mode {
        MidiJogMode::RelativeOffset64 => value as i16 - 64,
        MidiJogMode::RelativeTwosComplement => {
            if value <= 63 {
                value as i16
            } else {
                value as i16 - 128
            }
        }
        // Older profiles cannot select this mode, but treat it as one click
        // per directional message if one was persisted by a future build.
        MidiJogMode::SpeedCentered => (value as i16 - 64).signum(),
    }
}

fn rotary_binding_matches(
    binding: Option<&MidiBinding>,
    deck_window_focused: bool,
    port_name: &str,
    channel: u8,
    kind: MidiMessageKind,
    number: u8,
) -> bool {
    deck_window_focused
        && binding.is_some_and(|binding| binding_matches(binding, port_name, channel, kind, number))
}

fn jog_sample_dt(previous: Option<Instant>, at: Instant) -> f32 {
    previous
        .and_then(|previous| at.checked_duration_since(previous))
        .map(|duration| duration.as_secs_f32())
        .filter(|&dt| dt <= JOG_MAX_SAMPLE_SEC)
        .map(|dt| dt.max(0.001))
        .unwrap_or(JOG_INITIAL_SAMPLE_SEC)
}

/// Convert a jog CC to the shared platter angle. Speed-centered matches the
/// CDJ-3000's published 64=stop / 0..63=reverse / 65..127=forward format;
/// the relative modes accept two's-complement and 64-offset jog encoders.
fn jog_delta_rad(value: u8, dt: f32, mode: MidiJogMode, ring: bool, steps_per_rev: u16) -> f32 {
    let revolutions = match mode {
        MidiJogMode::SpeedCentered => {
            let offset = value as i16 - 64;
            if offset == 0 {
                0.0
            } else {
                let magnitude = offset.unsigned_abs() as f32;
                let minimum = if ring { 0.5 } else { 0.06 };
                let denominator = if offset > 0 { 62.0 } else { 63.0 };
                let speed = minimum + (magnitude - 1.0) / denominator * (4.0 - minimum);
                offset.signum() as f32 * speed * dt
            }
        }
        MidiJogMode::RelativeTwosComplement => {
            let steps = if value <= 63 {
                value as i16
            } else {
                value as i16 - 128
            };
            steps as f32 / steps_per_rev.max(1) as f32
        }
        MidiJogMode::RelativeOffset64 => (value as i16 - 64) as f32 / steps_per_rev.max(1) as f32,
    };
    revolutions * std::f32::consts::TAU
}

fn midi_input_loop(
    tx: Sender<MidiEvent>,
    repaint_gate: RepaintGate,
    connected_port_count: Arc<AtomicUsize>,
    connected_port_names: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    rescan_rx: Receiver<MidiScanRequest>,
) {
    let mut connections: Vec<ActiveMidiConnection> = Vec::new();
    let mut scan_failed = false;
    let mut pending_request = Some(MidiScanRequest::Refresh);
    let mut last_connection_failures = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        let request = pending_request.take();
        if request == Some(MidiScanRequest::Reset) {
            // A hot-plug can leave a CoreMIDI connection with the same stable
            // source ID but no live callback. Release every client before the
            // new discovery so the fresh connection cannot reuse that handle.
            for connection in &connections {
                connection.set_enabled(false);
            }
            connections.clear();
            publish_midi_connections(&connections, &connected_port_count, &connected_port_names);
            let _ = tx.send(MidiEvent::ResetButtons);
            repaint_gate.request();
        }
        match midi_port_infos() {
            Ok(port_infos) => {
                scan_failed = false;
                let discovered = port_infos.len();
                let reconnect_all = request == Some(MidiScanRequest::ReconnectAll);
                let (changed, failures) = reconcile_midi_ports(
                    &port_infos,
                    &mut connections,
                    reconnect_all,
                    &tx,
                    &repaint_gate,
                );
                publish_midi_connections(
                    &connections,
                    &connected_port_count,
                    &connected_port_names,
                );
                if changed {
                    let _ = tx.send(MidiEvent::ResetButtons);
                    repaint_gate.request();
                }
                if failures != last_connection_failures {
                    for failure in &failures {
                        eprintln!("[midi] {failure}");
                    }
                    last_connection_failures.clone_from(&failures);
                }
                if let Some(request) = request {
                    let _ = tx.send(MidiEvent::ScanComplete {
                        request,
                        discovered,
                        connected: connections.len(),
                        failures,
                    });
                    repaint_gate.request();
                }
            }
            Err(err) => {
                if !scan_failed {
                    eprintln!("[midi] input discovery failed: {err}");
                    scan_failed = true;
                }
                if request.is_some() {
                    let _ = tx.send(MidiEvent::ScanFailed(err.to_string()));
                    repaint_gate.request();
                }
            }
        }

        match rescan_rx.recv_timeout(PORT_SCAN_INTERVAL) {
            Ok(first) => {
                // Collapse queued refreshes. An explicit reconnect has
                // priority over refreshes; a full reset takes precedence.
                let mut request = first;
                while let Ok(next) = rescan_rx.try_recv() {
                    if next.priority() > request.priority() {
                        request = next;
                    }
                }
                pending_request = Some(request);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MidiPortInfo {
    id: String,
    name: String,
    index: usize,
}

struct ActiveMidiConnection {
    port: MidiPortInfo,
    enabled: Arc<AtomicBool>,
    _connection: MidiInputConnection<()>,
}

impl ActiveMidiConnection {
    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }
}

fn midi_port_infos() -> Result<Vec<MidiPortInfo>, midir::InitError> {
    let midi_in = MidiInput::new("cdj3k-emu MIDI discovery")?;
    let mut infos = midi_in
        .ports()
        .iter()
        .enumerate()
        .map(|(index, port)| {
            let name = midi_in
                .port_name(port)
                .unwrap_or_else(|_| format!("MIDI input {}", index + 1));
            let raw_id = port.id();
            let id = if raw_id == "0" {
                // CoreMIDI normally supplies a unique ID. Preserve
                // deterministic behavior for a broken third-party driver.
                format!("fallback:{name}:{index}")
            } else {
                raw_id
            };
            MidiPortInfo { id, name, index }
        })
        .collect::<Vec<_>>();
    infos.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(infos)
}

fn reconcile_midi_ports(
    discovered: &[MidiPortInfo],
    connections: &mut Vec<ActiveMidiConnection>,
    reconnect_all: bool,
    tx: &Sender<MidiEvent>,
    repaint_gate: &RepaintGate,
) -> (bool, Vec<String>) {
    let before_ids = connections
        .iter()
        .map(|connection| connection.port.id.clone())
        .collect::<Vec<_>>();
    let mut failures = Vec::new();

    if reconnect_all {
        // Make before break: establish replacement CoreMIDI inputs while the
        // old ones remain live. Drivers that reject parallel subscriptions
        // fall back to a narrowly scoped break/retry for that endpoint.
        let mut old = std::mem::take(connections);
        let mut replacements = Vec::with_capacity(discovered.len());
        for port in discovered {
            match connect_midi_port(port, tx, repaint_gate, false) {
                Ok(connection) => replacements.push(connection),
                Err(first_error) => {
                    if let Some(index) = old
                        .iter()
                        .position(|connection| connection.port.id == port.id)
                    {
                        // Most CoreMIDI devices allow make-before-break. A
                        // few vendor drivers reject a second subscription;
                        // for those, drop only this old handle and retry once.
                        let old_connection = old.swap_remove(index);
                        old_connection.set_enabled(false);
                        drop(old_connection);
                        thread::sleep(Duration::from_millis(25));
                        match connect_midi_port(port, tx, repaint_gate, false) {
                            Ok(connection) => replacements.push(connection),
                            Err(second_error) => failures.push(format!(
                                "{first_error}; reconnect retry failed: {second_error}"
                            )),
                        }
                    } else {
                        failures.push(first_error);
                    }
                }
            }
        }
        for connection in &old {
            connection.set_enabled(false);
        }
        for connection in &replacements {
            connection.set_enabled(true);
        }
        *connections = replacements;
    } else {
        // Drop only endpoints that disappeared. Healthy devices remain live,
        // so clicking Learn or attaching another controller cannot create an
        // input blackout.
        for connection in connections.iter() {
            if !discovered.iter().any(|port| port.id == connection.port.id) {
                connection.set_enabled(false);
            }
        }
        connections
            .retain(|connection| discovered.iter().any(|port| port.id == connection.port.id));
        for port in discovered {
            if connections
                .iter()
                .any(|connection| connection.port.id == port.id)
            {
                continue;
            }
            match connect_midi_port(port, tx, repaint_gate, true) {
                Ok(connection) => connections.push(connection),
                Err(error) => failures.push(error),
            }
        }
    }

    connections.sort_by(|left, right| left.port.id.cmp(&right.port.id));
    let after_ids = connections
        .iter()
        .map(|connection| connection.port.id.clone())
        .collect::<Vec<_>>();
    (reconnect_all || before_ids != after_ids, failures)
}

fn connect_midi_port(
    expected: &MidiPortInfo,
    tx: &Sender<MidiEvent>,
    repaint_gate: &RepaintGate,
    initially_enabled: bool,
) -> Result<ActiveMidiConnection, String> {
    let mut midi_in = MidiInput::new(&format!("cdj3k-emu MIDI input {}", expected.name))
        .map_err(|error| format!("could not initialize {}: {error}", expected.name))?;
    midi_in.ignore(Ignore::None);

    let port = if expected.id.starts_with("fallback:") {
        // A driver that reports CoreMIDI unique ID zero cannot be found by
        // our synthetic stable key. Use its discovery slot, checking the
        // name again in case the port list changed between scans.
        midi_in.ports().get(expected.index).cloned().filter(|port| {
            port.id() == "0"
                && midi_in
                    .port_name(port)
                    .unwrap_or_else(|_| format!("MIDI input {}", expected.index + 1))
                    == expected.name
        })
    } else {
        midi_in.find_port_by_id(expected.id.clone())
    }
    .ok_or_else(|| format!("{} disappeared before it could be connected", expected.name))?;
    let port_name = midi_in
        .port_name(&port)
        .unwrap_or_else(|_| expected.name.clone());
    let callback_port_name: Arc<str> = Arc::from(port_name.clone());
    let callback_tx = tx.clone();
    let callback_gate = repaint_gate.clone();
    let source = stable_source_key(&expected.id);
    let enabled = Arc::new(AtomicBool::new(initially_enabled));
    let callback_enabled = Arc::clone(&enabled);

    match midi_in.connect(
        &port,
        &format!("cdj3k-emu {port_name}"),
        move |_timestamp, bytes, ()| {
            if !callback_enabled.load(Ordering::Acquire) {
                return;
            }
            if let Some(message) = parse_midi_message(bytes) {
                let event = MidiEvent::Message {
                    source,
                    port_name: Arc::clone(&callback_port_name),
                    message,
                    at: Instant::now(),
                };
                if callback_tx.send(event).is_ok() {
                    callback_gate.request();
                }
            }
        },
        (),
    ) {
        Ok(connection) => {
            eprintln!("[midi] connected to {port_name}");
            Ok(ActiveMidiConnection {
                port: MidiPortInfo {
                    id: expected.id.clone(),
                    name: port_name,
                    index: expected.index,
                },
                enabled,
                _connection: connection,
            })
        }
        Err(error) => Err(format!("could not connect to {port_name}: {error}")),
    }
}

fn publish_midi_connections(
    connections: &[ActiveMidiConnection],
    connected_port_count: &AtomicUsize,
    connected_port_names: &Mutex<Vec<String>>,
) {
    connected_port_count.store(connections.len(), Ordering::Release);
    if let Ok(mut names) = connected_port_names.lock() {
        *names = connections
            .iter()
            .map(|connection| connection.port.name.clone())
            .collect();
    }
}

fn stable_source_key(port_id: &str) -> u64 {
    // Stable FNV-1a avoids reusing a numeric list index when devices are
    // removed/reordered, which could otherwise let one controller release a
    // button being held by another controller.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in port_id.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn parse_midi_message(message: &[u8]) -> Option<MidiMessage> {
    let (&status, data) = message.split_first()?;
    let channel = status & 0x0f;

    match status & 0xf0 {
        0x80 | 0x90 if data.len() >= 2 => Some(MidiMessage::Note {
            channel,
            note: data[0],
            pressed: status & 0xf0 == 0x90 && data[1] != 0,
        }),
        0xb0 if data.len() >= 2 => Some(MidiMessage::ControlChange {
            channel,
            controller: data[0],
            value: data[1],
        }),
        _ => None,
    }
}

/// Map the 7-bit MIDI fader around value 64, the physical centre position.
/// The two piecewise halves preserve both endpoints while landing value 64
/// exactly on `tempo = 0.5` and therefore the EP122 dead zone.
fn tempo_from_midi(value: u8) -> f32 {
    let value = value.min(127);
    if value <= 64 {
        value as f32 / 128.0
    } else {
        0.5 + (value - 64) as f32 / 126.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flx4_deck_led_messages_use_the_correct_banks() {
        let mut states = [false; 11];
        states[0] = true;
        states[8] = true;
        states[10] = true;
        let left = deck_led_messages(0, states);
        let right = deck_led_messages(1, states);
        assert_eq!(left[0], [0x97, 0, 0x7f]);
        assert_eq!(left[1], [0x97, 1, 0]);
        assert_eq!(right[7], [0x99, 7, 0]);
        assert_eq!(left[8], [0x90, 0x10, 0x7f]);
        assert_eq!(right[9], [0x91, 0x11, 0]);
        assert_eq!(right[10], [0x91, 0x58, 0x7f]);
    }

    #[test]
    fn empty_white_hot_cue_backlights_do_not_light_flx4_pads() {
        assert!(!hot_cue_led_has_assignment((0, 0, 0)));
        assert!(!hot_cue_led_has_assignment((24, 24, 24)));
        assert!(!hot_cue_led_has_assignment((30, 28, 29)));
        assert!(!hot_cue_led_has_assignment((255, 245, 240)));
        assert!(hot_cue_led_has_assignment((0, 24, 100)));
        assert!(hot_cue_led_has_assignment((120, 70, 0)));
    }

    #[test]
    fn learned_flx4_fx_button_uses_its_assigned_output_address() {
        let binding = MidiBinding {
            port_name: Some("DDJ-FLX4".to_owned()),
            channel: 1,
            kind: MidiMessageKind::Note,
            number: 42,
        };
        let address = midi_led_address(&binding).unwrap();
        assert_eq!(address, [0x91, 42]);
        let mut states = Vec::new();
        push_midi_led_state(&mut states, address, false);
        push_midi_led_state(&mut states, address, true);
        assert_eq!(states, vec![(address, true)]);
    }

    #[test]
    fn shared_tabs_partition_deck_and_mixer_controls() {
        for slot in 0..4 {
            let tab = MidiTab::Slot(slot);
            assert_eq!(MidiTab::from_shared(tab.shared_index()), tab);
        }
        assert_eq!(MidiTab::from_shared(4), MidiTab::Mixer);
        assert_eq!(MidiTarget::DECK.len(), 26);
        assert_eq!(EXTRA_BUTTONS.len(), MIDI_EXTRA_BUTTON_KEYS.len());
        assert_eq!(MidiTarget::MIXER.len(), 6);
        assert!(MidiTarget::DECK
            .iter()
            .all(|target| target.mixer_parameter().is_none()));
        assert!(MidiTarget::MIXER
            .iter()
            .all(|target| target.mixer_parameter().is_some()));
    }

    #[test]
    fn activate_deck_triggers_once_per_press_and_matches_its_slot() {
        let binding = MidiBinding {
            port_name: Some("Controller".to_owned()),
            channel: 1,
            kind: MidiMessageKind::Note,
            number: 70,
        };
        let press = MidiMessage::Note {
            channel: 1,
            note: 70,
            pressed: true,
        };
        let release = MidiMessage::Note {
            channel: 1,
            note: 70,
            pressed: false,
        };
        let mut held = std::collections::HashSet::new();
        assert!(!activation_press_edge(
            &mut held,
            Some(&binding),
            1,
            "Other",
            &press
        ));
        assert!(activation_press_edge(
            &mut held,
            Some(&binding),
            1,
            "Controller",
            &press
        ));

        let cc_binding = MidiBinding {
            kind: MidiMessageKind::ControlChange,
            number: 71,
            ..binding.clone()
        };
        let cc_on = MidiMessage::ControlChange {
            channel: 1,
            controller: 71,
            value: 127,
        };
        let cc_off = MidiMessage::ControlChange {
            channel: 1,
            controller: 71,
            value: 0,
        };
        assert!(activation_press_edge(
            &mut held,
            Some(&cc_binding),
            1,
            "Controller",
            &cc_on
        ));
        assert!(!activation_press_edge(
            &mut held,
            Some(&cc_binding),
            1,
            "Controller",
            &cc_off
        ));
        assert!(activation_press_edge(
            &mut held,
            Some(&cc_binding),
            1,
            "Controller",
            &cc_on
        ));
        assert!(!activation_press_edge(
            &mut held,
            Some(&binding),
            1,
            "Controller",
            &press
        ));
        assert!(!activation_press_edge(
            &mut held,
            Some(&binding),
            1,
            "Controller",
            &release
        ));
        assert!(activation_press_edge(
            &mut held,
            Some(&binding),
            1,
            "Controller",
            &press
        ));
    }

    #[test]
    fn platter_and_outer_ring_learn_separate_cc_bindings() {
        let mut assignments = MidiAssignments::for_instance(1);
        let platter = MidiMessage::ControlChange {
            channel: 0,
            controller: 34,
            value: 65,
        };
        let ring = MidiMessage::ControlChange {
            channel: 0,
            controller: 33,
            value: 65,
        };
        MidiTarget::JogPlatter.set_binding(
            &mut assignments,
            learned_binding(MidiTarget::JogPlatter, "Controller", &platter),
        );
        MidiTarget::JogRing.set_binding(
            &mut assignments,
            learned_binding(MidiTarget::JogRing, "Controller", &ring),
        );
        let platter_binding = MidiTarget::JogPlatter
            .binding(&assignments)
            .as_ref()
            .unwrap();
        let ring_binding = MidiTarget::JogRing.binding(&assignments).as_ref().unwrap();
        assert_eq!(platter_binding.number, 34);
        assert_eq!(ring_binding.number, 33);
        assert!(!binding_matches(
            platter_binding,
            "Controller",
            ring.channel(),
            ring.kind_and_number().0,
            ring.kind_and_number().1,
        ));
        assert!(!binding_matches(
            ring_binding,
            "Controller",
            platter.channel(),
            platter.kind_and_number().0,
            platter.kind_and_number().1,
        ));
    }

    #[test]
    fn short_midi_tap_remains_pressed_until_guest_can_observe_it() {
        let mut held = HashMap::new();
        let id = MidiButtonId {
            source: 1,
            channel: 0,
            kind: MidiMessageKind::Note,
            number: 88,
            button: miso_frame::BTN_BEAT_SYNC,
        };
        let press_at = Instant::now();
        assert!(update_midi_button(&mut held, id, true, press_at));
        assert!(!update_midi_button(
            &mut held,
            id,
            false,
            press_at + Duration::from_millis(5)
        ));
        assert!(held.contains_key(&id));
        assert_eq!(held[&id].release_at, Some(press_at + MIN_MIDI_BUTTON_PRESS));
        assert!(!expire_midi_buttons(
            &mut held,
            press_at + Duration::from_millis(59)
        ));
        assert!(expire_midi_buttons(
            &mut held,
            press_at + MIN_MIDI_BUTTON_PRESS
        ));
        assert!(held.is_empty());
    }

    #[test]
    fn rotary_press_only_works_on_focused_deck_and_releases_on_blur() {
        assert!(button_allowed_for_focus(miso_frame::BTN_ROTARY_PRESS, true));
        assert!(!button_allowed_for_focus(
            miso_frame::BTN_ROTARY_PRESS,
            false
        ));
        assert!(button_allowed_for_focus(miso_frame::BTN_BEAT_SYNC, false));

        let mut held = HashMap::new();
        let rotary = MidiButtonId {
            source: 1,
            channel: 0,
            kind: MidiMessageKind::Note,
            number: 17,
            button: miso_frame::BTN_ROTARY_PRESS,
        };
        let other = MidiButtonId {
            button: miso_frame::BTN_BEAT_SYNC,
            ..rotary
        };
        let now = Instant::now();
        update_midi_button(&mut held, rotary, true, now);
        update_midi_button(&mut held, other, true, now);
        assert!(!release_unfocused_rotary_press(&mut held, true));
        assert!(release_unfocused_rotary_press(&mut held, false));
        assert!(!held.contains_key(&rotary));
        assert!(held.contains_key(&other));
    }

    #[test]
    fn main_screen_popout_counts_as_the_active_deck_window() {
        assert!(deck_focus_from_viewports(true, false, false));
        assert!(deck_focus_from_viewports(false, true, true));
        assert!(!deck_focus_from_viewports(false, true, false));
        assert!(!deck_focus_from_viewports(false, false, true));
    }

    #[test]
    fn parses_note_press_and_both_release_forms() {
        assert_eq!(
            parse_midi_message(&[0x95, 0x24, 0x7f]),
            Some(MidiMessage::Note {
                channel: 5,
                note: 0x24,
                pressed: true,
            })
        );
        assert!(matches!(
            parse_midi_message(&[0x85, 0x24, 0x40]),
            Some(MidiMessage::Note { pressed: false, .. })
        ));
        assert!(matches!(
            parse_midi_message(&[0x95, 0x24, 0x00]),
            Some(MidiMessage::Note { pressed: false, .. })
        ));
    }

    #[test]
    fn parses_any_control_change_for_learning() {
        assert_eq!(
            parse_midi_message(&[0xb3, 74, 96]),
            Some(MidiMessage::ControlChange {
                channel: 3,
                controller: 74,
                value: 96,
            })
        );
    }

    #[test]
    fn learned_binding_is_port_and_channel_specific() {
        let message = MidiMessage::Note {
            channel: 2,
            note: 40,
            pressed: true,
        };
        let binding = learned_binding(MidiTarget::Play, "Controller A", &message).unwrap();
        assert!(binding_matches(
            &binding,
            "Controller A",
            2,
            MidiMessageKind::Note,
            40
        ));
        assert!(!binding_matches(
            &binding,
            "Controller B",
            2,
            MidiMessageKind::Note,
            40
        ));
        assert!(!binding_matches(
            &binding,
            "Controller A",
            1,
            MidiMessageKind::Note,
            40
        ));
    }

    #[test]
    fn crossfader_learning_requires_a_control_change() {
        let target = MidiTarget::MixerExtra(MixerMidiExtra::Crossfader);
        assert!(learned_binding(
            target,
            "DJ Mixer",
            &MidiMessage::Note {
                channel: 0,
                note: 33,
                pressed: true,
            },
        )
        .is_none());
        assert_eq!(
            learned_binding(
                target,
                "DJ Mixer",
                &MidiMessage::ControlChange {
                    channel: 0,
                    controller: 33,
                    value: 64,
                },
            )
            .unwrap()
            .number,
            33
        );
    }

    #[test]
    fn tempo_learning_requires_a_control_change() {
        let note = MidiMessage::Note {
            channel: 0,
            note: 1,
            pressed: true,
        };
        let cc = MidiMessage::ControlChange {
            channel: 0,
            controller: 29,
            value: 64,
        };
        assert!(learned_binding(MidiTarget::Tempo, "Controller", &note).is_none());
        assert!(learned_binding(MidiTarget::Tempo, "Controller", &cc).is_some());
        assert!(learned_binding(MidiTarget::JogPlatter, "Controller", &note).is_none());
        assert!(learned_binding(MidiTarget::JogRing, "Controller", &cc).is_some());
        assert!(learned_binding(MidiTarget::MixerFilter, "Controller", &note).is_none());
        assert!(learned_binding(MidiTarget::MixerFilter, "Controller", &cc).is_some());
        assert!(learned_binding(MidiTarget::RotaryTurn, "Controller", &note).is_none());
        assert!(learned_binding(MidiTarget::RotaryTurn, "Controller", &cc).is_some());
    }

    #[test]
    fn rotary_turn_decodes_detents_and_only_matches_focused_deck() {
        assert_eq!(rotary_clicks(65, MidiJogMode::RelativeOffset64), 1);
        assert_eq!(rotary_clicks(63, MidiJogMode::RelativeOffset64), -1);
        assert_eq!(rotary_clicks(64, MidiJogMode::RelativeOffset64), 0);
        assert_eq!(rotary_clicks(1, MidiJogMode::RelativeTwosComplement), 1);
        assert_eq!(rotary_clicks(127, MidiJogMode::RelativeTwosComplement), -1);
        let binding = MidiBinding {
            port_name: Some("Encoder".to_owned()),
            channel: 0,
            kind: MidiMessageKind::ControlChange,
            number: 42,
        };
        assert!(rotary_binding_matches(
            Some(&binding),
            true,
            "Encoder",
            0,
            MidiMessageKind::ControlChange,
            42
        ));
        assert!(!rotary_binding_matches(
            Some(&binding),
            false,
            "Encoder",
            0,
            MidiMessageKind::ControlChange,
            42
        ));
        assert!(!rotary_binding_matches(
            Some(&binding),
            true,
            "Other",
            0,
            MidiMessageKind::ControlChange,
            42
        ));
    }

    #[test]
    fn new_buttons_map_to_the_emulated_switches() {
        assert_eq!(
            MidiTarget::CallDelete.button(),
            Some(miso_frame::BTN_CALL_DELETE)
        );
        assert_eq!(MidiTarget::LoopIn.button(), Some(miso_frame::BTN_LOOP_IN));
        assert_eq!(MidiTarget::LoopOut.button(), Some(miso_frame::BTN_LOOP_OUT));
        assert_eq!(
            MidiTarget::ReloopExit.button(),
            Some(miso_frame::BTN_RELOOP)
        );
        assert_eq!(
            MidiTarget::BeatSync.button(),
            Some(miso_frame::BTN_BEAT_SYNC)
        );
        assert_eq!(
            MidiTarget::MasterTempo.button(),
            Some(miso_frame::BTN_MASTER_TEMPO)
        );
        assert_eq!(
            MidiTarget::TempoRange.button(),
            Some(miso_frame::BTN_TEMPO_RANGE)
        );
        assert_eq!(
            MidiTarget::TempoReset.button(),
            Some(miso_frame::BTN_TEMPO_RESET)
        );
        assert_eq!(MidiTarget::JogTouch.button(), None);
    }

    #[test]
    fn jog_cc_modes_preserve_direction_and_center_stop() {
        use std::f32::consts::TAU;

        let dt = 0.25;
        assert_eq!(
            jog_delta_rad(64, dt, MidiJogMode::SpeedCentered, false, 128),
            0.0
        );
        assert!(
            (jog_delta_rad(127, dt, MidiJogMode::SpeedCentered, false, 128) - TAU).abs() < 1e-5
        );
        assert!((jog_delta_rad(0, dt, MidiJogMode::SpeedCentered, true, 128) + TAU).abs() < 1e-5);
        assert!(jog_delta_rad(65, dt, MidiJogMode::SpeedCentered, true, 128) > 0.0);
        assert!(jog_delta_rad(63, dt, MidiJogMode::SpeedCentered, true, 128) < 0.0);

        let flx4_forward = jog_delta_rad(65, dt, MidiJogMode::RelativeOffset64, false, 128);
        let flx4_backward = jog_delta_rad(63, dt, MidiJogMode::RelativeOffset64, true, 128);
        assert!((flx4_forward - TAU / 128.0).abs() < 1e-5);
        assert!((flx4_backward + TAU / 128.0).abs() < 1e-5);
        assert!(
            (jog_delta_rad(65, dt, MidiJogMode::RelativeOffset64, false, 256) - TAU / 256.0).abs()
                < 1e-5
        );
        assert!(
            (jog_delta_rad(65, dt, MidiJogMode::RelativeOffset64, false, 64) - TAU / 64.0).abs()
                < 1e-5
        );
        assert_eq!(
            jog_delta_rad(64, dt, MidiJogMode::RelativeOffset64, false, 128),
            0.0
        );

        let forward = jog_delta_rad(1, dt, MidiJogMode::RelativeTwosComplement, false, 128);
        let backward = jog_delta_rad(127, dt, MidiJogMode::RelativeTwosComplement, false, 128);
        assert!((forward + backward).abs() < 1e-6);
        assert_eq!(
            jog_delta_rad(0, dt, MidiJogMode::RelativeTwosComplement, false, 128),
            0.0
        );
    }

    #[test]
    fn maps_tempo_endpoints_and_physical_centre() {
        assert_eq!(tempo_from_midi(0), 0.0);
        assert_eq!(tempo_from_midi(64), 0.5);
        assert_eq!(tempo_from_midi(127), 1.0);
    }

    #[test]
    fn ignores_unhandled_and_incomplete_messages() {
        assert_eq!(parse_midi_message(&[]), None);
        assert_eq!(parse_midi_message(&[0x90, 0]), None);
        assert_eq!(parse_midi_message(&[0xf8]), None);
    }

    #[test]
    fn coremidi_source_keys_are_stable_and_port_specific() {
        assert_eq!(stable_source_key("1234"), stable_source_key("1234"));
        assert_ne!(stable_source_key("1234"), stable_source_key("5678"));
    }
}
