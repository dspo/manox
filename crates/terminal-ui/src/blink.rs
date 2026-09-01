//! Cursor blink phase state.
//!
//! The view ticks the phase on a 530ms timer; per-frame visibility combines
//! the phase with the configured mode, the program's blink flag (DECSET 12 /
//! DECSCUSR), and force-visible conditions (selection, IME preedit, recent
//! input).

use manox_terminal::settings::CursorBlinkSetting;

/// Blink phase for the terminal cursor. `on` flips every tick; `reset` pins
/// it back to visible so input never lands on an invisible cursor.
pub struct CursorBlink {
    mode: CursorBlinkSetting,
    on: bool,
}

impl CursorBlink {
    pub fn new(mode: CursorBlinkSetting) -> Self {
        Self { mode, on: true }
    }

    pub fn mode(&self) -> CursorBlinkSetting {
        self.mode
    }

    pub fn tick(&mut self) {
        self.on = !self.on;
    }

    /// Pin the phase back to visible (input, `CursorBlinkingChange`).
    pub fn reset(&mut self) {
        self.on = true;
    }

    /// Whether the cursor is visible this frame: always under `Off`, phased
    /// under `On`, and phased only while the program asks for blinking under
    /// `Terminal`. `force` (selection / IME preedit / recent input) pins it
    /// visible regardless of mode and phase.
    pub fn visible(&self, term_blinking: bool, force: bool) -> bool {
        if force {
            return true;
        }
        match self.mode {
            CursorBlinkSetting::Off => true,
            CursorBlinkSetting::On => self.on,
            CursorBlinkSetting::Terminal => !term_blinking || self.on,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_steady() {
        let mut blink = CursorBlink::new(CursorBlinkSetting::Off);
        blink.tick();
        assert!(blink.visible(false, false));
        assert!(blink.visible(true, false));
    }

    #[test]
    fn on_follows_the_phase() {
        let mut blink = CursorBlink::new(CursorBlinkSetting::On);
        assert!(blink.visible(false, false));
        blink.tick();
        assert!(!blink.visible(false, false));
    }

    #[test]
    fn terminal_follows_the_program_flag() {
        let mut blink = CursorBlink::new(CursorBlinkSetting::Terminal);
        // Program not blinking: steady regardless of phase.
        blink.tick();
        assert!(blink.visible(false, false));
        // Program blinking: phased.
        assert!(!blink.visible(true, false));
    }

    #[test]
    fn force_pins_visible() {
        let mut blink = CursorBlink::new(CursorBlinkSetting::On);
        blink.tick();
        assert!(blink.visible(false, true));
    }

    #[test]
    fn reset_restores_the_visible_phase() {
        let mut blink = CursorBlink::new(CursorBlinkSetting::On);
        blink.tick();
        assert!(!blink.visible(false, false));
        blink.reset();
        assert!(blink.visible(false, false));
    }
}
