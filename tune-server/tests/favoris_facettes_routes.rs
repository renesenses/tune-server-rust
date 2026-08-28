//! Les routes de favoris répondent — vérifié SUR LE SERVEUR, pas sur l'URL.
//!
//! #2560 : trois testeurs (Dimitri, FabienM, Jean Valjean) ont versé le même
//! `WARN tune_server::routes: api_not_found
//! path=/api/v1/profiles/1/favorites/facets`, en 0.9.115 puis en 0.9.116. Les
//! trois routes de facette sont arrivées depuis, par la #2503 (`53bca52d`,
//! 26/08, publiée en 0.9.118) : le 404 des journaux est antérieur au correctif.
//!
//! Reste le défaut que ce ticket nomme et qui, lui, n'était PAS corrigé :
//! **rien côté serveur ne prouve qu'une de ces routes est montée**. Les tests
//! du client web (`favorisPlaylistLabel.test.ts`) vérifient que le client
//! FABRIQUE l'URL ; ils restent verts quand personne ne répond au bout. Et
//! avant ce fichier, la famille `/profiles/{id}/favorites*` — les sept routes
//! historiques comme les trois neuves — n'avait aucune couverture de route :
//! supprimer un `.route(...)` de `profiles::router()` ne faisait rougir aucun
//! test de ce dépôt.
//!
//! Ce fichier tient donc la jonction, du côté où elle peut casser sans bruit.
//! Il ne touche ni au schéma ni à une migration : il n'observe que des routes
//! déjà livrées.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
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

async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
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

// Tous les chemins visent le **profil 1** — celui des trois journaux de
// testeurs. `favorites` et `favorite_facets` ne portent aucune clé étrangère
// vers `profiles` : les routes répondent sans qu'on ait à créer un profil, et
// le plafond freemium (un seul profil en gratuit) ne trouble pas le test.

// --- Le défaut du ticket : la route est-elle montée ? ---

/// La trace exacte des trois testeurs. `api_not_found` sur ce chemin signifie
/// 404 ; c'est le seul code que ce test refuse.
#[tokio::test]
async fn la_route_de_lecture_des_facettes_repond_sans_parametre_de_requete() {
    let app = app();
    let (status, body) = get(&app, "/api/v1/profiles/1/favorites/facets").await;

    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "GET /api/v1/profiles/1/favorites/facets rend 404 — c'est le défaut #2560 revenu"
    );
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.is_array(),
        "la route doit rendre un tableau JSON, elle rend {body}"
    );
}

/// FabienM (fil 1581) a lu l'URL en entier dans Firefox : elle part **nue**,
/// sans `?facet=…`. Le test du client web n'attend que la forme
/// `?facet=label` — l'appelant que les testeurs déclenchent n'est donc couvert
/// nulle part. Les deux formes doivent répondre.
#[tokio::test]
async fn les_deux_appelants_de_la_lecture_sont_servis() {
    let app = app();

    let (nu, corps_nu) = get(&app, "/api/v1/profiles/1/favorites/facets").await;
    let (filtre, corps_filtre) = get(&app, "/api/v1/profiles/1/favorites/facets?facet=label").await;

    assert_eq!(
        nu,
        StatusCode::OK,
        "appel sans paramètre (celui des testeurs)"
    );
    assert_eq!(
        filtre,
        StatusCode::OK,
        "appel avec ?facet=label (celui du test client)"
    );
    assert!(corps_nu.is_array() && corps_filtre.is_array());
}

/// Les deux routes d'écriture. Un corps valide doit produire un code
/// d'écriture — surtout pas un 404, et surtout pas un 405 (route montée sur le
/// mauvais verbe).
#[tokio::test]
async fn les_deux_routes_d_ecriture_des_facettes_sont_montees() {
    let app = app();

    let (ajout, _) = post(
        &app,
        "/api/v1/profiles/1/favorites/facets/add",
        json!({"facet": "label", "value": "ECM Records"}),
    )
    .await;
    assert_eq!(
        ajout,
        StatusCode::CREATED,
        "POST /favorites/facets/add ne répond pas — 404 = route absente, 405 = mauvais verbe"
    );

    let (retrait, _) = post(
        &app,
        "/api/v1/profiles/1/favorites/facets/remove",
        json!({"facet": "label", "value": "ECM Records"}),
    )
    .await;
    assert_eq!(
        retrait,
        StatusCode::OK,
        "POST /favorites/facets/remove ne répond pas"
    );
}

/// Le cœur qu'on pose doit se retrouver allumé au rechargement de l'écran,
/// puis s'éteindre. Traversée complète par HTTP, pas par le dépôt.
#[tokio::test]
async fn un_label_pose_par_la_route_se_relit_puis_s_efface() {
    let app = app();
    let chemin = "/api/v1/profiles/1/favorites/facets";

    post(
        &app,
        &format!("{chemin}/add"),
        json!({"facet": "label", "value": "ECM Records"}),
    )
    .await;

    let (_, apres_ajout) = get(&app, chemin).await;
    let valeurs: Vec<&str> = apres_ajout
        .as_array()
        .expect("tableau")
        .iter()
        .filter_map(|v| v["value"].as_str())
        .collect();
    assert_eq!(valeurs, vec!["ECM Records"], "le cœur posé doit se relire");

    post(
        &app,
        &format!("{chemin}/remove"),
        json!({"facet": "label", "value": "ECM Records"}),
    )
    .await;

    let (_, apres_retrait) = get(&app, chemin).await;
    assert!(
        apres_retrait.as_array().expect("tableau").is_empty(),
        "le cœur retiré doit disparaître, il reste {apres_retrait}"
    );
}

/// Une valeur vide est une demande malformée, pas une panne : 400, pas 500.
/// Un 500 enverrait chercher la cause en base.
#[tokio::test]
async fn une_valeur_de_facette_vide_sort_en_400() {
    let app = app();
    let (status, _) = post(
        &app,
        "/api/v1/profiles/1/favorites/facets/add",
        json!({"facet": "label", "value": "   "}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// --- La garantie de non-régression sur l'existant ---

/// **Aucun favori existant n'est perdu.**
///
/// La table des favoris est polymorphe depuis la #2503, et les cinq types ne
/// se rangent PAS au même endroit — c'est délibéré : piste, album, artiste et
/// playlist portent un identifiant entier et vivent dans `favorites` ; le
/// label n'a aucune identité (c'est une chaîne, il n'existe ni table `labels`
/// ni identifiant) et vit dans `favorite_facets`, réconcilié par sa valeur.
///
/// Ce test verrouille la frontière dans les deux sens : poser un favori de
/// label ne doit rien retirer aux quatre types à identité, et la liste des
/// favoris à identité ne doit pas se mettre à faire disparaître le label.
#[tokio::test]
async fn poser_un_label_ne_perd_aucun_favori_a_identite() {
    let app = app();

    // Les quatre types à identité, tels qu'ils sont déjà en base chez les
    // utilisateurs.
    for (item_type, item_id) in [
        ("track", 11),
        ("album", 22),
        ("artist", 33),
        ("playlist", 44),
    ] {
        let (status, _) = post(
            &app,
            "/api/v1/profiles/1/favorites/add",
            json!({"item_type": item_type, "item_id": item_id}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "favori {item_type} refusé à l'écriture"
        );
    }

    let (_, avant) = get(&app, "/api/v1/profiles/1/favorites").await;
    let compte_avant = avant.as_array().expect("tableau").len();
    assert_eq!(
        compte_avant, 4,
        "les quatre favoris à identité doivent être en base"
    );

    // Le cinquième type entre par l'autre porte.
    post(
        &app,
        "/api/v1/profiles/1/favorites/facets/add",
        json!({"facet": "label", "value": "ECM Records"}),
    )
    .await;

    // Rien n'a bougé du côté des quatre.
    let (_, apres) = get(&app, "/api/v1/profiles/1/favorites").await;
    assert_eq!(
        apres.as_array().expect("tableau").len(),
        compte_avant,
        "un favori de label a modifié la liste des favoris à identité : {apres}"
    );
    assert_eq!(
        avant, apres,
        "les favoris à identité doivent être rendus à l'identique"
    );

    // Et le label est bien là, de son côté.
    let (_, facettes) = get(&app, "/api/v1/profiles/1/favorites/facets").await;
    assert_eq!(facettes.as_array().expect("tableau").len(), 1);

    // Retirer le label ne touche pas davantage aux quatre.
    post(
        &app,
        "/api/v1/profiles/1/favorites/facets/remove",
        json!({"facet": "label", "value": "ECM Records"}),
    )
    .await;
    let (_, apres_retrait) = get(&app, "/api/v1/profiles/1/favorites").await;
    assert_eq!(
        apres_retrait, avant,
        "retirer un favori de label a modifié les favoris à identité"
    );
}

/// La famille entière, d'un coup. #2560 relève que trois routes manquaient
/// sans que rien ne le signale ; les sept voisines étaient tout aussi
/// découvertes. Une route retirée de `profiles::router()` doit désormais
/// faire rougir ce test plutôt que le journal d'un testeur.
#[tokio::test]
async fn aucune_route_de_la_famille_favoris_ne_disparait_en_silence() {
    let app = app();

    let lectures = [
        "/api/v1/profiles/1/favorites",
        "/api/v1/profiles/1/favorites/streaming",
        "/api/v1/profiles/1/favorites/facets",
    ];
    for chemin in lectures {
        let (status, _) = get(&app, chemin).await;
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "GET {chemin} rend 404 : route absente"
        );
    }

    let ecritures: [(&str, Value); 7] = [
        (
            "/api/v1/profiles/1/favorites/add",
            json!({"item_type": "track", "item_id": 1}),
        ),
        (
            "/api/v1/profiles/1/favorites/remove",
            json!({"item_type": "track", "item_id": 1}),
        ),
        (
            "/api/v1/profiles/1/favorites/check",
            json!({"item_type": "track", "item_ids": [1]}),
        ),
        (
            "/api/v1/profiles/1/favorites/streaming/add",
            json!({"item_type": "album", "service": "qobuz", "service_id": "abc"}),
        ),
        (
            "/api/v1/profiles/1/favorites/streaming/remove",
            json!({"item_type": "album", "service": "qobuz", "service_id": "abc"}),
        ),
        (
            "/api/v1/profiles/1/favorites/facets/add",
            json!({"facet": "label", "value": "ECM Records"}),
        ),
        (
            "/api/v1/profiles/1/favorites/facets/remove",
            json!({"facet": "label", "value": "ECM Records"}),
        ),
    ];
    for (chemin, corps) in ecritures {
        let (status, _) = post(&app, chemin, corps).await;
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "POST {chemin} rend 404 : route absente"
        );
        assert_ne!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "POST {chemin} rend 405 : route montée sur le mauvais verbe"
        );
    }
}
