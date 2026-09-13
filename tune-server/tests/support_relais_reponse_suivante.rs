//! Le relais support sait-il porter le DEUXIÈME message du testeur ? (#3871,
//! suite de #2856)
//!
//! # Ce qui a été mesuré, et de quel côté
//!
//! Deux défauts distincts vivent sur le même mécanisme, et **aucun des deux
//! n'a sa cause dans ce dépôt** :
//!
//! 1. **#2856 — la création.** `MirrorTicketToForum::handle()` ne recevait que
//!    la description du ticket : ni les pièces jointes, ni la fiche système ne
//!    lui étaient passées (`CreateSupportTicket.php:65`). 62 fils miroités sur
//!    62 sans le moindre bloc de diagnostic. **Corrigé côté Laravel**
//!    (site-mozaiklabs, PR #196) : le miroir ANNONCE désormais les pièces
//!    (`attachmentsNotice()`) et republie le cœur de la fiche
//!    (`systemNotice()`) — sans republier un seul fichier, le stockage support
//!    étant privé à dessein. Le côté Tune de cette chaîne est verrouillé par
//!    `support_relais_diagnostic_sortant.rs`.
//!
//! 2. **#3871 — la suite.** Le miroir n'est branché **qu'à la création** :
//!    `AddSupportMessage::handle()` (`app/Actions/Support/AddSupportMessage.php`)
//!    crée le message, range ses pièces jointes, met le ticket à jour — et ne
//!    touche jamais au forum. Mesuré sur `cms_mozaiklabs` le 11/09/2026 :
//!    9 messages de testeurs, sur 5 tickets, restés invisibles du forum.
//!    **La cause est côté Laravel, et rien dans ce dépôt ne peut la corriger.**
//!
//! # Ce que ce fichier verrouille, qui EST de ce dépôt
//!
//! Il reste, sur le chemin de la suite, une moitié qui n'appartient qu'à Tune :
//! **le relais ne sait pas transporter une réponse autrement qu'en texte nu.**
//!
//! - `POST /api/v1/support/tickets/{id}/reply` n'acceptait que
//!   `application/json` avec `{ "body": … }` : tout `multipart/form-data` était
//!   refusé par l'extracteur avant d'atteindre le nuage, et aucune pièce jointe
//!   ne pouvait quitter la machine.
//! - Or mozaiklabs les attend : `ReplySupportTicketRequest` valide
//!   `attachments[]` (5 fichiers, 50 Mo, mêmes extensions qu'à la création) et
//!   `SupportTicketController::reply` les passe à `AddSupportMessage`, qui les
//!   range avec le message.
//!
//! Autrement dit : le PREMIER message d'un testeur peut porter son journal et
//! ses captures ; le DEUXIÈME ne pouvait rien porter du tout — pas même
//! jusqu'au ticket, que le SAV lit pourtant. C'est la moitié du dossier qui se
//! joue ici, et c'est exactement le trou que #3871 décrit sur son autre bord.
//!
//! # Comment il le prouve
//!
//! Le nuage est **simulé dans le test** : aucun octet ne part vers
//! mozaiklabs.fr. Le fait de base vérifié n'est jamais un code HTTP, c'est
//! **« l'appel de réponse est émis, à la bonne adresse, avec la pièce »**.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::Path as AxumPath;
use axum::extract::State as AxumState;
use axum::http::{Request, StatusCode, header};
use axum::routing::post;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Le ticket 108 de Dominique COMET — celui dont le message 121, écrit
/// 21 minutes après l'ouverture, n'a jamais traversé.
const TICKET: i64 = 108;
const REPONSE: &str = "/api/v1/support/tickets/108/reply";
const BOUNDARY: &str = "----TuneP3871";

/// La précision qu'il a écrite, mot pour mot (`support_messages` #121).
const CORPS_REPONSE: &str = "Une précision, les Vumètres et le barregraphe fonctionnent sur l'interface habituelle et pa sur la preview version 1.0.";

/// Le fichier qu'il ne pouvait pas joindre à cette réponse.
const PIECE_JOINTE: &str = "vumetres-preview.log";
const PIECE_CONTENU: &str = "WARN meter_bridge preview_v1 no_frames zone=diretta\n";

// ---------------------------------------------------------------------------
// mozaiklabs simulé — la seule route qui nous intéresse : la réponse au ticket
// ---------------------------------------------------------------------------

/// Ce que le nuage simulé a vu passer.
#[derive(Default)]
struct Journal {
    appels: Vec<Appel>,
}

#[derive(Clone, Debug)]
struct Appel {
    chemin: String,
    content_type: String,
    cle_licence: Option<String>,
    corps: String,
}

type Partage = Arc<Mutex<Journal>>;

/// `POST /api/v1/support/tickets/{id}/reply` du nuage simulé. Il joue
/// `SupportTicketController::reply` : il consigne le corps reçu et répond
/// proprement 201, comme Laravel.
async fn repondre(
    AxumState(journal): AxumState<Partage>,
    AxumPath(id): AxumPath<i64>,
    req: Request<Body>,
) -> impl axum::response::IntoResponse {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let cle_licence = req
        .headers()
        .get("X-License-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let octets = axum::body::to_bytes(req.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap_or_default();

    journal.lock().unwrap().appels.push(Appel {
        chemin: format!("/api/v1/support/tickets/{id}/reply"),
        content_type,
        cle_licence,
        corps: String::from_utf8_lossy(&octets).into_owned(),
    });

    (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"reply":{"id":121,"author":"user","body":"x"}}"#,
    )
}

/// Démarre le nuage simulé et rend son adresse de base plus son journal.
/// Le `JoinHandle` est gardé par l'appelant : la tâche vit tant que le test
/// dure, de sorte que chaque réponse est écrite en entier (pas de RST).
async fn nuage_simule() -> (String, Partage, tokio::task::JoinHandle<()>) {
    let journal: Partage = Arc::new(Mutex::new(Journal::default()));
    let app = Router::new()
        // Les DEUX formes exposées par mozaiklabs. Le relais ne doit en viser
        // qu'une, mais on les monte toutes les deux : un test qui n'ouvrirait
        // que la bonne ne saurait pas distinguer « le relais s'est trompé
        // d'adresse » de « le relais n'a rien envoyé ».
        .route("/api/v1/support/tickets/{id}/reply", post(repondre))
        .route("/api/v1/support/tickets/{id}/replies", post(repondre))
        .with_state(journal.clone());

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("port libre");
    let addr = listener.local_addr().unwrap();
    let tache = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), journal, tache)
}

// ---------------------------------------------------------------------------
// Découpage minimal du multipart SORTANT
// ---------------------------------------------------------------------------

/// La frontière de l'envoi sortant, lue dans son `Content-Type` : ce n'est pas
/// celle du corps entrant, `reqwest::multipart::Form` en tire une nouvelle au
/// hasard. Découper sur la mauvaise ne rend aucune section — et un test qui ne
/// trouve rien conclut « le champ manque » alors qu'il est là.
fn frontiere(content_type: &str) -> String {
    content_type
        .split("boundary=")
        .nth(1)
        .map(|b| b.trim().trim_matches('"').to_string())
        .expect("un multipart sortant déclare toujours sa frontière")
}

fn sections<'a>(corps: &'a str, frontiere: &str) -> Vec<&'a str> {
    corps.split(&format!("--{frontiere}")).collect()
}

fn champ_multipart(corps: &str, frontiere: &str, nom: &str) -> Option<String> {
    let marqueur = format!("name=\"{nom}\"");
    sections(corps, frontiere)
        .into_iter()
        .find(|s| s.contains(&marqueur) && !s.contains("filename="))
        .and_then(|s| s.split_once("\r\n\r\n"))
        .map(|(_, v)| v.trim_end_matches("\r\n").to_string())
}

// ---------------------------------------------------------------------------
// Le serveur Tune sous test
// ---------------------------------------------------------------------------

fn etat_avec(base: Option<&str>, licence: bool) -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Some(b) = base {
        settings.set("mozaik_base_url", b).unwrap();
    }
    if licence {
        settings.set("license_key", "TUNE-P1-3871").unwrap();
        settings
            .set("hardware_fingerprint", "empreinte-de-test")
            .unwrap();
    }
    state
}

/// Le multipart qu'un client compose pour une réponse AVEC pièce jointe :
/// `body`, puis le fichier sous `attachments[]` — exactement la forme que
/// `ReplySupportTicketRequest` valide côté mozaiklabs.
fn corps_multipart(nom_fichier: &str) -> String {
    let mut s = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"body\"\r\n\r\n{CORPS_REPONSE}\r\n"
    );
    s.push_str(&format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"attachments[]\"; filename=\"{nom_fichier}\"\r\nContent-Type: text/plain\r\n\r\n{PIECE_CONTENU}\r\n"
    ));
    s.push_str(&format!("--{BOUNDARY}--\r\n"));
    s
}

async fn repondre_au_ticket(state: &AppState, content_type: &str, corps: String) -> StatusCode {
    let app: Router = tune_server::routes::router(state.clone());
    app.oneshot(
        Request::post(REPONSE)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(corps))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

fn multipart_ct() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

// ---------------------------------------------------------------------------
// Les épreuves
// ---------------------------------------------------------------------------

/// **Le fait de base.** Le deuxième message du testeur part avec sa pièce
/// jointe, à l'adresse `…/{id}/reply`, sous le nom que Laravel attend.
///
/// Avant : `reply` n'acceptait que du JSON. Le multipart repartait en **415
/// Unsupported Media Type**, posé par l'extracteur `Json<ReplyBody>` ; le nuage
/// ne voyait **aucun appel**, et le testeur n'avait aucun moyen de joindre quoi
/// que ce soit à une réponse — pas même au ticket que le SAV lit.
#[tokio::test]
async fn une_reponse_peut_porter_le_fichier_du_testeur() {
    let (base, journal, _tache) = nuage_simule().await;
    let state = etat_avec(Some(&base), true);

    let status = repondre_au_ticket(&state, &multipart_ct(), corps_multipart(PIECE_JOINTE)).await;

    let j = journal.lock().unwrap();
    assert_eq!(
        j.appels.len(),
        1,
        "le nuage simulé n'a reçu aucune réponse (statut rendu au client : {status}) — \
         le deuxième message du testeur n'a pas quitté la machine"
    );
    let appel = &j.appels[0];

    // 1. La bonne adresse. `…/reply` et `…/replies` existent tous deux côté
    //    mozaiklabs ; se tromper rendrait un 404 propagé tel quel.
    assert_eq!(
        appel.chemin,
        format!("/api/v1/support/tickets/{TICKET}/reply"),
        "la réponse ne part pas sur la route de réponse du ticket"
    );

    // 2. L'appel sortant est bien un téléversement.
    assert!(
        appel.content_type.starts_with("multipart/form-data"),
        "la réponse ne part pas en multipart : {}",
        appel.content_type
    );

    // 3. La pièce jointe est DANS l'appel sortant, sous le nom attendu par
    //    Laravel (`attachments.*`), avec son contenu.
    assert!(
        appel.corps.contains("name=\"attachments[]\""),
        "le fichier ne part pas sous le nom attendu par Laravel (attachments[])"
    );
    assert!(
        appel
            .corps
            .contains(&format!("filename=\"{PIECE_JOINTE}\"")),
        "aucune partie « attachments[] » nommée {PIECE_JOINTE} dans l'envoi"
    );
    assert!(
        appel.corps.contains(PIECE_CONTENU.trim()),
        "le contenu du fichier ne part pas avec l'appel"
    );

    // 4. Le texte du testeur traverse intact — accents compris : c'est LUI que
    //    la ronde de tri n'a jamais lu.
    let corps = champ_multipart(&appel.corps, &frontiere(&appel.content_type), "body")
        .expect("le champ « body » doit partir");
    assert_eq!(
        corps, CORPS_REPONSE,
        "le corps de la réponse est altéré en route"
    );
    assert!(
        corps.contains("Vumètres") && corps.contains("précision"),
        "encodage cassé sur le corps de la réponse : {corps}"
    );

    // 5. L'identité voyage toujours.
    assert_eq!(appel.cle_licence.as_deref(), Some("TUNE-P1-3871"));
}

/// **Non-régression** — vert avant comme après. Le chemin JSON historique, le
/// seul que les clients déployés connaissent, ne doit pas être déplacé par
/// l'ajout du multipart.
#[tokio::test]
async fn la_reponse_json_historique_traverse_toujours() {
    let (base, journal, _tache) = nuage_simule().await;
    let state = etat_avec(Some(&base), true);

    let status = repondre_au_ticket(
        &state,
        "application/json",
        serde_json::json!({ "body": CORPS_REPONSE }).to_string(),
    )
    .await;

    let j = journal.lock().unwrap();
    assert_eq!(
        j.appels.len(),
        1,
        "le nuage simulé n'a rien reçu (statut rendu au client : {status})"
    );
    let appel = &j.appels[0];
    assert_eq!(
        appel.chemin,
        format!("/api/v1/support/tickets/{TICKET}/reply")
    );

    let recu: serde_json::Value = serde_json::from_str(&appel.corps).expect("un corps JSON");
    assert_eq!(recu["body"], CORPS_REPONSE);
}

/// **Témoin d'auth.** Sans clé de licence ni jeton, la réponse est refusée par
/// 412 **avant toute sortie réseau** : sans lui, les épreuves ci-dessus
/// pourraient passer sur un montage qui appelle le nuage à tort et à travers.
///
/// Rouge avant le correctif, mais pour une AUTRE raison, et c'est elle qui
/// nomme le défaut : le relais rendait **415 Unsupported Media Type**.
/// L'extracteur `Json<ReplyBody>` refusait le multipart avant même que le garde
/// d'auth ne soit consulté — la requête n'atteignait jamais le corps du
/// gestionnaire.
#[tokio::test]
async fn sans_identifiants_aucune_reponse_ne_sort() {
    let (base, journal, _tache) = nuage_simule().await;
    let state = etat_avec(Some(&base), false);

    let status = repondre_au_ticket(&state, &multipart_ct(), corps_multipart(PIECE_JOINTE)).await;

    assert_eq!(
        status,
        StatusCode::PRECONDITION_FAILED,
        "le garde d'auth doit refuser avant le réseau"
    );
    assert!(
        journal.lock().unwrap().appels.is_empty(),
        "une requête est sortie alors qu'aucune identité n'était disponible"
    );
}

/// **Contre-épreuve du filtre.** Une extension hors liste blanche est refusée
/// **avant** que le moindre octet ne parte — même liste, même refus qu'à la
/// création. Sans ce témoin, un relais qui laisserait tout passer rendrait
/// l'épreuve principale verte sans rien garder.
///
/// Rouge avant, en 415 comme les autres : le filtre n'existait pas sur ce
/// chemin, faute de chemin.
#[tokio::test]
async fn une_piece_interdite_ne_quitte_pas_la_machine() {
    let (base, journal, _tache) = nuage_simule().await;
    let state = etat_avec(Some(&base), true);

    let status = repondre_au_ticket(&state, &multipart_ct(), corps_multipart("charge.exe")).await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "une pièce jointe .exe doit être refusée par le relais"
    );
    assert!(
        journal.lock().unwrap().appels.is_empty(),
        "un fichier refusé est tout de même parti vers le nuage"
    );
}

/// **Contre-épreuve du corps vide.** Une réponse sans texte est refusée ici :
/// `ReplySupportTicketRequest` la rejetterait en 422, et un aller-retour réseau
/// pour apprendre une règle qu'on connaît ne sert personne. Rouge avant, en 415.
#[tokio::test]
async fn une_reponse_sans_texte_est_refusee_avant_le_reseau() {
    let (base, journal, _tache) = nuage_simule().await;
    let state = etat_avec(Some(&base), true);

    let corps = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"body\"\r\n\r\n   \r\n\
         --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"attachments[]\"; filename=\"{PIECE_JOINTE}\"\r\nContent-Type: text/plain\r\n\r\n{PIECE_CONTENU}\r\n\
         --{BOUNDARY}--\r\n"
    );

    let status = repondre_au_ticket(&state, &multipart_ct(), corps).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        journal.lock().unwrap().appels.is_empty(),
        "une réponse vide est partie vers le nuage"
    );
}
