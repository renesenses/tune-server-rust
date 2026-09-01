//! #2560 — les favoris d'un profil ne s'atteignent pas par le `{id}` du chemin.
//!
//! ## Ce que ce fichier ajoute à `favoris_facettes_routes.rs`
//!
//! Le premier volet de la #2560 — « les trois routes `/favorites/facets`
//! n'existent pas » — est réglé : la #2503 les a montées, et
//! `favoris_facettes_routes.rs` interdit désormais qu'elles disparaissent en
//! silence. Reste le défaut que personne n'avait mesuré : ces routes portent un
//! **identifiant de profil dans le chemin**, et pas une seule ne vérifiait qu'il
//! s'agit de celui de l'appelant.
//!
//! `/api/v1/profiles/1/favorites/facets` rendait donc les labels favoris du
//! profil 1 à n'importe qui, et `/facets/add` les écrivait chez lui. Les
//! identifiants de profils sont de petits entiers séquentiels : c'est
//! exactement le défaut que la #3073 puis la #3076 ont fermé sur les playlists
//! (#2794), laissé nu sur la famille voisine.
//!
//! ## Comment la preuve est faite
//!
//! Deux profils réels dans la base. L'identité de l'appelant passe par
//! l'en-tête `X-Profile-Id`, que l'extracteur `ActiveProfile` honore — c'est
//! la convention du dépôt : *header = qui agit*, le chemin ne dit que *sur
//! quoi*. Un essai supplémentaire tourne **auth activée**, avec un vrai JWT :
//! là, l'en-tête ne suffit plus (`header_allowed` le lie au porteur du jeton),
//! et le refus devient une vraie frontière et non une politesse.
//!
//! Chaque refus est vérifié **EN BASE d'abord**, le code de retour seulement
//! ensuite : un « 404 pour rien » posé devant une base déjà modifiée ne
//! prouverait rien — c'est le « 200 pour rien » retourné.
//!
//! Chaque refus opposé au profil 2 est doublé du même appel par le profil 1,
//! qui doit réussir. Sans ce témoin, un handler qui répondrait 404 à tout le
//! monde passerait le test — et l'écran Favoris serait vide pour tous.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::favorite_facets_repo::FavoriteFacetsRepo;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::streaming_favorites_repo::StreamingFavoritesRepo;
use tune_server::state::AppState;

// --- outillage ---------------------------------------------------------

const P1: &str = "1";
const P2: &str = "2";
const SECRET: &str = "secret-de-test-2560";

fn etat() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // Le profil visé par `X-Profile-Id` doit exister, sinon l'extracteur
    // retombe sur le profil actif global et les deux « utilisateurs » de
    // l'essai seraient le même.
    let profils = ProfileRepo::with_backend(state.backend.clone());
    let id = profils
        .create("voisin", Some("Le voisin"), None)
        .expect("create profile");
    assert_eq!(id, 2, "le second profil doit porter l'id 2");
    state
}

fn appli(state: &AppState) -> axum::Router {
    tune_server::routes::router(state.clone())
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
    path: &str,
    profil: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(methode)
        .uri(path)
        .header("X-Profile-Id", profil);
    let body = match corps {
        Some(v) => {
            req = req.header("Content-Type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    reponse(app, req.body(body).unwrap()).await
}

/// Appel identifié par un JWT — auth activée.
async fn appel_jeton(app: &axum::Router, path: &str, jeton: &str) -> (StatusCode, Value) {
    let req = Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {jeton}"))
        .body(Body::empty())
        .unwrap();
    reponse(app, req).await
}

// --- lectures en base : la seule preuve qui compte ---------------------

fn labels_en_base(state: &AppState, profil: i64) -> Vec<String> {
    FavoriteFacetsRepo::with_backend(state.backend.clone())
        .list(profil, None)
        .expect("list facettes")
        .into_iter()
        .map(|f| f.value)
        .collect()
}

fn favoris_en_base(state: &AppState, profil: i64) -> usize {
    ProfileRepo::with_backend(state.backend.clone())
        .list_favorites(profil, None)
        .expect("list favoris")
        .len()
}

fn streaming_en_base(state: &AppState, profil: i64) -> usize {
    StreamingFavoritesRepo::with_backend(state.backend.clone())
        .list(profil, None)
        .expect("list streaming")
        .len()
}

/// Pose « ECM Records » chez le profil 1, par la route, comme le ferait le
/// cœur de l'onglet Labels.
async fn label_du_profil_1(state: &AppState, app: &axum::Router) {
    let (st, _) = appel(
        app,
        "POST",
        "/api/v1/profiles/1/favorites/facets/add",
        P1,
        Some(json!({"facet": "label", "value": "ECM Records"})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "le profil 1 doit pouvoir poser son propre label"
    );
    assert_eq!(
        labels_en_base(state, 1),
        vec!["ECM Records".to_string()],
        "préalable : le label doit être en base chez le profil 1"
    );
}

// --- LECTURE : le voisin ne lit pas les labels du profil 1 --------------

/// Le fait de base, pas le code de retour : le corps rendu au voisin ne doit
/// **pas contenir** « ECM Records ». Un 404 qui recracherait quand même la
/// valeur serait une fuite ; un 200 vide serait un refus déguisé.
#[tokio::test]
async fn le_voisin_ne_lit_pas_les_labels_favoris_du_profil_1() {
    let state = etat();
    let app = appli(&state);
    label_du_profil_1(&state, &app).await;

    let (statut, corps) = appel(&app, "GET", "/api/v1/profiles/1/favorites/facets", P2, None).await;
    assert!(
        !corps.to_string().contains("ECM Records"),
        "les labels favoris du profil 1 sont rendus au profil 2 : {corps}"
    );
    assert_eq!(
        statut,
        StatusCode::NOT_FOUND,
        "404, jamais 403 : distinguer « existe mais pas à vous » rendrait \
         l'énumération des profils utile"
    );

    // TÉMOIN — sans lui, un handler qui rendrait 404 à tout le monde passerait,
    // et l'écran Favoris serait vide pour son propre propriétaire.
    let (statut, corps) = appel(&app, "GET", "/api/v1/profiles/1/favorites/facets", P1, None).await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "le profil 1 doit lire ses propres labels"
    );
    assert!(
        corps.to_string().contains("ECM Records"),
        "le propriétaire ne retrouve plus son label : {corps}"
    );
}

// --- ÉCRITURE : le voisin n'écrit pas chez le profil 1 ------------------

/// Vérifié en base d'abord. Sans cloisonnement, « Butin » atterrit dans les
/// favoris du profil 1 et son écran Labels l'affiche au rechargement.
#[tokio::test]
async fn le_voisin_n_ecrit_pas_de_label_chez_le_profil_1() {
    let state = etat();
    let app = appli(&state);

    let (statut, _) = appel(
        &app,
        "POST",
        "/api/v1/profiles/1/favorites/facets/add",
        P2,
        Some(json!({"facet": "label", "value": "Butin"})),
    )
    .await;
    assert!(
        !labels_en_base(&state, 1).contains(&"Butin".to_string()),
        "le profil 2 a écrit un label chez le profil 1 : {:?}",
        labels_en_base(&state, 1)
    );
    assert_eq!(statut, StatusCode::NOT_FOUND);
    // Et le refus n'a pas non plus détourné l'écriture vers le voisin lui-même :
    // un refus n'est pas une redirection silencieuse.
    assert!(
        labels_en_base(&state, 2).is_empty(),
        "le refus a écrit chez l'appelant au lieu de ne rien faire : {:?}",
        labels_en_base(&state, 2)
    );

    // TÉMOIN : la même écriture par son propriétaire atteint bien la base.
    let (statut, _) = appel(
        &app,
        "POST",
        "/api/v1/profiles/1/favorites/facets/add",
        P1,
        Some(json!({"facet": "label", "value": "Butin"})),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED);
    assert!(labels_en_base(&state, 1).contains(&"Butin".to_string()));
}

/// L'effacement est la moitié qu'on oublie : lire est visible, effacer ne
/// laisse aucune trace chez la victime.
#[tokio::test]
async fn le_voisin_n_efface_pas_le_label_du_profil_1() {
    let state = etat();
    let app = appli(&state);
    label_du_profil_1(&state, &app).await;

    let (statut, _) = appel(
        &app,
        "POST",
        "/api/v1/profiles/1/favorites/facets/remove",
        P2,
        Some(json!({"facet": "label", "value": "ECM Records"})),
    )
    .await;
    assert_eq!(
        labels_en_base(&state, 1),
        vec!["ECM Records".to_string()],
        "le profil 2 a effacé le label favori du profil 1"
    );
    assert_eq!(statut, StatusCode::NOT_FOUND);

    // TÉMOIN : le propriétaire, lui, retire bien son label.
    let (statut, _) = appel(
        &app,
        "POST",
        "/api/v1/profiles/1/favorites/facets/remove",
        P1,
        Some(json!({"facet": "label", "value": "ECM Records"})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert!(labels_en_base(&state, 1).is_empty());
}

// --- La famille entière, pas seulement les trois de la #2560 -----------

/// #2560 nomme trois routes ; les sept voisines portent le même `{id}` de
/// chemin et le même défaut. Corriger les trois seules reproduirait le motif
/// « un chemin corrigé, les autres nus ».
#[tokio::test]
async fn aucune_route_de_la_famille_favoris_ne_sert_le_voisin() {
    let state = etat();
    let app = appli(&state);

    let surfaces: [(&str, &str, Option<Value>); 10] = [
        ("GET", "/api/v1/profiles/1/favorites", None),
        (
            "POST",
            "/api/v1/profiles/1/favorites/add",
            Some(json!({"item_type": "album", "item_id": 22})),
        ),
        (
            "POST",
            "/api/v1/profiles/1/favorites/remove",
            Some(json!({"item_type": "album", "item_id": 22})),
        ),
        (
            "POST",
            "/api/v1/profiles/1/favorites/check",
            Some(json!({"item_type": "album", "item_ids": [22]})),
        ),
        ("GET", "/api/v1/profiles/1/favorites/streaming", None),
        (
            "POST",
            "/api/v1/profiles/1/favorites/streaming/add",
            Some(json!({"item_type": "track", "service": "tidal", "service_id": "42"})),
        ),
        (
            "POST",
            "/api/v1/profiles/1/favorites/streaming/remove",
            Some(json!({"item_type": "track", "service": "tidal", "service_id": "42"})),
        ),
        ("GET", "/api/v1/profiles/1/favorites/facets", None),
        (
            "POST",
            "/api/v1/profiles/1/favorites/facets/add",
            Some(json!({"facet": "label", "value": "ECM Records"})),
        ),
        (
            "POST",
            "/api/v1/profiles/1/favorites/facets/remove",
            Some(json!({"facet": "label", "value": "ECM Records"})),
        ),
    ];

    for (methode, chemin, corps) in surfaces {
        let (statut, _) = appel(&app, methode, chemin, P2, corps.clone()).await;
        assert_eq!(
            statut,
            StatusCode::NOT_FOUND,
            "{methode} {chemin} sert le profil 2 sur le profil 1"
        );

        // TÉMOIN, la même surface pour son propriétaire.
        let (statut, _) = appel(&app, methode, chemin, P1, corps).await;
        assert_ne!(
            statut,
            StatusCode::NOT_FOUND,
            "{methode} {chemin} refuse aussi son propriétaire — le cloisonnement \
             a vidé l'écran Favoris"
        );
    }

    // Aucun des dix appels du profil 2 n'a laissé quoi que ce soit derrière lui.
    assert_eq!(
        favoris_en_base(&state, 2),
        0,
        "écriture parasite dans favorites"
    );
    assert_eq!(
        streaming_en_base(&state, 2),
        0,
        "écriture parasite dans streaming"
    );
    assert!(
        labels_en_base(&state, 2).is_empty(),
        "écriture parasite dans favorite_facets"
    );
}

// --- Auth activée : l'en-tête n'est plus un laissez-passer -------------

/// Avec `auth_enabled`, `header_allowed` refuse à un porteur de jeton
/// d'emprunter l'en-tête d'autrui — mais le `{id}` du CHEMIN, lui, n'était lié
/// à rien. C'est là que le défaut cesse d'être une convention et devient une
/// fuite : jeton valide du profil 2, chemin du profil 1.
#[tokio::test]
async fn un_jeton_du_profil_2_ne_lit_pas_les_labels_du_profil_1() {
    let state = etat();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    let app = appli(&state);

    // Le label est posé avant d'allumer l'authentification.
    label_du_profil_1(&state, &app).await;
    reglages.set("auth_enabled", "true").unwrap();
    reglages.set("jwt_secret", SECRET).unwrap();

    let voisin = tune_server::auth::sign_jwt(2, "user", SECRET).unwrap();
    let (statut, corps) = appel_jeton(&app, "/api/v1/profiles/1/favorites/facets", &voisin).await;
    assert!(
        !corps.to_string().contains("ECM Records"),
        "un jeton du profil 2 lit les labels du profil 1 : {corps}"
    );
    assert_eq!(statut, StatusCode::NOT_FOUND);

    // TÉMOIN : le jeton du profil 1 lit bien ses propres labels.
    let proprietaire = tune_server::auth::sign_jwt(1, "user", SECRET).unwrap();
    let (statut, corps) =
        appel_jeton(&app, "/api/v1/profiles/1/favorites/facets", &proprietaire).await;
    assert_eq!(statut, StatusCode::OK);
    assert!(
        corps.to_string().contains("ECM Records"),
        "le porteur du jeton du profil 1 ne retrouve plus son label : {corps}"
    );
}

// --- TÉMOIN GLOBAL : la forme réelle du client web reste servie --------

/// `tune-web-client` construit le chemin avec le même identifiant que celui
/// qu'il met dans `X-Profile-Id` (`profileHeader.ts` et le magasin de profil
/// lisent tous deux `localStorage['tune-profile-id']`). Ce test rejoue cette
/// forme de bout en bout — il doit être VERT avant comme après le correctif :
/// c'est lui qui prouve que le cloisonnement ne casse pas l'écran Favoris.
#[tokio::test]
async fn la_forme_reelle_du_client_web_traverse_toujours() {
    let state = etat();
    let app = appli(&state);

    // Le voisin, chez lui, mène la vie complète d'un favori de label.
    let (st, _) = appel(
        &app,
        "POST",
        "/api/v1/profiles/2/favorites/facets/add",
        P2,
        Some(json!({"facet": "label", "value": "Blue Note / EMI"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // L'appelant nu que FabienM a lu dans Firefox : sans `?facet=`.
    let (st, corps) = appel(&app, "GET", "/api/v1/profiles/2/favorites/facets", P2, None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        corps.to_string().contains("Blue Note / EMI"),
        "la facette relue ne contient pas l'entrée ajoutée : {corps}"
    );

    // Et la forme filtrée, celle que couvre le test du client web.
    let (st, corps) = appel(
        &app,
        "GET",
        "/api/v1/profiles/2/favorites/facets?facet=label",
        P2,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(corps.to_string().contains("Blue Note / EMI"));

    let (st, _) = appel(
        &app,
        "POST",
        "/api/v1/profiles/2/favorites/facets/remove",
        P2,
        Some(json!({"facet": "label", "value": "Blue Note / EMI"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(labels_en_base(&state, 2).is_empty());
}
