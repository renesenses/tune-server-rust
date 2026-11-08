//! #7799 — au premier affichage à distance, la petite liste des dossiers
//! (`/library/browse`) ne doit plus attendre la fin des grosses réponses
//! (pochettes, listes) sur la liaison montante.
//!
//! Le défaut : chaque réponse d'API partait d'un seul bloc, dans une seule
//! file vers le relais, dans l'ordre d'arrivée. Derrière quelques pochettes
//! de plusieurs Mio, la liste des dossiers arrivait au pont après son délai
//! de 30 s : l'écran Répertoires restait vide. Et chaque requête lançait sa
//! lecture locale aussitôt, sans plafond.
//!
//! Le banc : le vrai `RelayClient`, un faux serveur local (axum), et
//! l'écrivain de la connexion rejoué par [`VoiesVersLeRelais`] vers un « fil »
//! d'une seule place, que le test lit à son rythme : une liaison montante
//! lente. On compte les OCTETS écrits sur le fil avant la liste des dossiers,
//! pas le temps.

use super::*;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;

const PATIENCE: Duration = Duration::from_secs(10);
/// Une grosse pochette : 2 Mio.
const TAILLE_DE_POCHETTE: usize = 2 * 1024 * 1024;
const POCHETTES: usize = 4;

fn pochette(n: usize) -> Vec<u8> {
    // Pas de l'UTF-8 (0xFF) : comme une vraie image, elle part en base64.
    (0..TAILLE_DE_POCHETTE)
        .map(|i| if i % 7 == 0 { 0xFF } else { (i + n) as u8 })
        .collect()
}

#[derive(Default)]
struct Compteurs {
    pochettes_servies: AtomicUsize,
    en_cours: AtomicUsize,
    maximum: AtomicUsize,
}

async fn repondre(State(c): State<Arc<Compteurs>>, uri: Uri) -> Response {
    let chemin = uri.path();
    if let Some(n) = chemin
        .strip_prefix("/api/v1/library/albums/")
        .and_then(|r| r.strip_suffix("/cover"))
    {
        let n: usize = n.parse().unwrap();
        c.pochettes_servies.fetch_add(1, Ordering::SeqCst);
        return ([("content-type", "image/jpeg")], pochette(n)).into_response();
    }
    if chemin == "/api/v1/lent" {
        let en_cours = c.en_cours.fetch_add(1, Ordering::SeqCst) + 1;
        c.maximum.fetch_max(en_cours, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        c.en_cours.fetch_sub(1, Ordering::SeqCst);
        return ([("content-type", "application/json")], "{}").into_response();
    }
    if chemin == "/api/v1/erreur" {
        let message = format!("{{\"error\":\"{}\"}}", "x".repeat(200 * 1024));
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("content-type", "application/json")],
            message,
        )
            .into_response();
    }
    (
        [("content-type", "application/json")],
        r#"{"roots":[{"path":"/mnt/musique","name":"musique"}]}"#,
    )
        .into_response()
}

async fn serveur_local() -> (u16, Arc<Compteurs>) {
    let compteurs = Arc::new(Compteurs::default());
    let app = Router::new()
        .fallback(repondre)
        .with_state(compteurs.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (port, compteurs)
}

/// Le vrai client, ses deux voies ouvertes comme par `connect_and_run`, et
/// l'écrivain de la connexion qui les vide vers un fil d'UNE place.
async fn client_et_fil(port: u16) -> (RelayClient, mpsc::Receiver<Trame>) {
    let c = RelayClient::new("srv".into(), "jeton".into(), "ws://x".into(), port);
    let (prio_tx, prio_rx) = mpsc::channel::<Trame>(256);
    let (vrac_tx, vrac_rx) = mpsc::channel::<Trame>(256);
    *c.ws_tx.lock().await = Some(prio_tx);
    *c.ws_vrac.lock().await = Some(vrac_tx);
    let (fil_tx, fil_rx) = mpsc::channel::<Trame>(1);
    tokio::spawn(async move {
        let mut voies = VoiesVersLeRelais::new(prio_rx, vrac_rx);
        while let Some(trame) = voies.suivante().await {
            if fil_tx.send(trame).await.is_err() {
                break;
            }
        }
    });
    (c, fil_rx)
}

fn requete(id: &str, chemin: &str) -> String {
    serde_json::json!({
        "type": "relay.request",
        "id": id,
        "method": "GET",
        "path": chemin,
        "headers": {"accept": "*/*"},
    })
    .to_string()
}

/// Une trame telle que le pont la lit : un morceau (son id et ses octets), ou
/// un message JSON.
enum Lue {
    Morceau(String, Vec<u8>),
    Message(serde_json::Value),
}

fn lire(trame: Trame) -> (usize, Lue) {
    let texte = trame.en_texte();
    let taille = texte.len();
    if let Some(reste) = texte.strip_prefix("BINARY:") {
        let (id, b64) = reste.split_once(':').unwrap();
        let octets = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        return (taille, Lue::Morceau(id.to_string(), octets));
    }
    (taille, Lue::Message(serde_json::from_str(&texte).unwrap()))
}

async fn trame_suivante(fil: &mut mpsc::Receiver<Trame>) -> Trame {
    tokio::time::timeout(PATIENCE, fil.recv())
        .await
        .expect("plus rien sur le fil")
        .expect("fil ferme")
}

/// LE TÉMOIN. Quatre pochettes de 2 Mio sont parties ; la liste des dossiers
/// est demandée ensuite. Elle doit passer devant ce qui reste des pochettes :
/// au plus quelques morceaux de 64 Kio écrits sur le fil avant elle, et non
/// les 8 Mio (plus de 10 Mio en base64) des pochettes entières.
///
/// Les pochettes, elles, arrivent intactes au bout, chacune annoncée par son
/// `relay.stream_start` avant son premier morceau.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_liste_des_dossiers_passe_devant_les_grosses_reponses() {
    let (port, compteurs) = serveur_local().await;
    let (c, mut fil) = client_et_fil(port).await;

    for n in 0..POCHETTES {
        c.handle_message(&requete(
            &format!("req-pochette-{n}"),
            &format!("/api/v1/library/albums/{n}/cover"),
        ))
        .await;
    }
    // Toutes les pochettes lues en local et mises en file vers le relais.
    let debut = std::time::Instant::now();
    while compteurs.pochettes_servies.load(Ordering::SeqCst) < POCHETTES {
        assert!(debut.elapsed() < PATIENCE, "pochettes jamais servies");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    c.handle_message(&requete("req-browse", "/api/v1/library/browse"))
        .await;

    let mut octets_avant_la_liste = 0usize;
    let mut debuts: HashMap<String, serde_json::Value> = HashMap::new();
    let mut corps: HashMap<String, Vec<u8>> = HashMap::new();
    let mut finis = 0usize;
    let mut liste_vue = false;
    while !(liste_vue && finis == POCHETTES) {
        let (taille, lue) = lire(trame_suivante(&mut fil).await);
        match lue {
            Lue::Morceau(id, octets) => {
                assert!(
                    debuts.contains_key(&id),
                    "morceau de {id} avant son relay.stream_start : le pont le jetterait"
                );
                corps.entry(id).or_default().extend(octets);
            }
            Lue::Message(m) => {
                let id = m["id"].as_str().unwrap().to_string();
                match m["type"].as_str().unwrap() {
                    "relay.response" if id == "req-browse" => {
                        assert_eq!(m["status"], 200);
                        assert!(m["body"].as_str().unwrap().contains("/mnt/musique"));
                        liste_vue = true;
                    }
                    "relay.stream_start" => {
                        debuts.insert(id, m);
                    }
                    "relay.stream_end" => finis += 1,
                    autre => panic!("trame inattendue {autre} pour {id} : {m}"),
                }
            }
        }
        if !liste_vue {
            octets_avant_la_liste += taille;
        }
    }

    eprintln!(
        "#7799 mesure : {octets_avant_la_liste} octets ecrits sur le fil avant la liste \
         des dossiers ({POCHETTES} pochettes de {TAILLE_DE_POCHETTE} octets en file)"
    );
    assert!(
        octets_avant_la_liste < 8 * TAILLE_DES_MORCEAUX_DAPI,
        "la liste des dossiers a attendu {octets_avant_la_liste} octets de grosses reponses \
         sur la liaison montante : elle est servie derriere les pochettes en cours"
    );

    for n in 0..POCHETTES {
        let id = format!("req-pochette-{n}");
        let debut = &debuts[&id];
        assert_eq!(debut["status"], 200);
        assert_eq!(debut["headers"]["content-type"], "image/jpeg");
        assert_eq!(debut["content_length"], TAILLE_DE_POCHETTE);
        assert!(corps[&id] == pochette(n), "pochette {n} corrompue");
    }
}

/// Trente requêtes d'un coup, comme un premier écran en HTTP/2 : jamais plus
/// de [`PLAFOND_DE_REQUETES_LOCALES`] lectures locales en même temps, et
/// toutes reçoivent leur réponse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn les_lectures_locales_sont_plafonnees() {
    let (port, compteurs) = serveur_local().await;
    let c = RelayClient::new("srv".into(), "jeton".into(), "ws://x".into(), port);
    let (tx, mut rx) = mpsc::channel::<Trame>(64);
    *c.ws_tx.lock().await = Some(tx);

    for n in 0..30 {
        c.handle_message(&requete(&format!("req-{n}"), "/api/v1/lent"))
            .await;
    }
    for _ in 0..30 {
        let (_, lue) = lire(trame_suivante(&mut rx).await);
        let Lue::Message(m) = lue else {
            panic!("morceau inattendu")
        };
        assert_eq!(m["type"], "relay.response");
        assert_eq!(m["status"], 200);
    }
    let maximum = compteurs.maximum.load(Ordering::SeqCst);
    assert!(
        maximum <= PLAFOND_DE_REQUETES_LOCALES,
        "{maximum} lectures locales en meme temps pour un plafond de {PLAFOND_DE_REQUETES_LOCALES}"
    );
    assert!(maximum >= 2, "les lectures ne se font plus en parallele");
}

/// Une grosse réponse en ERREUR part entière : le pont rend une erreur en
/// flux sans corps, son message serait perdu.
#[tokio::test]
async fn une_grosse_erreur_part_entiere_avec_son_message() {
    let (port, _) = serveur_local().await;
    let (c, mut fil) = client_et_fil(port).await;
    c.handle_message(&requete("req-err", "/api/v1/erreur"))
        .await;
    let (_, lue) = lire(trame_suivante(&mut fil).await);
    let Lue::Message(m) = lue else {
        panic!("une erreur ne part pas en morceaux")
    };
    assert_eq!(m["type"], "relay.response");
    assert_eq!(m["status"], 500);
    assert!(m["body"].as_str().unwrap().len() > SEUIL_DE_DECOUPAGE);
}

/// L'écrivain : quand les deux voies ont une trame, la prioritaire passe
/// d'abord, et l'ordre de chaque voie est gardé.
#[tokio::test]
async fn lecrivain_vide_la_voie_prioritaire_dabord() {
    let (prio_tx, prio_rx) = mpsc::channel::<Trame>(8);
    let (vrac_tx, vrac_rx) = mpsc::channel::<Trame>(8);
    let mut voies = VoiesVersLeRelais::new(prio_rx, vrac_rx);
    vrac_tx.send("morceau-1".to_string().into()).await.unwrap();
    vrac_tx.send("morceau-2".to_string().into()).await.unwrap();
    prio_tx.send("reponse".to_string().into()).await.unwrap();
    let mut ordre = Vec::new();
    for _ in 0..3 {
        ordre.push(voies.suivante().await.unwrap().en_texte());
    }
    assert_eq!(ordre, ["reponse", "morceau-1", "morceau-2"]);
    drop(prio_tx);
    drop(vrac_tx);
    assert!(voies.suivante().await.is_none());
}
