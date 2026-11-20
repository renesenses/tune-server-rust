//! La boucle de synchronisation face à un faux serveur Tune : vraie
//! WebSocket, vraie requête `GET /api/v1/zones`, vraie pochette. Aucun D-Bus :
//! le notifieur retient ce qu'il aurait signalé.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tune_mpris::client::{ClientTune, JETON_REQUIS, POCHETTE_MAX};
use tune_mpris::etat::{Changements, Etat, Statut};
use tune_mpris::synchro::{Delais, Notifieur, suivre, suivre_avec};

const JETON: &str = "jeton-d-essai";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfaux";

struct Notes(mpsc::UnboundedSender<Changements>);

#[async_trait::async_trait]
impl Notifieur for Notes {
    async fn notifier(&self, c: Changements) {
        let _ = self.0.send(c);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Instantané, quatre évènements, puis la connexion reste ouverte.
    Normal,
    /// Accepte le WebSocket puis ne lit plus rien : aucun pong ne revient,
    /// la connexion TCP reste ouverte (câble débranché, NAS en veille).
    Muet,
    /// Une première session normale, puis la connexion se ferme et toute
    /// requête suivante reçoit 401 (jeton expiré en cours de route).
    ExpireEnRoute,
}

/// Ce que le faux serveur a vu.
#[derive(Default)]
struct Vu {
    autorisations: Vec<String>,
    abonnement: Option<Value>,
    chemins: Vec<String>,
    websockets: usize,
}

struct Faux {
    mode: Mode,
    vu: Mutex<Vu>,
    refuser: AtomicBool,
}

async fn lire_entete(flux: &mut TcpStream) -> Option<String> {
    let mut tampon = Vec::new();
    let mut morceau = [0u8; 1024];
    while !tampon.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = flux.read(&mut morceau).await.ok()?;
        if n == 0 {
            return None;
        }
        tampon.extend_from_slice(&morceau[..n]);
    }
    Some(String::from_utf8_lossy(&tampon).to_string())
}

async fn repondre_http(mut flux: TcpStream, faux: Arc<Faux>) {
    let Some(requete) = lire_entete(&mut flux).await else {
        return;
    };
    let chemin = requete.split_whitespace().nth(1).unwrap_or("").to_string();
    {
        let mut v = faux.vu.lock().unwrap();
        v.chemins.push(chemin.clone());
        for ligne in requete.lines() {
            if let Some((k, val)) = ligne.split_once(':')
                && k.eq_ignore_ascii_case("authorization")
            {
                v.autorisations.push(val.trim().to_string());
            }
        }
    }
    let (statut, type_mime, corps): (&str, Option<&str>, Vec<u8>) = if faux
        .refuser
        .load(Ordering::SeqCst)
    {
        ("401 Unauthorized", None, Vec::new())
    } else if chemin == "/api/v1/zones" {
        let mut piste = json!({"track_id": 3, "title": "All Blues", "artist_name": "Miles Davis", "duration_ms": 693_000});
        if faux.mode == Mode::Normal {
            piste["cover_path"] = json!("c0ffee");
        }
        let zones = json!([
            {"id": 1, "name": "Bureau", "state": "stopped"},
            {"id": 4, "name": "Salon", "state": "paused", "position_ms": 1_000, "volume": 0.5,
             "volume_curve": "linear", "current_track": piste}
        ]);
        (
            "200 OK",
            Some("application/json"),
            zones.to_string().into_bytes(),
        )
    } else if chemin == "/api/v1/library/artwork/c0ffee" || chemin == "/autre/x.png" {
        ("200 OK", Some("image/png"), PNG.to_vec())
    } else if chemin == "/grosse.png" {
        ("200 OK", Some("image/png"), vec![0u8; POCHETTE_MAX + 1])
    } else if chemin == "/sans-type" {
        ("200 OK", None, PNG.to_vec())
    } else {
        ("404 Not Found", Some("text/plain"), b"introuvable".to_vec())
    };
    let mut tete = format!(
        "HTTP/1.1 {statut}\r\nContent-Length: {}\r\nConnection: close\r\n",
        corps.len()
    );
    if let Some(t) = type_mime {
        tete.push_str(&format!("Content-Type: {t}\r\n"));
    }
    tete.push_str("\r\n");
    let _ = flux.write_all(tete.as_bytes()).await;
    let _ = flux.write_all(&corps).await;
}

// La signature du rappel est celle de tungstenite (`ErrorResponse` en erreur).
#[allow(clippy::result_large_err)]
async fn servir_ws(flux: TcpStream, faux: Arc<Faux>) {
    if faux.refuser.load(Ordering::SeqCst) {
        return repondre_http(flux, faux).await;
    }
    faux.vu.lock().unwrap().websockets += 1;
    let faux_poignee = faux.clone();
    let mut ws = tokio_tungstenite::accept_hdr_async(flux, move |req: &Request, rep: Response| {
        if let Some(a) = req.headers().get("authorization") {
            faux_poignee
                .vu
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
        faux.vu.lock().unwrap().abonnement = serde_json::from_str(&t).ok();
    }
    if faux.mode == Mode::Muet {
        // Plus rien n'est lu : tungstenite ne répond donc plus aux pings.
        std::future::pending::<()>().await;
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
    if faux.mode == Mode::ExpireEnRoute {
        tokio::time::sleep(Duration::from_millis(200)).await;
        faux.refuser.store(true, Ordering::SeqCst);
        let _ = ws.close(None).await;
        return;
    }
    // Garder la connexion ouverte : la fermeture est un autre essai.
    while ws.next().await.is_some() {}
}

/// Aiguille sur la ligne de requête entière (`GET /ws `), lue par `peek`
/// jusqu'à l'avoir : un premier paquet peut en contenir moins.
async fn est_un_websocket(flux: &TcpStream) -> bool {
    let mut debut = [0u8; 64];
    for _ in 0..200 {
        let n = flux.peek(&mut debut).await.unwrap_or(0);
        if n >= 8 || debut[..n].contains(&b'\n') {
            return debut[..n].starts_with(b"GET /ws ") || debut[..n].starts_with(b"GET /ws?");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

async fn faux_serveur(mode: Mode) -> (String, Arc<Faux>) {
    let ecoute = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = format!("http://{}", ecoute.local_addr().unwrap());
    let faux = Arc::new(Faux {
        mode,
        vu: Mutex::new(Vu::default()),
        refuser: AtomicBool::new(false),
    });
    let faux_boucle = faux.clone();
    tokio::spawn(async move {
        loop {
            let (flux, _) = ecoute.accept().await.unwrap();
            let faux = faux_boucle.clone();
            tokio::spawn(async move {
                if est_un_websocket(&flux).await {
                    servir_ws(flux, faux).await;
                } else {
                    repondre_http(flux, faux).await;
                }
            });
        }
    });
    (adresse, faux)
}

async fn attendre<F: Fn() -> bool>(condition: F, quoi: &str) {
    let fin = Instant::now() + Duration::from_secs(10);
    while Instant::now() < fin {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("délai dépassé : {quoi}");
}

fn client(adresse: &str) -> ClientTune {
    let mut c = ClientTune::new(adresse, Some(JETON));
    c.zone_id = 4;
    c
}

#[tokio::test]
async fn instantane_puis_evenements_de_la_zone_puis_pochette_en_cache() {
    let racine = tempfile::tempdir().unwrap();
    let cache = racine.path().to_path_buf();
    // Seul essai de ce binaire qui lit le cache : les autres servent des
    // pistes sans pochette.
    unsafe { std::env::set_var("XDG_CACHE_HOME", &cache) };

    let (adresse, faux) = faux_serveur(Mode::Normal).await;
    let etat = Arc::new(Mutex::new(Etat::new(4, "", Instant::now())));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let tache = tokio::spawn(suivre(client(&adresse), etat.clone(), Arc::new(Notes(tx))));

    let e = || etat.lock().unwrap();
    attendre(
        || e().nom_de_zone == "Salon" && e().piste.is_some(),
        "instantané",
    )
    .await;
    attendre(|| e().statut == Statut::Lecture, "playback.resumed").await;
    attendre(
        || (e().gain - 0.25).abs() < 1e-9,
        "playback.volume de la zone 4",
    )
    .await;
    attendre(|| e().pochette.is_some(), "pochette").await;

    let pochette = e().pochette.clone().unwrap();
    assert!(pochette.starts_with("file://"), "{pochette}");
    let fichier = std::fs::read_dir(cache.join("tune-mpris"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|f| f.path().extension().is_some_and(|x| x == "png"))
        .expect("pochette rangée en .png");
    assert_eq!(std::fs::read(fichier.path()).unwrap(), PNG);
    assert!(e().position_ms(Instant::now()) >= 60_000);

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

    let vu = faux.vu.lock().unwrap();
    assert_eq!(vu.abonnement, Some(json!({"subscribe": ["playback.*"]})));
    // Le jeton part sur l'API, le WebSocket et la pochette du serveur.
    assert!(vu.chemins.contains(&"/api/v1/zones".to_string()));
    assert!(
        vu.chemins
            .contains(&"/api/v1/library/artwork/c0ffee".to_string())
    );
    assert!(vu.autorisations.len() >= 3, "{:?}", vu.autorisations);
    assert!(vu.autorisations.iter().all(|a| a == "Bearer jeton-d-essai"));
}

#[tokio::test]
async fn un_serveur_muet_est_tenu_pour_perdu_au_delai_de_pong() {
    let (adresse, faux) = faux_serveur(Mode::Muet).await;
    let etat = Arc::new(Mutex::new(Etat::new(4, "", Instant::now())));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let delais = Delais {
        ping: Duration::from_millis(200),
        pong: Duration::from_millis(300),
        tenue: Duration::from_secs(10),
    };
    let tache = tokio::spawn(suivre_avec(
        client(&adresse),
        etat.clone(),
        Arc::new(Notes(tx)),
        delais,
    ));
    // La connexion TCP n'est jamais fermée : seule l'absence de pong peut
    // faire tomber la première session et en ouvrir une seconde.
    attendre(
        || faux.vu.lock().unwrap().websockets >= 2,
        "reconnexion après le délai de pong",
    )
    .await;
    tache.abort();
    let mut deconnecte = false;
    while let Ok(c) = rx.try_recv() {
        deconnecte |= c.statut && c.metadonnees;
    }
    assert!(
        deconnecte,
        "la zone est repassée à Stopped sans métadonnées entre les deux"
    );
}

#[tokio::test]
async fn un_jeton_expire_en_cours_de_route_arrete_la_boucle() {
    let (adresse, _faux) = faux_serveur(Mode::ExpireEnRoute).await;
    let etat = Arc::new(Mutex::new(Etat::new(4, "", Instant::now())));
    let (tx, _rx) = mpsc::unbounded_channel();
    let fin = tokio::time::timeout(
        Duration::from_secs(10),
        suivre(client(&adresse), etat.clone(), Arc::new(Notes(tx))),
    )
    .await
    .expect("la boucle doit s'arrêter au lieu de réessayer sans fin");
    assert_eq!(fin, JETON_REQUIS);
    assert_eq!(etat.lock().unwrap().statut, Statut::Arret);
    assert!(etat.lock().unwrap().piste.is_none());
}

#[tokio::test]
async fn le_jeton_ne_part_pas_vers_une_pochette_d_un_autre_hote() {
    let (tune, faux_tune) = faux_serveur(Mode::Normal).await;
    let (ailleurs, faux_ailleurs) = faux_serveur(Mode::Normal).await;
    let c = client(&tune);
    let (octets, type_mime) = c
        .telecharger(&format!("{ailleurs}/autre/x.png"))
        .await
        .unwrap();
    assert_eq!((octets.as_slice(), type_mime.as_str()), (PNG, "image/png"));
    assert!(
        faux_ailleurs.vu.lock().unwrap().autorisations.is_empty(),
        "jeton envoyé à un autre hôte"
    );
    c.telecharger(&format!("{tune}/autre/x.png")).await.unwrap();
    assert_eq!(
        faux_tune.vu.lock().unwrap().autorisations,
        vec!["Bearer jeton-d-essai".to_string()]
    );
}

#[tokio::test]
async fn pochette_trop_grosse_ou_sans_type_refusee() {
    let (tune, _) = faux_serveur(Mode::Normal).await;
    let c = client(&tune);
    let e = c
        .telecharger(&format!("{tune}/grosse.png"))
        .await
        .unwrap_err();
    assert!(e.contains("plus de"), "{e}");
    let e = c
        .telecharger(&format!("{tune}/sans-type"))
        .await
        .unwrap_err();
    assert!(e.contains("Content-Type"), "{e}");
}
