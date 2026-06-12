use thiserror::Error;

#[derive(Error, Debug)]
pub enum TuneError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Database error: {0}")]
    Db(String),

    #[error("Streaming service error: {0}")]
    Streaming(String),

    #[error("Audio error: {0}")]
    Audio(String),

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

impl From<String> for TuneError {
    fn from(s: String) -> Self {
        TuneError::Other(s)
    }
}

impl From<&str> for TuneError {
    fn from(s: &str) -> Self {
        TuneError::Other(s.to_string())
    }
}

impl From<rusqlite::Error> for TuneError {
    fn from(e: rusqlite::Error) -> Self {
        TuneError::Db(e.to_string())
    }
}
