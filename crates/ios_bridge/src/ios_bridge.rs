//! Device services for the iOS port of Zed: container paths, logging, memory
//! accounting, and the strict Simulator mode that makes the Simulator fail the way
//! an iPhone would.
//!
//! Everything here also builds on macOS (and Linux, with reduced functionality) so
//! the logic can be unit-tested on the CI runner without a device.

pub mod device;
pub mod entitlements;
pub mod jit;
pub mod logging;
pub mod memory;
pub mod paths;
pub mod status;
pub mod strict;
