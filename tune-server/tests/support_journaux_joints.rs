//! Les journaux cochés dans l'écran Support atteignent-ils vraiment le relais ? (#2916)
//!
//! ## Le défaut
//!
//! Dans l'écran Support, « joindre les journaux » est cochée **par défaut**. Le
//! client web pose alors `logs` (le markdown de `/system/bug-report/markdown`)
//! et `system` dans le corps qu'il envoie à `POST /support/tickets` — sur les
//! DEUX chemins, avec pièce jointe (multipart) comme sans (JSON).
//!
//! Le chemin JSON les jetait : `CreateBody` ne déclarait que `subject`, `body`
//! et `category`, serde ignore en silence tout champ absent de la structure, et
//! le corps sortant était reconstruit sans eux. Le ticket partait quand même en
//! 201 et le client annonçait « envoyé » — une panne déguisée en résultat.
//! Mesuré en production le 29/08/2026 : **39 tickets sur 62** sans le moindre
//! journal, exactement ceux sans pièce jointe manuelle, c'est-à-dire exactement
//! ceux passés par ici.
//!
//! ## Pourquoi ce fichier, alors que le correctif est écrit
//!
//! Le correctif (`42ab8a16`) est éprouvé par deux tests unitaires **séparés** :
//! l'un montre que `CreateBody` retient `logs`, l'autre que `ticket_payload` le
//! recopie. Aucun des deux ne montre que la route **joint** les deux moitiés,
//! ni qu'un seul octet sort de la machine. C'est la forme exacte du défaut
//! dominant de ce dépôt — deux moitiés justes, personne qui les appelle. Si
//! quelqu'un retirait `logs: payload.logs` de `create_json`, les deux tests
//! unitaires resteraient verts.
//!
//! ## Ce que ce fichier éprouve — un fait de base, pas un code HTTP
//!
//! Le **corps réellement reçu par le relais**, capté par un vrai serveur HTTP
//! monté dans le test :
//!
//! 1. chemin JSON : le corps reçu porte `logs` **identique octet pour octet**
//!    au journal envoyé, `logs.len()` égale la taille du journal (aucune
//!    troncature sur ~4 Ko), plus `system` et `zone` ;
//! 2. **témoin** : le chemin multipart portait déjà les journaux et les porte
//!    toujours — vert des deux côtés de la contre-épreuve ;
//! 3. **témoin** : un client ancien, qui n'envoie ni `logs` ni `system`, ouvre
//!    son ticket comme avant (rétro-compatibilité #1073) — vert des deux côtés.
//!
//! Le relais de test répond **proprement** un 201 : jamais de connexion coupée
//! (RST), source d'intermittence mesurée dans ce dépôt.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const TICKETS: &str = "/api/v1/support/tickets";
const BORNE: &str = "borne-p2a-2916";

/// Une requête telle que le relais l'a reçue.
struct Recu {
    content_type: String,
    corps: Vec<u8>,
}

/// Faux mozaiklabs : capte ce qu'on lui envoie et répond **proprement** 201.
///
/// Rend la racine à poser dans `mozaik_base_url` et le journal des requêtes
/// reçues. Aucune connexion n'est coupée : le test ne peut pas devenir
/// intermittent pour cette raison.
async fn relais_capteur() -> (String, Arc<Mutex<Vec<Recu>>>) {
    let recus: Arc<Mutex<Vec<Recu>>> = Arc::new(Mutex::new(Vec::new()));
    let journal = recus.clone();

    let app = Router::new().fallback(move |entetes: HeaderMap, corps: Bytes| {
        let journal = journal.clone();
        async move {
            let content_type = entetes
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            journal.lock().expect("journal du relais").push(Recu {
                content_type,
                corps: corps.to_vec(),
            });
            (
                StatusCode::CREATED,
                axum::Json(json!({ "ticket": { "id": 4242 } })),
            )
        }
    });

    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let port = ecoute.local_addr().expect("adresse locale").port();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });

    (format!("http://127.0.0.1:{port}"), recus)
}

/// Routeur complet, nuage redirigé vers le relais de test et auth par clé de
/// licence — sans elle, `auth()` rendrait 412 avant toute sortie réseau et le
/// relais ne verrait rien.
fn app_vers(base: &str) -> Router {
    let state = AppState::new(":memory:", 0, Default::default()).expect("état en mémoire");
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages.set("mozaik_base_url", base).expect("racine nuage");
    reglages
        .set("license_key", "TUNE-TEST-2916")
        .expect("clé de licence");
    reglages
        .set("hardware_fingerprint", "empreinte-p2a-2916")
        .expect("empreinte");
    tune_server::routes::router(state)
}

/// Un vrai markdown de rapport de bogue, assez long (~4 Ko) pour qu'une
/// troncature en chemin se voie sur la taille reçue.
fn journal_de_test() -> String {
    let mut md = String::from(
        "# Tune Bug Report\n\n## Système\n\n- Version : 0.9.121\n- OS : macos\n\n## Journaux\n\n",
    );
    for i in 0..60 {
        md.push_str(&format!(
            "2026-08-30T07:2{}:0{}Z ERROR dlna_stall zone=\"Salon\" iteration={i} détail=\"flux interrompu\"\n",
            i % 10,
            i % 10,
        ));
    }
    md
}

/// Envoie un corps quelconque à `POST /support/tickets` et rend le statut.
async fn poster(app: &Router, content_type: &str, corps: Vec<u8>) -> (StatusCode, String) {
    let reponse = app
        .clone()
        .oneshot(
            Request::post(TICKETS)
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(corps))
                .expect("requête valide"),
        )
        .await
        .expect("réponse du relais local");
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps lisible");
    (status, String::from_utf8_lossy(&octets).into_owned())
}

/// Le seul corps reçu par le relais, ou un échec parlant.
fn seule_requete(recus: &Arc<Mutex<Vec<Recu>>>) -> (String, Vec<u8>) {
    let journal = recus.lock().expect("journal du relais");
    assert_eq!(
        journal.len(),
        1,
        "le relais devait recevoir exactement une requête, il en a vu {}",
        journal.len()
    );
    (journal[0].content_type.clone(), journal[0].corps.clone())
}

// ---------------------------------------------------------------------------
// 1. Le fait de base : les journaux arrivent au relais, entiers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn le_corps_recu_par_le_relais_porte_les_journaux_en_entier() {
    let (base, recus) = relais_capteur().await;
    let app = app_vers(&base);
    let journal = journal_de_test();

    let envoi = json!({
        "subject": "Coupure DLNA",
        "body": "Le salon s'arrête au bout de dix secondes.",
        "category": "bug",
        "zone": "Salon",
        "logs": journal,
        "system": { "os": "macos", "zones": 3 },
    });
    let (status, rendu) = poster(&app, "application/json", envoi.to_string().into_bytes()).await;
    assert!(
        status.is_success(),
        "le relais local a refusé le ticket : {status} — {rendu}"
    );

    let (content_type, corps) = seule_requete(&recus);
    assert!(
        content_type.starts_with("application/json"),
        "chemin JSON attendu, reçu : {content_type}"
    );
    let recu: Value = serde_json::from_slice(&corps).expect("corps JSON reçu par le relais");

    // Le fait de base : le journal est là, et il est ENTIER.
    let logs = recu["logs"].as_str().unwrap_or_default();
    assert_eq!(
        logs.len(),
        journal.len(),
        "taille des journaux reçus = {} octets, attendu {} — champ reçu : {}",
        logs.len(),
        journal.len(),
        recu["logs"]
    );
    assert_eq!(
        logs, journal,
        "les journaux reçus diffèrent de ceux envoyés"
    );
    assert!(
        journal.len() > 3_000,
        "l'éprouvette doit être assez grosse pour qu'une troncature se voie : {} octets",
        journal.len()
    );

    // La fiche système et la zone suivent le même chemin, et le même sort.
    assert_eq!(recu["system"]["os"], json!("macos"), "fiche système perdue");
    assert_eq!(recu["system"]["zones"], json!(3));
    assert_eq!(recu["zone"], json!("Salon"), "zone perdue");

    // Ce qui marchait déjà n'a pas bougé : version et OS restent injectés par
    // le serveur, jamais fournis par la page.
    assert_eq!(recu["subject"], json!("Coupure DLNA"));
    assert_eq!(recu["category"], json!("bug"));
    assert_eq!(recu["tune_version"], json!(tune_core::version()));
    assert_eq!(recu["platform"], json!(std::env::consts::OS));
}

// ---------------------------------------------------------------------------
// 2. Témoin : le chemin multipart portait déjà les journaux
// ---------------------------------------------------------------------------

#[tokio::test]
async fn temoin_le_chemin_multipart_porte_toujours_les_journaux() {
    let (base, recus) = relais_capteur().await;
    let app = app_vers(&base);
    let journal = journal_de_test();
    let piece = "2026-08-30 sample tune-server\nligne de trace\n";

    let mut corps = String::new();
    for (nom, valeur) in [
        ("subject", "Coupure DLNA"),
        ("body", "Le salon s'arrête au bout de dix secondes."),
        ("category", "bug"),
        ("zone", "Salon"),
        ("logs", journal.as_str()),
    ] {
        corps.push_str(&format!(
            "--{BORNE}\r\nContent-Disposition: form-data; name=\"{nom}\"\r\n\r\n{valeur}\r\n"
        ));
    }
    corps.push_str(&format!(
        "--{BORNE}\r\nContent-Disposition: form-data; name=\"attachments[]\"; \
         filename=\"tune-sample.txt\"\r\nContent-Type: text/plain\r\n\r\n{piece}\r\n--{BORNE}--\r\n"
    ));

    let (status, rendu) = poster(
        &app,
        &format!("multipart/form-data; boundary={BORNE}"),
        corps.into_bytes(),
    )
    .await;
    assert!(
        status.is_success(),
        "le relais local a refusé le multipart : {status} — {rendu}"
    );

    let (content_type, recu) = seule_requete(&recus);
    assert!(
        content_type.starts_with("multipart/form-data"),
        "chemin multipart attendu, reçu : {content_type}"
    );
    // reqwest ré-encode le multipart avec sa propre borne : on cherche les
    // octets, pas la forme.
    let texte = String::from_utf8_lossy(&recu);
    assert!(
        texte.contains(journal.as_str()),
        "les journaux ne sont pas dans le multipart reçu ({} octets)",
        recu.len()
    );
    assert!(
        texte.contains(piece),
        "la pièce jointe manuelle n'est pas dans le multipart reçu"
    );
    assert!(
        texte.contains("tune-sample.txt"),
        "le nom de fichier n'a pas été relayé"
    );
}

// ---------------------------------------------------------------------------
// 3. Témoin : un client ancien, sans diagnostic, ouvre son ticket comme avant
// ---------------------------------------------------------------------------

#[tokio::test]
async fn temoin_un_client_sans_diagnostic_ouvre_son_ticket() {
    let (base, recus) = relais_capteur().await;
    let app = app_vers(&base);

    let envoi = json!({ "subject": "Question", "body": "Comment activer le DSD ?" });
    let (status, rendu) = poster(&app, "application/json", envoi.to_string().into_bytes()).await;
    assert!(
        status.is_success(),
        "un corps sans diagnostic doit rester accepté : {status} — {rendu}"
    );

    let (_, corps) = seule_requete(&recus);
    let recu: Value = serde_json::from_slice(&corps).expect("corps JSON reçu par le relais");
    assert_eq!(recu["subject"], json!("Question"));
    // `null`, pas absent : les règles `nullable` de mozaiklabs l'acceptent, et
    // le SAV distingue « rien à joindre » de « champ jamais transmis ».
    assert!(recu["logs"].is_null(), "logs = {}", recu["logs"]);
    assert!(recu["system"].is_null(), "system = {}", recu["system"]);
    assert!(recu["zone"].is_null(), "zone = {}", recu["zone"]);
}

// ---------------------------------------------------------------------------
// 4. Témoin : sans réglage, on ne part JAMAIS ailleurs que chez mozaiklabs
// ---------------------------------------------------------------------------

/// La couture de test ne doit pas déplacer la production : `mozaik_base_url`
/// absent, le support vise mozaiklabs.fr. On ne l'appelle pas pour le prouver —
/// on éprouve la seule chose observable sans réseau : le relais de test, lui,
/// ne reçoit rien.
#[tokio::test]
async fn temoin_sans_reglage_le_relais_de_test_ne_recoit_rien() {
    let (base, recus) = relais_capteur().await;

    let state = AppState::new(":memory:", 0, Default::default()).expect("état en mémoire");
    // Pas de `mozaik_base_url`, et surtout pas de clé : `auth()` refuse par 412
    // AVANT toute sortie réseau — ce 412 est la preuve qu'on n'est pas sorti.
    let app = tune_server::routes::router(state);

    let envoi = json!({ "subject": "Question", "body": "DSD ?", "logs": "trace" });
    let (status, _) = poster(&app, "application/json", envoi.to_string().into_bytes()).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(
        recus.lock().expect("journal du relais").is_empty(),
        "le relais de test a été appelé sans y avoir été dirigé — base = {base}"
    );
}
