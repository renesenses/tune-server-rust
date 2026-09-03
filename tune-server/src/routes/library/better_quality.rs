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

/// Le barème vit dans `tune-core` : le repli d'album — affichage ET file de
/// lecture — s'en sert pour choisir laquelle de deux copies d'un morceau
/// survit (#1362). Deux implémentations du même arbitrage finiraient par
/// diverger, et l'écran proposerait une variante que la lecture n'irait pas
/// chercher.
pub(crate) use tune_core::library::quality::score_qualite;

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

    /// L'arbitrage lui-même (sans-perte, résolution, DSD, égalité) est testé
    /// là où il vit désormais — `tune_core::library::quality`. Ce qui se
    /// vérifie ICI est que la route parle bien à ce barème-là, et pas à une
    /// copie locale qui aurait recommencé à dériver.
    #[test]
    fn la_route_utilise_le_bareme_de_tune_core() {
        assert_eq!(
            score_qualite(Some("flac"), Some(44100), Some(16)),
            tune_core::library::quality::score_qualite(Some("flac"), Some(44100), Some(16))
        );
        assert!(
            score_qualite(Some("aiff"), Some(44100), Some(16))
                > score_qualite(Some("aac"), Some(48000), Some(24))
        );
    }
}
