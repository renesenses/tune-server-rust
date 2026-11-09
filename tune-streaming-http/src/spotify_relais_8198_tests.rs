//! renesenses/tune-server-rust#8198, volet 3 — retour Spotify par le relais
//! HTTPS de Mozaik Labs, désactivé par défaut ; autorisations persistées.
//!
//! Le vrai `SpotifyService`, par le vrai routeur ; les comptes Spotify sont une
//! doublure locale qui note ce qu'elle reçoit.

use super::*;
use std::sync::Mutex as VerrouSimple;
use tower::ServiceExt;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::spotify::{
    RELAIS_PAR_DEFAUT, SpotifyService, base64url_encode, decoder_composant, encoder_composant,
};

const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";
const BOUCLE: &str = "http://127.0.0.1:8888/api/v1/streaming/spotify/callback";

fn base_de_donnees() -> Arc<dyn DbBackend> {
    Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"))
}

fn app(backend: Arc<dyn DbBackend>, base_doublure: &str) -> Router {
    let svc = SpotifyService::with_config(Some(CLIENT_ID), None, 8888).avec_doublure(base_doublure);
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(svc));
    let etat = StreamingHttpState::new(
        backend,
        Arc::new(Mutex::new(registre)),
        Arc::new(EventBus::new()),
    );
    Router::new().nest("/api/v1/streaming", router().with_state(etat))
}

/// Doublure : rend un jeton à tout échange et garde le corps reçu.
async fn doublure() -> (String, Arc<VerrouSimple<Vec<String>>>) {
    let recus = Arc::new(VerrouSimple::new(Vec::new()));
    let pour_jeton = recus.clone();
    let routeur = Router::new()
        .route(
            "/api/token",
            post(move |corps: String| {
                let recus = pour_jeton.clone();
                async move {
                    recus.lock().unwrap().push(corps);
                    Json(json!({
                        "access_token": "jeton-essai",
                        "refresh_token": "renouveau-essai",
                        "expires_in": 3600,
                    }))
                }
            }),
        )
        .route(
            "/v1/me",
            get(|| async { Json(json!({"display_name": "Essai", "id": "u-essai"})) }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, routeur).await.unwrap() });
    (base, recus)
}

struct Reponse {
    statut: StatusCode,
    texte: String,
}

impl Reponse {
    fn json(&self) -> Value {
        serde_json::from_str(&self.texte).unwrap_or(Value::Null)
    }
}

async fn envoyer(app: &Router, req: axum::http::Request<axum::body::Body>) -> Reponse {
    let r = app.clone().oneshot(req).await.unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    Reponse {
        statut,
        texte: String::from_utf8_lossy(&octets).into_owned(),
    }
}

async fn json_req(app: &Router, methode: &str, uri: &str, corps: Option<Value>) -> Reponse {
    let mut req = axum::http::Request::builder().method(methode).uri(uri);
    let corps = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            axum::body::Body::from(c.to_string())
        }
        None => axum::body::Body::empty(),
    };
    envoyer(app, req.body(corps).unwrap()).await
}

/// Ouvre une connexion et rend (redirect_uri décodée, state) du lien Spotify.
async fn ouvrir(app: &Router, corps: Value) -> (String, String) {
    let r = json_req(app, "POST", "/api/v1/streaming/spotify/auth", Some(corps)).await;
    assert_eq!(r.statut, StatusCode::OK, "{}", r.texte);
    let lien = r.json()["verification_url"].as_str().unwrap().to_owned();
    let param = |nom: &str| {
        lien.split_once('?')
            .unwrap()
            .1
            .split('&')
            .find_map(|p| p.strip_prefix(&format!("{nom}=")))
            .map(decoder_composant)
            .unwrap_or_else(|| panic!("{nom} absent de {lien}"))
    };
    (param("redirect_uri"), param("state"))
}

async fn activer(app: &Router, actif: bool) -> Value {
    let r = json_req(
        app,
        "PUT",
        "/api/v1/streaming/spotify/relais",
        Some(json!({ "actif": actif })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK, "{}", r.texte);
    r.json()
}

async fn retour_get(app: &Router, requete: &str) -> Reponse {
    envoyer(
        app,
        axum::http::Request::builder()
            .uri(format!("/api/v1/streaming/spotify/retour?{requete}"))
            .header("accept-language", "fr")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
}

fn b64(texte: &str) -> String {
    base64url_encode(texte.as_bytes())
}

#[tokio::test]
async fn le_relais_est_desactive_par_defaut() {
    let (base, _) = doublure().await;
    let app = app(base_de_donnees(), &base);
    let r = json_req(&app, "GET", "/api/v1/streaming/spotify/relais", None).await;
    assert_eq!(
        r.json(),
        json!({"actif": false, "redirect_uri": RELAIS_PAR_DEFAUT})
    );

    let (redirect, state) = ouvrir(&app, json!({"origine": "http://192.168.1.20:8888"})).await;
    assert_eq!(redirect, BOUCLE, "relais coupé : rien ne change");
    assert!(
        !state.contains('.'),
        "relais coupé : le state reste un nonce : {state}"
    );
}

#[tokio::test]
async fn relais_actif_l_origine_voyage_dans_le_state() {
    let (base, _) = doublure().await;
    let app = app(base_de_donnees(), &base);
    assert_eq!(activer(&app, true).await["actif"], true);

    let (redirect, state) = ouvrir(&app, json!({"origine": "HTTP://192.168.1.20:8888/"})).await;
    assert_eq!(redirect, RELAIS_PAR_DEFAUT);
    let (nonce, origine) = state.split_once('.').expect("nonce.origine");
    assert_eq!(nonce.len(), 22, "{state}");
    assert_eq!(
        origine,
        b64("http://192.168.1.20:8888"),
        "origine réduite à schéma://hôte:port"
    );

    let diag = json_req(&app, "GET", "/api/v1/streaming/spotify/diagnostic", None)
        .await
        .json();
    assert_eq!(diag["redirect_uri"]["valeur"], RELAIS_PAR_DEFAUT, "{diag}");
    assert_eq!(diag["redirect_uri"]["relais"], true, "{diag}");

    // Origine douteuse (identifiants) ou absente : le nonce seul.
    for corps in [
        json!({"origine": "http://moi@192.168.1.20:8888"}),
        json!({}),
    ] {
        let (_, state) = ouvrir(&app, corps).await;
        assert!(!state.contains('.'), "{state}");
    }
}

/// Le parcours complet : le relais renvoie le navigateur sur `retour`, en
/// GET ou en formulaire POST ; l'échange renvoie l'adresse du relais.
#[tokio::test]
async fn retour_get_et_post_connectent() {
    for en_post in [false, true] {
        let (base, recus) = doublure().await;
        let app = app(base_de_donnees(), &base);
        activer(&app, true).await;
        let (_, state) = ouvrir(&app, json!({"origine": "http://192.168.1.20:8888"})).await;
        let r = if en_post {
            envoyer(
                &app,
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/streaming/spotify/retour")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("accept-language", "fr")
                    .body(axum::body::Body::from(format!(
                        "code=code-essai&state={}",
                        encoder_composant(&state)
                    )))
                    .unwrap(),
            )
            .await
        } else {
            retour_get(
                &app,
                &format!("code=code-essai&state={}", encoder_composant(&state)),
            )
            .await
        };
        assert_eq!(r.statut, StatusCode::OK, "{}", r.texte);
        assert!(
            r.texte.contains("data-spotify-retour=\"connecte\""),
            "{}",
            r.texte
        );
        assert!(r.texte.contains("Spotify est connecté"), "{}", r.texte);
        let echange = recus.lock().unwrap().join("\n");
        assert!(
            echange.contains(&format!(
                "redirect_uri={}",
                encoder_composant(RELAIS_PAR_DEFAUT)
            )),
            "l'échange renvoie l'adresse du relais : {echange}"
        );
    }
}

#[tokio::test]
async fn retour_refuse_affiche_le_code_et_la_phrase() {
    let (base, _) = doublure().await;
    let app = app(base_de_donnees(), &base);
    activer(&app, true).await;
    let (_, state) = ouvrir(&app, json!({})).await;
    let r = retour_get(&app, &format!("error=access_denied&state={state}")).await;
    assert_eq!(r.statut, StatusCode::BAD_REQUEST);
    assert!(
        r.texte
            .contains("data-spotify-retour=\"spotify_autorisation_refusee\""),
        "{}",
        r.texte
    );
    assert!(r.texte.contains("Vous avez refusé l'accès"), "{}", r.texte);
}

/// Un redémarrage entre le clic et le retour : la même base, un service neuf.
#[tokio::test]
async fn une_autorisation_survit_a_un_redemarrage() {
    let (base, _) = doublure().await;
    let backend = base_de_donnees();
    let avant = app(backend.clone(), &base);
    let (_, state) = ouvrir(&avant, json!({})).await;

    let apres = app(backend, &base);
    let r = retour_get(&apres, &format!("code=code-essai&state={state}")).await;
    assert_eq!(r.statut, StatusCode::OK, "{}", r.texte);
    assert!(
        r.texte.contains("data-spotify-retour=\"connecte\""),
        "{}",
        r.texte
    );
}

/// Une autorisation persistée de plus de 15 minutes ne se reprend pas.
#[tokio::test]
async fn une_autorisation_perimee_ne_se_reprend_pas() {
    let (base, _) = doublure().await;
    let backend = base_de_donnees();
    SettingsRepo::with_backend(backend.clone())
        .set(
            "spotify_tentatives_pkce",
            &json!([{
                "state": "ancien-state-de-test",
                "verifier": "v".repeat(64),
                "redirect_uri": BOUCLE,
                "ouverte_le": 1_000_000u64,
            }])
            .to_string(),
        )
        .unwrap();
    let app = app(backend, &base);
    let r = retour_get(&app, "code=code-essai&state=ancien-state-de-test").await;
    assert_eq!(r.statut, StatusCode::BAD_REQUEST, "{}", r.texte);
    assert!(
        r.texte
            .contains("data-spotify-retour=\"spotify_state_inconnu\""),
        "{}",
        r.texte
    );
}

/// Couper le relais après l'ouverture : l'échange garde l'adresse de CETTE
/// autorisation (sinon Spotify refuse : adresse différente).
#[tokio::test]
async fn chaque_autorisation_garde_son_adresse_de_retour() {
    let (base, recus) = doublure().await;
    let app = app(base_de_donnees(), &base);
    activer(&app, true).await;
    let (_, state) = ouvrir(&app, json!({})).await;
    activer(&app, false).await;
    let r = retour_get(&app, &format!("code=code-essai&state={state}")).await;
    assert_eq!(r.statut, StatusCode::OK, "{}", r.texte);
    let echange = recus.lock().unwrap().join("\n");
    assert!(
        echange.contains(&format!(
            "redirect_uri={}",
            encoder_composant(RELAIS_PAR_DEFAUT)
        )),
        "{echange}"
    );
}
