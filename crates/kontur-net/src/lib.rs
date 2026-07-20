pub mod protocol;
pub mod codec;
pub mod server;
pub mod agent;
pub mod client;
pub mod agent_endpoint;
pub mod tls;

pub use protocol::{
    ClientMsg, ServerMsg, WireRole, WireSeat, WirePhase, WireFleetCard, WireGate, WireState,
};
pub use codec::{write_json, read_json};
pub use server::{SessionConfig, SessionServer, ScriptedAgent, ScriptedTask};
pub use client::{SessionClient, SystemClock};
pub use agent_endpoint::serve_agent_endpoint;
pub use tls::{SessionTls, generate as generate_tls, connect_pinned, attach_tls, fp_hex, parse_fp_hex};
