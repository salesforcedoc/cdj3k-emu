//! Host keyboard forwarding for the firmware's focused text editor.
//!
//! QEMU exposes a USB HID keyboard to the guest. The firmware's software
//! keyboard gives its JUCE TextEditor X11 keyboard focus while visible, so
//! these ordinary key presses reach that editor without firmware-specific
//! hooks or framebuffer coordinate matching.

use cdj3k_emu_platform::menu_state::{self, GuestKeyboardInput, MAX_GUEST_KEYBOARD_QUEUE};

use super::CdjApp;

impl CdjApp {
    pub(super) fn forward_host_keyboard(&mut self, ctx: &egui::Context) {
        let pending = ctx.input(|input| {
            if !input.focused {
                return Vec::new();
            }

            let mut pending = Vec::new();
            for event in &input.events {
                match event {
                    egui::Event::Text(text) | egui::Event::Paste(text) => {
                        pending.extend(text.chars().map(GuestKeyboardInput::Text));
                    }
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } if !modifiers.command && !modifiers.ctrl && !modifiers.alt => {
                        if let Some(key) = editing_key(*key) {
                            pending.push(key);
                        }
                    }
                    _ => {}
                }
            }
            pending
        });

        if pending.is_empty() {
            return;
        }

        let mut state = menu_state::lock();
        let room = MAX_GUEST_KEYBOARD_QUEUE.saturating_sub(state.guest_keyboard_input.len());
        state
            .guest_keyboard_input
            .extend(pending.into_iter().take(room));
    }
}

fn editing_key(key: egui::Key) -> Option<GuestKeyboardInput> {
    use egui::Key;
    use GuestKeyboardInput::*;

    Some(match key {
        Key::Backspace => Backspace,
        Key::Delete => Delete,
        Key::Enter => Enter,
        Key::Escape => Escape,
        Key::Tab => Tab,
        Key::ArrowLeft => Left,
        Key::ArrowRight => Right,
        Key::ArrowUp => Up,
        Key::ArrowDown => Down,
        Key::Home => Home,
        Key::End => End,
        // Printable keys arrive as Event::Text. Handling them here as well
        // would type every character twice.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_keys_are_forwarded_but_printable_keys_are_not_duplicated() {
        assert_eq!(
            editing_key(egui::Key::Backspace),
            Some(GuestKeyboardInput::Backspace)
        );
        assert_eq!(
            editing_key(egui::Key::ArrowLeft),
            Some(GuestKeyboardInput::Left)
        );
        assert_eq!(editing_key(egui::Key::A), None);
        assert_eq!(editing_key(egui::Key::Space), None);
    }
}
