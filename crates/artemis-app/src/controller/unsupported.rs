use artemis_moonlight::Session;

use super::ControllerPreferences;

pub struct ControllerManager;

#[allow(clippy::unused_self)]
impl ControllerManager {
    pub fn new(_preferences: ControllerPreferences) -> Self {
        Self
    }

    pub fn poll(&mut self, _session: &mut Session, _window_focused: bool) {}

    pub fn disconnect(&mut self, _session: &mut Session) {}
}
