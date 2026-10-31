//! `GET /api/v1/streaming/qobuz/debug/raw-keys` — sonde de diagnostic (#5530).
//!
//! Établit si l'API Qobuz expose le marquage « contenu généré par IA »
//! annoncé par Qobuz le 24/09/2026, et sous quel nom, SANS faire sortir la
//! réponse brute : Tune appelle `album/get` (et `track/get` si `track_id` est
//! fourni) avec sa propre session, puis ne rend que l'arbre des clés — voir
//! `tune_core::streaming::qobuz_cles_brutes` pour ce qui sort et ce qui reste.
//!
//! Garde : [`RequireAdmin`], comme les autres routes privilégiées. Avec
//! l'authentification désactivée le serveur est ouvert, comme partout ailleurs
//! (voir la doc de `RequireAdmin`).

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::streaming::StreamingService;
use tune_core::streaming::qobuz::{QobuzService, RessourceBrute};
use tune_core::streaming::qobuz_cles_brutes::{
    FRAGMENTS_NOTABLES, FRAGMENTS_SENSIBLES, arbre_des_cles, arbre_des_types, chemins_notables,
};

use crate::auth::RequireAdmin;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ParametresClesBrutes {
    pub album_id: Option<String>,
    pub track_id: Option<String>,
    /// Fiche de label (`label/get`, #7414).
    pub label_id: Option<String>,
    /// `albums` : ajoute `extra=albums` à l'appel `label/get`.
    pub extra: Option<String>,
}

/// Un identifiant Qobuz plausible : alphanumérique, borné. Refuser le reste
/// avant tout appel évite que la route serve à injecter des paramètres.
fn identifiant_valide(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

fn erreur(statut: StatusCode, message: &str) -> Response {
    (statut, Json(json!({ "error": message }))).into_response()
}

fn rendre(resultat: Result<Value, Option<u16>>) -> Value {
    match resultat {
        Ok(document) => json!({
            "statut": "ok",
            "cles_notables": chemins_notables(&document),
            "arbre": arbre_des_cles(&document),
        }),
        Err(http) => json!({ "statut": "erreur", "http": http }),
    }
}

/// Comme [`rendre`], mais types seulement : aucune valeur, pas même un
/// booléen (#7414).
fn rendre_types_seuls(resultat: Result<Value, Option<u16>>) -> Value {
    match resultat {
        Ok(document) => json!({
            "statut": "ok",
            "arbre": arbre_des_types(&document),
        }),
        Err(http) => json!({ "statut": "erreur", "http": http }),
    }
}

pub async fn cles_brutes(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Query(p): Query<ParametresClesBrutes>,
) -> Response {
    let album_id = p.album_id.as_deref().filter(|s| !s.is_empty());
    let track_id = p.track_id.as_deref().filter(|s| !s.is_empty());
    let label_id = p.label_id.as_deref().filter(|s| !s.is_empty());
    let avec_albums = p.extra.as_deref() == Some("albums");
    if album_id.is_none() && track_id.is_none() && label_id.is_none() {
        return crate::message_api::refus(
            StatusCode::BAD_REQUEST,
            "album_or_track_id_required",
            "api.diagQobuz.albumOrTrackRequired",
            &[],
        );
    }
    if album_id.is_some_and(|id| !identifiant_valide(id))
        || track_id.is_some_and(|id| !identifiant_valide(id))
        || label_id.is_some_and(|id| !identifiant_valide(id))
        || p.extra
            .as_deref()
            .is_some_and(|e| !e.is_empty() && e != "albums")
    {
        return crate::message_api::refus(
            StatusCode::BAD_REQUEST,
            "invalid_identifier",
            "api.diagQobuz.invalidId",
            &[],
        );
    }

    // Le registre n'est tenu que le temps de cloner l'Arc.
    let Some(svc) = state.services.lock().await.get("qobuz") else {
        return erreur(StatusCode::NOT_FOUND, "service qobuz absent");
    };
    let garde = svc.read().await;
    let Some(qobuz) = garde.as_any().downcast_ref::<QobuzService>() else {
        return erreur(StatusCode::INTERNAL_SERVER_ERROR, "service qobuz inattendu");
    };

    let session_active = qobuz.auth_status().await.authenticated;
    let album = match album_id {
        Some(id) => Some(rendre(
            qobuz
                .reponse_brute_de_diagnostic(RessourceBrute::Album(id))
                .await,
        )),
        None => None,
    };
    let track = match track_id {
        Some(id) => Some(rendre(
            qobuz
                .reponse_brute_de_diagnostic(RessourceBrute::Piste(id))
                .await,
        )),
        None => None,
    };

    let label = match label_id {
        Some(id) => Some(rendre_types_seuls(
            qobuz
                .reponse_brute_de_diagnostic(RessourceBrute::Label { id, avec_albums })
                .await,
        )),
        None => None,
    };

    Json(json!({
        "service": "qobuz",
        "session_qobuz_active": session_active,
        "album_id": album_id,
        "track_id": track_id,
        "label_id": label_id,
        "regle": {
            "valeurs_rendues": "booléens, et clés (ou sous-clés) dont le nom contient un fragment notable",
            "fragments_notables": FRAGMENTS_NOTABLES,
            "fragments_sensibles_toujours_masques": FRAGMENTS_SENSIBLES,
        },
        "album": album,
        "track": track,
        "label": label,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;
    use tune_core::db::settings_repo::SettingsRepo;

    use super::*;

    const SECRET: &str = "secret-de-test-5530";
    const ROUTE: &str = "/api/v1/streaming/qobuz/debug/raw-keys";

    fn application(auth: bool) -> axum::Router {
        let state = AppState::new(":memory:", 0, Default::default()).expect("état");
        if auth {
            let s = SettingsRepo::with_backend(state.backend.clone());
            s.set("auth_enabled", "true").unwrap();
            s.set("jwt_secret", SECRET).unwrap();
        }
        crate::routes::router_with_plugins(state, Vec::new())
    }

    async fn statut(app: &axum::Router, chemin: &str, jeton: Option<&str>) -> StatusCode {
        let mut req = Request::get(chemin);
        if let Some(j) = jeton {
            req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
        }
        app.clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// Sans jeton : 401 ; jeton non admin : 403 ; jeton admin : la requête
    /// ENTRE dans le gestionnaire — prouvé par le 400 de la validation, qui
    /// ne touche pas le réseau.
    #[tokio::test]
    async fn la_route_est_reservee_a_l_administrateur() {
        let app = application(true);
        let chemin = format!("{ROUTE}?album_id=abc123");
        assert_eq!(statut(&app, &chemin, None).await, StatusCode::UNAUTHORIZED);
        let user = crate::auth::sign_jwt(2, "user", SECRET).unwrap();
        assert_eq!(
            statut(&app, &chemin, Some(&user)).await,
            StatusCode::FORBIDDEN
        );
        let admin = crate::auth::sign_jwt(1, "admin", SECRET).unwrap();
        assert_eq!(
            statut(&app, ROUTE, Some(&admin)).await,
            StatusCode::BAD_REQUEST
        );
    }

    /// La route est bien montée à ce chemin — et non avalée par
    /// `/{service}/…` du routeur de streaming.
    #[tokio::test]
    async fn la_route_est_montee_et_valide_ses_parametres() {
        let app = application(false);
        assert_eq!(statut(&app, ROUTE, None).await, StatusCode::BAD_REQUEST);
        assert_eq!(
            statut(&app, &format!("{ROUTE}?album_id=a%26limit%3D1"), None).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            statut(&app, &format!("{ROUTE}?track_id=../x"), None).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn la_sonde_label_appelle_label_get_avec_les_bons_parametres() {
        let (chemin, p) = RessourceBrute::Label {
            id: "9300",
            avec_albums: false,
        }
        .requete();
        assert_eq!(chemin, "/label/get");
        assert_eq!(p, vec![("label_id", "9300")]);
        let (_, p) = RessourceBrute::Label {
            id: "9300",
            avec_albums: true,
        }
        .requete();
        assert_eq!(p, vec![("label_id", "9300"), ("extra", "albums")]);
    }

    #[test]
    fn la_sortie_label_ne_porte_aucune_valeur() {
        let doc = json!({ "id": 9300, "name": "FUITE", "ok": true,
                          "image": { "large": "https://x/FUITE.jpg" } });
        let sortie = rendre_types_seuls(Ok(doc)).to_string();
        for interdit in ["FUITE", "9300", "http", "true", "valeurs", "cles_notables"] {
            assert!(!sortie.contains(interdit), "{interdit} dans {sortie}");
        }
        assert!(sortie.contains("\"large\""));
        // Échec : seul le code HTTP.
        assert_eq!(
            rendre_types_seuls(Err(Some(404))),
            json!({ "statut": "erreur", "http": 404 })
        );
    }

    #[tokio::test]
    async fn la_route_valide_label_id_et_extra() {
        let app = application(false);
        let get = |q: &str| format!("{ROUTE}?{q}");
        assert_eq!(
            statut(&app, &get("label_id=a%26b"), None).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            statut(&app, &get("label_id=9300&extra=tracks"), None).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn identifiants() {
        assert!(identifiant_valide("0825646254385"));
        assert!(identifiant_valide("ay8ts6xtfk2yb"));
        assert!(!identifiant_valide(""));
        assert!(!identifiant_valide("a&b"));
        assert!(!identifiant_valide(&"a".repeat(65)));
    }
}
