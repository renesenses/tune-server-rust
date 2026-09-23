//! #4767 — `GET /library/artists/{id}/albums?sections=1` découpe la
//! discographie : « Compilations » et « Apparitions ».
//!
//! Demande de FabienM (forum, fil 1875, captures de Roon). La mesure du
//! 23/09/2026 sur le .18 a montré que `track_credits` est VIDE — elle n'est
//! peuplée que par un enrichissement à la demande — donc les deux sections
//! reposent sur l'artiste de CHAQUE piste (`tracks.artist_id`), la seule
//! donnée disponible sans réseau.
//!
//! Épreuves contre le VRAI routeur et une vraie base SQLite en mémoire :
//!
//! - **Compilations** : album marqué compilation portant au moins une piste
//!   de l'artiste ;
//! - **Apparitions** : album NON compilation d'un AUTRE artiste portant au
//!   moins une piste de l'artiste ;
//! - l'album de l'artiste LUI-MÊME n'est dans aucune des deux (contre-épreuve
//!   de #4651, qui vient de purger la discographie des albums d'autrui) ;
//! - une section vide est **absente** de la réponse, jamais un tableau vide ;
//! - sans `sections=1`, la réponse reste le TABLEAU nu d'avant ;
//! - le coût, sur une base synthétique à l'échelle du .18 (~49 000 pistes),
//!   en `#[ignore]` : `--ignored --nocapture`.
//!
//! Cible `[[test]]` propre (`autotests = false`), hors de `server_contracts`.

use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

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
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn exec(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

/// Les titres d'une section, dans l'ordre rendu. `None` = clé absente.
fn section(body: &Value, cle: &str) -> Option<Vec<String>> {
    Some(
        body.get(cle)?
            .as_array()
            .unwrap_or_else(|| panic!("`{cle}` n'est pas un tableau : {body}"))
            .iter()
            .map(|a| a["title"].as_str().unwrap_or_default().to_string())
            .collect(),
    )
}

/// La bibliothèque de banc, calquée sur les captures de FabienM.
///
/// | album                    | artiste d'album   | compilation | pistes (artiste) |
/// |--------------------------|-------------------|-------------|------------------|
/// | Harvest                  | 1 — Neil Young    | non         | 1                |
/// | Hits from the 60s        | 2 — Artistes div. | OUI         | 1, 3             |
/// | Deja Vu                  | 3 — CSN           | non         | 1, 3             |
/// | Hits sans lui            | 2 — Artistes div. | OUI         | 3                |
/// | Autre album sans lui     | 3 — CSN           | non         | 3                |
///
/// Attendu pour l'artiste 1 : discographie = Harvest ; compilations = Hits
/// from the 60s ; apparitions = Deja Vu.
/// Attendu pour l'artiste 3 : compilations = les deux ; apparitions ABSENTE
/// (Harvest ne porte aucune de ses pistes).
fn bibliotheque() -> axum::Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    for (id, nom) in [
        (1, "Neil Young"),
        (2, "Artistes divers"),
        (3, "Crosby, Stills & Nash"),
        (4, "Artiste sans rien"),
    ] {
        exec(
            &state,
            &format!("INSERT INTO artists (id, name) VALUES ({id}, '{nom}')"),
        );
    }
    let albums: [(i64, &str, i64, i64, i64); 5] = [
        // (id, titre, artiste d'album, compilation, annee)
        (1, "Harvest", 1, 0, 1972),
        (2, "Hits from the 60s", 2, 1, 1969),
        (3, "Deja Vu", 3, 0, 1970),
        (4, "Hits sans lui", 2, 1, 1968),
        (5, "Autre album sans lui", 3, 0, 1977),
    ];
    for (id, titre, artiste, compil, annee) in albums {
        exec(
            &state,
            &format!(
                "INSERT INTO albums (id, title, artist_id, is_compilation, year, source) \
                 VALUES ({id}, '{titre}', {artiste}, {compil}, {annee}, 'local')"
            ),
        );
    }
    let pistes: [(i64, &str, i64, i64); 7] = [
        // (id, titre, album, artiste de la piste)
        (1, "Heart of Gold", 1, 1),
        (2, "Down by the River", 2, 1),
        (3, "Marrakesh Express", 2, 3),
        (4, "Helpless", 3, 1),
        (5, "Carry On", 3, 3),
        (6, "Guinnevere", 4, 3),
        (7, "Just a Song", 5, 3),
    ];
    for (id, titre, album, artiste) in pistes {
        exec(
            &state,
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format) \
                 VALUES ({id}, '{titre}', {album}, {artiste}, '/m/{id}.flac', 200000, 'flac')"
            ),
        );
    }
    tune_server::routes::router(state)
}

#[tokio::test]
async fn les_deux_sections_de_la_page_artiste_4767() {
    let app = bibliotheque();
    let (status, body) = get(&app, "/api/v1/library/artists/1/albums?sections=1").await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        section(&body, "albums"),
        Some(vec!["Harvest".to_string()]),
        "la discographie doit rester celle de #4651 : les albums de l'artiste, \
         et eux seuls — {body}"
    );
    assert_eq!(
        section(&body, "compilations"),
        Some(vec!["Hits from the 60s".to_string()]),
        "compilation portant une piste de l'artiste — {body}"
    );
    assert_eq!(
        section(&body, "appearances"),
        Some(vec!["Deja Vu".to_string()]),
        "album d'un AUTRE artiste portant une piste de l'artiste — {body}"
    );

    // Le compte que la page affiche entre parenthèses est la longueur du
    // tableau : rien d'autre ne le porte, donc rien ne peut diverger.
    assert_eq!(body["compilations"].as_array().unwrap().len(), 1);
    assert_eq!(body["appearances"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn l_album_de_l_artiste_lui_meme_n_est_dans_aucune_section_4767() {
    let app = bibliotheque();
    let (_, body) = get(&app, "/api/v1/library/artists/1/albums?sections=1").await;
    for cle in ["compilations", "appearances"] {
        let titres = section(&body, cle).unwrap_or_default();
        assert!(
            !titres.iter().any(|t| t == "Harvest"),
            "« Harvest » est l'album de l'artiste : il est déjà dans la \
             discographie et ne doit pas être répété dans « {cle} » — {titres:?}"
        );
    }
}

#[tokio::test]
async fn une_section_vide_est_absente_jamais_un_tableau_vide_4767() {
    let app = bibliotheque();

    // L'artiste 3 n'apparaît sur aucun album NON compilation d'autrui
    // (« Harvest » ne porte aucune de ses pistes).
    let (_, body) = get(&app, "/api/v1/library/artists/3/albums?sections=1").await;
    assert_eq!(
        section(&body, "compilations"),
        Some(vec![
            "Hits sans lui".to_string(),
            "Hits from the 60s".to_string(),
        ]),
        "les deux compilations, triées par année — {body}"
    );
    assert!(
        body.get("appearances").is_none(),
        "« appearances » doit être ABSENTE, pas un tableau vide — {body}"
    );

    // Un artiste sans rien : les deux clés absentes, et une discographie vide.
    let (status, body) = get(&app, "/api/v1/library/artists/4/albums?sections=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(section(&body, "albums"), Some(vec![]));
    assert!(body.get("compilations").is_none(), "{body}");
    assert!(body.get("appearances").is_none(), "{body}");
}

#[tokio::test]
async fn sans_le_drapeau_la_reponse_reste_le_tableau_nu_4767() {
    let app = bibliotheque();
    for chemin in [
        "/api/v1/library/artists/1/albums",
        "/api/v1/library/artists/1/albums?sections=0",
    ] {
        let (status, body) = get(&app, chemin).await;
        assert_eq!(status, StatusCode::OK);
        let items = body
            .as_array()
            .unwrap_or_else(|| panic!("{chemin} doit rendre un TABLEAU : {body}"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "Harvest");
    }
}

/// Le coût de la route sur une bibliothèque à l'échelle du .18.
///
/// Lancé à la main : `cargo test --test page_artiste_sections_4767 -- \
/// --ignored --nocapture`. Une page artiste ne doit pas devenir lente parce
/// qu'on lui a ajouté deux sections.
#[tokio::test]
#[ignore]
async fn cout_des_sections_sur_une_grande_bibliotheque_4767() {
    let n_albums: i64 = 4_900;
    let n_pistes = n_albums * 10; // ~49 000, l'ordre de grandeur du .18
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    exec(
        &state,
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 500) \
         INSERT INTO artists (id, name) SELECT x, 'Artiste ' || x FROM c",
    );
    // Un album sur dix est une compilation ; l'artiste d'album tourne sur les
    // 500 fiches.
    exec(
        &state,
        &format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < {n_albums}) \
             INSERT INTO albums (id, title, artist_id, is_compilation, year, source) \
             SELECT x, 'Album ' || x, (x % 500) + 1, CASE WHEN x % 10 = 0 THEN 1 ELSE 0 END, \
             1960 + (x % 60), 'local' FROM c"
        ),
    );
    // L'artiste de la PISTE ne suit pas celui de l'album : c'est ce décalage
    // que les deux sections exploitent.
    exec(
        &state,
        &format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < {n_pistes}) \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format) \
             SELECT x, 'T' || x, (x - 1) / 10 + 1, (x % 500) + 1, '/m/' || x || '.flac', \
             200000, 'flac' FROM c"
        ),
    );
    let app = tune_server::routes::router(state);

    for (etiquette, chemin) in [
        ("sans sections", "/api/v1/library/artists/7/albums"),
        (
            "avec sections",
            "/api/v1/library/artists/7/albums?sections=1",
        ),
    ] {
        let mut meilleure = f64::MAX;
        let mut pire: f64 = 0.0;
        for _ in 0..5 {
            let t = Instant::now();
            let (status, body) = get(&app, chemin).await;
            let ms = t.elapsed().as_secs_f64() * 1e3;
            meilleure = meilleure.min(ms);
            pire = pire.max(ms);
            assert_eq!(status, StatusCode::OK);
            assert!(!body.is_null());
        }
        println!(
            "MESURE #4767 — {n_albums} albums / {n_pistes} pistes, {etiquette} : \
             {meilleure:.1} ms au mieux, {pire:.1} ms au pire"
        );
    }
}
