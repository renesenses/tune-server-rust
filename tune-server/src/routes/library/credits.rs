use crate::routes::panne_sql::OuDefautJournalise;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::track_repo::TrackRepo;

use super::credits_mb::{LigneCredit, REGLAGE_AVANCEMENT_CREDITS, lignes_credits};

/// Nombre de pistes enrichies entre deux écritures du statut. La passe tient la
/// cadence MusicBrainz d'1 req/s : écrire à chaque piste ferait un `UPDATE` par
/// seconde pour rien.
const JALON_STATUT: i32 = 25;

/// Corps optionnel de `POST /library/enrich-credits` (#2799).
///
/// Sans corps — le contrat historique — `only_missing` vaut `false` et la passe
/// couvre **toutes** les pistes portant un `musicbrainz_recording_id`, à la
/// ligne près.
#[derive(serde::Deserialize, Default)]
pub(super) struct CorpsEnrichCredits {
    /// `true` : sauter les pistes qui ont déjà au moins une ligne dans
    /// `track_credits`. Un second clic ne refrappe alors que ce qui manque.
    #[serde(default)]
    pub(super) only_missing: bool,
}

/// Écrit les crédits d'une piste : purge puis insertion, positions numérotées.
///
/// 🔴 `position` est INCRÉMENTÉ. Les passes album et bibliothèque écrivaient
/// `0` en dur pour toutes les relations alors que la lecture trie
/// `ORDER BY position` : l'ordre des crédits était arbitraire dès qu'on passait
/// par autre chose que l'enrichissement d'une piste seule.
///
/// Les identifiants sont liés en CHAÎNE : sur le miroir PostgreSQL,
/// `track_credits.track_id` est du `TEXT`, et un bind i64 produit un
/// `text = bigint` que PG refuse — c'est déjà documenté sur la lecture en tête
/// de ce fichier, l'écriture ne l'avait pas reçu.
fn ecrire_credits(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    track_id: i64,
    lignes: &[LigneCredit],
) -> usize {
    use tune_core::db::backend::ToSqlValue;
    let id_str = track_id.to_string();
    backend
        .execute(
            "DELETE FROM track_credits WHERE track_id = ?",
            &[&id_str as &dyn ToSqlValue],
        )
        .ok();
    let mut ecrites = 0usize;
    for (pos, ligne) in lignes.iter().enumerate() {
        let pos = pos as i32;
        let ok = backend
            .execute(
                "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) \
                 VALUES (?, ?, ?, ?, ?)",
                &[
                    &id_str as &dyn ToSqlValue,
                    &ligne.artist_name as &dyn ToSqlValue,
                    &ligne.role as &dyn ToSqlValue,
                    &ligne.instrument as &dyn ToSqlValue,
                    &pos as &dyn ToSqlValue,
                ],
            )
            .is_ok();
        if ok {
            ecrites += 1;
        }
    }
    ecrites
}

pub(super) async fn track_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    use tune_core::db::backend::ToSqlValue;
    // On Postgres the mirror schema stores integer-semantic columns as TEXT, so
    // binding an i64 here made the comparison `text = bigint`, which Postgres
    // rejects ("operator does not exist: text = bigint") → 500. Bind the id as a
    // string: `text = text` on PG, and SQLite numeric affinity handles it too.
    let id_str = id.to_string();
    let rows = state
        .backend
        .query_many(
            "SELECT id, track_id, artist_id, artist_name, role, instrument, position FROM track_credits WHERE track_id = ? ORDER BY position",
            &[&id_str as &dyn ToSqlValue],
        )
        .map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "track_id": r.get(1).and_then(|v| v.as_i64()),
                "artist_id": r.get(2).and_then(|v| v.as_i64()),
                "artist_name": r.get(3).and_then(|v| v.as_string()),
                "role": r.get(4).and_then(|v| v.as_string()),
                "instrument": r.get(5).and_then(|v| v.as_string()),
                "position": r.get(6).and_then(|v| v.as_i64()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

pub(super) async fn artist_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    use tune_core::db::backend::ToSqlValue;
    // Bind as string — see track_credits above (Postgres TEXT columns vs bigint).
    let id_str = id.to_string();
    let rows = state
        .backend
        .query_many(
            "SELECT tc.id, tc.track_id, tc.artist_id, tc.artist_name, tc.role, tc.instrument, tc.position \
             FROM track_credits tc \
             WHERE tc.artist_id = ? OR tc.artist_name = (SELECT name FROM artists WHERE id = ?) \
             ORDER BY tc.track_id, tc.position",
            &[&id_str as &dyn ToSqlValue, &id_str as &dyn ToSqlValue],
        )
        .map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "track_id": r.get(1).and_then(|v| v.as_i64()),
                "artist_id": r.get(2).and_then(|v| v.as_i64()),
                "artist_name": r.get(3).and_then(|v| v.as_string()),
                "role": r.get(4).and_then(|v| v.as_string()),
                "instrument": r.get(5).and_then(|v| v.as_string()),
                "position": r.get(6).and_then(|v| v.as_i64()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

pub(super) async fn enrich_track_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return Json(json!({"enriched": false, "reason": "track not found"})).into_response(),
    };

    let Some(ref mbid) = track.musicbrainz_recording_id else {
        return Json(json!({"enriched": false, "reason": "no MusicBrainz recording ID"}))
            .into_response();
    };

    let url = format!(
        "https://musicbrainz.org/ws/2/recording/{mbid}?inc=artist-credits+artist-rels&fmt=json"
    );

    let resp =
        match state.http_client.get(&url).send().await {
            Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                Ok(data) => data,
                Err(_) => {
                    return Json(
                        json!({"enriched": false, "reason": "invalid MusicBrainz response"}),
                    )
                    .into_response();
                }
            },
            Ok(r) => return Json(
                json!({"enriched": false, "reason": format!("MusicBrainz HTTP {}", r.status())}),
            )
            .into_response(),
            Err(e) => return Json(
                json!({"enriched": false, "reason": format!("MusicBrainz request failed: {e}")}),
            )
            .into_response(),
        };

    let count = ecrire_credits(&state.backend, id, &lignes_credits(&resp));

    Json(json!({"enriched": true, "credits_count": count})).into_response()
}

pub(super) async fn enrich_album_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let tracks = track_repo.list_by_album(id).unwrap_or_default();

    let mut enriched = 0i32;
    let mut skipped = 0i32;
    let mut failed = 0i32;
    let total = tracks.len() as i32;

    for track in &tracks {
        let track_id = match track.id {
            Some(id) => id,
            None => {
                skipped += 1;
                continue;
            }
        };

        let Some(ref mbid) = track.musicbrainz_recording_id else {
            skipped += 1;
            continue;
        };

        let url = format!(
            "https://musicbrainz.org/ws/2/recording/{mbid}?inc=artist-credits+artist-rels&fmt=json"
        );

        let resp = match state.http_client.get(&url).send().await {
            Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                Ok(data) => data,
                Err(_) => {
                    failed += 1;
                    continue;
                }
            },
            _ => {
                failed += 1;
                continue;
            }
        };

        ecrire_credits(&state.backend, track_id, &lignes_credits(&resp));

        enriched += 1;

        // MusicBrainz rate limit: 1 request/sec
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    }

    Json(json!({
        "album_id": id,
        "total": total,
        "enriched": enriched,
        "skipped": skipped,
        "failed": failed,
    }))
}

/// Sélection des pistes candidates à l'enrichissement des crédits (#2799).
///
/// `only_missing = false` : contrat historique, toute piste portant un
/// `musicbrainz_recording_id`.
///
/// `only_missing = true` : on ajoute `NOT EXISTS (…)` sur `track_credits`. Le
/// filtre est fait par le SGBD, pas en Rust : sur une grosse bibliothèque, la
/// liste des déjà-couverts n'a aucune raison de transiter en mémoire.
///
/// Sous-requête CORRÉLÉE, sans paramètre lié — `tc.track_id = t.id` compare
/// deux colonnes du même type sur les deux backends (`INTEGER` sur SQLite,
/// `TEXT` sur le miroir PostgreSQL). Un `IN (…)` avec des identifiants liés
/// aurait rejoué le piège `text = bigint` documenté en tête de ce fichier.
fn requete_candidats(only_missing: bool) -> &'static str {
    if only_missing {
        "SELECT t.id, t.musicbrainz_recording_id FROM tracks t \
         WHERE t.musicbrainz_recording_id IS NOT NULL AND t.musicbrainz_recording_id != '' \
         AND NOT EXISTS (SELECT 1 FROM track_credits tc WHERE tc.track_id = t.id)"
    } else {
        "SELECT t.id, t.musicbrainz_recording_id FROM tracks t \
         WHERE t.musicbrainz_recording_id IS NOT NULL AND t.musicbrainz_recording_id != ''"
    }
}

/// Statut d'avancement sérialisé dans `settings`.
fn statut_credits(
    status: &str,
    task_id: &str,
    enriched: i32,
    errors: i32,
    skipped: i32,
    total: usize,
) -> String {
    json!({
        "status": status,
        "task_id": task_id,
        "enriched": enriched,
        "errors": errors,
        "skipped": skipped,
        "total": total,
    })
    .to_string()
}

/// POST /library/enrich-credits
///
/// C'est la route que le client web appelle réellement (Réglages + Smart
/// Collections). Elle rend `202` immédiatement et travaille en tâche de fond à
/// la cadence MusicBrainz d'1 req/s.
///
/// Deux manques comblés (#2799) :
/// - l'avancement est **persisté** dans `settings` et relisible par
///   [`enrich_credits_status`] ; jusqu'ici le seul retour était une ligne de
///   journal en fin de passe, et le `task_id` rendu en 202 n'était
///   interrogeable nulle part ;
/// - le corps optionnel `{"only_missing": true}` évite de refrapper
///   MusicBrainz pour les pistes déjà couvertes.
pub(super) async fn enrich_all_credits(
    State(state): State<AppState>,
    corps: Option<Json<CorpsEnrichCredits>>,
) -> impl IntoResponse {
    let only_missing = corps.map(|Json(c)| c.only_missing).unwrap_or(false);
    let task_id = uuid::Uuid::new_v4().to_string();
    let task_id_clone = task_id.clone();
    let backend = state.backend.clone();
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());

    // Écrit AVANT le spawn : sans cela, un client qui sonde le statut juste
    // après son 202 lit encore `idle` et croit la passe finie avant d'avoir
    // commencé.
    reglages
        .set(
            REGLAGE_AVANCEMENT_CREDITS,
            &statut_credits("running", &task_id, 0, 0, 0, 0),
        )
        .ok();

    tokio::spawn(async move {
        let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
        let track_ids: Vec<(i64, String)> = backend
            .query_many(requete_candidats(only_missing), &[])
            .ou_defaut_journalise()
            .into_iter()
            .filter_map(|r| {
                let id = r.get(0).and_then(|v| v.as_i64())?;
                let mbid = r.get(1).and_then(|v| v.as_string())?;
                Some((id, mbid))
            })
            .collect();
        let total = track_ids.len();

        // Le total dès qu'il est connu : la sélection peut prendre du temps sur
        // une grosse bibliothèque, et une barre bloquée sur « 0/0 » ne dit pas
        // si la passe travaille ou si elle est morte.
        reglages
            .set(
                REGLAGE_AVANCEMENT_CREDITS,
                &statut_credits("running", &task_id_clone, 0, 0, 0, total),
            )
            .ok();

        let mut enriched = 0i32;
        let mut errors = 0i32;
        let mut skipped = 0i32;
        for (track_id, mbid) in &track_ids {
            let url = format!(
                "https://musicbrainz.org/ws/2/recording/{mbid}?inc=artist-credits+artist-rels&fmt=json"
            );

            match state.http_client.get(&url).send().await {
                Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                    Ok(data) => {
                        let lignes = lignes_credits(&data);
                        if lignes.is_empty() {
                            // MusicBrainz connaît l'enregistrement mais n'a
                            // aucun crédit exploitable : ce n'est pas une
                            // erreur, et écraser les crédits existants par du
                            // vide serait une perte.
                            skipped += 1;
                        } else {
                            ecrire_credits(&backend, *track_id, &lignes);
                            enriched += 1;
                        }
                    }
                    Err(_) => errors += 1,
                },
                _ => errors += 1,
            }

            if (enriched + errors + skipped) % JALON_STATUT == 0 {
                reglages
                    .set(
                        REGLAGE_AVANCEMENT_CREDITS,
                        &statut_credits(
                            "running",
                            &task_id_clone,
                            enriched,
                            errors,
                            skipped,
                            total,
                        ),
                    )
                    .ok();
            }

            // MusicBrainz rate limit: 1 request/sec
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }

        reglages
            .set(
                REGLAGE_AVANCEMENT_CREDITS,
                &statut_credits("done", &task_id_clone, enriched, errors, skipped, total),
            )
            .ok();

        tracing::info!(task_id = %task_id_clone, enriched, errors, skipped, total, only_missing, "enrich_all_credits_done");
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "task_id": task_id,
            "only_missing": only_missing,
        })),
    )
}

/// GET /library/enrich-credits/status
///
/// Même forme que `/library/enrich-all/status`. Les compteurs sont rendus dans
/// TOUS les états, y compris `idle` : ne rendre que `status` au repos rendait
/// la réponse typée fausse et forçait chaque appelant à rattraper les champs
/// manquants (#1897).
pub(super) async fn enrich_credits_status(State(state): State<AppState>) -> Json<Value> {
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let resultat = reglages
        .get(REGLAGE_AVANCEMENT_CREDITS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or(json!({
            "status": "idle",
            "enriched": 0,
            "errors": 0,
            "skipped": 0,
            "total": 0,
        }));
    Json(resultat)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TÉMOIN ANTI-RÉGRESSION : sans `only_missing`, la sélection est celle
    /// d'avant, au caractère près — aucune piste ne disparaît de la passe.
    #[test]
    fn sans_only_missing_la_selection_ne_filtre_rien() {
        let q = requete_candidats(false);
        assert!(!q.contains("NOT EXISTS"), "{q}");
        assert!(!q.contains("track_credits"), "{q}");
        assert!(q.contains("musicbrainz_recording_id IS NOT NULL"), "{q}");
    }

    #[test]
    fn only_missing_exclut_les_pistes_deja_creditees() {
        let q = requete_candidats(true);
        assert!(
            q.contains("NOT EXISTS (SELECT 1 FROM track_credits tc WHERE tc.track_id = t.id)"),
            "{q}"
        );
        // Le socle historique reste : on RESTREINT, on ne remplace pas.
        assert!(q.contains("musicbrainz_recording_id IS NOT NULL"), "{q}");
    }
}
