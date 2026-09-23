//! Bannir un titre (#4806) — tranche serveur, par les ROUTES MONTÉES.
//!
//! Bertrand, 23/09/2026 : un titre banni est « jamais joué automatiquement,
//! visible mais grisé dans son album ». Décisions : visible mais grisé, jamais
//! effacé des listes ; exclu d'office des smart playlists ; une file déjà
//! constituée n'est pas purgée, la piste est SAUTÉE quand la lecture y
//! arrive ; jouable si on la choisit exprès.
//!
//! Ce que ce fichier cloue, chaque point avec son témoin :
//!
//!  (a) un titre banni ne sort jamais de la lecture aléatoire (20 tirages par
//!      la route, sur une base de banc ; les 500 tirages sont dans
//!      `track_repo.rs`) ;
//!  (b) il est exclu d'une smart playlist qui le ciblerait — y compris en
//!      `match_mode = any`, où la précédence de `OR` aurait pu le laisser
//!      passer ;
//!  (c) la file le saute (« Suivant » et l'enjambée de l'orchestrateur) ;
//!  (d) bannir le titre EN COURS passe au suivant ;
//!  (e) débannir rend tout ;
//!  (f) un titre local et une ligne de service de même identifiant numérique
//!      ne se confondent pas ;
//!  (g) par profil : banni chez l'un, pas chez l'autre ;
//!  (h) le générateur de playlists (`/smart-ai/*`), la radio (`/radio/auto`)
//!      et les recommandations (`/ai/recommendations`) ne le rendent jamais.
//!
//! Périmètre : bibliothèque LOCALE. Aucune forme `(source, source_id)` n'est
//! branchée — voir `hidden_repo::ITEM_TYPE_TRACK`.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`. Voir `tests_orphelins.rs`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
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

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// Appel identifié par `X-Profile-Id` — auth désactivée (LAN de confiance).
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
    reponse(app, req.body(body).unwrap()).await
}

async fn lire(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    appel(app, "GET", chemin, "1", None).await
}

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    appel(app, "POST", chemin, "1", Some(corps)).await
}

/// Une bibliothèque de banc : un artiste, un album, `n` pistes de genre
/// `genre`, chemins distincts. Rend `(album_id, ids des pistes)`.
fn bibliotheque(
    state: &AppState,
    artiste: &str,
    album: &str,
    genre: &str,
    n: usize,
) -> (i64, Vec<i64>) {
    let ar = ArtistRepo::with_backend(state.backend.clone())
        .get_or_create(artiste, None, None)
        .expect("artiste");
    let al = AlbumRepo::with_backend(state.backend.clone())
        .get_or_create(album, ar.id.unwrap(), None)
        .expect("album");
    let album_id = al.id.unwrap();
    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut ids = Vec::new();
    for i in 0..n {
        let mut t = Track::new(format!("{album} {i:02}"));
        t.artist_id = ar.id;
        t.album_id = Some(album_id);
        t.genre = Some(genre.into());
        t.format = Some("flac".into());
        t.duration_ms = 200_000;
        t.track_number = i as i32 + 1;
        t.file_path = Some(format!("/musique/{artiste}/{album}/{i:02}.flac"));
        ids.push(repo.create(&t).expect("piste"));
    }
    (album_id, ids)
}

/// Une zone EN BASE avec une sortie factice enregistrée : « Suivant » refuse
/// une zone sans appareil.
async fn zone_avec_sortie(state: &AppState, nom: &str) -> i64 {
    let device_id = format!("mock-{nom}");
    state.outputs.lock().await.register(Box::new(
        tune_core::outputs::mock::MockOutput::new(&device_id, "Sortie d'essai").with_type("mock"),
    ));
    ZoneRepo::with_backend(state.backend.clone())
        .create(nom, Some("mock"), Some(&device_id))
        .expect("creation de zone")
}

async fn enfiler(app: &axum::Router, zone_id: i64, pistes: &[i64]) {
    let (status, corps) = poster(
        app,
        &format!("/api/v1/zones/{zone_id}/queue/add"),
        json!({ "track_ids": pistes }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mise en file : {corps}");
}

fn ids_de(liste: &Value) -> Vec<i64> {
    liste
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_i64))
        .collect()
}

fn drapeau_banni(liste: &Value, id: i64) -> Option<bool> {
    liste
        .as_array()?
        .iter()
        .find(|v| v.get("id").and_then(Value::as_i64) == Some(id))
        .and_then(|v| v.get("banned"))
        .and_then(Value::as_bool)
}

/// Le contrat des routes : bannir, lister, voir le drapeau PARTOUT sans que
/// la piste disparaisse de nulle part, débannir (e), 404 sur un id inconnu.
#[tokio::test]
async fn bannir_lister_debannir_par_les_routes() {
    let (app, state) = app_et_etat();
    let (album_id, ids) = bibliotheque(&state, "Portishead", "Dummy", "Trip-hop", 4);
    let bannie = ids[2];

    // 404 sur un id fantôme.
    let (st, _) = poster(&app, "/api/v1/library/tracks/424242/ban", json!({})).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Témoin : avant, personne n'est banni.
    let (st, avant) = lire(&app, &format!("/api/v1/library/albums/{album_id}/tracks")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ids_de(&avant).len(), 4);
    assert_eq!(drapeau_banni(&avant, bannie), Some(false), "{avant}");

    let (st, v) = poster(
        &app,
        &format!("/api/v1/library/tracks/{bannie}/ban"),
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["banned"], json!(true));
    assert_eq!(v["track_id"], json!(bannie));
    assert_eq!(v["profile_id"], json!(1));
    assert_eq!(v["zones_passees_au_suivant"], json!([]), "rien ne jouait");

    // L'écran « Titres bannis ».
    let (st, liste) = lire(&app, "/api/v1/library/tracks/banned").await;
    assert_eq!(st, StatusCode::OK, "{liste}");
    assert_eq!(liste["total"], json!(1));
    assert_eq!(liste["items"][0]["track_id"], json!(bannie));
    assert_eq!(liste["items"][0]["title"], json!("Dummy 02"));
    assert_eq!(liste["items"][0]["artist"], json!("Portishead"));
    assert_eq!(liste["items"][0]["album_id"], json!(album_id));
    assert_eq!(liste["items"][0]["resolved"], json!(true));

    // VISIBLE, grisé : la piste reste dans l'album, la liste, la fiche, la
    // recherche, les pistes de l'artiste — avec `banned: true`, et les
    // autres avec `banned: false` (jamais absent).
    let (_, album) = lire(&app, &format!("/api/v1/library/albums/{album_id}/tracks")).await;
    assert_eq!(
        ids_de(&album).len(),
        4,
        "la piste bannie reste dans l'album : {album}"
    );
    assert_eq!(drapeau_banni(&album, bannie), Some(true));
    assert_eq!(drapeau_banni(&album, ids[0]), Some(false));

    let (_, tous) = lire(&app, "/api/v1/library/tracks?limit=50").await;
    assert_eq!(ids_de(&tous["items"]).len(), 4, "{tous}");
    assert_eq!(drapeau_banni(&tous["items"], bannie), Some(true));

    let (_, fiche) = lire(&app, &format!("/api/v1/library/tracks/{bannie}")).await;
    assert_eq!(fiche["banned"], json!(true), "{fiche}");

    let (_, recherche) = lire(&app, "/api/v1/library/search?q=Dummy").await;
    assert_eq!(ids_de(&recherche["tracks"]).len(), 4, "{recherche}");
    assert_eq!(drapeau_banni(&recherche["tracks"], bannie), Some(true));

    let (_, federee) = lire(&app, "/api/v1/search?q=Dummy").await;
    assert_eq!(
        drapeau_banni(&federee["local"]["tracks"], bannie),
        Some(true),
        "{federee}"
    );

    let artiste_id = ArtistRepo::with_backend(state.backend.clone())
        .get_or_create("Portishead", None, None)
        .unwrap()
        .id
        .unwrap();
    let (_, de_l_artiste) = lire(
        &app,
        &format!("/api/v1/library/artists/{artiste_id}/tracks"),
    )
    .await;
    assert_eq!(
        drapeau_banni(&de_l_artiste, bannie),
        Some(true),
        "{de_l_artiste}"
    );

    // Une playlist qui le contient le garde, grisé.
    let (st, pl) = poster(&app, "/api/v1/playlists", json!({"name": "Soirée"})).await;
    assert!(st.is_success(), "{pl}");
    let pl_id = pl["id"].as_i64().expect("id de playlist");
    let (st, _) = poster(
        &app,
        &format!("/api/v1/playlists/{pl_id}/tracks"),
        json!({"track_ids": ids}),
    )
    .await;
    assert!(st.is_success());
    let (_, pistes) = lire(&app, &format!("/api/v1/playlists/{pl_id}/tracks")).await;
    assert_eq!(ids_de(&pistes).len(), 4, "{pistes}");
    assert_eq!(drapeau_banni(&pistes, bannie), Some(true));

    // (e) Débannir rend tout.
    let (st, v) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/library/tracks/{bannie}/ban"),
        "1",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["banned"], json!(false));
    let (_, liste) = lire(&app, "/api/v1/library/tracks/banned").await;
    assert_eq!(liste["total"], json!(0));
    let (_, fiche) = lire(&app, &format!("/api/v1/library/tracks/{bannie}")).await;
    assert_eq!(fiche["banned"], json!(false));
    // Idempotent.
    let (st, v) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/library/tracks/{bannie}/ban"),
        "1",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

/// (a) par la ROUTE : vingt lectures aléatoires de la bibliothèque entière,
/// puis de l'album, puis de l'artiste — la piste bannie n'entre jamais dans
/// la file. Témoin avant, (e) après débannissement.
#[tokio::test]
async fn un_titre_banni_ne_sort_jamais_de_la_lecture_aleatoire() {
    let (app, state) = app_et_etat();
    let (album_id, ids) = bibliotheque(&state, "Chic", "Risqué", "Disco", 12);
    let bannie = ids[5];
    let zone = zone_avec_sortie(&state, "salle").await;
    let artiste_id = ArtistRepo::with_backend(state.backend.clone())
        .get_or_create("Chic", None, None)
        .unwrap()
        .id
        .unwrap();

    let file = |app: axum::Router| async move {
        let (_, f) = lire(&app, &format!("/api/v1/zones/{zone}/queue")).await;
        f["tracks"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|e| e.get("track_id").and_then(Value::as_i64))
            .collect::<Vec<i64>>()
    };

    // Témoin : avant le bannissement, la piste part.
    let mut vue = false;
    for _ in 0..20 {
        poster(
            &app,
            &format!("/api/v1/playback/shuffle-all?zone_id={zone}"),
            json!({}),
        )
        .await;
        let f = file(app.clone()).await;
        assert_eq!(f.len(), 12, "témoin : 12 pistes en file, {f:?}");
        vue |= f.contains(&bannie);
    }
    assert!(vue, "témoin : la piste doit partir avant d'être bannie");

    let (st, _) = poster(
        &app,
        &format!("/api/v1/library/tracks/{bannie}/ban"),
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let portees = [
        format!("/api/v1/playback/shuffle-all?zone_id={zone}"),
        format!("/api/v1/playback/shuffle-all?zone_id={zone}&album_id={album_id}"),
        format!("/api/v1/playback/shuffle-all?zone_id={zone}&artist_id={artiste_id}"),
        format!("/api/v1/playback/shuffle-all?zone_id={zone}&search_query=Risqu"),
        format!("/api/v1/playback/shuffle-all?zone_id={zone}&genre=Disco"),
    ];
    for portee in &portees {
        for _ in 0..20 {
            // Le statut n'est pas jugé : la zone 1 d'essai n'a pas de sortie,
            // la LECTURE peut échouer ; la SÉLECTION, elle, est en base.
            poster(&app, portee, json!({})).await;
            let f = file(app.clone()).await;
            assert_eq!(f.len(), 11, "{portee} : 11 pistes jouables, {f:?}");
            assert!(
                !f.contains(&bannie),
                "{portee} : bannie et pourtant en file {f:?}"
            );
        }
    }

    // (e) Débannir rend tout.
    appel(
        &app,
        "DELETE",
        &format!("/api/v1/library/tracks/{bannie}/ban"),
        "1",
        None,
    )
    .await;
    let mut revue = false;
    for _ in 0..20 {
        poster(
            &app,
            &format!("/api/v1/playback/shuffle-all?zone_id={zone}"),
            json!({}),
        )
        .await;
        let f = file(app.clone()).await;
        assert_eq!(f.len(), 12);
        revue |= f.contains(&bannie);
    }
    assert!(revue, "débannie, la piste doit repartir");
}

/// (b) Exclu d'office d'une smart playlist qui le ciblerait — sans règle à
/// configurer. Deux formes : `all` sur un genre, et `any` sur deux genres,
/// où `a OR b AND socle` aurait laissé passer la piste par précédence.
#[tokio::test]
async fn un_titre_banni_est_exclu_d_office_des_smart_playlists() {
    let (app, state) = app_et_etat();
    let (_, jazz) = bibliotheque(&state, "Coltrane", "Blue Train", "Jazz", 3);
    let (_, rock) = bibliotheque(&state, "Television", "Marquee Moon", "Rock", 3);
    let bannie = jazz[1];

    let (st, une) = poster(
        &app,
        "/api/v1/library/smart-playlists",
        json!({"name": "Que du jazz",
               "rules": [{"field":"genre","op":"contains","value":"Jazz"}]}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{une}");
    let une = une["id"].as_i64().unwrap();
    let (st, deux) = poster(
        &app,
        "/api/v1/library/smart-playlists",
        json!({"name": "Jazz ou rock", "match_mode": "any",
               "rules": [{"field":"genre","op":"contains","value":"Jazz"},
                         {"field":"genre","op":"contains","value":"Rock"}]}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{deux}");
    let deux = deux["id"].as_i64().unwrap();

    // Témoin : avant, la piste y est.
    let (st, v) = lire(
        &app,
        &format!("/api/v1/library/smart-playlists/{une}/tracks"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(ids_de(&v).contains(&bannie), "témoin : {v}");
    let (_, v) = lire(
        &app,
        &format!("/api/v1/library/smart-playlists/{deux}/tracks"),
    )
    .await;
    assert_eq!(ids_de(&v).len(), 6, "témoin any : {v}");

    poster(
        &app,
        &format!("/api/v1/library/tracks/{bannie}/ban"),
        json!({}),
    )
    .await;

    let (_, v) = lire(
        &app,
        &format!("/api/v1/library/smart-playlists/{une}/tracks"),
    )
    .await;
    let ids = ids_de(&v);
    assert_eq!(ids.len(), 2, "{v}");
    assert!(!ids.contains(&bannie), "exclue d'office : {v}");

    let (_, v) = lire(
        &app,
        &format!("/api/v1/library/smart-playlists/{deux}/tracks"),
    )
    .await;
    let ids = ids_de(&v);
    assert_eq!(ids.len(), 5, "any : {v}");
    assert!(
        !ids.contains(&bannie),
        "la précédence de OR ne la laisse pas passer : {v}"
    );
    assert!(
        rock.iter().all(|id| ids.contains(id)),
        "les autres restent : {v}"
    );

    // L'aperçu, même règle.
    let (_, v) = poster(
        &app,
        "/api/v1/library/smart-playlists/preview",
        json!({"rules": [{"field":"genre","op":"contains","value":"Jazz"}]}),
    )
    .await;
    let apercu = v
        .get("items")
        .or(v.get("tracks"))
        .cloned()
        .unwrap_or(v.clone());
    assert!(!ids_de(&apercu).contains(&bannie), "aperçu : {v}");

    // (e) Débannir rend tout.
    appel(
        &app,
        "DELETE",
        &format!("/api/v1/library/tracks/{bannie}/ban"),
        "1",
        None,
    )
    .await;
    let (_, v) = lire(
        &app,
        &format!("/api/v1/library/smart-playlists/{une}/tracks"),
    )
    .await;
    assert!(ids_de(&v).contains(&bannie), "{v}");
}

/// (c) La file n'est pas purgée : la piste bannie y reste, `banned: true`,
/// et « Suivant » la saute. Si tout ce qui reste est banni, la file finit.
#[tokio::test]
async fn la_file_saute_un_titre_banni() {
    let (app, state) = app_et_etat();
    let (_, ids) = bibliotheque(&state, "Neu!", "Neu! 75", "Krautrock", 4);
    let zone = zone_avec_sortie(&state, "salon").await;
    enfiler(&app, zone, &ids).await;
    state.playback.update_queue_info(zone, 0, 4).await;

    // Témoin : sans bannissement, « Suivant » va en 1.
    assert_eq!(
        state
            .orchestrator
            .enjamber_les_pistes_bannies(zone, 1)
            .await,
        Enjambee::Rien
    );

    poster(
        &app,
        &format!("/api/v1/library/tracks/{}/ban", ids[1]),
        json!({}),
    )
    .await;
    poster(
        &app,
        &format!("/api/v1/library/tracks/{}/ban", ids[2]),
        json!({}),
    )
    .await;

    // La file garde ses quatre lignes, deux grisées.
    let (_, f) = lire(&app, &format!("/api/v1/zones/{zone}/queue")).await;
    assert_eq!(f["length"], json!(4), "pas de purge : {f}");
    let drapeaux: Vec<bool> = f["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["banned"].as_bool().unwrap())
        .collect();
    assert_eq!(drapeaux, vec![false, true, true, false], "{f}");

    // L'orchestrateur enjambe 1 et 2 d'un coup.
    assert_eq!(
        state
            .orchestrator
            .enjamber_les_pistes_bannies(zone, 1)
            .await,
        Enjambee::Reprise(3)
    );

    // « Suivant » depuis 0 reprend en 3.
    let (st, v) = poster(&app, &format!("/api/v1/zones/{zone}/next"), json!({})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["queue_position"], json!(3), "{v}");

    // Tout ce qui reste est banni : la file est finie.
    poster(
        &app,
        &format!("/api/v1/library/tracks/{}/ban", ids[3]),
        json!({}),
    )
    .await;
    assert_eq!(
        state
            .orchestrator
            .enjamber_les_pistes_bannies(zone, 1)
            .await,
        Enjambee::FileEpuisee
    );
    state.playback.update_queue_info(zone, 0, 4).await;
    let (_, v) = poster(&app, &format!("/api/v1/zones/{zone}/next"), json!({})).await;
    assert_eq!(v["status"], json!("stopped"), "{v}");
    assert_eq!(v["reason"], json!("end_of_queue"), "{v}");
}

/// (d) Bannir le titre EN COURS passe au suivant — la route le dit, zone par
/// zone, avec la position reprise (en sautant aussi une bannie qui suit).
#[tokio::test]
async fn bannir_le_titre_en_cours_passe_au_suivant() {
    let (app, state) = app_et_etat();
    let (_, ids) = bibliotheque(&state, "Can", "Ege Bamyasi", "Krautrock", 4);
    let zone = zone_avec_sortie(&state, "cuisine").await;
    enfiler(&app, zone, &ids).await;

    // La zone joue la piste 1.
    let piste = TrackRepo::with_backend(state.backend.clone())
        .get(ids[1])
        .unwrap()
        .unwrap();
    state
        .playback
        .play(zone, NowPlaying::from_track(&piste))
        .await;
    state.playback.update_queue_info(zone, 1, 4).await;
    // La piste 2 est déjà bannie : le passage doit atterrir en 3.
    poster(
        &app,
        &format!("/api/v1/library/tracks/{}/ban", ids[2]),
        json!({}),
    )
    .await;

    let (st, v) = poster(
        &app,
        &format!("/api/v1/library/tracks/{}/ban", ids[1]),
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let passees = v["zones_passees_au_suivant"]
        .as_array()
        .expect("zones passées");
    assert_eq!(passees.len(), 1, "{v}");
    assert_eq!(passees[0]["zone_id"], json!(zone), "{v}");
    assert_eq!(passees[0]["queue_position"], json!(3), "{v}");
    // Le démarrage est attendu EN LIGNE et rendu tel quel ; sur ce banc les
    // fichiers n'existent pas, la LECTURE peut échouer (et l'échec est
    // annoncé) — c'est le PASSAGE qui est prouvé, pas le décodage.
    assert!(passees[0]["started"].is_boolean(), "{v}");

    // Bannir un titre que personne ne joue ne passe aucune zone. (Une piste
    // HORS de la file : le passage au suivant lancé ci-dessus peut avoir
    // fait avancer la zone entre-temps.)
    let (_, ailleurs) = bibliotheque(&state, "Faust", "IV", "Krautrock", 1);
    let (_, v) = poster(
        &app,
        &format!("/api/v1/library/tracks/{}/ban", ailleurs[0]),
        json!({}),
    )
    .await;
    assert_eq!(v["zones_passees_au_suivant"], json!([]), "{v}");
}

/// (f) Un titre LOCAL et une ligne de SERVICE de même identifiant numérique
/// ne se confondent pas : bannir `tracks.id = N` ne grise ni ne saute la
/// ligne `qobuz:N`, et ne touche à rien qui ne soit `tracks.id`.
#[tokio::test]
async fn local_et_service_de_meme_identifiant_ne_se_confondent_pas() {
    let (app, state) = app_et_etat();
    let (_, ids) = bibliotheque(&state, "Kraftwerk", "Computer World", "Electro", 2);
    let locale = ids[0];
    let zone = zone_avec_sortie(&state, "bureau").await;
    enfiler(&app, zone, &[ids[1]]).await;
    let (st, v) = poster(
        &app,
        &format!("/api/v1/zones/{zone}/queue/add"),
        json!({"source": "qobuz", "source_id": locale.to_string(),
               "title": "Homonyme de service", "artist_name": "Quelqu'un",
               "duration_ms": 180000}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    enfiler(&app, zone, &[locale]).await;

    poster(
        &app,
        &format!("/api/v1/library/tracks/{locale}/ban"),
        json!({}),
    )
    .await;

    let (_, f) = lire(&app, &format!("/api/v1/zones/{zone}/queue")).await;
    let lignes = f["tracks"].as_array().unwrap();
    assert_eq!(lignes.len(), 3, "{f}");
    assert_eq!(lignes[1]["source_id"], json!(locale.to_string()));
    assert_eq!(
        lignes[1]["banned"],
        json!(false),
        "la ligne de service : {f}"
    );
    assert_eq!(lignes[2]["track_id"], json!(locale));
    assert_eq!(lignes[2]["banned"], json!(true), "la ligne locale : {f}");

    // L'enjambée s'arrête sur la ligne de service (jouable), saute la locale.
    assert_eq!(
        state
            .orchestrator
            .enjamber_les_pistes_bannies(zone, 1)
            .await,
        Enjambee::Rien
    );
    assert_eq!(
        state
            .orchestrator
            .enjamber_les_pistes_bannies(zone, 2)
            .await,
        Enjambee::FileEpuisee
    );

    // Rien d'autre que `tracks.id = locale` n'est marqué : l'album masqué de
    // même numéro n'existe pas, l'autre piste n'est pas bannie.
    let (_, liste) = lire(&app, "/api/v1/library/tracks/banned").await;
    assert_eq!(liste["total"], json!(1));
    let (_, caches) = lire(&app, "/api/v1/library/albums/hidden").await;
    assert_eq!(caches["total"], json!(0), "{caches}");
}

/// (g) Par profil : banni chez le profil 2, pas chez le profil 1 — drapeau,
/// écran « Titres bannis » et lecture aléatoire.
#[tokio::test]
async fn banni_chez_l_un_pas_chez_l_autre() {
    let (app, state) = app_et_etat();
    let voisin = ProfileRepo::with_backend(state.backend.clone())
        .create("voisin", Some("Le voisin"), None)
        .expect("profil 2");
    assert_eq!(voisin, 2);
    let (album_id, ids) = bibliotheque(&state, "Air", "Moon Safari", "Downtempo", 5);
    let bannie = ids[3];
    let zone = zone_avec_sortie(&state, "salle").await;

    let (st, v) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/tracks/{bannie}/ban"),
        "2",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["profile_id"], json!(2));

    let (_, chez_2) = appel(
        &app,
        "GET",
        &format!("/api/v1/library/albums/{album_id}/tracks"),
        "2",
        None,
    )
    .await;
    assert_eq!(drapeau_banni(&chez_2, bannie), Some(true), "{chez_2}");
    let (_, chez_1) = appel(
        &app,
        "GET",
        &format!("/api/v1/library/albums/{album_id}/tracks"),
        "1",
        None,
    )
    .await;
    assert_eq!(drapeau_banni(&chez_1, bannie), Some(false), "{chez_1}");

    let (_, l2) = appel(&app, "GET", "/api/v1/library/tracks/banned", "2", None).await;
    assert_eq!(l2["total"], json!(1));
    let (_, l1) = appel(&app, "GET", "/api/v1/library/tracks/banned", "1", None).await;
    assert_eq!(l1["total"], json!(0));

    // L'aléatoire du profil 1 la tire ; celui du profil 2 jamais.
    let file = |app: axum::Router| async move {
        let (_, f) = lire(&app, &format!("/api/v1/zones/{zone}/queue")).await;
        f["tracks"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|e| e.get("track_id").and_then(Value::as_i64))
            .collect::<Vec<i64>>()
    };
    for _ in 0..10 {
        appel(
            &app,
            "POST",
            &format!("/api/v1/playback/shuffle-all?zone_id={zone}"),
            "2",
            Some(json!({})),
        )
        .await;
        let f = file(app.clone()).await;
        assert_eq!(f.len(), 4, "{f:?}");
        assert!(!f.contains(&bannie));
    }
    let mut vue = false;
    for _ in 0..10 {
        appel(
            &app,
            "POST",
            &format!("/api/v1/playback/shuffle-all?zone_id={zone}"),
            "1",
            Some(json!({})),
        )
        .await;
        let f = file(app.clone()).await;
        assert_eq!(f.len(), 5, "{f:?}");
        vue |= f.contains(&bannie);
    }
    assert!(vue, "chez le profil 1, rien n'est banni");

    // Débannir chez 1 ne débannit pas chez 2.
    appel(
        &app,
        "DELETE",
        &format!("/api/v1/library/tracks/{bannie}/ban"),
        "1",
        None,
    )
    .await;
    let (_, l2) = appel(&app, "GET", "/api/v1/library/tracks/banned", "2", None).await;
    assert_eq!(l2["total"], json!(1));
}

/// (h) Les autres sélections automatiques exposées par les routes : les six
/// variantes du générateur de playlists, la radio et les recommandations.
/// Cinquante passes chacune sur douze pistes ; témoin avant, retour après.
#[tokio::test]
async fn les_generateurs_la_radio_et_les_recommandations_ignorent_un_titre_banni() {
    let (app, state) = app_et_etat();
    let (_, ids) = bibliotheque(&state, "Chic", "C'est Chic", "Disco", 12);
    let bannie = ids[7];
    // Un tempo pour que « tempo-match » ait quelque chose à apparier.
    state
        .backend
        .execute("UPDATE tracks SET bpm = 120", &[])
        .unwrap();

    let radio = format!("/api/v1/radio/auto?seed_track={}&count=50", ids[0]);
    let appels: Vec<(&str, &str, Option<Value>)> = vec![
        ("GET", &radio, None),
        (
            "POST",
            "/api/v1/smart-ai/generate",
            Some(json!({"prompt": "disco", "limit": 50})),
        ),
        (
            "POST",
            "/api/v1/smart-ai/mood",
            Some(json!({"mood": "party", "limit": 50})),
        ),
        (
            "POST",
            "/api/v1/smart-ai/similar-to",
            Some(json!({"track_id": ids[0], "limit": 50})),
        ),
        (
            "POST",
            "/api/v1/smart-ai/history-based",
            Some(json!({"limit": 50})),
        ),
        (
            "POST",
            "/api/v1/smart-ai/tempo-match",
            Some(json!({"target_bpm": 120, "limit": 50})),
        ),
        (
            "POST",
            "/api/v1/smart-ai/discovery",
            Some(json!({"limit": 50})),
        ),
        ("GET", "/api/v1/ai/recommendations?limit=50", None),
    ];

    fn ids_rendus(v: &Value) -> Vec<i64> {
        v["tracks"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|t| t.get("id").or(t.get("track_id")).and_then(Value::as_i64))
            .collect()
    }

    for (methode, chemin, corps) in &appels {
        let mut vue = false;
        for _ in 0..50 {
            let (st, v) = appel(&app, methode, chemin, "1", corps.clone()).await;
            assert_eq!(st, StatusCode::OK, "{chemin} : {v}");
            vue |= ids_rendus(&v).contains(&bannie);
        }
        assert!(
            vue,
            "témoin {chemin} : la piste doit sortir avant le bannissement"
        );
    }

    let (st, _) = poster(
        &app,
        &format!("/api/v1/library/tracks/{bannie}/ban"),
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    for (methode, chemin, corps) in &appels {
        for _ in 0..50 {
            let (_, v) = appel(&app, methode, chemin, "1", corps.clone()).await;
            let rendus = ids_rendus(&v);
            assert!(!rendus.is_empty(), "{chemin} : rien ne sort ? {v}");
            assert!(
                !rendus.contains(&bannie),
                "{chemin} : bannie et pourtant rendue : {rendus:?}"
            );
        }
    }

    // (g) Chez un autre profil, rien n'est banni : le générateur la rend.
    ProfileRepo::with_backend(state.backend.clone())
        .create("voisin", Some("Le voisin"), None)
        .expect("profil 2");
    let vue = {
        let mut vue = false;
        for _ in 0..50 {
            let (_, v) = appel(
                &app,
                "POST",
                "/api/v1/smart-ai/generate",
                "2",
                Some(json!({"prompt": "disco", "limit": 50})),
            )
            .await;
            vue |= ids_rendus(&v).contains(&bannie);
        }
        vue
    };
    assert!(vue, "le profil 2 n'a rien banni");

    // (e) Débannir rend tout.
    appel(
        &app,
        "DELETE",
        &format!("/api/v1/library/tracks/{bannie}/ban"),
        "1",
        None,
    )
    .await;
    for (methode, chemin, corps) in &appels {
        let mut revue = false;
        for _ in 0..50 {
            let (_, v) = appel(&app, methode, chemin, "1", corps.clone()).await;
            revue |= ids_rendus(&v).contains(&bannie);
        }
        assert!(revue, "{chemin} : débannie, la piste doit ressortir");
    }
}
