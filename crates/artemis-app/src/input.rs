use std::collections::BTreeSet;

use artemis_moonlight::{
    BUTTON_ACTION_PRESS, BUTTON_ACTION_RELEASE, KEY_ACTION_DOWN, KEY_ACTION_UP, MODIFIER_ALT,
    MODIFIER_CTRL, MODIFIER_META, MODIFIER_SHIFT, MOUSE_LEFT, MOUSE_MIDDLE, MOUSE_RIGHT, MOUSE_X1,
    MOUSE_X2, Session,
};
use eframe::egui::{self, Event, Key, Modifiers, PointerButton};

pub struct InputRouter {
    pressed_keys: BTreeSet<i16>,
    pressed_buttons: BTreeSet<i32>,
}

impl InputRouter {
    pub fn new() -> Self {
        Self {
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
        }
    }

    pub fn forward(
        &mut self,
        context: &egui::Context,
        session: &mut Session,
        suppress_escape: bool,
    ) {
        let events = context.input(|input| input.raw.events.clone());
        for event in events {
            match event {
                Event::MouseMoved(delta) => {
                    let x = rounded_i16(delta.x);
                    let y = rounded_i16(delta.y);
                    if x != 0 || y != 0 {
                        let _ = session.mouse_move(x, y);
                    }
                }
                Event::PointerButton {
                    button, pressed, ..
                } => {
                    let button = mouse_button(button);
                    if pressed {
                        self.pressed_buttons.insert(button);
                    } else {
                        self.pressed_buttons.remove(&button);
                    }
                    let action = if pressed {
                        BUTTON_ACTION_PRESS
                    } else {
                        BUTTON_ACTION_RELEASE
                    };
                    let _ = session.mouse_button(action, button);
                }
                Event::MouseWheel { delta, .. } => {
                    let vertical = rounded_i16(-delta.y * 120.0);
                    let horizontal = rounded_i16(-delta.x * 120.0);
                    if vertical != 0 || horizontal != 0 {
                        let _ = session.scroll(vertical, horizontal);
                    }
                }
                Event::Key {
                    key,
                    physical_key,
                    pressed,
                    repeat,
                    modifiers,
                } => {
                    if repeat {
                        continue;
                    }
                    let key = physical_key.unwrap_or(key);
                    if is_local_shortcut(key, suppress_escape) {
                        continue;
                    }
                    if let Some(key) = virtual_key(key) {
                        if pressed {
                            self.pressed_keys.insert(key);
                        } else {
                            self.pressed_keys.remove(&key);
                        }
                        let action = if pressed {
                            KEY_ACTION_DOWN
                        } else {
                            KEY_ACTION_UP
                        };
                        let _ = session.keyboard(key, action, modifier_flags(modifiers));
                    }
                }
                Event::WindowFocused(false) => self.release_all(session),
                _ => {}
            }
        }
    }

    pub fn release_all(&mut self, session: &mut Session) {
        for key in std::mem::take(&mut self.pressed_keys) {
            let _ = session.keyboard(key, KEY_ACTION_UP, 0);
        }
        for button in std::mem::take(&mut self.pressed_buttons) {
            let _ = session.mouse_button(BUTTON_ACTION_RELEASE, button);
        }
    }
}

fn is_local_shortcut(key: Key, suppress_escape: bool) -> bool {
    matches!(key, Key::F10 | Key::F11) || (suppress_escape && key == Key::Escape)
}

#[allow(clippy::cast_possible_truncation)]
fn rounded_i16(value: f32) -> i16 {
    value
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

fn modifier_flags(modifiers: Modifiers) -> u8 {
    (if modifiers.shift { MODIFIER_SHIFT } else { 0 })
        | (if modifiers.ctrl { MODIFIER_CTRL } else { 0 })
        | (if modifiers.alt { MODIFIER_ALT } else { 0 })
        | (if modifiers.mac_cmd { MODIFIER_META } else { 0 })
}

fn mouse_button(button: PointerButton) -> i32 {
    match button {
        PointerButton::Primary => MOUSE_LEFT,
        PointerButton::Secondary => MOUSE_RIGHT,
        PointerButton::Middle => MOUSE_MIDDLE,
        PointerButton::Extra1 => MOUSE_X1,
        PointerButton::Extra2 => MOUSE_X2,
    }
}

#[allow(clippy::match_same_arms)]
fn virtual_key(key: Key) -> Option<i16> {
    Some(match key {
        Key::Backspace => 0x08,
        Key::Tab => 0x09,
        Key::Enter => 0x0D,
        Key::Escape => 0x1B,
        Key::Space => 0x20,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::End => 0x23,
        Key::Home => 0x24,
        Key::ArrowLeft => 0x25,
        Key::ArrowUp => 0x26,
        Key::ArrowRight => 0x27,
        Key::ArrowDown => 0x28,
        Key::Insert => 0x2D,
        Key::Delete => 0x2E,
        Key::Num0 => 0x30,
        Key::Num1 => 0x31,
        Key::Num2 => 0x32,
        Key::Num3 => 0x33,
        Key::Num4 => 0x34,
        Key::Num5 => 0x35,
        Key::Num6 => 0x36,
        Key::Num7 => 0x37,
        Key::Num8 => 0x38,
        Key::Num9 => 0x39,
        Key::A => 0x41,
        Key::B => 0x42,
        Key::C => 0x43,
        Key::D => 0x44,
        Key::E => 0x45,
        Key::F => 0x46,
        Key::G => 0x47,
        Key::H => 0x48,
        Key::I => 0x49,
        Key::J => 0x4A,
        Key::K => 0x4B,
        Key::L => 0x4C,
        Key::M => 0x4D,
        Key::N => 0x4E,
        Key::O => 0x4F,
        Key::P => 0x50,
        Key::Q => 0x51,
        Key::R => 0x52,
        Key::S => 0x53,
        Key::T => 0x54,
        Key::U => 0x55,
        Key::V => 0x56,
        Key::W => 0x57,
        Key::X => 0x58,
        Key::Y => 0x59,
        Key::Z => 0x5A,
        Key::F1 => 0x70,
        Key::F2 => 0x71,
        Key::F3 => 0x72,
        Key::F4 => 0x73,
        Key::F5 => 0x74,
        Key::F6 => 0x75,
        Key::F7 => 0x76,
        Key::F8 => 0x77,
        Key::F9 => 0x78,
        Key::F10 => 0x79,
        Key::F11 => 0x7A,
        Key::F12 => 0x7B,
        Key::Semicolon | Key::Colon => 0xBA,
        Key::Equals | Key::Plus => 0xBB,
        Key::Comma => 0xBC,
        Key::Minus => 0xBD,
        Key::Period => 0xBE,
        Key::Slash | Key::Questionmark => 0xBF,
        Key::Backtick => 0xC0,
        Key::OpenBracket | Key::OpenCurlyBracket => 0xDB,
        Key::Backslash | Key::Pipe => 0xDC,
        Key::CloseBracket | Key::CloseCurlyBracket => 0xDD,
        Key::Quote => 0xDE,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use eframe::egui::Key;

    use super::is_local_shortcut;

    #[test]
    fn local_stream_shortcuts_are_not_forwarded_to_the_host() {
        assert!(is_local_shortcut(Key::F10, false));
        assert!(is_local_shortcut(Key::F11, false));
        assert!(is_local_shortcut(Key::Escape, true));
        assert!(!is_local_shortcut(Key::Escape, false));
        assert!(!is_local_shortcut(Key::F12, true));
    }
}
