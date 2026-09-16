//! Graver le Dynamic Range calculé par Tune dans les fichiers.
//!
//! Demandé par Bertrand le 16/09/2026, depuis l'écran Métadonnées : « un
//! bouton pour graver les DR sur les pistes ». Et la contrainte, dans la
//! foulée : « cette clé doit pouvoir être relue ».
//!
//! ## Ce que la passe fait, et ne fait pas
//!
//! `dr_track` a DEUX producteurs (voir `metadata/mod.rs`, #3924) : le scan,
//! qui lit `DYNAMIC RANGE=` dans l'en-tête Vorbis et se marque
//! `dr_source = "tag"` ; et la passe d'analyse (`audio::replaygain`), qui
//! CALCULE la valeur et se marque `dr_source = "analysis"`. La seconde ne vit
//! qu'en base : un autre lecteur, un autre serveur, ou une base refaite
//! repartent de zéro.
//!
//! Cette passe prend chaque piste à `dr_source = "analysis"` et écrit sa valeur
//! dans le fichier, sous la clé que le scan relit — et sous elle seulement.
//! Après quoi la piste est marquée `dr_source = "tag"` : c'est la vérité du
//! disque désormais, et c'est ce que le prochain scan trouvera.
//!
//! Elle ne touche PAS :
//!
//! * une piste dont le fichier porte déjà une valeur — le tag du disque fait
//!   foi, c'est la base qu'on aligne, pas le fichier (`GravureDr::DejaPresente`) ;
//! * un conteneur que le scan ne relit pas (MP3, M4A, WAV…) — graver ce que
//!   personne ne relit serait un faux « fait ». Ces pistes sont COMPTÉES à part
//!   (`hors_format`) pour que l'écran puisse le dire ;
//! * le DR d'album : la demande porte sur les pistes, et `ALBUM DYNAMIC RANGE`
//!   est une moyenne que l'écran sait déjà déduire (`DR ~12`).
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tracing::{debug, info, warn};
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::metadata::tag_writer::{GravureDr, format_dr_relu, graver_dr};
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::state::AppState;

/// Identifiant de la passe au registre `background_tasks` (#2129) : c'est lui
/// que le bandeau global affiche, et lui que la route refuse de doubler.
pub(crate) const TACHE_GRAVER_DR: &str = "graver_dr";
/// Réglage qui garde le dernier état, lisible après coup par `statut`.
const REGLAGE_STATUT: &str = "graver_dr_status";
/// Cadence de publication au registre : une par piste ferait un événement
/// WebSocket par fichier.
const JALON_AVANCEMENT: i32 = 25;

/// Les pistes CANDIDATES : un `dr_track` non vide calculé par Tune, sur un
/// fichier local. La sélection par conteneur se fait en Rust — l'extension
/// est une affaire de chemin, pas de SQL.
const SQL_CANDIDATES: &str = "SELECT t.id, t.file_path, m.value \
     FROM tracks t \
     JOIN track_metadata m ON m.track_id = t.id AND m.key = 'dr_track' \
     JOIN track_metadata s ON s.track_id = t.id AND s.key = 'dr_source' AND s.value = 'analysis' \
     WHERE t.file_path IS NOT NULL AND t.file_path != '' AND TRIM(m.value) != '' \
     ORDER BY t.id";

/// Ce qu'il y a à graver, mesuré maintenant. Sert à l'écran AVANT de lancer
/// (« 1 234 pistes à graver ») comme au rapport de fin.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Inventaire {
    /// Calculées par Tune, sur un conteneur que le scan relit : à graver.
    pub a_graver: usize,
    /// Calculées par Tune, mais sur un conteneur que le scan ne relit pas.
    pub hors_format: usize,
}

fn inventaire(state: &AppState) -> (Inventaire, Vec<(i64, String, String)>) {
    let rows = state
        .backend
        .query_many(SQL_CANDIDATES, &[])
        .ou_defaut_journalise();
    let mut inv = Inventaire::default();
    let mut candidates = Vec::with_capacity(rows.len());
    for row in &rows {
        let (Some(id), Some(chemin), Some(dr)) = (
            row.first().and_then(|v| v.as_i64()),
            row.get(1).and_then(|v| v.as_string()),
            row.get(2).and_then(|v| v.as_string()),
        ) else {
            continue;
        };
        if format_dr_relu(&chemin) {
            inv.a_graver += 1;
            candidates.push((id, chemin, dr));
        } else {
            inv.hors_format += 1;
        }
    }
    (inv, candidates)
}

/// Combien de pistes portent déjà leur DR DANS le fichier (`dr_source = tag`).
fn deja_dans_les_fichiers(state: &AppState) -> i64 {
    state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM track_metadata WHERE key = 'dr_source' AND value = 'tag'",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

fn en_cours(state: &AppState) -> bool {
    state
        .background_tasks
        .snapshot()
        .iter()
        .any(|t| t.id == TACHE_GRAVER_DR)
}

fn lire_statut(state: &AppState) -> Value {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(REGLAGE_STATUT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({"status": "idle"}))
}

fn ecrire_statut(state_backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>, v: &Value) {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state_backend.clone())
        .set(REGLAGE_STATUT, &v.to_string())
        .ok();
}

/// GET /library/dr/gravure
///
/// L'inventaire du moment ET le dernier état de la passe, en une réponse :
/// l'écran a besoin des deux pour dire « 1 234 à graver » avant, et
/// « 1 234 gravées, 3 déjà présentes, 0 erreur » après.
pub(crate) async fn statut(State(state): State<AppState>) -> Json<Value> {
    let (inv, _) = inventaire(&state);
    let mut v = lire_statut(&state);
    if en_cours(&state) {
        v["status"] = json!("running");
    }
    v["a_graver"] = json!(inv.a_graver);
    v["hors_format"] = json!(inv.hors_format);
    v["dans_les_fichiers"] = json!(deja_dans_les_fichiers(&state));
    Json(v)
}

/// POST /library/dr/gravure
///
/// Lance la passe en tâche de fond. 202 avec l'inventaire ; 409 si elle tourne
/// déjà — deux passes concurrentes réécriraient les mêmes fichiers.
pub(crate) async fn lancer(State(state): State<AppState>) -> impl IntoResponse {
    if en_cours(&state) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"status": "running", "error": "already running"})),
        );
    }
    let (inv, candidates) = inventaire(&state);
    let total = candidates.len();

    // Garde RAII prise AVANT le spawn : entre ce point et le premier tour de
    // boucle, un second POST verrait déjà « en cours ».
    let garde = state.background_tasks.begin(
        TACHE_GRAVER_DR,
        "Gravure du Dynamic Range dans les fichiers…",
        "maintenance",
    );
    let taches = state.background_tasks.clone();
    let backend = state.backend.clone();
    ecrire_statut(
        &backend,
        &json!({"status": "running", "total": total, "written": 0, "already": 0, "skipped": 0, "errors": 0}),
    );

    tokio::spawn(async move {
        let _garde = garde;
        let repo = TrackMetadataRepo::with_backend(backend.clone());
        let (mut written, mut already, mut skipped, mut errors) = (0i32, 0i32, 0i32, 0i32);
        taches.update_progress(TACHE_GRAVER_DR, 0, total as u64, "Dynamic Range");

        for (track_id, chemin, dr) in &candidates {
            let traitees = written + already + skipped + errors;
            if traitees % JALON_AVANCEMENT == 0 {
                taches.update_progress(
                    TACHE_GRAVER_DR,
                    traitees as u64,
                    total as u64,
                    "Dynamic Range",
                );
                ecrire_statut(
                    &backend,
                    &json!({"status": "running", "total": total, "written": written,
                            "already": already, "skipped": skipped, "errors": errors}),
                );
            }
            match graver_dr(chemin, dr).await {
                Ok(GravureDr::Ecrite) => {
                    written += 1;
                    // Le fichier porte la valeur : c'est désormais le tag qui
                    // fait foi, et c'est ce que le prochain scan dira aussi.
                    let _ = repo.set(*track_id, "dr_source", "tag");
                    debug!(track_id, chemin = %chemin, dr = %dr, "dr_grave");
                }
                Ok(GravureDr::DejaPresente(du_fichier)) => {
                    already += 1;
                    // La base s'aligne sur le disque, jamais l'inverse.
                    if du_fichier.chars().all(|c| c.is_ascii_digit()) && du_fichier != *dr {
                        let _ = repo.set(*track_id, "dr_track", &du_fichier);
                    }
                    let _ = repo.set(*track_id, "dr_source", "tag");
                    debug!(track_id, chemin = %chemin, du_fichier = %du_fichier, "dr_deja_present");
                }
                Err(e) if e == "file not found" => {
                    skipped += 1;
                    debug!(track_id, chemin = %chemin, "dr_fichier_introuvable");
                }
                Err(e) => {
                    errors += 1;
                    warn!(track_id, chemin = %chemin, error = %e, "dr_gravure_echouee");
                }
            }
        }

        taches.update_progress(TACHE_GRAVER_DR, total as u64, total as u64, "Dynamic Range");
        ecrire_statut(
            &backend,
            &json!({"status": "done", "total": total, "written": written,
                    "already": already, "skipped": skipped, "errors": errors}),
        );
        info!(
            total,
            written, already, skipped, errors, "graver_dr_termine"
        );
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "accepted", "total": total, "hors_format": inv.hors_format})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::db::backend::ToSqlValue;

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    fn piste(state: &AppState, id: i64, chemin: &str, dr: Option<(&str, &str)>) {
        let titre = format!("p{id}");
        let chemin = chemin.to_string();
        state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, file_path) VALUES (?1, ?2, ?3)",
                &[&id as &dyn ToSqlValue, &titre, &chemin],
            )
            .unwrap();
        if let Some((valeur, source)) = dr {
            let repo = TrackMetadataRepo::with_backend(state.backend.clone());
            repo.set(id, "dr_track", valeur).unwrap();
            repo.set(id, "dr_source", source).unwrap();
        }
    }

    /// L'inventaire ne retient que ce que Tune a CALCULÉ, sur un conteneur que
    /// le scan relit. Tout le reste est compté ailleurs ou pas du tout.
    #[test]
    fn inventaire_trie_par_provenance_et_par_conteneur() {
        let s = etat();
        piste(&s, 1, "/m/a.flac", Some(("12", "analysis"))); // à graver
        piste(&s, 2, "/m/b.opus", Some(("9", "analysis"))); // à graver
        piste(&s, 3, "/m/c.mp3", Some(("11", "analysis"))); // hors format
        piste(&s, 4, "/m/d.flac", Some(("13", "tag"))); // déjà dans le fichier
        piste(&s, 5, "/m/e.flac", Some(("", "analysis"))); // vide : pas une valeur
        piste(&s, 6, "/m/f.flac", None); // jamais mesurée
        let (inv, cand) = inventaire(&s);
        assert_eq!(
            inv,
            Inventaire {
                a_graver: 2,
                hors_format: 1
            }
        );
        assert_eq!(
            cand.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(deja_dans_les_fichiers(&s), 1);
    }

    /// La passe s'inscrit au registre (#2129) et ne se laisse pas doubler.
    #[tokio::test]
    async fn la_passe_s_inscrit_au_registre_et_refuse_un_doublon() {
        let s = etat();
        let r = lancer(State(s.clone())).await.into_response();
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        // La garde est prise avant le spawn : visible sans céder le fil.
        assert!(
            en_cours(&s),
            "registre : {:?}",
            s.background_tasks
                .snapshot()
                .iter()
                .map(|t| t.id.clone())
                .collect::<Vec<_>>()
        );
        let r2 = lancer(State(s.clone())).await.into_response();
        assert_eq!(r2.status(), StatusCode::CONFLICT);
    }

    /// Sur une vraie fixture : la passe grave, puis la base dit « tag » — et
    /// un second passage n'a plus rien à faire, sans erreur.
    #[tokio::test]
    async fn la_passe_grave_puis_bascule_la_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("x.flac");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../tune-core/tests/fixtures/test.flac"),
            &cible,
        )
        .unwrap();
        let s = etat();
        piste(&s, 1, cible.to_str().unwrap(), Some(("12", "analysis")));
        piste(&s, 2, "/introuvable/y.flac", Some(("8", "analysis")));

        let r = lancer(State(s.clone())).await.into_response();
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        // Attendre la fin de la tâche de fond.
        for _ in 0..200 {
            if !en_cours(&s) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(!en_cours(&s), "la passe n'a pas fini");

        let statut = lire_statut(&s);
        assert_eq!(statut["status"], "done");
        assert_eq!(statut["written"], 1);
        assert_eq!(statut["skipped"], 1, "{statut}");
        assert_eq!(statut["errors"], 0, "{statut}");

        // Relu par le lecteur du scan : la valeur est dans le fichier.
        let m = tune_core::metadata::read_extended_metadata(&cible);
        assert_eq!(m.get("dr_track").map(String::as_str), Some("12"));
        assert_eq!(m.get("dr_source").map(String::as_str), Some("tag"));
        // Et la base a basculé.
        let repo = TrackMetadataRepo::with_backend(s.backend.clone());
        assert_eq!(
            repo.get_all(1)
                .unwrap()
                .get("dr_source")
                .map(String::as_str),
            Some("tag")
        );
        // Plus rien à graver pour la piste 1 ; la 2 reste (introuvable ≠ gravée).
        let (inv, _) = inventaire(&s);
        assert_eq!(inv.a_graver, 1);
    }
}
