//! renesenses/tune-server-rust#8198, volet 3 — le retour Spotify sans
//! copier-coller, par le relais HTTPS de Mozaik Labs.
//!
//! Spotify n'accepte en `http://` qu'une adresse de retour en boucle locale :
//! depuis un autre poste que le serveur, la page `127.0.0.1` ne s'ouvre pas et
//! l'utilisateur doit en copier l'adresse dans Tune. Avec l'option « retour
//! automatique », chacun déclare dans SA propre application Spotify une seule
//! adresse HTTPS, fixe : `https://mozaiklabs.fr/spotify-callback`. Le `state`
//! porte l'origine de la page Tune (`<nonce>.<origine en base64url>`) ; le
//! relais renvoie le navigateur vers `<origine>/api/v1/streaming/spotify/retour`.
//! Le code seul ne vaut rien sans le vérificateur PKCE, qui reste ici.
//!
//! L'option est DÉSACTIVÉE par défaut, tant que le banc d'essai des
//! navigateurs n'est pas validé (réglage `spotify_retour_relais`).
//!
//! Les autorisations entamées sont aussi persistées (`spotify_tentatives_pkce`,
//! 15 minutes) : un redémarrage du serveur entre le clic et le retour ne les
//! perd plus.

use axum::Form;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::streaming::spotify::{SpotifyService, adresse_du_relais, encoder_composant};

use crate::connexion_spotify_distante::{Langue, echapper_html, langue, page};
use crate::{StreamingHttpState, get_svc, purge_contenu_utilisateur, reponse_code_de};

/// Réglage : `"true"` active le retour par le relais.
pub(crate) const REGLAGE_RELAIS: &str = "spotify_retour_relais";
/// Réglage : les autorisations entamées (JSON), 15 minutes au plus.
pub(crate) const REGLAGE_TENTATIVES: &str = "spotify_tentatives_pkce";

fn relais_regle(state: &StreamingHttpState) -> bool {
    SettingsRepo::with_backend(state.backend.clone())
        .get(REGLAGE_RELAIS)
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

/// Avant toute opération de connexion Spotify : applique le réglage du relais
/// et reprend les autorisations persistées si le service n'en a aucune (il
/// vient de démarrer).
pub(crate) async fn preparer(state: &StreamingHttpState) {
    let Ok(svc) = get_svc(state, "spotify").await else {
        return;
    };
    let relais = relais_regle(state).then(adresse_du_relais);
    let persistees = SettingsRepo::with_backend(state.backend.clone())
        .get(REGLAGE_TENTATIVES)
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let mut garde = svc.write().await;
    if let Some(spotify) = garde.as_any_mut().downcast_mut::<SpotifyService>() {
        spotify.regler_relais(relais);
        if let Some(persistees) = persistees {
            let reprises = spotify.reprendre_tentatives(&persistees);
            if reprises > 0 {
                tracing::info!(reprises, "spotify_tentatives_reprises");
            }
        }
    }
}

/// Après toute opération de connexion Spotify : écrit les autorisations en
/// cours (une liste vide efface la ligne).
pub(crate) async fn persister(state: &StreamingHttpState) {
    let Ok(svc) = get_svc(state, "spotify").await else {
        return;
    };
    let tentatives = {
        let garde = svc.read().await;
        match garde.as_any().downcast_ref::<SpotifyService>() {
            Some(spotify) => spotify.tentatives_a_persister(),
            None => return,
        }
    };
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let vide = tentatives.as_array().is_none_or(|l| l.is_empty());
    let resultat = if vide {
        settings.delete(REGLAGE_TENTATIVES).map(|_| ())
    } else {
        settings.set(REGLAGE_TENTATIVES, &tentatives.to_string())
    };
    if let Err(e) = resultat {
        tracing::warn!(erreur = %e, "spotify_tentatives_non_persistees");
    }
}

fn etat_du_relais(actif: bool) -> Json<serde_json::Value> {
    Json(json!({ "actif": actif, "redirect_uri": adresse_du_relais() }))
}

/// `GET /streaming/spotify/relais`.
pub(crate) async fn lire(State(state): State<StreamingHttpState>) -> Response {
    etat_du_relais(relais_regle(&state)).into_response()
}

#[derive(Deserialize)]
pub(crate) struct ReglageRelais {
    actif: bool,
}

/// `PUT /streaming/spotify/relais` `{"actif": bool}`.
pub(crate) async fn regler(
    State(state): State<StreamingHttpState>,
    Json(reglage): Json<ReglageRelais>,
) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = settings.set(REGLAGE_RELAIS, if reglage.actif { "true" } else { "false" }) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"code": "reglage_non_enregistre", "error": e.to_string()})),
        )
            .into_response();
    }
    preparer(&state).await;
    tracing::info!(actif = reglage.actif, "spotify_retour_relais_regle");
    etat_du_relais(reglage.actif).into_response()
}

/// Les paramètres du retour, en requête (`GET`) ou en formulaire (`POST`).
#[derive(Deserialize, Default)]
pub(crate) struct ParametresRetour {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

impl ParametresRetour {
    /// Une adresse de retour équivalente, lue par le même chemin que
    /// l'adresse collée (`callback_url`) : mêmes codes de refus.
    fn adresse(&self) -> String {
        let mut paires = Vec::new();
        for (cle, valeur) in [
            ("code", &self.code),
            ("state", &self.state),
            ("error", &self.error),
        ] {
            if let Some(v) = valeur {
                paires.push(format!("{cle}={}", encoder_composant(v)));
            }
        }
        format!("/api/v1/streaming/spotify/retour?{}", paires.join("&"))
    }
}

/// La phrase d'un refus, dans la langue de la page : ce qu'il faut faire.
fn phrase(code: Option<&str>, langue: Langue) -> &'static str {
    use Langue::{En, Fr};
    match (code.unwrap_or(""), langue) {
        ("spotify_autorisation_refusee", Fr) => {
            "Vous avez refusé l'accès sur la page Spotify. Recommencez et acceptez."
        }
        ("spotify_autorisation_refusee", En) => {
            "You declined access on the Spotify page. Start again and accept."
        }
        ("spotify_state_inconnu", Fr) => {
            "Ce retour ne correspond à aucune connexion en cours (lien ancien, ou plus de 15 minutes). Cliquez de nouveau sur Se connecter dans Tune."
        }
        ("spotify_state_inconnu", En) => {
            "This return matches no sign-in in progress (old link, or more than 15 minutes). Click Sign in again in Tune."
        }
        ("spotify_code_invalide", Fr) => {
            "Ce code a déjà servi ou a expiré. Cliquez de nouveau sur Se connecter dans Tune."
        }
        ("spotify_code_invalide", En) => {
            "This code was already used or has expired. Click Sign in again in Tune."
        }
        ("spotify_redirect_uri_differente", Fr) => {
            "L'adresse de retour déclarée dans votre application Spotify ne correspond pas. Déclarez exactement celle que Tune affiche (https://mozaiklabs.fr/spotify-callback)."
        }
        ("spotify_redirect_uri_differente", En) => {
            "The redirect URI declared in your Spotify app does not match. Declare exactly the one Tune shows (https://mozaiklabs.fr/spotify-callback)."
        }
        ("spotify_client_id_refuse", Fr) => {
            "Spotify ne reconnaît pas ce Client ID. Vérifiez-le dans Tune (32 caractères, copiés depuis votre application Spotify)."
        }
        ("spotify_client_id_refuse", En) => {
            "Spotify does not recognise this Client ID. Check it in Tune (32 characters, copied from your Spotify app)."
        }
        ("spotify_client_id_absent", Fr) => {
            "Aucun Client ID n'est enregistré dans Tune. Saisissez celui de votre application Spotify, puis reconnectez-vous."
        }
        ("spotify_client_id_absent", En) => {
            "No Client ID is saved in Tune. Enter the one from your Spotify app, then sign in again."
        }
        ("spotify_compte_non_autorise", Fr) => {
            "Spotify refuse l'accès à ce compte. Ajoutez l'adresse e-mail de votre compte Spotify dans User Management de votre application, et vérifiez que son propriétaire a Spotify Premium."
        }
        ("spotify_compte_non_autorise", En) => {
            "Spotify denies access to this account. Add your Spotify account's e-mail address under User Management in your app, and check that the app owner has Spotify Premium."
        }
        ("spotify_callback_sans_code", Fr) => {
            "Le retour ne contient pas de code d'autorisation. Cliquez de nouveau sur Se connecter dans Tune."
        }
        ("spotify_callback_sans_code", En) => {
            "The return carries no authorization code. Click Sign in again in Tune."
        }
        (_, Fr) => {
            "La connexion à Spotify n'a pas abouti. Cliquez de nouveau sur Se connecter dans Tune."
        }
        (_, En) => "Connecting Spotify did not succeed. Click Sign in again in Tune.",
    }
}

async fn terminer(state: StreamingHttpState, headers: HeaderMap, p: ParametresRetour) -> Response {
    let langue = langue(&headers);
    preparer(&state).await;
    let svc = match get_svc(&state, "spotify").await {
        Ok(s) => s,
        Err((statut, message)) => {
            return page_echec(langue, statut, None, &message);
        }
    };
    let issue = svc
        .write()
        .await
        .authenticate(&json!({ "callback_url": p.adresse() }))
        .await;
    persister(&state).await;
    match issue {
        Ok(status) if status.authenticated => {
            state.save_tokens().await;
            purge_contenu_utilisateur("spotify");
            state.event_bus.emit(
                "streaming.auth.success",
                json!({ "service": "spotify", "username": &status.username }),
            );
            let nom = echapper_html(status.username.as_deref().unwrap_or(""));
            let (titre, compte, retour) = match langue {
                Langue::Fr => ("Spotify est connecté", "Compte", "Retour à Tune"),
                Langue::En => ("Spotify connected", "Account", "Back to Tune"),
            };
            page(
                langue,
                StatusCode::OK,
                &format!(
                    r#"<meta http-equiv="refresh" content="2;url=/">
<h1 style="color:#4ade80" data-spotify-retour="connecte">{titre}</h1>
<p>{compte} : <strong>{nom}</strong></p>
<p><a href="/" style="color:#1db954">{retour}</a></p>"#
                ),
            )
        }
        Ok(_) => page_echec(
            langue,
            StatusCode::BAD_REQUEST,
            None,
            "spotify: not authenticated after the code exchange",
        ),
        Err(e) => {
            tracing::warn!(
                code = reponse_code_de(&e).unwrap_or("-"),
                "spotify_retour_refuse"
            );
            page_echec(
                langue,
                StatusCode::BAD_REQUEST,
                reponse_code_de(&e),
                &e.to_string(),
            )
        }
    }
}

fn page_echec(langue: Langue, statut: StatusCode, code: Option<&str>, detail: &str) -> Response {
    let (titre, recommencer) = match langue {
        Langue::Fr => ("Connexion à Spotify impossible", "Retour à Tune"),
        Langue::En => ("Could not connect Spotify", "Back to Tune"),
    };
    page(
        langue,
        statut,
        &format!(
            r#"<h1 style="color:#ef4444" data-spotify-retour="{}">{titre}</h1>
<p>{}</p>
<p style="color:#888;font-size:.85em">{}</p>
<p><a href="/" style="color:#1db954">{recommencer}</a></p>"#,
            echapper_html(code.unwrap_or("echec")),
            phrase(code, langue),
            echapper_html(detail),
        ),
    )
}

/// `GET /streaming/spotify/retour?code=…&state=…` (ou `error=…`).
pub(crate) async fn retour_get(
    State(state): State<StreamingHttpState>,
    headers: HeaderMap,
    Query(p): Query<ParametresRetour>,
) -> Response {
    terminer(state, headers, p).await
}

/// `POST /streaming/spotify/retour` (formulaire `code`, `state`, `error`).
pub(crate) async fn retour_post(
    State(state): State<StreamingHttpState>,
    headers: HeaderMap,
    Form(p): Form<ParametresRetour>,
) -> Response {
    terminer(state, headers, p).await
}
