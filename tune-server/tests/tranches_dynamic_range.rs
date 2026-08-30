//! Classer et filtrer les albums par tranches de Dynamic Range (#2144).
//!
//! Seconde moitié de la demande de Patatorz (fil forum 1418, miroir #1699) :
//! la première — lire le tag et l'afficher — est livrée depuis la v0.9.82
//! (#1806, #1809, puis #1388 pour le DR par piste). Le classement, lui, avait
//! été explicitement reporté et n'était suivi nulle part.
//!
//! Ces épreuves tournent contre le VRAI routeur et une VRAIE base SQLite en
//! mémoire, parce que c'est le seul niveau où l'on prouve à la fois que les
//! paramètres de requête se désérialisent, que le SQL s'exécute, et que le
//! `total` annoncé compte la même chose que la liste rendue. Un test de
//! construction de chaîne ne verrait rien de tout cela.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

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

/// Une bibliothèque de six albums dont on connaît le DR d'avance.
///
/// | album    | tag `dr_album` | ce qu'il éprouve                       |
/// |----------|----------------|----------------------------------------|
/// | Alpha    | `6`            | un master compressé                    |
/// | Bravo    | `14`           | un master dynamique                    |
/// | Charlie  | `9`            | l'entre-deux                           |
/// | Delta    | *(aucun)*      | le cas de LOIN le plus courant         |
/// | Echo     | `DR12.5`       | ce que `normalise_dr` recopie tel quel |
/// | Foxtrot  | `0`            | DR0 est une MESURE, pas une absence    |
fn bibliotheque() -> axum::Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    for (n, (titre, dr)) in [
        ("Alpha", Some("6")),
        ("Bravo", Some("14")),
        ("Charlie", Some("9")),
        ("Delta", None),
        ("Echo", Some("DR12.5")),
        ("Foxtrot", Some("0")),
    ]
    .into_iter()
    .enumerate()
    {
        let id = n + 1;
        state
            .backend
            .execute(
                &format!(
                    "INSERT INTO albums (id, title, source) VALUES ({id}, '{titre}', 'local')"
                ),
                &[],
            )
            .expect("insertion d'album");
        state
            .backend
            .execute(
                &format!(
                    "INSERT INTO tracks (id, title, album_id, file_path, duration_ms, format) \
                     VALUES ({id}, '{titre} — piste', {id}, '/music/{titre}.flac', 200000, 'flac')"
                ),
                &[],
            )
            .expect("insertion de piste");
        if let Some(v) = dr {
            // Le scan écrit le tag PAR PISTE (`track_metadata['dr_album']`,
            // #1806) même s'il décrit l'album : c'est là qu'il vit, dans le
            // fichier.
            state
                .backend
                .execute(
                    &format!(
                        "INSERT INTO track_metadata (track_id, key, value) \
                         VALUES ({id}, 'dr_album', '{v}')"
                    ),
                    &[],
                )
                .expect("insertion du tag DR");
        }
    }
    tune_server::routes::router(state)
}

fn titres(body: &Value) -> Vec<String> {
    body.get("items")
        .and_then(Value::as_array)
        .expect("la liste doit rendre des items")
        .iter()
        .filter_map(|a| a.get("title").and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn total(body: &Value) -> i64 {
    body.get("total").and_then(Value::as_i64).unwrap_or(-1)
}

/// Sans aucun paramètre de DR, la réponse est CELLE D'AVANT.
///
/// C'est la seule garantie qui compte pour les clients déjà livrés — iOS,
/// macOS, Android, le client web, les points de terminaison UPnP — dont aucun
/// ne connaît ces paramètres.
#[tokio::test]
async fn sans_parametre_la_liste_d_albums_est_inchangee_2144() {
    let app = bibliotheque();
    let (status, body) = get(&app, "/api/v1/library/albums?limit=50").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(total(&body), 6, "le total reste celui de la bibliothèque");
    assert_eq!(titres(&body).len(), 6, "les six albums sont rendus");
    // Et aucune clé nouvelle ne s'invite dans la charge utile d'un album.
    let premier = &body["items"][0];
    assert!(
        premier.get("dynamic_range").is_none(),
        "le contrat de la GRILLE ne change pas : le DR se lit sur la fiche \
         album (#1809) et par piste (#1388), pas dans la liste"
    );
}

/// `?sort=dynamic_range` — tri NUMÉRIQUE, non tagués en fin de liste.
#[tokio::test]
async fn le_tri_par_dynamic_range_repond_et_relegue_les_non_tagues_2144() {
    let app = bibliotheque();

    let (status, body) = get(
        &app,
        "/api/v1/library/albums?sort=dynamic_range&order=asc&limit=50",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        titres(&body),
        vec!["Foxtrot", "Alpha", "Charlie", "Bravo", "Delta", "Echo"],
        "0 < 6 < 9 < 14 — et non l'ordre alphabétique des chaînes, où « 14 » \
         précéderait « 6 » ; les sans-valeur ferment la marche"
    );

    let (_, body) = get(
        &app,
        "/api/v1/library/albums?sort=dynamic_range&order=desc&limit=50",
    )
    .await;
    assert_eq!(
        titres(&body),
        vec!["Bravo", "Charlie", "Alpha", "Foxtrot", "Delta", "Echo"],
        "en décroissant AUSSI les non tagués finissent"
    );
}

/// `?dr_min` / `?dr_max` — la tranche, bornes incluses, et son `total`.
#[tokio::test]
async fn la_tranche_filtre_et_le_total_compte_la_meme_chose_2144() {
    let app = bibliotheque();

    for (requete, attendu) in [
        ("dr_min=8&dr_max=14", vec!["Charlie", "Bravo"]),
        ("dr_min=10", vec!["Bravo"]),
        ("dr_max=8", vec!["Foxtrot", "Alpha"]),
        ("dr_min=9&dr_max=9", vec!["Charlie"]),
        ("dr_min=20", vec![]),
        (
            "dr_min=0&dr_max=99",
            vec!["Foxtrot", "Alpha", "Charlie", "Bravo"],
        ),
    ] {
        let (status, body) = get(
            &app,
            &format!("/api/v1/library/albums?sort=dynamic_range&limit=50&{requete}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{requete}");
        assert_eq!(titres(&body), attendu, "tranche {requete}");
        // Le défaut qui rendrait la fonction inutilisable : un `total` de 6
        // sur une liste de 2 ferait paginer la grille dans le vide, comme en
        // #1391. Le tag DR n'existant que sur une poignée d'albums, l'écart
        // serait de plusieurs ordres de grandeur en vrai.
        assert_eq!(
            total(&body),
            attendu.len() as i64,
            "le total annoncé doit compter la TRANCHE, pas la bibliothèque \
             ({requete})"
        );
    }
}

/// Les valeurs de DR réellement présentes sortent par la route des filtres,
/// pour que le client dessine ses pastilles sur des données mesurées.
///
/// L'issue ne fixe AUCUNE borne de tranche : MinimServer y est cité en modèle
/// mais ses bornes exactes n'ont jamais été relevées, et la couverture des
/// bibliothèques en tags DR n'a jamais été mesurée. Le serveur n'invente donc
/// pas de découpage — il dit ce qu'il a.
#[tokio::test]
async fn la_route_des_filtres_annonce_les_valeurs_de_dr_presentes_2144() {
    let app = bibliotheque();
    let (status, body) = get(&app, "/api/v1/library/albums/filters").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["dynamic_ranges"],
        serde_json::json!([0, 6, 9, 14]),
        "valeurs distinctes et croissantes ; « DR12.5 » est écarté et DR0 \
         conservé"
    );
    // La clé s'AJOUTE : les anciennes restent.
    assert!(body.get("formats").is_some());
    assert!(body.get("sample_rates").is_some());
}
