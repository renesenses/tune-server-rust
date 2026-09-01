//! « Disponible en meilleure qualité » — au lancement d'une piste ou d'un
//! album, proposer la variante de qualité supérieure déjà possédée
//! (Bertrand, 25/08). Le `quality_split` garde les variantes d'un album en
//! entrées séparées : ces routes les rapprochent par titre+artiste, comme
//! les Doublons, mais dans le sens du service — trouver MIEUX, pas pareil.

use axum::Json;
use axum::extract::{Path, State};
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::error::AppError;
use crate::state::AppState;

/// Score de qualité comparable entre formats : `(sans_perte, débit)`.
///
/// Un sans-perte bat toujours un avec-perte ; à famille égale,
/// `sample_rate × bit_depth` départage. Le DSD porte `bit_depth = 1` :
/// DSD64 (2,8 M) bat le CD (0,7 M) et le 96/24 (2,3 M), mais s'incline
/// devant le 192/24 (4,6 M) — arbitrage assumé, documenté, et testé.
pub(crate) fn score_qualite(
    format: Option<&str>,
    sample_rate: Option<i64>,
    bit_depth: Option<i64>,
) -> (bool, i64) {
    // La même liste que le filtre « Lossy » de la bibliothèque.
    const AVEC_PERTE: [&str; 5] = ["mp3", "aac", "ogg", "opus", "wma"];
    let sans_perte = format
        .map(|f| !AVEC_PERTE.contains(&f.to_lowercase().as_str()))
        .unwrap_or(false);
    let sr = sample_rate.unwrap_or(44100).max(1);
    let bd = bit_depth.unwrap_or(16).max(1);
    (sans_perte, sr.saturating_mul(bd))
}

/// `GET /library/tracks/{id}/better-quality` — la meilleure variante d'une
/// piste (même titre, même artiste, autre piste), ou `null`.
pub(super) async fn track_better_quality(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let rows = state
        .backend
        .query_many(
            &format!(
                "SELECT t2.id, t2.title, t2.format, t2.sample_rate, t2.bit_depth, \
                        t2.album_id, al2.title, al2.cover_path, \
                        t1.format, t1.sample_rate, t1.bit_depth \
                 FROM tracks t1 \
                 JOIN tracks t2 ON LOWER(t2.title) = LOWER(t1.title) \
                      AND t2.artist_id = t1.artist_id AND t2.id <> t1.id \
                 LEFT JOIN albums al2 ON t2.album_id = al2.id \
                 WHERE t1.id = {id}"
            ),
            &[],
        )
        .ou_defaut_journalise();

    let mut courant: Option<(bool, i64)> = None;
    let mut meilleur: Option<(Value, (bool, i64))> = None;
    for r in rows {
        let score_courant = score_qualite(
            r.get(8).and_then(|v| v.as_string()).as_deref(),
            r.get(9).and_then(|v| v.as_i64()),
            r.get(10).and_then(|v| v.as_i64()),
        );
        courant = Some(score_courant);
        let score = score_qualite(
            r.get(2).and_then(|v| v.as_string()).as_deref(),
            r.get(3).and_then(|v| v.as_i64()),
            r.get(4).and_then(|v| v.as_i64()),
        );
        if score > score_courant && meilleur.as_ref().is_none_or(|(_, s)| score > *s) {
            meilleur = Some((
                json!({
                    "track_id": r.get(0).and_then(|v| v.as_i64()),
                    "title": r.get(1).and_then(|v| v.as_string()),
                    "format": r.get(2).and_then(|v| v.as_string()),
                    "sample_rate": r.get(3).and_then(|v| v.as_i64()),
                    "bit_depth": r.get(4).and_then(|v| v.as_i64()),
                    "album_id": r.get(5).and_then(|v| v.as_i64()),
                    "album_title": r.get(6).and_then(|v| v.as_string()),
                    "cover_path": r.get(7).and_then(|v| v.as_string()),
                }),
                score,
            ));
        }
    }
    let _ = courant;
    Ok(Json(json!({ "better": meilleur.map(|(v, _)| v) })))
}

/// `GET /library/albums/{id}/better-quality` — la meilleure édition d'un
/// album (même titre, même artiste, autre album), ou `null`. Le
/// `quality_split` garde justement ces éditions séparées.
pub(super) async fn album_better_quality(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let rows = state
        .backend
        .query_many(
            &format!(
                "SELECT a2.id, a2.title, a2.format, a2.sample_rate, a2.bit_depth, \
                        a2.cover_path, \
                        a1.format, a1.sample_rate, a1.bit_depth \
                 FROM albums a1 \
                 JOIN albums a2 ON LOWER(a2.title) = LOWER(a1.title) \
                      AND a2.artist_id = a1.artist_id AND a2.id <> a1.id \
                 WHERE a1.id = {id}"
            ),
            &[],
        )
        .ou_defaut_journalise();

    let mut meilleur: Option<(Value, (bool, i64))> = None;
    for r in rows {
        let score_courant = score_qualite(
            r.get(6).and_then(|v| v.as_string()).as_deref(),
            r.get(7).and_then(|v| v.as_i64()),
            r.get(8).and_then(|v| v.as_i64()),
        );
        let score = score_qualite(
            r.get(2).and_then(|v| v.as_string()).as_deref(),
            r.get(3).and_then(|v| v.as_i64()),
            r.get(4).and_then(|v| v.as_i64()),
        );
        if score > score_courant && meilleur.as_ref().is_none_or(|(_, s)| score > *s) {
            meilleur = Some((
                json!({
                    "album_id": r.get(0).and_then(|v| v.as_i64()),
                    "album_title": r.get(1).and_then(|v| v.as_string()),
                    "format": r.get(2).and_then(|v| v.as_string()),
                    "sample_rate": r.get(3).and_then(|v| v.as_i64()),
                    "bit_depth": r.get(4).and_then(|v| v.as_i64()),
                    "cover_path": r.get(5).and_then(|v| v.as_string()),
                }),
                score,
            ));
        }
    }
    Ok(Json(json!({ "better": meilleur.map(|(v, _)| v) })))
}

#[cfg(test)]
mod tests {
    use super::score_qualite;

    /// Un sans-perte bat toujours un avec-perte, même « hi-res lossy ».
    #[test]
    fn le_sans_perte_bat_l_avec_perte() {
        assert!(
            score_qualite(Some("flac"), Some(44100), Some(16))
                > score_qualite(Some("mp3"), Some(48000), Some(24))
        );
    }

    /// À famille égale, la résolution départage : 96/24 > 44.1/16.
    #[test]
    fn la_resolution_departage_les_sans_perte() {
        assert!(
            score_qualite(Some("flac"), Some(96000), Some(24))
                > score_qualite(Some("flac"), Some(44100), Some(16))
        );
    }

    /// DSD64 (2,8 M × 1 bit) bat le CD et le 96/24, s'incline devant 192/24 —
    /// l'arbitrage documenté.
    #[test]
    fn le_dsd_est_arbitre_comme_documente() {
        let dsd64 = score_qualite(Some("dsf"), Some(2_822_400), Some(1));
        assert!(dsd64 > score_qualite(Some("flac"), Some(44100), Some(16)));
        assert!(dsd64 > score_qualite(Some("flac"), Some(96000), Some(24)));
        assert!(score_qualite(Some("flac"), Some(192_000), Some(24)) > dsd64);
    }

    /// Deux fichiers identiques : aucun n'est « meilleur ».
    #[test]
    fn a_egalite_rien_n_est_meilleur() {
        let a = score_qualite(Some("flac"), Some(44100), Some(16));
        assert!(!(a > a));
    }
}
