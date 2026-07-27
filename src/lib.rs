//! Reusable Ferry modules for the desktop workbench and integration tests.
//!
//! The CLI keeps its own small binary entry point; this library exposes the
//! same device, PTY, serial, and WebSocket primitives without duplicating them.

pub mod adbx;
pub mod all;
pub mod blackbox;
pub mod config;
pub mod doctor;
pub mod fingerprint;
pub mod fwd;
pub mod hash;
pub mod httpd;
pub mod jsonout;
pub mod logs;
pub mod mdns;
pub mod netdiag;
pub mod proxyd;
pub mod pty;
pub mod plugins;
pub mod runx;
pub mod scan;
pub mod serialx;
pub mod serve;
pub mod share;
pub mod sshx;
pub mod sync;
pub mod tomlite;
pub mod ui;
pub mod up;
pub mod usbnet;
pub mod util;
pub mod watchd;
pub mod wsutil;
pub mod xfer;
