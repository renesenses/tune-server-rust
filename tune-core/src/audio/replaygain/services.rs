//! #8689 — le ReplayGain que Qobuz et TIDAL publient pour leurs titres.
//!
//! Jusqu'ici le gain ne venait que de `track_metadata`, donc des seules
//! pistes de la bibliothèque : un titre Qobuz ou TIDAL jouait à facteur 1,0,
//! pré-gain compris (« Qobuz is too loud by default, and tracks are
//! inconsistent », fil forum 2801). Or les deux services publient leurs
//! mesures dans des réponses que Tune lit DÉJÀ :
//!
//! * Qobuz : `audio_info.replaygain_track_gain` / `replaygain_track_peak`
//!   (et `replaygain_album_*` quand ils existent) sur chaque objet piste —
//!   `track/get`, les pistes d'`album/get`, de playlist, de recherche ;
//! * TIDAL : `trackReplayGain` / `trackPeakAmplitude` / `albumReplayGain` /
//!   `albumPeakAmplitude` dans `playbackinfopostpaywall` (v1), et
//!   `trackAudioNormalizationData` / `albumAudioNormalizationData`
//!   (`replayGain`, `peakAmplitude`) dans `trackManifests` (v2) — les deux
//!   réponses qui donnent l'URL du flux ; `replayGain` / `peak` sur l'objet
//!   piste de l'API v1.
//!
//! Ce module les RETIENT au passage, dans un registre du processus, sans
//! aucun appel supplémentaire, et rend le gain au même calcul que le local
//! ([`super::gain_factor_with_peak`]) : mode piste ou album, pré-gain,
//! anti-écrêtage par le pic. Une donnée absente laisse la piste à 1,0 —
//! exactement le comportement d'avant. PURE reste gardé par les appelants,
//! comme pour une piste locale.

use super::{PEAK_MAX_PLAUSIBLE, PeakKind, ReplayGainMode, TrackGain};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Au-delà, le registre repart à vide : quelques milliers de titres
/// parcourus suffisent à toute écoute, et un gain perdu ne coûte qu'un retour
/// au comportement d'avant (facteur 1,0) jusqu'à la prochaine lecture.
const CAPACITE: usize = 50_000;

/// Un gain ReplayGain plausible, en dB. Hors de là ce n'est pas une mesure.
const GAIN_MAX_ABS_DB: f64 = 60.0;

/// Les gains qu'un service publie pour UN titre.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GainsDuService {
    pub piste_db: Option<f64>,
    pub piste_pic: Option<f64>,
    pub album_db: Option<f64>,
    pub album_pic: Option<f64>,
}

fn nombre(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

fn gain(v: &Value) -> Option<f64> {
    nombre(v).filter(|g| g.is_finite() && g.abs() <= GAIN_MAX_ABS_DB)
}

/// Même borne que les pics tagués des fichiers (`PEAK_MAX_PLAUSIBLE`) : une
/// valeur hors échelle est ignorée, l'anti-écrêtage retombe alors sur le cas
/// « aucun pic » (gain positif refusé), jamais sur un son éteint.
fn pic(v: &Value) -> Option<f64> {
    nombre(v).filter(|p| p.is_finite() && *p > 0.0 && *p <= PEAK_MAX_PLAUSIBLE)
}

impl GainsDuService {
    pub fn est_vide(&self) -> bool {
        self.piste_db.is_none() && self.album_db.is_none()
    }

    fn non_vide(self) -> Option<Self> {
        (!self.est_vide()).then_some(self)
    }

    /// Un objet piste Qobuz (`track/get`, items d'album, de playlist…).
    pub fn depuis_qobuz(item: &Value) -> Option<Self> {
        let a = &item["audio_info"];
        Self {
            piste_db: gain(&a["replaygain_track_gain"]),
            piste_pic: pic(&a["replaygain_track_peak"]),
            album_db: gain(&a["replaygain_album_gain"]),
            album_pic: pic(&a["replaygain_album_peak"]),
        }
        .non_vide()
    }

    /// La réponse de lecture TIDAL : v1 `playbackinfopostpaywall`, ou la v2
    /// ramenée à sa forme (`interpreter_manifeste_v2` y recopie les champs).
    pub fn depuis_tidal_lecture(data: &Value) -> Option<Self> {
        Self {
            piste_db: gain(&data["trackReplayGain"]),
            piste_pic: pic(&data["trackPeakAmplitude"]),
            album_db: gain(&data["albumReplayGain"]),
            album_pic: pic(&data["albumPeakAmplitude"]),
        }
        .non_vide()
    }

    /// Les attributs d'un document `trackManifests` (v2).
    pub fn depuis_tidal_v2(attributs: &Value) -> Option<Self> {
        let t = &attributs["trackAudioNormalizationData"];
        let a = &attributs["albumAudioNormalizationData"];
        Self {
            piste_db: gain(&t["replayGain"]),
            piste_pic: pic(&t["peakAmplitude"]),
            album_db: gain(&a["replayGain"]),
            album_pic: pic(&a["peakAmplitude"]),
        }
        .non_vide()
    }

    /// Un objet piste de l'API TIDAL v1 (`replayGain`, `peak`) : gain de
    /// piste seulement.
    pub fn depuis_tidal_piste(item: &Value) -> Option<Self> {
        Self {
            piste_db: gain(&item["replayGain"]),
            piste_pic: pic(&item["peak"]),
            album_db: None,
            album_pic: None,
        }
        .non_vide()
    }

    /// Champ par champ : une valeur reçue remplace l'ancienne, une absence ne
    /// l'efface pas (l'objet piste TIDAL ne porte pas le gain d'album que la
    /// réponse de lecture a donné).
    fn completer(&mut self, neuf: Self) {
        self.piste_db = neuf.piste_db.or(self.piste_db);
        self.piste_pic = neuf.piste_pic.or(self.piste_pic);
        self.album_db = neuf.album_db.or(self.album_db);
        self.album_pic = neuf.album_pic.or(self.album_pic);
    }

    /// Le gain à appliquer pour `mode`, comme [`super::stored_gain_with_peak`]
    /// pour une piste locale : en mode album, repli sur le gain de piste
    /// quand l'album manque. Les pics publiés sont des pics d'ÉCHANTILLON :
    /// la réserve de [`PeakKind::SamplePeak`] s'applique comme au local.
    pub fn choisir(&self, mode: ReplayGainMode) -> Option<(TrackGain, ReplayGainMode, PeakKind)> {
        let piste = || {
            self.piste_db.map(|g| {
                (
                    TrackGain {
                        gain_db: g,
                        peak: self.piste_pic,
                    },
                    ReplayGainMode::Track,
                    nature(self.piste_pic),
                )
            })
        };
        match mode {
            ReplayGainMode::Off => None,
            ReplayGainMode::Album => self
                .album_db
                .map(|g| {
                    (
                        TrackGain {
                            gain_db: g,
                            peak: self.album_pic,
                        },
                        ReplayGainMode::Album,
                        nature(self.album_pic),
                    )
                })
                .or_else(piste),
            ReplayGainMode::Track => piste(),
        }
    }
}

fn nature(pic: Option<f64>) -> PeakKind {
    if pic.is_some() {
        PeakKind::SamplePeak
    } else {
        PeakKind::None
    }
}

/// Les services dont Tune lit le gain publié.
pub fn service_pris_en_charge(source: &str) -> bool {
    matches!(cle_source(source).as_str(), "qobuz" | "tidal")
}

fn cle_source(source: &str) -> String {
    source.trim().to_ascii_lowercase()
}

static REGISTRE: LazyLock<Mutex<HashMap<(String, String), GainsDuService>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Retient les gains d'un titre (`source` = `"qobuz"` / `"tidal"`, `id` =
/// l'identifiant du titre chez le service, celui de `source_id`).
pub fn noter(source: &str, id: &str, gains: GainsDuService) {
    let id = id.trim();
    if id.is_empty() || id == "0" || gains.est_vide() {
        return;
    }
    let Ok(mut r) = REGISTRE.lock() else { return };
    if r.len() >= CAPACITE {
        r.clear();
    }
    r.entry((cle_source(source), id.to_string()))
        .or_default()
        .completer(gains);
}

/// Les gains retenus pour un titre, s'il y en a.
pub fn lire(source: &str, id: &str) -> Option<GainsDuService> {
    REGISTRE
        .lock()
        .ok()?
        .get(&(cle_source(source), id.trim().to_string()))
        .copied()
}

/// La piste dont on cherche le gain : de la bibliothèque (`track_id`), ou
/// d'un service (`source`, `source_id`). La bibliothèque prime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PisteRg<'a> {
    pub track_id: Option<i64>,
    pub service: Option<(&'a str, &'a str)>,
}

impl PisteRg<'static> {
    /// Aucune piste identifiée : facteur 1,0.
    pub const AUCUNE: PisteRg<'static> = PisteRg {
        track_id: None,
        service: None,
    };

    pub fn bibliotheque(track_id: i64) -> Self {
        Self {
            track_id: Some(track_id),
            service: None,
        }
    }
}

impl From<Option<i64>> for PisteRg<'static> {
    fn from(track_id: Option<i64>) -> Self {
        Self {
            track_id,
            service: None,
        }
    }
}

impl<'a> PisteRg<'a> {
    /// `source` / `source_id` comme les portent `PlayRequest` et
    /// `NowPlaying`. Un service dont Tune ne lit pas le gain n'est pas retenu.
    pub fn new(track_id: Option<i64>, source: Option<&'a str>, source_id: Option<&'a str>) -> Self {
        let service = match (track_id, source, source_id) {
            (None, Some(s), Some(id)) if service_pris_en_charge(s) && !id.trim().is_empty() => {
                Some((s, id))
            }
            _ => None,
        };
        Self { track_id, service }
    }

    pub fn est_identifiee(&self) -> bool {
        self.track_id.is_some() || self.service.is_some()
    }
}

#[cfg(test)]
#[path = "services_tests.rs"]
mod tests;
