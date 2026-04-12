/// Custom error type for the RESP server modules.
///
/// Replaces `Box<dyn std::error::Error>` to preserve error context
/// and enable structured error handling.
#[derive(Debug, thiserror::Error)]
pub enum RespError {
    #[error("bind failed: {0}")]
    Bind(#[source] std::io::Error),
    #[error("accept failed: {0}")]
    Accept(#[source] std::io::Error),
    #[error("connection read error: {0}")]
    Read(#[source] std::io::Error),
    #[error("connection write error: {0}")]
    Write(#[source] std::io::Error),
    #[error("io_uring submit error: {0}")]
    Submit(#[source] std::io::Error),
}
