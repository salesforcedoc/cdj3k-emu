//! Main LCD touch input handling - capture from any viewport (main or popout)
//! and apply to the per-frame MISO state (touch coords, navigation rotary,
//! ctrl-latched touch).

use cdj3k_emu_subucom::miso_frame;
use std::time::{Duration, Instant};

use super::frame_inject::LCD_TOUCH_RANGE;
use super::CdjApp;

/// A press and release can arrive in one UI frame; retain the touch long
/// enough for the guest's input poll to observe a down state.
const MIN_LCD_TOUCH_PRESS: Duration = Duration::from_millis(60);

/// Raw LCD touch input captured from a viewport frame, applied via
/// [`CdjApp::apply_lcd_touch`].
pub(crate) struct LcdTouchCapture {
    pub hovered: bool,
    pub scroll_y: f32,
    pub pointer_moved: bool,
    pub is_down: bool,
    pub clicked: bool,
    pub ctrl: bool,
    pub right_down: bool,
    pub interact_pos: Option<egui::Pos2>,
    pub display_rect: egui::Rect,
}

impl CdjApp {
    /// Apply raw LCD touch input captured from any viewport (main or popout).
    pub(crate) fn apply_lcd_touch(&mut self, cap: LcdTouchCapture, ctx: &egui::Context) {
        use super::ui::{NAV_DETENT_COUNT, NAV_SCROLL_PX_PER_TICK};

        if cap.hovered {
            if cap.scroll_y != 0.0 {
                self.consume_lcd_scroll(cap.scroll_y, NAV_DETENT_COUNT, NAV_SCROLL_PX_PER_TICK);
            }
            if cap.pointer_moved {
                self.lcd_nav_mode = false;
            }
        }

        if self.lcd_nav_mode {
            self.handle_btn_interaction(
                cap.is_down,
                cap.clicked,
                cap.ctrl,
                miso_frame::BTN_ROTARY_PRESS,
            );
            self.handle_btn_interaction(cap.right_down, false, false, miso_frame::BTN_BACK);
            return;
        }

        let now = Instant::now();
        if cap.is_down || cap.clicked {
            self.handle_lcd_press(&cap, now);
            if cap.clicked && !cap.is_down && !self.lcd_touch_ctrl_latched {
                self.release_lcd_touch_after_minimum(now, ctx);
            }
        } else if self.lcd_touch_ctrl_latched {
            // Mouse released while Ctrl is still held: keep the latched touch
            // alive until Ctrl is released.
            if !cap.ctrl {
                self.clear_lcd_touch();
            }
        } else if self.lcd_touch.is_some() {
            self.release_lcd_touch_after_minimum(now, ctx);
        }
    }

    fn consume_lcd_scroll(&mut self, scroll_y: f32, detent_count: f32, px_per_tick: f32) {
        self.nav_scroll_accum += scroll_y;
        let detent_rad = std::f32::consts::TAU / detent_count;
        while self.nav_scroll_accum >= px_per_tick {
            self.nav_scroll_accum -= px_per_tick;
            self.nav_angle += detent_rad;
            self.rotary = self.rotary.wrapping_sub(1);
            self.inject_rotary();
        }
        while self.nav_scroll_accum <= -px_per_tick {
            self.nav_scroll_accum += px_per_tick;
            self.nav_angle -= detent_rad;
            self.rotary = self.rotary.wrapping_add(1);
            self.inject_rotary();
        }
        self.lcd_nav_mode = true;
    }

    fn handle_lcd_press(&mut self, cap: &LcdTouchCapture, now: Instant) {
        if self.lcd_touch_ctrl_latched {
            if !cap.ctrl {
                self.clear_lcd_touch();
            }
            return;
        }
        if let Some(pos) = cap.interact_pos {
            if self.lcd_touch.is_none() {
                self.lcd_touch_pressed_at = Some(now);
            }
            // X is mirrored: the device's origin is at the right edge.
            let nx = (LCD_TOUCH_RANGE
                - (pos.x - cap.display_rect.left()) / cap.display_rect.width() * LCD_TOUCH_RANGE)
                as u16;
            let ny = ((pos.y - cap.display_rect.top()) / cap.display_rect.height()
                * LCD_TOUCH_RANGE) as u16;
            if self.lcd_touch != Some((nx, ny)) {
                self.lcd_touch = Some((nx, ny));
                self.inject_touch(nx, ny);
            }
        }
        if cap.ctrl {
            self.lcd_touch_ctrl_latched = true;
        }
    }

    fn release_lcd_touch_after_minimum(&mut self, now: Instant, ctx: &egui::Context) {
        let remaining = remaining_touch_hold(self.lcd_touch_pressed_at, now);
        if remaining.is_zero() {
            self.clear_lcd_touch();
        } else {
            ctx.request_repaint_after(remaining);
        }
    }

    fn clear_lcd_touch(&mut self) {
        self.lcd_touch = None;
        self.lcd_touch_pressed_at = None;
        self.lcd_touch_ctrl_latched = false;
        self.inject_touch(0, 0);
    }
}

fn remaining_touch_hold(pressed_at: Option<Instant>, now: Instant) -> Duration {
    pressed_at
        .map(|at| (at + MIN_LCD_TOUCH_PRESS).saturating_duration_since(now))
        .unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_click_is_held_for_guest_input_poll() {
        let press_at = Instant::now();
        assert_eq!(
            remaining_touch_hold(Some(press_at), press_at),
            MIN_LCD_TOUCH_PRESS
        );
        assert_eq!(
            remaining_touch_hold(Some(press_at), press_at + Duration::from_millis(20)),
            Duration::from_millis(40)
        );
        assert_eq!(
            remaining_touch_hold(Some(press_at), press_at + MIN_LCD_TOUCH_PRESS),
            Duration::ZERO
        );
    }
}
