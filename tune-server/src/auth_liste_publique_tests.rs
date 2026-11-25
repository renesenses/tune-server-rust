//! La liste publique de la couche d'authentification, confrontée au VRAI
//! routeur (`crate::routes::router`), authentification active, sans jeton.
//!
//! Trois épreuves :
//!
//! * les routes voisines d'une route publique (même préfixe, un segment de
//!   plus) demandent un jeton — `/system/health/monitor` et
//!   `/system/health/alerts` en tête ;
//! * chaque route publique voulue passe la couche sans jeton ;
//! * un tableau appelle TOUTES les routes du routeur sous `/api/v1`, avec
//!   chaque méthode, sans jeton, et relève celles qui ne reçoivent pas 401.
//!   Il ne consulte pas la liste publique : il observe le routeur. Le
//!   résultat est comparé à une liste écrite ici — toute route qui devient
//!   publique demain, ou qui est montée sous `/api/v1` hors de la couche,
//!   rougit ce test et doit y être écrite à la main.

use std::collections::BTreeSet;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use super::est_route_publique;
use crate::state::AppState;

/// Le corps du refus de la couche quand aucun jeton n'est présenté.
const REFUS_DE_LA_COUCHE: &str = "authentication required";

/// Le corps du refus de la couche quand le jeton présenté ne se vérifie pas.
const JETON_REFUSE_PAR_LA_COUCHE: &str = "invalid token";

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
    "POST /api/v1/auth/login",
    "POST /api/v1/auth/logout",
];

/// Les routes montées sous `/api/v1` HORS de la couche d'authentification,
/// qui font leur propre contrôle. Chacune doit être écrite ici sciemment.
///
/// * `/api/v1/ws` : le WebSocket, monté à la racine du routeur (le client
///   web s'y connecte sous ce préfixe) ; `WsAuthorized` y vérifie le jeton.
const HORS_COUCHE_ATTENDU: &[&str] = &["/api/v1/ws"];

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

/// Les chemins sont écrits tels que la couche les voit : axum a déjà retiré
/// `/api/v1`. Un `/api/v1/…` vu par la couche vient d'un préfixe doublé.
#[test]
fn les_routes_voisines_ne_sont_pas_publiques() {
    for vu in [
        "/system/health/monitor",
        "/system/health/alerts",
        "/system/health/",
        "/system/healthz",
        "/system/version/x",
        "/system/peer-info/x",
        "/auth/login/x",
        "/streaming/qobuz/auth/login",
        "/streaming/qobuz/auth/logout",
        "/ext/auth/login",
        "/x/system/health",
        // Préfixe doublé : `/api/v1/api/v1/…` arrive ainsi à la couche.
        "/api/v1/system/health",
        "/api/v1/auth/login",
        "/api/v1/sources-reseau/essai-1/fichier",
    ] {
        for methode in METHODES {
            assert!(
                !est_route_publique(&methode, vu),
                "{methode} {vu} ne doit pas être publique"
            );
        }
    }
    // La méthode compte : l'écriture de la configuration reste protégée.
    assert!(!est_route_publique(&Method::POST, "/auth/config"));
    assert!(!est_route_publique(&Method::POST, "/auth/activation"));
    assert!(!est_route_publique(&Method::POST, "/system/peer-info"));
    assert!(!est_route_publique(&Method::DELETE, "/system/health"));
}

#[tokio::test]
async fn le_prefixe_double_ne_passe_pas_la_couche() {
    let (_dossier, state) = etat_sous_authentification();
    for (methode, uri) in [
        (Method::GET, "/api/v1/api/v1/system/health"),
        (Method::POST, "/api/v1/api/v1/auth/login"),
        (Method::GET, "/api/v1/api/v1/sources-reseau/essai-1/fichier"),
    ] {
        let (statut, corps) = appel(&state, methode.clone(), uri).await;
        assert!(
            refuse_par_la_couche(statut, &corps),
            "{methode} {uri} sans jeton : {statut} {corps:?} (attendu 401 de la couche)"
        );
    }
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

/// Un appel sur un routeur déjà construit, avec un en-tête `Authorization`
/// facultatif.
async fn appel_sur(
    app: &axum::Router,
    methode: Method,
    uri: &str,
    autorisation: Option<&str>,
) -> (StatusCode, String) {
    let mut requete = Request::builder().method(methode).uri(uri);
    if let Some(valeur) = autorisation {
        requete = requete.header("authorization", valeur);
    }
    let reponse = app
        .clone()
        .oneshot(requete.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

/// Le tableau, par observation du routeur seul (la liste publique n'est pas
/// consultée) :
///
/// 1. chaque route est d'abord sondée en PUT avec un jeton invalide. La
///    couche s'exécute avant l'aiguillage par méthode : sous la couche, la
///    réponse est son refus (`invalid token`), que la méthode existe ou non,
///    que la route soit publique ou non. Toute autre réponse : la route est
///    montée hors de la couche ;
/// 2. chaque route sous la couche est appelée avec chaque méthode, sans
///    jeton : tout ce qui ne reçoit pas le 401 de la couche l'a passée.
#[tokio::test]
async fn tableau_des_routes_publiques_du_routeur() {
    let (_dossier, state) = etat_sous_authentification();
    let app = crate::routes::router(state.clone());
    let mut publiques = BTreeSet::new();
    let mut hors_couche = BTreeSet::new();
    for motif in routes_du_routeur(&state) {
        let concret = chemin_concret(&motif);
        let (statut, corps) =
            appel_sur(&app, Method::PUT, &concret, Some("Bearer jeton-invalide")).await;
        if !(statut == StatusCode::UNAUTHORIZED && corps == JETON_REFUSE_PAR_LA_COUCHE) {
            hors_couche.insert(motif);
            continue;
        }
        for methode in METHODES {
            let (statut, corps) = appel_sur(&app, methode.clone(), &concret, None).await;
            // Le refus de la couche a son corps : un 401 rendu par le
            // gestionnaire (retour SSO hors de cette machine, par exemple)
            // a passé la couche.
            if !refuse_par_la_couche(statut, &corps) {
                publiques.insert(format!("{methode} {motif}"));
            }
        }
    }
    let ecart = |vu: &BTreeSet<String>, attendu: &[&str]| {
        let attendu: BTreeSet<String> = attendu.iter().map(|s| s.to_string()).collect();
        let en_trop: Vec<String> = vu.difference(&attendu).cloned().collect();
        let manquantes: Vec<String> = attendu.difference(vu).cloned().collect();
        (en_trop, manquantes)
    };
    let (publiques_en_trop, publiques_manquantes) = ecart(&publiques, TABLEAU_ATTENDU);
    let (hors_couche_en_trop, hors_couche_manquantes) = ecart(&hors_couche, HORS_COUCHE_ATTENDU);
    assert!(
        publiques_en_trop.is_empty()
            && publiques_manquantes.is_empty()
            && hors_couche_en_trop.is_empty()
            && hors_couche_manquantes.is_empty(),
        "sans jeton, hors 401 ({}) :\n{}\n\nhors de la couche ({}) :\n{}\n\n\
         publiques en trop (absentes de TABLEAU_ATTENDU) : {publiques_en_trop:#?}\n\
         publiques manquantes : {publiques_manquantes:#?}\n\
         hors couche en trop (absentes de HORS_COUCHE_ATTENDU) : {hors_couche_en_trop:#?}\n\
         hors couche manquantes : {hors_couche_manquantes:#?}",
        publiques.len(),
        publiques.iter().cloned().collect::<Vec<_>>().join("\n"),
        hors_couche.len(),
        hors_couche.iter().cloned().collect::<Vec<_>>().join("\n"),
    );
}
