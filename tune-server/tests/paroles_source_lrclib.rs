//! Garde-fou : la provenance `"lrclib"` est nommée sur les DEUX routes que le
//! client web consomme.
//!
//! ## Pourquoi
//!
//! Le serveur nomme la provenance des paroles dans chaque réponse 200 :
//! `{"synced": bool, "source": "lrc"|"tag"|"lrclib", "lines": [...]}`. Le
//! contrat en tête de `track_lyrics` (`routes/library/tracks.rs`) le marque
//! « the web client is built against this — do not change ».
//!
//! Ce champ n'était affiché nulle part. Un testeur (fil forum 1555, Tune
//! 0.9.110) a découvert seul, par déduction, que son lecteur interrogeait un
//! service en ligne, et a dû poser la question sur le forum : « Tune va les
//! chercher sur un site ? ». Le client web l'affiche depuis
//! renesenses/tune-web-client#592 — mais il ne peut afficher que ce que le
//! serveur émet, et **rien côté serveur ne garde la valeur qui compte le plus**.
//!
//! Car l'existant ne couvrait que les sources LOCALES. `integration.rs` vérifie
//! `source == "lrc"` (sidecar) et `source == "tag"` (étiquette embarquée) ; il
//! annonce lui-même son abstention en tête de section : « no network involved:
//! the lyrics_lrclib_enabled setting stays unset → LRCLIB is skipped ».
//!
//! `"lrclib"` est bien asserté ailleurs — `karaoke_plugin.rs:129` — mais sur
//! `/api/v1/ext/karaoke/lyrics/{id}`, une route de **greffon** que le client web
//! n'appelle jamais, au format différent (`lines[].time_ms`, et non `t_ms`).
//! Cette assertion-là ne garde donc pas le contrat web. Le seul cas où
//! l'utilisateur doit *vraiment* être informé — le titre et l'artiste de ce
//! qu'il écoute partent chez `lrclib.net` — était le seul non gardé.
//!
//! Et `GET /lyrics/by-meta`, qui sert les radios et les pistes streaming
//! (Qobuz/Tidal, sans id de bibliothèque), n'avait **aucun test** : sa source
//! est pourtant toujours `"lrclib"`, puisqu'il n'a ni fichier ni étiquette à
//! lire. C'est un appel réseau sortant à 100 %.
//!
//! ## Aucun réseau ici
//!
//! Les deux routes sont *cache-first*. On sème `lyrics_cache` — sous l'id de la
//! piste pour la route par id, sous `meta_cache_id(titre, artiste)` pour
//! `by-meta` — et le gestionnaire répond depuis la base sans jamais appeler
//! `lrclib.net`. Ces tests sont donc hermétiques et déterministes.
//!
//! Enregistré dans `server_contracts.rs` (`autotests = false` : un fichier non
//! déclaré n'est jamais compilé).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

/// Les trois seules valeurs du contrat. Toute autre valeur signifie que la
/// cascade a changé sans que le client web en soit informé.
const SOURCES_DU_CONTRAT: [&str; 3] = ["lrc", "tag", "lrclib"];

fn make_app_with_state() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

/// Active l'opt-in LRCLIB (`lyrics_lrclib_enabled`), sans quoi les deux routes
/// répondent 404 avant même de consulter le cache.
fn activer_lrclib(state: &AppState) {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("lyrics_lrclib_enabled", "true")
        .expect("le réglage LRCLIB s'écrit");
}

/// Insère une piste AVEC un artiste réel.
///
/// `tracks` n'a pas de colonne `artist_name` : `TrackRepo::create` ne persiste
/// que `artist_id`, et `artist_name` est reconstitué par jointure à la lecture.
/// Poser `t.artist_name` sur la structure ne fait donc rien — la piste relue
/// revient sans artiste, et `track_lyrics` s'arrête sur sa garde
/// `if artist.is_empty() { return no_lyrics(); }` avant même de consulter le
/// cache. Il faut créer la ligne `artists` et lier `artist_id`.
fn insert_track(state: &AppState, title: &str, artist: &str) -> i64 {
    let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let aid = artist_repo
        .create(&tune_core::db::models::Artist::new(artist.into()))
        .expect("insert artist");

    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut t = tune_core::db::models::Track::new(title.into());
    t.artist_id = Some(aid);
    t.duration_ms = 180_000;
    let tid = repo.create(&t).expect("insert track");

    // Le fixture ne vaut que si l'artiste revient bien par la jointure : sans
    // cette vérification, une régression du schéma rendrait ces tests verts en
    // testant le vide (404 attendu partout).
    let relue = repo.get(tid).expect("relire la piste").expect("piste");
    assert_eq!(
        relue.artist_name.as_deref(),
        Some(artist),
        "le fixture doit produire une piste dont l'artiste est lisible"
    );
    tid
}

/// Sème une entrée de cache positive. `track_id` est l'id de piste pour la
/// route par id, ou `meta_cache_id(...)` (négatif) pour `by-meta`.
fn semer_cache(
    state: &AppState,
    track_id: i64,
    title: &str,
    artist: &str,
    synced: Option<&str>,
    plain: Option<&str>,
) {
    tune_core::lyrics::store_cache_entry(&state.backend, track_id, title, artist, synced, plain);
}

/// Vérifie la forme complète du contrat, pas seulement la source : une réponse
/// qui nommerait la bonne provenance dans un corps malformé ne vaut rien.
fn assert_contrat(body: &Value, source_attendue: &str, synced_attendu: bool) {
    let source = body["source"].as_str().unwrap_or_else(|| {
        panic!("le contrat impose un champ `source` de type chaîne — corps : {body}")
    });
    assert!(
        SOURCES_DU_CONTRAT.contains(&source),
        "`source` = {source:?} hors contrat {SOURCES_DU_CONTRAT:?} — corps : {body}"
    );
    assert_eq!(source, source_attendue, "mauvaise provenance : {body}");
    assert_eq!(
        body["synced"], synced_attendu,
        "mauvais drapeau `synced` : {body}"
    );
    let lines = body["lines"]
        .as_array()
        .unwrap_or_else(|| panic!("le contrat impose un tableau `lines` — corps : {body}"));
    assert!(
        !lines.is_empty(),
        "`lines` ne doit jamais être vide : {body}"
    );
    for l in lines {
        assert!(
            l["text"].is_string(),
            "chaque ligne porte un `text` : {body}"
        );
        assert!(
            l["t_ms"].is_u64() || l["t_ms"].is_null(),
            "`t_ms` est un entier ou null : {body}"
        );
    }
}

// ───────────────── GET /library/tracks/{id}/lyrics — source "lrclib" ────────

#[tokio::test]
async fn route_par_id_nomme_lrclib_sur_des_paroles_synchronisees() {
    let (app, state) = make_app_with_state();
    activer_lrclib(&state);
    let tid = insert_track(&state, "Chanson Distante", "Artiste Distant");
    semer_cache(
        &state,
        tid,
        "Chanson Distante",
        "Artiste Distant",
        Some("[00:10.00] Une ligne\n[00:20.50] Une autre"),
        None,
    );

    let (status, body) = get(&app, &format!("/api/v1/library/tracks/{tid}/lyrics")).await;

    assert_eq!(status, StatusCode::OK, "corps : {body}");
    assert_contrat(&body, "lrclib", true);
    let lines = body["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 2, "corps : {body}");
    assert_eq!(lines[0]["t_ms"], 10_000);
    assert_eq!(lines[0]["text"], "Une ligne");
    assert_eq!(lines[1]["t_ms"], 20_500);
}

#[tokio::test]
async fn route_par_id_nomme_lrclib_sur_des_paroles_non_synchronisees() {
    let (app, state) = make_app_with_state();
    activer_lrclib(&state);
    let tid = insert_track(&state, "Sans Horodatage", "Artiste Distant");
    semer_cache(
        &state,
        tid,
        "Sans Horodatage",
        "Artiste Distant",
        None,
        Some("Première ligne\n\nDeuxième ligne"),
    );

    let (status, body) = get(&app, &format!("/api/v1/library/tracks/{tid}/lyrics")).await;

    assert_eq!(status, StatusCode::OK, "corps : {body}");
    assert_contrat(&body, "lrclib", false);
    let lines = body["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 2, "les lignes vides sont retirées : {body}");
    assert_eq!(lines[0]["t_ms"], Value::Null);
    assert_eq!(lines[0]["text"], "Première ligne");
    assert_eq!(lines[1]["text"], "Deuxième ligne");
}

// ───────────────── GET /lyrics/by-meta — radios et streaming ────────────────

#[tokio::test]
async fn route_by_meta_nomme_lrclib_sur_des_paroles_synchronisees() {
    let (app, state) = make_app_with_state();
    activer_lrclib(&state);
    let (titre, artiste) = ("Titre Radio", "Artiste Radio");
    // Pas de piste en base : c'est tout l'objet de cette route.
    semer_cache(
        &state,
        tune_core::lyrics::meta_cache_id(titre, artiste),
        titre,
        artiste,
        Some("[00:05.00] Refrain"),
        None,
    );

    let url = format!(
        "/api/v1/lyrics/by-meta?title={}&artist={}",
        urlencoding(titre),
        urlencoding(artiste)
    );
    let (status, body) = get(&app, &url).await;

    assert_eq!(status, StatusCode::OK, "corps : {body}");
    assert_contrat(&body, "lrclib", true);
    let lines = body["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 1, "corps : {body}");
    assert_eq!(lines[0]["t_ms"], 5_000);
    assert_eq!(lines[0]["text"], "Refrain");
}

#[tokio::test]
async fn route_by_meta_nomme_lrclib_sur_des_paroles_non_synchronisees() {
    let (app, state) = make_app_with_state();
    activer_lrclib(&state);
    let (titre, artiste) = ("Titre Streaming", "Artiste Streaming");
    semer_cache(
        &state,
        tune_core::lyrics::meta_cache_id(titre, artiste),
        titre,
        artiste,
        None,
        Some("Une seule ligne"),
    );

    let url = format!(
        "/api/v1/lyrics/by-meta?title={}&artist={}",
        urlencoding(titre),
        urlencoding(artiste)
    );
    let (status, body) = get(&app, &url).await;

    assert_eq!(status, StatusCode::OK, "corps : {body}");
    assert_contrat(&body, "lrclib", false);
    assert_eq!(body["lines"][0]["text"], "Une seule ligne");
}

/// Le réglage éteint prime sur un cache garni : aucune parole distante ne doit
/// sortir, donc aucune source à afficher. C'est la garde que le testeur du fil
/// 1555 supposait active — si elle cédait, l'affichage de la provenance
/// deviendrait le seul témoin d'une sortie réseau non consentie.
#[tokio::test]
async fn reglage_eteint_ne_sert_aucune_parole_distante() {
    let (app, state) = make_app_with_state();
    // `lyrics_lrclib_enabled` volontairement NON positionné.
    let tid = insert_track(&state, "Chanson Distante", "Artiste Distant");
    semer_cache(
        &state,
        tid,
        "Chanson Distante",
        "Artiste Distant",
        Some("[00:10.00] Une ligne"),
        None,
    );

    let (status, body) = get(&app, &format!("/api/v1/library/tracks/{tid}/lyrics")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "corps : {body}");
    assert_eq!(body["error"], "no_lyrics");

    let url = format!(
        "/api/v1/lyrics/by-meta?title={}&artist={}",
        urlencoding("Chanson Distante"),
        urlencoding("Artiste Distant")
    );
    let (status, body) = get(&app, &url).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "corps : {body}");
    assert_eq!(body["error"], "no_lyrics");
}

/// Encodage minimal des seuls caractères présents dans ces fixtures (espaces).
/// Évite une dépendance de test pour trois URL.
fn urlencoding(s: &str) -> String {
    s.replace(' ', "%20")
}
