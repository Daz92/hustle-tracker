pub mod monitor;
pub mod parser;
pub mod windows_inspection;
pub mod macos_inspection;
#[cfg(target_os = "linux")]
pub mod idle_wayland;
