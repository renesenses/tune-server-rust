//! Extraction d'un CD audio vers la bibliothèque (#2466), en FLAC ou en WAV.
//!
//! ## Pourquoi un module de `tune-cd`, et pas un greffon voisin
//!
//! Le lecteur est UN objet physique. Le greffon `cd` en tient la seule
//! poignée (`LecteurBranchable`, qui suit le branchement à chaud et choisit
//! le lecteur qui a un disque) ; la lecture vers une zone, l'éjection et
//! l'extraction doivent se voir l'une l'autre : on n'éjecte pas un disque
//! qu'on extrait, on ne lance pas la lecture d'un disque qu'on extrait, on
//! n'extrait pas un disque qu'une zone joue. Un greffon voisin aurait ouvert
//! sa propre poignée, et les deux se seraient disputé le lecteur sans se
//! voir. La TOC, l'identifiant de disque et la consultation MusicBrainz (avec
//! sa mémoire) sont ceux du greffon, sans copie.
//!
//! ## Le chemin
//!
//! ```text
//! POST /extractions ─ routes.rs : corps validé, destination vérifiée,
//!        │                        plan des pistes et des chemins
//!        ▼
//! travail.rs (fil bloquant) : pour chaque piste
//!        lecture_sure.rs  : bloc lu, relu en cas de doute (deux lectures
//!                           concordantes), secteur par secteur, silence
//!        accuraterip.rs   : CRC v1 et v2 de la piste (sans comparaison)
//!        encodeur FLAC de tune-core, ou WAV écrit au fil de l'eau
//!        balises.rs       : titre, artistes, album, numéros, MBIDs, pochette
//!        NN - Titre.flac.part → NN - Titre.flac
//!        ▼
//! scan ciblé du dossier de l'album (`ScanCible`, fourni par l'hôte)
//! ```
//!
//! Aucun sous-processus : ni cdparanoia, ni ffmpeg.

pub mod accuraterip;
pub mod balises;
pub mod destination;
pub mod lecture_sure;
pub mod routes;
#[cfg(test)]
mod tests;
pub mod travail;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tune_core::db::backend::DbBackend;
use tune_core::event_bus::EventBus;

pub use lecture_sure::Verification;

/// Ce que l'hôte fournit pour faire entrer les fichiers dans la
/// bibliothèque : un scan CIBLÉ du dossier (`POST /system/scan?path=`).
#[async_trait]
pub trait ScanCible: Send + Sync {
    /// Lance le scan de `dossier`. `false` : un scan tourne déjà, celui-ci
    /// n'est pas lancé (la surveillance des dossiers le verra).
    async fn scanner(&self, dossier: String) -> bool;
}

/// Les deux formats de cette version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Flac,
    Wav,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Flac => "flac",
            Format::Wav => "wav",
        }
    }

    pub fn depuis(texte: &str) -> Option<Format> {
        match texte {
            "flac" => Some(Format::Flac),
            "wav" => Some(Format::Wav),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatutTravail {
    EnCours,
    Terminee,
    Echec,
    Annulee,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatutPiste {
    EnAttente,
    Extraction,
    Ecriture,
    Terminee,
    Echec,
    Annulee,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErreurTravail {
    pub code: &'static str,
    pub message: String,
}

impl ErreurTravail {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// L'avancement d'une piste, tel que l'écran le lit.
#[derive(Debug, Clone, Serialize)]
pub struct EtatPiste {
    pub numero: u8,
    pub titre: String,
    pub statut: StatutPiste,
    pub secteurs: u32,
    pub secteurs_lus: u32,
    pub pourcentage: f32,
    /// Lectures faites EN PLUS de la première, pour vérifier ou reprendre.
    pub lectures_supplementaires: u32,
    /// Secteurs restés illisibles, remplacés par du silence.
    pub secteurs_illisibles: u32,
    /// CRC AccurateRip v1 et v2 de la piste, en hexadécimal (8 chiffres),
    /// calculés sur les secteurs lus SANS correction du décalage du lecteur.
    pub accuraterip_v1: Option<String>,
    pub accuraterip_v2: Option<String>,
    /// Chemin absolu du fichier écrit.
    pub fichier: Option<String>,
    pub erreur: Option<String>,
}

/// L'état complet d'une extraction : la réponse des routes ET la charge
/// des évènements `cd.extraction.*`.
#[derive(Debug, Clone, Serialize)]
pub struct EtatTravail {
    pub id: String,
    pub statut: StatutTravail,
    pub format: Format,
    pub verification: Verification,
    pub lecteur: String,
    pub disc_id: String,
    /// `musicbrainz` ou `repli` (« Piste NN »).
    pub metadonnees: &'static str,
    pub artiste: String,
    pub album: String,
    pub disque: u32,
    pub disques: u32,
    /// L'emplacement de la bibliothèque choisi.
    pub destination: String,
    /// Le dossier de l'album, sous la destination.
    pub dossier: String,
    pub pistes: Vec<EtatPiste>,
    pub piste_courante: Option<u8>,
    pub pourcentage: f32,
    /// Secondes Unix.
    pub debut: i64,
    pub fin: Option<i64>,
    pub erreur: Option<ErreurTravail>,
    /// Suite donnée au scan ciblé : `lance`, `deja_en_cours`,
    /// `indisponible` (hôte sans scan) ou `non_lance` (aucun fichier écrit).
    pub scan: Option<&'static str>,
}

impl EtatTravail {
    /// Recalcule le pourcentage global à partir des secteurs lus.
    pub fn recalculer(&mut self) {
        let total: u64 = self.pistes.iter().map(|p| p.secteurs as u64).sum();
        let lus: u64 = self.pistes.iter().map(|p| p.secteurs_lus as u64).sum();
        self.pourcentage = if total == 0 {
            0.0
        } else {
            arrondi(lus as f64 * 100.0 / total as f64)
        };
    }
}

pub(crate) fn arrondi(p: f64) -> f32 {
    ((p * 10.0).round() / 10.0) as f32
}

/// Une extraction : son état et son drapeau d'annulation.
pub struct Travail {
    pub id: String,
    annulation: AtomicBool,
    etat: Mutex<EtatTravail>,
}

impl Travail {
    pub fn new(etat: EtatTravail) -> Self {
        Self {
            id: etat.id.clone(),
            annulation: AtomicBool::new(false),
            etat: Mutex::new(etat),
        }
    }

    pub fn annuler(&self) {
        self.annulation.store(true, Ordering::SeqCst);
    }

    pub fn annulee(&self) -> bool {
        self.annulation.load(Ordering::SeqCst)
    }

    pub fn etat(&self) -> EtatTravail {
        self.etat.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn modifier<R>(&self, f: impl FnOnce(&mut EtatTravail) -> R) -> R {
        f(&mut self.etat.lock().unwrap_or_else(|p| p.into_inner()))
    }

    pub fn en_cours(&self) -> bool {
        self.modifier(|e| e.statut == StatutTravail::EnCours)
    }
}

/// Combien d'extractions FINIES restent consultables.
pub const HISTORIQUE: usize = 20;

/// Les noms d'évènements, sur le bus puis sur le WebSocket.
pub const EVT_DEMARREE: &str = "cd.extraction.demarree";
pub const EVT_PROGRESSION: &str = "cd.extraction.progression";
pub const EVT_TERMINEE: &str = "cd.extraction.terminee";

/// Les extractions du serveur (une seule à la fois) et ce qu'il leur faut.
pub struct Extractions {
    pub backend: Arc<dyn DbBackend>,
    pub bus: Option<EventBus>,
    pub scan: Option<Arc<dyn ScanCible>>,
    pub pochettes: Arc<dyn routes::Pochettes>,
    travaux: Mutex<Vec<Arc<Travail>>>,
}

impl Extractions {
    pub fn new(
        backend: Arc<dyn DbBackend>,
        bus: Option<EventBus>,
        scan: Option<Arc<dyn ScanCible>>,
    ) -> Self {
        Self {
            backend,
            bus,
            scan,
            pochettes: Arc::new(routes::CoverArtArchive),
            travaux: Mutex::new(Vec::new()),
        }
    }

    /// Une autre source de pochettes (témoins).
    pub fn avec_pochettes(mut self, p: Arc<dyn routes::Pochettes>) -> Self {
        self.pochettes = p;
        self
    }

    fn liste_verrouillee(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Travail>>> {
        self.travaux.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// L'extraction en cours, s'il y en a une.
    pub fn en_cours(&self) -> Option<Arc<Travail>> {
        self.liste_verrouillee()
            .iter()
            .find(|t| t.en_cours())
            .cloned()
    }

    /// Inscrit `t`, sauf si une autre extraction tourne (rendue alors).
    /// Vérification et inscription sous le même verrou : deux `POST`
    /// simultanés n'en lancent pas deux.
    pub fn inscrire(&self, t: Arc<Travail>) -> Result<(), Arc<Travail>> {
        let mut v = self.liste_verrouillee();
        if let Some(autre) = v.iter().find(|t| t.en_cours()) {
            return Err(autre.clone());
        }
        v.push(t);
        // Garde les HISTORIQUE plus récentes parmi les finies.
        while v.len() > HISTORIQUE + 1 {
            if let Some(i) = v.iter().position(|t| !t.en_cours()) {
                v.remove(i);
            } else {
                break;
            }
        }
        Ok(())
    }

    pub fn trouver(&self, id: &str) -> Option<Arc<Travail>> {
        self.liste_verrouillee()
            .iter()
            .find(|t| t.id == id)
            .cloned()
    }

    /// Les extractions connues, la plus récente d'abord.
    pub fn liste(&self) -> Vec<EtatTravail> {
        self.liste_verrouillee()
            .iter()
            .rev()
            .map(|t| t.etat())
            .collect()
    }

    pub fn publier(&self, evenement: &str, etat: &EtatTravail) {
        if let Some(bus) = &self.bus {
            bus.emit(
                evenement,
                serde_json::to_value(etat).unwrap_or(serde_json::Value::Null),
            );
        }
    }
}
