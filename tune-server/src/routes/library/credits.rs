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

/// Identifiant de la passe au registre `background_tasks` (#2129).
///
/// Le réglage `REGLAGE_AVANCEMENT_CREDITS` et sa route `/enrich-credits/status`
/// ne se lisent que depuis l'écran qui a lancé la passe. Le registre, lui, est
/// le PASSE-PARTOUT : `GET /system/background-tasks` et l'événement
/// `system.background_tasks` alimentent le bandeau global du client, visible
/// quelle que soit la vue. Sans inscription ici, une passe qui dure des heures
/// (1 req/s côté MusicBrainz) est indistinguable d'une passe absente dès qu'on
/// quitte les Réglages — c'est le reproche de Bilou.
///
/// Aucun second compteur n'est fabriqué : les chiffres publiés sont EXACTEMENT
/// ceux que la passe écrivait déjà dans le réglage.
const TACHE_CREDITS: &str = "credits_enrich";

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

/// Pistes déjà traitées — le numérateur du bandeau global.
///
/// Les trois compteurs de la passe sont disjoints (une piste est enrichie, OU
/// sautée, OU en erreur), et leur somme est exactement le dénombrement que le
/// jalon de statut utilise déjà pour décider quand écrire. Extrait ici pour
/// être jouable par un test : un numérateur qui ne compterait que `enriched`
/// ferait stagner la barre sur une bibliothèque où MusicBrainz ne rend aucun
/// crédit — le « compteur bloqué » déjà mesuré sur les images d'artistes.
fn traitees(enriched: i32, errors: i32, skipped: i32) -> u64 {
    (enriched + errors + skipped).max(0) as u64
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

    // Garde RAII pris AVANT le spawn et déplacé dedans : il retire la tâche du
    // registre quand le futur se termine, y compris en panique. Un drapeau posé
    // à `running` et jamais remis serait la « tâche perpétuelle fantôme » que ce
    // registre existe pour éviter.
    let garde_tache =
        state
            .background_tasks
            .begin(TACHE_CREDITS, "Enrichissement des crédits…", "enrichment");
    let taches = state.background_tasks.clone();

    tokio::spawn(async move {
        let _garde_tache = garde_tache; // libère la tâche à la fin de ce futur
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
        // Même chiffre, même instant, dans le registre : le bandeau global
        // n'affiche une fraction qu'une fois le total connu (0/0 se lit comme
        // un arrêt côté client).
        taches.update_progress(TACHE_CREDITS, 0, total as u64, "Crédits");

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
                // Le registre suit le MÊME jalon que le réglage : une seule
                // cadence, donc jamais deux avancements qui se contredisent.
                taches.update_progress(
                    TACHE_CREDITS,
                    traitees(enriched, errors, skipped),
                    total as u64,
                    "Crédits",
                );
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

/// Inscription de la passe des crédits au registre des tâches de fond (#2129).
///
/// **Hermétique : aucun appel réseau.** La base en mémoire ne contient aucune
/// piste portant un `musicbrainz_recording_id`, donc la passe n'a aucun
/// candidat et ne joint jamais MusicBrainz.
///
/// Ce que ces essais observent tient à une propriété du réacteur mono-fil de
/// `#[tokio::test]` : une tâche déposée par `tokio::spawn` n'est sondée qu'au
/// premier point de rendez-vous du test. Entre le retour du gestionnaire et
/// l'assertion, il n'y a aucun `.await` — on lit donc le registre exactement
/// dans l'état où le trouve un client qui l'interroge pendant que la passe
/// travaille, sans dépendre d'un ordonnancement.
#[cfg(test)]
mod tests_tache_de_fond_credits {
    use super::*;
    use crate::state::AppState;

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    fn identifiants(state: &AppState) -> Vec<String> {
        state
            .background_tasks
            .snapshot()
            .into_iter()
            .map(|t| t.id)
            .collect()
    }

    /// Le défaut de #2129 : la passe des crédits persistait bien son avancement
    /// dans `settings` et le rendait sur `/library/enrich-credits/status`, mais
    /// ne s'inscrivait NULLE PART au registre `background_tasks`. Or c'est ce
    /// registre — et lui seul — qui alimente `GET /system/background-tasks`,
    /// l'événement `system.background_tasks` et le bandeau global du client.
    ///
    /// Conséquence mesurable : une passe qui tient la cadence MusicBrainz d'une
    /// requête par seconde, donc des heures sur une grosse bibliothèque, était
    /// indistinguable d'une passe absente dès qu'on quittait les Réglages.
    #[tokio::test]
    async fn la_passe_de_credits_s_inscrit_au_registre() {
        let state = etat();
        let _ = enrich_all_credits(State(state.clone()), None).await;

        let ids = identifiants(&state);
        assert!(
            ids.contains(&TACHE_CREDITS.to_string()),
            "la passe des crédits doit figurer au registre des tâches de fond, \
             sinon le bandeau global du client ne peut pas l'afficher (#2129) — \
             registre observé : {ids:?}"
        );
    }

    /// Le bandeau affiche le libellé du serveur mot pour mot : il ne le traduit
    /// pas et n'en fabrique pas un. Un libellé vide ferait retomber le client
    /// sur son texte de secours générique — « un enrichissement est en cours »
    /// — qui ne dit pas LEQUEL, et c'est justement ce que Bilou reprochait.
    #[tokio::test]
    async fn la_tache_porte_un_libelle_non_vide() {
        let state = etat();
        let _ = enrich_all_credits(State(state.clone()), None).await;

        let tache = state
            .background_tasks
            .snapshot()
            .into_iter()
            .find(|t| t.id == TACHE_CREDITS)
            .expect("la passe doit être inscrite");
        assert!(
            !tache.label.trim().is_empty(),
            "sans libellé, le bandeau retombe sur son texte de secours générique"
        );
    }

    /// Témoin anti-régression : la route garde son contrat d'origine. Inscrire
    /// la passe au registre ne doit changer ni le code de retour, ni le
    /// `task_id` que le client conserve pour interroger `/status`.
    #[tokio::test]
    async fn le_contrat_de_la_route_est_inchange() {
        use axum::response::IntoResponse;

        let state = etat();
        let reponse = enrich_all_credits(State(state.clone()), None)
            .await
            .into_response();
        assert_eq!(
            reponse.status(),
            StatusCode::ACCEPTED,
            "la route répond toujours 202"
        );

        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        let corps: Value = serde_json::from_slice(&octets).unwrap();
        assert_eq!(corps["status"], "accepted");
        assert!(
            corps["task_id"].as_str().is_some_and(|s| !s.is_empty()),
            "le task_id reste rendu : c'est la clef de /enrich-credits/status"
        );
    }

    /// Le numérateur du bandeau compte les pistes TRAITÉES, pas les seules
    /// réussites. Sur une bibliothèque où MusicBrainz ne rend aucun crédit
    /// exploitable, tout part en `skipped` : un numérateur limité à `enriched`
    /// afficherait « 0/4000 » du début à la fin, soit le compteur bloqué déjà
    /// mesuré sur les images d'artistes.
    #[test]
    fn le_numerateur_compte_les_pistes_sautees_et_en_erreur() {
        assert_eq!(traitees(0, 0, 37), 37, "37 pistes sautées sont 37 traitées");
        assert_eq!(traitees(5, 2, 3), 10);
        assert_eq!(traitees(0, 0, 0), 0);
    }
}
