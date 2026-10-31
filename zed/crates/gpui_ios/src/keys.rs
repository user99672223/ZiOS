//! Conversion from UIKit hardware-key presses to GPUI keystrokes.
//!
//! The rules mirror `gpui_macos`'s `parse_keystroke` so Zed's macOS keymap applies
//! unchanged (Cmd shortcuts work as on a Mac). This module has no UIKit
//! dependency so it can be unit-tested on any host; the iOS window fills in a
//! [`RawKey`] from a `UIKey`.

use gpui::{Capslock, Keystroke, Modifiers};

/// The parts of a `UIKey` that determine the keystroke.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawKey {
    /// `UIKey.keyCode`, a USB HID usage on the keyboard page (0x07).
    pub hid_usage: isize,
    /// `UIKey.characters`, with modifiers applied.
    pub characters: String,
    /// `UIKey.charactersIgnoringModifiers`.
    pub characters_ignoring_modifiers: String,
    pub modifiers: Modifiers,
    pub capslock: bool,
}

/// What a hardware-key press means to GPUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyInput {
    /// A modifier key changed state; GPUI receives `ModifiersChanged`.
    Modifiers(Modifiers, Capslock),
    /// Any other key; GPUI receives `KeyDown`/`KeyUp`.
    Key(Keystroke),
}

mod hid {
    pub const A: isize = 0x04;
    pub const Z: isize = 0x1D;
    pub const ONE: isize = 0x1E;
    pub const ZERO: isize = 0x27;
    pub const RETURN: isize = 0x28;
    pub const ESCAPE: isize = 0x29;
    pub const BACKSPACE: isize = 0x2A;
    pub const TAB: isize = 0x2B;
    pub const SPACE: isize = 0x2C;
    pub const CAPS_LOCK: isize = 0x39;
    pub const F1: isize = 0x3A;
    pub const F12: isize = 0x45;
    pub const INSERT: isize = 0x49;
    pub const HOME: isize = 0x4A;
    pub const PAGE_UP: isize = 0x4B;
    pub const DELETE_FORWARD: isize = 0x4C;
    pub const END: isize = 0x4D;
    pub const PAGE_DOWN: isize = 0x4E;
    pub const RIGHT: isize = 0x4F;
    pub const LEFT: isize = 0x50;
    pub const DOWN: isize = 0x51;
    pub const UP: isize = 0x52;
    pub const KEYPAD_ENTER: isize = 0x58;
    pub const F13: isize = 0x68;
    pub const F24: isize = 0x73;
    pub const LEFT_CONTROL: isize = 0xE0;
    pub const RIGHT_GUI: isize = 0xE7;
}

/// US-layout characters (unshifted, shifted) for keys that print one. Used when
/// the active layout produces no ASCII character, so `cmd-s` still works on, for
/// example, a Russian layout, as `gpui_macos` does with the "command layout".
fn us_layout(hid_usage: isize) -> Option<(char, char)> {
    const DIGITS: [(char, char); 10] = [
        ('1', '!'),
        ('2', '@'),
        ('3', '#'),
        ('4', '$'),
        ('5', '%'),
        ('6', '^'),
        ('7', '&'),
        ('8', '*'),
        ('9', '('),
        ('0', ')'),
    ];
    match hid_usage {
        hid::A..=hid::Z => {
            let letter = (b'a' + (hid_usage - hid::A) as u8) as char;
            Some((letter, letter.to_ascii_uppercase()))
        }
        hid::ONE..=hid::ZERO => DIGITS.get((hid_usage - hid::ONE) as usize).copied(),
        0x2D => Some(('-', '_')),
        0x2E => Some(('=', '+')),
        0x2F => Some(('[', '{')),
        0x30 => Some((']', '}')),
        0x31 | 0x32 => Some(('\\', '|')),
        0x33 => Some((';', ':')),
        0x34 => Some(('\'', '"')),
        0x35 => Some(('`', '~')),
        0x36 => Some((',', '<')),
        0x37 => Some(('.', '>')),
        0x38 => Some(('/', '?')),
        _ => None,
    }
}

fn named_key(hid_usage: isize) -> Option<(&'static str, Option<&'static str>)> {
    const FUNCTION_KEYS: [&str; 24] = [
        "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14",
        "f15", "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24",
    ];
    Some(match hid_usage {
        hid::RETURN | hid::KEYPAD_ENTER => ("enter", Some("\n")),
        hid::ESCAPE => ("escape", None),
        hid::BACKSPACE => ("backspace", None),
        hid::TAB => ("tab", Some("\t")),
        hid::SPACE => ("space", Some(" ")),
        hid::INSERT => ("insert", None),
        hid::HOME => ("home", None),
        hid::PAGE_UP => ("pageup", None),
        hid::DELETE_FORWARD => ("delete", None),
        hid::END => ("end", None),
        hid::PAGE_DOWN => ("pagedown", None),
        hid::RIGHT => ("right", None),
        hid::LEFT => ("left", None),
        hid::DOWN => ("down", None),
        hid::UP => ("up", None),
        hid::F1..=hid::F12 => (*FUNCTION_KEYS.get((hid_usage - hid::F1) as usize)?, None),
        hid::F13..=hid::F24 => (*FUNCTION_KEYS.get((hid_usage - hid::F13 + 12) as usize)?, None),
        _ => return None,
    })
}

fn is_modifier_key(hid_usage: isize) -> bool {
    hid_usage == hid::CAPS_LOCK || (hid::LEFT_CONTROL..=hid::RIGHT_GUI).contains(&hid_usage)
}

fn printable(text: &str) -> Option<String> {
    (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_string())
}

pub fn key_input(raw: &RawKey) -> KeyInput {
    let mut modifiers = raw.modifiers;
    if is_modifier_key(raw.hid_usage) {
        return KeyInput::Modifiers(modifiers, Capslock { on: raw.capslock });
    }

    let command_or_control = modifiers.platform || modifiers.control;
    if let Some((key, key_char)) = named_key(raw.hid_usage) {
        // Shift stays on named keys (shift-tab, shift-left).
        let key_char = if command_or_control {
            None
        } else {
            key_char.map(str::to_string)
        };
        return KeyInput::Key(Keystroke {
            modifiers,
            key: key.to_string(),
            key_char,
        });
    }

    let layout = us_layout(raw.hid_usage);
    // The unshifted character on the key cap. UIKit sometimes includes shift in
    // `charactersIgnoringModifiers`, so normalize ASCII letters to lowercase.
    let mut base = raw.characters_ignoring_modifiers.clone();
    if base.len() == 1 && base.chars().all(|character| character.is_ascii_uppercase()) {
        base = base.to_ascii_lowercase();
    }
    let base_is_ascii = !base.is_empty() && base.is_ascii();
    if !base_is_ascii {
        // Non-ASCII layouts (Thai, Russian, ...): bind on the ASCII key, type the
        // layout's character.
        if let Some((unshifted, _)) = layout {
            base = unshifted.to_string();
        }
    }

    let key_char = if command_or_control {
        None
    } else {
        printable(&raw.characters)
    };

    let key = if modifiers.shift && base.chars().all(|character| character.is_ascii_lowercase()) {
        base
    } else if modifiers.shift {
        // shift-/ is "?" with shift consumed, as on macOS.
        modifiers.shift = false;
        let shifted_by_layout = (!command_or_control && !modifiers.alt)
            .then(|| printable(&raw.characters))
            .flatten()
            .filter(|characters| characters.is_ascii());
        shifted_by_layout
            .or_else(|| layout.map(|(_, shifted)| shifted.to_string()))
            .unwrap_or(base)
    } else {
        base
    };

    KeyInput::Key(Keystroke {
        modifiers,
        key,
        key_char,
    })
}

/// Hardware-key repeat. UIKit only repeats keys that reach its text system, so
/// keys GPUI consumes (arrows, backspace bound to actions, shortcuts) are repeated
/// here, driven by the display link.
#[derive(Debug)]
pub struct KeyRepeat {
    keystroke: Keystroke,
    next_repeat: std::time::Instant,
}

impl KeyRepeat {
    /// macOS defaults are close to these; iOS exposes no user setting to read.
    pub const INITIAL_DELAY: std::time::Duration = std::time::Duration::from_millis(400);
    pub const INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

    pub fn new(keystroke: Keystroke, now: std::time::Instant) -> Self {
        Self {
            keystroke,
            next_repeat: now + Self::INITIAL_DELAY,
        }
    }

    pub fn keystroke(&self) -> &Keystroke {
        &self.keystroke
    }

    /// Returns whether a repeat is due at `now`. At most one repeat is reported
    /// per call, so a stalled frame does not replay a burst of keystrokes.
    pub fn poll(&mut self, now: std::time::Instant) -> bool {
        if now < self.next_repeat {
            return false;
        }
        self.next_repeat = now + Self::INTERVAL;
        true
    }

    /// Escape never repeats; modifier keys never reach this type.
    pub fn repeats(keystroke: &Keystroke) -> bool {
        keystroke.key != "escape"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(hid_usage: isize, characters: &str, ignoring: &str, modifiers: Modifiers) -> RawKey {
        RawKey {
            hid_usage,
            characters: characters.into(),
            characters_ignoring_modifiers: ignoring.into(),
            modifiers,
            capslock: false,
        }
    }

    /// Compares against keymap syntax. `unparse` spells the platform modifier per
    /// host OS, so compare parsed values instead.
    fn assert_binding(stroke: &Keystroke, binding: &str) {
        let expected = Keystroke::parse(binding).unwrap();
        assert_eq!(
            (&stroke.modifiers, &stroke.key),
            (&expected.modifiers, &expected.key),
            "expected {binding}"
        );
    }

    fn keystroke(input: KeyInput) -> Keystroke {
        match input {
            KeyInput::Key(keystroke) => keystroke,
            other => panic!("expected a key, got {other:?}"),
        }
    }

    #[test]
    fn plain_letter_types_itself() {
        let stroke = keystroke(key_input(&raw(0x16, "s", "s", Modifiers::none())));
        assert_eq!(stroke.key, "s");
        assert_eq!(stroke.key_char.as_deref(), Some("s"));
        assert_eq!(stroke.modifiers, Modifiers::none());
    }

    #[test]
    fn command_shortcuts_match_the_macos_keymap() {
        let stroke = keystroke(key_input(&raw(0x16, "s", "s", Modifiers::command())));
        assert_binding(&stroke, "cmd-s");
        assert_eq!(stroke.key_char, None);

        let stroke = keystroke(key_input(&raw(
            0x13,
            "p",
            "P",
            Modifiers::command_shift(),
        )));
        assert_binding(&stroke, "cmd-shift-p");
    }

    #[test]
    fn shifted_letter_keeps_shift() {
        let stroke = keystroke(key_input(&raw(0x04, "A", "A", Modifiers::shift())));
        assert_binding(&stroke, "shift-a");
        assert_eq!(stroke.key_char.as_deref(), Some("A"));
    }

    #[test]
    fn shifted_symbol_consumes_shift() {
        let stroke = keystroke(key_input(&raw(0x38, "?", "/", Modifiers::shift())));
        assert_eq!(stroke.key, "?");
        assert!(!stroke.modifiers.shift);
        assert_eq!(stroke.key_char.as_deref(), Some("?"));

        // With cmd, UIKit's characters are unreliable; the US layout decides.
        let stroke = keystroke(key_input(&raw(0x38, "/", "/", Modifiers::command_shift())));
        assert_binding(&stroke, "cmd-?");
    }

    #[test]
    fn option_keeps_the_key_and_types_the_composed_character() {
        let stroke = keystroke(key_input(&raw(
            0x16,
            "ß",
            "s",
            Modifiers {
                alt: true,
                ..Default::default()
            },
        )));
        assert_binding(&stroke, "alt-s");
        assert_eq!(stroke.key_char.as_deref(), Some("ß"));
    }

    #[test]
    fn named_keys() {
        let cases = [
            (0x28, "enter", Some("\n")),
            (0x29, "escape", None),
            (0x2A, "backspace", None),
            (0x2B, "tab", Some("\t")),
            (0x2C, "space", Some(" ")),
            (0x4C, "delete", None),
            (0x50, "left", None),
            (0x52, "up", None),
            (0x3A, "f1", None),
            (0x45, "f12", None),
            (0x68, "f13", None),
        ];
        for (hid_usage, key, key_char) in cases {
            let stroke = keystroke(key_input(&raw(hid_usage, "", "", Modifiers::none())));
            assert_eq!(stroke.key, key, "hid {hid_usage:#x}");
            assert_eq!(stroke.key_char.as_deref(), key_char, "hid {hid_usage:#x}");
        }
        let stroke = keystroke(key_input(&raw(0x2B, "\t", "\t", Modifiers::shift())));
        assert_binding(&stroke, "shift-tab");
        let stroke = keystroke(key_input(&raw(0x50, "", "", Modifiers::command())));
        assert_binding(&stroke, "cmd-left");
    }

    #[test]
    fn non_ascii_layout_binds_on_the_ascii_key() {
        // Russian layout: the "s" key types "ы".
        let stroke = keystroke(key_input(&raw(0x16, "ы", "ы", Modifiers::none())));
        assert_eq!(stroke.key, "s");
        assert_eq!(stroke.key_char.as_deref(), Some("ы"));
        let stroke = keystroke(key_input(&raw(0x16, "ы", "ы", Modifiers::command())));
        assert_binding(&stroke, "cmd-s");
    }

    #[test]
    fn modifier_keys_report_modifier_changes() {
        let input = key_input(&RawKey {
            hid_usage: 0xE3,
            modifiers: Modifiers::command(),
            ..Default::default()
        });
        assert_eq!(
            input,
            KeyInput::Modifiers(Modifiers::command(), Capslock { on: false })
        );
        let input = key_input(&RawKey {
            hid_usage: 0x39,
            capslock: true,
            ..Default::default()
        });
        assert_eq!(
            input,
            KeyInput::Modifiers(Modifiers::none(), Capslock { on: true })
        );
    }

    #[test]
    fn repeat_waits_for_the_initial_delay_then_paces_by_interval() {
        let start = std::time::Instant::now();
        let mut repeat = KeyRepeat::new(Keystroke::parse("left").unwrap(), start);
        assert!(!repeat.poll(start + std::time::Duration::from_millis(100)));
        assert!(repeat.poll(start + KeyRepeat::INITIAL_DELAY));
        assert!(!repeat.poll(start + KeyRepeat::INITIAL_DELAY + std::time::Duration::from_millis(10)));
        assert!(repeat.poll(start + KeyRepeat::INITIAL_DELAY + KeyRepeat::INTERVAL));
        assert!(!KeyRepeat::repeats(&Keystroke::parse("escape").unwrap()));
    }
}
