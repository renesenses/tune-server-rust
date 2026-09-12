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

    /// Le service a répondu — en refusant. **Ce n'est pas une panne.**
    ///
    /// « Bandcamp ne fournit pas de playlists » n'est pas « la passerelle est
    /// en panne » : la frontière HTTP doit pouvoir les distinguer, et un
    /// message libre rangé dans [`TuneError::Other`] ne le permet pas.
    ///
    /// 🔴 #859 — mesuré sur le .18 le 12/09/2026 :
    /// `GET /api/v1/streaming/bandcamp/playlists` sortait en **502 Bad
    /// Gateway** en 4 ms, corps « Bandcamp ne fournit pas de playlists ».
    /// Quatre millisecondes : aucun aller-retour réseau n'avait eu lieu. Un
    /// testeur lisait « Bad Gateway » et signalait une panne serveur ; on
    /// cherchait une passerelle qui n'avait jamais été sollicitée.
    ///
    /// Porte le refus DÉLIBÉRÉ d'un connecteur : la fonctionnalité n'existe
    /// pas chez ce service, la requête était pourtant recevable. Le texte
    /// reste celui du refus, sans préfixe — le corps de la réponse HTTP ne
    /// change pas d'un octet, seul son statut change.
    #[error("{0}")]
    Unsupported(String),

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

// ── Bridge: TuneError → String ──────────────────────────────────
// This allows code using TuneError internally to convert back to String
// at API boundaries where callers still expect `Result<T, String>`.
// Pattern: `fn foo() -> Result<T, String> { inner().map_err(|e: TuneError| e.to_string()) }`
impl From<TuneError> for String {
    fn from(e: TuneError) -> Self {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tune_error_from_string() {
        let err = TuneError::from("something failed".to_string());
        assert_eq!(err.to_string(), "something failed");
    }

    #[test]
    fn tune_error_from_str() {
        let err = TuneError::from("io broke");
        assert_eq!(err.to_string(), "io broke");
    }

    #[test]
    fn tune_error_into_string() {
        let err = TuneError::Db("connection lost".into());
        let s: String = err.into();
        assert_eq!(s, "Database error: connection lost");
    }

    #[test]
    fn tune_error_io_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err = TuneError::from(io_err);
        assert!(err.to_string().contains("file missing"));
    }

    #[test]
    fn tune_error_roundtrip_string() {
        // String → TuneError → String — the bridge pattern
        let original = "network timeout".to_string();
        let err: TuneError = original.clone().into();
        let back: String = err.into();
        assert_eq!(back, original);
    }

    #[test]
    fn tune_error_not_found_into_string() {
        let err = TuneError::NotFound("track 42".into());
        let s: String = err.into();
        assert_eq!(s, "Not found: track 42");
    }
}
