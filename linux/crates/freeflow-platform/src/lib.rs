//! Linux desktop integration: audio capture, compositor context, clipboard,
//! and text injection.
//!
//! Everything here is Wayland- and PipeWire-specific and therefore cannot be
//! unit tested without a session. Logic worth testing lives in
//! `freeflow-core`; this crate keeps the untestable parts thin and behind
//! narrow interfaces.

pub mod audio;
pub mod clipboard;
pub mod context;
pub mod inject;
pub mod keywatch;
