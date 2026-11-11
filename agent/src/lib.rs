//! FlowCursor agent library (the binary is a thin CLI around it; tests and
//! the latency bench embed it with the simulated backend).

pub mod backend;
pub mod config;
pub mod core;
pub mod cross;
pub mod edge;
pub mod hub;
pub mod logbuf;
pub mod netcheck;

pub use crate::core::{start, start_with, AgentHandle, AgentStatus, LogHook, StartOptions};
