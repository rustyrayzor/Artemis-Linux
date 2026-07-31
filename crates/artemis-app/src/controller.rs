#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ControllerPreferences {
    pub swap_face_buttons: bool,
    pub force_gamepad_one: bool,
    pub background_input: bool,
}

#[cfg(target_os = "linux")]
pub use linux::ControllerManager;
#[cfg(not(target_os = "linux"))]
pub use unsupported::ControllerManager;
