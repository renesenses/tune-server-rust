//! Paroles : l'indicateur de couverture et la passe de fond (issue #2172).
//!
//! Deux routes, dans l'ordre de risque :
//!
//! - `GET  /library/lyrics/status` — l'indicateur. Du SQL, aucun réseau, aucun
//!   accès disque : répond même quand rien n'a jamais tourné.
//! - `POST /library/lyrics/fetch` — la passe de fond.
//!
//! La mécanique suit celle de l'enrichissement des images d'artistes
//! (`routes/library/artwork.rs`) : bilan JSON dans un réglage, inscription au
//! registre `background_tasks` avec un garde RAII, route d'état séparée. Pas
//! de second mécanisme.
//!
//! **Pas de porte payante.** La cascade d'affichage des paroles n'en a pas
//! (`routes/library/tracks.rs` : « No premium gate: this is a display
//! feature ») ; la remplir en fond n'en introduit pas une.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use tune_core::library::lyrics_pass;

use crate::error::AppError;
use crate::state::AppState;

/// Identifiant de la tâche au registre `background_tasks`.
const TASK_ID: &str = "lyrics_fetch";

fn coverage_json(c: &lyrics_pass::LyricsCoverage) -> Value {
    let pct = if c.total_tracks > 0 {
        (c.with_lyrics as f64 / c.total_tracks as f64 * 100.0).round()
    } else {
        0.0
    };
    json!({
        "total_tracks": c.total_tracks,
        "with_lyrics": c.with_lyrics,
        "without_lyrics": c.without_lyrics,
        "from_lrc": c.from_lrc,
        "from_tag": c.from_tag,
        "from_lrclib": c.from_lrclib,
        "searched_no_result": c.searched_no_result,
        "never_searched": c.never_searched,
        "lyrics_pct": pct,
    })
}

/// GET /api/v1/library/lyrics/status
///
/// Ce que la bibliothèque sait de ses paroles — la moitié de l'issue #2172 qui
/// disait « rien ne sait ce qui en a ».
///
/// - `coverage` : les comptes par source, exclusifs et dans l'ordre de la
///   cascade d'affichage (`lrc` > `tag` > `lrclib`), plus la part sans paroles
///   séparée en « déjà cherchée sans résultat » / « jamais cherchée ».
/// - `lrclib_enabled` : le consentement, tel que la passe le lira.
/// - `result` : le bilan du dernier run, ou `null`.
pub(super) async fn lyrics_status(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let coverage = lyrics_pass::coverage(&state.backend).map_err(AppError::internal)?;
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let result = settings
        .get(lyrics_pass::SETTING_FILL_RESULT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    Ok(Json(json!({
        "coverage": coverage_json(&coverage),
        "lrclib_enabled": lyrics_pass::lrclib_consent_given(&state.backend),
        "result": result,
    })))
}

/// POST /api/v1/library/lyrics/fetch
///
/// Lance la passe de fond. Deux phases, la seconde sous condition :
///
/// 1. **Locale** — toujours. Repère les `.lrc` voisins et les inscrit, pour que
///    l'indicateur cesse de sous-compter. Aucun réseau.
/// 2. **LRCLIB** — seulement si `lyrics_lrclib_enabled` vaut `"true"`. La
///    réponse annonce lequel des deux cas s'applique (`"lrclib"`), plutôt que
///    de refuser tout le travail : un utilisateur qui ne veut pas de requêtes
///    distantes a quand même droit à son indicateur.
///
/// Réponse immédiate (202) ; l'avancement se lit sur
/// `GET /library/lyrics/status`.
pub(super) async fn lyrics_fetch(State(state): State<AppState>) -> impl IntoResponse {
    let consent = lyrics_pass::lrclib_consent_given(&state.backend);

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            lyrics_pass::SETTING_FILL_RESULT,
            &json!({"status": "running", "phase": "local", "lrclib": consent}).to_string(),
        )
        .ok();

    let task_guard =
        state
            .background_tasks
            .begin(TASK_ID, "Recherche des paroles manquantes…", "enrichment");
    let backend = state.backend.clone();
    let http = state.http_client.clone();
    let bg_tasks = state.background_tasks.clone();

    tokio::spawn(async move {
        let _task_guard = task_guard; // libère la tâche quand ce futur se termine

        // --- Phase 1 : locale (disque). Bloquante, donc hors du réacteur.
        let local_db = backend.clone();
        let local_tasks = bg_tasks.clone();
        let local = tokio::task::spawn_blocking(move || {
            lyrics_pass::run_local_index(&local_db, 0, |done, total| {
                local_tasks.update_progress(TASK_ID, done as u64, total as u64, "Fichiers .lrc");
            })
        })
        .await
        .unwrap_or_default();

        let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
        let write_result = |v: Value| {
            settings
                .set(lyrics_pass::SETTING_FILL_RESULT, &v.to_string())
                .ok();
        };

        if !consent {
            // Sans consentement on s'arrête là — et on le DIT, pour que
            // l'interface puisse proposer d'activer le réglage plutôt que
            // laisser croire à une passe qui n'a rien trouvé.
            write_result(json!({
                "status": "done",
                "phase": "done",
                "lrclib": false,
                "reason": "lrclib_disabled",
                "local_examined": local.examined,
                "local_found": local.found,
            }));
            return;
        }

        // --- Phase 2 : LRCLIB. Débit tenu par le limiteur partagé.
        write_result(json!({
            "status": "running",
            "phase": "lrclib",
            "lrclib": true,
            "local_examined": local.examined,
            "local_found": local.found,
        }));

        let progress_tasks = bg_tasks.clone();
        let report = lyrics_pass::run_lrclib_fill(
            &backend,
            lyrics_pass::FillOptions::production(),
            |r| {
                progress_tasks.update_progress(
                    TASK_ID,
                    r.requested as u64,
                    r.requested.max(1) as u64,
                    "LRCLIB",
                );
            },
            |cand| {
                let http = http.clone();
                async move { lyrics_pass::fetch_for_pass(&http, &cand).await }
            },
        )
        .await;

        write_result(json!({
            "status": "done",
            "phase": "done",
            "lrclib": true,
            "local_examined": local.examined,
            "local_found": local.found,
            "fill": report,
        }));
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            // Ce que la passe va réellement faire — pas ce qu'on aimerait
            // qu'elle fasse.
            "lrclib": consent,
            "message": if consent {
                "passe de fond démarrée (fichiers .lrc puis LRCLIB)"
            } else {
                "passe de fond démarrée (fichiers .lrc seulement — \
                 lyrics_lrclib_enabled n'est pas activé)"
            },
        })),
    )
}
