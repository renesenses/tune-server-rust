//! Les sondes de vie des images Docker doivent viser une route que la couche
//! d'authentification laisse passer sans jeton.
//!
//! Le `HEALTHCHECK` de `Dockerfile` et `Dockerfile.dist` appelait
//! `/api/v1/system/stats`, absente de la liste publique : dès que
//! l'utilisateur activait l'authentification, la sonde prenait un 401 et
//! Docker marquait « unhealthy » un conteneur qui servait parfaitement.
//!
//! L'épreuve lit les Dockerfile eux-mêmes : une sonde réécrite demain vers une
//! autre route protégée rougit ici, sans attendre qu'un utilisateur le voie.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use super::est_route_publique;

const DOCKERFILES: [&str; 2] = ["Dockerfile", "Dockerfile.dist"];

/// Le chemin (avec `/api/v1`) que vise le `HEALTHCHECK` d'un Dockerfile du dépôt.
fn chemin_du_healthcheck(nom: &str) -> String {
    let fichier = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(nom);
    let texte = std::fs::read_to_string(&fichier)
        .unwrap_or_else(|e| panic!("lecture de {}: {e}", fichier.display()));
    // L'instruction tient sur plusieurs lignes jointes par `\`.
    let joint = texte.replace("\\\n", " ");
    let ligne = joint
        .lines()
        .find(|l| l.trim_start().starts_with("HEALTHCHECK"))
        .unwrap_or_else(|| panic!("{nom} : aucun HEALTHCHECK"));
    let debut = ligne
        .find("http://")
        .unwrap_or_else(|| panic!("{nom} : HEALTHCHECK sans URL : {ligne}"));
    let url = ligne[debut..]
        .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .next()
        .unwrap();
    let apres_schema = &url["http://".len()..];
    let chemin = &apres_schema[apres_schema
        .find('/')
        .unwrap_or_else(|| panic!("{nom} : URL sans chemin : {url}"))..];
    chemin.split('?').next().unwrap().to_string()
}

#[test]
fn le_healthcheck_des_dockerfile_vise_une_route_publique() {
    for nom in DOCKERFILES {
        let chemin = chemin_du_healthcheck(nom);
        assert!(
            chemin.starts_with("/api/v1/"),
            "{nom} : la sonde vise {chemin}, hors de /api/v1"
        );
        // Les deux graphies : selon l'imbrication d'axum, le middleware voit
        // le chemin avec ou sans le préfixe.
        let court = chemin.strip_prefix("/api/v1").unwrap();
        for vu in [chemin.as_str(), court] {
            assert!(
                est_route_publique(&Method::GET, vu),
                "{nom} : le HEALTHCHECK appelle {chemin}, qui demande un jeton \
                 quand l'authentification est active — Docker déclarerait le \
                 conteneur « unhealthy »"
            );
        }
    }
}

#[test]
fn system_stats_reste_protegee() {
    // La contre-épreuve : la liste ne s'est pas ouverte pour faire verdir.
    assert!(!est_route_publique(&Method::GET, "/api/v1/system/stats"));
    assert!(!est_route_publique(&Method::GET, "/system/stats"));
}

#[tokio::test]
async fn le_healthcheck_repond_200_sans_jeton_sous_authentification() {
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let state = crate::state::AppState::new(
        base.to_str().unwrap(),
        0,
        crate::config::TuneConfig::default(),
    )
    .unwrap();
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    reglages.set("auth_enabled", "true").unwrap();
    reglages.set("jwt_secret", "secret-de-l-epreuve").unwrap();

    let appel = |uri: String| {
        let app = crate::routes::router(state.clone());
        async move {
            app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }
    };

    // Contre-épreuve : l'authentification mord bien sur une route protégée.
    assert_eq!(
        appel("/api/v1/system/stats".into()).await,
        StatusCode::UNAUTHORIZED
    );

    for nom in DOCKERFILES {
        let chemin = chemin_du_healthcheck(nom);
        assert_eq!(
            appel(chemin.clone()).await,
            StatusCode::OK,
            "{nom} : GET {chemin} sans jeton, authentification active"
        );
    }
}
