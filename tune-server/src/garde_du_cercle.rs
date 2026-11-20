//! Tune Circle : qui peut agir au nom du compte mozaiklabs de ce serveur.
//!
//! Le greffon `circle` parle au cloud avec LA session SSO du serveur : un seul
//! compte, quel que soit le profil Tune qui appelle. Décision du 19/11/2026 :
//!
//! - les lectures (`GET`, `HEAD`) restent ouvertes comme les autres routes ;
//! - toute autre méthode (inviter, accepter, ranger, partager, poser sa
//!   position, écrire un message…) exige un profil **administrateur** quand
//!   l'authentification est active : `403 circle.admin_required` sinon ;
//! - authentification coupée, rien ne change en local (l'utilisateur unique
//!   est l'administrateur) ;
//! - une requête rejouée par le relais ou l'écoute directe ne fait que lire,
//!   authentification active ou non : `403 circle.remote_read_only`. La même
//!   règle est écrite dans [`crate::garde_du_relais`], qui passe en premier.
//!
//! Quatre `POST` ne changent rien au cercle ni au compte et restent ouverts à
//! tous les profils en local : ils lisent ou font jouer, comme `/zones` —
//! voir [`sans_effet_sur_le_cercle`].

use axum::extract::{FromRequestParts, Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::AuthUser;
use crate::state::AppState;

pub const CODE_ADMIN_REQUIS: &str = "circle.admin_required";
pub const CODE_LECTURE_SEULE_A_DISTANCE: &str = "circle.remote_read_only";
pub const CLE_ADMIN_REQUIS: &str = "api.circle.adminRequired";
pub const CLE_LECTURE_SEULE_A_DISTANCE: &str = "api.circle.remoteReadOnly";

fn lecture(m: &Method) -> bool {
    m == Method::GET || m == Method::HEAD || m == Method::OPTIONS
}

/// Les segments du chemin sous le greffon, quel que soit le préfixe encore
/// présent (`/api/v1/ext/circle`, `/ext/circle` ou aucun, sous `nest_service`).
fn segments(chemin: &str) -> Vec<&str> {
    let mut s: Vec<&str> = chemin.split('/').filter(|x| !x.is_empty()).collect();
    for prefixe in [&["api", "v1", "ext", "circle"][..], &["ext", "circle"][..]] {
        if s.starts_with(prefixe) {
            s.drain(..prefixe.len());
            break;
        }
    }
    s
}

/// Les `POST` du greffon qui lisent ou font jouer, sans rien écrire au compte :
/// `playlists/{id}/resolve`, `playlists/{id}/play`,
/// `contacts/{user_id}/listen` et `my-sources/album-covers`.
pub fn sans_effet_sur_le_cercle(m: &Method, chemin: &str) -> bool {
    m == Method::POST
        && matches!(
            segments(chemin).as_slice(),
            ["playlists", _, "resolve" | "play"]
                | ["contacts", _, "listen"]
                | ["my-sources", "album-covers"]
        )
}

/// La requête écrit-elle au nom du compte ?
pub fn ecriture_du_cercle(m: &Method, chemin: &str) -> bool {
    !lecture(m) && !sans_effet_sur_le_cercle(m, chemin)
}

/// Couche posée sur le routeur du greffon `circle`, sous `auth_middleware`.
pub async fn garde_des_ecritures_du_cercle(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let methode = request.method().clone();
    if !lecture(&methode) && crate::garde_du_relais::requete_relayee(request.headers()) {
        tracing::info!(methode = %methode, "cercle_ecriture_distante_refusee");
        return crate::message_api::refus(
            StatusCode::FORBIDDEN,
            CODE_LECTURE_SEULE_A_DISTANCE,
            CLE_LECTURE_SEULE_A_DISTANCE,
            &[],
        );
    }
    if !ecriture_du_cercle(&methode, request.uri().path())
        || !crate::auth::authentification_active(&state)
    {
        return next.run(request).await;
    }
    let (mut parts, corps) = request.into_parts();
    match <AuthUser as FromRequestParts<AppState>>::from_request_parts(&mut parts, &state).await {
        Ok(user) if user.role == "admin" => next.run(Request::from_parts(parts, corps)).await,
        Ok(_) => {
            tracing::info!(methode = %methode, "cercle_ecriture_refusee_profil_non_admin");
            crate::message_api::refus(
                StatusCode::FORBIDDEN,
                CODE_ADMIN_REQUIS,
                CLE_ADMIN_REQUIS,
                &[],
            )
        }
        Err(refus) => refus.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventaire_des_ecritures() {
        for (m, c) in [
            (Method::POST, "/invitations"),
            (Method::POST, "/invitations/3/accept"),
            (Method::POST, "/invitations/3/decline"),
            (Method::DELETE, "/invitations/3"),
            (Method::POST, "/invitations/by-pseudo"),
            (Method::POST, "/invitations/3/block"),
            (Method::POST, "/invitation-links"),
            (Method::DELETE, "/invitation-links/3"),
            (Method::DELETE, "/blocks/7"),
            (Method::DELETE, "/members/7"),
            (Method::POST, "/circles"),
            (Method::PATCH, "/circles/1"),
            (Method::DELETE, "/circles/1"),
            (Method::PUT, "/circles/1/members/7"),
            (Method::DELETE, "/circles/1/members/7"),
            (Method::PUT, "/circles/1/sharing/library"),
            (Method::DELETE, "/circles/1/sharing/library"),
            (Method::PUT, "/circles/1/sharing/members"),
            (Method::DELETE, "/circles/1/sharing/members"),
            (Method::PUT, "/member-of/1/show-me"),
            (Method::DELETE, "/member-of/1/show-me"),
            (Method::PUT, "/me/location"),
            (Method::DELETE, "/me/location"),
            (Method::PUT, "/me/public-map-opt-out"),
            (Method::DELETE, "/me/public-map-opt-out"),
            (Method::POST, "/notifications/3/read"),
            (Method::POST, "/notifications/read-all"),
            (Method::PUT, "/notifications/preferences"),
            (Method::POST, "/general-circles/2/membership"),
            (Method::DELETE, "/general-circles/2/membership"),
            (Method::POST, "/general-circles/2/conversation"),
            (Method::POST, "/conversations/4/name-consent"),
            (Method::POST, "/contacts/7/conversation"),
            (Method::POST, "/conversations/4/messages"),
            (Method::POST, "/conversations/4/read"),
            (Method::DELETE, "/conversations/4/messages/9"),
            (Method::POST, "/conversations/4/reports"),
            (Method::POST, "/circles/1/conversation"),
            (Method::POST, "/circles/1/conversation/messages"),
            (Method::PUT, "/circles/1/messaging"),
            (Method::PUT, "/circles/1/sets/tag/9"),
            (Method::DELETE, "/circles/1/sets/tag/9"),
            (Method::POST, "/playlists"),
            (Method::PATCH, "/playlists/3"),
            (Method::DELETE, "/playlists/3"),
            (Method::POST, "/playlists/3/items"),
            (Method::POST, "/playlists/3/bulk-items"),
            (Method::DELETE, "/playlists/3/items/5"),
            (Method::PUT, "/playlists/3/order"),
            (Method::DELETE, "/recoverable-playlists/3"),
            (Method::POST, "/recoverable-playlists/3/copy"),
            (Method::PUT, "/playlists/3/follow"),
            (Method::DELETE, "/playlists/3/follow"),
            (Method::POST, "/playlists/3/follow/sync"),
            (Method::PUT, "/artwork-sharing"),
            (Method::POST, "/api/v1/ext/circle/invitations"),
        ] {
            assert!(ecriture_du_cercle(&m, c), "{m} {c} est une écriture");
        }
    }

    #[test]
    fn lectures_et_ecoute_restent_ouvertes() {
        for (m, c) in [
            (Method::GET, "/"),
            (Method::GET, "/contacts/locations"),
            (Method::HEAD, "/playlists/3"),
            (Method::POST, "/playlists/3/resolve"),
            (Method::POST, "/playlists/3/play"),
            (Method::POST, "/contacts/7/listen"),
            (Method::POST, "/my-sources/album-covers"),
            (Method::POST, "/api/v1/ext/circle/playlists/3/play"),
        ] {
            assert!(!ecriture_du_cercle(&m, c), "{m} {c} n'écrit rien");
        }
        // Le même chemin sous une autre méthode, ou un segment de plus, écrit.
        assert!(ecriture_du_cercle(&Method::PUT, "/playlists/3/play"));
        assert!(ecriture_du_cercle(&Method::POST, "/playlists/3/play/x"));
    }
}
