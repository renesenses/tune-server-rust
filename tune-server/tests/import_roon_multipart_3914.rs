//! `POST /system/import/roon` accepte le FICHIER que le client envoie — #3914.
//!
//! Ce que ce fichier garde n'est pas l'analyseur : `parse_roon_csv` et
//! `parse_plex_xml` vivent dans `tune-core/src/library/importer.rs`, avec leurs
//! propres essais. Le défaut est ailleurs, et il est plus bête : **la route
//! n'acceptait pas le format que le client envoie**.
//!
//! Le client téléverse un fichier (`FormData` avec une partie `file`,
//! `tune-web-client/src/lib/api.ts`) ; le gestionnaire déclarait
//! `Json<ImportRoonRequest>`. Axum refuse alors le corps **avant** d'entrer
//! dans le gestionnaire : `415 Unsupported Media Type`. L'import Roon n'a donc
//! jamais pu fonctionner, depuis le premier jour.
//!
//! 🔴 **Pourquoi rien n'avait rougi.** `import.rs` porte cinq essais unitaires,
//! sérieux et verts. Mais les trois essais de comportement appellent
//! `import_roon(State(state), Json(body))` **directement** : ils éprouvent le
//! GESTIONNAIRE. Le 415 vit exactement dans l'espace qu'aucun d'eux ne
//! traverse — entre le routeur et le gestionnaire. Ces cinq essais resteront
//! verts quoi qu'il arrive au contrat HTTP ; ils ne gardent pas cette
//! frontière-ci.
//!
//! Ce fichier passe donc par `tune_server::routes::router(state)` — le routeur
//! réel, son préfixe `/api/v1`, ses extracteurs — et par le chemin exact que le
//! client tape, avec un VRAI corps `multipart/form-data`. Remettre
//! `Json<…>` à la place du dispatcheur fait rougir ces témoins sur un 415.
//!
//! ⚠️ **Sur la fixture, et c'est une limite assumée.** Aucun export CSV réel de
//! Roon n'était disponible : ni dans le dépôt, ni dans l'archive de la sonde de
//! phase 0 (`sonde-roon-20260911-170204.tar.gz`, qui ne contient que du JSON de
//! l'API `browse`). Les en-têtes utilisés ci-dessous sont donc ceux que
//! `ROON_*_HEADERS` de `tune-core` accepte déjà — l'hypothèse du dépôt, pas une
//! mesure. Ce que ces témoins prouvent est le TRANSPORT : le multipart est
//! accepté, le CSV est décodé, la route rend 202 et non 415. La correspondance
//! exacte des colonnes reste à confirmer sur un export réel
//! (*Roon → Library → Export*). D'ici là, un en-tête inconnu ne passe pas en
//! silence : il sort en 422 nommé — c'est le quatrième témoin.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

const ROON: &str = "/api/v1/system/import/roon";
const PLEX: &str = "/api/v1/system/import/plex";

/// Un export CSV **de forme Roon**, pas un export Roon.
///
/// Les colonnes sont celles que `tune_core::library::importer::parse_roon_csv`
/// reconnaît déjà (`ROON_TITLE_HEADERS`, `ROON_ARTIST_HEADERS`, …). Voir la
/// réserve en tête de fichier : personne n'a encore posé un export réel à côté.
const CSV_ROON: &str = "Title,Artist,Album,File Path,Play Count\r\n\
     Walking,Sokratis Sinopoulos,Eight Winds,/music/eight-winds/01.flac,5\r\n\
     Liberte,Sokratis Sinopoulos,Eight Winds,/music/eight-winds/02.flac,2\r\n";

/// Un export XML de bibliothèque Plex, forme `MediaContainer` — celle que
/// `parse_plex_xml` lit, et celle des essais de `tune-core`.
const XML_PLEX: &str = r#"<MediaContainer><Track title="Time" grandparentTitle="Pink Floyd" parentTitle="The Dark Side of the Moon" viewCount="3" /></MediaContainer>"#;

async fn app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

/// Un corps `multipart/form-data` écrit à la main, avec UNE partie `file`.
///
/// Écrit ici plutôt qu'emprunté à un utilitaire : le témoin doit poster ce que
/// `FormData` du navigateur poste, et non ce que notre propre code croit que
/// c'est.
fn multipart(nom_du_fichier: &str, type_mime: &str, contenu: &str) -> (String, Vec<u8>) {
    let frontiere = "----tune3914RoonMultipart";
    let corps = format!(
        "--{frontiere}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"{nom_du_fichier}\"\r\n\
         Content-Type: {type_mime}\r\n\
         \r\n\
         {contenu}\r\n\
         --{frontiere}--\r\n"
    );
    (
        format!("multipart/form-data; boundary={frontiere}"),
        corps.into_bytes(),
    )
}

async fn poster(
    app: &axum::Router,
    chemin: &str,
    type_de_contenu: &str,
    corps: Vec<u8>,
) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header(header::CONTENT_TYPE, type_de_contenu)
                .body(Body::from(corps))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).to_string();
    (
        statut,
        serde_json::from_str(&texte).unwrap_or(json!({ "_brut": texte })),
    )
}

// --- Le défaut gardé : la route accepte-t-elle ce que le client envoie ? ---

/// LE témoin. Un vrai multipart, un vrai CSV, sur le chemin réel : 202.
#[tokio::test]
async fn import_roon_accepte_un_multipart_csv_et_rend_202_pas_415() {
    let app = app().await;
    let (type_de_contenu, corps) = multipart("roon-export.csv", "text/csv", CSV_ROON);
    let (statut, reponse) = poster(&app, ROON, &type_de_contenu, corps).await;

    assert_ne!(
        statut,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "415 sur {ROON} : la route refuse le multipart que le client envoie, \
         AVANT d'entrer dans le gestionnaire — l'import Roon est inutilisable \
         (#3914). Réponse : {reponse}"
    );
    assert_ne!(
        statut,
        StatusCode::NOT_FOUND,
        "route non montée sur {ROON} : {reponse}"
    );
    assert_eq!(
        statut,
        StatusCode::ACCEPTED,
        "le CSV téléversé doit être accepté ; réponse : {reponse}"
    );
    assert!(
        reponse["task_id"].as_str().is_some_and(|t| !t.is_empty()),
        "un import accepté doit rendre un identifiant de tâche : {reponse}"
    );
}

/// Le jumeau. Même défaut, même correctif : `/system/import/plex`.
#[tokio::test]
async fn import_plex_accepte_un_multipart_et_rend_202_pas_415() {
    let app = app().await;
    let (type_de_contenu, corps) = multipart("plex-export.xml", "text/xml", XML_PLEX);
    let (statut, reponse) = poster(&app, PLEX, &type_de_contenu, corps).await;

    assert_ne!(
        statut,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "415 sur {PLEX} : même défaut que Roon, le client y téléverse aussi un \
         fichier (#3914). Réponse : {reponse}"
    );
    assert_eq!(
        statut,
        StatusCode::ACCEPTED,
        "l'export Plex téléversé doit être accepté ; réponse : {reponse}"
    );
}

/// Ce qui marchait doit continuer de marcher : le chemin JSON, inchangé.
///
/// Le dispatcheur choisit d'après le `Content-Type`. Un dispatcheur qui
/// n'écouterait que le multipart remplacerait un défaut par un autre.
#[tokio::test]
async fn le_chemin_json_reste_accepte() {
    let app = app().await;
    let corps = json!({
        "data": [{
            "title": "Walking",
            "artist": "Sokratis Sinopoulos",
            "album": "Eight Winds",
            "file_path": "/music/eight-winds/01.flac",
        }]
    });
    let (statut, reponse) = poster(
        &app,
        ROON,
        "application/json",
        corps.to_string().into_bytes(),
    )
    .await;

    assert_eq!(
        statut,
        StatusCode::ACCEPTED,
        "le chemin JSON historique doit rester accepté tel quel ; réponse : {reponse}"
    );
    assert!(
        reponse["task_id"].as_str().is_some_and(|t| !t.is_empty()),
        "le chemin JSON doit toujours rendre un identifiant de tâche : {reponse}"
    );
}

/// Un fichier qu'on ne sait pas lire sort NOMMÉ, pas en 415 muet.
///
/// C'est la garde qui tient la réserve de la fixture : le jour où un export
/// réel de Roon portera d'autres colonnes que celles supposées ici, l'écran
/// dira laquelle il a reçue — au lieu d'annoncer un import qui n'importe rien.
#[tokio::test]
async fn un_csv_aux_colonnes_inconnues_est_refuse_avec_un_nom_lisible() {
    let app = app().await;
    let (type_de_contenu, corps) = multipart(
        "autre-chose.csv",
        "text/csv",
        "Colonne A,Colonne B\r\n1,2\r\n",
    );
    let (statut, reponse) = poster(&app, ROON, &type_de_contenu, corps).await;

    assert_ne!(
        statut,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "un CSV illisible doit être refusé pour son CONTENU, pas rejeté en 415 \
         avant lecture : {reponse}"
    );
    assert_eq!(
        statut,
        StatusCode::UNPROCESSABLE_ENTITY,
        "un CSV sans colonne utilisable doit sortir en 422 : {reponse}"
    );
    assert_eq!(
        reponse["error"], "csv_sans_colonne_utilisable",
        "le refus doit porter un nom stable : {reponse}"
    );
    assert!(
        reponse["detail"]
            .as_str()
            .is_some_and(|d| d.contains("Colonne A")),
        "le refus doit RENDRE l'en-tête reçu, sinon l'utilisateur ne peut rien \
         en faire : {reponse}"
    );
}

/// Un multipart sans partie `file` ne doit pas passer pour un import vide.
#[tokio::test]
async fn un_multipart_sans_partie_file_est_refuse_avec_un_nom_lisible() {
    let app = app().await;
    let frontiere = "----tune3914Vide";
    let corps = format!(
        "--{frontiere}\r\n\
         Content-Disposition: form-data; name=\"autre\"\r\n\
         \r\n\
         rien\r\n\
         --{frontiere}--\r\n"
    );
    let (statut, reponse) = poster(
        &app,
        ROON,
        &format!("multipart/form-data; boundary={frontiere}"),
        corps.into_bytes(),
    )
    .await;

    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "un envoi sans partie « file » doit être refusé explicitement : {reponse}"
    );
    assert_eq!(
        reponse["error"], "fichier_absent",
        "le refus doit porter un nom stable : {reponse}"
    );
}
