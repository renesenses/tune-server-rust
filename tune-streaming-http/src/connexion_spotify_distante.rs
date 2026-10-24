//! #6784 — connecter Spotify depuis un téléphone ou un autre poste.
//!
//! Spotify n'accepte en `http://` qu'une adresse de retour en boucle locale
//! (`http://127.0.0.1:<port>/…`, règle du 9 avril 2025, voir
//! `tune_core::streaming::spotify::refus_redirection`). Construire l'adresse
//! depuis l'hôte de la requête (`http://192.168.1.20:8888/…`) donnerait
//! justement une adresse refusée (« Insecure »), et Spotify n'a pas de code
//! d'appareil comme TIDAL (#6889). Depuis un téléphone, le retour tombe donc
//! sur la boucle locale du TÉLÉPHONE, où rien ne répond.
//!
//! Le code d'autorisation est pourtant dans l'adresse de cette page qui ne
//! s'ouvre pas : la coller dans Tune termine l'échange (#2680,
//! `callback_url`). Seul le client web savait le faire ; les apps, non.
//!
//! Cette page, servie par le serveur lui-même, rend ce geste possible depuis
//! N'IMPORTE QUEL navigateur du réseau, sans client web ni mise à jour d'app :
//! `GET /streaming/spotify/connect` ouvre une autorisation et donne le lien
//! Spotify ; le formulaire renvoie l'adresse collée en
//! `POST /streaming/spotify/connect`. HTML simple, sans script : il marche
//! aussi dans le navigateur intégré d'une app.

use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::etiquettes_langue::langues_demandees;
use crate::{StreamingHttpState, get_svc, purge_contenu_utilisateur};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Langue {
    Fr,
    En,
}

/// La première de `fr` / `en` que demande la requête ; l'anglais à défaut.
fn langue(headers: &HeaderMap) -> Langue {
    langues_demandees(headers)
        .iter()
        .find_map(|l| match l.as_str() {
            "fr" => Some(Langue::Fr),
            "en" => Some(Langue::En),
            _ => None,
        })
        .unwrap_or(Langue::En)
}

/// Tout ce qui entre dans la page passe par ici : l'adresse d'autorisation
/// (ses `&`), les messages d'erreur, qui peuvent reprendre un morceau de
/// l'adresse collée (`?error=…`).
fn echapper_html(texte: &str) -> String {
    let mut sortie = String::with_capacity(texte.len());
    for c in texte.chars() {
        match c {
            '&' => sortie.push_str("&amp;"),
            '<' => sortie.push_str("&lt;"),
            '>' => sortie.push_str("&gt;"),
            '"' => sortie.push_str("&quot;"),
            '\'' => sortie.push_str("&#39;"),
            _ => sortie.push(c),
        }
    }
    sortie
}

fn page(langue: Langue, statut: StatusCode, corps: &str) -> Response {
    let lang = match langue {
        Langue::Fr => "fr",
        Langue::En => "en",
    };
    let html = format!(
        r#"<!DOCTYPE html><html lang="{lang}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Tune — Spotify</title></head>
<body style="font-family:system-ui;background:#1a1a2e;color:#eee;margin:0;padding:1.5rem;line-height:1.5">
<div style="max-width:36rem;margin:0 auto">
{corps}
</div></body></html>"#
    );
    (statut, Html(html)).into_response()
}

fn page_erreur(langue: Langue, statut: StatusCode, message: &str) -> Response {
    let (titre, recommencer) = match langue {
        Langue::Fr => ("Connexion à Spotify impossible", "Recommencer"),
        Langue::En => ("Could not connect Spotify", "Start again"),
    };
    page(
        langue,
        statut,
        &format!(
            r#"<h1 style="color:#ef4444">{titre}</h1>
<p>{}</p>
<p><a href="connect" style="color:#1db954">{recommencer}</a></p>"#,
            echapper_html(message)
        ),
    )
}

/// `GET /streaming/spotify/connect` — ouvre une autorisation et montre le
/// lien Spotify, puis le champ où coller l'adresse de retour.
pub(crate) async fn page_connexion(
    State(state): State<StreamingHttpState>,
    headers: HeaderMap,
) -> Response {
    let langue = langue(&headers);
    let svc = match get_svc(&state, "spotify").await {
        Ok(s) => s,
        Err((statut, message)) => return page_erreur(langue, statut, &message),
    };
    // Un corps sans `code`, `callback_url` ni `poll` démarre une autorisation
    // PKCE ; ses tentatives précédentes sont gardées (fil 221).
    let debut = svc.write().await.authenticate(&json!({})).await;
    let url = match debut {
        Ok(status) => match status.verification_url {
            Some(url) => url,
            None => {
                return page_erreur(
                    langue,
                    StatusCode::BAD_GATEWAY,
                    "spotify: no authorization URL",
                );
            }
        },
        Err(e) => return page_erreur(langue, StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let textes = match langue {
        Langue::Fr => [
            "Connecter Spotify",
            "1. Ouvrez Spotify et acceptez l'accès de Tune.",
            "Ouvrir Spotify",
            "2. Spotify vous renvoie ensuite vers une page <code>127.0.0.1</code> qui ne \
             s'ouvre pas : c'est normal sur cet appareil. Copiez l'adresse complète de \
             cette page, dans la barre d'adresse (elle contient <code>?code=</code>).",
            "3. Collez-la ici :",
            "Terminer la connexion",
        ],
        Langue::En => [
            "Connect Spotify",
            "1. Open Spotify and allow Tune access.",
            "Open Spotify",
            "2. Spotify then sends you to a <code>127.0.0.1</code> page that does not \
             load: this is expected on this device. Copy the full address of that page \
             from the address bar (it contains <code>?code=</code>).",
            "3. Paste it here:",
            "Finish connecting",
        ],
    };
    let [titre, etape1, ouvrir, etape2, etape3, terminer] = textes;
    page(
        langue,
        StatusCode::OK,
        &format!(
            r#"<h1 style="color:#1db954">{titre}</h1>
<p>{etape1}</p>
<p><a href="{}" target="_blank" rel="noopener" style="display:inline-block;background:#1db954;color:#000;padding:.6rem 1.2rem;border-radius:2rem;text-decoration:none;font-weight:600">{ouvrir}</a></p>
<p>{etape2}</p>
<form method="post">
<label for="adresse">{etape3}</label><br>
<input id="adresse" name="adresse" type="text" required autocomplete="off" placeholder="http://127.0.0.1:…/callback?code=…" style="width:100%;box-sizing:border-box;padding:.5rem;margin:.5rem 0">
<button type="submit" style="padding:.5rem 1rem">{terminer}</button>
</form>"#,
            echapper_html(&url)
        ),
    )
}

#[derive(Deserialize)]
pub(crate) struct AdresseCollee {
    adresse: String,
}

/// `POST /streaming/spotify/connect` — l'adresse collée termine l'échange,
/// par le même chemin que le client web (`callback_url`, #2680).
pub(crate) async fn terminer_connexion(
    State(state): State<StreamingHttpState>,
    headers: HeaderMap,
    Form(colle): Form<AdresseCollee>,
) -> Response {
    let langue = langue(&headers);
    let svc = match get_svc(&state, "spotify").await {
        Ok(s) => s,
        Err((statut, message)) => return page_erreur(langue, statut, &message),
    };
    let mut svc = svc.write().await;
    let issue = svc
        .authenticate(&json!({"callback_url": colle.adresse}))
        .await;
    drop(svc);
    match issue {
        Ok(status) if status.authenticated => {
            state.save_tokens().await;
            purge_contenu_utilisateur("spotify");
            state.event_bus.emit(
                "streaming.auth.success",
                json!({
                    "service": "spotify",
                    "username": &status.username,
                }),
            );
            let nom = echapper_html(status.username.as_deref().unwrap_or(""));
            let (titre, compte, fermer) = match langue {
                Langue::Fr => (
                    "Spotify est connecté",
                    "Compte",
                    "Vous pouvez fermer cette page et revenir à Tune.",
                ),
                Langue::En => (
                    "Spotify connected",
                    "Account",
                    "You can close this page and go back to Tune.",
                ),
            };
            page(
                langue,
                StatusCode::OK,
                &format!(
                    r#"<h1 style="color:#4ade80">{titre}</h1>
<p>{compte} : <strong>{nom}</strong></p>
<p style="color:#888">{fermer}</p>"#
                ),
            )
        }
        Ok(_) => page_erreur(
            langue,
            StatusCode::BAD_REQUEST,
            "spotify: not authenticated after the code exchange",
        ),
        Err(e) => {
            tracing::warn!(error = %e, "spotify_connexion_distante_refusee");
            page_erreur(langue, StatusCode::BAD_REQUEST, &e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::sync::Mutex;
    use tower::ServiceExt;
    use tune_core::TuneError;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::event_bus::EventBus;
    use tune_core::streaming::ServiceRegistry;
    use tune_core::streaming::traits::{
        AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack,
        StreamUrl, StreamingService,
    };

    const AUTORISATION: &str =
        "https://accounts.spotify.com/authorize?client_id=abc&redirect_uri=x&state=s1";

    /// Un faux Spotify : il note les identifiants reçus, ne joint aucun réseau.
    /// Un `callback_url` portant `code=bon` connecte « Alex » ; tout autre est
    /// refusé avec le texte de l'erreur, qui reprend l'adresse collée.
    struct FauxSpotify {
        recus: Arc<StdMutex<Vec<Value>>>,
        connecte: bool,
    }

    const PANNE: &str = "hors sujet";

    #[async_trait::async_trait]
    impl StreamingService for FauxSpotify {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn name(&self) -> &str {
            "spotify"
        }
        fn enabled(&self) -> bool {
            true
        }
        fn set_enabled(&mut self, _e: bool) {}
        async fn authenticate(&mut self, c: &Value) -> Result<AuthStatus, TuneError> {
            self.recus.lock().unwrap().push(c.clone());
            match c.get("callback_url").and_then(Value::as_str) {
                None => Ok(AuthStatus {
                    verification_url: Some(AUTORISATION.into()),
                    ..Default::default()
                }),
                Some(colle) if colle.contains("code=bon") => {
                    self.connecte = true;
                    Ok(self.auth_status().await)
                }
                Some(colle) => Err(TuneError::Streaming(format!("spotify: refus {colle}"))),
            }
        }
        async fn auth_status(&self) -> AuthStatus {
            AuthStatus {
                authenticated: self.connecte,
                username: self.connecte.then(|| "Alex".into()),
                ..Default::default()
            }
        }
        async fn logout(&mut self) -> Result<(), TuneError> {
            Ok(())
        }
        async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
    }

    fn app(recus: Arc<StdMutex<Vec<Value>>>) -> Router {
        let backend: Arc<dyn DbBackend> =
            Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
        let mut registre = ServiceRegistry::new();
        registre.register(Box::new(FauxSpotify {
            recus,
            connecte: false,
        }));
        let etat = StreamingHttpState::new(
            backend,
            Arc::new(Mutex::new(registre)),
            Arc::new(EventBus::new()),
        );
        Router::new().nest("/api/v1/streaming", router().with_state(etat))
    }

    async fn appel(app: Router, requete: Request<Body>) -> (StatusCode, String) {
        let r = app.oneshot(requete).await.unwrap();
        let statut = r.status();
        let octets = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (statut, String::from_utf8_lossy(&octets).into_owned())
    }

    fn ouvrir(langue: &str) -> Request<Body> {
        Request::builder()
            .uri("/api/v1/streaming/spotify/connect")
            // Le téléphone joint le serveur par son adresse sur le réseau.
            .header("host", "192.168.1.18:8888")
            .header("accept-language", langue)
            .body(Body::empty())
            .unwrap()
    }

    fn coller(adresse: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/streaming/spotify/connect")
            .header("host", "192.168.1.18:8888")
            .header("accept-language", "fr-FR,fr;q=0.9")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!("adresse={}", urlencoding_form(adresse))))
            .unwrap()
    }

    /// Encodage de formulaire minimal pour les adresses des essais.
    fn urlencoding_form(texte: &str) -> String {
        texte
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    /// TÉMOIN ROUGE avant le correctif : la route n'existait pas (404), un
    /// téléphone n'avait aucun moyen, hors client web, de finir la connexion.
    #[tokio::test]
    async fn un_telephone_obtient_le_lien_spotify_et_le_champ_ou_coller() {
        let recus = Arc::new(StdMutex::new(Vec::new()));
        let (statut, corps) = appel(app(recus.clone()), ouvrir("fr-FR,fr;q=0.9")).await;
        assert_eq!(
            statut,
            StatusCode::OK,
            "GET /streaming/spotify/connect doit exister (#6784) : {corps}"
        );
        // Le lien d'autorisation est celui du service, `&` échappés.
        assert!(
            corps.contains(&format!("href=\"{}\"", AUTORISATION.replace('&', "&amp;"))),
            "{corps}"
        );
        assert!(corps.contains(r#"<form method="post">"#), "{corps}");
        assert!(corps.contains(r#"name="adresse""#), "{corps}");
        assert!(corps.contains("Collez-la ici"), "{corps}");
        // Une autorisation a été ouverte, sans rien d'autre dans la demande.
        assert_eq!(*recus.lock().unwrap(), vec![json!({})]);
    }

    #[tokio::test]
    async fn la_page_suit_la_langue_du_navigateur() {
        let recus = Arc::new(StdMutex::new(Vec::new()));
        let (_, corps) = appel(app(recus), ouvrir("en-GB,en;q=0.9,fr;q=0.5")).await;
        assert!(corps.contains("Paste it here"), "{corps}");
        assert!(corps.contains(r#"lang="en""#), "{corps}");
    }

    /// TÉMOIN ROUGE avant le correctif : l'adresse collée depuis le téléphone
    /// n'atteignait le service par aucune route servie en HTML.
    #[tokio::test]
    async fn l_adresse_collee_termine_la_connexion() {
        let recus = Arc::new(StdMutex::new(Vec::new()));
        let adresse = "http://127.0.0.1:8888/api/v1/streaming/spotify/callback?code=bon&state=s1";
        let (statut, corps) = appel(app(recus.clone()), coller(adresse)).await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert!(corps.contains("Spotify est connecté"), "{corps}");
        assert!(corps.contains("<strong>Alex</strong>"), "{corps}");
        assert_eq!(
            *recus.lock().unwrap(),
            vec![json!({"callback_url": adresse})],
            "l'adresse collée doit passer telle quelle par `callback_url` (#2680)"
        );
    }

    /// Un refus est montré, échappé : l'erreur reprend l'adresse collée.
    #[tokio::test]
    async fn un_refus_est_affiche_sans_injecter_l_adresse_collee() {
        let recus = Arc::new(StdMutex::new(Vec::new()));
        let adresse = "http://127.0.0.1:8888/cb?error=<script>x</script>";
        let (statut, corps) = appel(app(recus), coller(adresse)).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
        assert!(corps.contains("Connexion à Spotify impossible"), "{corps}");
        assert!(!corps.contains("<script>"), "{corps}");
        assert!(corps.contains("&lt;script&gt;"), "{corps}");
        assert!(corps.contains(r#"href="connect""#), "{corps}");
    }

    /// Le vrai service Spotify, sans réseau : la page donne l'autorisation
    /// qu'il ouvre (retour en boucle locale, que Spotify accepte), et une
    /// adresse collée d'une autre tentative est refusée avant tout échange.
    #[tokio::test]
    async fn avec_le_vrai_service_spotify() {
        use tune_core::streaming::spotify::SpotifyService;
        let backend: Arc<dyn DbBackend> =
            Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
        let mut registre = ServiceRegistry::new();
        registre.register(Box::new(SpotifyService::with_config(
            Some("0123456789abcdef0123456789abcdef"),
            Some("http://127.0.0.1:8888/api/v1/streaming/spotify/callback"),
            8888,
        )));
        let etat = StreamingHttpState::new(
            backend,
            Arc::new(Mutex::new(registre)),
            Arc::new(EventBus::new()),
        );
        let app = Router::new().nest("/api/v1/streaming", router().with_state(etat));

        let (statut, corps) = appel(app.clone(), ouvrir("fr")).await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert!(
            corps.contains("href=\"https://accounts.spotify.com/authorize?client_id="),
            "{corps}"
        );
        assert!(
            corps.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8888"),
            "{corps}"
        );

        let (statut, corps) = appel(
            app,
            coller("http://127.0.0.1:8888/api/v1/streaming/spotify/callback?code=c&state=autre"),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
        assert!(corps.contains("autre tentative"), "{corps}");
    }

    #[test]
    fn echappement_html() {
        assert_eq!(
            echapper_html(r#"a&b<c>"d"'e"#),
            "a&amp;b&lt;c&gt;&quot;d&quot;&#39;e"
        );
    }
}
