//! Shortcut-modifier translation between Apple and PC conventions.
//!
//! macOS uses Command for shortcuts (⌘C) where Windows, Linux and Android use
//! Control (Ctrl+C). The receiving core translates keys from a peer that uses
//! the other convention, so every platform shell gets keys that already mean
//! what the user intended and none of them has to repeat this logic.
//!
//! - Apple → PC: Command becomes Control. Control stays Control, so pressing
//!   it never sends a lone Windows-key tap that would open the Start menu.
//! - PC → Apple: Control and the Windows/Super key swap places.

use crate::model::{
    LC_INPUT_KEY_DOWN, LC_INPUT_KEY_UP, LC_MODIFIER_COMMAND, LC_MODIFIER_CONTROL,
    LC_MODIFIER_LEFT_COMMAND, LC_MODIFIER_LEFT_CONTROL, LC_MODIFIER_RIGHT_COMMAND,
    LC_MODIFIER_RIGHT_CONTROL, LcInputEvent,
};

const HID_LEFT_CONTROL: u32 = 0xE0;
const HID_LEFT_GUI: u32 = 0xE3;
const HID_RIGHT_CONTROL: u32 = 0xE4;
const HID_RIGHT_GUI: u32 = 0xE7;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShortcutRemap {
    #[default]
    None,
    AppleToPc,
    PcToApple,
}

impl ShortcutRemap {
    /// The translation for input sent by `sender` and played on `receiver`,
    /// both given as `CoreConfig::platform` values.
    pub fn between(sender: &str, receiver: &str) -> Self {
        let apple = |platform: &str| platform == "macos";
        let pc = |platform: &str| matches!(platform, "windows" | "linux" | "android");
        if apple(sender) && pc(receiver) {
            Self::AppleToPc
        } else if pc(sender) && apple(receiver) {
            Self::PcToApple
        } else {
            Self::None
        }
    }

    pub fn apply(self, event: &mut LcInputEvent) {
        if self == Self::None {
            return;
        }
        if matches!(event.kind, LC_INPUT_KEY_DOWN | LC_INPUT_KEY_UP) {
            event.code = self.key(event.code);
        }
        event.modifiers = self.modifiers(event.modifiers);
    }

    fn key(self, code: u32) -> u32 {
        match (self, code) {
            (Self::AppleToPc, HID_LEFT_GUI) => HID_LEFT_CONTROL,
            (Self::AppleToPc, HID_RIGHT_GUI) => HID_RIGHT_CONTROL,
            (Self::PcToApple, HID_LEFT_CONTROL) => HID_LEFT_GUI,
            (Self::PcToApple, HID_RIGHT_CONTROL) => HID_RIGHT_GUI,
            (Self::PcToApple, HID_LEFT_GUI) => HID_LEFT_CONTROL,
            (Self::PcToApple, HID_RIGHT_GUI) => HID_RIGHT_CONTROL,
            _ => code,
        }
    }

    fn modifiers(self, mask: u32) -> u32 {
        const PAIRS: [(u32, u32); 3] = [
            (LC_MODIFIER_COMMAND, LC_MODIFIER_CONTROL),
            (LC_MODIFIER_LEFT_COMMAND, LC_MODIFIER_LEFT_CONTROL),
            (LC_MODIFIER_RIGHT_COMMAND, LC_MODIFIER_RIGHT_CONTROL),
        ];
        let mut result = mask;
        for (command, control) in PAIRS {
            let has_command = mask & command != 0;
            let has_control = mask & control != 0;
            result &= !(command | control);
            match self {
                Self::AppleToPc => {
                    if has_command || has_control {
                        result |= control;
                    }
                }
                Self::PcToApple => {
                    if has_control {
                        result |= command;
                    }
                    if has_command {
                        result |= control;
                    }
                }
                Self::None => {
                    result |= mask & (command | control);
                }
            }
        }
        result
    }
}
