//! FlowCursor agent library (the binary is a thin CLI around it; tests and
//! the latency bench embed it with the simulated backend).

pub mod backend;
pub mod config;
pub mod core;
pub mod edge;
pub mod hub;

pub use crate::core::{start, AgentHandle, AgentStatus, LogHook};
