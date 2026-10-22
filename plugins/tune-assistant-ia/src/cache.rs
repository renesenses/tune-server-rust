//! Le cache des réponses : ne jamais payer deux fois la même question.
//!
//! La clé est un SHA-256 de l'usage, de la famille, du modèle, de la version
//! des consignes et de l'entrée NORMALISÉE (espaces repliés, minuscules pour
//! une phrase). La valeur est la réponse BRUTE du modèle : elle est revalidée à
//! chaque lecture, contre l'état de la base à ce moment-là — une suggestion
//! mise en cache avant qu'un album soit identifié par MusicBrainz ne passe pas
//! la garde après.
//!
//! Un fichier par entrée, dans le dossier de données du greffon. Au-delà de
//! [`ENTREES_MAX`], les plus anciennes sont retirées.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

pub const ENTREES_MAX: usize = 500;

pub struct Cache {
    dossier: PathBuf,
}

/// Une phrase normalisée : minuscules, espaces repliés, sans blancs autour.
pub fn normaliser_phrase(phrase: &str) -> String {
    phrase
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// La clé d'une entrée.
pub fn cle(parties: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parties {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

impl Cache {
    pub fn new(dossier: PathBuf) -> Self {
        Self { dossier }
    }

    fn chemin(&self, cle: &str) -> PathBuf {
        self.dossier.join(format!("{cle}.txt"))
    }

    pub fn lire(&self, cle: &str) -> Option<String> {
        std::fs::read_to_string(self.chemin(cle)).ok()
    }

    pub fn ecrire(&self, cle: &str, valeur: &str) {
        if std::fs::create_dir_all(&self.dossier).is_err() {
            return;
        }
        if std::fs::write(self.chemin(cle), valeur).is_ok() {
            self.elaguer();
        }
    }

    fn elaguer(&self) {
        let Ok(entrees) = std::fs::read_dir(&self.dossier) else {
            return;
        };
        let mut fichiers: Vec<(std::time::SystemTime, PathBuf)> = entrees
            .filter_map(Result::ok)
            .filter_map(|e| {
                let m = e.metadata().ok()?.modified().ok()?;
                Some((m, e.path()))
            })
            .collect();
        if fichiers.len() <= ENTREES_MAX {
            return;
        }
        fichiers.sort();
        let trop = fichiers.len() - ENTREES_MAX;
        for (_, p) in fichiers.into_iter().take(trop) {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deux_ecritures_d_une_meme_phrase_ont_la_meme_cle() {
        let a = normaliser_phrase("  Jazz calme   des années 60 ");
        let b = normaliser_phrase("jazz CALME des années 60");
        assert_eq!(a, b);
        assert_eq!(cle(&["playlist", "m", &a]), cle(&["playlist", "m", &b]));
        // Le modèle fait partie de la clé.
        assert_ne!(cle(&["playlist", "m1", &a]), cle(&["playlist", "m2", &a]));
        // Les parties ne se recollent pas : (« ab », « c ») ≠ (« a », « bc »).
        assert_ne!(cle(&["ab", "c"]), cle(&["a", "bc"]));
    }

    #[test]
    fn ecrire_puis_lire() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::new(dir.path().join("cache"));
        assert_eq!(c.lire("k"), None);
        c.ecrire("k", "réponse");
        assert_eq!(c.lire("k").as_deref(), Some("réponse"));
    }
}
