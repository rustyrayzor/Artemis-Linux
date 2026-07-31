use crate::{Error, EventReceiver, MediaIngressStats, NetworkStats, Result, StreamConfig};

/// Non-Linux compile-time placeholder.
pub struct Session;

#[allow(clippy::missing_errors_doc)]
impl Session {
    pub fn connect(_config: StreamConfig) -> Result<(Self, EventReceiver)> {
        Err(Error::UnsupportedPlatform)
    }

    pub fn stop(&mut self) {}

    pub fn interrupt(&mut self) {}

    pub fn mouse_move(&mut self, _x: i16, _y: i16) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    pub fn mouse_move_as_position(
        &mut self,
        _x: i16,
        _y: i16,
        _reference_width: i16,
        _reference_height: i16,
    ) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    pub fn mouse_button(&mut self, _action: u8, _button: i32) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    pub fn scroll(&mut self, _vertical: i16, _horizontal: i16) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    pub fn keyboard(&mut self, _key: i16, _action: u8, _modifiers: u8) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    pub fn controller_arrival(&mut self) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn controller_state(
        &mut self,
        _buttons: i32,
        _left_trigger: u8,
        _right_trigger: u8,
        _left_x: i16,
        _left_y: i16,
        _right_x: i16,
        _right_y: i16,
    ) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    pub fn controller_departure(&mut self) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    pub fn request_idr(&mut self) {}

    pub fn network_stats(&self) -> Result<NetworkStats> {
        Err(Error::UnsupportedPlatform)
    }

    #[must_use]
    pub fn media_ingress_stats(&self) -> MediaIngressStats {
        MediaIngressStats::default()
    }
}
