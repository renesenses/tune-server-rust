//! #4767 — les crédits MusicBrainz PAR DISQUE et les deux sections qu'ils
//! ouvrent sur la page artiste : « Collaborations » et « Reprises ».
//!
//! Épreuves contre le VRAI routeur et une vraie base SQLite en mémoire, sans
//! réseau : la réponse MusicBrainz est la fixture RÉELLE de *Déjà vu*
//! (Crosby, Stills, Nash & Young), appliquée par `appliquer_release` — la
//! fonction même que la passe appelle après sa requête.
//!
//! - idempotence : appliquer deux fois la même release laisse EXACTEMENT les
//!   mêmes lignes, et le disque sort des candidats (reprise) ;
//! - classement : Collaborations groupées par artiste principal, Reprises,
//!   exclusions (son propre disque, compilation « Various Artists »,
//!   producteur seul, disque déjà en « Apparitions ») ;
//! - focus : `focus_track_ids` = les pistes où il joue (ou qu'il a écrites) ;
//! - une section vide est ABSENTE ;
//! - `POST`/`GET /system/enrich-credits` : 202 puis état lisible.
//!
//! Cible `[[test]]` propre (`autotests = false`), hors de `server_contracts`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::metadata::credits_release::{albums_candidats, appliquer_release};
use tune_server::state::AppState;

const DEJA_VU: &str =
    include_str!("../../tune-core/tests/fixtures/musicbrainz/release_deja_vu_credits.json");
const NEIL_YOUNG_MBID: &str = "75167b8b-44e4-407b-9d35-effe87b223cf";
const RELEASE_DEJA_VU: &str = "f33a4c92-3275-4348-8bf2-237a26976f4e";

async fn requete(app: &axum::Router, methode: &str, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn exec(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

fn release() -> Value {
    serde_json::from_str(DEJA_VU).unwrap()
}

/// La bibliothèque de banc.
///
/// | album | titre                  | artiste d'album        | ce que Neil Young y fait                     |
/// |-------|------------------------|------------------------|----------------------------------------------|
/// | 1     | Harvest                | 1 Neil Young           | son disque                                   |
/// | 3     | Déjà vu (MBID release) | 3 CSNY                 | crédits MusicBrainz réels (fixture)          |
/// | 4     | Live sans nom          | 3 CSNY                 | artiste d'une piste → « Apparitions »        |
/// | 5     | Helpless (k.d. lang)   | 5 k.d. lang            | compositeur SEUL, reconnu par MBID → Reprise |
/// | 6     | Hits 70                | 2 Various Artists      | pianiste sur une compilation → exclu         |
/// | 8     | Session                | 3 CSNY                 | producteur seul → exclu                      |
fn bibliotheque() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    exec(
        &state,
        &format!(
            "INSERT INTO artists (id, name, musicbrainz_id) VALUES \
             (1, 'Neil Young', '{NEIL_YOUNG_MBID}'), (2, 'Various Artists', NULL), \
             (3, 'Crosby, Stills, Nash & Young', NULL), (5, 'k.d. lang', NULL), \
             (7, 'Un groupe', NULL), (9, 'Personne', NULL)"
        ),
    );
    exec(
        &state,
        &format!(
            "INSERT INTO albums (id, title, artist_id, is_compilation, year, source, musicbrainz_release_id) VALUES \
             (1, 'Harvest', 1, 0, 1972, 'local', NULL), \
             (3, 'Déjà vu', 3, 0, 1970, 'local', '{RELEASE_DEJA_VU}'), \
             (4, 'Live sans nom', 3, 0, 1971, 'local', NULL), \
             (5, 'Helpless (k.d. lang)', 5, 0, 2004, 'local', NULL), \
             (6, 'Hits 70', 2, 0, 1975, 'local', NULL), \
             (8, 'Session', 3, 0, 1980, 'local', NULL)"
        ),
    );
    // Les dix titres de Déjà vu, SANS MBID d'enregistrement : l'appariement
    // passe par la place et le titre.
    for (i, p) in tune_core::metadata::credits_release::pistes_de_la_release(&release())
        .iter()
        .enumerate()
    {
        let id = 301 + i as i64;
        let titre = p.titre.replace('\'', "''");
        exec(
            &state,
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format, disc_number, track_number) \
                 VALUES ({id}, '{titre}', 3, 3, '/m/{id}.flac', 200000, 'flac', 1, {})",
                p.numero
            ),
        );
    }
    for (id, titre, album, artiste) in [
        (101, "Heart of Gold", 1, 1),
        (401, "Ohio", 4, 1),
        (501, "Helpless", 5, 5),
        (601, "Un tube", 6, 7),
        (801, "Prise 1", 8, 3),
    ] {
        exec(
            &state,
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format, disc_number, track_number) \
                 VALUES ({id}, '{titre}', {album}, {artiste}, '/m/{id}.flac', 200000, 'flac', 1, 1)"
            ),
        );
    }
    // Crédits écrits par d'autres voies que la release (saisie, autre passe).
    exec(
        &state,
        &format!(
            "INSERT INTO track_credits (track_id, artist_id, artist_name, role, instrument, position, artist_mbid) VALUES \
             (101, 1, 'Neil Young', 'performer', 'guitar', 0, NULL), \
             (401, 1, 'Neil Young', 'performer', 'guitar', 0, NULL), \
             (501, NULL, 'N. Young', 'composer', NULL, 0, '{NEIL_YOUNG_MBID}'), \
             (601, NULL, 'Neil Young', 'performer', 'piano', 0, NULL), \
             (801, NULL, 'Neil Young', 'producer', NULL, 0, NULL)"
        ),
    );
    state
}

fn credits_de(state: &AppState, album_id: i64) -> Vec<Vec<String>> {
    state
        .backend
        .query_many(
            &format!(
                "SELECT tc.track_id, COALESCE(CAST(tc.artist_id AS TEXT), ''), tc.artist_name, tc.role, \
                        COALESCE(tc.instrument, ''), tc.position, COALESCE(tc.artist_mbid, '') \
                 FROM track_credits tc JOIN tracks t ON t.id = tc.track_id \
                 WHERE t.album_id = {album_id} ORDER BY tc.track_id, tc.position"
            ),
            &[],
        )
        .unwrap()
        .into_iter()
        .map(|r| {
            r.iter()
                .map(|v| {
                    v.as_string()
                        .or_else(|| v.as_i64().map(|i| i.to_string()))
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect()
}

/// Appliquer deux fois la même release : mêmes lignes, au caractère près.
/// Et le disque interrogé sort des candidats — c'est le curseur de reprise.
#[tokio::test]
async fn appliquer_la_release_est_idempotent_et_pose_le_curseur_4767() {
    let state = bibliotheque();
    assert_eq!(
        albums_candidats(&state.backend),
        vec![(3, RELEASE_DEJA_VU.to_string())],
        "seul le disque qui porte un MBID de release est candidat"
    );

    let premier = appliquer_release(&state.backend, 3, &release());
    assert_eq!(premier.pistes_creditees, 10, "{premier:?}");
    assert_eq!(premier.pistes_sans_correspondance, 0);
    let avant = credits_de(&state, 3);
    assert!(!avant.is_empty());

    let second = appliquer_release(&state.backend, 3, &release());
    assert_eq!(second, premier);
    assert_eq!(
        credits_de(&state, 3),
        avant,
        "la seconde application a changé les lignes"
    );

    assert!(
        albums_candidats(&state.backend).is_empty(),
        "un disque déjà interrogé ne doit plus être candidat"
    );

    // La fiche de Neil Young est LIÉE par son MBID, et le MBID est écrit.
    let neil: Vec<&Vec<String>> = avant.iter().filter(|l| l[2] == "Neil Young").collect();
    assert!(!neil.is_empty());
    assert!(
        neil.iter().all(|l| l[1] == "1" && l[6] == NEIL_YOUNG_MBID),
        "{neil:?}"
    );
}

fn titres(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("pas un tableau : {v}"))
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn collaborations_et_reprises_de_la_page_artiste_4767() {
    let state = bibliotheque();
    appliquer_release(&state.backend, 3, &release());
    let app = tune_server::routes::router(state);

    let (status, body) = requete(&app, "GET", "/api/v1/library/artists/1/albums?sections=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(titres(&body["albums"]), ["Harvest"], "{body}");
    assert_eq!(titres(&body["appearances"]), ["Live sans nom"], "{body}");

    // Collaborations : UN groupe, l'artiste principal de Déjà vu.
    let collab = body["collaborations"]
        .as_array()
        .unwrap_or_else(|| panic!("`collaborations` absente : {body}"));
    assert_eq!(collab.len(), 1, "{collab:?}");
    assert_eq!(collab[0]["artist_id"], 3);
    assert_eq!(collab[0]["artist_name"], "Crosby, Stills, Nash & Young");
    assert_eq!(titres(&collab[0]["albums"]), ["Déjà vu"]);
    let deja_vu = &collab[0]["albums"][0];
    // Le FOCUS : il joue sur Almost Cut My Hair (3), Helpless (4),
    // Woodstock (5), Country Girl (9), Everybody I Love You (10) — pas sur les
    // titres où il n'est que producteur.
    assert_eq!(
        deja_vu["focus_track_ids"],
        serde_json::json!([303, 304, 305, 309, 310]),
        "{deja_vu}"
    );
    let roles: Vec<&str> = deja_vu["credit_roles"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        roles.contains(&"guitar") && roles.contains(&"vocals"),
        "{roles:?}"
    );
    assert!(!roles.contains(&"producer"), "{roles:?}");

    // Reprises : compositeur SEUL, reconnu par son MBID sous un autre nom.
    assert_eq!(titres(&body["covers"]), ["Helpless (k.d. lang)"], "{body}");
    assert_eq!(
        body["covers"][0]["focus_track_ids"],
        serde_json::json!([501])
    );
    assert_eq!(
        body["covers"][0]["credit_roles"],
        serde_json::json!(["composer"])
    );

    // Exclusions : ni son disque, ni la compilation « Various Artists », ni
    // le disque où il n'est que producteur, ni celui déjà en « Apparitions ».
    let tout = serde_json::to_string(&body["collaborations"]).unwrap()
        + &serde_json::to_string(&body["covers"]).unwrap();
    for exclu in ["Harvest", "Hits 70", "Session", "Live sans nom"] {
        assert!(
            !tout.contains(exclu),
            "« {exclu} » ne doit être ni collaboration ni reprise"
        );
    }
}

#[tokio::test]
async fn sans_credit_les_deux_sections_sont_absentes_4767() {
    let app = tune_server::routes::router(bibliotheque());
    // Crosby, Stills, Nash & Young : aucun crédit à son nom hors de ses disques.
    for id in [3, 9] {
        let (status, body) = requete(
            &app,
            "GET",
            &format!("/api/v1/library/artists/{id}/albums?sections=1"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("collaborations").is_none(), "{body}");
        assert!(body.get("covers").is_none(), "{body}");
    }
    // Et sans `sections=1`, toujours le tableau nu.
    let (_, body) = requete(&app, "GET", "/api/v1/library/artists/1/albums").await;
    assert!(body.is_array(), "{body}");
}

/// La route : état lisible au repos, 202 au lancement avec le coût annoncé.
/// Hermétique : la bibliothèque n'a AUCUN disque portant un MBID de release,
/// donc la passe n'a aucun candidat et ne joint jamais MusicBrainz.
#[tokio::test]
async fn la_route_enrich_credits_annonce_son_cout_et_son_etat_4767() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state);

    let (status, etat) = requete(&app, "GET", "/api/v1/system/enrich-credits").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(etat["status"], "idle", "{etat}");
    for champ in [
        "total",
        "processed",
        "enriched",
        "tracks_credited",
        "unmatched",
        "unknown",
        "errors",
        "candidats",
        "albums_avec_mbid",
    ] {
        assert!(
            etat.get(champ).is_some(),
            "`{champ}` manque au repos : {etat}"
        );
    }

    let (status, lance) = requete(&app, "POST", "/api/v1/system/enrich-credits").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{lance}");
    assert_eq!(lance["status"], "credits_enrichment_started");
    assert_eq!(lance["candidats"], 0);
    assert!(lance["task_id"].as_str().is_some_and(|s| !s.is_empty()));
}
