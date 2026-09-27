use thiserror::Error;

#[derive(Error, Debug)]
pub enum NzbError {
    #[error("NZB parse error: {0}")]
    ParseError(String),

    #[error("Invalid NZB: {0}")]
    InvalidNzb(String),

    #[error("Job not found: {0}")]
    JobNotFound(String),

    #[error("Idempotency key is already bound to another payload")]
    AdmissionConflict,

    #[error("Server not found: {0}")]
    ServerNotFound(String),

    #[error("Category not found: {0}")]
    CategoryNotFound(String),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("NNTP error: {0}")]
    Nntp(String),

    #[error("Decode error: {0}")]
    Decode(String),

    #[error("Post-processing error: {0}")]
    PostProc(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, NzbError>;
