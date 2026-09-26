//! Le rapport de bogue, le journal exporté et le ticket de support sortent-ils
//! SANS donnée personnelle ? (#5124)
//!
//! ## Le constat
//!
//! Le rapport de bogue part sur le forum public. Sa section « Network » liste
//! les serveurs multimédia sous le nom qu'ils ANNONCENT, et ce nom peut porter
//! une adresse électronique. Les répertoires musicaux portent le nom du compte
//! (`/Users/<nom>`), et le journal joint peut porter la réponse d'un échange de
//! jeton (Tidal l'écrivait en entier au niveau INFO).
//!
//! ## Pourquoi par les ROUTES MONTÉES
//!
//! Les épreuves unitaires de `tune_core::confidentialite` prouvent que la
//! fonction nettoie. Elles restent vertes si personne ne l'appelle — « écrit
//! mais pas branché ». On lit donc ce qui SORT : le markdown, le JSON du
//! rapport, l'export des journaux, et les deux envois vers mozaiklabs
//! (forum et ticket), captés par un nuage simulé. Aucun octet ne part vers
//! mozaiklabs.fr.
//!
//! ## Une seule épreuve, pas cinq
//!
//! Le chemin du journal se règle par `TUNE_LOG_FILE`, une variable du
//! PROCESSUS. La poser pendant qu'une autre épreuve du même binaire lit
//! l'environnement serait une course ; tout se joue donc dans une épreuve, en
//! séquence.
//!
//! `autotests = false` dans `tune-server/Cargo.toml` : ce fichier a sa cible
//! `[[test]]`, sans quoi il ne serait jamais compilé.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State as AxumState;
use axum::http::{Request, header};
use axum::routing::post;
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Ce qui ne doit JAMAIS sortir. Chaque valeur est improbable, pour qu'un
/// `contains` la trouve à coup sûr si elle fuyait.
const FUITES: &[&str] = &[
    "jean.dupont5124",
    "exemple-5124.fr",
    "jdupont5124",
    "eyJhbGciOiJIUzI1NiJ9",
    "rt-SECRET-5124",
    "UAT-SECRET-5124",
    "0f1e2d3c4b5a5124",
    "3C:4D:5E",
];

/// Ce qui DOIT rester : le diagnostic. Une sortie vide passerait sinon
/// l'épreuve des fuites sans rien prouver.
const UTILES_RAPPORT: &[&str] = &[
    "MinimServer",
    "192.168.1.20:9790",
    "Serveurs multimedia: 1",
    "~/Musique",
    "tidal_token_exchange_success",
    "00:1A:2B",
];

/// Le journal du testeur, au format `tracing` que le serveur écrit.
const JOURNAL: &str = concat!(
    "2026-09-26T10:00:00.000Z  INFO tune_core::streaming::tidal: tidal_token_exchange_success body={\"access_token\":\"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOjF9.c2ln\",\"refresh_token\":\"rt-SECRET-5124\",\"user\":{\"email\":\"jean.dupont5124@exemple-5124.fr\"}}\n",
    "2026-09-26T10:00:01.000Z  INFO tune_core::streaming::qobuz: qobuz_get_file_url track_id=42 sig=0f1e2d3c4b5a5124\n",
    "2026-09-26T10:00:02.000Z  WARN tune_core::http: GET https://www.qobuz.com/api.json/0.2/file?user_auth_token=UAT-SECRET-5124&format_id=27\n",
    "2026-09-26T10:00:03.000Z  INFO tune_core::scanner: scan_dir path=/Users/jdupont5124/Musique/Album\n",
    "2026-09-26T10:00:04.000Z  INFO tune_core::discovery: renderer mac=00:1A:2B:3C:4D:5E\n",
);

#[derive(Default)]
struct Captes {
    forum: Vec<Value>,
    tickets: Vec<Value>,
}
type Partage = Arc<Mutex<Captes>>;

async fn capter_forum(
    AxumState(c): AxumState<Partage>,
    axum::Json(corps): axum::Json<Value>,
) -> axum::Json<Value> {
    c.lock().unwrap().forum.push(corps);
    axum::Json(
        json!({ "status": "ok", "thread": { "id": 1, "slug": "s", "url": "http://forum/s" } }),
    )
}

async fn capter_ticket(
    AxumState(c): AxumState<Partage>,
    axum::Json(corps): axum::Json<Value>,
) -> (axum::http::StatusCode, axum::Json<Value>) {
    c.lock().unwrap().tickets.push(corps);
    (
        axum::http::StatusCode::CREATED,
        axum::Json(json!({ "id": 1 })),
    )
}

async fn nuage_simule() -> (String, Partage, tokio::task::JoinHandle<()>) {
    let captes: Partage = Arc::default();
    let app = Router::new()
        .route("/api/v1/community/bug-report", post(capter_forum))
        .route("/api/v1/support/tickets", post(capter_ticket))
        .with_state(captes.clone());
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("port libre");
    let addr = listener.local_addr().unwrap();
    let tache = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), captes, tache)
}

async fn etat(base: &str) -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite");
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("mozaik_base_url", base).unwrap();
    settings.set("license_key", "TUNE-5124").unwrap();
    settings
        .set("hardware_fingerprint", "empreinte-5124")
        .unwrap();
    settings
        .set("music_dirs", r#"["/Users/jdupont5124/Musique"]"#)
        .unwrap();
    state.media_servers.lock().await.insert(
        "uuid:minim".into(),
        tune_core::discovery::ssdp::MediaServerInfo {
            id: "uuid:minim".into(),
            // Le cas du constat : l'adresse est DANS le nom annoncé.
            name: "MinimServer [jean.dupont5124@exemple-5124.fr]".into(),
            manufacturer: "MinimWorld".into(),
            model: "MinimServer".into(),
            location: "http://192.168.1.20:9790/desc.xml".into(),
            content_directory_url: "http://192.168.1.20:9790/cd".into(),
            host: "192.168.1.20".into(),
            port: 9790,
            last_seen: std::time::Instant::now(),
            max_age: std::time::Duration::from_secs(1800),
        },
    );
    state
}

async fn appel(state: &AppState, req: Request<Body>) -> String {
    let app: Router = tune_server::routes::router(state.clone());
    let route = req.uri().to_string();
    let reponse = app.oneshot(req).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).into_owned();
    assert!(statut.is_success(), "{route} → {statut} : {texte}");
    texte
}

async fn lire(state: &AppState, route: &str) -> String {
    appel(state, Request::get(route).body(Body::empty()).unwrap()).await
}

async fn poster(state: &AppState, route: &str, corps: Value) -> String {
    appel(
        state,
        Request::post(route)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

/// Les fuites d'une sortie, nommées pour que l'échec dise le coupable.
fn fuites(texte: &str) -> Vec<&'static str> {
    FUITES
        .iter()
        .copied()
        .filter(|f| texte.contains(f))
        .collect()
}

fn exiger_propre(sortie: &str, texte: &str) {
    let f = fuites(texte);
    assert!(
        f.is_empty(),
        "#5124 — {sortie} laisse sortir {f:?} :\n{texte}"
    );
}

fn exiger_utiles(sortie: &str, texte: &str, utiles: &[&str]) {
    for u in utiles {
        assert!(
            texte.contains(u),
            "{sortie} a perdu « {u} », utile au diagnostic :\n{texte}"
        );
    }
}

/// Le détecteur sait rougir : sans cette garde, un `fuites` cassé rendrait
/// l'épreuve principale verte contre rien.
#[test]
fn le_detecteur_de_fuite_rougit_sur_le_journal_brut() {
    assert_eq!(
        fuites(&format!(
            "{JOURNAL} MinimServer [jean.dupont5124@exemple-5124.fr]"
        ))
        .len(),
        FUITES.len(),
        "chaque fuite fabriquée doit être détectée"
    );
}

#[tokio::test]
async fn aucune_sortie_ne_porte_de_donnee_personnelle() {
    // `TempDir` et non un chemin composé à la main : il se supprime seul,
    // même si l'épreuve tombe (#3030).
    let dossier = tempfile::TempDir::new().unwrap();
    let journal = dossier.path().join("tune-server.log");
    std::fs::write(&journal, JOURNAL).unwrap();
    // SAFETY : seule épreuve asynchrone du binaire ; l'autre n'est qu'un calcul
    // pur qui ne lit pas l'environnement.
    unsafe { std::env::set_var("TUNE_LOG_FILE", &journal) };

    let (base, captes, _tache) = nuage_simule().await;
    let state = etat(&base).await;

    // 1. Le markdown : l'aperçu ET le `logs` d'un ticket.
    let md = lire(&state, "/api/v1/system/bug-report/markdown").await;
    exiger_propre("le markdown du rapport", &md);
    exiger_utiles("le markdown du rapport", &md, UTILES_RAPPORT);
    assert!(
        md.contains(tune_core::version()),
        "la version a disparu :\n{md}"
    );

    // 2. Le JSON du rapport, en entier.
    let rapport = lire(&state, "/api/v1/system/bug-report").await;
    exiger_propre("le JSON du rapport", &rapport);
    let rapport: Value = serde_json::from_str(&rapport).unwrap();
    assert_eq!(rapport["version"], tune_core::version());
    assert_eq!(rapport["library"]["music_dirs"][0], "~/Musique");

    // 3. « Exporter les journaux ».
    let logs = lire(&state, "/api/v1/system/logs?lines=100").await;
    exiger_propre("l'export des journaux", &logs);
    exiger_utiles(
        "l'export des journaux",
        &logs,
        &[
            "tidal_token_exchange_success",
            "track_id=42",
            "format_id=27",
            "~/Musique/Album",
        ],
    );

    // 4. Le fil de forum, tel que le site le reçoit.
    poster(
        &state,
        "/api/v1/system/bug-report/submit",
        json!({ "description": "La zone Salon coupe" }),
    )
    .await;
    let forum = captes
        .lock()
        .unwrap()
        .forum
        .pop()
        .expect("aucun envoi au forum");
    let corps_forum = forum["body"].as_str().unwrap_or_default().to_string();
    exiger_propre("le fil de forum", &forum.to_string());
    exiger_utiles("le fil de forum", &corps_forum, UTILES_RAPPORT);
    assert!(
        corps_forum.starts_with("La zone Salon coupe"),
        "{corps_forum}"
    );

    // 5. Le ticket de support : la fiche et le journal composés par le CLIENT.
    let fiche = json!({
        "server": { "version": "0.9.165", "os": "macos" },
        "library": { "music_dirs": ["/Users/jdupont5124/Musique"] },
        "network": { "media_servers": [
            { "name": "MinimServer [jean.dupont5124@exemple-5124.fr]", "host": "192.168.1.20", "port": 9790 }
        ]},
    });
    poster(
        &state,
        "/api/v1/support/tickets",
        json!({ "subject": "Coupure", "body": "Voir fiche", "system": fiche, "logs": JOURNAL }),
    )
    .await;
    let ticket = captes
        .lock()
        .unwrap()
        .tickets
        .pop()
        .expect("aucun ticket relayé");
    exiger_propre("le ticket de support", &ticket.to_string());
    assert_eq!(
        ticket["system"]["network"]["media_servers"][0]["host"],
        "192.168.1.20"
    );
    assert_eq!(ticket["system"]["server"]["version"], "0.9.165");
    assert!(
        ticket["system"]["network"]["media_servers"][0]["name"]
            .as_str()
            .is_some_and(|n| n.starts_with("MinimServer")),
        "{ticket}"
    );
}
