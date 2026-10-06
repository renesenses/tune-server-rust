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
///
/// 🔴 « Local » se lit sur la SOURCE. Le commentaire ci-dessus le disait déjà,
/// la requête ne le vérifiait pas : elle n'avait que
/// `t.file_path IS NOT NULL AND t.file_path != ''`, c'est-à-dire le chemin pris
/// pour un substitut de la source. Cette passe **réécrit les balises des
/// fichiers de l'utilisateur** ; le prédicat manquant est donc ajouté, sans
/// retirer celui du chemin — les deux questions sont distinctes.
fn sql_candidates() -> String {
    format!(
        "SELECT t.id, t.file_path, m.value \
         FROM tracks t \
         JOIN track_metadata m ON m.track_id = t.id AND m.key = 'dr_track' \
         JOIN track_metadata s ON s.track_id = t.id AND s.key = 'dr_source' AND s.value = 'analysis' \
         WHERE {piste_locale} \
           AND t.file_path IS NOT NULL AND t.file_path != '' AND TRIM(m.value) != '' \
         ORDER BY t.id",
        piste_locale = tune_core::db::track_repo::sql::PISTE_LOCALE,
    )
}

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
        .query_many(&sql_candidates(), &[])
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

/// État « la passe est morte avant d'avoir fini » (fil 2137, ticket 229).
///
/// Le réglage [`REGLAGE_STATUT`] est réécrit tous les [`JALON_AVANCEMENT`]
/// pistes avec `status = running`. Une passe tuée en route (redémarrage,
/// `kill`, panne) laisse donc derrière elle un `running` que plus personne ne
/// viendra changer. Le renvoyer tel quel grisait le bouton « Graver » à vie,
/// redémarrage compris. Les compteurs (`total`, `written`, `already`,
/// `skipped`, `errors`) sont gardés : ce sont ceux du dernier jalon écrit.
const ETAT_INTERROMPU: &str = "interrupted";

/// Le dernier état enregistré, corrigé par le registre de CE processus.
///
/// * tâche au registre → `running`, quoi que dise la base ;
/// * `running` en base sans tâche au registre → `interrupted`.
///
/// La relecture évite un faux « interrompu » à la fin normale d'une passe :
/// celle-ci écrit `done` AVANT de lâcher sa garde de registre. Si la tâche a
/// disparu entre la première lecture et le contrôle, la seconde lecture voit
/// donc déjà `done`.
fn statut_courant(state: &AppState) -> Value {
    let mut v = lire_statut(state);
    if en_cours(state) {
        v["status"] = json!("running");
        return v;
    }
    if v["status"] == "running" {
        v = lire_statut(state);
        if en_cours(state) {
            v["status"] = json!("running");
        } else if v["status"] == "running" {
            v["status"] = json!(ETAT_INTERROMPU);
        }
    }
    v
}

/// Au démarrage, aucune passe ne tourne encore dans ce processus : un
/// `running` en base est forcément l'héritage d'un processus mort. On le
/// réécrit `interrupted`, compteurs gardés, pour que la base dise la même
/// chose que `GET /library/dr/gravure`. Appelé par
/// `background::spawn_background_tasks`, avant que les routes ne servent.
pub(crate) fn marquer_passe_interrompue_au_demarrage(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) {
    let repo = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
    let Some(mut v) = repo
        .get(REGLAGE_STATUT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    else {
        return;
    };
    if v["status"] != "running" {
        return;
    }
    v["status"] = json!(ETAT_INTERROMPU);
    ecrire_statut(backend, &v);
    info!(etat = %v, "graver_dr_passe_interrompue");
}

/// GET /library/dr/gravure
///
/// L'inventaire du moment ET le dernier état de la passe, en une réponse :
/// l'écran a besoin des deux pour dire « 1 234 à graver » avant, et
/// « 1 234 gravées, 3 déjà présentes, 0 erreur » après.
///
/// `status` vaut `idle`, `running`, `done` ou `interrupted` (voir
/// [`ETAT_INTERROMPU`]).
pub(crate) async fn statut(State(state): State<AppState>) -> Json<Value> {
    let (inv, _) = inventaire(&state);
    let mut v = statut_courant(&state);
    v["a_graver"] = json!(inv.a_graver);
    v["hors_format"] = json!(inv.hors_format);
    v["dans_les_fichiers"] = json!(deja_dans_les_fichiers(&state));
    v[crate::routes::ecriture_fichiers::CHAMP_REPONSE] =
        json!(crate::routes::ecriture_fichiers::autorisee(&state));
    Json(v)
}

/// Fil 2134 (Levente Toth) — après la gravure, la ligne de la piste reprend
/// la taille et la date du fichier réécrit.
///
/// Seul le tag `DYNAMIC RANGE` a changé, et la base porte déjà sa valeur.
/// Sans cela, le surveillant voyait chaque fichier gravé comme modifié : il
/// le relisait, relisait le dossier entier quand une feuille CUE l'accompagne,
/// et annonçait « bibliothèque modifiée » à chaque lot (1 121 fichiers gravés,
/// une annonce toutes les 0,96 s chez le client). Le scan complet, lui, aurait
/// relu les mêmes fichiers pour rien.
fn remettre_la_ligne_en_phase(pistes: &tune_core::db::track_repo::TrackRepo, chemin: &str) {
    let Some((taille, mtime)) =
        tune_core::audio::iso9660::taille_et_mtime(std::path::Path::new(chemin))
    else {
        return;
    };
    if let Err(e) = pistes.update_mtime_and_size(chemin, mtime, taille as i64) {
        warn!(chemin = %chemin, error = %e, "dr_ligne_non_remise_en_phase");
    }
}

/// POST /library/dr/gravure
///
/// Lance la passe en tâche de fond. 202 avec l'inventaire ; 409 si elle tourne
/// déjà — deux passes concurrentes réécriraient les mêmes fichiers.
pub(crate) async fn lancer(State(state): State<AppState>) -> axum::response::Response {
    // La gravure n'a pas d'autre effet que d'écrire dans les fichiers :
    // désactivée (le défaut), elle refuse — le DR reste en base.
    if !crate::routes::ecriture_fichiers::autorisee(&state) {
        return crate::routes::ecriture_fichiers::refus("graver_dr");
    }
    if en_cours(&state) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"status": "running", "error": "already running"})),
        )
            .into_response();
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
        let pistes = tune_core::db::track_repo::TrackRepo::with_backend(backend.clone());
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
            // Fil 2134 — la ligne était-elle d'accord avec le disque AVANT
            // la gravure ? Seulement alors, elle peut être remise d'accord
            // après : sinon un changement venu d'ailleurs, pas encore relu,
            // passerait pour l'écriture de Tune.
            let en_phase = crate::auto_scan::fichier_conforme_a_la_base(&backend, chemin);
            match graver_dr(chemin, dr).await {
                Ok(GravureDr::Ecrite) => {
                    written += 1;
                    // Le fichier porte la valeur : c'est désormais le tag qui
                    // fait foi, et c'est ce que le prochain scan dira aussi.
                    let _ = repo.set(*track_id, "dr_source", "tag");
                    if en_phase {
                        remettre_la_ligne_en_phase(&pistes, chemin);
                    }
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
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::db::backend::ToSqlValue;

    fn etat() -> AppState {
        let s = AppState::new(":memory:", 0, Default::default()).unwrap();
        crate::routes::ecriture_fichiers::activer_pour_test(&s.backend);
        s
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

    /// Réglage « Écrire les modifications dans les fichiers audio » jamais
    /// touché : 409, rien au registre, aucun fichier ouvert.
    #[tokio::test]
    async fn reglage_absent_la_gravure_refuse() {
        let s = AppState::new(":memory:", 0, Default::default()).unwrap();
        piste(&s, 1, "/m/a.flac", Some(("12", "analysis")));
        let r = lancer(State(s.clone())).await.into_response();
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert!(!en_cours(&s));
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

/// Fil 2137 / ticket 229 : une passe morte en route ne doit pas griser le
/// bouton à vie.
#[cfg(test)]
mod tests_passe_interrompue_2137 {
    use super::*;

    fn etat() -> AppState {
        let s = AppState::new(":memory:", 0, Default::default()).unwrap();
        crate::routes::ecriture_fichiers::activer_pour_test(&s.backend);
        s
    }

    /// La photo qu'une passe tuée laisse en base : le jalon des 50 pistes.
    fn photo_d_une_passe_morte(s: &AppState) {
        ecrire_statut(
            &s.backend,
            &json!({"status": "running", "total": 4046, "written": 40,
                    "already": 10, "skipped": 0, "errors": 0}),
        );
    }

    #[tokio::test]
    async fn running_sans_tache_au_registre_est_rendu_interrupted() {
        let s = etat();
        photo_d_une_passe_morte(&s);
        assert!(!en_cours(&s));
        let Json(v) = statut(State(s.clone())).await;
        assert_eq!(v["status"], "interrupted", "{v}");
        // Le dernier compteur est gardé, pour « interrompue à 50 / 4 046 ».
        assert_eq!(v["total"], 4046, "{v}");
        assert_eq!(v["written"], 40, "{v}");
        assert_eq!(v["already"], 10, "{v}");
        // Et la relance n'est pas refusée.
        let r = lancer(State(s.clone())).await.into_response();
        assert_eq!(r.status(), StatusCode::ACCEPTED);
    }

    /// L'AUTRE sens : une passe vivante reste `running`.
    #[tokio::test]
    async fn running_avec_tache_au_registre_reste_running() {
        let s = etat();
        photo_d_une_passe_morte(&s);
        let _garde = s
            .background_tasks
            .begin(TACHE_GRAVER_DR, "Gravure", "maintenance");
        let Json(v) = statut(State(s.clone())).await;
        assert_eq!(v["status"], "running", "{v}");
    }

    #[test]
    fn le_demarrage_reecrit_running_en_interrupted_compteurs_gardes() {
        let s = etat();
        photo_d_une_passe_morte(&s);
        marquer_passe_interrompue_au_demarrage(&s.backend);
        let v = lire_statut(&s);
        assert_eq!(v["status"], "interrupted", "{v}");
        assert_eq!(v["total"], 4046, "{v}");
        assert_eq!(v["written"], 40, "{v}");
    }

    #[test]
    fn le_demarrage_ne_touche_ni_done_ni_un_reglage_absent() {
        let s = etat();
        marquer_passe_interrompue_au_demarrage(&s.backend);
        assert_eq!(lire_statut(&s), json!({"status": "idle"}));
        let fini = json!({"status": "done", "total": 3, "written": 3,
                          "already": 0, "skipped": 0, "errors": 0});
        ecrire_statut(&s.backend, &fini);
        marquer_passe_interrompue_au_demarrage(&s.backend);
        assert_eq!(lire_statut(&s), fini);
    }
}

/// Fil 2134 (Levente Toth) — la gravure ne doit pas faire réimporter au
/// surveillant les fichiers qu'elle vient de réécrire.
#[cfg(test)]
mod tests_ligne_en_phase_2134 {
    use super::*;
    use tune_core::db::backend::ToSqlValue;

    /// Une copie de la fixture FLAC, indexée avec un DR calculé. `en_phase` :
    /// la ligne porte la taille et la date du disque ; sinon, une date
    /// d'avant (un changement venu d'ailleurs que le surveillant n'a pas
    /// encore relu).
    fn banc(en_phase: bool) -> (tempfile::TempDir, String, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("x.flac");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../tune-core/tests/fixtures/test.flac"),
            &cible,
        )
        .unwrap();
        let chemin = cible.to_string_lossy().into_owned();
        let s = AppState::new(":memory:", 0, Default::default()).unwrap();
        crate::routes::ecriture_fichiers::activer_pour_test(&s.backend);
        let (taille, mtime) = tune_core::audio::iso9660::taille_et_mtime(&cible).unwrap();
        let mtime = if en_phase { mtime } else { mtime - 3600.0 };
        let taille = taille as i64;
        s.backend
            .execute(
                "INSERT INTO tracks (id, title, file_path, file_size, file_mtime) \
                 VALUES (1, 'x', ?1, ?2, ?3)",
                &[&chemin as &dyn ToSqlValue, &taille, &mtime],
            )
            .unwrap();
        let repo = TrackMetadataRepo::with_backend(s.backend.clone());
        repo.set(1, "dr_track", "12").unwrap();
        repo.set(1, "dr_source", "analysis").unwrap();
        (dir, chemin, s)
    }

    async fn graver(s: &AppState) {
        let r = lancer(State(s.clone())).await.into_response();
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        for _ in 0..200 {
            if !en_cours(s) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(!en_cours(s), "la passe n'a pas fini");
        assert_eq!(lire_statut(s)["written"], 1, "{}", lire_statut(s));
    }

    /// Le fichier a bien été réécrit (sa date a changé), et pourtant sa
    /// ligne est d'accord avec le disque : le surveillant l'écartera de son
    /// lot, sans relecture ni annonce.
    #[tokio::test]
    async fn le_fichier_grave_reste_conforme_a_sa_ligne_2134() {
        let (_dir, chemin, s) = banc(true);
        let stat = || tune_core::audio::iso9660::taille_et_mtime(std::path::Path::new(&chemin));
        let avant = stat();
        assert!(crate::auto_scan::fichier_conforme_a_la_base(
            &s.backend, &chemin
        ));
        graver(&s).await;
        assert_ne!(avant, stat(), "témoin : la gravure a réécrit le fichier");
        assert!(
            crate::auto_scan::fichier_conforme_a_la_base(&s.backend, &chemin),
            "après la gravure, la ligne doit porter la taille et la date du fichier gravé"
        );
    }

    /// Une ligne qui n'était PAS d'accord avec le disque avant la gravure le
    /// reste : le changement venu d'ailleurs sera relu par le surveillant ou
    /// le prochain scan, au lieu de passer pour l'écriture de Tune.
    #[tokio::test]
    async fn une_ligne_deja_en_retard_n_est_pas_remise_en_phase_2134() {
        let (_dir, chemin, s) = banc(false);
        assert!(!crate::auto_scan::fichier_conforme_a_la_base(
            &s.backend, &chemin
        ));
        graver(&s).await;
        assert!(!crate::auto_scan::fichier_conforme_a_la_base(
            &s.backend, &chemin
        ));
    }
}

/// Témoins de la règle « bibliothèque LOCALE » — Bertrand, 27/09/2026.
#[cfg(test)]
mod tests_source_locale_20260927 {
    use super::*;
    use tune_core::db::backend::ToSqlValue;

    fn banc() -> AppState {
        let s = AppState::new(":memory:", 0, Default::default()).expect("état");
        let repo = TrackMetadataRepo::with_backend(s.backend.clone());
        // 🔴 La piste distante porte un chemin ET un DR calculé : le cas que la
        // base de Bertrand ne contient pas, et sans lequel le filtre par chemin
        // rendrait le même résultat.
        for (id, chemin, source) in [(1i64, "/m/a.flac", "local"), (2, "/u/b.flac", "upnp")] {
            let c = chemin.to_string();
            let src = source.to_string();
            s.backend
                .execute(
                    "INSERT INTO tracks (id, title, file_path, source) VALUES (?1, ?2, ?3, ?4)",
                    &[&id as &dyn ToSqlValue, &format!("p{id}"), &c, &src],
                )
                .expect("piste");
            repo.set(id, "dr_track", "12").unwrap();
            repo.set(id, "dr_source", "analysis").unwrap();
        }
        s
    }

    /// 🔴 Cette passe RÉÉCRIT LES BALISES DES FICHIERS.
    #[test]
    fn la_gravure_dr_ecarte_une_piste_non_locale_qui_porte_un_chemin() {
        let (inv, cand) = inventaire(&banc());
        let ids: Vec<i64> = cand.iter().map(|(id, _, _)| *id).collect();
        assert!(
            !ids.contains(&2),
            "la piste 2 est `source = upnp` : son fichier ne doit PAS être \
             réécrit — candidates {ids:?}"
        );
        assert_eq!(
            inv.a_graver, 1,
            "l'écran annoncerait sinon une piste de plus qu'il ne doit graver"
        );
    }

    /// L'AUTRE sens : sans lui, un filtre qui rejette tout serait vert.
    #[test]
    fn la_gravure_dr_garde_la_piste_locale() {
        let (_, cand) = inventaire(&banc());
        let ids: Vec<i64> = cand.iter().map(|(id, _, _)| *id).collect();
        assert!(
            ids.contains(&1),
            "la piste LOCALE 1 doit rester candidate — candidates {ids:?}"
        );
    }
}
