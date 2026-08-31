use crate::routes::panne_sql::OuDefautJournalise;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use tune_core::db::backend::ToSqlValue;
use tune_core::metadata::tag_writer::{TagUpdate, write_tags};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct WriteTagsRequest {
    /// If true (default), only write fields that are currently empty in
    /// the file's tags. If false, overwrite all fields from DB metadata.
    #[serde(default = "default_true")]
    pub only_missing: bool,
    /// Specific track IDs to process. `None` means all tracks.
    pub track_ids: Option<Vec<i64>>,
    /// Restrict to a single album's tracks. Ignored when `track_ids` is set.
    /// Lets the per-album "write tags" button target one album.
    pub album_id: Option<i64>,
}

fn default_true() -> bool {
    true
}

/// Identifiant de la passe au registre `background_tasks` (#2129).
///
/// Même raison que pour les crédits : `write_tags_status` et sa route ne se
/// lisent que depuis l'écran qui a lancé la passe. Réécrire les étiquettes de
/// toute une bibliothèque touche chaque fichier du disque et dure longtemps ;
/// sans inscription au registre, rien ne le signale ailleurs que là.
const TACHE_WRITE_TAGS: &str = "write_tags";

/// Cadence de publication de l'avancement au registre, alignée sur le jalon
/// que la passe utilise DÉJÀ pour écrire son statut. Une publication par piste
/// ferait un événement WebSocket par fichier.
const JALON_AVANCEMENT: i32 = 50;

/// POST /library/write-tags
///
/// Writes metadata from the DB back to audio files' tags using lofty.
/// Reads current file tags first, then only fills in missing fields
/// (when `only_missing` is true).
pub(crate) async fn write_tags_to_files(
    State(state): State<AppState>,
    Json(body): Json<WriteTagsRequest>,
) -> impl IntoResponse {
    let task_id = uuid::Uuid::new_v4().to_string();
    let backend = state.backend.clone();

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "write_tags_status",
            &json!({"status": "running", "task_id": task_id, "written": 0}).to_string(),
        )
        .ok();

    // Garde RAII pris avant le spawn, déplacé dedans : la tâche disparaît du
    // registre quand le futur se termine, panique comprise.
    let garde_tache = state.background_tasks.begin(
        TACHE_WRITE_TAGS,
        "Écriture des étiquettes dans les fichiers…",
        "maintenance",
    );
    let taches = state.background_tasks.clone();

    let backend2 = backend.clone();
    let task_id_clone = task_id.clone();
    let only_missing = body.only_missing;
    let track_ids = body.track_ids;
    let album_id = body.album_id;

    tokio::spawn(async move {
        let _garde_tache = garde_tache; // libère la tâche à la fin de ce futur
        // Build the SQL query based on whether specific track IDs were given
        let track_rows = if let Some(ref ids) = track_ids {
            if ids.is_empty() {
                vec![]
            } else {
                // Build IN clause with placeholders
                let placeholders: Vec<String> = ids.iter().map(|_| "?".to_string()).collect();
                let sql = format!(
                    "SELECT id, file_path, title, artist_name, album_title, \
                     track_number, disc_number, genre, composer, year, label, \
                     comment \
                     FROM tracks WHERE file_path IS NOT NULL AND id IN ({})",
                    placeholders.join(",")
                );
                let params: Vec<Box<dyn ToSqlValue>> = ids
                    .iter()
                    .map(|id| Box::new(*id) as Box<dyn ToSqlValue>)
                    .collect();
                let param_refs: Vec<&dyn ToSqlValue> = params.iter().map(|p| p.as_ref()).collect();
                backend2
                    .query_many(&sql, &param_refs)
                    .ou_defaut_journalise()
            }
        } else if let Some(aid) = album_id {
            // Single album: write tags for that album's tracks only.
            let params: Vec<Box<dyn ToSqlValue>> = vec![Box::new(aid) as Box<dyn ToSqlValue>];
            let param_refs: Vec<&dyn ToSqlValue> = params.iter().map(|p| p.as_ref()).collect();
            backend2
                .query_many(
                    "SELECT id, file_path, title, artist_name, album_title, \
                     track_number, disc_number, genre, composer, year, label, \
                     comment \
                     FROM tracks WHERE file_path IS NOT NULL AND album_id = ?",
                    &param_refs,
                )
                .ou_defaut_journalise()
        } else {
            backend2
                .query_many(
                    "SELECT id, file_path, title, artist_name, album_title, \
                     track_number, disc_number, genre, composer, year, label, \
                     comment \
                     FROM tracks WHERE file_path IS NOT NULL",
                    &[],
                )
                .ou_defaut_journalise()
        };

        let total = track_rows.len();
        let mut written = 0i32;
        let mut skipped = 0i32;
        let mut errors = 0i32;

        // Le total dès qu'il est connu. La sélection SQL précède la boucle :
        // publier ici évite un bandeau bloqué sur « 0/0 », que le client
        // n'affiche pas en fraction (il le lirait comme un arrêt).
        taches.update_progress(TACHE_WRITE_TAGS, 0, total as u64, "Étiquettes");

        for row in &track_rows {
            // Avancement au registre, en TÊTE de boucle — sur le nombre de
            // pistes RÉELLEMENT traitées, `skipped` compris.
            //
            // Deux raisons, et les deux comptent :
            //
            // - deux branches de ce corps sortent par `continue` (colonne
            //   `file_path` nulle, fichier introuvable sur le disque). Un jalon
            //   placé en queue les manquerait toutes ;
            // - le jalon du réglage, plus bas, ne compte que `written + errors`.
            //   Une bibliothèque dont les fichiers sont introuvables — chemins
            //   NFD, disque démonté — les compte tous en `skipped` : une barre
            //   indexée sur ce jalon-là resterait figée à 0 du début à la fin,
            //   c'est-à-dire précisément dans le cas où l'on a besoin de voir
            //   que la passe tourne.
            let traitees = written + skipped + errors;
            if traitees % JALON_AVANCEMENT == 0 {
                taches.update_progress(
                    TACHE_WRITE_TAGS,
                    traitees.max(0) as u64,
                    total as u64,
                    "Étiquettes",
                );
            }

            let track_id = row.get(0).and_then(|v| v.as_i64()).unwrap_or(0);
            let file_path = match row.get(1).and_then(|v| v.as_string()) {
                Some(fp) => fp,
                None => continue,
            };
            let title = row.get(2).and_then(|v| v.as_string());
            let artist_name = row.get(3).and_then(|v| v.as_string());
            let album_title = row.get(4).and_then(|v| v.as_string());
            let track_number = row.get(5).and_then(|v| v.as_i64()).map(|v| v as i32);
            let disc_number = row.get(6).and_then(|v| v.as_i64()).map(|v| v as i32);
            let genre = row.get(7).and_then(|v| v.as_string());
            let composer = row.get(8).and_then(|v| v.as_string());
            let year = row.get(9).and_then(|v| v.as_i64()).map(|v| v as i32);
            let _label = row.get(10).and_then(|v| v.as_string());
            let comment = row.get(11).and_then(|v| v.as_string());

            // Le fichier est-il là ? Pas `exists()` sur le chemin de la base :
            // il est en NFC et le disque peut porter le nom en NFD, auquel cas
            // la piste était comptée « sautée » alors qu'elle est présente
            // (#1865). `tag_writer` résout de son côté ; ce pré-contrôle doit
            // résoudre pareil, sinon il écarte avant même de l'appeler.
            if tune_core::library::local_path::resolve_local_path(&file_path).is_missing() {
                debug!(track_id, file_path = %file_path, "write_tags_file_not_found");
                skipped += 1;
                continue;
            }

            if only_missing {
                // Read current tags from file to check what's missing
                let current_tags = match tune_core::metadata::tag_writer::read_tags(&file_path)
                    .await
                {
                    Ok(tags) => tags,
                    Err(e) => {
                        warn!(track_id, file_path = %file_path, error = %e, "write_tags_read_failed");
                        errors += 1;
                        continue;
                    }
                };

                // Build update with only fields missing from the file
                let update = TagUpdate {
                    title: if current_tags.get("title").map_or(true, |v| v.is_empty()) {
                        title
                    } else {
                        None
                    },
                    artist_name: if current_tags.get("artist").map_or(true, |v| v.is_empty()) {
                        artist_name
                    } else {
                        None
                    },
                    album_title: if current_tags.get("album").map_or(true, |v| v.is_empty()) {
                        album_title
                    } else {
                        None
                    },
                    track_number: if current_tags
                        .get("tracknumber")
                        .map_or(true, |v| v.is_empty())
                    {
                        track_number
                    } else {
                        None
                    },
                    disc_number: if current_tags
                        .get("discnumber")
                        .map_or(true, |v| v.is_empty())
                    {
                        disc_number
                    } else {
                        None
                    },
                    genre: if current_tags.get("genre").map_or(true, |v| v.is_empty()) {
                        genre
                    } else {
                        None
                    },
                    composer: if current_tags.get("composer").map_or(true, |v| v.is_empty()) {
                        composer
                    } else {
                        None
                    },
                    year: if current_tags.get("date").map_or(true, |v| v.is_empty()) {
                        year
                    } else {
                        None
                    },
                    comment: if current_tags.get("comment").map_or(true, |v| v.is_empty()) {
                        comment
                    } else {
                        None
                    },
                    label: None, // label/isrc/bpm/lyrics handled by extended writer
                    isrc: None,
                    bpm: None,
                    lyrics: None,
                };

                // Skip if nothing to write
                if update.title.is_none()
                    && update.artist_name.is_none()
                    && update.album_title.is_none()
                    && update.track_number.is_none()
                    && update.disc_number.is_none()
                    && update.genre.is_none()
                    && update.composer.is_none()
                    && update.year.is_none()
                    && update.comment.is_none()
                {
                    skipped += 1;
                    continue;
                }

                match write_tags(&file_path, &update).await {
                    Ok(result) => {
                        written += 1;
                        debug!(
                            track_id,
                            file_path = %file_path,
                            fields = result.fields_written,
                            "tags_written"
                        );
                    }
                    Err(e) => {
                        warn!(track_id, file_path = %file_path, error = %e, "write_tags_failed");
                        errors += 1;
                    }
                }
            } else {
                // Overwrite mode: write all DB fields to file
                let update = TagUpdate {
                    title,
                    artist_name,
                    album_title,
                    track_number,
                    disc_number,
                    genre,
                    composer,
                    year,
                    comment,
                    label: None,
                    isrc: None,
                    bpm: None,
                    lyrics: None,
                };

                match write_tags(&file_path, &update).await {
                    Ok(result) => {
                        written += 1;
                        debug!(
                            track_id,
                            file_path = %file_path,
                            fields = result.fields_written,
                            "tags_written_overwrite"
                        );
                    }
                    Err(e) => {
                        warn!(track_id, file_path = %file_path, error = %e, "write_tags_failed");
                        errors += 1;
                    }
                }
            }

            // Update status periodically
            if (written + errors) % 50 == 0 {
                let settings =
                    tune_core::db::settings_repo::SettingsRepo::with_backend(backend2.clone());
                settings
                    .set(
                        "write_tags_status",
                        &json!({
                            "status": "running",
                            "task_id": task_id_clone,
                            "written": written,
                            "skipped": skipped,
                            "errors": errors,
                            "total": total,
                        })
                        .to_string(),
                    )
                    .ok();
            }
        }

        let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend2);
        settings
            .set(
                "write_tags_status",
                &json!({
                    "status": "done",
                    "task_id": task_id_clone,
                    "written": written,
                    "skipped": skipped,
                    "errors": errors,
                    "total": total,
                })
                .to_string(),
            )
            .ok();
        info!(
            task_id = %task_id_clone,
            written,
            skipped,
            errors,
            total,
            "write_tags_to_files done"
        );
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "accepted", "task_id": task_id})),
    )
}

/// GET /library/write-tags/status
pub(super) async fn write_tags_status(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let result = settings
        .get("write_tags_status")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({"status": "idle"}));
    Json(result)
}

/// Inscription de la passe d'écriture des étiquettes au registre des tâches de
/// fond (#2129).
///
/// **Hermétique : aucun accès disque, aucun appel réseau.** La base en mémoire
/// ne contient aucune piste, donc la passe n'ouvre aucun fichier.
///
/// Voir l'en-tête des essais de `credits.rs` pour la propriété du réacteur
/// mono-fil sur laquelle repose l'observation du registre.
#[cfg(test)]
mod tests_tache_de_fond_write_tags {
    use super::*;
    use crate::state::AppState;

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    fn demande() -> WriteTagsRequest {
        WriteTagsRequest {
            only_missing: true,
            track_ids: None,
            album_id: None,
        }
    }

    /// Réécrire les étiquettes de toute une bibliothèque touche chaque fichier
    /// du disque et dure longtemps. `write_tags_status` le disait, mais ce
    /// réglage ne se lit que depuis l'écran qui a lancé la passe : ailleurs,
    /// rien. C'est le défaut de forme que décrit #2129 — « un traitement long
    /// qui n'est visible que depuis l'écran qui l'a lancé se lit comme un
    /// traitement absent ».
    #[tokio::test]
    async fn la_passe_d_ecriture_des_etiquettes_s_inscrit_au_registre() {
        let state = etat();
        let _ = write_tags_to_files(State(state.clone()), Json(demande())).await;

        let ids: Vec<String> = state
            .background_tasks
            .snapshot()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(
            ids.contains(&TACHE_WRITE_TAGS.to_string()),
            "l'écriture des étiquettes doit figurer au registre des tâches de \
             fond, sinon le bandeau global ne peut pas l'afficher (#2129) — \
             registre observé : {ids:?}"
        );
    }

    /// Témoin anti-régression : la route garde son 202 et son `task_id`.
    #[tokio::test]
    async fn le_contrat_de_la_route_est_inchange() {
        let state = etat();
        let reponse = write_tags_to_files(State(state.clone()), Json(demande()))
            .await
            .into_response();
        assert_eq!(reponse.status(), StatusCode::ACCEPTED);

        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        let corps: Value = serde_json::from_slice(&octets).unwrap();
        assert_eq!(corps["status"], "accepted");
        assert!(corps["task_id"].as_str().is_some_and(|s| !s.is_empty()));
    }
}
