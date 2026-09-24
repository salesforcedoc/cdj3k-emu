pub mod emmc;
pub mod gpt;
pub mod settings;

pub use emmc::{default_path, provision_emmc, EmmcConfig, FirmwareInfo};
pub use settings::{
    delete_all_midi_profile, delete_midi_profile, delete_mixer_midi_profile,
    list_all_midi_profiles, list_midi_profiles, list_mixer_midi_profiles, load_all_midi_profile,
    load_audio_device_uid, load_cue_device_uid, load_midi_assignments, load_midi_profile,
    load_mixer_master_settings, load_mixer_midi_assignments, load_mixer_midi_profile,
    save_all_midi_profile, save_audio_device_uid, save_cue_device_uid, save_midi_assignments,
    save_midi_profile, save_mixer_master_settings, save_mixer_midi_assignments,
    save_mixer_midi_profile, AppSettings, InstanceSettings, MidiAssignments, MidiBinding,
    MidiJogMode, MidiMessageKind, MidiMixerAssignments, MidiProfile, MixerChannelSettings,
    MixerMasterSettings, MixerMidiExtra, MIDI_EXTRA_BUTTON_KEYS,
};

use std::path::PathBuf;

/// `~/Library/Application Support/<BUNDLE_ID>/` on macOS,
/// `$XDG_DATA_HOME/<BUNDLE_ID>/` (fallback: `~/.local/share/<BUNDLE_ID>/`)
/// elsewhere. The directory name is the macOS bundle identifier, matching
/// the OS convention (reverse-DNS, never the human display name).
pub fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Library")
        .join("Application Support");
    #[cfg(not(target_os = "macos"))]
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share")
        });
    base.join(cdj3k_emu_platform::app_meta::BUNDLE_ID)
}
