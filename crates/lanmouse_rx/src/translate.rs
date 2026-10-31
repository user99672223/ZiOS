//! Turns lan-mouse input events into GPUI platform input.
//!
//! lan-mouse sends relative pointer motion, evdev button codes, scroll axes, and
//! Linux evdev key codes. The receiver keeps a virtual cursor inside the editor
//! window, produces real GPUI mouse and keyboard events (so hover, right-click,
//! and drag work exactly as with a native pointer), and reports when the cursor
//! pushes past the edge it entered from so control can return to the laptop.

use std::time::{Duration, Instant};

use gpui::{
    Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection,
    PlatformInput, Point, ScrollDelta, ScrollEvent, ScrollSource, Size, TouchPhase, px,
};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode::Linux,
};
use crate::proto::Position;

/// Pixels the cursor must be pushed past the entry edge before control returns
/// to the laptop, so brushing the edge does not bounce the cursor back.
const RELEASE_OVERSHOOT: f64 = 24.0;
/// macOS's double-click interval and slop.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: f64 = 4.0;
/// Pixels per line for wheel ticks (120 units = one notch = three lines).
const LINES_PER_NOTCH: f32 = 3.0;

#[derive(Debug, Clone)]
pub enum Output {
    Input(PlatformInput),
    /// The cursor left through the entry edge: tell the laptop to take over.
    Release,
}

#[derive(Debug)]
struct ClickTracker {
    button: MouseButton,
    time: Instant,
    position: Point<f64>,
    count: usize,
}

#[derive(Debug)]
pub struct Translator {
    position: Point<f64>,
    bounds: Size<f64>,
    entered_from: Option<Position>,
    overshoot: f64,
    modifiers: Modifiers,
    capslock: bool,
    pressed_button: Option<MouseButton>,
    last_click: Option<ClickTracker>,
    /// lan-mouse backends disagree on scroll sign; see docs/DECISIONS.md.
    pub invert_scroll: bool,
}

impl Output {
    pub fn is_release(&self) -> bool {
        matches!(self, Self::Release)
    }
}

impl Translator {
    pub fn new(bounds: Size<f64>) -> Self {
        Self {
            position: Point::new(bounds.width / 2.0, bounds.height / 2.0),
            bounds,
            entered_from: None,
            overshoot: 0.0,
            modifiers: Modifiers::default(),
            capslock: false,
            pressed_button: None,
            last_click: None,
            invert_scroll: false,
        }
    }

    pub fn position(&self) -> Point<f64> {
        self.position
    }

    pub fn set_bounds(&mut self, bounds: Size<f64>) {
        self.bounds = bounds;
        self.clamp();
    }

    fn clamp(&mut self) {
        self.position.x = self.position.x.clamp(0.0, (self.bounds.width - 1.0).max(0.0));
        self.position.y = self.position.y.clamp(0.0, (self.bounds.height - 1.0).max(0.0));
    }

    /// The laptop's cursor entered this device through the `from` edge (the
    /// position lan-mouse sends is the side of this screen it entered on).
    pub fn enter(&mut self, from: Position) {
        self.entered_from = Some(from);
        self.overshoot = 0.0;
        match from {
            Position::Left => self.position.x = 0.0,
            Position::Right => self.position.x = self.bounds.width - 1.0,
            Position::Top => self.position.y = 0.0,
            Position::Bottom => self.position.y = self.bounds.height - 1.0,
        }
        self.clamp();
    }

    /// The laptop took control back; release anything still held.
    pub fn leave(&mut self) -> Vec<Output> {
        self.entered_from = None;
        let mut outputs = Vec::new();
        if let Some(button) = self.pressed_button.take() {
            outputs.push(Output::Input(PlatformInput::MouseUp(MouseUpEvent {
                button,
                position: self.gpui_position(),
                modifiers: self.modifiers,
                click_count: 1,
            })));
        }
        if self.modifiers != Modifiers::default() {
            self.modifiers = Modifiers::default();
            outputs.push(Output::Input(PlatformInput::ModifiersChanged(
                ModifiersChangedEvent {
                    modifiers: self.modifiers,
                    capslock: Capslock { on: self.capslock },
                },
            )));
        }
        outputs
    }

    fn gpui_position(&self) -> Point<gpui::Pixels> {
        Point::new(px(self.position.x as f32), px(self.position.y as f32))
    }

    pub fn translate(&mut self, event: Event, now: Instant) -> Vec<Output> {
        match event {
            Event::Pointer(PointerEvent::Motion { dx, dy, .. }) => self.motion(dx, dy),
            Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                self.button(button, state != 0, now)
            }
            Event::Pointer(PointerEvent::Axis { axis, value, .. }) => {
                let value = if self.invert_scroll { -value } else { value };
                // Positive values scroll down (Wayland convention); GPUI's
                // positive deltas scroll toward the top.
                let delta = match axis {
                    0 => Point::new(px(0.), px(-value as f32)),
                    _ => Point::new(px(-value as f32), px(0.)),
                };
                vec![self.scroll(ScrollDelta::Pixels(delta))]
            }
            Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value }) => {
                let value = if self.invert_scroll { -value } else { value };
                let lines = -(value as f32) / 120.0 * LINES_PER_NOTCH;
                let delta = match axis {
                    0 => Point::new(0.0, lines),
                    _ => Point::new(lines, 0.0),
                };
                vec![self.scroll(ScrollDelta::Lines(delta))]
            }
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => self.key(key, state != 0),
            Event::Keyboard(KeyboardEvent::Modifiers {
                depressed, locked, ..
            }) => self.xkb_modifiers(depressed, locked),
        }
    }

    fn motion(&mut self, dx: f64, dy: f64) -> Vec<Output> {
        let target = Point::new(self.position.x + dx, self.position.y + dy);
        // How far the motion pushes beyond the edge the cursor came in through.
        let beyond = match self.entered_from {
            Some(Position::Left) => -target.x,
            Some(Position::Right) => target.x - (self.bounds.width - 1.0),
            Some(Position::Top) => -target.y,
            Some(Position::Bottom) => target.y - (self.bounds.height - 1.0),
            None => 0.0,
        };
        self.position = target;
        self.clamp();
        let mut outputs = vec![Output::Input(PlatformInput::MouseMove(MouseMoveEvent {
            position: self.gpui_position(),
            pressed_button: self.pressed_button,
            modifiers: self.modifiers,
        }))];
        if beyond > 0.0 && self.pressed_button.is_none() {
            self.overshoot += beyond;
            if self.overshoot >= RELEASE_OVERSHOOT {
                self.overshoot = 0.0;
                outputs.extend(self.leave());
                outputs.push(Output::Release);
            }
        } else {
            self.overshoot = 0.0;
        }
        outputs
    }

    fn button(&mut self, code: u32, pressed: bool, now: Instant) -> Vec<Output> {
        let button = match code {
            BTN_LEFT => MouseButton::Left,
            BTN_RIGHT => MouseButton::Right,
            BTN_MIDDLE => MouseButton::Middle,
            BTN_BACK => MouseButton::Navigate(NavigationDirection::Back),
            BTN_FORWARD => MouseButton::Navigate(NavigationDirection::Forward),
            other => {
                log::debug!("ignoring mouse button {other:#x}");
                return Vec::new();
            }
        };
        let position = self.gpui_position();
        if pressed {
            let count = match &self.last_click {
                Some(last)
                    if last.button == button
                        && now.duration_since(last.time) <= MULTI_CLICK_INTERVAL
                        && (last.position.x - self.position.x).abs() <= MULTI_CLICK_DISTANCE
                        && (last.position.y - self.position.y).abs() <= MULTI_CLICK_DISTANCE =>
                {
                    last.count + 1
                }
                _ => 1,
            };
            self.last_click = Some(ClickTracker {
                button,
                time: now,
                position: self.position,
                count,
            });
            self.pressed_button = Some(button);
            vec![Output::Input(PlatformInput::MouseDown(MouseDownEvent {
                button,
                position,
                modifiers: self.modifiers,
                click_count: count,
                first_mouse: false,
            }))]
        } else {
            if self.pressed_button == Some(button) {
                self.pressed_button = None;
            }
            let click_count = self
                .last_click
                .as_ref()
                .filter(|last| last.button == button)
                .map_or(1, |last| last.count);
            vec![Output::Input(PlatformInput::MouseUp(MouseUpEvent {
                button,
                position,
                modifiers: self.modifiers,
                click_count,
            }))]
        }
    }

    fn scroll(&self, delta: ScrollDelta) -> Output {
        Output::Input(PlatformInput::Scroll(ScrollEvent {
            position: self.gpui_position(),
            delta,
            modifiers: self.modifiers,
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        }))
    }

    fn modifiers_changed(&self) -> Output {
        Output::Input(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
            modifiers: self.modifiers,
            capslock: Capslock { on: self.capslock },
        }))
    }

    /// X11/xkb modifier masks, which lan-mouse forwards from every backend.
    fn xkb_modifiers(&mut self, depressed: u32, locked: u32) -> Vec<Output> {
        const SHIFT: u32 = 1;
        const LOCK: u32 = 1 << 1;
        const CONTROL: u32 = 1 << 2;
        const ALT: u32 = 1 << 3;
        const SUPER: u32 = 1 << 6;
        let modifiers = Modifiers {
            shift: depressed & SHIFT != 0,
            control: depressed & CONTROL != 0,
            alt: depressed & ALT != 0,
            platform: depressed & SUPER != 0,
            function: false,
        };
        let capslock = (depressed | locked) & LOCK != 0;
        if modifiers == self.modifiers && capslock == self.capslock {
            return Vec::new();
        }
        self.modifiers = modifiers;
        self.capslock = capslock;
        vec![self.modifiers_changed()]
    }

    fn key(&mut self, code: u32, pressed: bool) -> Vec<Output> {
        let Ok(key) = Linux::try_from(code) else {
            log::debug!("ignoring unknown key code {code}");
            return Vec::new();
        };
        if let Some(update) = modifier_for(key) {
            update(&mut self.modifiers, pressed);
            if matches!(key, Linux::KeyCapsLock) && pressed {
                self.capslock = !self.capslock;
            }
            return vec![self.modifiers_changed()];
        }
        let Some(keystroke) = keystroke_for(key, self.modifiers, self.capslock) else {
            log::debug!("no GPUI mapping for {key:?}");
            return Vec::new();
        };
        let input = if pressed {
            PlatformInput::KeyDown(KeyDownEvent {
                keystroke,
                is_held: false,
                prefer_character_input: false,
            })
        } else {
            PlatformInput::KeyUp(KeyUpEvent { keystroke })
        };
        vec![Output::Input(input)]
    }
}

type ModifierUpdate = fn(&mut Modifiers, bool);

fn modifier_for(key: Linux) -> Option<ModifierUpdate> {
    Some(match key {
        Linux::KeyLeftShift | Linux::KeyRightShift => |modifiers, down| modifiers.shift = down,
        Linux::KeyLeftCtrl | Linux::KeyRightCtrl => |modifiers, down| modifiers.control = down,
        Linux::KeyLeftAlt | Linux::KeyRightalt => |modifiers, down| modifiers.alt = down,
        // A Mac's Command key arrives as Meta; it is GPUI's platform modifier,
        // so Zed's macOS keymap applies.
        Linux::KeyLeftMeta | Linux::KeyRightmeta => |modifiers, down| modifiers.platform = down,
        Linux::KeyCapsLock => |_, _| {},
        _ => return None,
    })
}

/// US-layout key names and characters, as GPUI's macOS keystrokes use them.
pub fn keystroke_for(key: Linux, modifiers: Modifiers, capslock: bool) -> Option<Keystroke> {
    use Linux::*;
    let named = |name: &str, key_char: Option<&str>| (name.to_string(), key_char.map(str::to_string));
    let (name, typed): (String, Option<String>) = match key {
        KeyEnter | KeyKpEnter => named("enter", Some("\n")),
        KeyEsc => named("escape", None),
        KeyBackspace => named("backspace", None),
        KeyTab => named("tab", Some("\t")),
        KeySpace => named("space", Some(" ")),
        KeyDelete => named("delete", None),
        KeyInsert => named("insert", None),
        KeyHome => named("home", None),
        KeyEnd => named("end", None),
        KeyPageup => named("pageup", None),
        KeyPagedown => named("pagedown", None),
        KeyUp => named("up", None),
        KeyDown => named("down", None),
        KeyLeft => named("left", None),
        KeyRight => named("right", None),
        KeyF1 => named("f1", None),
        KeyF2 => named("f2", None),
        KeyF3 => named("f3", None),
        KeyF4 => named("f4", None),
        KeyF5 => named("f5", None),
        KeyF6 => named("f6", None),
        KeyF7 => named("f7", None),
        KeyF8 => named("f8", None),
        KeyF9 => named("f9", None),
        KeyF10 => named("f10", None),
        KeyF11 => named("f11", None),
        KeyF12 => named("f12", None),
        other => {
            let (unshifted, shifted) = printable(other)?;
            let command_or_control = modifiers.platform || modifiers.control;
            let letter = unshifted.is_ascii_lowercase();
            let shift_applies = modifiers.shift ^ (capslock && letter);
            let character = if shift_applies { shifted } else { unshifted };
            let key_char = (!command_or_control).then(|| character.to_string());
            // Same folding as macOS: shift stays on letters, shifted symbols
            // become their own key (shift-/ is "?").
            let mut keystroke_modifiers = modifiers;
            let name = if modifiers.shift && !letter {
                keystroke_modifiers.shift = false;
                shifted.to_string()
            } else {
                unshifted.to_string()
            };
            return Some(Keystroke {
                modifiers: keystroke_modifiers,
                key: name,
                key_char,
            });
        }
    };
    let key_char = if modifiers.platform || modifiers.control {
        None
    } else {
        typed
    };
    Some(Keystroke {
        modifiers,
        key: name,
        key_char,
    })
}

fn printable(key: Linux) -> Option<(char, char)> {
    use Linux::*;
    Some(match key {
        KeyA => ('a', 'A'),
        KeyB => ('b', 'B'),
        KeyC => ('c', 'C'),
        KeyD => ('d', 'D'),
        KeyE => ('e', 'E'),
        KeyF => ('f', 'F'),
        KeyG => ('g', 'G'),
        KeyH => ('h', 'H'),
        KeyI => ('i', 'I'),
        KeyJ => ('j', 'J'),
        KeyK => ('k', 'K'),
        KeyL => ('l', 'L'),
        KeyM => ('m', 'M'),
        KeyN => ('n', 'N'),
        KeyO => ('o', 'O'),
        KeyP => ('p', 'P'),
        KeyQ => ('q', 'Q'),
        KeyR => ('r', 'R'),
        KeyS => ('s', 'S'),
        KeyT => ('t', 'T'),
        KeyU => ('u', 'U'),
        KeyV => ('v', 'V'),
        KeyW => ('w', 'W'),
        KeyX => ('x', 'X'),
        KeyY => ('y', 'Y'),
        KeyZ => ('z', 'Z'),
        Key1 => ('1', '!'),
        Key2 => ('2', '@'),
        Key3 => ('3', '#'),
        Key4 => ('4', '$'),
        Key5 => ('5', '%'),
        Key6 => ('6', '^'),
        Key7 => ('7', '&'),
        Key8 => ('8', '*'),
        Key9 => ('9', '('),
        Key0 => ('0', ')'),
        KeyMinus => ('-', '_'),
        KeyEqual => ('=', '+'),
        KeyLeftbrace => ('[', '{'),
        KeyRightbrace => (']', '}'),
        KeyBackslash | Key102nd => ('\\', '|'),
        KeySemicolon => (';', ':'),
        KeyApostrophe => ('\'', '"'),
        KeyGrave => ('`', '~'),
        KeyComma => (',', '<'),
        KeyDot => ('.', '>'),
        KeySlash => ('/', '?'),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> Size<f64> {
        Size {
            width: 1000.0,
            height: 800.0,
        }
    }

    fn inputs(outputs: Vec<Output>) -> Vec<PlatformInput> {
        outputs
            .into_iter()
            .filter_map(|output| match output {
                Output::Input(input) => Some(input),
                Output::Release => None,
            })
            .collect()
    }

    fn key(code: Linux, pressed: bool) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: code as u32,
            state: pressed as u8,
        })
    }

    #[test]
    fn motion_moves_and_clamps_the_cursor() {
        let mut translator = Translator::new(size());
        translator.enter(Position::Left);
        let now = Instant::now();
        let outputs = translator.translate(
            Event::Pointer(PointerEvent::Motion {
                time: 0,
                dx: 100.0,
                dy: -10.0,
            }),
            now,
        );
        assert_eq!(translator.position(), Point::new(100.0, 390.0));
        assert!(matches!(inputs(outputs)[..], [PlatformInput::MouseMove(_)]));
        translator.translate(
            Event::Pointer(PointerEvent::Motion {
                time: 0,
                dx: 5000.0,
                dy: 5000.0,
            }),
            now,
        );
        assert_eq!(translator.position(), Point::new(999.0, 799.0));
    }

    #[test]
    fn pushing_past_the_entry_edge_releases() {
        let mut translator = Translator::new(size());
        translator.enter(Position::Left);
        let motion = |dx| {
            Event::Pointer(PointerEvent::Motion {
                time: 0,
                dx,
                dy: 0.0,
            })
        };
        let now = Instant::now();
        assert!(!translator.translate(motion(-10.0), now).iter().any(Output::is_release));
        assert!(translator.translate(motion(-30.0), now).iter().any(Output::is_release));
        // Pushing the opposite edge never releases.
        translator.enter(Position::Left);
        assert!(!translator.translate(motion(5000.0), now).iter().any(Output::is_release));
    }

    #[test]
    fn double_click_counts() {
        let mut translator = Translator::new(size());
        let start = Instant::now();
        let press = |translator: &mut Translator, pressed, at| {
            inputs(translator.translate(
                Event::Pointer(PointerEvent::Button {
                    time: 0,
                    button: BTN_LEFT,
                    state: pressed as u32,
                }),
                at,
            ))
        };
        press(&mut translator, true, start);
        press(&mut translator, false, start);
        let second = press(&mut translator, true, start + Duration::from_millis(200));
        match &second[..] {
            [PlatformInput::MouseDown(event)] => assert_eq!(event.click_count, 2),
            other => panic!("unexpected {other:?}"),
        }
        press(&mut translator, false, start + Duration::from_millis(250));
        let late = press(&mut translator, true, start + Duration::from_secs(2));
        match &late[..] {
            [PlatformInput::MouseDown(event)] => assert_eq!(event.click_count, 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn right_click_and_drag_report_the_button() {
        let mut translator = Translator::new(size());
        let now = Instant::now();
        translator.translate(
            Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_RIGHT,
                state: 1,
            }),
            now,
        );
        let moved = inputs(translator.translate(
            Event::Pointer(PointerEvent::Motion {
                time: 0,
                dx: 1.0,
                dy: 1.0,
            }),
            now,
        ));
        match &moved[..] {
            [PlatformInput::MouseMove(event)] => {
                assert_eq!(event.pressed_button, Some(MouseButton::Right))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn scrolling() {
        let mut translator = Translator::new(size());
        let now = Instant::now();
        let scrolled = inputs(translator.translate(
            Event::Pointer(PointerEvent::AxisDiscrete120 { axis: 0, value: 120 }),
            now,
        ));
        match &scrolled[..] {
            [PlatformInput::Scroll(event)] => {
                assert!(matches!(event.delta, ScrollDelta::Lines(lines) if lines == Point::new(0.0, -3.0)))
            }
            other => panic!("unexpected {other:?}"),
        }
        translator.invert_scroll = true;
        let scrolled = inputs(translator.translate(
            Event::Pointer(PointerEvent::Axis {
                time: 0,
                axis: 0,
                value: 10.0,
            }),
            now,
        ));
        match &scrolled[..] {
            [PlatformInput::Scroll(event)] => {
                assert!(matches!(event.delta, ScrollDelta::Pixels(pixels) if pixels == Point::new(px(0.), px(10.))))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn command_shortcuts_use_the_macos_keymap() {
        let mut translator = Translator::new(size());
        translator.translate(key(Linux::KeyLeftMeta, true), Instant::now());
        let typed = inputs(translator.translate(key(Linux::KeyS, true), Instant::now()));
        match &typed[..] {
            [PlatformInput::KeyDown(event)] => {
                assert!(event.keystroke.modifiers.platform);
                assert_eq!(event.keystroke.key, "s");
                assert_eq!(event.keystroke.key_char, None);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn typing_produces_characters() {
        let shifted = keystroke_for(Linux::Key1, Modifiers::shift(), false).unwrap();
        assert_eq!(shifted.key, "!");
        assert!(!shifted.modifiers.shift);
        assert_eq!(shifted.key_char.as_deref(), Some("!"));

        let capital = keystroke_for(Linux::KeyA, Modifiers::shift(), false).unwrap();
        assert_eq!(capital.key, "a");
        assert!(capital.modifiers.shift);
        assert_eq!(capital.key_char.as_deref(), Some("A"));

        let caps = keystroke_for(Linux::KeyA, Modifiers::none(), true).unwrap();
        assert_eq!(caps.key_char.as_deref(), Some("A"));

        let enter = keystroke_for(Linux::KeyEnter, Modifiers::none(), false).unwrap();
        assert_eq!((enter.key.as_str(), enter.key_char.as_deref()), ("enter", Some("\n")));
    }

    #[test]
    fn xkb_modifier_masks() {
        let mut translator = Translator::new(size());
        let changed = inputs(translator.translate(
            Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 1 | (1 << 6),
                latched: 0,
                locked: 0,
                group: 0,
            }),
            Instant::now(),
        ));
        match &changed[..] {
            [PlatformInput::ModifiersChanged(event)] => {
                assert!(event.modifiers.shift && event.modifiers.platform);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn leaving_releases_held_buttons_and_modifiers() {
        let mut translator = Translator::new(size());
        let now = Instant::now();
        translator.translate(key(Linux::KeyLeftShift, true), now);
        translator.translate(
            Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_LEFT,
                state: 1,
            }),
            now,
        );
        let released = inputs(translator.leave());
        assert!(matches!(
            released[..],
            [PlatformInput::MouseUp(_), PlatformInput::ModifiersChanged(_)]
        ));
    }
}
