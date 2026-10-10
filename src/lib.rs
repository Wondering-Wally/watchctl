//! Client/daemon plumbing for driving the apple_watch_connector Linux backend
//! (`watch_transport_linux::LinuxBackend`) from the command line.
pub mod daemon;
pub mod exit;
pub mod format;
pub mod ipc;
pub mod paths;
