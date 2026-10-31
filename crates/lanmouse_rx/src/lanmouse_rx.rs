//! lan-mouse receiver for the iPhone (CLAUDE.md section 11).
//!
//! The laptop runs unmodified lan-mouse and adds the iPhone as a client. This
//! crate is the iPhone's side: lan-mouse's own DTLS listener and certificate
//! code ([`listen`], [`crypto`], copied from v0.11.0), a service that speaks the
//! protocol ([`service`]), and a translator to GPUI input ([`translate`]).

mod crypto;
#[allow(dead_code, reason = "copied verbatim from lan-mouse; port changes are unused")]
mod listen;
pub mod proto;
pub mod service;
pub mod translate;

pub use crate::proto::Position;
pub use service::{Config, DEFAULT_PORT, Receiver, ServiceEvent, Status};
pub use translate::{Output, Translator};
