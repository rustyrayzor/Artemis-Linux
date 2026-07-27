#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

#[cfg(target_os = "linux")]
pub use linux::{DecodedFrame, MediaRuntime};
#[cfg(not(target_os = "linux"))]
pub use unsupported::{DecodedFrame, MediaRuntime};
