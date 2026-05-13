//! midi-pages: virtual paging proxy for grid MIDI controllers.

pub mod config;
pub mod midi;
pub mod ports;
pub mod proxy;

#[cfg(target_os = "windows")]
pub mod shutdown;

#[cfg(target_os = "windows")]
pub mod wms_bindings;
