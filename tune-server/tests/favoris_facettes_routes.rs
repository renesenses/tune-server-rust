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

    let ecritures: [(&str, Value); 9] = [
        // #2001 piste 2 — l'ordre manuel. Sans cette ligne, retirer un
        // `.route(...)` de `profiles::router()` ne ferait rougir que les tests
        // de comportement, jamais le contrat de la famille.
        (
            "/api/v1/profiles/1/favorites/reorder",
            json!({"item_type": "track", "item_ids": [1]}),
        ),
        (
            "/api/v1/profiles/1/favorites/streaming/reorder",
            json!({"item_type": "album", "items": [{"service": "qobuz", "service_id": "abc"}]}),
        ),
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

// --- #2001 : « aucun tri ni réordonnancement, l'ordre d'ajout est subi » ---
//
// Le client web sait trier depuis la v0.9.96, mais dans son propre code : le
// serveur, lui, rendait toujours l'ordre d'ajout, donc les clients Flutter,
// Swift, le widget et l'UPnP restaient sans recours. Ces tests vérifient la
// jonction là où elle peut casser sans bruit : que `sort`/`order` traversent
// bien l'extracteur `Query` jusqu'au dépôt, et que leur ABSENCE ne change rien.

/// Trois favoris de service dont les titres ne se rangent pas comme leur ordre
/// d'ajout, et dont les artistes départagent accents et casse.
async fn trois_favoris_de_service(app: &axum::Router) {
    let items: [(&str, &str, &str); 3] = [
        ("s1", "Volume 10", "Éric Zimmer"),
        ("s2", "volume 2", "aaron Zed"),
        ("s3", "Zorro", "Erik Satie"),
    ];
    for (id, titre, artiste) in items {
        let (status, _) = post(
            app,
            "/api/v1/profiles/1/favorites/streaming/add",
            json!({
                "item_type": "track",
                "service": "qobuz",
                "service_id": id,
                "title": titre,
                "artist": artiste,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "l'ajout de {id} a échoué");
    }
}

fn identifiants(corps: &Value) -> Vec<String> {
    corps
        .as_array()
        .expect("la route rend une liste")
        .iter()
        .map(|f| f["service_id"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn le_tri_demande_traverse_la_route_des_favoris_de_service() {
    let app = app();
    trois_favoris_de_service(&app).await;

    let (status, corps) = get(
        &app,
        "/api/v1/profiles/1/favorites/streaming?item_type=track&sort=title&order=asc",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // « volume 2 » avant « Volume 10 » : le tri est naturel, pas lexical.
    assert_eq!(identifiants(&corps), ["s2", "s1", "s3"]);

    let (_, corps) = get(
        &app,
        "/api/v1/profiles/1/favorites/streaming?item_type=track&sort=title&order=desc",
    )
    .await;
    assert_eq!(identifiants(&corps), ["s3", "s1", "s2"]);

    // « Éric Zimmer » se range entre « aaron Zed » et « Erik Satie » : les
    // accents suivent leur lettre, ils ne finissent pas la liste.
    let (_, corps) = get(
        &app,
        "/api/v1/profiles/1/favorites/streaming?item_type=track&sort=artist",
    )
    .await;
    assert_eq!(identifiants(&corps), ["s2", "s1", "s3"]);
}

/// Rétro-compatibilité : sans `sort`, la route rend la même chose qu'avant —
/// et un `sort` inconnu ne fait pas d'erreur, il ne trie simplement pas.
#[tokio::test]
async fn sans_parametre_de_tri_la_route_des_favoris_ne_change_pas() {
    let app = app();
    trois_favoris_de_service(&app).await;

    let (status, sans) = get(
        &app,
        "/api/v1/profiles/1/favorites/streaming?item_type=track",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sans = identifiants(&sans);
    assert_eq!(sans.len(), 3);

    let (status, inconnu) = get(
        &app,
        "/api/v1/profiles/1/favorites/streaming?item_type=track&sort=bpm&order=asc",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "une clé de tri inconnue ne doit pas casser une route de lecture"
    );
    assert_eq!(
        identifiants(&inconnu),
        sans,
        "une clé inconnue doit laisser l'ordre d'avant"
    );
}

/// La route des favoris LOCAUX accepte les mêmes paramètres et garde la forme
/// de sa réponse : `sort` n'ajoute aucun champ au JSON.
#[tokio::test]
async fn la_route_des_favoris_locaux_accepte_le_tri_sans_changer_sa_reponse() {
    let app = app();
    for item_id in [7, 8, 9] {
        let (status, _) = post(
            &app,
            "/api/v1/profiles/1/favorites/add",
            json!({"item_type": "track", "item_id": item_id}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (status, sans) = get(&app, "/api/v1/profiles/1/favorites?item_type=track").await;
    assert_eq!(status, StatusCode::OK);
    let (status, trie) = get(
        &app,
        "/api/v1/profiles/1/favorites?item_type=track&sort=title&order=asc",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let cles = |v: &Value| -> Vec<String> {
        let mut k: Vec<String> = v.as_array().unwrap()[0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };
    assert_eq!(
        cles(&sans),
        cles(&trie),
        "l'instantané d'identité sert au tri, il ne doit PAS fuir dans la réponse"
    );
    assert_eq!(trie.as_array().unwrap().len(), 3);
}

// --- #2001, piste 2 : l'ordre MANUEL ------------------------------------
//
// Le tri par champ (PR #2829) range d'après une donnée. Tades, lui, essayait de
// DÉPLACER un favori à la souris — un ordre qu'aucun champ ne produit. Ces
// tests vérifient la jonction complète : la route de réordonnancement existe,
// ce qu'elle écrit se relit par `sort=manual`, et l'absence du paramètre laisse
// tout comme avant.

fn identifiants_locaux(corps: &Value) -> Vec<i64> {
    corps
        .as_array()
        .expect("la route rend une liste")
        .iter()
        .map(|f| f["item_id"].as_i64().unwrap_or_default())
        .collect()
}

async fn trois_favoris_locaux(app: &axum::Router) {
    for item_id in [7, 8, 9] {
        let (status, _) = post(
            app,
            "/api/v1/profiles/1/favorites/add",
            json!({"item_type": "track", "item_id": item_id}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
}

#[tokio::test]
async fn l_ordre_manuel_pose_par_la_route_se_relit_par_la_route() {
    let app = app();
    trois_favoris_locaux(&app).await;

    let (status, corps) = post(
        &app,
        "/api/v1/profiles/1/favorites/reorder",
        json!({"item_type": "track", "item_ids": [8, 9, 7]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(corps["ordered"], json!(3), "les trois favoris sont rangés");

    let (status, corps) = get(
        &app,
        "/api/v1/profiles/1/favorites?item_type=track&sort=manual&order=asc",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(identifiants_locaux(&corps), vec![8, 9, 7]);

    let (_, corps) = get(
        &app,
        "/api/v1/profiles/1/favorites?item_type=track&sort=manual&order=desc",
    )
    .await;
    assert_eq!(identifiants_locaux(&corps), vec![7, 9, 8]);
}

/// Témoin anti-régression : un ordre manuel posé ne doit rien changer à ce que
/// la route rend **sans** `sort`. La colonne `position` n'est lue que par
/// `sort=manual`, et elle ne fuit pas dans la réponse.
#[tokio::test]
async fn l_ordre_manuel_ne_change_ni_la_liste_par_defaut_ni_sa_forme() {
    let app = app();
    trois_favoris_locaux(&app).await;

    let (_, avant) = get(&app, "/api/v1/profiles/1/favorites?item_type=track").await;
    post(
        &app,
        "/api/v1/profiles/1/favorites/reorder",
        json!({"item_type": "track", "item_ids": [8, 9, 7]}),
    )
    .await;
    let (status, apres) = get(&app, "/api/v1/profiles/1/favorites?item_type=track").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        identifiants_locaux(&avant),
        identifiants_locaux(&apres),
        "sans `sort`, l'ordre servi doit rester exactement celui d'avant"
    );

    let cles = |v: &Value| -> Vec<String> {
        let mut k: Vec<String> = v.as_array().unwrap()[0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };
    let (_, manuel) = get(
        &app,
        "/api/v1/profiles/1/favorites?item_type=track&sort=manual",
    )
    .await;
    assert_eq!(
        cles(&apres),
        cles(&manuel),
        "le rang manuel sert au tri, il ne doit PAS fuir dans la réponse"
    );
}

/// L'ordre manuel des favoris de service traverse sa propre route — la clé
/// n'est pas un `item_id` entier mais `(service, service_id)`.
#[tokio::test]
async fn l_ordre_manuel_traverse_la_route_des_favoris_de_service() {
    let app = app();
    trois_favoris_de_service(&app).await;

    let (status, corps) = post(
        &app,
        "/api/v1/profiles/1/favorites/streaming/reorder",
        json!({"item_type": "track", "items": [
            {"service": "qobuz", "service_id": "s3"},
            {"service": "qobuz", "service_id": "s1"},
            {"service": "qobuz", "service_id": "s2"}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(corps["ordered"], json!(3));

    let (status, corps) = get(
        &app,
        "/api/v1/profiles/1/favorites/streaming?item_type=track&sort=manual",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(identifiants(&corps), ["s3", "s1", "s2"]);
}

/// Le rang est par ONGLET : ranger les albums ne défait pas l'ordre des pistes.
/// C'est le motif « un chemin corrigé, les autres nus » appliqué aux onglets
/// des favoris — vérifié à travers la route, pas seulement dans le dépôt.
#[tokio::test]
async fn reordonner_un_onglet_ne_defait_pas_l_autre_a_travers_la_route() {
    let app = app();
    trois_favoris_locaux(&app).await;
    for item_id in [21, 22] {
        post(
            &app,
            "/api/v1/profiles/1/favorites/add",
            json!({"item_type": "album", "item_id": item_id}),
        )
        .await;
    }

    post(
        &app,
        "/api/v1/profiles/1/favorites/reorder",
        json!({"item_type": "track", "item_ids": [8, 9, 7]}),
    )
    .await;
    post(
        &app,
        "/api/v1/profiles/1/favorites/reorder",
        json!({"item_type": "album", "item_ids": [22, 21]}),
    )
    .await;

    let (_, pistes) = get(
        &app,
        "/api/v1/profiles/1/favorites?item_type=track&sort=manual",
    )
    .await;
    assert_eq!(identifiants_locaux(&pistes), vec![8, 9, 7]);
    let (_, albums) = get(
        &app,
        "/api/v1/profiles/1/favorites?item_type=album&sort=manual",
    )
    .await;
    assert_eq!(identifiants_locaux(&albums), vec![22, 21]);
}
