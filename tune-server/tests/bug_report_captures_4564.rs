//! #4564 — « Signaler dans le forum » depuis Tune : les captures.
//!
//! Jean Valjean, fil forum 1855, 19/09/2026, 0.9.158, Windows :
//!
//! > « Je ne peux pas mettre de copies d'ecran, l'ajout de fichier a disparu »
//!
//! La contre-épreuve est dans son propre comportement : **quatre minutes plus
//! tard**, il rouvre un fil à la main (1856) pour la seule raison d'y joindre
//! sa capture. Et l'écart est DANS LE MÊME ÉCRAN : « Écrire au support », le
//! geste voisin, accepte `attachments[]` depuis toujours.
//!
//! # Ce que le forum accepte réellement
//!
//! Vérifié dans `site-mozaiklabs` avant d'écrire le contrat, et non supposé :
//! `BugReportController::store` ne validait que
//! `{title?, body, os?, version?, instance_id?}`, et `validate()` jette en
//! silence toute clé inconnue. La voie voisine est une AUTRE chaîne
//! (`SupportTicketController`, premium, modèle `SupportAttachment`). Un fil de
//! forum, lui, n'a plus de pièces jointes du tout — la table
//! `forum_attachments` a été supprimée (migration `2026_03_07_100001`) : il
//! porte ses images EN LIGNE, déposées par `ThreadController::uploadImage`,
//! qui exige une session authentifiée et reste donc inatteignable depuis ce
//! relais serveur-à-serveur sans jeton.
//!
//! D'où `images[]`, et une PR jumelle sur `site-mozaiklabs` qui l'accepte.
//!
//! # Ce que ce fichier cloue
//!
//! 1. ⭐ un envoi multipart sort bien vers le service communautaire avec les
//!    captures sous **`images[]`** — observé dans les octets reçus par un faux
//!    service local, pas déduit du code. Sous un autre nom, le site accepterait
//!    la requête et jetterait les fichiers en silence ;
//! 2. la RÉTRO-COMPATIBILITÉ : sans capture, le corps reste du JSON, exactement
//!    comme avant #4564 ;
//! 3. les refus sont des PHRASES, rendues **avant** tout appel réseau : trop de
//!    captures, fichier qui n'est pas une image, image trop lourde. Le faux
//!    service compte ses appels — un refus qui aurait quand même posté serait
//!    rouge ;
//! 4. le nombre de captures rendu au client est celui que le SITE dit avoir
//!    rangé, pas celui qu'on a envoyé.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré en cible `[[test]]` dans `tune-server/Cargo.toml`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Ce que le faux service communautaire a RÉELLEMENT reçu.
#[derive(Default)]
struct Recu {
    content_type: String,
    accept: String,
    corps: Vec<u8>,
}

struct FauxForum {
    base: String,
    appels: Arc<AtomicUsize>,
    dernier: Arc<Mutex<Recu>>,
}

/// Un service communautaire de contrebande, sur un port libre de la boucle
/// locale. Il n'imite rien : il ENREGISTRE ce qu'on lui envoie, et c'est
/// exactement ce qu'on veut prouver.
async fn faux_forum() -> FauxForum {
    let appels = Arc::new(AtomicUsize::new(0));
    let dernier = Arc::new(Mutex::new(Recu::default()));

    let a = appels.clone();
    let d = dernier.clone();
    let app = axum::Router::new().route(
        "/api/v1/community/bug-report",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, corps: axum::body::Bytes| {
                let a = a.clone();
                let d = d.clone();
                async move {
                    a.fetch_add(1, Ordering::SeqCst);
                    let ct = headers
                        .get(axum::http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    // Combien de parties `images[]` ? Compté sur les octets.
                    let texte = String::from_utf8_lossy(&corps).to_string();
                    let n = texte.matches("name=\"images[]\"").count();
                    let accept = headers
                        .get(axum::http::header::ACCEPT)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    *d.lock().await = Recu {
                        content_type: ct,
                        accept,
                        corps: corps.to_vec(),
                    };
                    axum::Json(json!({
                        "status": "submitted",
                        "images": n,
                        "thread": { "id": 42, "slug": "bug-essai", "url": "https://exemple/forum/threads/bug-essai" },
                    }))
                }
            },
        ),
    );

    // 🔴 Le plafond de corps PAR DÉFAUT d'axum est de 2 Mio : sans cette
    // couche, le faux service rendait 413 sur une capture de 4 Mio — pourtant
    // DANS les bornes — et la contre-épreuve tombait pour une raison qui
    // n'appartenait qu'au banc. C'est elle qui l'a montré.
    let app = app.layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    FauxForum {
        base: format!("http://127.0.0.1:{port}"),
        appels,
        dernier,
    }
}

fn app_et_etat(base: &str) -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // Le MÊME réglage que celui de l'API support — on n'en invente pas un second.
    SettingsRepo::with_backend(state.backend.clone())
        .set("mozaik_base_url", base)
        .unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

/// Un PNG minuscule mais VALIDE : en-tête PNG puis du remplissage. Le serveur
/// ne décode pas l'image — il juge l'extension et la taille — mais un octet
/// quelconque déguisé en .png rendrait le témoin moins lisible.
fn png(taille: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.resize(taille.max(8), 0);
    v
}

/// Construit un `multipart/form-data` à la main : le banc doit produire
/// exactement ce que le navigateur produit, sans passer par une bibliothèque
/// qui « arrangerait » le corps.
fn corps_multipart(description: &str, fichiers: &[(&str, Vec<u8>)]) -> (String, Vec<u8>) {
    let limite = "----tune4564";
    let mut corps: Vec<u8> = Vec::new();
    corps.extend_from_slice(
        format!(
            "--{limite}\r\nContent-Disposition: form-data; name=\"description\"\r\n\r\n{description}\r\n"
        )
        .as_bytes(),
    );
    for (nom, octets) in fichiers {
        corps.extend_from_slice(
            format!(
                "--{limite}\r\nContent-Disposition: form-data; name=\"images[]\"; \
                 filename=\"{nom}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        corps.extend_from_slice(octets);
        corps.extend_from_slice(b"\r\n");
    }
    corps.extend_from_slice(format!("--{limite}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={limite}"), corps)
}

async fn poster(app: &axum::Router, ct: &str, corps: Vec<u8>) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/system/bug-report/submit")
                .header("content-type", ct)
                .body(Body::from(corps))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

// --- 1 : ⭐ les captures sortent, sous le bon nom ----------------------

#[tokio::test]
async fn les_captures_partent_au_forum_sous_images() {
    let forum = faux_forum().await;
    let (app, _state) = app_et_etat(&forum.base);

    let (ct, corps) = corps_multipart(
        "La liste saute quand je descends.",
        &[("capture.png", png(64)), ("seconde.jpg", png(32))],
    );
    let (code, reponse) = poster(&app, &ct, corps).await;

    assert_eq!(code, StatusCode::OK, "réponse : {reponse}");
    assert_eq!(forum.appels.load(Ordering::SeqCst), 1);

    let recu = forum.dernier.lock().await;
    assert!(
        recu.content_type.starts_with("multipart/form-data"),
        "avec des captures, le relais doit sortir en multipart ; il a envoyé : {}",
        recu.content_type
    );
    let texte = String::from_utf8_lossy(&recu.corps);
    assert_eq!(
        texte.matches("name=\"images[]\"").count(),
        2,
        "⭐ les DEUX captures doivent partir sous `images[]` — le nom qu'attend \
         la règle Laravel `images.*`. Sous un autre nom, le site accepterait la \
         requête et jetterait les fichiers EN SILENCE."
    );
    // Les champs déjà éprouvés du contrat n'ont pas bougé.
    for champ in ["title", "body", "os", "version", "instance_id"] {
        assert!(
            texte.contains(&format!("name=\"{champ}\"")),
            "le champ `{champ}` doit rester dans l'envoi"
        );
    }
    // Et la description du testeur ouvre bien le corps du fil.
    assert!(texte.contains("La liste saute quand je descends."));

    // 🔴 `Accept: application/json`. Sans lui, Laravel répond à un refus de
    // validation par une REDIRECTION 302 ; `reqwest` la suit, tombe sur une
    // page HTML en 200, et le refus se lirait « envoyé ». Mesuré sur le banc
    // Pest de la PR jumelle `site-mozaiklabs`.
    assert_eq!(
        recu.accept, "application/json",
        "sans cet en-tête, un refus du forum revient en 302 et se lit « envoyé »"
    );

    // 4 — le nombre rendu est celui que le SITE annonce.
    assert_eq!(
        reponse["images"].as_u64(),
        Some(2),
        "l'écran annonce ce qui est arrivé, pas ce qu'on espérait"
    );
}

// --- 2 : la rétro-compatibilité ---------------------------------------

/// Sans capture, rien ne change : le corps reste du JSON. Un serveur qui ne
/// joint rien émet exactement ce qu'il émettait avant #4564.
#[tokio::test]
async fn sans_capture_le_corps_reste_du_json() {
    let forum = faux_forum().await;
    let (app, _state) = app_et_etat(&forum.base);

    let (code, _) = poster(
        &app,
        "application/json",
        br#"{"description":"Rapport sans capture."}"#.to_vec(),
    )
    .await;

    assert_eq!(code, StatusCode::OK);
    let recu = forum.dernier.lock().await;
    assert!(
        recu.content_type.starts_with("application/json"),
        "sans capture, le relais doit rester JSON ; il a envoyé : {}",
        recu.content_type
    );
    assert_eq!(recu.accept, "application/json");
    let corps: Value = serde_json::from_slice(&recu.corps).unwrap();
    assert!(
        corps["body"]
            .as_str()
            .unwrap()
            .contains("Rapport sans capture.")
    );
}

// --- 3 : les refus sont des phrases, et ils n'appellent personne -------

#[tokio::test]
async fn trop_de_captures_refuse_avant_tout_appel() {
    let forum = faux_forum().await;
    let (app, _state) = app_et_etat(&forum.base);

    let (ct, corps) = corps_multipart(
        "Quatre captures.",
        &[
            ("a.png", png(16)),
            ("b.png", png(16)),
            ("c.png", png(16)),
            ("d.png", png(16)),
        ],
    );
    let (code, reponse) = poster(&app, &ct, corps).await;

    assert_eq!(code, StatusCode::BAD_REQUEST, "réponse : {reponse}");
    assert_eq!(reponse["error"].as_str(), Some("too_many_images"));
    assert!(
        reponse["message"].as_str().is_some_and(|m| m.contains("3")),
        "le refus doit DIRE la borne ; il porte : {reponse}"
    );
    assert_eq!(
        forum.appels.load(Ordering::SeqCst),
        0,
        "un refus qui aurait quand même posté n'est pas un refus"
    );
}

#[tokio::test]
async fn un_fichier_qui_nest_pas_une_image_est_refuse_nommement() {
    let forum = faux_forum().await;
    let (app, _state) = app_et_etat(&forum.base);

    let (ct, corps) = corps_multipart("Un journal.", &[("serveur.log", b"INFO ...".to_vec())]);
    let (code, reponse) = poster(&app, &ct, corps).await;

    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(reponse["error"].as_str(), Some("image_type"));
    assert!(
        reponse["message"]
            .as_str()
            .is_some_and(|m| m.contains("serveur.log")),
        "le refus doit NOMMER le fichier en cause ; il porte : {reponse}"
    );
    assert_eq!(forum.appels.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn une_image_trop_lourde_est_refusee_nommement() {
    let forum = faux_forum().await;
    let (app, _state) = app_et_etat(&forum.base);

    // 4 Mio + 1 octet : juste au-dessus de la borne, pour qu'elle soit la cause.
    let (ct, corps) = corps_multipart(
        "Capture lourde.",
        &[("enorme.png", png(4 * 1024 * 1024 + 1))],
    );
    let (code, reponse) = poster(&app, &ct, corps).await;

    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(reponse["error"].as_str(), Some("image_too_large"));
    assert!(
        reponse["message"]
            .as_str()
            .is_some_and(|m| m.contains("enorme.png")),
        "le refus doit NOMMER le fichier en cause ; il porte : {reponse}"
    );
    assert_eq!(forum.appels.load(Ordering::SeqCst), 0);
}

/// CONTRE-ÉPREUVE des trois refus ci-dessus : une image JUSTE SOUS la borne
/// passe. Sans elle, un refus posé trop bas — ou un chemin multipart cassé —
/// rendrait les trois témoins verts pour la mauvaise raison.
#[tokio::test]
async fn contre_epreuve_une_image_dans_les_bornes_passe() {
    let forum = faux_forum().await;
    let (app, _state) = app_et_etat(&forum.base);

    let (ct, corps) = corps_multipart("Capture limite.", &[("juste.png", png(4 * 1024 * 1024))]);
    let (code, reponse) = poster(&app, &ct, corps).await;

    assert_eq!(code, StatusCode::OK, "réponse : {reponse}");
    assert_eq!(forum.appels.load(Ordering::SeqCst), 1);
    assert_eq!(reponse["images"].as_u64(), Some(1));
}
