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

/// Le DR est une FACETTE du rail d'Oxygen, avec ses effectifs (#2144).
///
/// C'est la forme que le ticket réclame — « classer par tranches, façon
/// pastilles de genres » — et la seule qui réponde à la question que
/// `/library/albums/filters` laissait ouverte : *combien* de disques dans
/// chaque tranche. Sans effectif, une pastille peut ne rien rendre.
#[tokio::test]
async fn le_dynamic_range_est_une_facette_avec_ses_effectifs_2144() {
    let app = bibliotheque();
    let (status, body) = get(&app, "/api/v1/library/facets?fields=dr").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["dr"],
        serde_json::json!([
            { "value": "14", "count": 1 },
            { "value": "9",  "count": 1 },
            { "value": "6",  "count": 1 },
            { "value": "0",  "count": 1 },
        ]),
        "du plus dynamique au plus compressé ; « DR12.5 » écarté, DR0 gardé, \
         et l'album SANS tag ne fabrique pas de pastille"
    );
}

/// Plusieurs valeurs cochées = **une tranche**, en OU (#2168 appliqué au DR).
///
/// C'est ici que « filtrer par tranches » se joue : le serveur ne grave aucune
/// borne, l'utilisateur coche DR14, DR9 — et obtient la réunion. Une seule
/// valeur cochée reste un filtre exact.
#[tokio::test]
async fn plusieurs_dr_coches_forment_la_tranche_en_ou_2144() {
    let app = bibliotheque();

    for (requete, attendu) in [
        ("dr=14", vec!["Bravo"]),
        ("dr=14&dr=9", vec!["Bravo", "Charlie"]),
        ("dr=0", vec!["Foxtrot"]),
        // Une valeur qu'aucun album ne porte ne rend RIEN — surtout pas tout.
        ("dr=13", vec![]),
        // La tranche est toujours RESTRICTIVE : « DR12.5 » et l'album non
        // tagué n'y entrent par aucune valeur.
        ("dr=12", vec![]),
    ] {
        let (status, body) = get(
            &app,
            &format!("/api/v1/library/albums-detailed?limit=50&{requete}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{requete}");
        let mut vus = titres(&body);
        vus.sort();
        let mut veut: Vec<String> = attendu.iter().map(|s| s.to_string()).collect();
        veut.sort();
        assert_eq!(vus, veut, "cartes album pour {requete}");
        assert_eq!(
            total(&body),
            veut.len() as i64,
            "le total compte la sélection, pas la bibliothèque ({requete})"
        );
    }

    // La liste de PISTES filtre à l'identique — c'est le jumeau du rail, et
    // deux prédicats recopiés auraient fini par diverger.
    let (status, body) = get(&app, "/api/v1/library/tracks?limit=50&dr=14&dr=6").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(total(&body), 2, "Bravo et Alpha, une piste chacun");
}

/// Le TÉMOIN d'anti-régression : sans valeur de DR, rien ne change.
///
/// `?dr=` (case décochée, ce que le client envoie parfois) ne doit ni filtrer,
/// ni activer le chemin filtré — le piège n°1 de `facet_filter`, qui rendrait
/// la bibliothèque entière avec un total qui la confirme. Et une valeur non
/// numérique REFUSE la requête plutôt que de laisser tout passer.
#[tokio::test]
async fn une_facette_dr_vide_ou_invalide_ne_filtre_pas_a_moitie_2144() {
    let app = bibliotheque();

    let (_, plein) = get(&app, "/api/v1/library/albums-detailed?limit=50").await;
    let (status, vide) = get(&app, "/api/v1/library/albums-detailed?limit=50&dr=").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(total(&vide), 6, "les six albums, comme sans le paramètre");
    assert_eq!(total(&plein), total(&vide));

    let (status, _) = get(&app, "/api/v1/library/albums-detailed?limit=50&dr=abc").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "une valeur non numérique refuse la requête : ignorée, elle rendrait \
         un filtre annoncé qui laisse tout passer"
    );

    // Et les facettes SŒURS ne bougent pas d'un cheveu quand `dr` s'ajoute au
    // jeu demandé : la clé s'ajoute, aucune ne se remplace.
    let (_, avant) = get(&app, "/api/v1/library/facets?fields=format,year").await;
    let (_, apres) = get(&app, "/api/v1/library/facets?fields=format,year,dr").await;
    assert_eq!(avant["format"], apres["format"]);
    assert_eq!(avant["year"], apres["year"]);
    assert!(avant.get("dr").is_none(), "non demandée, non rendue");
}

/// Les effectifs des AUTRES facettes suivent la sélection de DR — c'est ce que
/// « cumulatif » veut dire, et ce qu'un rail qui ment sur ses effectifs ferait
/// perdre : une pastille annonçant 6 pour une liste de 1.
#[tokio::test]
async fn la_selection_de_dr_retrecit_les_effectifs_des_autres_facettes_2144() {
    let app = bibliotheque();

    let (_, large) = get(&app, "/api/v1/library/facets?fields=format").await;
    assert_eq!(
        large["format"],
        serde_json::json!([{ "value": "flac", "count": 6 }]),
        "sans sélection, les six pistes"
    );

    let (status, etroit) = get(&app, "/api/v1/library/facets?fields=format&dr=14&dr=9").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        etroit["format"],
        serde_json::json!([{ "value": "flac", "count": 2 }]),
        "Bravo et Charlie seulement"
    );

    // ⚠️ La facette DR, elle, ne se filtre PAS elle-même : ses alternatives
    // doivent rester visibles, sinon cocher DR14 effacerait DR9 de l'écran et
    // l'utilisateur ne pourrait plus élargir sa tranche.
    let (_, soi) = get(&app, "/api/v1/library/facets?fields=dr&dr=14").await;
    assert_eq!(
        soi["dr"].as_array().map(Vec::len),
        Some(4),
        "les quatre valeurs restent proposées malgré DR14 coché"
    );
}
