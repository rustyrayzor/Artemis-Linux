//! Portable `GameStream` control plane for Artemis Linux.

mod discovery;
mod error;
mod host;
mod http;
mod identity;
mod pairing;
mod protocol;
mod store;

pub use discovery::{DiscoveredHost, discover};
pub use error::{Error, Result};
pub use host::{Application, HostAddress, ServerInfo};
pub use http::NvClient;
pub use identity::ClientIdentity;
pub use pairing::{PairingOutcome, generate_pin, pair};
pub use protocol::{
    LaunchResult, RemoteInputKey, StreamProfile, cancel_host_application, launch_application,
    list_applications, stereo_audio_configuration,
};
pub use store::{HostRecord, HostStore};
