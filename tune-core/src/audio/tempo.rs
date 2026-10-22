//! D'après MetaRust, de Xavier Joly (code offert à Tune le 05/10/2026).
//!
//! Le TEMPO (BPM) d'une piste : Wikidata (P1725) d'abord, puis une estimation
//! à partir du signal (#4805, idée 9 de l'analyse de MetaRust).
//!
//! # Repris, exclu
//!
//! * **Repris** de MetaRust (`bpm.rs`), tels quels : [`estimate_bpm_from_mono`]
//!   (enveloppe RMS par pas de 10 ms, flux positif, autocorrélation entre 70
//!   et 180 BPM) et [`fold_bpm`] (repli demi/double temps vers la zone
//!   « naturelle » 78–155, au plus près de 112). Ce sont des fonctions pures.
//!   Les fenêtres aussi : 75 s analysées après avoir sauté les 18 premières
//!   (« intros libres, fade-in — souvent la cause de BPM absurdes »).
//! * **Exclus** : les chemins `aubio` et `ffmpeg` de MetaRust. Le signal est
//!   décodé par le décodeur de Tune ([`super::analyzer::decode_pcm`], natif
//!   Rust), jamais par un binaire externe.
//!
//! # Stockage
//!
//! Le BPM estimé va dans `bpm_estimes`, par CHEMIN de fichier (un scan
//! complet renumérote les pistes ; le chemin reste). La colonne `tracks.bpm`
//! est la BALISE du fichier et n'est jamais écrite ici : une balise BPM passe
//! toujours devant une estimation, et rien n'est écrit dans les fichiers.

// Code de signal : même règle que `analyzer.rs` — le découpage par
// `chunks_exact` est gardé tel quel (clippy 1.98).
#![allow(clippy::chunks_exact_to_as_chunks)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tracing::{debug, info};

use crate::db::backend::{DbBackend, ToSqlValue};

const ANALYZE_SECS: f64 = 75.0;
/// Ignore le début (intros libres, fade-in) — souvent la cause de BPM absurdes.
const ANALYZE_SKIP_SECS: f64 = 18.0;
const PCM_RATE: u32 = 22_050;
const MIN_BPM: f64 = 70.0;
const MAX_BPM: f64 = 180.0;
/// Zone « naturelle » pour choisir entre BPM et demi/double temps.
const PREFERRED_BPM_LOW: f64 = 78.0;
const PREFERRED_BPM_HIGH: f64 = 155.0;

/// Estime le BPM d'un signal mono (repris de MetaRust, sans changement).
pub fn estimate_bpm_from_mono(samples: &[f32], sample_rate: u32) -> Option<f64> {
    if samples.len() < sample_rate as usize {
        return None;
    }
    let hop = (sample_rate / 100).max(1) as usize;
    let n_hops = samples.len() / hop;
    if n_hops < 32 {
        return None;
    }
    let mut env = vec![0f32; n_hops];
    for (i, slot) in env.iter_mut().enumerate() {
        let start = i * hop;
        let slice = &samples[start..start + hop];
        let sum_sq: f32 = slice.iter().map(|x| x * x).sum();
        *slot = (sum_sq / hop as f32).sqrt();
    }
    let mut flux = vec![0f32; n_hops];
    for i in 1..n_hops {
        flux[i] = (env[i] - env[i - 1]).max(0.0);
    }
    let mean = flux.iter().sum::<f32>() / flux.len() as f32;
    for v in &mut flux {
        *v -= mean;
    }

    let hops_per_sec = sample_rate as f64 / hop as f64;
    let min_lag = ((hops_per_sec * 60.0 / MAX_BPM).round() as usize).max(1);
    let max_lag = ((hops_per_sec * 60.0 / MIN_BPM).round() as usize).min(flux.len() / 2);
    if max_lag <= min_lag {
        return None;
    }

    let mut best_lag = 0usize;
    let mut best_score = f32::MIN;
    for lag in min_lag..=max_lag {
        let mut acc = 0f32;
        let n = flux.len() - lag;
        for i in lag..flux.len() {
            acc += flux[i] * flux[i - lag];
        }
        acc /= n as f32;
        if acc > best_score {
            best_score = acc;
            best_lag = lag;
        }
    }
    if best_lag == 0 || best_score <= 0.0 {
        return None;
    }
    Some(fold_bpm(60.0 * hops_per_sec / best_lag as f64))
}

/// Ramène un BPM mesuré dans la zone naturelle par demi ou double temps
/// (repris de MetaRust, sans changement).
pub fn fold_bpm(bpm: f64) -> f64 {
    if !bpm.is_finite() || bpm <= 0.0 {
        return bpm;
    }
    // Candidats octave (demi / double) dans la plage utile.
    let mut cands = Vec::new();
    let mut b = bpm;
    while b < MIN_BPM {
        b *= 2.0;
    }
    while b > MAX_BPM {
        b /= 2.0;
    }
    cands.push(b);
    if b / 2.0 >= MIN_BPM {
        cands.push(b / 2.0);
    }
    if b * 2.0 <= MAX_BPM {
        cands.push(b * 2.0);
    }
    // Un détecteur compte souvent les croches → préfère la zone 78–155.
    let preferred: Vec<f64> = cands
        .iter()
        .copied()
        .filter(|x| *x >= PREFERRED_BPM_LOW && *x <= PREFERRED_BPM_HIGH)
        .collect();
    let pool = if preferred.is_empty() {
        cands
    } else {
        preferred
    };
    let target = 112.0;
    pool.into_iter()
        .min_by(|a, b| {
            (a - target)
                .abs()
                .partial_cmp(&(b - target).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(bpm)
}

/// i16 LE (le contrat de `analyzer::decode_pcm`) en f32 [-1, 1].
pub fn pcm_i16_en_f32(octets: &[u8]) -> Vec<f32> {
    octets
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect()
}

/// Mesure le BPM d'un fichier par le décodeur de Tune : 75 s après les 18
/// premières (depuis le début si la piste est trop courte pour les sauter).
pub async fn mesurer_bpm_du_fichier(chemin: &str) -> Option<f64> {
    let duree = super::analyzer::get_duration(chemin).await.ok()?;
    if duree <= 0.0 {
        return None;
    }
    let debut = if duree > ANALYZE_SKIP_SECS + 30.0 {
        ANALYZE_SKIP_SECS
    } else {
        0.0
    };
    let pcm = super::analyzer::decode_pcm(chemin, PCM_RATE, 1, debut, ANALYZE_SECS)
        .await
        .ok()?;
    let mono = pcm_i16_en_f32(&pcm);
    let bpm = estimate_bpm_from_mono(&mono, PCM_RATE)?;
    debug!(file = chemin, bpm, "bpm_estime_signal");
    Some(bpm)
}

/// Le BPM tel qu'on l'écrit : un entier.
pub fn arrondir(bpm: f64) -> f64 {
    bpm.round()
}

// ── En base ─────────────────────────────────────────────────────────────────

/// D'où vient un BPM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceBpm {
    /// La balise du fichier (`tracks.bpm`).
    Balise,
    Wikidata,
    Signal,
}

impl SourceBpm {
    fn code(self) -> &'static str {
        match self {
            SourceBpm::Balise => "tag",
            SourceBpm::Wikidata => "wikidata",
            SourceBpm::Signal => "signal",
        }
    }
    fn depuis(code: &str) -> Option<SourceBpm> {
        match code {
            "wikidata" => Some(SourceBpm::Wikidata),
            "signal" => Some(SourceBpm::Signal),
            _ => None,
        }
    }
}

/// Le BPM d'une piste et sa source.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BpmPiste {
    pub track_id: i64,
    pub bpm: Option<f64>,
    pub source: Option<SourceBpm>,
}

/// Une piste de l'album, telle que la passe la lit.
#[derive(Debug, Clone)]
struct PisteLue {
    id: i64,
    chemin: String,
    balise: Option<f64>,
    recording_mbid: Option<String>,
}

fn pistes(backend: &Arc<dyn DbBackend>, album_id: i64) -> Vec<PisteLue> {
    backend
        .query_many(
            "SELECT id, file_path, bpm, musicbrainz_recording_id FROM tracks \
             WHERE album_id = ? ORDER BY disc_number, track_number, id",
            &[&album_id as &dyn ToSqlValue],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            Some(PisteLue {
                id: r.first()?.as_i64()?,
                chemin: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                balise: r.get(2).and_then(|v| v.as_f64()).filter(|b| *b > 0.0),
                recording_mbid: r
                    .get(3)
                    .and_then(|v| v.as_string())
                    .filter(|s| !s.trim().is_empty()),
            })
        })
        .collect()
}

fn estime(backend: &Arc<dyn DbBackend>, chemin: &str) -> Option<(f64, SourceBpm)> {
    let r = backend
        .query_one(
            "SELECT bpm, source FROM bpm_estimes WHERE file_path = ?",
            &[&chemin as &dyn ToSqlValue],
        )
        .ok()
        .flatten()?;
    Some((
        r.first()?.as_f64()?,
        SourceBpm::depuis(&r.get(1)?.as_string()?)?,
    ))
}

fn garder(backend: &Arc<dyn DbBackend>, chemin: &str, bpm: f64, source: SourceBpm) {
    let quand = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let code = source.code();
    if let Err(e) = backend.execute(
        "INSERT INTO bpm_estimes (file_path, bpm, source, mesure_le) VALUES (?, ?, ?, ?) \
         ON CONFLICT (file_path) DO UPDATE SET bpm = excluded.bpm, source = excluded.source, \
         mesure_le = excluded.mesure_le",
        &[
            &chemin as &dyn ToSqlValue,
            &bpm as &dyn ToSqlValue,
            &code as &dyn ToSqlValue,
            &quand as &dyn ToSqlValue,
        ],
    ) {
        tracing::warn!(erreur = %e, "bpm_estimes_ecriture_impossible");
    }
}

/// Les BPM d'un album : la balise d'abord, sinon l'estimation gardée.
pub fn bpm_de_l_album(backend: &Arc<dyn DbBackend>, album_id: i64) -> Vec<BpmPiste> {
    pistes(backend, album_id)
        .into_iter()
        .map(|p| match p.balise {
            Some(b) => BpmPiste {
                track_id: p.id,
                bpm: Some(b),
                source: Some(SourceBpm::Balise),
            },
            None => match estime(backend, &p.chemin) {
                Some((b, s)) => BpmPiste {
                    track_id: p.id,
                    bpm: Some(b),
                    source: Some(s),
                },
                None => BpmPiste {
                    track_id: p.id,
                    bpm: None,
                    source: None,
                },
            },
        })
        .collect()
}

/// Les œuvres de chaque enregistrement d'une release gardée
/// (`recording.relations[].work.id`) : le second recours Wikidata.
pub fn oeuvres_par_enregistrement(release: &Value) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for medium in release
        .get("media")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for t in medium
            .get("tracks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(rec) = t.get("recording") else {
                continue;
            };
            let Some(rid) = rec.get("id").and_then(Value::as_str) else {
                continue;
            };
            for rel in rec
                .get("relations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(w) = rel.pointer("/work/id").and_then(Value::as_str) {
                    out.entry(rid.to_string()).or_default().push(w.to_string());
                }
            }
        }
    }
    out
}

/// Bilan de l'estimation d'un album.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct BilanBpm {
    /// Pistes dont la balise porte déjà un BPM : laissées.
    pub balises: usize,
    pub deja_estimees: usize,
    pub wikidata: usize,
    pub signal: usize,
    pub sans_resultat: usize,
}

/// Estime les BPM d'un album : Wikidata d'abord, puis le signal. Ni la
/// balise ni le fichier ne sont touchés. Transports en paramètre :
/// `wikidata(recording, works)` et `mesurer(chemin)`.
pub async fn estimer_l_album_par<W, WF, M, MF>(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    refaire: bool,
    wikidata: W,
    mesurer: M,
) -> BilanBpm
where
    W: Fn(String, Vec<String>) -> WF,
    WF: Future<Output = Option<f64>>,
    M: Fn(String) -> MF,
    MF: Future<Output = Option<f64>>,
{
    let oeuvres = crate::metadata::completude::pressage_de_l_album(backend, album_id)
        .and_then(|mbid| {
            crate::metadata::musicbrainz_release_cache::lire(
                backend,
                &mbid,
                "recordings+recording-level-rels+work-rels",
                chrono::Utc::now(),
            )
        })
        .map(|r| oeuvres_par_enregistrement(&r))
        .unwrap_or_default();
    let mut bilan = BilanBpm::default();
    for p in pistes(backend, album_id) {
        if p.balise.is_some() {
            bilan.balises += 1;
            continue;
        }
        if p.chemin.is_empty() || p.chemin.contains("://") {
            bilan.sans_resultat += 1;
            continue;
        }
        if !refaire && estime(backend, &p.chemin).is_some() {
            bilan.deja_estimees += 1;
            continue;
        }
        crate::taches_de_fond::attendre_la_reprise(crate::taches_de_fond::Tache::Enrichissement)
            .await;
        if let Some(rid) = &p.recording_mbid {
            let works = oeuvres.get(rid).cloned().unwrap_or_default();
            if let Some(b) = wikidata(rid.clone(), works).await {
                garder(backend, &p.chemin, b, SourceBpm::Wikidata);
                bilan.wikidata += 1;
                continue;
            }
        }
        match mesurer(p.chemin.clone()).await {
            Some(b) => {
                garder(backend, &p.chemin, arrondir(fold_bpm(b)), SourceBpm::Signal);
                bilan.signal += 1;
            }
            None => bilan.sans_resultat += 1,
        }
    }
    info!(album_id, ?bilan, "bpm_album_estime");
    bilan
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le repli d'un BPM aberrant : double temps, demi-temps, croches.
    #[test]
    fn le_repli_d_un_bpm_aberrant_est_correct() {
        assert!((fold_bpm(60.0) - 120.0).abs() < 0.01, "60 → 120");
        assert!((fold_bpm(240.0) - 120.0).abs() < 0.01, "240 → 120");
        let croches = fold_bpm(171.0);
        assert!(
            croches > 80.0 && croches < 100.0,
            "171 (croches) → demi-temps vers 85, obtenu {croches}"
        );
        assert!(
            (fold_bpm(35.0) - 140.0).abs() < 0.01,
            "35 → 140 (deux doublements)"
        );
        assert!((fold_bpm(480.0) - 120.0).abs() < 0.01, "480 → 120");
        // Un BPM déjà naturel ne bouge pas.
        assert!((fold_bpm(128.0) - 128.0).abs() < 0.01);
        assert!((fold_bpm(92.0) - 92.0).abs() < 0.01);
    }

    #[test]
    fn estime_un_metronome_120() {
        let sr = 22_050u32;
        let secs = 8u32;
        let n = (sr * secs) as usize;
        let mut samples = vec![0f32; n];
        let period = sr as usize / 2; // 120 BPM
        for i in (0..n).step_by(period) {
            for k in 0..80.min(n - i) {
                samples[i + k] = 0.9;
            }
        }
        let bpm = estimate_bpm_from_mono(&samples, sr).unwrap();
        assert!((bpm - 120.0).abs() < 4.0, "attendu ~120, obtenu {bpm}");
    }

    #[test]
    fn un_silence_ne_donne_pas_de_bpm() {
        assert!(estimate_bpm_from_mono(&vec![0f32; 22_050 * 4], 22_050).is_none());
        assert!(estimate_bpm_from_mono(&[0.5; 100], 22_050).is_none());
    }

    #[test]
    fn le_pcm_i16_se_lit_en_flottant() {
        let v = pcm_i16_en_f32(&[0x00, 0x40, 0x00, 0xC0]);
        assert_eq!(v, vec![0.5, -0.5]);
    }

    fn base() -> Arc<dyn DbBackend> {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Wikidata passe avant le signal ; la balise passe avant tout, et n'est
    /// jamais écrite.
    #[tokio::test]
    async fn wikidata_d_abord_puis_le_signal_et_la_balise_reste() {
        let b = base();
        b.execute("INSERT INTO albums (id, title) VALUES (1, 'A')", &[])
            .unwrap();
        b.execute(
            "INSERT INTO tracks (id, title, album_id, track_number, file_path, bpm, musicbrainz_recording_id) VALUES \
             (1, 'Balisée', 1, 1, '/m/1.flac', 97, NULL), \
             (2, 'Wikidata', 1, 2, '/m/2.flac', NULL, '1a13c710-4b7e-4701-8968-cd61f2e58110'), \
             (3, 'Signal', 1, 3, '/m/3.flac', NULL, NULL)",
            &[],
        )
        .unwrap();
        let bilan = estimer_l_album_par(
            &b,
            1,
            false,
            |rid, _| std::future::ready((rid.starts_with("1a13c710")).then_some(105.0)),
            // Un détecteur qui compte les croches : 240 doit se replier en 120.
            |_| std::future::ready(Some(240.0)),
        )
        .await;
        assert_eq!(bilan.balises, 1);
        assert_eq!(bilan.wikidata, 1);
        assert_eq!(bilan.signal, 1);
        let bpm = bpm_de_l_album(&b, 1);
        assert_eq!(bpm[0].bpm, Some(97.0));
        assert_eq!(bpm[0].source, Some(SourceBpm::Balise));
        assert_eq!(bpm[1].bpm, Some(105.0));
        assert_eq!(bpm[1].source, Some(SourceBpm::Wikidata));
        assert_eq!(bpm[2].bpm, Some(120.0));
        assert_eq!(bpm[2].source, Some(SourceBpm::Signal));
        // `tracks.bpm` (la balise) n'a pas bougé pour les pistes estimées.
        let r = b
            .query_one("SELECT bpm FROM tracks WHERE id = 3", &[])
            .unwrap()
            .unwrap();
        assert!(r.first().and_then(|v| v.as_f64()).is_none());
        // Rejouée : rien n'est remesuré.
        let encore = estimer_l_album_par(
            &b,
            1,
            false,
            |_, _| std::future::ready(None),
            |_| std::future::ready(None),
        )
        .await;
        assert_eq!(encore.deja_estimees, 2);
    }
}
