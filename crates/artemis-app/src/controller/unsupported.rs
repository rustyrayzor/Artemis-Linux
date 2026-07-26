use artemis_moonlight::Session;

pub struct ControllerManager;

#[allow(clippy::unused_self)]
impl ControllerManager {
    pub fn new() -> Self {
        Self
    }

    pub fn poll(&mut self, _session: &mut Session) {}

    pub fn disconnect(&mut self, _session: &mut Session) {}
}
