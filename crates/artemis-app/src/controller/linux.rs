use artemis_moonlight::Session;
use gilrs::{Axis, Button, EventType, Gilrs};

use super::ControllerPreferences;

const UP: i32 = 0x0001;
const DOWN: i32 = 0x0002;
const LEFT: i32 = 0x0004;
const RIGHT: i32 = 0x0008;
const START: i32 = 0x0010;
const BACK: i32 = 0x0020;
const LEFT_STICK: i32 = 0x0040;
const RIGHT_STICK: i32 = 0x0080;
const LEFT_BUMPER: i32 = 0x0100;
const RIGHT_BUMPER: i32 = 0x0200;
const SPECIAL: i32 = 0x0400;
const A: i32 = 0x1000;
const B: i32 = 0x2000;
const X: i32 = 0x4000;
const Y: i32 = 0x8000;

#[derive(Default)]
struct State {
    buttons: i32,
    left_trigger: u8,
    right_trigger: u8,
    left_x: i16,
    left_y: i16,
    right_x: i16,
    right_y: i16,
}

pub struct ControllerManager {
    gilrs: Option<Gilrs>,
    connected: bool,
    state: State,
    preferences: ControllerPreferences,
}

impl ControllerManager {
    pub fn new(preferences: ControllerPreferences) -> Self {
        Self {
            gilrs: Gilrs::new().ok(),
            connected: false,
            state: State::default(),
            preferences,
        }
    }

    pub fn poll(&mut self, session: &mut Session, window_focused: bool) {
        if !window_focused && !self.preferences.background_input {
            return;
        }
        if self.preferences.force_gamepad_one && !self.connected {
            self.connected = session.controller_arrival().is_ok();
        }
        let Some(gilrs) = &mut self.gilrs else {
            return;
        };
        while let Some(event) = gilrs.next_event() {
            let mut changed = false;
            match event.event {
                EventType::Connected => {
                    if !self.connected {
                        self.connected = session.controller_arrival().is_ok();
                    }
                }
                EventType::Disconnected => {
                    if self.connected {
                        let _ = session.controller_departure();
                        self.connected = false;
                        self.state = State::default();
                    }
                }
                EventType::ButtonPressed(button, _) => {
                    if let Some(flag) = button_flag(button, self.preferences.swap_face_buttons) {
                        self.state.buttons |= flag;
                        changed = true;
                    }
                }
                EventType::ButtonReleased(button, _) => {
                    if let Some(flag) = button_flag(button, self.preferences.swap_face_buttons) {
                        self.state.buttons &= !flag;
                        changed = true;
                    }
                }
                EventType::ButtonChanged(button, value, _) => match button {
                    Button::LeftTrigger2 => {
                        self.state.left_trigger = trigger(value);
                        changed = true;
                    }
                    Button::RightTrigger2 => {
                        self.state.right_trigger = trigger(value);
                        changed = true;
                    }
                    _ => {}
                },
                EventType::AxisChanged(axis, value, _) => {
                    let converted = stick(value);
                    match axis {
                        Axis::LeftStickX => self.state.left_x = converted,
                        Axis::LeftStickY => self.state.left_y = converted.saturating_neg(),
                        Axis::RightStickX => self.state.right_x = converted,
                        Axis::RightStickY => self.state.right_y = converted.saturating_neg(),
                        Axis::LeftZ => self.state.left_trigger = trigger((value + 1.0) / 2.0),
                        Axis::RightZ => {
                            self.state.right_trigger = trigger((value + 1.0) / 2.0);
                        }
                        _ => continue,
                    }
                    changed = true;
                }
                _ => {}
            }
            if self.connected && changed {
                let _ = session.controller_state(
                    self.state.buttons,
                    self.state.left_trigger,
                    self.state.right_trigger,
                    self.state.left_x,
                    self.state.left_y,
                    self.state.right_x,
                    self.state.right_y,
                );
            }
        }
    }

    pub fn disconnect(&mut self, session: &mut Session) {
        if self.connected {
            let _ = session.controller_departure();
            self.connected = false;
        }
    }
}

fn button_flag(button: Button, swap_face_buttons: bool) -> Option<i32> {
    Some(match button {
        Button::South if swap_face_buttons => B,
        Button::East if swap_face_buttons => A,
        Button::West if swap_face_buttons => Y,
        Button::North if swap_face_buttons => X,
        Button::South => A,
        Button::East => B,
        Button::West => X,
        Button::North => Y,
        Button::DPadUp => UP,
        Button::DPadDown => DOWN,
        Button::DPadLeft => LEFT,
        Button::DPadRight => RIGHT,
        Button::Start => START,
        Button::Select => BACK,
        Button::Mode => SPECIAL,
        Button::LeftThumb => LEFT_STICK,
        Button::RightThumb => RIGHT_STICK,
        Button::LeftTrigger => LEFT_BUMPER,
        Button::RightTrigger => RIGHT_BUMPER,
        _ => return None,
    })
}

// These casts intentionally quantize a clamped normalized axis to Moonlight's wire format.
#[allow(clippy::cast_possible_truncation)]
fn stick(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn trigger(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * f32::from(u8::MAX)).round() as u8
}

#[cfg(test)]
mod tests {
    use super::{A, B, X, Y, button_flag};
    use gilrs::Button;

    #[test]
    fn face_button_swap_changes_both_button_pairs() {
        assert_eq!(button_flag(Button::South, false), Some(A));
        assert_eq!(button_flag(Button::South, true), Some(B));
        assert_eq!(button_flag(Button::West, false), Some(X));
        assert_eq!(button_flag(Button::West, true), Some(Y));
    }
}
