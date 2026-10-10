//! #3326 S2-c — une enceinte `player@v1` simulée contre le VRAI routeur.
//!
//! Le pair est écrit ici, sur `snow`, d'après `Sendspin/spec` 1.0.0-rc1
//! (`messaging.md`, `roles/player/v1.md`) : `client/init`, Noise `KKpsk2`
//! sous une PSK longue durée provisionnée, `client/hello` avec
//! `player@v1_support`, `client/state`, puis il lit et VÉRIFIE ce que Tune
//! envoie. La sortie est pilotée par l'API `OutputTarget`, prise dans le vrai
//! registre des sorties de `AppState`, comme le ferait l'orchestrateur.
//!
//! Inclus par `sendspin_point_d_acces_s2a` (`autotests = false`).
use super::*;
use serde_json::{Value, json};
use std::time::Duration;
use tune_core::outputs::{OutputTarget, PlayMedia, TransportState};
use tune_core::sendspin::magasin::{MagasinAppairage, MethodeAppairage};
use tune_core::sendspin::psk::{CategoriePsk, PskPair};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const ATTENTE: Duration = tune_server::routes::sendspin::DELAI_MESSAGE;
const TAUX: u32 = 44_100;

struct Banc {
    _temporaire: tempfile::TempDir,
    etat: tune_server::state::AppState,
    adresse: std::net::SocketAddr,
    tache: tokio::task::JoinHandle<()>,
    wav: std::path::PathBuf,
    pcm: Vec<u8>,
}
impl Drop for Banc {
    fn drop(&mut self) {
        self.tache.abort();
    }
}

/// Un WAV PCM 16 bits stéréo de `trames` trames, au contenu non trivial.
fn ecrire_wav(chemin: &std::path::Path, trames: usize) -> Vec<u8> {
    ecrire_wav_a(chemin, trames, TAUX, 0)
}

/// Idem à `taux`, avec un motif décalé par `graine` (deux pistes différentes).
fn ecrire_wav_a(chemin: &std::path::Path, trames: usize, taux: u32, graine: usize) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(trames * 4);
    for i in 0..trames {
        let g = ((i * 37 + graine) % 65_536) as u16 as i16;
        let d = ((i * 101 + 7 + graine * 3) % 65_536) as u16 as i16;
        pcm.extend_from_slice(&g.to_le_bytes());
        pcm.extend_from_slice(&d.to_le_bytes());
    }
    let mut f = Vec::new();
    let taille = pcm.len() as u32;
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + taille).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&taux.to_le_bytes());
    f.extend_from_slice(&(taux * 4).to_le_bytes());
    f.extend_from_slice(&4u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&taille.to_le_bytes());
    f.extend_from_slice(&pcm);
    std::fs::write(chemin, f).unwrap();
    pcm
}

impl Banc {
    /// `appaires` : identités dont la PSK longue durée est déjà en magasin.
    async fn nouveau(appaires: &[(&Identite, &PskPair)]) -> Self {
        let temporaire = tempfile::tempdir().unwrap();
        let db = temporaire.path().join("tune.db");
        let db = db.to_str().unwrap().to_owned();
        {
            let mut magasin =
                MagasinAppairage::ouvrir(std::path::Path::new(&format!("{db}.sendspin"))).unwrap();
            for (id, psk) in appaires {
                magasin
                    .conserver(&id.id(), psk, MethodeAppairage::Psk)
                    .unwrap();
            }
        }
        let config = tune_server::config::TuneConfig {
            db_path: db,
            ..Default::default()
        };
        let etat = tune_server::state::AppState::new(":memory:", 0, config).unwrap();
        let app = tune_server::routes::router(etat.clone());
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecoute.local_addr().unwrap();
        let tache = tokio::spawn(async move {
            axum::serve(ecoute, app).await.unwrap();
        });
        let wav = temporaire.path().join("piste.wav");
        // 0,5 s : dix morceaux de 50 ms, plus le reste éventuel.
        let pcm = ecrire_wav(&wav, (TAUX / 2) as usize);
        Self {
            _temporaire: temporaire,
            etat,
            adresse,
            tache,
            wav,
            pcm,
        }
    }

    async fn sortie(
        &self,
        client_id: &str,
    ) -> std::sync::Arc<tokio::sync::Mutex<Box<dyn OutputTarget>>> {
        let id = format!("sendspin:{client_id}");
        tokio::time::timeout(ATTENTE, async {
            loop {
                if let Some(s) = self.etat.outputs.lock().await.get(&id) {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("la sortie Sendspin doit etre enregistree apres le premier client/state")
    }

    async fn sortie_absente(&self, client_id: &str) -> bool {
        let id = format!("sendspin:{client_id}");
        !self.etat.outputs.lock().await.contains(&id)
    }
}

#[derive(Debug)]
enum Recu {
    Json(Value),
    Audio { ts: i64, avance: u32, pcm: Vec<u8> },
}

struct Pair {
    ws: Socket,
    transport: snow::TransportState,
}

impl Pair {
    async fn ouvrir(b: &Banc, id: &Identite, cle: &PskPair, support: Value) -> (Self, Value) {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/sendspin", b.adresse))
            .await
            .unwrap();
        let suite = Suite::ChaChaPoly;
        let init = json!({"type":"client/init","payload":{"client_id":id.id(),"version":1,"suite":suite.nom()}}).to_string();
        ws.send(Message::Text(init.clone().into())).await.unwrap();
        let reponse = match ws.next().await {
            Some(Ok(Message::Text(t))) => t.to_string(),
            autre => panic!("server/init attendu : {autre:?}"),
        };
        let v: Value = serde_json::from_str(&reponse).unwrap();
        let serveur_public: [u8; 32] = URL_SAFE_NO_PAD
            .decode(v["payload"]["server_id"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let prologue = [init.as_bytes(), reponse.as_bytes()].concat();
        let mut noise =
            Enceinte::nouvelle(id, &serveur_public, &prologue, suite, cle.secret()).etat;
        let premier: Value = match ws.next().await {
            Some(Ok(Message::Text(t))) => serde_json::from_str(&t).unwrap(),
            autre => panic!("noise/handshake attendu : {autre:?}"),
        };
        let donnees = URL_SAFE_NO_PAD
            .decode(premier["payload"]["data"].as_str().unwrap())
            .unwrap();
        let mut tampon = vec![0; MAX_NOISE];
        noise.read_message(&donnees, &mut tampon).unwrap();
        let n = noise.write_message(b"{}", &mut tampon).unwrap();
        let second = json!({"type":"noise/handshake","payload":{"data":URL_SAFE_NO_PAD.encode(&tampon[..n])}});
        ws.send(Message::Text(second.to_string().into()))
            .await
            .unwrap();
        let mut p = Self {
            ws,
            transport: noise.into_transport_mode().unwrap(),
        };
        let hello = p.json().await;
        assert_eq!(hello["type"], "server/hello");
        p.envoyer(
            "client/hello",
            json!({
                "name": "Enceinte simulee S2-c",
                "supported_roles": ["player@v1"],
                "player@v1_support": support,
                "supported_pair_methods": {"pairing_psk": {}},
                "unpaired_access": {"enabled": false},
            }),
        )
        .await;
        let activation = p.json().await;
        assert_eq!(activation["type"], "server/activate");
        (p, activation)
    }

    async fn envoyer(&mut self, typ: &str, charge: Value) {
        let texte = json!({"type":typ,"payload":charge}).to_string();
        let clair = [&[0u8][..], texte.as_bytes()].concat();
        let mut b = vec![0; MAX_NOISE];
        let n = self.transport.write_message(&clair, &mut b).unwrap();
        b.truncate(n);
        self.ws.send(Message::Binary(b.into())).await.unwrap();
    }

    async fn recevoir(&mut self) -> Recu {
        let b = tokio::time::timeout(ATTENTE, async {
            loop {
                match self
                    .ws
                    .next()
                    .await
                    .expect("WebSocket ferme")
                    .expect("trame")
                {
                    Message::Binary(b) => return b,
                    Message::Ping(_) | Message::Pong(_) => continue,
                    autre => panic!("trame binaire attendue, recu {autre:?}"),
                }
            }
        })
        .await
        .expect("le serveur s'est tu");
        let mut clair = vec![0; MAX_NOISE];
        let n = self.transport.read_message(&b, &mut clair).unwrap();
        match clair[0] {
            0 => Recu::Json(serde_json::from_slice(&clair[1..n]).unwrap()),
            4 => {
                assert!(n >= 13, "en-tete audio de 13 octets");
                Recu::Audio {
                    ts: i64::from_be_bytes(clair[1..9].try_into().unwrap()),
                    avance: u32::from_be_bytes(clair[9..13].try_into().unwrap()),
                    pcm: clair[13..n].to_vec(),
                }
            }
            t => panic!("type binaire inattendu {t}"),
        }
    }

    async fn json(&mut self) -> Value {
        match self.recevoir().await {
            Recu::Json(v) => v,
            Recu::Audio { .. } => panic!("JSON attendu, morceau audio recu"),
        }
    }

    /// Lit jusqu'au message `typ`, en rendant les morceaux audio croisés.
    async fn jusqu_a(&mut self, typ: &str) -> (Value, Vec<(i64, u32, Vec<u8>)>) {
        let mut audio = Vec::new();
        loop {
            match self.recevoir().await {
                Recu::Json(v) if v["type"] == typ => return (v, audio),
                Recu::Json(v) => panic!("{typ} attendu, recu {v}"),
                Recu::Audio { ts, avance, pcm } => audio.push((ts, avance, pcm)),
            }
        }
    }
}

fn support_pcm16() -> Value {
    json!({
        "buffer_capacity": 1_000_000,
        "supported_formats": [
            {"codec": "opus", "sample_rate": 48000, "bit_depth": 16, "channels": 2},
            {"codec": "pcm", "sample_rate": TAUX, "bit_depth": 16, "channels": 2},
        ]
    })
}

fn etat_client(commandes: Value) -> Value {
    json!({"available": true, "player": {
        "volume": 30, "muted": false, "output_delay_ms": 0,
        "required_lead_time_ms": 0, "min_buffer_ms": 0,
        "supported_commands": commandes,
    }})
}

fn media(b: &Banc) -> String {
    b.wav.to_str().unwrap().to_owned()
}

#[tokio::test]
async fn i3326_s2c_enceinte_appairee_devient_une_zone_et_recoit_le_pcm_horodate() {
    let id = Identite::generer();
    let lt = PskPair::pour_pair(&id.id(), [77; 32], CategoriePsk::LongueDuree).unwrap();
    let b = Banc::nouveau(&[(&id, &lt)]).await;
    let (mut p, activation) = Pair::ouvrir(&b, &id, &lt, support_pcm16()).await;

    // Rôle actif, aucune activité, puis group/update (MUST après la 1re activation).
    assert_eq!(activation["payload"]["activities"], json!([]));
    assert_eq!(activation["payload"]["active_roles"], json!(["player@v1"]));
    let groupe = p.json().await;
    assert_eq!(groupe["type"], "group/update");
    assert_eq!(groupe["payload"]["playback_state"], "stopped");

    // Avant le premier client/state : aucune sortie.
    assert!(b.sortie_absente(&id.id()).await);
    p.envoyer("client/state", etat_client(json!(["volume", "mute"])))
        .await;
    let sortie = b.sortie(&id.id()).await;
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(b.etat.backend.clone())
        .get_by_device_id(&format!("sendspin:{}", id.id()))
        .unwrap()
        .expect("la zone est creee automatiquement");
    assert_eq!(zone.output_type.as_deref(), Some("sendspin"));
    assert!(sortie.lock().await.is_available().await);

    // Horloge : server/time répond avec les trois horodatages.
    p.envoyer("client/time", json!({"client_transmitted": 123}))
        .await;
    let t = p.json().await;
    assert_eq!(t["type"], "server/time");
    assert_eq!(t["payload"]["client_transmitted"], 123);
    assert!(
        t["payload"]["server_received"].as_i64().unwrap()
            <= t["payload"]["server_transmitted"].as_i64().unwrap()
    );

    // Lecture.
    let chemin = media(&b);
    sortie
        .lock()
        .await
        .play_media(&PlayMedia {
            url: &chemin,
            file_path: Some(&chemin),
            duration_ms: Some(500),
            ..Default::default()
        })
        .await
        .expect("play_media");
    let a = p.json().await;
    assert_eq!(a["type"], "server/activate");
    assert_eq!(a["payload"]["activities"], json!(["playback"]));
    let g = p.json().await;
    assert_eq!(g["payload"]["playback_state"], "playing");
    let debut = p.json().await;
    assert_eq!(debut["type"], "stream/start");
    // Opus est en tête mais Tune ne le produit pas : premier PCM annoncé.
    assert_eq!(
        debut["payload"]["player"],
        json!({"codec":"pcm","sample_rate":TAUX,"channels":2,"bit_depth":16})
    );
    let emis = debut["payload"]["server_transmitted"].as_i64().unwrap();

    let mut recu = Vec::new();
    let mut morceaux = Vec::new();
    while recu.len() < b.pcm.len() {
        match p.recevoir().await {
            Recu::Audio { ts, avance, pcm } => {
                recu.extend_from_slice(&pcm);
                morceaux.push((ts, avance, pcm.len()));
            }
            Recu::Json(v) => panic!("audio attendu, recu {v}"),
        }
    }
    assert_eq!(
        recu, b.pcm,
        "le PCM recu est le PCM du fichier, octet pour octet"
    );
    let t0 = morceaux[0].0;
    assert!(
        t0 - emis >= tune_core::sendspin::lecteur::MARGE_DEPART_US,
        "le premier morceau est horodate assez loin apres stream/start"
    );
    let mut trames = 0u64;
    for (i, (ts, avance, octets)) in morceaux.iter().enumerate() {
        assert_eq!(
            *ts,
            t0 + (trames * 1_000_000 / u64::from(TAUX)) as i64,
            "ligne de temps continue (morceau {i})"
        );
        assert!(
            *avance > 0,
            "envoye en avance de son horodatage (morceau {i})"
        );
        let n = (*octets / 4) as u64;
        let duree = n * 1_000_000 / u64::from(TAUX);
        assert!(duree <= 150_000, "morceau {i} de {duree} us > 150 ms");
        if i + 1 < morceaux.len() {
            assert!(duree >= 15_000, "morceau {i} de {duree} us < 15 ms");
        }
        trames += n;
    }

    // Volume et sourdine : server/command, valeurs de la spécification.
    sortie.lock().await.set_volume(0.5).await.unwrap();
    let c = p.json().await;
    assert_eq!(c["type"], "server/command");
    assert_eq!(
        c["payload"],
        json!({"player":{"command":"volume","volume":50}})
    );
    sortie.lock().await.set_mute(true).await.unwrap();
    let c = p.json().await;
    assert_eq!(
        c["payload"],
        json!({"player":{"command":"mute","mute":true}})
    );

    // Fin naturelle : signalée quand la ligne de temps est jouée, pas à l'envoi.
    let fin = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let s = sortie.lock().await.get_status().await.unwrap();
            if s.ended_naturally {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fin naturelle");
    assert_eq!(fin.state, TransportState::Stopped);
    // Même processus, même horloge : la fin n'est annoncée qu'une fois la
    // ligne de temps entièrement jouée, pas quand le dernier morceau est parti.
    let fin_ligne = t0 + (trames * 1_000_000 / u64::from(TAUX)) as i64;
    assert!(
        tune_core::sendspin::horloge::maintenant_us() >= fin_ligne,
        "fin annoncee avant la fin de la ligne de temps"
    );

    // Arrêt : stream/end, groupe arrêté, activité retirée.
    sortie.lock().await.stop().await.unwrap();
    assert_eq!(p.json().await["type"], "stream/end");
    assert_eq!(p.json().await["payload"]["playback_state"], "stopped");
    let a = p.json().await;
    assert_eq!(a["type"], "server/activate");
    assert_eq!(a["payload"]["activities"], json!([]));

    // Départ : la sortie disparaît, la zone passe hors ligne.
    p.ws.close(None).await.unwrap();
    drop(sortie);
    tokio::time::timeout(ATTENTE, async {
        while !b.sortie_absente(&id.id()).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("la sortie doit etre retiree a la deconnexion");
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(b.etat.backend.clone())
        .get_by_device_id(&format!("sendspin:{}", id.id()))
        .unwrap()
        .unwrap();
    assert!(!zone.online, "zone hors ligne apres le depart");
}

#[tokio::test]
async fn i3326_s2c_pause_reprise_et_seek() {
    let id = Identite::generer();
    let lt = PskPair::pour_pair(&id.id(), [78; 32], CategoriePsk::LongueDuree).unwrap();
    let b = Banc::nouveau(&[(&id, &lt)]).await;
    let (mut p, _) = Pair::ouvrir(&b, &id, &lt, support_pcm16()).await;
    assert_eq!(p.json().await["type"], "group/update");
    // Une avance de départ d'une seconde : la pause tombe avant la fin.
    let mut etat = etat_client(json!(["volume"]));
    etat["player"]["required_lead_time_ms"] = json!(1000);
    p.envoyer("client/state", etat).await;
    let sortie = b.sortie(&id.id()).await;
    let chemin = media(&b);
    sortie
        .lock()
        .await
        .play_media(&PlayMedia {
            url: &chemin,
            file_path: Some(&chemin),
            duration_ms: Some(500),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(p.json().await["type"], "server/activate");
    assert_eq!(p.json().await["type"], "group/update");
    let debut = p.json().await;
    assert_eq!(debut["type"], "stream/start");
    let t_start = debut["payload"]["server_transmitted"].as_i64().unwrap();
    let premier = loop {
        if let Recu::Audio { ts, .. } = p.recevoir().await {
            break ts;
        }
    };
    assert!(
        premier - t_start >= 1_000_000,
        "required_lead_time_ms respecte"
    );

    // Sourdine non proposée : refus nommé, rien sur le fil.
    let refus = sortie.lock().await.set_mute(true).await.unwrap_err();
    assert!(refus.contains("mute"), "{refus}");

    sortie.lock().await.pause().await.unwrap();
    let (_, _) = p.jusqu_a("stream/end").await;
    assert_eq!(p.json().await["payload"]["playback_state"], "stopped");
    assert_eq!(
        sortie.lock().await.get_status().await.unwrap().state,
        TransportState::Paused
    );
    sortie.lock().await.resume().await.unwrap();
    let g = p.json().await;
    assert_eq!(g["payload"]["playback_state"], "playing");
    assert_eq!(
        p.json().await["type"],
        "stream/start",
        "reprise = nouveau flux"
    );

    sortie.lock().await.seek(100).await.unwrap();
    let (clear, _) = p.jusqu_a("stream/clear").await;
    assert_eq!(clear["payload"]["roles"], json!(["player"]));
    // Après le clear, l'audio repart de 100 ms : 0,4 s restante.
    let mut octets = 0usize;
    let attendu = b.pcm.len() - (TAUX as usize / 10) * 4;
    let mut premier_apres = None;
    while octets < attendu {
        if let Recu::Audio { pcm, .. } = p.recevoir().await {
            if premier_apres.is_none() {
                premier_apres = Some(pcm[..4].to_vec());
            }
            octets += pcm.len();
        }
    }
    let decalage = (TAUX as usize / 10) * 4;
    assert_eq!(
        premier_apres.unwrap(),
        b.pcm[decalage..decalage + 4].to_vec(),
        "le seek reprend a la bonne trame"
    );
    assert_eq!(octets, attendu);
}

#[tokio::test]
async fn i3326_s2c_session_sentinelle_ne_devient_jamais_une_zone() {
    // Contre-épreuve intégrée : même enceinte, même hello, NON appairée.
    let id = Identite::generer();
    let b = Banc::nouveau(&[]).await;
    let (mut p, activation) = Pair::ouvrir(&b, &id, &PskPair::sentinelle(), support_pcm16()).await;
    assert_eq!(activation["payload"]["activities"], json!([]));
    assert!(
        activation["payload"]
            .get("active_roles")
            .is_none_or(|r| r == &json!([])),
        "aucun role actif sans appairage : {activation}"
    );
    p.envoyer("client/state", etat_client(json!(["volume"])))
        .await;
    // Le serveur a traité le client/state (la réponse au client/time le suit).
    p.envoyer("client/time", json!({"client_transmitted": 1}))
        .await;
    assert_eq!(p.json().await["type"], "server/time");
    assert!(
        b.sortie_absente(&id.id()).await,
        "une session Sentinelle n'enregistre aucune sortie"
    );
}

/// Démarre la lecture et consomme activate + group/update ; rend le
/// `stream/start`.
async fn demarrer(
    p: &mut Pair,
    sortie: &std::sync::Arc<tokio::sync::Mutex<Box<dyn OutputTarget>>>,
    chemin: &str,
    taux: Option<u32>,
    duree_ms: u64,
) -> Value {
    sortie
        .lock()
        .await
        .play_media(&PlayMedia {
            url: chemin,
            file_path: Some(chemin),
            duration_ms: Some(duree_ms),
            sample_rate: taux,
            ..Default::default()
        })
        .await
        .expect("play_media");
    assert_eq!(p.json().await["type"], "server/activate");
    assert_eq!(p.json().await["payload"]["playback_state"], "playing");
    let debut = p.json().await;
    assert_eq!(debut["type"], "stream/start");
    debut
}

#[tokio::test]
async fn i3326_s2c_flac_trames_completes_sans_perte() {
    // « Servers MUST support the flac and pcm codecs » ; « flac: one or more
    // complete FLAC frames. codec_header is required ».
    let id = Identite::generer();
    let lt = PskPair::pour_pair(&id.id(), [79; 32], CategoriePsk::LongueDuree).unwrap();
    let b = Banc::nouveau(&[(&id, &lt)]).await;
    let support = json!({"buffer_capacity": 1_000_000, "supported_formats": [
        {"codec": "flac", "sample_rate": TAUX, "bit_depth": 16, "channels": 2},
        {"codec": "pcm", "sample_rate": TAUX, "bit_depth": 16, "channels": 2}]});
    let (mut p, _) = Pair::ouvrir(&b, &id, &lt, support).await;
    assert_eq!(p.json().await["type"], "group/update");
    p.envoyer("client/state", etat_client(json!([]))).await;
    let sortie = b.sortie(&id.id()).await;
    let debut = demarrer(&mut p, &sortie, &media(&b), Some(TAUX), 500).await;
    let joueur = &debut["payload"]["player"];
    assert_eq!(joueur["codec"], "flac");
    let entete = base64::engine::general_purpose::STANDARD
        .decode(
            joueur["codec_header"]
                .as_str()
                .expect("codec_header requis en FLAC"),
        )
        .expect("Base64 standard");
    assert_eq!(&entete[..4], b"fLaC");
    let mut flux = entete;
    let mut ts_attendu = None;
    let mut trames_recues = 0u64;
    while trames_recues < (TAUX / 2) as u64 {
        let Recu::Audio { ts, pcm: trame, .. } = p.recevoir().await else {
            panic!("audio attendu");
        };
        // Chaque morceau est UNE trame FLAC complète : synchro 0xFFF8 en tête.
        assert_eq!(
            &trame[..2],
            &[0xFF, 0xF8],
            "trame FLAC complete en tete de morceau"
        );
        if let Some(t) = ts_attendu {
            assert_eq!(ts, t, "ligne de temps continue en FLAC");
        }
        flux.extend_from_slice(&trame);
        let tmp = tempfile::Builder::new().suffix(".flac").tempfile().unwrap();
        std::fs::write(tmp.path(), &flux).unwrap();
        let n = tune_core::audio::decode::decode_to_pcm(
            tmp.path().to_str().unwrap(),
            None,
            None,
            0.0,
            0.0,
        )
        .unwrap()
        .samples_i32
        .len() as u64
            / 2;
        let morceau = n - trames_recues;
        assert!(
            morceau * 1_000_000 / u64::from(TAUX) <= 150_000,
            "morceau <= 150 ms"
        );
        trames_recues = n;
        ts_attendu = Some(ts + (morceau * 1_000_000 / u64::from(TAUX)) as i64);
    }
    let tmp = tempfile::Builder::new().suffix(".flac").tempfile().unwrap();
    std::fs::write(tmp.path(), &flux).unwrap();
    let dec =
        tune_core::audio::decode::decode_to_pcm(tmp.path().to_str().unwrap(), None, None, 0.0, 0.0)
            .unwrap();
    let attendu: Vec<i32> = b
        .pcm
        .chunks_exact(2)
        .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
        .collect();
    assert!(
        dec.samples_i32 == attendu,
        "FLAC recu = PCM du fichier, echantillon pour echantillon"
    );
}

#[tokio::test]
async fn i3326_s2c_enchainement_sans_coupure_et_changement_de_format_en_place() {
    // « Track transitions: stream commands SHOULD NOT be sent, except
    // stream/start to update the existing stream configuration » et « Servers
    // MUST timestamp the first chunk in the new format to follow the last
    // chunk in the previous format on the existing timeline ».
    let id = Identite::generer();
    let lt = PskPair::pour_pair(&id.id(), [80; 32], CategoriePsk::LongueDuree).unwrap();
    let b = Banc::nouveau(&[(&id, &lt)]).await;
    let support = json!({"buffer_capacity": 1_000_000, "supported_formats": [
        {"codec": "pcm", "sample_rate": TAUX, "bit_depth": 16, "channels": 2},
        {"codec": "pcm", "sample_rate": 48000, "bit_depth": 16, "channels": 2}]});
    let (mut p, _) = Pair::ouvrir(&b, &id, &lt, support).await;
    assert_eq!(p.json().await["type"], "group/update");
    p.envoyer("client/state", etat_client(json!([]))).await;
    let sortie = b.sortie(&id.id()).await;
    let seconde = b._temporaire.path().join("seconde.wav");
    let pcm2 = ecrire_wav_a(&seconde, 48_000 / 2, 48_000, 11);
    let seconde = seconde.to_str().unwrap().to_owned();

    let debut = demarrer(&mut p, &sortie, &media(&b), Some(TAUX), 500).await;
    assert_eq!(debut["payload"]["player"]["sample_rate"], TAUX);
    sortie
        .lock()
        .await
        .set_next_media(&PlayMedia {
            url: &seconde,
            file_path: Some(&seconde),
            duration_ms: Some(500),
            sample_rate: Some(48_000),
            ..Default::default()
        })
        .await
        .unwrap();

    // Piste 1 : tout le PCM, puis un stream/start EN PLACE (rien d'autre).
    let (bascule, morceaux1) = p.jusqu_a("stream/start").await;
    assert_eq!(
        bascule["payload"]["player"],
        json!({"codec":"pcm","sample_rate":48000,"channels":2,"bit_depth":16})
    );
    let pcm1: Vec<u8> = morceaux1.iter().flat_map(|m| m.2.clone()).collect();
    assert_eq!(pcm1, b.pcm, "piste 1 entiere, octet pour octet");
    let mut trames = 0u64;
    let t0 = morceaux1[0].0;
    for (i, (ts, _, octets)) in morceaux1.iter().enumerate() {
        assert_eq!(
            *ts,
            t0 + (trames * 1_000_000 / u64::from(TAUX)) as i64,
            "piste 1 morceau {i}"
        );
        trames += (octets.len() / 4) as u64;
    }
    let fin1 = t0 + (trames * 1_000_000 / u64::from(TAUX)) as i64;

    // Piste 2 : premier morceau horodaté À LA SUITE du dernier de la piste 1.
    let mut pcm_recu2 = Vec::new();
    let mut trames2 = 0u64;
    while pcm_recu2.len() < pcm2.len() {
        match p.recevoir().await {
            Recu::Audio { ts, pcm, .. } => {
                assert_eq!(
                    ts,
                    fin1 + (trames2 * 1_000_000 / 48_000) as i64,
                    "ligne de temps continue a travers le changement de format"
                );
                trames2 += (pcm.len() / 4) as u64;
                pcm_recu2.extend_from_slice(&pcm);
            }
            Recu::Json(v) => panic!("ni stream/clear ni stream/end entre deux pistes : {v}"),
        }
    }
    assert_eq!(pcm_recu2, pcm2, "piste 2 entiere, octet pour octet");

    // L'état suit la ligne de temps : piste 2 une fois la frontière jouée.
    let s = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let s = sortie.lock().await.get_status().await.unwrap();
            if s.current_uri.as_deref() == Some(seconde.as_str()) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("la piste 2 devient la piste courante");
    assert!(tune_core::sendspin::horloge::maintenant_us() >= fin1);
    assert!(
        s.position_ms < 500,
        "la position repart de zero sur la piste 2"
    );
    assert_eq!(s.state, TransportState::Playing);
}

#[tokio::test]
async fn i3326_s2c_preference_changee_en_lecture_stream_start_en_place_sans_trou() {
    // « When format changes while a player stream is active, the server
    // re-derives the stream format and sends a stream/start if it changed ».
    let id = Identite::generer();
    let lt = PskPair::pour_pair(&id.id(), [81; 32], CategoriePsk::LongueDuree).unwrap();
    let b = Banc::nouveau(&[(&id, &lt)]).await;
    let support = json!({"buffer_capacity": 1_000_000, "supported_formats": [
        {"codec": "pcm", "sample_rate": TAUX, "bit_depth": 16, "channels": 2},
        {"codec": "pcm", "sample_rate": TAUX, "bit_depth": 24, "channels": 2}]});
    let (mut p, _) = Pair::ouvrir(&b, &id, &lt, support).await;
    assert_eq!(p.json().await["type"], "group/update");
    p.envoyer("client/state", etat_client(json!([]))).await;
    let sortie = b.sortie(&id.id()).await;
    let longue = b._temporaire.path().join("longue.wav");
    let pcm = ecrire_wav_a(&longue, TAUX as usize * 4, TAUX, 5);
    let longue = longue.to_str().unwrap().to_owned();
    let debut = demarrer(&mut p, &sortie, &longue, Some(TAUX), 4_000).await;
    assert_eq!(debut["payload"]["player"]["bit_depth"], 16);

    // Quelques morceaux en 16 bits, puis l'enceinte préfère le 24 bits.
    let mut morceaux16 = Vec::new();
    while morceaux16.len() < 3 {
        if let Recu::Audio { ts, pcm, .. } = p.recevoir().await {
            morceaux16.push((ts, pcm));
        }
    }
    let mut etat = etat_client(json!([]));
    etat["player"]["format"] =
        json!({"codec":"pcm","sample_rate":TAUX,"channels":2,"bit_depth":24});
    p.envoyer("client/state", etat).await;
    let (bascule, suite16) = p.jusqu_a("stream/start").await;
    assert_eq!(bascule["payload"]["player"]["bit_depth"], 24);
    morceaux16.extend(suite16.into_iter().map(|(ts, _, pcm)| (ts, pcm)));
    let t0 = morceaux16[0].0;
    let mut trames = 0u64;
    let mut recu: Vec<i32> = Vec::new();
    for (ts, pcm) in &morceaux16 {
        assert_eq!(*ts, t0 + (trames * 1_000_000 / u64::from(TAUX)) as i64);
        trames += (pcm.len() / 4) as u64;
        recu.extend(
            pcm.chunks_exact(2)
                .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]]))),
        );
    }
    let attendu_ts = t0 + (trames * 1_000_000 / u64::from(TAUX)) as i64;
    // Le premier morceau en 24 bits suit le dernier en 16 bits, sans trou ni
    // recouvrement, et reprend à la trame suivante du fichier.
    let mut premier24 = true;
    while recu.len() < pcm.len() / 2 {
        let Recu::Audio { ts, pcm: p24, .. } = p.recevoir().await else {
            panic!("audio attendu");
        };
        if premier24 {
            assert_eq!(
                ts, attendu_ts,
                "le 24 bits suit le 16 bits sur la meme ligne de temps"
            );
            premier24 = false;
        }
        recu.extend(
            p24.chunks_exact(3)
                .map(|c| i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 16),
        );
    }
    let attendu: Vec<i32> = pcm
        .chunks_exact(2)
        .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
        .collect();
    assert!(
        recu == attendu,
        "rien de renvoye, rien de saute au changement de format"
    );
    sortie.lock().await.stop().await.unwrap();
}

#[tokio::test]
async fn i3326_s2c_enceinte_indisponible_le_serveur_arrete_sa_lecture() {
    // *External Source Handling* : groupe solo → stream/end et group/update
    // `stopped` ; pas de reprise automatique.
    let id = Identite::generer();
    let lt = PskPair::pour_pair(&id.id(), [82; 32], CategoriePsk::LongueDuree).unwrap();
    let b = Banc::nouveau(&[(&id, &lt)]).await;
    let (mut p, _) = Pair::ouvrir(&b, &id, &lt, support_pcm16()).await;
    assert_eq!(p.json().await["type"], "group/update");
    let mut etat = etat_client(json!([]));
    etat["player"]["required_lead_time_ms"] = json!(2000);
    p.envoyer("client/state", etat.clone()).await;
    let sortie = b.sortie(&id.id()).await;
    demarrer(&mut p, &sortie, &media(&b), Some(TAUX), 500).await;
    etat["available"] = json!(false);
    p.envoyer("client/state", etat).await;
    let (_, _) = p.jusqu_a("stream/end").await;
    assert_eq!(p.json().await["payload"]["playback_state"], "stopped");
    assert_eq!(p.json().await["payload"]["activities"], json!([]));
    let s = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let s = sortie.lock().await.get_status().await.unwrap();
            if s.state == TransportState::Stopped {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("la sortie s'arrete");
    assert!(
        !s.ended_naturally,
        "un arret force n'est pas une fin naturelle"
    );
    assert!(!sortie.lock().await.is_available().await);
}

#[tokio::test]
async fn i3326_s2c_client_leave_arrete_la_lecture_sans_fin_naturelle() {
    // `client/leave` : traité comme l'indisponibilité, l'enceinte reste
    // disponible et ne reprend pas seule.
    let id = Identite::generer();
    let lt = PskPair::pour_pair(&id.id(), [83; 32], CategoriePsk::LongueDuree).unwrap();
    let b = Banc::nouveau(&[(&id, &lt)]).await;
    let (mut p, _) = Pair::ouvrir(&b, &id, &lt, support_pcm16()).await;
    assert_eq!(p.json().await["type"], "group/update");
    p.envoyer("client/state", etat_client(json!([]))).await;
    let sortie = b.sortie(&id.id()).await;
    demarrer(&mut p, &sortie, &media(&b), Some(TAUX), 500).await;
    p.envoyer("client/leave", json!({})).await;
    let (_, _) = p.jusqu_a("stream/end").await;
    assert_eq!(p.json().await["payload"]["playback_state"], "stopped");
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let s = sortie.lock().await.get_status().await.unwrap();
    assert_eq!(s.state, TransportState::Stopped);
    assert!(
        !s.ended_naturally,
        "client/leave n'est pas une fin naturelle"
    );
    assert!(
        sortie.lock().await.is_available().await,
        "l'enceinte reste disponible"
    );
}

#[path = "lecteur_aiosendspin_3326.rs"]
mod aiosendspin;
