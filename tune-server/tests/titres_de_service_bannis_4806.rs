//! Bannir un titre de SERVICE (#4806 suite) — par les ROUTES MONTÉES.
//!
//! FabienM, fil 1946, réponse 6820 (25/09/2026) : « Il faut pouvoir bannir un
//! titre service, pas uniquement local ». Go de Bertrand le 26/09/2026.
//!
//! Même promesse que le bannissement local (`titres_bannis_4806.rs`) : jamais
//! joué automatiquement, visible mais grisé, jouable à la main. Un titre de
//! service n'a pas d'entier : il est désigné par la paire `source` +
//! `source_id`, table `streaming_hidden_items` — la forme de
//! `streaming_item_tags` et `streaming_favorites`.
//!
//! Ce que ce fichier cloue, chaque point avec son témoin :
//!
//!  (a) les routes : bannir, lister (avec les titres locaux, `track_id: null`
//!      et la paire), rebannir sans doublon, débannir ; 400 sur une paire
//!      incomplète ou une source de bibliothèque ; par profil ;
//!  (b) la file le saute (enjambée de l'orchestrateur et « Suivant ») et le
//!      grise (`banned: true`) sans le retirer ;
//!  (c) un titre de service et un titre local de même identifiant ne se
//!      confondent pas, dans les DEUX sens ;
//!  (d) bannir le titre de service EN COURS passe au suivant ;
//!  (e) une smart playlist « Source = Qobuz » ne le rend plus, en `all`
//!      comme en `any`.
//!
//! La radio de service (« Plus comme ça ») est prouvée dans
//! `plus_comme_ca_qobuz_fil1906.rs`, qui a le service simulé.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::models::Track;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::orchestrator::Enjambee;
use tune_core::playback::NowPlaying;
use tune_server::state::AppState;

fn app_et_etat() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    profil: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("X-Profile-Id", profil);
    let body = match corps {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
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

async fn lire(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    appel(app, "GET", chemin, "1", None).await
}

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    appel(app, "POST", chemin, "1", Some(corps)).await
}

async fn bannir(app: &axum::Router, source: &str, source_id: &str) -> (StatusCode, Value) {
    poster(
        app,
        "/api/v1/library/tracks/streaming/ban",
        json!({"source": source, "source_id": source_id,
               "title": format!("Titre {source_id}"), "artist": "Quelqu'un",
               "album": "Un album", "album_source_id": "alb-1",
               "cover_url": "https://exemple.invalid/p.jpg"}),
    )
    .await
}

async fn debannir(app: &axum::Router, source: &str, source_id: &str) -> (StatusCode, Value) {
    poster(
        app,
        "/api/v1/library/tracks/streaming/unban",
        json!({"source": source, "source_id": source_id}),
    )
    .await
}

fn pistes_locales(state: &AppState, n: usize) -> Vec<i64> {
    let ar = ArtistRepo::with_backend(state.backend.clone())
        .get_or_create("Harmonia", None, None)
        .expect("artiste");
    let al = AlbumRepo::with_backend(state.backend.clone())
        .get_or_create("Deluxe", ar.id.unwrap(), None)
        .expect("album");
    let repo = TrackRepo::with_backend(state.backend.clone());
    (0..n)
        .map(|i| {
            let mut t = Track::new(format!("Deluxe {i:02}"));
            t.artist_id = ar.id;
            t.album_id = al.id;
            t.format = Some("flac".into());
            t.duration_ms = 200_000;
            t.track_number = i as i32 + 1;
            t.file_path = Some(format!("/musique/Harmonia/Deluxe/{i:02}.flac"));
            repo.create(&t).expect("piste")
        })
        .collect()
}

async fn zone_avec_sortie(state: &AppState, nom: &str) -> i64 {
    let device_id = format!("mock-{nom}");
    state.outputs.lock().await.register(Box::new(
        tune_core::outputs::mock::MockOutput::new(&device_id, "Sortie d'essai").with_type("mock"),
    ));
    ZoneRepo::with_backend(state.backend.clone())
        .create(nom, Some("mock"), Some(&device_id))
        .expect("creation de zone")
}

async fn enfiler_local(app: &axum::Router, zone: i64, id: i64) {
    let (st, v) = poster(
        app,
        &format!("/api/v1/zones/{zone}/queue/add"),
        json!({ "track_ids": [id] }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "mise en file : {v}");
}

async fn enfiler_service(app: &axum::Router, zone: i64, source: &str, source_id: &str) {
    let (st, v) = poster(
        app,
        &format!("/api/v1/zones/{zone}/queue/add"),
        json!({"source": source, "source_id": source_id,
               "title": format!("Titre {source_id}"), "artist_name": "Quelqu'un",
               "duration_ms": 180000}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "mise en file de service : {v}");
}

async fn drapeaux_de_la_file(app: &axum::Router, zone: i64) -> Vec<bool> {
    let (_, f) = lire(app, &format!("/api/v1/zones/{zone}/queue")).await;
    f["tracks"]
        .as_array()
        .unwrap_or_else(|| panic!("file illisible : {f}"))
        .iter()
        .map(|e| {
            e["banned"]
                .as_bool()
                .unwrap_or_else(|| panic!("ligne sans drapeau : {e}"))
        })
        .collect()
}

/// (a) Le contrat des routes.
#[tokio::test]
async fn bannir_lister_debannir_un_titre_de_service_par_les_routes() {
    let (app, state) = app_et_etat();
    let locales = pistes_locales(&state, 1);
    ProfileRepo::with_backend(state.backend.clone())
        .create("voisin", Some("Le voisin"), None)
        .expect("profil 2");

    // Témoin : rien de banni.
    let (_, liste) = lire(&app, "/api/v1/library/tracks/banned").await;
    assert_eq!(liste["total"], json!(0), "{liste}");

    // La provenance s'écrit comme le client l'a reçue : elle est normalisée.
    let (st, v) = bannir(&app, " Qobuz", "123456").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["banned"], json!(true), "{v}");
    assert_eq!(v["source"], json!("qobuz"), "{v}");
    assert_eq!(v["source_id"], json!("123456"), "{v}");
    assert_eq!(v["track_id"], Value::Null, "{v}");
    assert_eq!(v["zones_passees_au_suivant"], json!([]), "{v}");
    // Rebannir : idempotent, pas de doublon.
    let (st, _) = bannir(&app, "qobuz", "123456").await;
    assert_eq!(st, StatusCode::OK);
    // Et un titre local, pour la liste mêlée.
    let (st, _) = poster(
        &app,
        &format!("/api/v1/library/tracks/{}/ban", locales[0]),
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, liste) = lire(&app, "/api/v1/library/tracks/banned").await;
    assert_eq!(st, StatusCode::OK, "{liste}");
    assert_eq!(
        liste["total"],
        json!(2),
        "un local, un de service : {liste}"
    );
    let items = liste["items"].as_array().unwrap();
    let locale = items
        .iter()
        .find(|i| i["track_id"] == json!(locales[0]))
        .unwrap_or_else(|| panic!("le titre local : {liste}"));
    assert_eq!(locale["source"], Value::Null, "{liste}");
    let service = items
        .iter()
        .find(|i| i["source"] == json!("qobuz"))
        .unwrap_or_else(|| panic!("le titre de service : {liste}"));
    assert_eq!(
        service["track_id"],
        Value::Null,
        "jamais un entier seul : {liste}"
    );
    assert_eq!(service["source_id"], json!("123456"));
    assert_eq!(service["title"], json!("Titre 123456"));
    assert_eq!(service["artist"], json!("Quelqu'un"));
    assert_eq!(service["album_title"], json!("Un album"));
    assert_eq!(service["album_source_id"], json!("alb-1"));
    assert_eq!(
        service["cover_path"],
        json!("https://exemple.invalid/p.jpg")
    );
    assert_eq!(service["resolved"], json!(true));

    // Par profil : le voisin n'a rien banni.
    let (_, chez_2) = appel(&app, "GET", "/api/v1/library/tracks/banned", "2", None).await;
    assert_eq!(chez_2["total"], json!(0), "{chez_2}");

    // Désignations refusées.
    let (st, _) = bannir(&app, "qobuz", "  ").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "identifiant vide");
    let (st, _) = bannir(&app, "", "123").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "service vide");
    let (st, _) = bannir(&app, "local", "123").await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "une piste de bibliothèque se bannit par son id"
    );

    // Débannir.
    let (st, v) = debannir(&app, "QOBUZ", "123456").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["banned"], json!(false), "{v}");
    let (_, liste) = lire(&app, "/api/v1/library/tracks/banned").await;
    assert_eq!(liste["total"], json!(1), "reste le local : {liste}");
    // Idempotent.
    let (st, _) = debannir(&app, "qobuz", "123456").await;
    assert_eq!(st, StatusCode::OK);
}

/// (b) et (c) La file garde ses lignes, grise la ligne de service bannie et
/// l'enjambe ; le titre local de même identifiant n'est pas touché, et
/// inversement.
#[tokio::test]
async fn la_file_saute_un_titre_de_service_banni_sans_confondre_les_espaces() {
    let (app, state) = app_et_etat();
    let locales = pistes_locales(&state, 3);
    let zone = zone_avec_sortie(&state, "salon").await;
    // 0 local · 1 qobuz:s1 · 2 qobuz:s2 · 3 qobuz:<id local 2> · 4 local 2
    enfiler_local(&app, zone, locales[0]).await;
    enfiler_service(&app, zone, "qobuz", "s1").await;
    enfiler_service(&app, zone, "qobuz", "s2").await;
    let homonyme = locales[2].to_string();
    enfiler_service(&app, zone, "qobuz", &homonyme).await;
    enfiler_local(&app, zone, locales[2]).await;
    state.playback.update_queue_info(zone, 0, 5).await;

    // Témoin : rien de banni, rien d'enjambé.
    assert_eq!(
        drapeaux_de_la_file(&app, zone).await,
        vec![false; 5],
        "témoin"
    );
    assert_eq!(
        state
            .orchestrator
            .enjamber_les_pistes_bannies(zone, 1)
            .await,
        Enjambee::Rien,
        "témoin : la ligne de service se joue"
    );

    bannir(&app, "qobuz", "s1").await;
    assert_eq!(
        drapeaux_de_la_file(&app, zone).await,
        vec![false, true, false, false, false],
        "la ligne de service bannie est grisée, pas retirée"
    );
    assert_eq!(
        state
            .orchestrator
            .enjamber_les_pistes_bannies(zone, 1)
            .await,
        Enjambee::Reprise(2)
    );

    bannir(&app, "qobuz", "s2").await;
    assert_eq!(
        state
            .orchestrator
            .enjamber_les_pistes_bannies(zone, 1)
            .await,
        Enjambee::Reprise(3),
        "deux lignes de service bannies d'affilée, enjambées d'un coup"
    );
    // « Suivant » depuis 0 atterrit en 3.
    let (st, v) = poster(&app, &format!("/api/v1/zones/{zone}/next"), json!({})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["queue_position"], json!(3), "{v}");

    // (c) Bannir le titre de SERVICE « <id local 2> » ne bannit pas la piste
    // locale 2…
    bannir(&app, "qobuz", &homonyme).await;
    assert_eq!(
        drapeaux_de_la_file(&app, zone).await,
        vec![false, true, true, true, false],
        "la ligne locale de même identifiant n'est pas grisée"
    );
    assert_eq!(
        state
            .orchestrator
            .enjamber_les_pistes_bannies(zone, 3)
            .await,
        Enjambee::Reprise(4),
        "la ligne locale se joue"
    );
    // … et bannir la piste LOCALE 0 ne bannit aucune ligne de service.
    debannir(&app, "qobuz", &homonyme).await;
    let (st, _) = poster(
        &app,
        &format!("/api/v1/library/tracks/{}/ban", locales[2]),
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        drapeaux_de_la_file(&app, zone).await,
        vec![false, true, true, false, true],
        "la ligne de service de même identifiant n'est pas grisée"
    );

    // Débannir rend tout.
    debannir(&app, "qobuz", "s1").await;
    debannir(&app, "qobuz", "s2").await;
    assert_eq!(
        state
            .orchestrator
            .enjamber_les_pistes_bannies(zone, 1)
            .await,
        Enjambee::Rien
    );
}

/// (d) Bannir le titre de service EN COURS passe la zone au suivant.
#[tokio::test]
async fn bannir_le_titre_de_service_en_cours_passe_au_suivant() {
    let (app, state) = app_et_etat();
    let locales = pistes_locales(&state, 2);
    let zone = zone_avec_sortie(&state, "cuisine").await;
    // 0 local · 1 qobuz:en-cours · 2 qobuz:deja-bannie · 3 local
    enfiler_local(&app, zone, locales[0]).await;
    enfiler_service(&app, zone, "qobuz", "en-cours").await;
    enfiler_service(&app, zone, "qobuz", "deja-bannie").await;
    enfiler_local(&app, zone, locales[1]).await;

    state
        .playback
        .play(
            zone,
            NowPlaying {
                track_id: None,
                title: "Titre en-cours".into(),
                source: "qobuz".into(),
                source_id: Some("en-cours".into()),
                duration_ms: 180_000,
                ..Default::default()
            },
        )
        .await;
    state.playback.update_queue_info(zone, 1, 4).await;
    bannir(&app, "qobuz", "deja-bannie").await;

    // Témoin : bannir un AUTRE titre de service ne passe aucune zone, pas
    // plus qu'un titre local de même identifiant que celui qui joue.
    let (_, v) = bannir(&app, "qobuz", "ailleurs").await;
    assert_eq!(v["zones_passees_au_suivant"], json!([]), "{v}");
    let (_, v) = bannir(&app, "tidal", "en-cours").await;
    assert_eq!(
        v["zones_passees_au_suivant"],
        json!([]),
        "même identifiant, autre service : {v}"
    );

    let (st, v) = bannir(&app, "Qobuz", "en-cours").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let passees = v["zones_passees_au_suivant"]
        .as_array()
        .unwrap_or_else(|| panic!("zones passées : {v}"));
    assert_eq!(passees.len(), 1, "{v}");
    assert_eq!(passees[0]["zone_id"], json!(zone), "{v}");
    assert_eq!(
        passees[0]["queue_position"],
        json!(3),
        "la suivante, bannie elle aussi, est enjambée : {v}"
    );
    assert!(passees[0]["started"].is_boolean(), "{v}");
}

fn favori_de_service(state: &AppState, source: &str, source_id: &str, titre: &str) {
    let params: [&dyn ToSqlValue; 3] = [&source, &source_id, &titre];
    state
        .backend
        .execute(
            "INSERT INTO streaming_favorites (profile_id, item_type, service, service_id, title, artist, album) \
             VALUES (1, 'track', ?, ?, ?, 'Alice Coltrane', 'Journey in Satchidananda')",
            &params,
        )
        .expect("favori de service");
}

fn source_ids(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("liste attendue : {v}"))
        .iter()
        .filter_map(|p| p["source_id"].as_str().map(str::to_owned))
        .collect()
}

/// (e) Une smart playlist « Source = Qobuz » (les favoris Qobuz du profil)
/// exclut d'office un titre de service banni — en `all` comme en `any`, où
/// la précédence de `OR` aurait pu le laisser passer.
#[tokio::test]
async fn une_smart_playlist_de_service_exclut_un_titre_banni() {
    let (app, state) = app_et_etat();
    favori_de_service(&state, "qobuz", "f1", "Shiva-Loka");
    favori_de_service(&state, "qobuz", "f2", "Blue Nile");

    let (st, une) = poster(
        &app,
        "/api/v1/library/smart-playlists",
        json!({"name": "Mes favoris Qobuz",
               "rules": [{"field":"source","op":"=","value":"qobuz"}]}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{une}");
    let une = une["id"].as_i64().unwrap();
    let (st, deux) = poster(
        &app,
        "/api/v1/library/smart-playlists",
        json!({"name": "Qobuz ou Blue", "match_mode": "any",
               "rules": [{"field":"source","op":"=","value":"qobuz"},
                         {"field":"title","op":"contains","value":"Shiva"}]}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{deux}");
    let deux = deux["id"].as_i64().unwrap();

    // Témoin : les deux favoris y sont.
    for id in [une, deux] {
        let (st, v) = lire(
            &app,
            &format!("/api/v1/library/smart-playlists/{id}/tracks"),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let mut ids = source_ids(&v);
        ids.sort();
        assert_eq!(ids, vec!["f1", "f2"], "témoin {id} : {v}");
    }

    bannir(&app, "qobuz", "f1").await;

    for id in [une, deux] {
        let (_, v) = lire(
            &app,
            &format!("/api/v1/library/smart-playlists/{id}/tracks"),
        )
        .await;
        assert_eq!(
            source_ids(&v),
            vec!["f2"],
            "le titre de service banni sort d'office de la playlist {id} : {v}"
        );
    }
    // Débannir rend tout.
    debannir(&app, "qobuz", "f1").await;
    let (_, v) = lire(
        &app,
        &format!("/api/v1/library/smart-playlists/{une}/tracks"),
    )
    .await;
    assert_eq!(source_ids(&v).len(), 2, "{v}");
}
