use std::fmt;
use std::io;

#[derive(Debug)]
#[non_exhaustive]
pub enum ApeError {
    Io(io::Error),
    InvalidFormat(&'static str),
    InvalidChecksum,
    UnsupportedVersion(u16),
    DecodingError(&'static str),
}

impl fmt::Display for ApeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApeError::Io(e) => write!(f, "I/O error: {}", e),
            ApeError::InvalidFormat(msg) => write!(f, "invalid format: {}", msg),
            ApeError::InvalidChecksum => write!(f, "invalid checksum"),
            ApeError::UnsupportedVersion(v) => write!(f, "unsupported version: {}", v),
            ApeError::DecodingError(msg) => write!(f, "decoding error: {}", msg),
        }
    }
}

impl std::error::Error for ApeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ApeError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ApeError {
    fn from(e: io::Error) -> Self {
        ApeError::Io(e)
    }
}

pub type ApeResult<T> = Result<T, ApeError>;
