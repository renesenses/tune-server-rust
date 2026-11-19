//! #9089 (Jon, fil 2757) — le décalage manuel d'une zone qui suit la meneuse
//! de son groupe : enregistré et borné par la route, appliqué au départ de la
//! suiveuse et à ses déplacements.
//!
//! Le banc est celui de `groupe_dlna_joue_sur_les_deux_6155.rs` : deux
//! renderers DLNA factices, le Phantom mène, la KEF suit.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par son entrée `[[test]]` dans `Cargo.toml`.
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::PlayState;
use tune_server::routes::zones::relais_de_groupe_6155::{
    deplacer_les_suiveuses, lancer_le_relais_de_groupe,
};
use tune_server::state::AppState;

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// Les titres que le renderer factice a reçus.
async fn joues_par(state: &AppState, device_id: &str) -> Vec<String> {
    let registre = state.outputs.lock().await;
    let sortie = registre.get(device_id).expect("sortie enregistree");
    let garde = sortie.lock().await;
    let mock = garde
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice");
    mock.play_titles().await
}

/// Attend (au plus 10 s) que `condition` soit vraie : le relais agit dans une
/// tâche de fond.
async fn attendre<F, Fut>(mut condition: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if condition().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

struct Banc {
    app: axum::Router,
    state: AppState,
    phantom: i64,
    kef: i64,
    _dir: tempfile::TempDir,
}

/// Deux zones DLNA, chacune sur son renderer factice, une piste FLAC réelle en
/// file sur le Phantom, le relais lancé comme le fait `spawn_background_tasks`.
async fn phantom_et_kef() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // Le regroupement relève du droit « Multiroom Sync ».
    state.license.set_account_premium(true, None).await;
    let app = tune_server::routes::router(state.clone());

    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("piste.flac");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tune-core/tests/fixtures/test.flac"
        ),
        &chemin,
    )
    .unwrap();
    state
        .backend
        .execute_batch(&format!(
            "INSERT INTO artists (id, name) VALUES (1, 'Artiste');\
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1);\
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                                 duration_ms, sample_rate, bit_depth, channels, source) \
             VALUES (1, 'Canopée', 1, 1, '{}', 'flac', 300000, 44100, 16, 2, 'local');",
            chemin.to_string_lossy()
        ))
        .unwrap();

    let zones = ZoneRepo::with_backend(state.backend.clone());
    let phantom = zones
        .create("Phantom 95 dB", Some("dlna"), Some("dlna-phantom"))
        .unwrap();
    let kef = zones
        .create("KEF LS50 II", Some("dlna"), Some("dlna-kef"))
        .unwrap();
    {
        let mut registre = state.outputs.lock().await;
        registre.register(Box::new(
            MockOutput::new("dlna-phantom", "Phantom 95 dB").with_type("dlna"),
        ));
        registre.register(Box::new(
            MockOutput::new("dlna-kef", "KEF LS50 II").with_type("dlna"),
        ));
    }
    lancer_le_relais_de_groupe(&state);

    let (status, corps) = poster(
        &app,
        &format!("/api/v1/zones/{phantom}/queue/add"),
        json!({ "track_ids": [1] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mise en file : {corps}");

    Banc {
        app,
        state,
        phantom,
        kef,
        _dir: dir,
    }
}

/// Le corps exact du client web (`groupZones(zone.id, [zone.id, …])`).
async fn grouper(banc: &Banc) {
    let (status, corps) = poster(
        &banc.app,
        "/api/v1/zones/group",
        json!({ "leader_id": banc.phantom, "zone_ids": [banc.phantom, banc.kef] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "regroupement : {corps}");
}

/// Les cibles des `seek` que le renderer factice a reçus.
async fn deplacements_de(state: &AppState, device_id: &str) -> Vec<u64> {
    let registre = state.outputs.lock().await;
    let sortie = registre.get(device_id).expect("sortie enregistree");
    let garde = sortie.lock().await;
    garde
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice")
        .seek_calls()
}

async fn requete(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(chemin);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn decaler(banc: &Banc, zone: i64, ms: i64) {
    let (status, corps) = requete(
        &banc.app,
        "PUT",
        &format!("/api/v1/zones/{zone}/group-offset"),
        Some(json!({ "offset_ms": ms })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "décalage {ms} ms : {corps}");
    assert_eq!(corps["offset_ms"], json!(ms));
}

#[tokio::test]
async fn la_route_enregistre_et_borne_le_decalage() {
    let banc = phantom_et_kef().await;
    let chemin = format!("/api/v1/zones/{}/group-offset", banc.kef);

    let (status, corps) = requete(&banc.app, "GET", &chemin, None).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["offset_ms"],
        json!(0),
        "sans réglage : aucun décalage"
    );
    assert_eq!(corps["min_ms"], json!(-2000));
    assert_eq!(corps["max_ms"], json!(2000));

    decaler(&banc, banc.kef, -450).await;
    let (_, corps) = requete(&banc.app, "GET", &chemin, None).await;
    assert_eq!(corps["offset_ms"], json!(-450), "le réglage est gardé");

    // Hors bornes : refusé, l'ancien réglage reste.
    let (status, corps) = requete(
        &banc.app,
        "PUT",
        &chemin,
        Some(json!({ "offset_ms": 2500 })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{corps}");
    assert_eq!(corps["error"], json!("group_offset_out_of_range"));
    let (_, corps) = requete(&banc.app, "GET", &chemin, None).await;
    assert_eq!(corps["offset_ms"], json!(-450));

    // Remis à zéro.
    decaler(&banc, banc.kef, 0).await;
    let (_, corps) = requete(&banc.app, "GET", &chemin, None).await;
    assert_eq!(corps["offset_ms"], json!(0));

    // Borne négative : refusée de même.
    let (status, corps) = requete(
        &banc.app,
        "PUT",
        &chemin,
        Some(json!({ "offset_ms": -2500 })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{corps}");
    let (_, corps) = requete(&banc.app, "GET", &chemin, None).await;
    assert_eq!(corps["offset_ms"], json!(0));

    // Zone inconnue, en lecture comme en écriture.
    let (status, _) = requete(&banc.app, "GET", "/api/v1/zones/987654/group-offset", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = requete(
        &banc.app,
        "PUT",
        "/api/v1/zones/987654/group-offset",
        Some(json!({ "offset_ms": 100 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Une suiveuse décalée de +800 ms ne reçoit la piste de la meneuse qu'après
/// ce délai : rien 300 ms après le départ de la meneuse, la piste ensuite.
///
/// Le chrono part au retour de `play_from_queue` (la meneuse joue déjà).
/// Contre-épreuve (mesurée sur Shrek) : forcer `attente_avant_le_geste_ms` à
/// rendre 0 fait tomber ce test à la vérification des 300 ms (la KEF a déjà
/// reçu la piste).
#[tokio::test]
async fn une_suiveuse_retardee_part_apres_son_decalage() {
    let banc = phantom_et_kef().await;
    decaler(&banc, banc.kef, 800).await;
    grouper(&banc).await;

    banc.state
        .orchestrator
        .play_from_queue(banc.phantom, 0)
        .await
        .expect("lecture sur la meneuse");
    let depart = Instant::now();
    assert_eq!(
        joues_par(&banc.state, "dlna-phantom").await,
        vec!["Canopée".to_string()],
        "témoin : la meneuse joue"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        joues_par(&banc.state, "dlna-kef").await.is_empty(),
        "#9089 : la KEF (décalage +800 ms) ne doit rien recevoir {:?} après le départ \
         de la meneuse — reçu : {:?}",
        depart.elapsed(),
        joues_par(&banc.state, "dlna-kef").await
    );
    let kef_joue = attendre(|| async {
        joues_par(&banc.state, "dlna-kef").await == vec!["Canopée".to_string()]
    })
    .await;
    assert!(kef_joue, "la KEF finit par rejoindre le groupe");
}

/// Relecture de #9155, point 1 — une attente en cours ne fait rien si la
/// zone a quitté le groupe entre-temps.
#[tokio::test]
async fn une_attente_ne_rejoint_plus_une_zone_degroupee() {
    let banc = phantom_et_kef().await;
    decaler(&banc, banc.kef, 800).await;
    grouper(&banc).await;
    banc.state
        .orchestrator
        .play_from_queue(banc.phantom, 0)
        .await
        .expect("lecture sur la meneuse");
    // Dégroupé pendant l'attente de la KEF.
    tune_core::db::settings_repo::SettingsRepo::with_backend(banc.state.backend.clone())
        .set("zone_groups", "[]")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        joues_par(&banc.state, "dlna-kef").await.is_empty(),
        "une zone dégroupée pendant l'attente ne doit pas être relancée — reçu : {:?}",
        joues_par(&banc.state, "dlna-kef").await
    );
}

/// Relecture de #9155, point 2 — deux gestes pour la même suiveuse : le plus
/// ancien, dépassé, ne fait rien, et un seul transfert part.
#[tokio::test]
async fn un_geste_differe_depasse_ne_transfere_pas_une_seconde_fois() {
    use tune_server::routes::zones::relais_de_groupe_6155::aligner_les_suiveuses_pour_test;

    let banc = phantom_et_kef().await;
    decaler(&banc, banc.kef, 600).await;
    grouper(&banc).await;
    banc.state
        .orchestrator
        .play_from_queue(banc.phantom, 0)
        .await
        .expect("lecture sur la meneuse");
    // Un second alignement, comme un second `started` rapproché.
    aligner_les_suiveuses_pour_test(&banc.state, banc.phantom).await;
    let kef_joue = attendre(|| async {
        joues_par(&banc.state, "dlna-kef").await == vec!["Canopée".to_string()]
    })
    .await;
    assert!(kef_joue, "la KEF rejoint le groupe");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        joues_par(&banc.state, "dlna-kef").await.len(),
        1,
        "un seul transfert vers la KEF"
    );
}

/// Un déplacement de la meneuse est relayé à la suiveuse avec son décalage :
/// en arrière pour un retard, en avant pour une avance.
#[tokio::test]
async fn un_deplacement_garde_le_decalage_de_la_suiveuse() {
    let banc = phantom_et_kef().await;
    grouper(&banc).await;
    banc.state
        .orchestrator
        .play_from_queue(banc.phantom, 0)
        .await
        .expect("lecture sur la meneuse");
    let kef_joue = attendre(|| async {
        banc.state.playback.get_state(banc.kef).await.state == PlayState::Playing
    })
    .await;
    assert!(kef_joue, "témoin : la KEF a rejoint la lecture du groupe");
    let duree = banc
        .state
        .playback
        .get_state(banc.phantom)
        .await
        .now_playing
        .map(|np| np.duration_ms)
        .unwrap_or(0) as u64;
    let cible = (duree / 2).max(1);
    let ecart = (duree / 4).clamp(1, 1500) as i64;

    // Avance : la KEF va `ecart` ms plus loin que la meneuse.
    decaler(&banc, banc.kef, -ecart).await;
    let demande = banc
        .state
        .deplacements_de_groupe
        .nouvelle_demande(banc.phantom);
    deplacer_les_suiveuses(&banc.state, banc.phantom, cible, demande).await;
    assert_eq!(
        deplacements_de(&banc.state, "dlna-kef")
            .await
            .last()
            .copied(),
        Some(cible + ecart as u64),
        "avance de {ecart} ms (durée {duree} ms)"
    );

    // Retard : la KEF reste `ecart` ms derrière la meneuse.
    decaler(&banc, banc.kef, ecart).await;
    let demande = banc
        .state
        .deplacements_de_groupe
        .nouvelle_demande(banc.phantom);
    deplacer_les_suiveuses(&banc.state, banc.phantom, cible, demande).await;
    assert_eq!(
        deplacements_de(&banc.state, "dlna-kef")
            .await
            .last()
            .copied(),
        Some(cible - ecart as u64),
        "retard de {ecart} ms (durée {duree} ms)"
    );

    // Sans décalage : la position de la meneuse, telle quelle.
    decaler(&banc, banc.kef, 0).await;
    let demande = banc
        .state
        .deplacements_de_groupe
        .nouvelle_demande(banc.phantom);
    deplacer_les_suiveuses(&banc.state, banc.phantom, cible, demande).await;
    assert_eq!(
        deplacements_de(&banc.state, "dlna-kef")
            .await
            .last()
            .copied(),
        Some(cible)
    );
}
