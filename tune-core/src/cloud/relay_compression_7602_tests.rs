//! #7602 — l'écran Répertoires par le relais : la liste des dossiers
//! (`/library/browse`) doit revenir compressée quand le navigateur distant
//! l'accepte, et lisible au bout.
//!
//! Le défaut : par le relais, la requête locale partait sans
//! `accept-encoding`, la route répondait en clair, et `content-encoding`
//! n'était de toute façon pas rendu au pont. Le JSON du premier écran
//! traversait le débit montant de la box 5 à 10 fois plus lourd qu'en local,
//! jusqu'à dépasser les 30 s du relais : l'écran restait vide.
//!
//! Le faux serveur local compresse comme le `CompressionLayer` du vrai
//! (tower-http, gzip) : seulement si la requête accepte gzip, avec
//! `content-encoding: gzip` et `vary: accept-encoding`.

use super::*;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use std::io::{Read as _, Write as _};
use std::sync::Mutex as StdMutex;

const PATIENCE: Duration = Duration::from_secs(5);

/// Les racines d'une bibliothèque : de quoi peser comme un vrai premier
/// écran, avec des accents et des espaces dans les chemins.
fn liste_des_racines() -> String {
    let racines: Vec<serde_json::Value> = (0..400)
        .map(|i| {
            serde_json::json!({
                "path": format!("/mnt/musique/Disques à écouter/Dossier n°{i}"),
                "name": format!("Dossier n°{i}"),
                "track_count": 1000 + i,
            })
        })
        .collect();
    serde_json::json!({ "roots": racines }).to_string()
}

/// Le `accept-encoding` reçu par la route locale, à chaque appel.
type Journal = Arc<StdMutex<Vec<Option<String>>>>;

async fn parcourir(State(journal): State<Journal>, headers: HeaderMap) -> Response {
    let accepte = headers
        .get("accept-encoding")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    journal.lock().unwrap().push(accepte.clone());
    let json = liste_des_racines();
    if accepte.is_some_and(|a| a.contains("gzip")) {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(json.as_bytes()).unwrap();
        (
            [
                ("content-type", "application/json"),
                ("content-encoding", "gzip"),
                ("vary", "accept-encoding"),
            ],
            gz.finish().unwrap(),
        )
            .into_response()
    } else {
        ([("content-type", "application/json")], json).into_response()
    }
}

async fn serveur_local() -> (u16, Journal) {
    let journal: Journal = Arc::new(StdMutex::new(Vec::new()));
    let app = Router::new()
        .fallback(parcourir)
        .with_state(journal.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (port, journal)
}

/// La trame `relay.response` que le serveur renvoie au pont pour un
/// `GET /api/v1/library/browse` portant ces en-têtes.
async fn reponse_relayee(port: u16, entetes: serde_json::Value) -> serde_json::Value {
    let c = RelayClient::new("srv".into(), "jeton".into(), "ws://x".into(), port);
    let (tx, mut rx) = mpsc::channel::<Trame>(8);
    *c.ws_tx.lock().await = Some(tx);
    c.handle_message(
        &serde_json::json!({
            "type": "relay.request",
            "id": "req-browse",
            "method": "GET",
            "path": "/api/v1/library/browse",
            "headers": entetes,
        })
        .to_string(),
    )
    .await;
    let trame = tokio::time::timeout(PATIENCE, rx.recv())
        .await
        .expect("aucune reponse vers le pont")
        .expect("canal ferme")
        .en_texte();
    serde_json::from_str(&trame).unwrap()
}

/// Ce que le navigateur lit, une fois la réponse décodée comme il le fait :
/// base64 si binaire, puis gzip si `content-encoding` le dit.
fn ce_que_lit_le_navigateur(r: &serde_json::Value) -> serde_json::Value {
    let octets = match r["body_base64"].as_str() {
        Some(b64) => base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap(),
        None => r["body"].as_str().unwrap_or_default().as_bytes().to_vec(),
    };
    let octets = if r["headers"]["content-encoding"] == "gzip" {
        let mut clair = Vec::new();
        flate2::read::GzDecoder::new(&octets[..])
            .read_to_end(&mut clair)
            .expect("content-encoding gzip mais corps non gzip");
        clair
    } else {
        octets
    };
    serde_json::from_slice(&octets).unwrap_or_else(|e| {
        panic!(
            "corps illisible pour le navigateur ({e}) : {} octets, en-tetes {}",
            octets.len(),
            r["headers"]
        )
    })
}

/// LE défaut de #7602 : le navigateur distant accepte gzip, le pont le dit au
/// serveur, et la liste des dossiers doit revenir compressée ET lisible.
#[tokio::test]
async fn la_liste_des_dossiers_revient_compressee_et_lisible_par_le_relais() {
    let (port, journal) = serveur_local().await;
    let r = reponse_relayee(
        port,
        serde_json::json!({
            "accept": "application/json",
            "x-tune-accept-encoding": "gzip, deflate, br, zstd",
        }),
    )
    .await;
    assert_eq!(r["status"], 200);
    // Le contrat avec le pont (`tune-bridge/src/api_proxy.rs`).
    assert_eq!(ENTETE_ENCODAGES_ACCEPTES, "x-tune-accept-encoding");
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        [Some("gzip, deflate, br, zstd".to_string())],
        "la route locale n'a pas recu les compressions du navigateur distant"
    );
    assert_eq!(
        r["headers"]["content-encoding"], "gzip",
        "reponse en clair par le relais : content-encoding absent ({})",
        r["headers"]
    );
    assert_eq!(r["headers"]["vary"], "accept-encoding");
    let poids_relaye = r["body_base64"].as_str().map_or(0, str::len);
    let poids_en_clair = liste_des_racines().len();
    assert!(
        poids_relaye * 4 < poids_en_clair,
        "la liste ne s'allege pas : {poids_relaye} octets relayes pour {poids_en_clair} en clair"
    );
    let lu = ce_que_lit_le_navigateur(&r);
    let racines = lu["roots"].as_array().expect("pas de roots");
    assert_eq!(racines.len(), 400, "liste des dossiers incomplete");
    assert_eq!(
        racines[7]["path"],
        "/mnt/musique/Disques à écouter/Dossier n°7"
    );
}

/// Un pont antérieur ne relaie pas les compressions du navigateur, et ne
/// rendrait pas `content-encoding` : la réponse doit rester en clair.
#[tokio::test]
async fn sans_lentete_du_pont_la_reponse_reste_en_clair() {
    let (port, journal) = serveur_local().await;
    let r = reponse_relayee(port, serde_json::json!({"accept": "application/json"})).await;
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        [Some("identity".to_string())]
    );
    assert!(r["headers"].get("content-encoding").is_none(), "{r}");
    assert!(r.get("body_base64").is_none());
    assert_eq!(
        ce_que_lit_le_navigateur(&r)["roots"][0]["track_count"],
        1000
    );
}

/// Un `accept-encoding` brut dans la trame ne vaut pas la déclaration du
/// pont : rien ne dit que ce pont-là rendrait `content-encoding`.
#[tokio::test]
async fn un_accept_encoding_brut_ne_suffit_pas() {
    let (port, journal) = serveur_local().await;
    let r = reponse_relayee(port, serde_json::json!({"accept-encoding": "gzip"})).await;
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        [Some("identity".to_string())]
    );
    assert!(r["headers"].get("content-encoding").is_none(), "{r}");
    assert_eq!(
        ce_que_lit_le_navigateur(&r)["roots"]
            .as_array()
            .unwrap()
            .len(),
        400
    );
}
