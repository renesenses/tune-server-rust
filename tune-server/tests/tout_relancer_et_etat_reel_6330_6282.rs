//! #6330 et #6282 — « Tout relancer » relance vraiment, et un enrichissement
//! suspendu se dit suspendu. Par les ROUTES montées (`routes::router`).
//!
//! * #6330 (Benjithom, fil 2270) : ReplayGain suspendu à 99 % avec 84 pistes
//!   en échec de mesure, plage dynamique suspendue à 95 % avec 79 pistes
//!   illisibles ; « Tout relancer » ne touchait ni à l'un ni à l'autre.
//! * #6282 (Kimon, fil 2259) : enrichissement suspendu, garé vivant au
//!   registre ; les Réglages et le bandeau disaient « en cours ».
//!
//! **Hermétique** : aucune requête MusicBrainz ne part. La seule passe
//! d'enrichissement lancée l'est sous pause et se gare avant sa première
//! requête ; elle meurt avec le runtime de l'essai.
//!
//! Les pauses sont un état GLOBAL du processus : les essais sont sérialisés
//! par [`UN_ESSAI_A_LA_FOIS`], et ce fichier est une cible à part.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::taches_de_fond::{self, Tache};
use tune_server::state::AppState;

static UN_ESSAI_A_LA_FOIS: Mutex<()> = Mutex::new(());

fn base_neuve(nom: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("tune-6330-{nom}-"))
        .tempdir()
        .expect("dossier temporaire");
    let base = dir.path().join("tune.db");
    (dir, base)
}

/// Un « processus » : son runtime, son `AppState`, le miroir des pauses relu.
/// Le runtime détruit à la sortie emporte toute passe garée.
fn processus<T>(nom: &str, corps: impl AsyncFnOnce(AppState) -> T) -> T {
    let (_dir, base) = base_neuve(nom);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let chemin = base.to_str().expect("chemin UTF-8").to_string();
    let sortie = rt.block_on(async move {
        let state = AppState::new(&chemin, 0, Default::default()).expect("état");
        taches_de_fond::oublier_pour_les_essais();
        taches_de_fond::hydrater(&state.backend);
        corps(state).await
    });
    rt.shutdown_timeout(Duration::from_secs(2));
    sortie
}

async fn appel(state: &AppState, methode: &str, chemin: &str) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .body(Body::empty())
        .expect("requête");
    let reponse = tune_server::routes::router(state.clone())
        .oneshot(requete)
        .await
        .expect("réponse");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps");
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn compter(state: &AppState, cle: &str) -> i64 {
    state
        .backend
        .query_one(
            &format!("SELECT COUNT(*) FROM track_metadata WHERE key = '{cle}'"),
            &[],
        )
        .expect("comptage")
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .expect("un entier")
}

fn carte<'a>(instantane: &'a Value, id: &str) -> &'a Value {
    instantane["pausable"]
        .as_array()
        .expect("pausable")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("carte {id} absente : {instantane}"))
}

/// La bibliothèque du fil 2270, en miniature : une piste mesurée, une piste en
/// échec de mesure ReplayGain, une piste mesurée mais dont la plage dynamique
/// est marquée indisponible.
fn bibliotheque_du_fil_2270(state: &AppState) {
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Benjithom'); \
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Fil 2270', 1); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) VALUES \
               (10, 'Mesurée', 1, 1, '/m-6330/a/01.flac', 'local'), \
               (11, 'Échec RG', 1, 1, '/m-6330/a/02.flac', 'local'), \
               (12, 'Échec DR', 1, 1, '/m-6330/a/03.flac', 'local'); \
             INSERT INTO track_metadata (track_id, key, value) VALUES \
               (10, 'rg_analyzed', '1760000000'), (10, 'rg_track_gain', '-7.20 dB'), \
               (10, 'dr_track', '11'), \
               (11, 'rg_analyzed', '1760000000'), \
               (12, 'rg_analyzed', '1760000000'), (12, 'rg_track_gain', '-5.00 dB'), \
               (12, 'dr_indisponible', '1760000000');",
        )
        .expect("bibliothèque");
}

/// 🔴 #6330 — l'état photographié par Benjithom : ReplayGain, plage
/// dynamique, enrichissement et images d'artistes suspendus un à un, des
/// pistes en échec. « Tout relancer » doit TOUT reprendre — ReplayGain
/// compris — et rendre les pistes en échec à la cascade.
#[test]
fn tout_relancer_reprend_les_passes_suspendues_et_rend_les_pistes_en_echec() {
    let _un = UN_ESSAI_A_LA_FOIS.lock().unwrap_or_else(|e| e.into_inner());
    processus("tout-relancer", async |state| {
        bibliotheque_du_fil_2270(&state);
        for t in [
            Tache::ReplayGain,
            Tache::PlageDynamique,
            Tache::Enrichissement,
            Tache::ImagesArtistes,
        ] {
            taches_de_fond::mettre_en_pause(&state.backend, t).expect("pause");
        }
        let (_, avant) = appel(&state, "GET", "/api/v1/system/background-tasks").await;
        assert_eq!(carte(&avant, "replaygain")["state"], "en_pause");
        assert_eq!(
            avant["all_paused"], false,
            "l'interrupteur général ne s'allume pas"
        );

        let (statut, corps) = appel(
            &state,
            "POST",
            "/api/v1/system/background-tasks/relaunch-all",
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");

        for id in ["replaygain", "dynamic_range", "enrichment", "artist_images"] {
            assert_eq!(
                carte(&corps, id)["paused"],
                false,
                "« Tout relancer » doit reprendre {id}, suspendu : {corps}"
            );
        }
        for t in Tache::TOUTES {
            assert!(!taches_de_fond::est_en_pause(t), "{t:?} encore suspendu");
        }
        assert_eq!(
            corps["resumed"],
            serde_json::json!(["replaygain", "dynamic_range", "enrichment", "artist_images"]),
            "{corps}"
        );
        assert_eq!(
            corps["retried"]["replaygain"], 1,
            "la piste en échec de mesure ReplayGain doit être rendue à la passe : {corps}"
        );
        assert_eq!(
            corps["retried"]["dynamic_range"], 1,
            "la piste à la plage dynamique indisponible doit être rendue : {corps}"
        );

        // En base : seules les marques d'échec sont parties.
        assert_eq!(compter(&state, "dr_indisponible"), 0);
        assert_eq!(
            compter(&state, "rg_analyzed"),
            2,
            "10 et 12 gardent leur témoin"
        );
        assert_eq!(compter(&state, "rg_track_gain"), 2, "aucun gain effacé");
        assert_eq!(compter(&state, "dr_track"), 1, "aucune plage effacée");

        // La jauge ReplayGain ne compte plus d'échec.
        let (_, rg) = appel(&state, "GET", "/api/v1/system/replaygain/progress").await;
        if !rg["library_failed"].is_null() {
            assert_eq!(rg["library_failed"], 0, "{rg}");
        }

        // Idempotent : un second clic ne trouve plus rien à rendre.
        let (_, encore) = appel(
            &state,
            "POST",
            "/api/v1/system/background-tasks/relaunch-all",
        )
        .await;
        assert_eq!(encore["retried"]["replaygain"], 0);
        assert_eq!(encore["retried"]["dynamic_range"], 0);
        assert_eq!(encore["resumed"], serde_json::json!([]));
    });
}

/// 🔴 #6282 — l'enrichissement suspendu, garé vivant au registre, se dit
/// suspendu PARTOUT : dans `enrich-all/status` (les Réglages) et dans la
/// tâche du registre (le bandeau), comme sur sa carte.
#[test]
fn un_enrichissement_garde_se_dit_en_pause_partout() {
    let _un = UN_ESSAI_A_LA_FOIS.lock().unwrap_or_else(|e| e.into_inner());
    processus("enrich-garee", async |state| {
        state
            .backend
            .execute_batch(
                "INSERT INTO artists (id, name) VALUES (1, 'Kimon'); \
                 INSERT INTO albums (id, title, artist_id) VALUES (1, 'Fil 2259', 1); \
                 INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) \
                   VALUES (10, 'T', 1, 1, '/m-6282/a/01.flac', 'local');",
            )
            .expect("candidat");
        taches_de_fond::mettre_en_pause(&state.backend, Tache::Enrichissement).expect("pause");
        let (statut, corps) = appel(&state, "POST", "/api/v1/library/enrich-all").await;
        assert_eq!(statut, StatusCode::ACCEPTED, "{corps}");
        // Laisser la passe atteindre sa frontière et s'y garer.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let (_, inst) = appel(&state, "GET", "/api/v1/system/background-tasks").await;
        assert_eq!(carte(&inst, "enrichment")["state"], "en_pause");
        let tache = inst["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|t| t["id"] == "enrich_all")
            .unwrap_or_else(|| panic!("la passe garée reste inscrite : {inst}"));
        assert_eq!(
            tache["paused"], true,
            "la tâche garée doit se dire suspendue au bandeau : {tache}"
        );

        let (_, st) = appel(&state, "GET", "/api/v1/library/enrich-all/status").await;
        assert_eq!(
            st["status"], "paused",
            "les Réglages disaient « En cours… » sur une passe garée : {st}"
        );
        assert_eq!(st["paused"], true, "{st}");
    });
}

/// #6282, le cas voisin : « running » laissé par une passe qu'un arrêt a tuée
/// n'est pas « en cours ». Et le témoin : une passe vivante et non suspendue
/// reste « running ».
#[test]
fn running_sans_passe_vivante_n_est_pas_en_cours() {
    let _un = UN_ESSAI_A_LA_FOIS.lock().unwrap_or_else(|e| e.into_inner());
    processus("enrich-morte", async |state| {
        SettingsRepo::with_backend(state.backend.clone())
            .set(
                "enrich_all_status",
                r#"{"status":"running","task_id":"x","enriched":0,"total":110781}"#,
            )
            .expect("réglage");
        let (_, st) = appel(&state, "GET", "/api/v1/library/enrich-all/status").await;
        assert_eq!(st["status"], "interrupted", "{st}");
        assert_eq!(st["paused"], false, "{st}");
        assert_eq!(st["total"], 110781, "les compteurs restent servis");

        // Passe vivante (inscrite), enrichissement non suspendu : « running ».
        let _garde = state.background_tasks.begin(
            "enrich_all",
            "Enrichissement des métadonnées…",
            "enrichment",
        );
        let (_, st) = appel(&state, "GET", "/api/v1/library/enrich-all/status").await;
        assert_eq!(st["status"], "running", "{st}");
        let (_, inst) = appel(&state, "GET", "/api/v1/system/background-tasks").await;
        let tache = inst["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|t| t["id"] == "enrich_all")
            .expect("inscrite");
        assert_eq!(tache["paused"], false, "{tache}");
        assert_eq!(carte(&inst, "enrichment")["state"], "en_cours");
    });
}
