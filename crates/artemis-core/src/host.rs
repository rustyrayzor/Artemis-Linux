use serde::{Deserialize, Serialize};

/// Address and control-plane port for an Apollo or Sunshine host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HostAddress {
    pub host: String,
    pub http_port: u16,
}

impl HostAddress {
    #[must_use]
    pub fn new(host: impl Into<String>, http_port: u16) -> Self {
        Self {
            host: host.into(),
            http_port,
        }
    }
}

/// Parsed `/serverinfo` fields needed by the first streaming slice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerInfo {
    pub name: String,
    pub unique_id: String,
    pub app_version: String,
    pub gfe_version: Option<String>,
    pub pair_status: bool,
    pub https_port: u16,
    pub current_game: i32,
    pub codec_mode_support: i32,
    pub state: String,
}

/// An application advertised by `/applist`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Application {
    pub id: i32,
    pub uuid: Option<String>,
    pub title: String,
    pub hdr_supported: bool,
}
