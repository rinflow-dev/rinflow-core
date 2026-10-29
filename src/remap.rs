//! Modifier keys between Apple and PC conventions (`CoreConfig::modifier_map`).
//!
//! macOS uses Command for shortcuts (⌘C) where Windows, Linux and Android use
//! Control (Ctrl+C). When a peer of the other convention drives this device,
//! the receiving core plays each of the peer's modifier keys as the key the
//! user chose here, so every platform shell gets keys that already mean what
//! the user intended and none of them has to repeat this logic.
//!
//! Keys are named by position, the same on both conventions: `super` is
//! Command on a Mac and the Windows/Super key on a PC, `alt` is Option. The
//! map is used only between conventions (a Mac driving a PC, or a PC driving
//! a Mac); between two PCs or two Macs keys always keep their position.
//!
//! - [`ModifierMap::KEEP`] (the core's default) plays keys where they are, as
//!   Deskflow does: a Mac's Command is the Windows key.
//! - [`ModifierMap::SWAP`] swaps Command and Control: ⌘C copies on a PC, and
//!   the Mac's Control is the Windows key; the other way, a PC's Ctrl is
//!   Command on a Mac and its Windows key is Control.
//! - Any other choice, like Deskflow's per-screen modifier settings: each key
//!   plays as Control, Super, Alt, Shift, or nothing.
//!
//! Left and right keys keep their side. Two keys mapped to the same target
//! share it: releasing either releases it.

use crate::model::{
    LC_INPUT_KEY_DOWN, LC_INPUT_KEY_UP, LC_MODIFIER_COMMAND, LC_MODIFIER_CONTROL,
    LC_MODIFIER_LEFT_COMMAND, LC_MODIFIER_LEFT_CONTROL, LC_MODIFIER_LEFT_OPTION,
    LC_MODIFIER_LEFT_SHIFT, LC_MODIFIER_OPTION, LC_MODIFIER_RIGHT_COMMAND,
    LC_MODIFIER_RIGHT_CONTROL, LC_MODIFIER_RIGHT_OPTION, LC_MODIFIER_RIGHT_SHIFT,
    LC_MODIFIER_SHIFT, LcInputEvent,
};
use serde::{Deserialize, Serialize};

/// A modifier key, by position; or none (the key is dropped).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Control,
    /// Command on a Mac, the Windows/Super key on a PC.
    Super,
    /// Option on a Mac.
    Alt,
    Shift,
    None,
}

/// What each modifier key of a peer with the other platform convention
/// plays as here. JSON: `{"super":"control","control":"super","alt":"alt"}`;
/// a missing key keeps its position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModifierMap {
    #[serde(rename = "super")]
    pub super_key: Modifier,
    pub control: Modifier,
    pub alt: Modifier,
}

impl Default for ModifierMap {
    fn default() -> Self {
        Self::KEEP
    }
}

/// HID usages of the left and right key of each modifier.
const fn hid_codes(modifier: Modifier) -> Option<(u32, u32)> {
    match modifier {
        Modifier::Control => Some((0xE0, 0xE4)),
        Modifier::Shift => Some((0xE1, 0xE5)),
        Modifier::Alt => Some((0xE2, 0xE6)),
        Modifier::Super => Some((0xE3, 0xE7)),
        Modifier::None => None,
    }
}

/// The modifier mask bits of each modifier: either side, left, right.
const fn mask_bits(modifier: Modifier) -> Option<[u32; 3]> {
    match modifier {
        Modifier::Control => Some([
            LC_MODIFIER_CONTROL,
            LC_MODIFIER_LEFT_CONTROL,
            LC_MODIFIER_RIGHT_CONTROL,
        ]),
        Modifier::Shift => Some([
            LC_MODIFIER_SHIFT,
            LC_MODIFIER_LEFT_SHIFT,
            LC_MODIFIER_RIGHT_SHIFT,
        ]),
        Modifier::Alt => Some([
            LC_MODIFIER_OPTION,
            LC_MODIFIER_LEFT_OPTION,
            LC_MODIFIER_RIGHT_OPTION,
        ]),
        Modifier::Super => Some([
            LC_MODIFIER_COMMAND,
            LC_MODIFIER_LEFT_COMMAND,
            LC_MODIFIER_RIGHT_COMMAND,
        ]),
        Modifier::None => None,
    }
}

impl ModifierMap {
    /// Keys keep their position.
    pub const KEEP: Self = Self {
        super_key: Modifier::Super,
        control: Modifier::Control,
        alt: Modifier::Alt,
    };
    /// Command (Super) and Control swap places.
    pub const SWAP: Self = Self {
        super_key: Modifier::Control,
        control: Modifier::Super,
        alt: Modifier::Alt,
    };

    /// The map for input sent by a peer on `sender` and played on
    /// `receiver` (`CoreConfig::platform` values): `self` between the Apple
    /// and PC conventions, [`Self::KEEP`] otherwise.
    pub fn between(self, sender: &str, receiver: &str) -> Self {
        let apple = |platform: &str| platform == "macos";
        let pc = |platform: &str| matches!(platform, "windows" | "linux" | "android");
        if (apple(sender) && pc(receiver)) || (pc(sender) && apple(receiver)) {
            self
        } else {
            Self::KEEP
        }
    }

    /// The three keys this map moves, with where each goes.
    fn pairs(self) -> [(Modifier, Modifier); 3] {
        [
            (Modifier::Super, self.super_key),
            (Modifier::Control, self.control),
            (Modifier::Alt, self.alt),
        ]
    }

    /// Plays `event` as this map says. False when the event is dropped: a
    /// key mapped to nothing.
    pub fn apply(self, event: &mut LcInputEvent) -> bool {
        if self == Self::KEEP {
            return true;
        }
        if matches!(event.kind, LC_INPUT_KEY_DOWN | LC_INPUT_KEY_UP) {
            match self.key(event.code) {
                Some(code) => event.code = code,
                None => return false,
            }
        }
        event.modifiers = self.modifiers(event.modifiers);
        true
    }

    /// The code `code` plays as; `None` when it is dropped.
    fn key(self, code: u32) -> Option<u32> {
        for (from, to) in self.pairs() {
            let Some((left, right)) = hid_codes(from) else {
                continue;
            };
            if code == left || code == right {
                return hid_codes(to).map(
                    |(to_left, to_right)| {
                        if code == left { to_left } else { to_right }
                    },
                );
            }
        }
        Some(code)
    }

    fn modifiers(self, mask: u32) -> u32 {
        let pairs = self.pairs();
        let mut result = mask;
        // The moved keys' bits first go, then come back where they play.
        for (from, _) in pairs {
            for bit in mask_bits(from).into_iter().flatten() {
                result &= !bit;
            }
        }
        for (from, to) in pairs {
            let (Some(from), Some(to)) = (mask_bits(from), mask_bits(to)) else {
                continue;
            };
            for (from_bit, to_bit) in from.into_iter().zip(to) {
                if mask & from_bit != 0 {
                    result |= to_bit;
                }
            }
        }
        result
    }
}
