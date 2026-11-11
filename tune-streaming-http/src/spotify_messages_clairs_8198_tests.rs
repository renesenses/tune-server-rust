//! renesenses/tune-server-rust#8198, volet 1 — la connexion Spotify dit
//! précisément ce qui ne va pas.
//!
//! * les refus courants de l'échange du code (`invalid_grant`, adresse de
//!   retour différente, `invalid_client`) et l'absence de Client ID portent un
//!   `code` stable ;
//! * un jeton délivré à un compte que l'application n'autorise pas (absent de
//!   User Management, ou propriétaire sans Premium : `/me` en 403) n'est plus
//!   annoncé « connecté » ;
//! * `GET /streaming/spotify/diagnostic` rapporte chaque point.
//!
//! Le vrai `SpotifyService`, par le vrai routeur ; Spotify est une doublure
//! locale dont on règle les réponses.

use super::*;
use std::sync::Mutex as VerrouSimple;
use tower::ServiceExt;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::spotify::{
    CODE_CLIENT_ID_ABSENT, CODE_CLIENT_ID_REFUSE, CODE_CODE_INVALIDE, CODE_COMPTE_NON_AUTORISE,
    CODE_REDIRECT_URI_DIFFERENTE, CODE_STATE_INCONNU, SpotifyService,
};

const RETOUR: &str = "http://127.0.0.1:8888/api/v1/streaming/spotify/callback";
/// 32 caractères hexadécimaux : la forme d'un vrai Client ID.
const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";

fn app(svc: SpotifyService) -> Router {
    let backend: Arc<dyn DbBackend> =
        Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(svc));
    let etat = StreamingHttpState::new(
        backend,
        Arc::new(Mutex::new(registre)),
        Arc::new(EventBus::new()),
    );
    Router::new().nest("/api/v1/streaming", router().with_state(etat))
}

async fn appel(
    app: &Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = axum::http::Request::builder().method(methode).uri(uri);
    let corps = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            axum::body::Body::from(c.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let r = app.clone().oneshot(req.body(corps).unwrap()).await.unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    let corps = serde_json::from_slice(&octets)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&octets).into_owned()));
    (statut, corps)
}

async fn auth(app: &Router, corps: Value) -> (StatusCode, Value) {
    appel(app, "POST", "/api/v1/streaming/spotify/auth", Some(corps)).await
}

async fn diagnostic(app: &Router) -> Value {
    let (statut, corps) = appel(app, "GET", "/api/v1/streaming/spotify/diagnostic", None).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    corps
}

async fn ouvrir(app: &Router) -> String {
    let (statut, corps) = auth(app, json!({})).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let lien = corps["verification_url"]
        .as_str()
        .expect("lien d'autorisation");
    lien.split_once('?')
        .unwrap()
        .1
        .split('&')
        .find_map(|p| p.strip_prefix("state="))
        .expect("state")
        .to_owned()
}

async fn coller(app: &Router, state: &str) -> (StatusCode, Value) {
    auth(
        app,
        json!({ "callback_url": format!("{RETOUR}?code=code-essai&state={state}") }),
    )
    .await
}

fn assert_refus(statut: StatusCode, corps: &Value, code: &str) {
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(
        corps["code"], code,
        "le refus doit porter le code {code} : {corps}"
    );
    assert_eq!(corps["service"], "spotify", "{corps}");
}

/// Ce que la doublure répond à `/api/token` et à `/v1/me`.
#[derive(Clone)]
struct Reponses {
    jeton: (u16, Value),
    me: (u16, Value),
    /// Attente avant de répondre à `/api/token` (Spotify muet).
    delai_jeton: Option<std::time::Duration>,
    /// Le dernier jeton présenté à `/v1/me`.
    jeton_vu: Option<String>,
}

impl Reponses {
    fn normales() -> Self {
        Self {
            jeton: (
                200,
                json!({
                    "access_token": "jeton-essai",
                    "refresh_token": "renouveau-essai",
                    "expires_in": 3600,
                    "scope": "user-read-private",
                }),
            ),
            me: (200, json!({"display_name": "Essai", "id": "u-essai"})),
            delai_jeton: None,
            jeton_vu: None,
        }
    }
}

async fn doublure(reponses: Reponses) -> (String, Arc<VerrouSimple<Reponses>>) {
    let etat = Arc::new(VerrouSimple::new(reponses));
    let pour_jeton = etat.clone();
    let pour_me = etat.clone();
    let doublure = Router::new()
        .route(
            "/api/token",
            post(move || {
                let (statut, corps) = pour_jeton.lock().unwrap().jeton.clone();
                let delai = pour_jeton.lock().unwrap().delai_jeton;
                async move {
                    if let Some(delai) = delai {
                        tokio::time::sleep(delai).await;
                    }
                    (StatusCode::from_u16(statut).unwrap(), Json(corps))
                }
            }),
        )
        .route(
            "/v1/me",
            get(move |entetes: axum::http::HeaderMap| {
                let mut reponses = pour_me.lock().unwrap();
                reponses.jeton_vu = entetes
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.trim_start_matches("Bearer ").to_owned());
                let (statut, corps) = reponses.me.clone();
                drop(reponses);
                async move { (StatusCode::from_u16(statut).unwrap(), Json(corps)) }
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, doublure).await.unwrap() });
    (base, etat)
}

async fn app_avec(reponses: Reponses) -> (Router, Arc<VerrouSimple<Reponses>>) {
    let (base, etat) = doublure(reponses).await;
    let svc = SpotifyService::with_config(Some(CLIENT_ID), None, 8888).avec_doublure(&base);
    (app(svc), etat)
}

#[tokio::test]
async fn sans_client_id_le_refus_porte_spotify_client_id_absent() {
    let app = app(SpotifyService::with_config(Some("placeholder"), None, 8888));
    let (statut, corps) = auth(&app, json!({})).await;
    assert_refus(statut, &corps, CODE_CLIENT_ID_ABSENT);
    let diag = diagnostic(&app).await;
    assert_eq!(diag["client_id"]["etat"], "absent", "{diag}");
    assert_eq!(diag["a_corriger"][0], "client_id_absent", "{diag}");
}

/// Les trois refus courants de l'échange du code, tels que Spotify les écrit.
#[tokio::test]
async fn les_refus_de_l_echange_portent_un_code_stable() {
    for (erreur, description, code) in [
        (
            "invalid_grant",
            "Invalid authorization code",
            CODE_CODE_INVALIDE,
        ),
        (
            "invalid_grant",
            "Authorization code expired",
            CODE_CODE_INVALIDE,
        ),
        (
            "invalid_grant",
            "Invalid redirect URI",
            CODE_REDIRECT_URI_DIFFERENTE,
        ),
        ("invalid_client", "Invalid client", CODE_CLIENT_ID_REFUSE),
    ] {
        let mut reponses = Reponses::normales();
        reponses.jeton = (
            400,
            json!({"error": erreur, "error_description": description}),
        );
        let (app, _) = app_avec(reponses).await;
        let state = ouvrir(&app).await;
        let (statut, corps) = coller(&app, &state).await;
        assert_refus(statut, &corps, code);
        let diag = diagnostic(&app).await;
        assert_eq!(diag["derniere_erreur"]["code"], code, "{diag}");
        assert!(
            diag["a_corriger"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == code),
            "{diag}"
        );
    }
}

/// Le cas silencieux : Spotify délivre le jeton, puis `/me` répond 403.
#[tokio::test]
async fn un_403_sur_me_apres_l_echange_refuse_la_connexion() {
    let mut reponses = Reponses::normales();
    reponses.me = (
        403,
        json!({"error": {"status": 403, "message":
            "Check settings on developer.spotify.com/dashboard, the user may not be registered."}}),
    );
    let (app, _) = app_avec(reponses).await;
    let state = ouvrir(&app).await;
    let (statut, corps) = coller(&app, &state).await;
    assert_refus(statut, &corps, CODE_COMPTE_NON_AUTORISE);
    assert!(
        corps["error"].as_str().unwrap().contains("User Management"),
        "le message dit quoi faire : {corps}"
    );

    let diag = diagnostic(&app).await;
    assert_eq!(
        diag["connexion"]["etat"], "non_connecte",
        "une liaison refusée n'est pas gardée : {diag}"
    );
    assert_eq!(
        diag["derniere_erreur"]["code"], CODE_COMPTE_NON_AUTORISE,
        "{diag}"
    );
}

/// Le vérificateur, configuration correcte et compte autorisé.
#[tokio::test]
async fn diagnostic_tout_va_bien() {
    let mut reponses = Reponses::normales();
    reponses.me = (
        200,
        json!({"display_name": "Essai", "id": "u-essai", "product": "premium"}),
    );
    let (app, _) = app_avec(reponses).await;
    let state = ouvrir(&app).await;
    let (statut, corps) = coller(&app, &state).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");

    let diag = diagnostic(&app).await;
    assert_eq!(diag["client_id"]["etat"], "ok", "{diag}");
    assert_eq!(diag["redirect_uri"]["valeur"], RETOUR, "{diag}");
    assert_eq!(diag["redirect_uri"]["etat"], "ok", "{diag}");
    assert_eq!(diag["connexion"]["etat"], "connecte", "{diag}");
    assert_eq!(diag["connexion"]["compte"], "Essai", "{diag}");
    assert_eq!(diag["acces_api"]["etat"], "ok", "{diag}");
    assert_eq!(diag["premium"]["etat"], "oui", "{diag}");
    assert_eq!(diag["derniere_erreur"], Value::Null, "{diag}");
    assert_eq!(diag["a_corriger"], json!([]), "{diag}");
}

/// Mode développement : `/me` ne dit plus `product`. Premium reste inconnu,
/// ce n'est pas un problème à corriger.
#[tokio::test]
async fn diagnostic_premium_inconnu_sans_product() {
    let (app, _) = app_avec(Reponses::normales()).await;
    let state = ouvrir(&app).await;
    coller(&app, &state).await;
    let diag = diagnostic(&app).await;
    assert_eq!(diag["premium"]["etat"], "inconnu", "{diag}");
    assert_eq!(diag["a_corriger"], json!([]), "{diag}");
}

/// Une liaison existante que Spotify se met à refuser (compte retiré de User
/// Management, Premium du propriétaire arrêté).
#[tokio::test]
async fn diagnostic_rapporte_un_403_sur_une_liaison_existante() {
    let (app, reponses) = app_avec(Reponses::normales()).await;
    let state = ouvrir(&app).await;
    let (statut, _) = coller(&app, &state).await;
    assert_eq!(statut, StatusCode::OK);
    reponses.lock().unwrap().me = (
        403,
        json!({"error": {"status": 403, "message": "Forbidden"}}),
    );

    let diag = diagnostic(&app).await;
    assert_eq!(diag["acces_api"]["etat"], "compte_non_autorise", "{diag}");
    assert_eq!(diag["a_corriger"], json!(["compte_non_autorise"]), "{diag}");
}

/// Non connecté, Client ID de mauvaise forme, `localhost` déclaré.
#[tokio::test]
async fn diagnostic_liste_les_problemes_dans_l_ordre() {
    let svc = SpotifyService::with_config(
        Some("pas-un-client-id"),
        Some("http://localhost:8888/api/v1/streaming/spotify/callback"),
        8888,
    );
    let diag = diagnostic(&app(svc)).await;
    assert_eq!(
        diag["a_corriger"],
        json!([
            "client_id_forme_invalide",
            "redirect_localhost",
            "non_connecte"
        ]),
        "{diag}"
    );
    assert_eq!(diag["acces_api"]["etat"], "non_verifie", "{diag}");
    assert_eq!(diag["redirect_uri"]["relais"], false, "{diag}");
}

/// Le rappel direct (`GET /spotify/callback`, navigateur sur la machine du
/// serveur) rend lui aussi le code stable, plus un texte brut.
#[tokio::test]
async fn le_rappel_direct_rend_le_code_stable() {
    let (app, _) = app_avec(Reponses::normales()).await;
    ouvrir(&app).await;
    let (statut, corps) = appel(
        &app,
        "GET",
        "/api/v1/streaming/spotify/callback?code=abc&state=autre-tentative",
        None,
    )
    .await;
    assert_refus(statut, &corps, CODE_STATE_INCONNU);
}

/// Retour d'un testeur (09/11) : `INVALID_CLIENT: Invalid client` avec un
/// Client ID pourtant correct. Spotify refuse l'échange factice
/// (`invalid_client`, « Failed to get client ») : le vérificateur le dit et
/// propose les causes probables.
#[tokio::test]
async fn diagnostic_signale_un_client_id_refuse_par_spotify_et_ses_causes() {
    let mut reponses = Reponses::normales();
    reponses.jeton = (
        400,
        json!({"error": "invalid_client", "error_description": "Failed to get client"}),
    );
    let (app, _) = app_avec(reponses).await;
    let diag = diagnostic(&app).await;
    assert_eq!(
        diag["client_id"]["etat"], "ok",
        "la forme est bonne : {diag}"
    );
    assert_eq!(diag["client_id"]["spotify"], "refuse", "{diag}");
    assert_eq!(diag["client_id"]["debut"], "01234567", "{diag}");
    assert_eq!(
        diag["client_id"]["causes_possibles"],
        json!([
            "connexion_lancee_avant_le_client_id",
            "proprietaire_sans_premium",
            "application_supprimee_ou_desactivee"
        ]),
        "{diag}"
    );
    assert_eq!(
        diag["a_corriger"],
        json!(["client_id_refuse_par_spotify", "non_connecte"]),
        "{diag}"
    );
}

/// Un Client ID que Spotify accepte (l'échange factice répond `invalid_grant`) :
/// aucune cause à proposer.
#[tokio::test]
async fn diagnostic_client_id_accepte_par_spotify() {
    let mut reponses = Reponses::normales();
    reponses.jeton = (
        400,
        json!({"error": "invalid_grant", "error_description": "Invalid authorization code"}),
    );
    let (app, _) = app_avec(reponses).await;
    let diag = diagnostic(&app).await;
    assert_eq!(diag["client_id"]["spotify"], "accepte", "{diag}");
    assert_eq!(diag["client_id"]["causes_possibles"], json!([]), "{diag}");
    assert_eq!(diag["a_corriger"], json!(["non_connecte"]), "{diag}");
}

/// Une reconnexion que Spotify refuse en 403 ne doit pas déconnecter un
/// utilisateur dont la liaison marche : l'ancienne session reste en place,
/// avec son jeton.
#[tokio::test]
async fn un_403_n_ecrase_pas_une_session_valide() {
    let (app, reponses) = app_avec(Reponses::normales()).await;
    let state = ouvrir(&app).await;
    let (statut, corps) = coller(&app, &state).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");

    // Deuxième connexion, avec un autre compte que l'application refuse.
    {
        let mut r = reponses.lock().unwrap();
        r.jeton.1["access_token"] = json!("jeton-refuse");
        r.jeton.1["refresh_token"] = json!("renouveau-refuse");
        r.me = (
            403,
            json!({"error": {"status": 403, "message": "the user may not be registered."}}),
        );
    }
    let state = ouvrir(&app).await;
    let (statut, corps) = coller(&app, &state).await;
    assert_refus(statut, &corps, CODE_COMPTE_NON_AUTORISE);

    reponses.lock().unwrap().me = (200, json!({"display_name": "Essai", "id": "u-essai"}));
    let diag = diagnostic(&app).await;
    assert_eq!(
        diag["connexion"]["etat"], "connecte",
        "la session valide est gardée : {diag}"
    );
    assert_eq!(diag["connexion"]["compte"], "Essai", "{diag}");
    assert_eq!(diag["acces_api"]["etat"], "ok", "{diag}");
    assert_eq!(
        reponses.lock().unwrap().jeton_vu.as_deref(),
        Some("jeton-essai"),
        "l'ancien jeton reste celui en service"
    );
}

/// Le vérificateur ne garde pas le verrou du service pendant ses appels à
/// Spotify, et ne l'attend pas plus de 5 s : une connexion lancée pendant
/// une sonde lente aboutit tout de suite.
#[tokio::test]
async fn diagnostic_sans_verrou_pendant_les_sondes_et_borne() {
    let mut reponses = Reponses::normales();
    reponses.delai_jeton = Some(std::time::Duration::from_secs(30));
    let (app, _) = app_avec(reponses).await;

    let debut = std::time::Instant::now();
    let pour_diag = app.clone();
    let diag = tokio::spawn(async move { diagnostic(&pour_diag).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Ouvrir une connexion demande le verrou en écriture.
    let state = tokio::time::timeout(std::time::Duration::from_secs(2), ouvrir(&app))
        .await
        .expect("la connexion n'attend pas la fin du diagnostic");
    assert!(!state.is_empty());

    let diag = tokio::time::timeout(std::time::Duration::from_secs(15), diag)
        .await
        .expect("le diagnostic est borné")
        .unwrap();
    assert!(
        debut.elapsed() < std::time::Duration::from_secs(12),
        "sonde bornée à 5 s : {:?}",
        debut.elapsed()
    );
    assert_eq!(diag["client_id"]["spotify"], "non_verifie", "{diag}");
}
