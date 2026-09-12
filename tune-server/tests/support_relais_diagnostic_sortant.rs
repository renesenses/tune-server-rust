//! Le relais support émet-il réellement le diagnostic et les pièces jointes ?
//! (#2856)
//!
//! # Ce qui se perdait, et où
//!
//! Un testeur ouvre un ticket depuis Tune, coche « joindre les journaux » et
//! attache son fichier. Le fil publié sur le forum — le seul endroit que la
//! ronde de tri regarde — n'en portait rien : 62 fils miroités sur 62 sans le
//! moindre bloc de diagnostic, du 07/08/2026 au 30/08/2026.
//!
//! Le miroir lui-même est côté mozaiklabs
//! (`app/Actions/Support/MirrorTicketToForum.php`) : il republie le seul corps
//! du ticket. Il sait désormais ANNONCER les pièces jointes du ticket — nombre,
//! nom, taille, type — sans en republier une seule, le stockage support étant
//! privé à dessein. Mais cette annonce ne peut nommer que ce que **Tune a
//! réellement livré**.
//!
//! # Ce que ce test verrouille, et qui n'existait pas
//!
//! Rien, dans ce dépôt, ne prouvait que `logs` et `attachments[]` quittaient la
//! machine. L'adresse du relais était une **constante en dur**
//! (`https://mozaiklabs.fr/api/v1/support/tickets`), alors que toutes les autres
//! portes du nuage lisent le réglage `mozaik_base_url` : la requête sortante
//! était donc inobservable, et le seul témoignage disponible était le 201 de
//! mozaiklabs — un « publié » qui ne dit rien de ce qui a été attaché. C'est
//! ainsi que le diagnostic a disparu de 39 tickets sur 62 pendant vingt-trois
//! jours sans une ligne rouge (#2916).
//!
//! Ici, le nuage ET le forum sont **simulés dans le test** : aucun octet ne part
//! vers mozaiklabs.fr, et le serveur simulé répond proprement (jamais de
//! coupure brutale, qui rendrait la suite intermittente).
//!
//! Le fait de base vérifié n'est jamais un code HTTP : c'est
//! **« l'appel de téléversement est émis avec la pièce »**, puis
//! **« le corps publié sur le forum annonce le diagnostic »**.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State as AxumState;
use axum::http::{Request, StatusCode, header};
use axum::routing::post;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const TICKETS: &str = "/api/v1/support/tickets";
const BOUNDARY: &str = "----TuneP2a2856";

/// Le diagnostic que `/system/bug-report/markdown` produit et que le client
/// place dans le champ texte `logs`. On garde ses en-têtes réels : ce sont eux
/// que la ronde de tri cherchait dans les 62 fils, et n'a jamais trouvés.
const DIAGNOSTIC: &str = "# Tune Bug Report\n\n**Version**: 0.9.127 (engine: rust)\n\n## Database\n- Engine: sqlite\n\n## Recent Logs\n```\nERROR clap_stall jauge à 0%\n```\n";

/// Le fichier du testeur, mot pour mot le cas du fil 1616.
const PIECE_JOINTE: &str = "tune-sample.txt";
const PIECE_CONTENU: &str = "Call graph:\n  2731 Thread_1a2b   ::  tune-server`clap_render\n";

// ---------------------------------------------------------------------------
// mozaiklabs simulé — API support + miroir forum
// ---------------------------------------------------------------------------

/// Une pièce jointe telle que `CreateSupportTicket` la matérialise : les
/// fichiers reçus, PLUS `diagnostic.md` fabriqué depuis le champ texte `logs`.
#[derive(Clone, Debug)]
struct PieceJointe {
    nom: String,
    taille: usize,
}

/// Ce que le nuage simulé a vu passer.
#[derive(Default)]
struct Journal {
    /// Corps bruts reçus sur `POST /api/v1/support/tickets`.
    envois: Vec<Envoi>,
    /// Corps des fils publiés par le miroir forum simulé.
    fils: Vec<String>,
}

#[derive(Clone, Debug)]
struct Envoi {
    content_type: String,
    cle_licence: Option<String>,
    corps: String,
}

type Partage = Arc<Mutex<Journal>>;

/// Transcription de `MirrorTicketToForum::attachmentsNotice` (site-mozaiklabs,
/// #196) : le fil ANNONCE les pièces du ticket, il n'en republie aucune. Chaîne
/// vide s'il n'y en a pas — c'est le fil muet des 62.
fn annonce_des_pieces(pieces: &[PieceJointe]) -> String {
    if pieces.is_empty() {
        return String::new();
    }
    let mut items = String::new();
    for p in pieces {
        items.push_str(&format!("<li>{} — {} o</li>", p.nom, p.taille));
    }
    format!("\n\n<h4>Pièces jointes du rapport</h4>\n<ul>{items}</ul>")
}

/// `POST /api/v1/support/tickets` du nuage simulé : il joue `CreateSupportTicket`
/// (matérialisation de `logs` en `diagnostic.md`) puis `MirrorTicketToForum`, et
/// répond **proprement** 201.
async fn ouvrir_ticket(
    AxumState(journal): AxumState<Partage>,
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
    let corps = String::from_utf8_lossy(&octets).into_owned();

    // --- CreateSupportTicket : quelles pièces le ticket porte-t-il ? ---
    let mut pieces = Vec::new();
    let (description, logs) = if content_type.starts_with("multipart/form-data") {
        let f = frontiere(&content_type);
        for (nom, contenu) in fichiers_multipart(&corps, &f) {
            let taille = contenu.len();
            pieces.push(PieceJointe { nom, taille });
        }
        (
            champ_multipart(&corps, &f, "body").unwrap_or_default(),
            champ_multipart(&corps, &f, "logs").unwrap_or_default(),
        )
    } else {
        let v: serde_json::Value = serde_json::from_str(&corps).unwrap_or_default();
        let texte = |cle: &str| {
            v.get(cle)
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string()
        };
        (texte("body"), texte("logs"))
    };

    // Le champ texte `logs` devient la pièce jointe `diagnostic.md` — c'est ce
    // que fait `CreateSupportTicket::handle`, et c'est ce qui donne au miroir
    // quelque chose à annoncer.
    if !logs.trim().is_empty() {
        pieces.push(PieceJointe {
            nom: "diagnostic.md".into(),
            taille: logs.len(),
        });
    }

    // --- MirrorTicketToForum : le corps publié sur le forum ---
    let fil = format!(
        "{description}{}\n\n---\n*Signalé depuis Tune (Tune 0.9.127 · macOS)*",
        annonce_des_pieces(&pieces)
    );

    {
        let mut j = journal.lock().unwrap();
        j.envois.push(Envoi {
            content_type,
            cle_licence,
            corps,
        });
        j.fils.push(fil);
    }

    (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"ticket":{"id":1,"subject":"x","status":"open","priority":"high"}}"#,
    )
}

/// Démarre le nuage simulé et rend son adresse de base plus son journal.
/// Le `JoinHandle` est gardé par l'appelant : la tâche vit tant que le test dure,
/// de sorte que chaque réponse est écrite en entier (pas de RST).
async fn nuage_simule() -> (String, Partage, tokio::task::JoinHandle<()>) {
    let journal: Partage = Arc::new(Mutex::new(Journal::default()));
    let app = Router::new()
        .route(TICKETS, post(ouvrir_ticket))
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
// Découpage minimal du multipart reçu (assez pour nommer champs et fichiers)
// ---------------------------------------------------------------------------

/// La frontière de l'envoi SORTANT, lue dans son `Content-Type`.
///
/// Elle n'est pas celle du corps entrant : `reqwest::multipart::Form` en tire
/// une nouvelle au hasard. Découper sur la mauvaise ne rend aucune section — et
/// un test qui ne trouve rien conclut « le champ manque » alors qu'il est là.
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

fn fichiers_multipart(corps: &str, frontiere: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for s in sections(corps, frontiere) {
        let Some(i) = s.find("filename=\"") else {
            continue;
        };
        let reste = &s[i + "filename=\"".len()..];
        let Some(j) = reste.find('"') else { continue };
        let nom = reste[..j].to_string();
        let contenu = s
            .split_once("\r\n\r\n")
            .map(|(_, v)| v.trim_end_matches("\r\n").to_string())
            .unwrap_or_default();
        out.push((nom, contenu));
    }
    out
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
        settings.set("license_key", "TUNE-P2A-2856").unwrap();
        settings
            .set("hardware_fingerprint", "empreinte-de-test")
            .unwrap();
    }
    state
}

/// Corps multipart identique à celui que `SupportView.svelte` compose quand le
/// testeur a ajouté un fichier : `subject`, `body`, `category`, `logs`, `system`,
/// puis `attachments[]`.
fn corps_multipart() -> String {
    let mut s = String::new();
    for (nom, valeur) in [
        ("subject", "CLAP suite"),
        (
            "body",
            "Comme demandé j'ai laissé tourner CLAP. La jauge est restée à 0%. Voici les logs.",
        ),
        ("category", "bug"),
        ("logs", DIAGNOSTIC),
        ("system", r#"{"os":"macos","zones":3}"#),
    ] {
        s.push_str(&format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{nom}\"\r\n\r\n{valeur}\r\n"
        ));
    }
    s.push_str(&format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"attachments[]\"; filename=\"{PIECE_JOINTE}\"\r\nContent-Type: text/plain\r\n\r\n{PIECE_CONTENU}\r\n"
    ));
    s.push_str(&format!("--{BOUNDARY}--\r\n"));
    s
}

async fn ouvrir(state: &AppState, content_type: &str, corps: String) -> StatusCode {
    let app: Router = tune_server::routes::router(state.clone());
    app.oneshot(
        Request::post(TICKETS)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(corps))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

// ---------------------------------------------------------------------------
// Les épreuves
// ---------------------------------------------------------------------------

/// **Le fait de base, chemin multipart** : l'appel de téléversement sort avec la
/// pièce jointe du testeur ET avec son diagnostic — et le fil publié par le
/// miroir les annonce. Avant la couture `mozaik_base_url`, le relais partait vers
/// `https://mozaiklabs.fr` quoi qu'on règle : le nuage simulé ne recevait rien,
/// et le fil n'existait pas.
#[tokio::test]
async fn le_televersement_sort_avec_la_piece_jointe_et_le_diagnostic() {
    let (base, journal, _tache) = nuage_simule().await;
    let state = etat_avec(Some(&base), true);

    let status = ouvrir(
        &state,
        &format!("multipart/form-data; boundary={BOUNDARY}"),
        corps_multipart(),
    )
    .await;

    let j = journal.lock().unwrap();
    assert_eq!(
        j.envois.len(),
        1,
        "le nuage simulé n'a rien reçu (statut rendu au client : {status}) — \
         le relais support est parti ailleurs qu'à l'adresse réglée"
    );
    let envoi = &j.envois[0];

    // 1. L'appel sortant est bien un téléversement multipart.
    assert!(
        envoi.content_type.starts_with("multipart/form-data"),
        "le ticket ne part pas en multipart : {}",
        envoi.content_type
    );

    // 2. La pièce jointe du testeur est bien DANS l'appel sortant.
    assert!(
        envoi
            .corps
            .contains(&format!("filename=\"{PIECE_JOINTE}\"")),
        "aucune partie « attachments[] » nommée {PIECE_JOINTE} dans l'envoi"
    );
    assert!(
        envoi.corps.contains("name=\"attachments[]\""),
        "le fichier ne part pas sous le nom attendu par Laravel (attachments[])"
    );
    assert!(
        envoi.corps.contains(PIECE_CONTENU.trim()),
        "le contenu du fichier ne part pas avec l'appel"
    );

    // 3. Le diagnostic aussi — c'est lui qui devient `diagnostic.md`.
    let logs = champ_multipart(&envoi.corps, &frontiere(&envoi.content_type), "logs")
        .expect("le champ « logs » doit partir");
    assert!(
        logs.contains("## Recent Logs") && logs.contains("## Database"),
        "le champ « logs » ne porte pas le bloc de diagnostic : {logs}"
    );

    // 4. L'identité voyage toujours (la couture ne l'a pas mangée).
    assert_eq!(envoi.cle_licence.as_deref(), Some("TUNE-P2A-2856"));

    // 5. Le fait qui coûtait cher : le fil PUBLIÉ annonce les deux pièces.
    let fil = j.fils.first().expect("un fil miroité");
    assert!(
        fil.contains("Pièces jointes du rapport"),
        "le fil ne dit toujours pas que la matière existe : {fil}"
    );
    assert!(
        fil.contains(PIECE_JOINTE),
        "le fil n'annonce pas le fichier du testeur : {fil}"
    );
    assert!(
        fil.contains("diagnostic.md"),
        "le fil n'annonce pas le diagnostic : {fil}"
    );
    // Les accents survivent au bout de la chaîne (le forum en a déjà reçu de cassés).
    assert!(
        fil.contains("Signalé depuis Tune") && fil.contains("Pièces"),
        "encodage cassé sur le corps publié : {fil}"
    );
}

/// **Le fait de base, chemin JSON** — celui des tickets sans pièce jointe
/// manuelle, 39 sur 62 : `logs` doit partir là aussi, sinon le ticket n'a rien à
/// annoncer et le fil reste muet.
#[tokio::test]
async fn le_chemin_json_emet_aussi_le_diagnostic() {
    let (base, journal, _tache) = nuage_simule().await;
    let state = etat_avec(Some(&base), true);

    let corps = serde_json::json!({
        "subject": "CLAP suite",
        "body": "La jauge reste à 0%.",
        "category": "bug",
        "logs": DIAGNOSTIC,
        "system": { "os": "macos", "zones": 3 },
    })
    .to_string();

    let status = ouvrir(&state, "application/json", corps).await;

    let j = journal.lock().unwrap();
    assert_eq!(
        j.envois.len(),
        1,
        "le nuage simulé n'a rien reçu (statut rendu au client : {status})"
    );

    let recu: serde_json::Value = serde_json::from_str(&j.envois[0].corps).expect("un corps JSON");
    let logs = recu["logs"].as_str().unwrap_or_default();
    assert!(
        logs.contains("## Recent Logs"),
        "le diagnostic ne part pas sur le chemin JSON : {recu}"
    );
    assert_eq!(recu["system"]["os"], "macos");

    let fil = j.fils.first().expect("un fil miroité");
    assert!(
        fil.contains("diagnostic.md"),
        "le fil ne dit pas que le diagnostic est joint : {fil}"
    );
}

/// **Témoin** — vert avant comme après. Sans clé de licence ni jeton, le relais
/// refuse par 412 **avant toute sortie réseau** : le nuage simulé ne voit rien.
/// Sans ce témoin, les épreuves ci-dessus pourraient passer sur un montage qui
/// appelle le nuage à tort et à travers.
#[tokio::test]
async fn sans_identifiants_aucun_octet_ne_sort() {
    let (base, journal, _tache) = nuage_simule().await;
    let state = etat_avec(Some(&base), false);

    let status = ouvrir(
        &state,
        &format!("multipart/form-data; boundary={BOUNDARY}"),
        corps_multipart(),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::PRECONDITION_FAILED,
        "le garde d'auth doit refuser avant le réseau"
    );
    assert!(
        journal.lock().unwrap().envois.is_empty(),
        "une requête est sortie alors qu'aucune identité n'était disponible"
    );
}

/// **Témoin** — vert avant comme après. Le nuage simulé n'a AUCUNE pièce : le
/// miroir ne pose alors aucun intitulé creux. C'est l'état du fil 1616, et la
/// contre-épreuve de l'annonce : si `annonce_des_pieces` écrivait un bloc dans
/// tous les cas, les épreuves ci-dessus passeraient sans rien prouver.
#[test]
fn un_ticket_sans_piece_ne_produit_aucune_annonce() {
    assert_eq!(annonce_des_pieces(&[]), "");
    let avec = annonce_des_pieces(&[PieceJointe {
        nom: "diagnostic.md".into(),
        taille: 12,
    }]);
    assert!(avec.contains("Pièces jointes du rapport") && avec.contains("diagnostic.md"));
}
