#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

#[cfg(target_os = "linux")]
pub use linux::ControllerManager;
#[cfg(not(target_os = "linux"))]
pub use unsupported::ControllerManager;
