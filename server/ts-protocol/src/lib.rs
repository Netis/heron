pub mod de;
pub mod flow;
pub mod http;
pub mod model;
pub mod net;
pub mod stage;
pub mod tcp;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("parse error: {0}")]
    Parse(String),

    #[error("channel closed")]
    ChannelClosed,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

pub use flow::WorkerInput;
pub use stage::{spawn_flow_dispatcher, spawn_protocol_stage};
