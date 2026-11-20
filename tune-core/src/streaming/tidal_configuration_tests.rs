//! TIDAL : identifiants client en configuration, pays de lecture, extraits
//! sur la voie v1, journal sans secret. Un faux TIDAL sur la boucle locale
//! sert les points d'accès ; identifiants factices seulement.
use std::sync::Mutex as StdMutex;

use base64::Engine as _;

use super::*;
use crate::streaming::traits::StreamingService;

// ---------------------------------------------------------------------------
// Capture du journal
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Journal(Arc<StdMutex<Vec<u8>>>);
impl std::io::Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
impl Journal {
    fn texte(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn capturer() -> (Journal, tracing::subscriber::DefaultGuard) {
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    (journal.clone(), tracing::subscriber::set_default(abonne))
}

// ---------------------------------------------------------------------------
// Faux TIDAL
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Notes {
    /// `countryCode` de chaque requête, préfixé de la route.
    pays: Vec<String>,
    /// Champs du dernier formulaire OAuth reçu.
    formulaire: HashMap<String, String>,
}

type Etat = (Arc<StdMutex<Notes>>, Arc<serde_json::Value>);

/// `v1` : la réponse `playbackinfopostpaywall` servie à toutes les qualités.
/// `trackManifests` (v2) répond toujours 404 : la v1 décide.
async fn faux_tidal(v1: serde_json::Value) -> (TidalService, Arc<StdMutex<Notes>>) {
    use axum::extract::{Form, Query, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};

    async fn moi(
        State((notes, _)): State<Etat>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::Json<serde_json::Value> {
        let pays = q.get("countryCode").cloned().unwrap_or_default();
        notes.lock().unwrap().pays.push(format!("moi:{pays}"));
        axum::Json(json!({"username": "temoin", "countryCode": "FR"}))
    }
    async fn lecture(
        State((notes, v1)): State<Etat>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::Json<serde_json::Value> {
        let pays = q.get("countryCode").cloned().unwrap_or_default();
        notes.lock().unwrap().pays.push(format!("lecture:{pays}"));
        axum::Json((*v1).clone())
    }
    async fn manifeste() -> axum::response::Response {
        (StatusCode::NOT_FOUND, axum::Json(json!({"errors": []}))).into_response()
    }
    async fn appareil(
        State((notes, _)): State<Etat>,
        Form(f): Form<HashMap<String, String>>,
    ) -> axum::Json<serde_json::Value> {
        notes.lock().unwrap().formulaire = f;
        axum::Json(json!({
            "deviceCode": "code-appareil-temoin",
            "userCode": "SAISIE-TEMOIN",
            "verificationUri": "link.tidal.com",
            "verificationUriComplete": "link.tidal.com/SAISIE-TEMOIN",
            "expiresIn": 300,
            "interval": 2
        }))
    }
    async fn jeton(
        State((notes, _)): State<Etat>,
        Form(f): Form<HashMap<String, String>>,
    ) -> axum::response::Response {
        notes.lock().unwrap().formulaire = f;
        (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "status": 400,
                "error": "authorization_pending",
                "sub_status": 1002,
                "error_description": "client-id-factice code-appareil-temoin"
            })),
        )
            .into_response()
    }

    let notes: Arc<StdMutex<Notes>> = Arc::default();
    let app = axum::Router::new()
        .route("/users/me", get(moi))
        .route("/tracks/{id}/playbackinfopostpaywall", get(lecture))
        .route("/trackManifests/{id}", get(manifeste))
        .route("/device_authorization", post(appareil))
        .route("/token", post(jeton))
        .with_state((notes.clone(), Arc::new(v1)));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let base = format!("http://{adresse}");
    let mut svc = TidalService::pour_un_faux_tidal(base.clone());
    svc.auth_forcee = Some(base);
    (svc, notes)
}

/// Une réponse v1 jouable (FLAC, manifeste BTS), entière ou extrait.
fn v1(presentation: &str) -> serde_json::Value {
    let bts = json!({
        "mimeType": "audio/flac",
        "codecs": "flac",
        "encryptionType": "NONE",
        "urls": ["https://cdn.essai.invalid/v1/flac-16-44.flac"]
    });
    json!({
        "trackId": 1,
        "assetPresentation": presentation,
        "audioMode": "STEREO",
        "audioQuality": "LOSSLESS",
        "bitDepth": 16,
        "sampleRate": 44_100,
        "manifestMimeType": "application/vnd.tidal.bts",
        "manifest": base64::engine::general_purpose::STANDARD.encode(bts.to_string()),
    })
}

fn sans_identifiants(mut svc: TidalService) -> TidalService {
    svc.identifiants = None;
    svc
}

// ---------------------------------------------------------------------------
// 1. Identifiants client : configuration, compilation, « non configuré »
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sans_identifiants_la_connexion_est_refusee_clairement() {
    let (svc, notes) = faux_tidal(v1("FULL")).await;
    let mut svc = sans_identifiants(svc);
    // Un service connecté par des jetons restaurés ne doit pas être court-
    // circuité par le test : on part d'un service sans jeton.
    svc.tokens = Mutex::new(TokenState {
        access_token: None,
        refresh_token: None,
        token_expires: None,
    });
    assert!(!svc.est_configure());

    for demande in [json!({}), json!({"device_flow": true})] {
        match svc.authenticate(&demande).await {
            Err(TuneError::Refused { code, message }) => {
                assert_eq!(code, CODE_NON_CONFIGURE);
                assert_eq!(message, NON_CONFIGURE);
            }
            autre => panic!("{demande} : refus attendu, reçu {autre:?}"),
        }
    }
    // Rien n'est parti vers TIDAL.
    assert!(notes.lock().unwrap().formulaire.is_empty());

    // Un simple sondage répond l'état, sans erreur.
    let etat = svc.authenticate(&json!({"poll": true})).await.unwrap();
    assert!(!etat.authenticated);
}

#[tokio::test]
async fn sans_identifiants_les_jetons_en_place_restent_intacts() {
    let (svc, _) = faux_tidal(v1("FULL")).await;
    let svc = sans_identifiants(svc);
    {
        let mut ts = svc.tokens.lock().await;
        ts.refresh_token = Some("rafraichissement-temoin".into());
    }
    let erreur = svc.do_refresh_token().await.unwrap_err();
    assert_eq!(erreur, NON_CONFIGURE);
    let ts = svc.tokens.lock().await;
    assert_eq!(ts.access_token.as_deref(), Some("jeton-essai"));
    assert_eq!(ts.refresh_token.as_deref(), Some("rafraichissement-temoin"));
}

/// Contre-épreuve : configuré, le code d'appareil part avec l'identifiant de
/// la configuration ; ni lui, ni le code d'appareil, ni le code à saisir ne
/// passent au journal.
#[tokio::test]
async fn configure_le_code_appareil_part_avec_l_identifiant_sans_le_journaliser() {
    let (journal, _garde) = capturer();
    let (mut svc, notes) = faux_tidal(v1("FULL")).await;
    svc.tokens = Mutex::new(TokenState {
        access_token: None,
        refresh_token: None,
        token_expires: None,
    });
    assert!(svc.est_configure());

    let etat = svc
        .authenticate(&json!({"device_flow": true}))
        .await
        .expect("code d'appareil");
    assert_eq!(etat.user_code.as_deref(), Some("SAISIE-TEMOIN"));
    assert_eq!(
        notes
            .lock()
            .unwrap()
            .formulaire
            .get("client_id")
            .map(String::as_str),
        Some("client-id-factice")
    );

    // Sondage : TIDAL répond « en attente », avec un corps bavard.
    let etat = svc.authenticate(&json!({"poll": true})).await.unwrap();
    assert!(!etat.authenticated);
    assert_eq!(
        notes
            .lock()
            .unwrap()
            .formulaire
            .get("client_secret")
            .map(String::as_str),
        Some("client-secret-factice")
    );

    // Démarrage PKCE : l'adresse rendue porte l'identifiant, le journal non.
    svc.pending_device_auth = None;
    let etat = svc.authenticate(&json!({})).await.unwrap();
    assert!(
        etat.verification_url
            .as_deref()
            .is_some_and(|u| u.contains("client_id=client-id-factice"))
    );

    let texte = journal.texte();
    assert!(
        texte.contains("tidal_device_auth_started"),
        "capture : {texte}"
    );
    assert!(
        texte.contains("tidal_pkce_auth_started"),
        "capture : {texte}"
    );
    assert!(
        texte.contains("authorization_pending"),
        "code d'erreur : {texte}"
    );
    for secret in [
        "client-id-factice",
        "client-secret-factice",
        "code-appareil-temoin",
        "SAISIE-TEMOIN",
    ] {
        assert!(
            !texte.contains(secret),
            "« {secret} » au journal :\n{texte}"
        );
    }
}

#[test]
fn le_service_lit_la_configuration_avant_la_compilation() {
    let svc = TidalService::new().avec_identifiants(Some("id-conf"), Some("secret-conf"));
    let i = svc.identifiants.as_ref().expect("configuré");
    assert_eq!(
        (i.id(), i.secret(), i.origine),
        ("id-conf", "secret-conf", "configuration")
    );
}

// ---------------------------------------------------------------------------
// 2. Aucune valeur d'identifiant dans les sources
// ---------------------------------------------------------------------------

#[test]
fn aucune_constante_d_identifiant_client_dans_les_sources() {
    for (nom, source) in [
        ("tidal.rs", include_str!("tidal.rs")),
        (
            "tidal_manifeste_v2.rs",
            include_str!("tidal_manifeste_v2.rs"),
        ),
    ] {
        for interdit in ["const CLIENT_ID", "const CLIENT_SECRET"] {
            assert!(!source.contains(interdit), "{nom} : « {interdit} »");
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Workflows : chaque build publié reçoit les identifiants
// ---------------------------------------------------------------------------

const RELEASE: &str = include_str!("../../../.github/workflows/release.yml");
const DOCKER: &str = include_str!("../../../.github/workflows/docker.yml");
const CROSS: &str = include_str!("../../../Cross.toml");

fn etapes_de_build_avec_identifiants(nom: &str, workflow: &str) -> usize {
    let lignes: Vec<&str> = workflow.lines().collect();
    let mut vues = 0;
    for (i, ligne) in lignes.iter().enumerate() {
        let t = ligne.trim_start();
        if t.starts_with('#')
            || !(t.contains("cargo build --release --package tune-server")
                || t.contains("cross build --release --package tune-server"))
        {
            continue;
        }
        let debut = (0..i)
            .rev()
            .find(|&j| lignes[j].trim_start().starts_with("- name:"))
            .unwrap_or_else(|| panic!("{nom}:{} : build hors d'une étape nommée", i + 1));
        let etape = lignes[debut..=i].join("\n");
        for var in [ENV_CLIENT_ID, ENV_CLIENT_SECRET] {
            assert!(
                etape.contains(&format!("{var}: ${{{{ secrets.{var} }}}}")),
                "{nom}:{} : l'étape « {} » ne reçoit pas {var}",
                i + 1,
                lignes[debut].trim()
            );
        }
        vues += 1;
    }
    vues
}

#[test]
fn chaque_build_publie_recoit_les_identifiants_tidal() {
    assert_eq!(etapes_de_build_avec_identifiants("release.yml", RELEASE), 4);
    assert_eq!(etapes_de_build_avec_identifiants("docker.yml", DOCKER), 2);
}

#[test]
fn le_conteneur_cross_recoit_les_identifiants_tidal() {
    for cible in ["aarch64-unknown-linux-gnu", "aarch64-unknown-linux-musl"] {
        let entete = format!("[target.{cible}.env]");
        let section = CROSS
            .split(&entete)
            .nth(1)
            .unwrap_or_else(|| panic!("Cross.toml : {entete} absent"));
        let section = section.split("\n[").next().unwrap();
        for var in [ENV_CLIENT_ID, ENV_CLIENT_SECRET] {
            assert!(
                section.contains(&format!("\"{var}\"")),
                "Cross.toml {entete} : {var}"
            );
        }
    }
}

#[test]
fn un_tag_sans_les_secrets_tidal_s_arrete_avant_la_compilation() {
    let etape = RELEASE
        .split("- name: Identifiants client TIDAL présents")
        .nth(1)
        .expect("étape de garde absente de release.yml");
    let etape = etape.split("\n      - ").next().unwrap();
    assert!(etape.contains("exit 1"), "{etape}");
    assert!(etape.contains("!github.event.repository.fork"), "{etape}");
    // Elle précède la première compilation du serveur.
    let garde = RELEASE.find("Identifiants client TIDAL présents").unwrap();
    let premier_build = RELEASE
        .find("build --release --package tune-server")
        .unwrap();
    assert!(garde < premier_build);
    // docker.yml : facultatifs (appel réutilisable).
    for var in [ENV_CLIENT_ID, ENV_CLIENT_SECRET] {
        let apres = DOCKER
            .split(&format!("      {var}:\n"))
            .nth(1)
            .unwrap_or_else(|| panic!("docker.yml : {var} non déclaré"));
        assert!(apres.trim_start().starts_with("required: false"));
    }
}

/// docker.yml lancé sans les secrets (à la main, ou depuis un fork) : un
/// avertissement visible avant la compilation, sans jamais afficher les
/// valeurs.
#[test]
fn docker_sans_les_secrets_tidal_avertit_avant_la_compilation() {
    let etape = DOCKER
        .split("- name: Identifiants client TIDAL (avertissement)")
        .nth(1)
        .expect("étape d'avertissement absente de docker.yml");
    let etape = etape.split("\n      - ").next().unwrap();
    assert!(etape.contains("::warning"), "{etape}");
    assert!(etape.contains("GITHUB_STEP_SUMMARY"), "{etape}");
    // Jamais la valeur : seul le test de vacuité la lit.
    assert!(!etape.contains("echo \"$TUNE_TIDAL"), "{etape}");
    let garde = DOCKER
        .find("Identifiants client TIDAL (avertissement)")
        .unwrap();
    let premier_build = DOCKER
        .find("build --release --package tune-server")
        .unwrap();
    assert!(garde < premier_build);
}

// ---------------------------------------------------------------------------
// 4. Pays : navigation au choix, lecture au pays du compte
// ---------------------------------------------------------------------------

#[tokio::test]
async fn la_lecture_part_avec_le_pays_du_compte_la_navigation_avec_le_choix() {
    let (mut svc, notes) = faux_tidal(v1("FULL")).await;
    svc.refresh_user_info().await; // compte : FR
    svc.choisir_le_pays_du_catalogue(Some("PL")).unwrap();

    svc.api_get("/users/me").await.unwrap();
    svc.fetch_playback_info("1", "LOSSLESS").await.unwrap();

    // Contre-épreuve : sans choix, les deux partent avec le pays du compte.
    svc.choisir_le_pays_du_catalogue(None).unwrap();
    svc.api_get("/users/me").await.unwrap();
    svc.fetch_playback_info("1", "LOSSLESS").await.unwrap();

    assert_eq!(
        notes.lock().unwrap().pays.as_slice(),
        ["moi:US", "moi:PL", "lecture:FR", "moi:FR", "lecture:FR"]
    );
}

// ---------------------------------------------------------------------------
// 5. Extraits (`PREVIEW`) sur la voie v1
// ---------------------------------------------------------------------------

#[test]
fn un_extrait_v1_se_reconnait() {
    assert!(est_un_extrait_v1(&v1("PREVIEW")));
    assert!(est_un_extrait_v1(&v1("preview")));
    assert!(!est_un_extrait_v1(&v1("FULL")));
    assert!(!est_un_extrait_v1(&json!({})));
}

#[tokio::test]
async fn un_extrait_v1_n_est_jamais_lu_comme_un_morceau_entier() {
    let (svc, _) = faux_tidal(v1("PREVIEW")).await;
    let erreur = svc
        .get_track_url("1", Some("LOSSLESS"))
        .await
        .expect_err("un extrait ne se lit pas");
    assert_eq!(erreur.to_string(), EXTRAIT_SEULEMENT);
}

/// Contre-épreuve : la même réponse en `FULL` se lit.
#[tokio::test]
async fn un_titre_entier_v1_se_lit() {
    let (svc, _) = faux_tidal(v1("FULL")).await;
    let flux = svc.get_track_url("1", Some("LOSSLESS")).await.unwrap();
    assert_eq!(flux.url, "https://cdn.essai.invalid/v1/flac-16-44.flac");
}

// ---------------------------------------------------------------------------
// 6. Journal : adresses de flux sans leur jeton signé
// ---------------------------------------------------------------------------

#[test]
fn une_adresse_de_flux_perd_sa_requete_au_journal() {
    assert_eq!(
        url_pour_journal("https://sp-ad-fa.audio.tidal.com/mediatracks/abc/0.mp4?token=xyz&info=1"),
        "https://sp-ad-fa.audio.tidal.com/…"
    );
    // Jeton dans le chemin (Akamai) : seul l'hôte reste.
    assert_eq!(
        url_pour_journal("https://ak.cdn.invalid/hdntl=exp=1~hmac=SECRET/seg.mp4"),
        "https://ak.cdn.invalid/…"
    );
    assert_eq!(url_pour_journal("file:///tmp/a.mp4"), "file:///tmp/a.mp4");
}

/// Les refus OAuth ne journalisent que leur code ([`erreur_oauth`]), jamais
/// le corps ; ni le code d'appareil, ni le code à saisir, ni l'adresse
/// d'autorisation ne passent au journal.
#[test]
fn aucun_corps_de_reponse_oauth_ni_code_d_appareil_au_journal() {
    let source = include_str!("tidal.rs");
    for evenement in [
        "\"tidal_pkce_token_exchange_failed\"",
        "\"tidal_refresh_token_rejected\"",
        "\"tidal_device_authorization_failed\"",
        "\"tidal_token_exchange\"",
    ] {
        let ligne = source
            .lines()
            .find(|l| l.contains(evenement))
            .unwrap_or_else(|| panic!("{evenement} introuvable"));
        assert!(!ligne.contains("body"), "{evenement} : {ligne}");
        assert!(
            ligne.contains("erreur_oauth") || ligne.contains("erreur = %erreur"),
            "{ligne}"
        );
    }
    for interdit in ["device_code = ?", "user_code = %", "url = %authorize_url"] {
        assert!(!source.contains(interdit), "tidal.rs : « {interdit} »");
    }
}
