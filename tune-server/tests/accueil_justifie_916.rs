//! L'accueil qui dit POURQUOI, et « Pas pour moi » — par les ROUTES MONTÉES
//! (renesenses/tune-web-client#916, FabienM, fil forum 1527, go de Bertrand
//! du 12/10/2026).
//!
//! Ce que ce fichier cloue, chaque point avec son témoin :
//!
//!  (a) `/home/recommendations` porte `raison: {type: genre_frequent, genre}`
//!      quand l'album vient d'un genre écouté — et AUCUNE raison quand il vient
//!      du repli au hasard ;
//!  (b) « Pas pour moi » sur un album l'ôte des recommandations du profil qui
//!      l'a dit, et de lui seul : le profil 2 le voit toujours (contre-épreuve) ;
//!  (c) « Annuler » le rend ;
//!  (d) « Pas pour moi » sur un ARTISTE ôte tous ses albums, nom comparé sans
//!      casse ni espaces en trop ;
//!  (e) la liste des Réglages est par profil ; une désignation qui ne désigne
//!      rien rend 400 ;
//!  (f) `/home/new-in-library` porte `raison: {type: ajout_recent, ajoute_le}`.
//!
//! Go du 13/10/2026 (les autres rangées) :
//!
//!  (g) le repli au hasard dit qu'il est le hasard : `raison: {type: hasard}` ;
//!  (h) « Continuer l'écoute » porte `raison: {type: reprise, ecoute_le}` et
//!      n'y remet plus ce que le profil a écarté — lui seul ;
//!  (i) `/home/top-mixes` porte `raison: genre_frequent` par mix, et ses
//!      pistes n'incluent plus un album écarté ;
//!  (j) `/home` (page composée) parle la langue de l'interface et filtre
//!      « À découvrir » et les reprises par profil.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::profile_repo::ProfileRepo;
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

fn sql(state: &AppState, requete: &str) {
    state.backend.execute(requete, &[]).expect(requete);
}

/// Miles Davis (albums 1 et 2) et Bill Evans (album 3), tous en Jazz ; seul
/// l'album 1 a été écouté. Les recommandations par genre proposent donc 2 et 3.
fn bibliotheque_jazz(state: &AppState) {
    sql(
        state,
        "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis'), (2, 'Bill Evans')",
    );
    sql(
        state,
        "INSERT INTO albums (id, title, artist_id, genre) VALUES \
         (1, 'Kind of Blue', 1, 'Jazz'), (2, 'Bitches Brew', 1, 'Jazz'), \
         (3, 'Sunday at the Village Vanguard', 2, 'Jazz')",
    );
    sql(
        state,
        "INSERT INTO tracks (id, title, album_id, artist_id, file_path, file_mtime) VALUES \
         (1, 'So What', 1, 1, '/m/1.flac', 1760000000), \
         (2, 'Pharaoh''s Dance', 2, 1, '/m/2.flac', 1760000100), \
         (3, 'Gloria''s Step', 3, 2, '/m/3.flac', 1760000200)",
    );
    sql(
        state,
        "INSERT INTO listen_history (track_id, album_id, title, artist_name, album_title, source) \
         VALUES (1, 1, 'So What', 'Miles Davis', 'Kind of Blue', 'local')",
    );
}

/// Les identifiants d'albums recommandés au profil, triés.
async fn recommandes(app: &axum::Router, profil: &str) -> (Vec<i64>, Value) {
    let (st, v) = appel(
        app,
        "GET",
        "/api/v1/home/recommendations?limit=10",
        profil,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let mut ids: Vec<i64> = v
        .as_array()
        .expect("un tableau")
        .iter()
        .filter_map(|a| a["id"].as_i64())
        .collect();
    ids.sort();
    (ids, v)
}

#[tokio::test]
async fn la_recommandation_dit_pourquoi_et_entend_pas_pour_moi_par_profil() {
    let (app, state) = app_et_etat();
    bibliotheque_jazz(&state);
    ProfileRepo::with_backend(state.backend.clone())
        .create("voisin", Some("Le voisin"), None)
        .expect("profil 2");

    // (a) La raison, structurée, pour chaque album.
    let (ids, v) = recommandes(&app, "1").await;
    assert_eq!(ids, vec![2, 3], "{v}");
    for a in v.as_array().unwrap() {
        assert_eq!(
            a["raison"],
            json!({"type": "genre_frequent", "genre": "Jazz"}),
            "{a}"
        );
        assert_eq!(
            a["reason"],
            json!("genre_match"),
            "l'ancien champ reste : {a}"
        );
    }

    // (b) « Pas pour moi » sur l'album 2, par le profil 1.
    let (st, v) = appel(
        &app,
        "POST",
        "/api/v1/home/dismissed-suggestions",
        "1",
        Some(
            json!({"item_type": "album", "source": "local", "item_id": "2",
                    "title": "Bitches Brew", "subtitle": "Miles Davis"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["item"]["item_id"], json!("2"), "{v}");
    assert_eq!(v["item"]["source"], json!("local"), "{v}");

    let (ids, v) = recommandes(&app, "1").await;
    assert_eq!(ids, vec![3], "le profil 1 a écarté l'album 2 : {v}");
    // Contre-épreuve : le voisin le voit toujours.
    let (ids, v) = recommandes(&app, "2").await;
    assert_eq!(ids, vec![2, 3], "le profil 2 n'a rien écarté : {v}");

    // (e) La liste des Réglages, par profil.
    let (_, l1) = appel(&app, "GET", "/api/v1/home/dismissed-suggestions", "1", None).await;
    assert_eq!(l1["total"], json!(1), "{l1}");
    assert_eq!(l1["items"][0]["title"], json!("Bitches Brew"), "{l1}");
    let (_, l2) = appel(&app, "GET", "/api/v1/home/dismissed-suggestions", "2", None).await;
    assert_eq!(l2["total"], json!(0), "{l2}");

    // (c) « Annuler ».
    let (st, v) = appel(
        &app,
        "POST",
        "/api/v1/home/dismissed-suggestions/restore",
        "1",
        Some(json!({"item_type": "album", "source": "local", "item_id": "2"})),
    )
    .await;
    assert_eq!(
        (st, v["restored"].clone()),
        (StatusCode::OK, json!(true)),
        "{v}"
    );
    let (ids, _) = recommandes(&app, "1").await;
    assert_eq!(ids, vec![2, 3], "rétabli");

    // (d) « Pas pour moi » sur l'ARTISTE, nom mal tapé.
    let (st, v) = appel(
        &app,
        "POST",
        "/api/v1/home/dismissed-suggestions",
        "1",
        Some(json!({"item_type": "artist", "item_id": "  bill   EVANS "})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["item"]["item_id"], json!("bill evans"), "{v}");
    let (ids, v) = recommandes(&app, "1").await;
    assert_eq!(
        ids,
        vec![2],
        "tout Bill Evans est écarté pour le profil 1 : {v}"
    );
    let (ids, _) = recommandes(&app, "2").await;
    assert_eq!(ids, vec![2, 3], "et pas pour le profil 2");
}

#[tokio::test]
async fn une_designation_qui_ne_designe_rien_rend_400() {
    let (app, _state) = app_et_etat();
    for corps in [
        json!({"item_type": "playlist", "source": "local", "item_id": "4"}),
        json!({"item_type": "track", "source": "qobuz", "item_id": "4"}),
        json!({"item_type": "album", "source": "local", "item_id": "pas-un-entier"}),
        json!({"item_type": "album", "source": "qobuz", "item_id": "  "}),
    ] {
        let (st, v) = appel(
            &app,
            "POST",
            "/api/v1/home/dismissed-suggestions",
            "1",
            Some(corps.clone()),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{corps} → {v}");
    }
    let (_, l) = appel(&app, "GET", "/api/v1/home/dismissed-suggestions", "1", None).await;
    assert_eq!(l["total"], json!(0), "rien n'a été écrit : {l}");
}

#[tokio::test]
async fn le_hasard_dit_qu_il_est_le_hasard() {
    let (app, state) = app_et_etat();
    // Aucun genre, aucune écoute : le repli au hasard.
    sql(
        &state,
        "INSERT INTO artists (id, name) VALUES (1, 'Quelqu''un')",
    );
    sql(
        &state,
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Sans genre', 1)",
    );
    let (st, v) = appel(&app, "GET", "/api/v1/home/recommendations", "1", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let a = &v.as_array().expect("tableau")[0];
    assert_eq!(a["reason"], json!("random"), "{a}");
    assert_eq!(a["raison"], json!({"type": "hasard"}), "{a}");
}

#[tokio::test]
async fn les_nouveautes_de_la_bibliotheque_disent_leur_date_d_ajout() {
    let (app, state) = app_et_etat();
    bibliotheque_jazz(&state);
    let (st, v) = appel(&app, "GET", "/api/v1/home/new-in-library", "1", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let premiers = v.as_array().expect("tableau");
    assert_eq!(premiers.len(), 3, "{v}");
    assert_eq!(premiers[0]["id"], json!(3), "le plus récent d'abord : {v}");
    assert_eq!(
        premiers[0]["raison"],
        json!({"type": "ajout_recent", "ajoute_le": 1760000200}),
        "{v}"
    );
}

async fn ecarter(app: &axum::Router, profil: &str, corps: Value) {
    let (st, v) = appel(
        app,
        "POST",
        "/api/v1/home/dismissed-suggestions",
        profil,
        Some(corps),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

/// La bibliothèque Jazz, avec un nombre de pistes par album (la reprise ne
/// propose qu'un album NON fini) et deux écoutes datées : Kind of Blue
/// (album 1) à midi, Sunday at the Village Vanguard (album 3) à dix heures.
fn bibliotheque_jazz_ecoutee(state: &AppState) {
    bibliotheque_jazz(state);
    sql(state, "UPDATE albums SET track_count = 10");
    sql(
        state,
        "UPDATE listen_history SET listened_at = '2026-10-12T12:00:00Z' WHERE album_id = 1",
    );
    sql(
        state,
        "INSERT INTO listen_history (track_id, album_id, title, artist_name, album_title, source, listened_at) \
         VALUES (3, 3, 'Gloria''s Step', 'Bill Evans', 'Sunday at the Village Vanguard', 'local', \
                 '2026-10-12T10:00:00Z')",
    );
}

fn albums_de(v: &Value) -> Vec<i64> {
    let mut ids: Vec<i64> = v
        .as_array()
        .expect("un tableau")
        .iter()
        .filter_map(|a| a["album_id"].as_i64().or(a["id"].as_i64()))
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn la_reprise_dit_quand_et_entend_pas_pour_moi() {
    let (app, state) = app_et_etat();
    bibliotheque_jazz_ecoutee(&state);
    ProfileRepo::with_backend(state.backend.clone())
        .create("voisin", Some("Le voisin"), None)
        .expect("profil 2");

    let (st, v) = appel(
        &app,
        "GET",
        "/api/v1/home/continue-listening?limit=10",
        "1",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(albums_de(&v), vec![1, 3], "{v}");
    let kob = v
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["album_id"] == json!(1))
        .unwrap();
    // 2026-10-12T12:00:00Z
    assert_eq!(
        kob["raison"],
        json!({"type": "reprise", "ecoute_le": 1_791_806_400_i64}),
        "{kob}"
    );

    ecarter(
        &app,
        "1",
        json!({"item_type": "artist", "item_id": "Bill Evans"}),
    )
    .await;
    let (_, v) = appel(
        &app,
        "GET",
        "/api/v1/home/continue-listening?limit=10",
        "1",
        None,
    )
    .await;
    assert_eq!(
        albums_de(&v),
        vec![1],
        "Bill Evans écarté par le profil 1 : {v}"
    );
    let (_, v) = appel(
        &app,
        "GET",
        "/api/v1/home/continue-listening?limit=10",
        "2",
        None,
    )
    .await;
    assert_eq!(
        albums_de(&v),
        vec![1, 3],
        "le profil 2 n'a rien écarté : {v}"
    );
}

#[tokio::test]
async fn les_mix_disent_leur_genre_et_laissent_l_album_ecarte() {
    let (app, state) = app_et_etat();
    bibliotheque_jazz(&state);
    ProfileRepo::with_backend(state.backend.clone())
        .create("voisin", Some("Le voisin"), None)
        .expect("profil 2");
    let (st, v) = appel(&app, "GET", "/api/v1/home/top-mixes", "1", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let mix = &v.as_array().expect("tableau")[0];
    assert_eq!(
        mix["raison"],
        json!({"type": "genre_frequent", "genre": "Jazz"}),
        "{mix}"
    );
    assert_eq!(albums_de(&mix["tracks"]), vec![1, 2, 3], "{mix}");

    ecarter(
        &app,
        "1",
        json!({"item_type": "album", "source": "local", "item_id": "2"}),
    )
    .await;
    let (_, v) = appel(&app, "GET", "/api/v1/home/top-mixes", "1", None).await;
    assert_eq!(albums_de(&v[0]["tracks"]), vec![1, 3], "{v}");
    let (_, v) = appel(&app, "GET", "/api/v1/home/top-mixes", "2", None).await;
    assert_eq!(albums_de(&v[0]["tracks"]), vec![1, 2, 3], "{v}");
}

#[tokio::test]
async fn la_page_composee_parle_la_langue_et_filtre_par_profil() {
    let (app, state) = app_et_etat();
    bibliotheque_jazz(&state);
    sql(&state, "UPDATE albums SET track_count = 10");
    ProfileRepo::with_backend(state.backend.clone())
        .create("voisin", Some("Le voisin"), None)
        .expect("profil 2");
    ecarter(
        &app,
        "1",
        json!({"item_type": "album", "source": "local", "item_id": "2"}),
    )
    .await;

    let req = |langue: &str, profil: &str| {
        Request::builder()
            .uri("/api/v1/home")
            .header("X-Profile-Id", profil)
            .header("Accept-Language", langue)
            .body(Body::empty())
            .unwrap()
    };
    let lire = |r: axum::response::Response| async move {
        let b = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice::<Value>(&b).unwrap()
    };
    let section = |v: &Value, id: &str| {
        v["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == json!(id))
            .cloned()
            .unwrap_or_else(|| panic!("section {id} absente : {v}"))
    };

    let en = lire(app.clone().oneshot(req("en", "1")).await.unwrap()).await;
    assert_eq!(
        section(&en, "continue")["title"],
        json!("Continue listening"),
        "{en}"
    );
    assert_eq!(
        section(&en, "discover")["title"],
        json!("To discover"),
        "{en}"
    );
    assert_eq!(
        albums_de(&section(&en, "discover")["items"]),
        vec![3],
        "album 2 écarté : {en}"
    );
    assert_eq!(
        section(&en, "continue")["items"][0]["raison"]["type"],
        json!("reprise"),
        "{en}"
    );

    let fr = lire(app.clone().oneshot(req("fr", "2")).await.unwrap()).await;
    assert_eq!(
        section(&fr, "continue")["title"],
        json!("Continuer l'écoute"),
        "{fr}"
    );
    assert_eq!(
        albums_de(&section(&fr, "discover")["items"]),
        vec![2, 3],
        "profil 2 : {fr}"
    );
}
