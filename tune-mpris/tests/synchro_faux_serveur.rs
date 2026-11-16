//! La boucle de synchronisation face à un faux serveur Tune : vraie
//! WebSocket, vraie requête `GET /api/v1/zones`, vraie pochette. Aucun D-Bus :
//! le notifieur retient ce qu'il aurait signalé.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tune_mpris::client::ClientTune;
use tune_mpris::etat::{Changements, Etat, Statut};
use tune_mpris::synchro::{Notifieur, suivre};

const JETON: &str = "jeton-d-essai";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfaux";

struct Notes(mpsc::UnboundedSender<Changements>);

#[async_trait::async_trait]
impl Notifieur for Notes {
    async fn notifier(&self, c: Changements) {
        let _ = self.0.send(c);
    }
}

/// Ce que le faux serveur a vu.
#[derive(Default)]
struct Vu {
    autorisations: Vec<String>,
    abonnement: Option<Value>,
    chemins: Vec<String>,
}

async fn repondre_http(mut flux: TcpStream, vu: Arc<Mutex<Vu>>) {
    let mut tampon = Vec::new();
    let mut morceau = [0u8; 1024];
    while !tampon.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = flux.read(&mut morceau).await.unwrap();
        if n == 0 {
            return;
        }
        tampon.extend_from_slice(&morceau[..n]);
    }
    let requete = String::from_utf8_lossy(&tampon).to_string();
    let chemin = requete.split_whitespace().nth(1).unwrap_or("").to_string();
    {
        let mut v = vu.lock().unwrap();
        v.chemins.push(chemin.clone());
        for ligne in requete.lines() {
            if let Some((k, val)) = ligne.split_once(':')
                && k.eq_ignore_ascii_case("authorization")
            {
                v.autorisations.push(val.trim().to_string());
            }
        }
    }
    let (type_mime, corps): (&str, Vec<u8>) = if chemin == "/api/v1/zones" {
        let zones = json!([
            {"id": 1, "name": "Bureau", "state": "stopped"},
            {"id": 4, "name": "Salon", "state": "paused", "position_ms": 1_000, "volume": 0.5,
             "volume_curve": "linear",
             "current_track": {"track_id": 3, "title": "All Blues", "artist_name": "Miles Davis",
                               "duration_ms": 693_000, "cover_path": "c0ffee"}}
        ]);
        ("application/json", zones.to_string().into_bytes())
    } else if chemin == "/api/v1/library/artwork/c0ffee" {
        ("image/png", PNG.to_vec())
    } else {
        ("text/plain", b"introuvable".to_vec())
    };
    let statut = if type_mime == "text/plain" {
        "404 Not Found"
    } else {
        "200 OK"
    };
    let tete = format!(
        "HTTP/1.1 {statut}\r\nContent-Type: {type_mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        corps.len()
    );
    flux.write_all(tete.as_bytes()).await.unwrap();
    flux.write_all(&corps).await.unwrap();
}

// La signature du rappel est celle de tungstenite (`ErrorResponse` en erreur).
#[allow(clippy::result_large_err)]
async fn servir_ws(flux: TcpStream, vu: Arc<Mutex<Vu>>) {
    let vu_poignee = vu.clone();
    let mut ws = tokio_tungstenite::accept_hdr_async(flux, move |req: &Request, rep: Response| {
        if let Some(a) = req.headers().get("authorization") {
            vu_poignee
                .lock()
                .unwrap()
                .autorisations
                .push(a.to_str().unwrap().to_string());
        }
        Ok(rep)
    })
    .await
    .unwrap();
    if let Some(Ok(Message::Text(t))) = ws.next().await {
        vu.lock().unwrap().abonnement = serde_json::from_str(&t).ok();
    }
    // Laisser la boucle lire l'instantané avant les évènements.
    tokio::time::sleep(Duration::from_millis(300)).await;
    for message in [
        json!({"type": "playback.volume", "data": {"zone_id": 1, "volume": 0.9}}),
        json!({"type": "playback.resumed", "data": {"zone_id": 4}}),
        json!({"type": "playback.seek", "data": {"zone_id": 4, "position_ms": 60_000}}),
        json!({"type": "playback.volume", "data": {"zone_id": 4, "volume": 0.25}}),
    ] {
        ws.send(Message::Text(message.to_string().into()))
            .await
            .unwrap();
    }
    // Garder la connexion ouverte : la fermeture est un autre essai.
    while ws.next().await.is_some() {}
}

async fn faux_serveur() -> (String, Arc<Mutex<Vu>>) {
    let ecoute = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = format!("http://{}", ecoute.local_addr().unwrap());
    let vu = Arc::new(Mutex::new(Vu::default()));
    let vu_boucle = vu.clone();
    tokio::spawn(async move {
        loop {
            let (flux, _) = ecoute.accept().await.unwrap();
            let vu = vu_boucle.clone();
            tokio::spawn(async move {
                let mut debut = [0u8; 8];
                let n = flux.peek(&mut debut).await.unwrap_or(0);
                if debut[..n].starts_with(b"GET /ws") {
                    servir_ws(flux, vu).await;
                } else {
                    repondre_http(flux, vu).await;
                }
            });
        }
    });
    (adresse, vu)
}

async fn attendre<F: Fn(&Etat) -> bool>(etat: &Arc<Mutex<Etat>>, condition: F, quoi: &str) {
    let fin = Instant::now() + Duration::from_secs(10);
    while Instant::now() < fin {
        if condition(&etat.lock().unwrap()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("délai dépassé : {quoi}");
}

#[tokio::test]
async fn instantane_puis_evenements_de_la_zone_puis_pochette_en_cache() {
    let cache = std::env::temp_dir().join(format!("tune-mpris-synchro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache);
    // Seul essai de ce binaire : rien d'autre ne lit l'environnement.
    unsafe { std::env::set_var("XDG_CACHE_HOME", &cache) };

    let (adresse, vu) = faux_serveur().await;
    let mut client = ClientTune::new(&adresse, Some(JETON));
    client.zone_id = 4;
    let etat = Arc::new(Mutex::new(Etat::new(4, "", Instant::now())));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let tache = tokio::spawn(suivre(client, etat.clone(), Arc::new(Notes(tx))));

    attendre(
        &etat,
        |e| e.nom_de_zone == "Salon" && e.piste.is_some(),
        "instantané",
    )
    .await;
    attendre(&etat, |e| e.statut == Statut::Lecture, "playback.resumed").await;
    attendre(
        &etat,
        |e| (e.gain - 0.25).abs() < 1e-9,
        "playback.volume de la zone 4",
    )
    .await;
    attendre(&etat, |e| e.pochette.is_some(), "pochette").await;

    let pochette = etat.lock().unwrap().pochette.clone().unwrap();
    assert!(pochette.starts_with("file://"), "{pochette}");
    let fichier = std::fs::read_dir(cache.join("tune-mpris"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|f| f.path().extension().is_some_and(|x| x == "png"))
        .expect("pochette rangée en .png");
    assert_eq!(std::fs::read(fichier.path()).unwrap(), PNG);
    assert!(etat.lock().unwrap().position_ms(Instant::now()) >= 60_000);

    tache.abort();
    let mut notes = vec![];
    while let Ok(c) = rx.try_recv() {
        notes.push(c);
    }
    assert!(
        notes.iter().any(|c| c.saut == Some(60_000_000)),
        "Seeked : {notes:?}"
    );
    assert!(notes.iter().any(|c| c.statut), "{notes:?}");
    assert!(
        notes.iter().filter(|c| c.metadonnees).count() >= 2,
        "instantané puis pochette : {notes:?}"
    );

    let vu = vu.lock().unwrap();
    assert_eq!(
        vu.abonnement,
        Some(json!({"subscribe": ["playback.*"]})),
        "abonnement au WebSocket"
    );
    // Le jeton part sur l'API, le WebSocket et la pochette du serveur.
    assert!(vu.chemins.contains(&"/api/v1/zones".to_string()));
    assert!(
        vu.chemins
            .contains(&"/api/v1/library/artwork/c0ffee".to_string())
    );
    assert!(vu.autorisations.len() >= 3, "{:?}", vu.autorisations);
    assert!(vu.autorisations.iter().all(|a| a == "Bearer jeton-d-essai"));
    let _ = std::fs::remove_dir_all(&cache);
}
