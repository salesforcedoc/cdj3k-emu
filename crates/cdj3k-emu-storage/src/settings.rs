//! Persistent settings: a shared app file plus a per-instance file.
//!
//! Layout (macOS):
//!   ~/Library/Application Support/<BUNDLE_ID>/settings.txt            - app-wide
//!   ~/Library/Application Support/<BUNDLE_ID>/instance-N/settings.txt - per instance
//!
//! Format is plain `key=value\n` lines. Unknown keys are preserved on save so
//! older builds don't drop newer fields. No serde dep; the value space is tiny
//! and the file is human-editable for debugging.

use std::collections::BTreeMap;
use std::path::PathBuf;

use cdj3k_emu_platform::menu_state::{
    AudioLatencyProfile, AudioLatencySettings, AudioLatencyTuning,
};
use cdj3k_emu_platform::mixer::MixerParameter;

fn app_path() -> PathBuf {
    crate::app_data_dir().join("settings.txt")
}

fn instance_path(instance_id: u32) -> PathBuf {
    crate::app_data_dir()
        .join(format!("instance-{}", instance_id))
        .join("settings.txt")
}

fn midi_profiles_dir(instance_id: u32) -> PathBuf {
    crate::app_data_dir()
        .join("midi-profiles")
        .join(format!("player-{instance_id}"))
}

fn mixer_midi_profiles_dir() -> PathBuf {
    crate::app_data_dir().join("midi-profiles").join("mixer")
}

fn all_midi_profiles_dir() -> PathBuf {
    crate::app_data_dir().join("midi-profiles").join("all")
}

fn read_kv(path: &std::path::Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(s) = std::fs::read_to_string(path) {
        for line in s.lines() {
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    map
}

fn write_kv(path: &std::path::Path, map: &BTreeMap<String, String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut buf = String::new();
    for (k, v) in map {
        buf.push_str(k);
        buf.push('=');
        buf.push_str(v);
        buf.push('\n');
    }
    std::fs::write(path, buf)
}

// ── App-wide settings ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AppSettings {
    pub jog_adjust: f32, // [0, 1]
    pub vinyl_speed: u8,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            jog_adjust: 0.5,
            vinyl_speed: 0,
        }
    }
}

impl AppSettings {
    pub fn load() -> Self {
        let map = read_kv(&app_path());
        let mut s = Self::default();
        if let Some(v) = map.get("jog_adjust").and_then(|v| v.parse().ok()) {
            s.jog_adjust = f32::clamp(v, 0.0, 1.0);
        }
        if let Some(v) = map.get("vinyl_speed").and_then(|v| v.parse().ok()) {
            s.vinyl_speed = v;
        }
        s
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = app_path();
        let mut map = read_kv(&path); // preserve unknown keys
        map.insert("jog_adjust".into(), format!("{}", self.jog_adjust));
        map.insert("vinyl_speed".into(), format!("{}", self.vinyl_speed));
        write_kv(&path, &map)
    }
}

/// Load the application-wide CoreAudio output selection. Older builds stored
/// this per player, so on the first launch after upgrading migrate the first
/// non-default value found in Players 1-4 into the shared settings file.
pub fn load_audio_device_uid() -> Option<String> {
    let path = app_path();
    let mut map = read_kv(&path);
    let selected = map
        .get("audio_device_uid")
        .map(|value| (!value.is_empty()).then(|| value.clone()))
        .unwrap_or_else(|| {
            (1..=4).find_map(|instance_id| {
                read_kv(&instance_path(instance_id))
                    .get("audio_device_uid")
                    .filter(|value| !value.is_empty())
                    .cloned()
            })
        });
    map.insert(
        "audio_device_uid".into(),
        selected.clone().unwrap_or_default(),
    );
    if let Err(error) = write_kv(&path, &map) {
        eprintln!("cdj3k-emu-storage: failed to persist shared audio device: {error}");
        return selected;
    }
    for instance_id in 1..=4 {
        let legacy_path = instance_path(instance_id);
        let mut legacy = read_kv(&legacy_path);
        if legacy.remove("audio_device_uid").is_some() {
            if let Err(error) = write_kv(&legacy_path, &legacy) {
                eprintln!(
                    "cdj3k-emu-storage: failed to remove Player {instance_id} audio device: {error}"
                );
            }
        }
    }
    selected
}

/// Persist the one CoreAudio output used by the shared native mixer.
pub fn save_audio_device_uid(device_uid: Option<&str>) -> std::io::Result<()> {
    let path = app_path();
    let mut map = read_kv(&path);
    map.insert(
        "audio_device_uid".into(),
        device_uid.unwrap_or_default().to_string(),
    );
    write_kv(&path, &map)
}

/// Dedicated headphone cue outputs. `None` disables that bus until an output
/// is selected; unlike Master, it does not follow the system default.
pub fn load_cue_device_uid(bus: char) -> Option<String> {
    let key = format!("cue_{}_device_uid", bus.to_ascii_lowercase());
    read_kv(&app_path())
        .get(&key)
        .filter(|value| !value.is_empty())
        .cloned()
}

pub fn save_cue_device_uid(bus: char, device_uid: Option<&str>) -> std::io::Result<()> {
    if !matches!(bus, 'A' | 'B') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid cue bus",
        ));
    }
    let path = app_path();
    let mut map = read_kv(&path);
    map.insert(
        format!("cue_{}_device_uid", bus.to_ascii_lowercase()),
        device_uid.unwrap_or_default().to_owned(),
    );
    write_kv(&path, &map)
}

// ── Per-instance settings ─────────────────────────────────────────────────────

/// MIDI message type stored in a per-player control assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MidiMessageKind {
    Note,
    ControlChange,
}

/// Interpretation of a jog wheel's 7-bit CC value. CDJ-3000 jog messages
/// report signed speed around 64; many other controllers report relative
/// movement as 1 (forward) / 127 (backward) instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiJogMode {
    SpeedCentered,
    RelativeTwosComplement,
    /// Difference count biased by 64: 65 is +1, 63 is -1.
    RelativeOffset64,
}

impl MidiJogMode {
    fn load(map: &BTreeMap<String, String>, key: &str) -> Self {
        match map.get(key).map(String::as_str) {
            Some("relative") => Self::RelativeTwosComplement,
            Some("relative_64") => Self::RelativeOffset64,
            _ => Self::SpeedCentered,
        }
    }

    fn save(self, map: &mut BTreeMap<String, String>, key: &str) {
        let value = match self {
            Self::SpeedCentered => "speed",
            Self::RelativeTwosComplement => "relative",
            Self::RelativeOffset64 => "relative_64",
        };
        map.insert(key.to_owned(), value.to_owned());
    }
}

/// One learned MIDI input assignment. `port_name = None` accepts the message
/// from any CoreMIDI source; learned assignments always store a concrete port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiBinding {
    pub port_name: Option<String>,
    /// Zero-based MIDI channel (`0..=15`).
    pub channel: u8,
    pub kind: MidiMessageKind,
    /// Note or controller number (`0..=127`).
    pub number: u8,
}

/// Learnable physical CDJ switches beyond the core transport/hot-cue group.
/// Indices are stable so saved assignments remain compatible across releases.
pub const MIDI_EXTRA_BUTTON_KEYS: &[&str] = &[
    "search_prev",
    "search_next",
    "track_prev",
    "track_next",
    "beatjump_prev",
    "beatjump_next",
    "auto_cue",
    "key_sync",
    "master",
    "beatloop_half",
    "beatloop_2x",
    "slip",
    "memory",
    "delete",
    "call_prev",
    "call_next",
    "source",
    "browse",
    "tag_list",
    "playlist",
    "search_menu",
    "menu",
    "jog_mode",
    "back",
    "tag_track",
    "track_filter",
    "shortcut",
    "rotary_press",
    "sleep",
    "time_mode",
    "quantize",
    "usb_stop",
    "power_on",
];

/// Per-player bindings for the MIDI-enabled controls. Each slot has its own
/// copy in `instance-N/settings.txt` so multiple emulator windows can listen
/// to different devices or MIDI channels without cross-triggering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiAssignments {
    pub play: Option<MidiBinding>,
    pub cue: Option<MidiBinding>,
    pub hot_cues: [Option<MidiBinding>; 8],
    pub tempo: Option<MidiBinding>,
    pub call_delete: Option<MidiBinding>,
    pub loop_in: Option<MidiBinding>,
    pub loop_out: Option<MidiBinding>,
    pub reloop_exit: Option<MidiBinding>,
    pub beat_sync: Option<MidiBinding>,
    pub master_tempo: Option<MidiBinding>,
    pub tempo_range: Option<MidiBinding>,
    pub tempo_reset: Option<MidiBinding>,
    pub jog_platter: Option<MidiBinding>,
    pub jog_ring: Option<MidiBinding>,
    pub jog_touch: Option<MidiBinding>,
    pub rotary_turn: Option<MidiBinding>,
    pub activate_deck: Option<MidiBinding>,
    pub direction_reverse: Option<MidiBinding>,
    pub direction_slip_reverse: Option<MidiBinding>,
    pub extra_buttons: Vec<Option<MidiBinding>>,
    pub mixer_trim: Option<MidiBinding>,
    pub mixer_high: Option<MidiBinding>,
    pub mixer_mid: Option<MidiBinding>,
    pub mixer_low: Option<MidiBinding>,
    pub mixer_filter: Option<MidiBinding>,
    pub mixer_fader: Option<MidiBinding>,
    pub jog_platter_mode: MidiJogMode,
    pub jog_ring_mode: MidiJogMode,
    pub rotary_turn_mode: MidiJogMode,
    pub jog_platter_steps_per_rev: u16,
    pub jog_ring_steps_per_rev: u16,
}

impl MidiAssignments {
    /// CDJ-3000-compatible defaults, separated by player: slot 1 uses MIDI
    /// channel 1, slot 2 channel 2, and so on. The source is initially `Any`;
    /// MIDI Learn pins a binding to a specific CoreMIDI port.
    pub fn for_instance(instance_id: u32) -> Self {
        let channel = instance_id.saturating_sub(1).min(15) as u8;
        let note = |number| {
            Some(MidiBinding {
                port_name: None,
                channel,
                kind: MidiMessageKind::Note,
                number,
            })
        };
        let cc = |number| {
            Some(MidiBinding {
                port_name: None,
                channel,
                kind: MidiMessageKind::ControlChange,
                number,
            })
        };
        Self {
            play: note(0x00),
            cue: note(0x01),
            hot_cues: std::array::from_fn(|index| note(0x1b + index as u8)),
            tempo: cc(0x1d),
            call_delete: note(0x1a),
            loop_in: note(0x23),
            loop_out: note(0x24),
            reloop_exit: note(0x25),
            beat_sync: note(0x16),
            master_tempo: note(0x0b),
            tempo_range: note(0x0a),
            tempo_reset: note(0x09),
            jog_platter: cc(0x10),
            jog_ring: cc(0x30),
            jog_touch: note(0x17),
            rotary_turn: None,
            activate_deck: None,
            direction_reverse: note(0x19),
            direction_slip_reverse: note(0x18),
            extra_buttons: MIDI_EXTRA_BUTTON_KEYS
                .iter()
                .map(|key| {
                    // Published CDJ-3000 MIDI input notes. Switches without a
                    // documented MIDI input remain unassigned until learned.
                    let number = match *key {
                        "search_prev" => Some(0x03),
                        "search_next" => Some(0x02),
                        "track_prev" => Some(0x05),
                        "track_next" => Some(0x04),
                        "beatjump_prev" => Some(0x37),
                        "beatjump_next" => Some(0x36),
                        "key_sync" => Some(0x48),
                        "master" => Some(0x15),
                        "slip" => Some(0x07),
                        "memory" => Some(0x0d),
                        "delete" => Some(0x10),
                        "call_prev" => Some(0x0f),
                        "call_next" => Some(0x0e),
                        "jog_mode" => Some(0x0c),
                        "back" => Some(0x12),
                        "tag_track" => Some(0x13),
                        "track_filter" => Some(0x14),
                        "rotary_press" => Some(0x11),
                        "time_mode" => Some(0x08),
                        "quantize" => Some(0x06),
                        _ => None,
                    };
                    number.and_then(note)
                })
                .collect(),
            mixer_trim: None,
            mixer_high: None,
            mixer_mid: None,
            mixer_low: None,
            mixer_filter: None,
            mixer_fader: None,
            jog_platter_mode: MidiJogMode::SpeedCentered,
            jog_ring_mode: MidiJogMode::SpeedCentered,
            rotary_turn_mode: MidiJogMode::RelativeOffset64,
            jog_platter_steps_per_rev: 128,
            jog_ring_steps_per_rev: 128,
        }
    }

    fn load(map: &BTreeMap<String, String>, instance_id: u32) -> Self {
        let defaults = Self::for_instance(instance_id);
        Self {
            play: load_midi_binding(map, "midi_play", defaults.play),
            cue: load_midi_binding(map, "midi_cue", defaults.cue),
            hot_cues: std::array::from_fn(|index| {
                let key = format!("midi_hot_{}", (b'a' + index as u8) as char);
                load_midi_binding(map, &key, defaults.hot_cues[index].clone())
            }),
            tempo: load_midi_binding(map, "midi_tempo", defaults.tempo),
            call_delete: load_midi_binding(map, "midi_call_delete", defaults.call_delete),
            loop_in: load_midi_binding(map, "midi_loop_in", defaults.loop_in),
            loop_out: load_midi_binding(map, "midi_loop_out", defaults.loop_out),
            reloop_exit: load_midi_binding(map, "midi_reloop_exit", defaults.reloop_exit),
            beat_sync: load_midi_binding(map, "midi_beat_sync", defaults.beat_sync),
            master_tempo: load_midi_binding(map, "midi_master_tempo", defaults.master_tempo),
            tempo_range: load_midi_binding(map, "midi_tempo_range", defaults.tempo_range),
            tempo_reset: load_midi_binding(map, "midi_tempo_reset", defaults.tempo_reset),
            jog_platter: load_midi_binding(map, "midi_jog_platter", defaults.jog_platter),
            jog_ring: load_midi_binding(map, "midi_jog_ring", defaults.jog_ring),
            jog_touch: load_midi_binding(map, "midi_jog_touch", defaults.jog_touch),
            rotary_turn: load_midi_binding(map, "midi_rotary_turn", defaults.rotary_turn),
            activate_deck: load_midi_binding(map, "midi_activate_deck", defaults.activate_deck),
            direction_reverse: load_midi_binding(
                map,
                "midi_direction_reverse",
                defaults.direction_reverse,
            ),
            direction_slip_reverse: load_midi_binding(
                map,
                "midi_direction_slip_reverse",
                defaults.direction_slip_reverse,
            ),
            extra_buttons: MIDI_EXTRA_BUTTON_KEYS
                .iter()
                .enumerate()
                .map(|(index, key)| {
                    load_midi_binding(
                        map,
                        &format!("midi_extra_{key}"),
                        defaults.extra_buttons[index].clone(),
                    )
                })
                .collect(),
            mixer_trim: load_midi_binding(map, "midi_mixer_trim", defaults.mixer_trim),
            mixer_high: load_midi_binding(map, "midi_mixer_high", defaults.mixer_high),
            mixer_mid: load_midi_binding(map, "midi_mixer_mid", defaults.mixer_mid),
            mixer_low: load_midi_binding(map, "midi_mixer_low", defaults.mixer_low),
            mixer_filter: load_midi_binding(map, "midi_mixer_filter", defaults.mixer_filter),
            mixer_fader: load_midi_binding(map, "midi_mixer_fader", defaults.mixer_fader),
            jog_platter_mode: MidiJogMode::load(map, "midi_jog_platter_mode"),
            jog_ring_mode: MidiJogMode::load(map, "midi_jog_ring_mode"),
            rotary_turn_mode: match map.get("midi_rotary_turn_mode").map(String::as_str) {
                Some("relative") => MidiJogMode::RelativeTwosComplement,
                _ => MidiJogMode::RelativeOffset64,
            },
            jog_platter_steps_per_rev: map
                .get("midi_jog_platter_steps_per_rev")
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(128)
                .clamp(1, 8192),
            jog_ring_steps_per_rev: map
                .get("midi_jog_ring_steps_per_rev")
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(128)
                .clamp(1, 8192),
        }
    }

    fn save(&self, map: &mut BTreeMap<String, String>) {
        save_midi_binding(map, "midi_play", self.play.as_ref());
        save_midi_binding(map, "midi_cue", self.cue.as_ref());
        for (index, binding) in self.hot_cues.iter().enumerate() {
            let key = format!("midi_hot_{}", (b'a' + index as u8) as char);
            save_midi_binding(map, &key, binding.as_ref());
        }
        save_midi_binding(map, "midi_tempo", self.tempo.as_ref());
        save_midi_binding(map, "midi_call_delete", self.call_delete.as_ref());
        save_midi_binding(map, "midi_loop_in", self.loop_in.as_ref());
        save_midi_binding(map, "midi_loop_out", self.loop_out.as_ref());
        save_midi_binding(map, "midi_reloop_exit", self.reloop_exit.as_ref());
        save_midi_binding(map, "midi_beat_sync", self.beat_sync.as_ref());
        save_midi_binding(map, "midi_master_tempo", self.master_tempo.as_ref());
        save_midi_binding(map, "midi_tempo_range", self.tempo_range.as_ref());
        save_midi_binding(map, "midi_tempo_reset", self.tempo_reset.as_ref());
        save_midi_binding(map, "midi_jog_platter", self.jog_platter.as_ref());
        save_midi_binding(map, "midi_jog_ring", self.jog_ring.as_ref());
        save_midi_binding(map, "midi_jog_touch", self.jog_touch.as_ref());
        save_midi_binding(map, "midi_rotary_turn", self.rotary_turn.as_ref());
        save_midi_binding(map, "midi_activate_deck", self.activate_deck.as_ref());
        save_midi_binding(
            map,
            "midi_direction_reverse",
            self.direction_reverse.as_ref(),
        );
        save_midi_binding(
            map,
            "midi_direction_slip_reverse",
            self.direction_slip_reverse.as_ref(),
        );
        for (key, binding) in MIDI_EXTRA_BUTTON_KEYS.iter().zip(&self.extra_buttons) {
            save_midi_binding(map, &format!("midi_extra_{key}"), binding.as_ref());
        }
        save_midi_binding(map, "midi_mixer_trim", self.mixer_trim.as_ref());
        save_midi_binding(map, "midi_mixer_high", self.mixer_high.as_ref());
        save_midi_binding(map, "midi_mixer_mid", self.mixer_mid.as_ref());
        save_midi_binding(map, "midi_mixer_low", self.mixer_low.as_ref());
        save_midi_binding(map, "midi_mixer_filter", self.mixer_filter.as_ref());
        save_midi_binding(map, "midi_mixer_fader", self.mixer_fader.as_ref());
        self.jog_platter_mode.save(map, "midi_jog_platter_mode");
        self.jog_ring_mode.save(map, "midi_jog_ring_mode");
        self.rotary_turn_mode.save(map, "midi_rotary_turn_mode");
        map.insert(
            "midi_jog_platter_steps_per_rev".into(),
            self.jog_platter_steps_per_rev.clamp(1, 8192).to_string(),
        );
        map.insert(
            "midi_jog_ring_steps_per_rev".into(),
            self.jog_ring_steps_per_rev.clamp(1, 8192).to_string(),
        );
    }
}

/// MIDI bindings for the shared four-channel mixer. Unlike deck controls,
/// these are stored once in the application settings and handled by the one
/// elected mixer UI process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MixerMidiExtra {
    CrossAssign,
    Crossfader,
    CueA,
    CueB,
    EqCurve,
    MasterLevel,
    CueALevel,
    CueBLevel,
    FxType,
    FxChannel,
    FxOn,
    FxDepth,
    FxLow,
    FxMid,
    FxHigh,
}

impl MixerMidiExtra {
    fn key(self) -> &'static str {
        match self {
            Self::CrossAssign => "cross_assign",
            Self::Crossfader => "crossfader",
            Self::CueA => "cue_a",
            Self::CueB => "cue_b",
            Self::EqCurve => "eq_curve",
            Self::MasterLevel => "master_level",
            Self::CueALevel => "cue_a_level",
            Self::CueBLevel => "cue_b_level",
            Self::FxType => "fx_type",
            Self::FxChannel => "fx_channel",
            Self::FxOn => "fx_on",
            Self::FxDepth => "fx_depth",
            Self::FxLow => "fx_low",
            Self::FxMid => "fx_mid",
            Self::FxHigh => "fx_high",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiMixerAssignments {
    pub channels: [[Option<MidiBinding>; 6]; 4],
    pub cross_assign: [Option<MidiBinding>; 4],
    pub crossfader: Option<MidiBinding>,
    pub cue_a: [Option<MidiBinding>; 5],
    pub cue_b: [Option<MidiBinding>; 5],
    pub eq_curve: Option<MidiBinding>,
    pub master_level: Option<MidiBinding>,
    pub cue_a_level: Option<MidiBinding>,
    pub cue_b_level: Option<MidiBinding>,
    pub fx_type: Option<MidiBinding>,
    pub fx_channel: Option<MidiBinding>,
    pub fx_on: Option<MidiBinding>,
    pub fx_depth: Option<MidiBinding>,
    pub fx_low: Option<MidiBinding>,
    pub fx_mid: Option<MidiBinding>,
    pub fx_high: Option<MidiBinding>,
}

impl Default for MidiMixerAssignments {
    fn default() -> Self {
        Self {
            channels: std::array::from_fn(|_| std::array::from_fn(|_| None)),
            cross_assign: std::array::from_fn(|_| None),
            crossfader: None,
            cue_a: std::array::from_fn(|_| None),
            cue_b: std::array::from_fn(|_| None),
            eq_curve: None,
            master_level: None,
            cue_a_level: None,
            cue_b_level: None,
            fx_type: None,
            fx_channel: None,
            fx_on: None,
            fx_depth: None,
            fx_low: None,
            fx_mid: None,
            fx_high: None,
        }
    }
}

impl MidiMixerAssignments {
    pub fn extra_binding(&self, channel: usize, extra: MixerMidiExtra) -> Option<&MidiBinding> {
        match extra {
            MixerMidiExtra::CrossAssign => self.cross_assign.get(channel)?.as_ref(),
            MixerMidiExtra::Crossfader => self.crossfader.as_ref(),
            MixerMidiExtra::CueA => self.cue_a.get(channel)?.as_ref(),
            MixerMidiExtra::CueB => self.cue_b.get(channel)?.as_ref(),
            MixerMidiExtra::EqCurve => self.eq_curve.as_ref(),
            MixerMidiExtra::MasterLevel => self.master_level.as_ref(),
            MixerMidiExtra::CueALevel => self.cue_a_level.as_ref(),
            MixerMidiExtra::CueBLevel => self.cue_b_level.as_ref(),
            MixerMidiExtra::FxType => self.fx_type.as_ref(),
            MixerMidiExtra::FxChannel => self.fx_channel.as_ref(),
            MixerMidiExtra::FxOn => self.fx_on.as_ref(),
            MixerMidiExtra::FxDepth => self.fx_depth.as_ref(),
            MixerMidiExtra::FxLow => self.fx_low.as_ref(),
            MixerMidiExtra::FxMid => self.fx_mid.as_ref(),
            MixerMidiExtra::FxHigh => self.fx_high.as_ref(),
        }
    }

    pub fn set_extra_binding(
        &mut self,
        channel: usize,
        extra: MixerMidiExtra,
        binding: Option<MidiBinding>,
    ) {
        match extra {
            MixerMidiExtra::CrossAssign if channel < 4 => self.cross_assign[channel] = binding,
            MixerMidiExtra::Crossfader => self.crossfader = binding,
            MixerMidiExtra::CueA if channel < 5 => self.cue_a[channel] = binding,
            MixerMidiExtra::CueB if channel < 5 => self.cue_b[channel] = binding,
            MixerMidiExtra::EqCurve => self.eq_curve = binding,
            MixerMidiExtra::MasterLevel => self.master_level = binding,
            MixerMidiExtra::CueALevel => self.cue_a_level = binding,
            MixerMidiExtra::CueBLevel => self.cue_b_level = binding,
            MixerMidiExtra::FxType => self.fx_type = binding,
            MixerMidiExtra::FxChannel => self.fx_channel = binding,
            MixerMidiExtra::FxOn => self.fx_on = binding,
            MixerMidiExtra::FxDepth => self.fx_depth = binding,
            MixerMidiExtra::FxLow => self.fx_low = binding,
            MixerMidiExtra::FxMid => self.fx_mid = binding,
            MixerMidiExtra::FxHigh => self.fx_high = binding,
            _ => {}
        }
    }
    pub fn binding(&self, channel: usize, parameter: MixerParameter) -> Option<&MidiBinding> {
        self.channels
            .get(channel)?
            .get(mixer_midi_index(parameter))?
            .as_ref()
    }

    pub fn set_binding(
        &mut self,
        channel: usize,
        parameter: MixerParameter,
        binding: Option<MidiBinding>,
    ) {
        if let Some(slot) = self
            .channels
            .get_mut(channel)
            .and_then(|row| row.get_mut(mixer_midi_index(parameter)))
        {
            *slot = binding;
        }
    }

    fn load(map: &BTreeMap<String, String>) -> Self {
        let mut assignments = Self::default();
        for channel in 0..4 {
            for parameter in MixerParameter::ALL {
                let key = mixer_midi_key(channel, parameter);
                assignments.set_binding(channel, parameter, load_midi_binding(map, &key, None));
            }
            for extra in [
                MixerMidiExtra::CrossAssign,
                MixerMidiExtra::CueA,
                MixerMidiExtra::CueB,
            ] {
                assignments.set_extra_binding(
                    channel,
                    extra,
                    load_midi_binding(
                        map,
                        &format!("midi_mixer_ch{}_{}", channel + 1, extra.key()),
                        None,
                    ),
                );
            }
        }
        assignments.eq_curve = load_midi_binding(map, "midi_mixer_eq_curve", None).or_else(|| {
            (0..4).find_map(|channel| {
                load_midi_binding(map, &format!("midi_mixer_ch{}_eq_curve", channel + 1), None)
            })
        });
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
            MixerMidiExtra::FxDepth,
            MixerMidiExtra::FxLow,
            MixerMidiExtra::FxMid,
            MixerMidiExtra::FxHigh,
        ] {
            let legacy_depth = if extra == MixerMidiExtra::FxDepth {
                load_midi_binding(map, "midi_mixer_fx_frequency", None)
            } else {
                None
            };
            assignments.set_extra_binding(
                4,
                extra,
                load_midi_binding(map, &format!("midi_mixer_{}", extra.key()), legacy_depth),
            );
        }
        assignments
    }

    fn save(&self, map: &mut BTreeMap<String, String>) {
        map.insert("midi_mixer_shared_v1".into(), "1".into());
        for channel in 0..4 {
            for parameter in MixerParameter::ALL {
                save_midi_binding(
                    map,
                    &mixer_midi_key(channel, parameter),
                    self.binding(channel, parameter),
                );
            }
            for extra in [
                MixerMidiExtra::CrossAssign,
                MixerMidiExtra::CueA,
                MixerMidiExtra::CueB,
            ] {
                save_midi_binding(
                    map,
                    &format!("midi_mixer_ch{}_{}", channel + 1, extra.key()),
                    self.extra_binding(channel, extra),
                );
            }
        }
        save_midi_binding(map, "midi_mixer_eq_curve", self.eq_curve.as_ref());
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
            MixerMidiExtra::FxDepth,
            MixerMidiExtra::FxLow,
            MixerMidiExtra::FxMid,
            MixerMidiExtra::FxHigh,
        ] {
            save_midi_binding(
                map,
                &format!("midi_mixer_{}", extra.key()),
                self.extra_binding(4, extra),
            );
        }
    }
}

fn mixer_midi_index(parameter: MixerParameter) -> usize {
    match parameter {
        MixerParameter::Trim => 0,
        MixerParameter::High => 1,
        MixerParameter::Mid => 2,
        MixerParameter::Low => 3,
        MixerParameter::Filter => 4,
        MixerParameter::Fader => 5,
    }
}

fn mixer_midi_key(channel: usize, parameter: MixerParameter) -> String {
    let suffix = match parameter {
        MixerParameter::Trim => "trim",
        MixerParameter::High => "high",
        MixerParameter::Mid => "mid",
        MixerParameter::Low => "low",
        MixerParameter::Filter => "filter",
        MixerParameter::Fader => "fader",
    };
    format!("midi_mixer_ch{}_{}", channel + 1, suffix)
}

/// Load shared mixer MIDI bindings. On first use, import the six legacy
/// per-player mixer bindings for each channel without changing deck controls.
pub fn load_mixer_midi_assignments() -> MidiMixerAssignments {
    let path = app_path();
    let mut map = read_kv(&path);
    if map.get("midi_mixer_shared_v1").map(String::as_str) == Some("1") {
        return MidiMixerAssignments::load(&map);
    }
    let mut assignments = MidiMixerAssignments::default();
    for channel in 0..4 {
        let legacy = MidiAssignments::load(
            &read_kv(&instance_path(channel as u32 + 1)),
            channel as u32 + 1,
        );
        for (parameter, binding) in [
            (MixerParameter::Trim, legacy.mixer_trim),
            (MixerParameter::High, legacy.mixer_high),
            (MixerParameter::Mid, legacy.mixer_mid),
            (MixerParameter::Low, legacy.mixer_low),
            (MixerParameter::Filter, legacy.mixer_filter),
            (MixerParameter::Fader, legacy.mixer_fader),
        ] {
            assignments.set_binding(channel, parameter, binding);
        }
    }
    assignments.save(&mut map);
    if let Err(error) = write_kv(&path, &map) {
        eprintln!("cdj3k-emu-storage: failed to migrate mixer MIDI bindings: {error}");
    }
    assignments
}

pub fn save_mixer_midi_assignments(assignments: &MidiMixerAssignments) -> std::io::Result<()> {
    let path = app_path();
    let mut map = read_kv(&path);
    assignments.save(&mut map);
    write_kv(&path, &map)
}

/// Read a player's deck bindings without creating or rewriting its settings.
pub fn load_midi_assignments(instance_id: u32) -> MidiAssignments {
    MidiAssignments::load(&read_kv(&instance_path(instance_id)), instance_id)
}

/// Update only the MIDI keys in one player's settings file.
pub fn save_midi_assignments(
    instance_id: u32,
    assignments: &MidiAssignments,
) -> std::io::Result<()> {
    let path = instance_path(instance_id);
    let mut map = read_kv(&path);
    assignments.save(&mut map);
    write_kv(&path, &map)
}

/// Saved channel-strip values for one player. All values use the shared
/// mixer's normalized `0.0..=1.0` range; EQ and filter are neutral at `0.5`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixerChannelSettings {
    pub trim: f32,
    pub high: f32,
    pub mid: f32,
    pub low: f32,
    pub filter: f32,
    pub fader: f32,
    pub cross_assignment: Option<u32>,
    pub eq_isolator: bool,
}

impl Default for MixerChannelSettings {
    fn default() -> Self {
        Self {
            trim: 0.5,
            high: 0.5,
            mid: 0.5,
            low: 0.5,
            filter: 0.5,
            fader: 1.0,
            cross_assignment: None,
            eq_isolator: false,
        }
    }
}

impl MixerChannelSettings {
    fn load(map: &BTreeMap<String, String>) -> Self {
        let defaults = Self::default();
        let value = |key: &str, default: f32| {
            map.get(key)
                .and_then(|value| value.parse::<f32>().ok())
                .unwrap_or(default)
                .clamp(0.0, 1.0)
        };
        Self {
            trim: value("mixer_trim", defaults.trim),
            high: value("mixer_high", defaults.high),
            mid: value("mixer_mid", defaults.mid),
            low: value("mixer_low", defaults.low),
            filter: value("mixer_filter", defaults.filter),
            fader: value("mixer_fader", defaults.fader),
            cross_assignment: map
                .get("mixer_cross_assignment")
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|&value| value <= 2),
            eq_isolator: map
                .get("mixer_eq_isolator")
                .is_some_and(|value| value == "1"),
        }
    }

    fn save(self, map: &mut BTreeMap<String, String>) {
        for (key, value) in [
            ("mixer_trim", self.trim),
            ("mixer_high", self.high),
            ("mixer_mid", self.mid),
            ("mixer_low", self.low),
            ("mixer_filter", self.filter),
            ("mixer_fader", self.fader),
        ] {
            map.insert(key.to_owned(), value.clamp(0.0, 1.0).to_string());
        }
        if let Some(assignment) = self.cross_assignment {
            map.insert(
                "mixer_cross_assignment".to_owned(),
                assignment.min(2).to_string(),
            );
        }
        map.insert(
            "mixer_eq_isolator".to_owned(),
            u8::from(self.eq_isolator).to_string(),
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixerMasterSettings {
    pub level: f32,
    pub cue_a_level: f32,
    pub cue_b_level: f32,
    pub eq_isolator: bool,
    pub fx_type: u32,
    pub fx_channel: u32,
    pub fx_enabled: bool,
    pub fx_depth: f32,
    pub fx_bands: u32,
}

impl Default for MixerMasterSettings {
    fn default() -> Self {
        Self {
            level: 0.5,
            cue_a_level: 1.0,
            cue_b_level: 1.0,
            eq_isolator: false,
            fx_type: 0,
            fx_channel: 4,
            fx_enabled: false,
            fx_depth: 0.5,
            fx_bands: 0b111,
        }
    }
}

pub fn load_mixer_master_settings() -> MixerMasterSettings {
    let map = read_kv(&app_path());
    let mut settings = load_mixer_master_settings_from_map(&map);
    if !map.contains_key("mixer_eq_isolator") {
        settings.eq_isolator = read_kv(&instance_path(1))
            .get("mixer_eq_isolator")
            .is_some_and(|value| value == "1");
    }
    settings
}

fn load_mixer_master_settings_from_map(map: &BTreeMap<String, String>) -> MixerMasterSettings {
    let defaults = MixerMasterSettings::default();
    let level = map
        .get("mixer_master_level")
        .and_then(|v| v.parse::<f32>().ok());
    let old_curve = map.get("mixer_master_curve_v2").map(String::as_str) != Some("1");
    let fx_type = map.get("mixer_fx_type").and_then(|v| v.parse::<u32>().ok());
    let old_fx_selector = map.get("mixer_fx_selector_v2").map(String::as_str) != Some("1");
    MixerMasterSettings {
        level: level
            .map(|value| if old_curve { value * 0.5 } else { value })
            .unwrap_or(defaults.level)
            .clamp(0.0, 1.0),
        cue_a_level: map
            .get("mixer_cue_a_level")
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(defaults.cue_a_level)
            .clamp(0.0, 1.0),
        cue_b_level: map
            .get("mixer_cue_b_level")
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(defaults.cue_b_level)
            .clamp(0.0, 1.0),
        eq_isolator: map.get("mixer_eq_isolator").is_some_and(|v| v == "1"),
        fx_type: fx_type
            .map(|value| {
                if old_fx_selector {
                    value.saturating_sub(1)
                } else {
                    value
                }
            })
            .unwrap_or(defaults.fx_type)
            .min(3),
        fx_channel: map
            .get("mixer_fx_channel")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(defaults.fx_channel)
            .min(4),
        fx_enabled: map.get("mixer_fx_enabled").is_some_and(|v| v == "1"),
        fx_depth: map
            .get("mixer_fx_depth")
            .or_else(|| map.get("mixer_fx_frequency"))
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(defaults.fx_depth)
            .clamp(0.0, 1.0),
        fx_bands: map
            .get("mixer_fx_bands")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(defaults.fx_bands)
            & 0b111,
    }
}

pub fn save_mixer_master_settings(settings: MixerMasterSettings) -> std::io::Result<()> {
    let path = app_path();
    let mut map = read_kv(&path);
    map.insert(
        "mixer_master_level".into(),
        settings.level.clamp(0.0, 1.0).to_string(),
    );
    map.insert("mixer_master_curve_v2".into(), "1".into());
    map.insert(
        "mixer_cue_a_level".into(),
        settings.cue_a_level.clamp(0.0, 1.0).to_string(),
    );
    map.insert(
        "mixer_cue_b_level".into(),
        settings.cue_b_level.clamp(0.0, 1.0).to_string(),
    );
    map.insert(
        "mixer_eq_isolator".into(),
        u8::from(settings.eq_isolator).to_string(),
    );
    map.insert("mixer_fx_type".into(), settings.fx_type.min(3).to_string());
    map.insert("mixer_fx_selector_v2".into(), "1".into());
    map.insert(
        "mixer_fx_channel".into(),
        settings.fx_channel.min(4).to_string(),
    );
    map.insert(
        "mixer_fx_enabled".into(),
        u8::from(settings.fx_enabled).to_string(),
    );
    map.insert(
        "mixer_fx_depth".into(),
        settings.fx_depth.clamp(0.0, 1.0).to_string(),
    );
    map.insert(
        "mixer_fx_bands".into(),
        (settings.fx_bands & 0b111).to_string(),
    );
    write_kv(&path, &map)
}

fn validated_profile_name(name: &str) -> std::io::Result<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.contains(['\n', '\r']) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "profile name must be 1-64 characters on one line",
        ));
    }
    Ok(name)
}

fn midi_profile_path(instance_id: u32, name: &str) -> std::io::Result<PathBuf> {
    let name = validated_profile_name(name)?;
    Ok(midi_profiles_dir(instance_id).join(format!("{}.txt", hex_encode(name))))
}

fn mixer_midi_profile_path(name: &str) -> std::io::Result<PathBuf> {
    let name = validated_profile_name(name)?;
    Ok(mixer_midi_profiles_dir().join(format!("{}.txt", hex_encode(name))))
}

fn all_midi_profile_path(name: &str) -> std::io::Result<PathBuf> {
    let name = validated_profile_name(name)?;
    Ok(all_midi_profiles_dir().join(format!("{}.txt", hex_encode(name))))
}

/// A single MIDI profile covers every player slot and the shared mixer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiProfile {
    pub slots: [MidiAssignments; 4],
    pub mixer: MidiMixerAssignments,
}

impl MidiProfile {
    fn current() -> Self {
        Self {
            slots: std::array::from_fn(|slot| load_midi_assignments(slot as u32 + 1)),
            mixer: load_mixer_midi_assignments(),
        }
    }
}

fn legacy_profile_name(name: &str) -> &str {
    if name.eq_ignore_ascii_case("flx4-1") || name.eq_ignore_ascii_case("flx4-2") {
        "flx4"
    } else {
        name
    }
}

/// Return unified profiles and importable legacy profiles. The two deck-specific
/// FLX4 profiles appear as one entry, while their original files stay untouched.
pub fn list_all_midi_profiles() -> std::io::Result<Vec<String>> {
    let mut names = list_profiles_in(all_midi_profiles_dir())?;
    for slot in 1..=4 {
        for name in list_midi_profiles(slot)? {
            let display = legacy_profile_name(&name).to_owned();
            if !names
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&display))
            {
                names.push(display);
            }
        }
    }
    for name in list_mixer_midi_profiles()? {
        if !names
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&name))
        {
            names.push(name);
        }
    }
    names.sort_by_key(|name| name.to_lowercase());
    Ok(names)
}

pub fn save_all_midi_profile(name: &str, profile: &MidiProfile) -> std::io::Result<()> {
    let path = all_midi_profile_path(name)?;
    write_kv(&path, &encode_all_midi_profile(profile))
}

fn encode_all_midi_profile(profile: &MidiProfile) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    map.insert("profile_version".to_owned(), "1".to_owned());
    for (slot, assignments) in profile.slots.iter().enumerate() {
        let mut part = BTreeMap::new();
        assignments.save(&mut part);
        for (key, value) in part {
            map.insert(format!("slot{}_{}", slot + 1, key), value);
        }
    }
    let mut part = BTreeMap::new();
    profile.mixer.save(&mut part);
    for (key, value) in part {
        map.insert(format!("mixer_{key}"), value);
    }
    map
}

pub fn load_all_midi_profile(name: &str) -> std::io::Result<MidiProfile> {
    let path = all_midi_profile_path(name)?;
    if path.exists() {
        return decode_all_midi_profile(&read_kv(&path));
    }

    // Import a legacy per-tab profile over the current assignments for tabs
    // that had no matching legacy profile. No old profile is overwritten.
    let mut profile = MidiProfile::current();
    let mut found = false;
    for slot in 1..=4 {
        let legacy_names = list_midi_profiles(slot)?;
        let match_name = if name.eq_ignore_ascii_case("FLX4") {
            legacy_names
                .iter()
                .find(|candidate| candidate.eq_ignore_ascii_case(&format!("flx4-{slot}")))
        } else {
            legacy_names
                .iter()
                .find(|candidate| candidate.eq_ignore_ascii_case(name))
        };
        if let Some(legacy_name) = match_name {
            profile.slots[(slot - 1) as usize] = load_midi_profile(slot, legacy_name)?;
            found = true;
        }
    }
    if let Some(legacy_name) = list_mixer_midi_profiles()?
        .into_iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(name))
    {
        profile.mixer = load_mixer_midi_profile(&legacy_name)?;
        found = true;
    }
    if !found {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "MIDI profile not found",
        ));
    }
    Ok(profile)
}

fn decode_all_midi_profile(map: &BTreeMap<String, String>) -> std::io::Result<MidiProfile> {
    if map.get("profile_version").map(String::as_str) != Some("1") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported MIDI profile format",
        ));
    }
    let slots = std::array::from_fn(|slot| {
        let prefix = format!("slot{}_", slot + 1);
        let part = map
            .iter()
            .filter_map(|(key, value)| {
                key.strip_prefix(&prefix)
                    .map(|key| (key.to_owned(), value.clone()))
            })
            .collect();
        MidiAssignments::load(&part, slot as u32 + 1)
    });
    let mixer = map
        .iter()
        .filter_map(|(key, value)| {
            key.strip_prefix("mixer_")
                .map(|key| (key.to_owned(), value.clone()))
        })
        .collect();
    Ok(MidiProfile {
        slots,
        mixer: MidiMixerAssignments::load(&mixer),
    })
}

pub fn delete_all_midi_profile(name: &str) -> std::io::Result<()> {
    let path = all_midi_profile_path(name)?;
    let mut paths = Vec::new();
    if path.exists() {
        paths.push(path);
    }
    for slot in 1..=4 {
        for legacy_name in list_midi_profiles(slot)? {
            if legacy_profile_name(&legacy_name).eq_ignore_ascii_case(name) {
                paths.push(midi_profile_path(slot, &legacy_name)?);
            }
        }
    }
    for legacy_name in list_mixer_midi_profiles()? {
        if legacy_name.eq_ignore_ascii_case(name) {
            paths.push(mixer_midi_profile_path(&legacy_name)?);
        }
    }
    if paths.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "MIDI profile not found",
        ));
    }
    for path in paths {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Return the saved MIDI profile names for one player, sorted for display.
pub fn list_midi_profiles(instance_id: u32) -> std::io::Result<Vec<String>> {
    list_profiles_in(midi_profiles_dir(instance_id))
}

pub fn list_mixer_midi_profiles() -> std::io::Result<Vec<String>> {
    list_profiles_in(mixer_midi_profiles_dir())
}

fn list_profiles_in(dir: PathBuf) -> std::io::Result<Vec<String>> {
    let mut profiles = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(profiles),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("txt") {
            continue;
        }
        if let Some(name) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(hex_decode)
        {
            profiles.push(name);
        }
    }
    profiles.sort_by_key(|name| name.to_lowercase());
    profiles.dedup();
    Ok(profiles)
}

pub fn save_mixer_midi_profile(
    name: &str,
    assignments: &MidiMixerAssignments,
) -> std::io::Result<()> {
    let path = mixer_midi_profile_path(name)?;
    let mut map = BTreeMap::new();
    map.insert(
        "profile_name".to_owned(),
        hex_encode(validated_profile_name(name)?),
    );
    assignments.save(&mut map);
    write_kv(&path, &map)
}

pub fn load_mixer_midi_profile(name: &str) -> std::io::Result<MidiMixerAssignments> {
    let path = mixer_midi_profile_path(name)?;
    let map = read_kv(&path);
    if map.is_empty() && !path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "MIDI profile not found",
        ));
    }
    Ok(MidiMixerAssignments::load(&map))
}

pub fn delete_mixer_midi_profile(name: &str) -> std::io::Result<()> {
    let path = mixer_midi_profile_path(name)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Save (or overwrite) a named MIDI assignment profile for one player.
pub fn save_midi_profile(
    instance_id: u32,
    name: &str,
    assignments: &MidiAssignments,
) -> std::io::Result<()> {
    let path = midi_profile_path(instance_id, name)?;
    let mut map = BTreeMap::new();
    map.insert(
        "profile_name".to_owned(),
        hex_encode(validated_profile_name(name)?),
    );
    assignments.save(&mut map);
    write_kv(&path, &map)
}

/// Load a named MIDI assignment profile for one player.
pub fn load_midi_profile(instance_id: u32, name: &str) -> std::io::Result<MidiAssignments> {
    let path = midi_profile_path(instance_id, name)?;
    let map = read_kv(&path);
    if map.is_empty() && !path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "MIDI profile not found",
        ));
    }
    Ok(MidiAssignments::load(&map, instance_id))
}

/// Delete a named MIDI assignment profile for one player.
pub fn delete_midi_profile(instance_id: u32, name: &str) -> std::io::Result<()> {
    let path = midi_profile_path(instance_id, name)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn load_midi_binding(
    map: &BTreeMap<String, String>,
    key: &str,
    default: Option<MidiBinding>,
) -> Option<MidiBinding> {
    match map.get(key) {
        None => default,
        Some(value) if value.is_empty() => None,
        Some(value) => decode_midi_binding(value).or(default),
    }
}

fn save_midi_binding(map: &mut BTreeMap<String, String>, key: &str, binding: Option<&MidiBinding>) {
    map.insert(
        key.to_owned(),
        binding.map(encode_midi_binding).unwrap_or_default(),
    );
}

fn encode_midi_binding(binding: &MidiBinding) -> String {
    let kind = match binding.kind {
        MidiMessageKind::Note => "note",
        MidiMessageKind::ControlChange => "cc",
    };
    let port = binding
        .port_name
        .as_deref()
        .map(hex_encode)
        .unwrap_or_else(|| "*".to_owned());
    format!("{kind},{},{},{port}", binding.channel, binding.number)
}

fn decode_midi_binding(value: &str) -> Option<MidiBinding> {
    let mut fields = value.splitn(4, ',');
    let kind = match fields.next()? {
        "note" => MidiMessageKind::Note,
        "cc" => MidiMessageKind::ControlChange,
        _ => return None,
    };
    let channel = fields.next()?.parse::<u8>().ok()?;
    let number = fields.next()?.parse::<u8>().ok()?;
    if channel > 15 || number > 127 {
        return None;
    }
    let encoded_port = fields.next()?;
    let port_name = if encoded_port == "*" {
        None
    } else {
        Some(hex_decode(encoded_port)?)
    };
    Some(MidiBinding {
        port_name,
        channel,
        kind,
        number,
    })
}

fn hex_encode(value: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn hex_decode(value: &str) -> Option<String> {
    if value.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        bytes.push(((hi << 4) | lo) as u8);
    }
    String::from_utf8(bytes).ok()
}

#[derive(Debug, Clone)]
pub struct InstanceSettings {
    /// Locally-administered MAC, e.g. "0a:11:22:33:44:55". Stable across launches.
    pub mac: String,
    /// Whether QEMU is launched with the audio backend.  When true virtio-sound
    /// is added and routed to CoreAudio; when false no ALSA card is present.
    pub audio_enabled: bool,
    /// Per-player host/guest audio buffering profile.
    pub audio_latency: AudioLatencySettings,
    /// "Enable ALC" (audio-latency compensation) toggle. Mirrors the guest's
    /// `audio_sync_enabled` sysfs param. Defaults to **true** for new
    /// instances - the sync compensation is the better experience for the
    /// vast majority of users; opting out is a power-user choice.
    /// The runtime worker pushes this value to the guest cfg daemon once
    /// per QEMU boot so the kernel module's default-off doesn't override us.
    pub alc_enabled: bool,
    /// "Trackpad Haptics" toggle.  Gates the Force Touch detent clicks emitted
    /// by `cdj3k_emu_platform::haptic::actuate` as the jog crosses detents.
    /// Defaults to **true**; users on hardware without an actuator see no
    /// change either way (the platform layer no-ops silently).
    pub haptic_enabled: bool,
    /// "EP122 Mods" toggle.  When false the emulator puts `ep122_no_mods` on
    /// the kernel cmdline and the guest exports `EP122_NO_MODS=1` into
    /// EP122's environment, so the cdj3k-mods linked into ep122_shim.so stay
    /// out of the process (the shim's own emulation plumbing still loads).
    /// Defaults to **false**: a fresh instance boots a stock EP122 and the
    /// user opts in from the menu.  Applied at the next QEMU (re)start.
    pub mods_enabled: bool,
    /// Last user-selected network interface name (e.g. "en0"), or `None` for
    /// "no network".  Restored on launch if the iface is still present;
    /// otherwise kept on disk so it can re-bind when the iface returns.
    pub net_iface: Option<String>,
    /// Last user-selected virtual USB image path, or `None`.  Restored on
    /// launch if the file still exists; kept on disk regardless.
    pub usb_virtual_path: Option<PathBuf>,
    /// Last user-selected physical USB disk BSD name (e.g. "disk2"), or
    /// `None`.  Restored on launch if the disk is still present; kept on disk
    /// regardless.
    pub usb_physical_bsd: Option<String>,
    /// MIDI assignments owned by this player/instance.
    pub midi_assignments: MidiAssignments,
    /// Virtual mixer channel strip owned by this player.
    pub mixer: MixerChannelSettings,
}

impl InstanceSettings {
    /// Load from disk, or generate + persist a fresh MAC if no file/key exists.
    pub fn load_or_init(instance_id: u32) -> Self {
        let path = instance_path(instance_id);
        let mut map = read_kv(&path);
        let mac = match map.get("mac") {
            Some(m) if is_valid_mac(m) => m.clone(),
            _ => {
                let m = generate_mac();
                map.insert("mac".into(), m.clone());
                if let Err(e) = write_kv(&path, &map) {
                    // Disk write failed — the generated MAC won't survive a
                    // restart, but we'd rather proceed with a one-shot MAC
                    // than refuse to launch the slot.  Log so the user sees
                    // it in Console.app instead of getting a silent MAC churn.
                    eprintln!(
                        "cdj3k-emu-storage: failed to persist MAC for instance {}: {}",
                        instance_id, e
                    );
                }
                m
            }
        };
        let audio_enabled = map
            .get("audio_enabled")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let audio_latency = load_audio_latency(&map);
        // ALC default: ON. New users get the sync compensation by default;
        // opt-out is a power-user choice that the toggle persists.
        let alc_enabled = map
            .get("alc_enabled")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true);
        // Trackpad haptics default: ON.  See `haptic_enabled` doc-comment.
        let haptic_enabled = map
            .get("haptic_enabled")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true);
        // Mods default: OFF - opt-in from Emulation > EP122 Mods.
        let mods_enabled = map
            .get("mods_enabled")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let net_iface = map.get("net_iface").filter(|v| !v.is_empty()).cloned();
        let usb_virtual_path = map
            .get("usb_virtual_path")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let usb_physical_bsd = map
            .get("usb_physical_bsd")
            .filter(|v| !v.is_empty())
            .cloned();
        let midi_assignments = MidiAssignments::load(&map, instance_id);
        let mixer = MixerChannelSettings::load(&map);
        Self {
            mac,
            audio_enabled,
            audio_latency,
            alc_enabled,
            haptic_enabled,
            mods_enabled,
            net_iface,
            usb_virtual_path,
            usb_physical_bsd,
            midi_assignments,
            mixer,
        }
    }

    pub fn save(&self, instance_id: u32) -> std::io::Result<()> {
        let path = instance_path(instance_id);
        let mut map = read_kv(&path);
        map.insert("mac".into(), self.mac.clone());
        map.insert(
            "audio_enabled".into(),
            (if self.audio_enabled { "1" } else { "0" }).to_string(),
        );
        // Output routing belongs to the application-wide native mixer. Drop
        // the obsolete per-player key if an older settings file still has it.
        map.remove("audio_device_uid");
        save_audio_latency(&mut map, self.audio_latency);
        map.insert(
            "alc_enabled".into(),
            (if self.alc_enabled { "1" } else { "0" }).to_string(),
        );
        map.insert(
            "haptic_enabled".into(),
            (if self.haptic_enabled { "1" } else { "0" }).to_string(),
        );
        map.insert(
            "mods_enabled".into(),
            (if self.mods_enabled { "1" } else { "0" }).to_string(),
        );
        map.insert(
            "net_iface".into(),
            self.net_iface.clone().unwrap_or_default(),
        );
        map.insert(
            "usb_virtual_path".into(),
            self.usb_virtual_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        map.insert(
            "usb_physical_bsd".into(),
            self.usb_physical_bsd.clone().unwrap_or_default(),
        );
        // MIDI Learn can edit another running slot. Ordinary settings saves
        // must preserve the newest bindings on disk instead of restoring this
        // process's potentially stale in-memory copy.
        if !map.contains_key("midi_play") {
            self.midi_assignments.save(&mut map);
        }
        self.mixer.save(&mut map);
        write_kv(&path, &map)
    }
}

fn load_audio_latency(map: &BTreeMap<String, String>) -> AudioLatencySettings {
    let defaults = AudioLatencySettings::default();
    let profile = match map.get("audio_latency_profile").map(String::as_str) {
        Some("safe") => AudioLatencyProfile::Safe,
        Some("low_latency") => AudioLatencyProfile::LowLatency,
        Some("custom") => AudioLatencyProfile::Custom,
        _ => AudioLatencyProfile::Balanced,
    };
    let read_u32 = |key: &str, fallback: u32| {
        map.get(key)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(fallback)
    };
    let custom = AudioLatencySettings::sanitize_custom(AudioLatencyTuning {
        startup_prefill_frames: read_u32(
            "audio_custom_prefill_frames",
            defaults.custom.startup_prefill_frames,
        ),
        guest_transfer_frames: read_u32(
            "audio_custom_transfer_frames",
            defaults.custom.guest_transfer_frames,
        ),
        coreaudio_buffer_us: read_u32(
            "audio_custom_coreaudio_us",
            defaults.custom.coreaudio_buffer_us,
        ),
    });
    AudioLatencySettings { profile, custom }
}

fn save_audio_latency(map: &mut BTreeMap<String, String>, settings: AudioLatencySettings) {
    let profile = match settings.profile {
        AudioLatencyProfile::Safe => "safe",
        AudioLatencyProfile::Balanced => "balanced",
        AudioLatencyProfile::LowLatency => "low_latency",
        AudioLatencyProfile::Custom => "custom",
    };
    let custom = AudioLatencySettings::sanitize_custom(settings.custom);
    map.insert("audio_latency_profile".into(), profile.into());
    map.insert(
        "audio_custom_prefill_frames".into(),
        custom.startup_prefill_frames.to_string(),
    );
    map.insert(
        "audio_custom_transfer_frames".into(),
        custom.guest_transfer_frames.to_string(),
    );
    map.insert(
        "audio_custom_coreaudio_us".into(),
        custom.coreaudio_buffer_us.to_string(),
    );
}

fn is_valid_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && u8::from_str_radix(p, 16).is_ok())
}

/// Generate a random locally-administered unicast MAC.
/// Uses `uuid::Uuid::new_v4()` (already a dep) as the entropy source - its bytes
/// are cryptographically random on macOS/Linux. The first byte is forced to
/// `02` (LAA bit set, multicast bit clear) so it's a valid host address.
fn generate_mac() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let m0 = (bytes[0] & 0xfe) | 0x02; // LAA, unicast
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m0, bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_controls_migrate_to_centered_level_and_global_eq() {
        let mut map = BTreeMap::new();
        map.insert("mixer_master_level".into(), "1".into());
        map.insert("mixer_fx_type".into(), "3".into());
        map.insert("mixer_eq_isolator".into(), "1".into());
        let settings = load_mixer_master_settings_from_map(&map);
        assert_eq!(settings.level, 0.5);
        assert_eq!(settings.fx_type, 2);
        assert!(settings.eq_isolator);
        assert_eq!(settings.fx_bands, 0b111);
        assert_eq!(settings.cue_a_level, 1.0);
        assert_eq!(settings.cue_b_level, 1.0);
        map.insert("mixer_cue_a_level".into(), "0.25".into());
        map.insert("mixer_cue_b_level".into(), "0.75".into());
        let settings = load_mixer_master_settings_from_map(&map);
        assert_eq!(settings.cue_a_level, 0.25);
        assert_eq!(settings.cue_b_level, 0.75);
    }

    #[test]
    fn global_eq_and_fx_band_midi_assignments_round_trip() {
        let mut assignments = MidiMixerAssignments::default();
        let binding = MidiBinding {
            port_name: Some("DJ Mixer".into()),
            channel: 0,
            kind: MidiMessageKind::Note,
            number: 42,
        };
        assignments.set_extra_binding(4, MixerMidiExtra::EqCurve, Some(binding.clone()));
        assignments.set_extra_binding(4, MixerMidiExtra::FxLow, Some(binding.clone()));
        assignments.set_extra_binding(4, MixerMidiExtra::CueALevel, Some(binding.clone()));
        assignments.set_extra_binding(4, MixerMidiExtra::CueBLevel, Some(binding.clone()));
        let mut map = BTreeMap::new();
        assignments.save(&mut map);
        let loaded = MidiMixerAssignments::load(&map);
        assert_eq!(
            loaded.extra_binding(4, MixerMidiExtra::EqCurve),
            Some(&binding)
        );
        assert_eq!(
            loaded.extra_binding(4, MixerMidiExtra::FxLow),
            Some(&binding)
        );
        assert_eq!(
            loaded.extra_binding(4, MixerMidiExtra::CueALevel),
            Some(&binding)
        );
        assert_eq!(
            loaded.extra_binding(4, MixerMidiExtra::CueBLevel),
            Some(&binding)
        );
    }

    #[test]
    fn audio_latency_custom_profile_round_trips_and_sanitizes() {
        let mut map = BTreeMap::new();
        save_audio_latency(
            &mut map,
            AudioLatencySettings {
                profile: AudioLatencyProfile::Custom,
                custom: AudioLatencyTuning {
                    startup_prefill_frames: 900,
                    guest_transfer_frames: 700,
                    coreaudio_buffer_us: 1_000,
                },
            },
        );
        let loaded = load_audio_latency(&map);
        assert_eq!(loaded.profile, AudioLatencyProfile::Custom);
        assert_eq!(loaded.custom.guest_transfer_frames, 640);
        assert_eq!(loaded.custom.startup_prefill_frames, 1280);
        assert_eq!(loaded.custom.coreaudio_buffer_us, 3_000);
    }

    #[test]
    fn audio_latency_defaults_to_balanced() {
        assert_eq!(
            load_audio_latency(&BTreeMap::new()),
            AudioLatencySettings::default()
        );
    }

    #[test]
    fn midi_defaults_use_the_players_channel() {
        let player_three = MidiAssignments::for_instance(3);
        assert_eq!(player_three.play.as_ref().unwrap().channel, 2);
        assert_eq!(player_three.play.as_ref().unwrap().number, 0);
        assert_eq!(player_three.hot_cues[7].as_ref().unwrap().number, 0x22);
        assert_eq!(
            player_three.tempo.as_ref().unwrap().kind,
            MidiMessageKind::ControlChange
        );
        assert_eq!(player_three.call_delete.as_ref().unwrap().number, 0x1a);
        assert_eq!(player_three.reloop_exit.as_ref().unwrap().number, 0x25);
        assert_eq!(player_three.beat_sync.as_ref().unwrap().number, 0x16);
        assert_eq!(player_three.jog_platter.as_ref().unwrap().number, 0x10);
        assert_eq!(player_three.jog_ring.as_ref().unwrap().number, 0x30);
        assert_eq!(player_three.jog_touch.as_ref().unwrap().number, 0x17);
        assert_eq!(player_three.jog_platter_mode, MidiJogMode::SpeedCentered);
        assert!(player_three.mixer_trim.is_none());
        assert!(player_three.mixer_fader.is_none());
        let search_next = MIDI_EXTRA_BUTTON_KEYS
            .iter()
            .position(|key| *key == "search_next")
            .unwrap();
        let key_sync = MIDI_EXTRA_BUTTON_KEYS
            .iter()
            .position(|key| *key == "key_sync")
            .unwrap();
        assert_eq!(
            player_three.extra_buttons[search_next]
                .as_ref()
                .unwrap()
                .number,
            0x02
        );
        assert_eq!(
            player_three.extra_buttons[key_sync]
                .as_ref()
                .unwrap()
                .number,
            0x48
        );
    }

    #[test]
    fn midi_new_controls_and_jog_modes_round_trip() {
        let mut assignments = MidiAssignments::for_instance(2);
        assignments.loop_out = None;
        assignments.jog_platter_mode = MidiJogMode::RelativeTwosComplement;
        assignments.jog_ring_mode = MidiJogMode::RelativeOffset64;
        assignments.rotary_turn = Some(MidiBinding {
            port_name: Some("Encoder".to_owned()),
            channel: 1,
            kind: MidiMessageKind::ControlChange,
            number: 42,
        });
        assignments.rotary_turn_mode = MidiJogMode::RelativeTwosComplement;
        assignments.activate_deck = Some(MidiBinding {
            port_name: Some("Deck selector".to_owned()),
            channel: 1,
            kind: MidiMessageKind::Note,
            number: 72,
        });
        assignments.jog_platter_steps_per_rev = 360;
        assignments.jog_ring_steps_per_rev = 96;
        let mut map = BTreeMap::new();
        assignments.save(&mut map);
        assert_eq!(MidiAssignments::load(&map, 2), assignments);
        map.insert("midi_jog_platter_steps_per_rev".into(), "0".into());
        assert_eq!(MidiAssignments::load(&map, 2).jog_platter_steps_per_rev, 1);
    }

    #[test]
    fn unified_midi_profile_round_trips_all_tabs() {
        let mut profile = MidiProfile {
            slots: std::array::from_fn(|slot| MidiAssignments::for_instance(slot as u32 + 1)),
            mixer: MidiMixerAssignments::default(),
        };
        profile.slots[0].beat_sync = None;
        profile.slots[1].jog_platter_steps_per_rev = 360;
        profile.slots[2].loop_in = None;
        profile.slots[3].jog_ring_steps_per_rev = 96;
        profile.mixer.channels[0][0] = profile.slots[0].play.clone();
        assert_eq!(
            decode_all_midi_profile(&encode_all_midi_profile(&profile)).unwrap(),
            profile
        );
        assert_eq!(legacy_profile_name("FLX4-1"), "flx4");
        assert_eq!(legacy_profile_name("FLX4-2"), "flx4");
    }

    #[test]
    fn midi_binding_round_trips_unicode_port_names() {
        let binding = MidiBinding {
            port_name: Some("DJ Controller α".to_owned()),
            channel: 9,
            kind: MidiMessageKind::ControlChange,
            number: 74,
        };
        assert_eq!(
            decode_midi_binding(&encode_midi_binding(&binding)),
            Some(binding)
        );
    }

    #[test]
    fn empty_midi_setting_disables_a_control() {
        let mut map = BTreeMap::new();
        map.insert("midi_play".to_owned(), String::new());
        let assignments = MidiAssignments::load(&map, 1);
        assert_eq!(assignments.play, None);
        assert!(assignments.cue.is_some());
    }

    #[test]
    fn profile_names_are_encoded_as_safe_single_components() {
        let path = midi_profile_path(2, "Club / Controller α").unwrap();
        assert_eq!(path.parent(), Some(midi_profiles_dir(2).as_path()));
        assert!(!path.file_name().unwrap().to_string_lossy().contains('/'));
        assert!(midi_profile_path(1, "\n").is_err());
        assert!(midi_profile_path(1, "").is_err());
        let mixer_path = mixer_midi_profile_path("Four channels / α").unwrap();
        assert_eq!(
            mixer_path.parent(),
            Some(mixer_midi_profiles_dir().as_path())
        );
        assert!(mixer_midi_profile_path("\n").is_err());
    }

    #[test]
    fn mixer_midi_bindings_round_trip_for_all_channels() {
        let mut assignments = MidiMixerAssignments::default();
        for channel in 0..4 {
            for parameter in MixerParameter::ALL {
                assignments.set_binding(
                    channel,
                    parameter,
                    Some(MidiBinding {
                        port_name: Some(format!("Controller {channel}")),
                        channel: channel as u8,
                        kind: MidiMessageKind::ControlChange,
                        number: (channel * 6 + mixer_midi_index(parameter)) as u8,
                    }),
                );
            }
            assignments.set_extra_binding(
                channel,
                MixerMidiExtra::CrossAssign,
                Some(MidiBinding {
                    port_name: Some("DJ Mixer".into()),
                    channel: channel as u8,
                    kind: MidiMessageKind::ControlChange,
                    number: (80 + channel) as u8,
                }),
            );
            for (extra, number) in [
                (MixerMidiExtra::CueA, 40 + channel as u8),
                (MixerMidiExtra::CueB, 50 + channel as u8),
            ] {
                assignments.set_extra_binding(
                    channel,
                    extra,
                    Some(MidiBinding {
                        port_name: Some("DJ Mixer".into()),
                        channel: channel as u8,
                        kind: MidiMessageKind::Note,
                        number,
                    }),
                );
            }
        }
        assignments.set_extra_binding(
            4,
            MixerMidiExtra::CueA,
            Some(MidiBinding {
                port_name: Some("DJ Mixer".into()),
                channel: 0,
                kind: MidiMessageKind::Note,
                number: 60,
            }),
        );
        assignments.set_extra_binding(
            4,
            MixerMidiExtra::CueB,
            Some(MidiBinding {
                port_name: Some("DJ Mixer".into()),
                channel: 0,
                kind: MidiMessageKind::Note,
                number: 61,
            }),
        );
        assignments.set_extra_binding(
            4,
            MixerMidiExtra::MasterLevel,
            Some(MidiBinding {
                port_name: Some("DJ Mixer".into()),
                channel: 0,
                kind: MidiMessageKind::ControlChange,
                number: 90,
            }),
        );
        assignments.set_extra_binding(
            4,
            MixerMidiExtra::Crossfader,
            Some(MidiBinding {
                port_name: Some("DJ Mixer".into()),
                channel: 0,
                kind: MidiMessageKind::ControlChange,
                number: 91,
            }),
        );
        let mut map = BTreeMap::new();
        assignments.save(&mut map);
        assert_eq!(MidiMixerAssignments::load(&map), assignments);
    }
}
