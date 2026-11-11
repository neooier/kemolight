pub mod app;
pub mod brightness;
#[cfg(target_os = "windows")]
mod brightness_overlay;
pub mod config;
#[cfg(target_os = "windows")]
mod display_events;
pub mod logo;
pub mod profiles;
pub mod single_instance;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod startup;
pub mod tray;
pub mod ui;
#[cfg(target_os = "windows")]
pub mod ui_process;
