use std::io;

/// Errors surfaced by the Artemis control plane.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("TLS or certificate error: {0}")]
    Tls(#[from] openssl::error::ErrorStack),
    #[error("XML error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("invalid host response: {0}")]
    InvalidResponse(String),
    #[error("host returned status {code}: {message}")]
    HostStatus { code: i64, message: String },
    #[error("HTTP transport error: {0}")]
    Http(String),
    #[error("pairing failed: {0}")]
    Pairing(String),
    #[error("host is not paired with this client")]
    NotPaired,
    #[error("application {0} is not available on the host")]
    ApplicationNotFound(i32),
    #[error("another application ({0}) is already running")]
    AnotherApplicationRunning(i32),
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("mDNS error: {0}")]
    Discovery(String),
}

pub type Result<T> = std::result::Result<T, Error>;
