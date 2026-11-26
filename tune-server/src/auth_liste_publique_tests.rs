//! La liste publique de la couche d'authentification, confrontée au VRAI
//! routeur (`crate::routes::router`), authentification active, sans jeton.
//!
//! Trois épreuves :
//!
//! * les routes voisines d'une route publique (même préfixe, un segment de
//!   plus) demandent un jeton — `/system/health/monitor` et
//!   `/system/health/alerts` en tête ;
//! * chaque route publique voulue passe la couche sans jeton ;
//! * un tableau parcourt TOUTES les routes du routeur sous `/api/v1` et
//!   rend celles qui passent sans jeton. Il est comparé à une liste écrite
//!   ici : toute route qui devient publique demain — ajout à la liste, ou
//!   motif qui s'élargit — rougit ce test, et doit y être écrite à la main.

use std::collections::BTreeSet;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use super::est_route_publique;
use crate::state::AppState;

/// Le corps du refus de la couche quand aucun jeton n'est présenté.
const REFUS_DE_LA_COUCHE: &str = "authentication required";

/// Les méthodes que le tableau essaie sur chaque route.
const METHODES: [Method; 6] = [
    Method::GET,
    Method::HEAD,
    Method::POST,
    Method::PUT,
    Method::PATCH,
    Method::DELETE,
];

/// Le tableau attendu : `MÉTHODE chemin` de chaque route du routeur qui passe
/// la couche sans jeton. Ouvrir une route, c'est l'ajouter ici ET dans
/// `LISTE_PUBLIQUE` (auth.rs), en connaissance de cause.
const TABLEAU_ATTENDU: &[&str] = &[
    "GET /api/v1/auth/activation",
    "GET /api/v1/auth/config",
    "GET /api/v1/cloud/sso/callback",
    "GET /api/v1/sources-reseau/{id}/fichier",
    "GET /api/v1/system/health",
    "GET /api/v1/system/peer-info",
    "GET /api/v1/system/version",
    "HEAD /api/v1/sources-reseau/{id}/fichier",
    "HEAD /api/v1/system/health",
    "HEAD /api/v1/system/version",
    "POST /api/v1/auth/admin/reinitialiser",
    "POST /api/v1/auth/login",
    "POST /api/v1/auth/logout",
    "POST /api/v1/auth/refresh",
];

fn etat_sous_authentification() -> (tempfile::TempDir, AppState) {
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let state = AppState::new(
        base.to_str().unwrap(),
        0,
        crate::config::TuneConfig::default(),
    )
    .unwrap();
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    reglages.set("auth_enabled", "true").unwrap();
    reglages.set("jwt_secret", "secret-de-l-epreuve").unwrap();
    (dossier, state)
}

/// Un appel sans jeton : statut et corps.
async fn appel(state: &AppState, methode: Method, uri: &str) -> (StatusCode, String) {
    let reponse = crate::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

/// Le refus de la couche : 401 et son corps. Une réponse à HEAD n'a pas de
/// corps — le 401 seul fait foi.
fn refuse_par_la_couche(statut: StatusCode, corps: &str) -> bool {
    statut == StatusCode::UNAUTHORIZED && (corps == REFUS_DE_LA_COUCHE || corps.is_empty())
}

/// Toutes les routes déclarées sous `/api/v1`, motifs compris
/// (`/api/v1/zones/{id}`…), lues dans le routeur lui-même.
///
/// axum n'expose pas la liste de ses routes, mais son `Debug` imprime la
/// table des chemins (`Node { paths: {RouteId(n): "/api/v1/…", … } }`),
/// imbrications aplaties. C'est cette table qu'on lit. Si le format change,
/// les gardes de `le_routeur_est_bien_lu` rougissent.
fn routes_du_routeur(state: &AppState) -> BTreeSet<String> {
    let texte = format!("{:?}", crate::routes::router(state.clone()));
    texte
        .split('"')
        .filter(|morceau| morceau.starts_with("/api/v1/"))
        .map(str::to_string)
        .collect()
}

/// Un chemin concret pour un motif : chaque `{param}` ou `{*reste}` devient
/// un segment valide pour les gardes de forme (identifiant de source réseau
/// compris : lettres, chiffres, `-` et `_`).
fn chemin_concret(motif: &str) -> String {
    motif
        .split('/')
        .map(|segment| {
            if segment.starts_with('{') && segment.ends_with('}') {
                "essai-1"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[test]
fn les_routes_voisines_ne_sont_pas_publiques() {
    for chemin in [
        "/system/health/monitor",
        "/system/health/alerts",
        "/system/health/",
        "/system/healthz",
        "/system/version/x",
        "/system/peer-info/x",
        "/auth/login/x",
        "/streaming/qobuz/auth/login",
        "/ext/auth/login",
        "/x/system/health",
        "/api/v1/api/v1/system/health",
    ] {
        for vu in [chemin.to_string(), format!("/api/v1{chemin}")] {
            for methode in METHODES {
                assert!(
                    !est_route_publique(&methode, &vu),
                    "{methode} {vu} ne doit pas être publique"
                );
            }
        }
    }
    // La méthode compte : l'écriture de la configuration reste protégée.
    assert!(!est_route_publique(&Method::POST, "/api/v1/auth/config"));
    assert!(!est_route_publique(
        &Method::POST,
        "/api/v1/auth/activation"
    ));
    assert!(!est_route_publique(
        &Method::POST,
        "/api/v1/system/peer-info"
    ));
    assert!(!est_route_publique(
        &Method::DELETE,
        "/api/v1/system/health"
    ));
}

#[tokio::test]
async fn sous_routes_de_health_refusees_sans_jeton() {
    let (_dossier, state) = etat_sous_authentification();
    for uri in [
        "/api/v1/system/health/monitor",
        "/api/v1/system/health/alerts",
    ] {
        let (statut, corps) = appel(&state, Method::GET, uri).await;
        assert!(
            refuse_par_la_couche(statut, &corps),
            "GET {uri} sans jeton, authentification active : {statut} {corps:?} \
             (attendu 401 de la couche)"
        );
    }
}

#[tokio::test]
async fn les_routes_publiques_voulues_repondent_sans_jeton() {
    let (_dossier, state) = etat_sous_authentification();

    // Lectures : 200 franc.
    for uri in [
        "/api/v1/system/health",
        "/api/v1/system/version",
        "/api/v1/system/peer-info",
        "/api/v1/auth/config",
        "/api/v1/auth/activation",
    ] {
        let (statut, corps) = appel(&state, Method::GET, uri).await;
        assert_eq!(statut, StatusCode::OK, "GET {uri} sans jeton : {corps:?}");
    }
    for uri in ["/api/v1/system/health", "/api/v1/system/version"] {
        let (statut, _) = appel(&state, Method::HEAD, uri).await;
        assert_eq!(statut, StatusCode::OK, "HEAD {uri} sans jeton");
    }

    // Poignées de main qui attendent un corps ou des paramètres : le
    // gestionnaire répond (400, 401 de mauvais identifiants, redirection…),
    // mais jamais le refus de la couche.
    for (methode, uri) in [
        (Method::POST, "/api/v1/auth/login"),
        (Method::POST, "/api/v1/auth/refresh"),
        (Method::POST, "/api/v1/auth/logout"),
        (Method::GET, "/api/v1/cloud/sso/callback"),
        (Method::GET, "/api/v1/sources-reseau/essai-1/fichier"),
    ] {
        let (statut, corps) = appel(&state, methode.clone(), uri).await;
        assert!(
            !refuse_par_la_couche(statut, &corps) && statut != StatusCode::NOT_FOUND,
            "{methode} {uri} sans jeton : {statut} {corps:?} — la couche ne doit pas la refuser"
        );
    }

    // Contre-épreuve : la couche mord bien.
    let (statut, corps) = appel(&state, Method::GET, "/api/v1/system/stats").await;
    assert!(refuse_par_la_couche(statut, &corps), "{statut} {corps:?}");
}

#[tokio::test]
async fn le_routeur_est_bien_lu() {
    let (_dossier, state) = etat_sous_authentification();
    let routes = routes_du_routeur(&state);
    assert!(
        routes.len() > 300,
        "seulement {} routes lues sous /api/v1 : le format du Debug d'axum a changé ?",
        routes.len()
    );
    for attendue in [
        "/api/v1/system/health",
        "/api/v1/system/health/monitor",
        "/api/v1/system/health/alerts",
        "/api/v1/system/stats",
        "/api/v1/auth/login",
        "/api/v1/sources-reseau/{id}/fichier",
    ] {
        assert!(routes.contains(attendue), "{attendue} absente du relevé");
    }
}

/// Le tableau : chaque (méthode, route) du routeur qui passe la couche sans
/// jeton. Une route publique doit aussi exister (ni 404 ni 405) : une entrée
/// morte de la liste rougit ici.
#[tokio::test]
async fn tableau_des_routes_publiques_du_routeur() {
    let (_dossier, state) = etat_sous_authentification();
    let mut tableau = BTreeSet::new();
    let mut refusees = BTreeSet::new();
    for motif in routes_du_routeur(&state) {
        let concret = chemin_concret(&motif);
        for methode in METHODES {
            let court = concret.strip_prefix("/api/v1").unwrap();
            if !(est_route_publique(&methode, &concret) || est_route_publique(&methode, court)) {
                continue;
            }
            // La décision pure dit « publique » : le vrai routeur confirme
            // que la couche laisse passer ET que la méthode existe.
            let (statut, corps) = appel(&state, methode.clone(), &concret).await;
            if statut == StatusCode::METHOD_NOT_ALLOWED {
                continue;
            }
            if refuse_par_la_couche(statut, &corps) {
                // Entrée morte : la liste l'ouvre, la requête est refusée
                // quand même (route hors de la couche, ou autre graphie).
                refusees.insert(format!("{methode} {motif}"));
                continue;
            }
            tableau.insert(format!("{methode} {motif}"));
        }
    }
    let attendu: BTreeSet<String> = TABLEAU_ATTENDU.iter().map(|s| s.to_string()).collect();
    let en_trop: Vec<_> = tableau.difference(&attendu).collect();
    let manquantes: Vec<_> = attendu.difference(&tableau).collect();
    assert!(
        en_trop.is_empty() && manquantes.is_empty() && refusees.is_empty(),
        "routes publiques du routeur ({}) :\n{}\n\nen trop (publiques sans être écrites \
         dans TABLEAU_ATTENDU) : {en_trop:#?}\nmanquantes : {manquantes:#?}\n\
         publiques selon la liste mais refusées : {refusees:#?}",
        tableau.len(),
        tableau.iter().cloned().collect::<Vec<_>>().join("\n"),
    );
}
