pub mod agent;
pub mod agent_endpoint;
pub mod clarify;
pub mod client;
pub mod codec;
pub mod difftext;
pub mod protocol;
pub mod server;
pub mod tls;

pub use agent_endpoint::serve_agent_endpoint;
pub use client::{SessionClient, SystemClock};
pub use codec::{read_json, write_json};
pub use protocol::{
    ClientMsg, ServerMsg, WireFleetCard, WireGate, WirePhase, WireRole, WireSeat, WireState,
};
pub use server::{ScriptedAgent, ScriptedTask, SessionConfig, SessionServer};
pub use tls::{
    attach_tls, connect_pinned, fp_hex, generate as generate_tls, parse_fp_hex, SessionTls,
};
