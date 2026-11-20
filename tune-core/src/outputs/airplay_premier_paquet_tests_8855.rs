//! #8855 (GO de Bertrand, 20/11) — AirPlay 1 : le volume de la zone part au
//! début de CHAQUE piste, après RECORD et avant le premier paquet audio.
//!
//! Banc : un faux récepteur RTSP + UDP sur `127.0.0.1` qui note, dans l'ordre
//! d'arrivée, RECORD, chaque `SET_PARAMETER volume` et le premier paquet RTP
//! de chaque session ; un faux `/stream` WAV court. Aucun appareil réel.

use super::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;

const APPAREIL: &str = "airplay:fixture-8855-paquet";

type Journal = Arc<std::sync::Mutex<Vec<String>>>;

/// Un `/stream` WAV d'une demi-seconde, servi d'un bloc.
async fn stream_court() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut requete = Vec::new();
                let mut octet = [0u8; 1];
                while !requete.ends_with(b"\r\n\r\n") {
                    if sock.read(&mut octet).await.unwrap_or(0) == 0 {
                        return;
                    }
                    requete.push(octet[0]);
                }
                let entetes =
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(entetes.as_bytes()).await;
                let wav = crate::audio::wav::build_wav_header_streaming(2, 44_100, 16);
                let _ = sock.write_all(&wav).await;
                let _ = sock.write_all(&vec![0x11_u8; 22_050 * 4]).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}/stream/court-8855.wav")
}

/// Le faux récepteur : chaque connexion RTSP est une session ; le port audio
/// UDP est commun. Le journal note `RECORD`, `VOLUME <corps>` et `RTP` (le
/// premier paquet après chaque RECORD).
async fn faux_recepteur() -> (u16, Journal) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let audio = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port_audio = audio.local_addr().unwrap().port();
    let journal: Journal = Arc::new(std::sync::Mutex::new(Vec::new()));
    let attend_rtp = Arc::new(AtomicBool::new(false));

    let (journal_udp, attend_udp) = (journal.clone(), attend_rtp.clone());
    tokio::spawn(async move {
        let mut tampon = vec![0u8; 2_048];
        while audio.recv(&mut tampon).await.is_ok() {
            if attend_udp.swap(false, Ordering::SeqCst) {
                journal_udp.lock().unwrap().push("RTP".into());
            }
        }
    });

    let journal_rtsp = journal.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let (journal, attend_rtp) = (journal_rtsp.clone(), attend_rtp.clone());
            tokio::spawn(async move {
                let mut lecteur = BufReader::new(socket);
                loop {
                    let mut ligne = String::new();
                    if lecteur.read_line(&mut ligne).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let methode = ligne.split_whitespace().next().unwrap_or("").to_string();
                    let mut taille = 0usize;
                    loop {
                        let mut entete = String::new();
                        if lecteur.read_line(&mut entete).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let entete = entete.trim_end().to_string();
                        if entete.is_empty() {
                            break;
                        }
                        if let Some((cle, valeur)) = entete.split_once(':')
                            && cle.trim().eq_ignore_ascii_case("Content-Length")
                        {
                            taille = valeur.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut corps = vec![0_u8; taille];
                    if taille > 0 && lecteur.read_exact(&mut corps).await.is_err() {
                        return;
                    }
                    match methode.as_str() {
                        "RECORD" => {
                            journal.lock().unwrap().push("RECORD".into());
                            attend_rtp.store(true, Ordering::SeqCst);
                        }
                        "SET_PARAMETER" => {
                            let corps = String::from_utf8_lossy(&corps).trim().to_string();
                            journal.lock().unwrap().push(format!("VOLUME {corps}"));
                        }
                        _ => {}
                    }
                    let supplement = if methode == "SETUP" {
                        format!(
                            "Session: 1\r\nTransport: RTP/AVP/UDP;unicast;mode=record;\
                             server_port={port_audio};control_port=0;timing_port=0\r\n"
                        )
                    } else {
                        String::new()
                    };
                    let reponse = format!(
                        "RTSP/1.0 200 OK\r\nCSeq: 1\r\n{supplement}Content-Length: 0\r\n\r\n"
                    );
                    if lecteur
                        .get_mut()
                        .write_all(reponse.as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    (port, journal)
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn sortie(port: u16) -> AirplayOutput {
    AirplayOutput::new(
        "Enceinte 8855".into(),
        APPAREIL.into(),
        "127.0.0.1".into(),
        port,
    )
}

/// Lance une piste et attend son premier paquet RTP ; rend le journal de
/// cette piste (depuis son RECORD).
async fn jouer_une_piste(sortie: &AirplayOutput, url: &str, journal: &Journal) -> Vec<String> {
    let debut = journal.lock().unwrap().len();
    tokio::time::timeout(
        Duration::from_secs(5),
        sortie.play_media(&PlayMedia {
            url,
            mime_type: "audio/wav",
            ..Default::default()
        }),
    )
    .await
    .expect("play_media ne doit pas rester suspendu")
    .expect("le faux récepteur accepte tout");
    let limite = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let vu = journal.lock().unwrap()[debut..].to_vec();
        if vu.iter().any(|e| e == "RTP") {
            return vu;
        }
        assert!(
            tokio::time::Instant::now() < limite,
            "aucun paquet RTP : {vu:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn volume(curseur: f64) -> String {
    format!("VOLUME volume: {:.1}", linear_to_airplay_db(curseur))
}

/// 🔴 Témoin du GO du 20/11 : chaque piste, enchaînée ou non, porte le volume
/// de la zone (graine de la découverte : 40 %, limite de sécurité 0,35)
/// entre RECORD et le premier paquet audio. Rouge avant : aucun
/// `SET_PARAMETER` sans consigne en attente.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_premier_paquet_part_au_volume_de_la_zone_a_chaque_piste_8855() {
    let db = base();
    let repo = ZoneRepo::with_backend(db.clone());
    let zone_id = repo
        .create("Salon 8855", Some("airplay"), Some(APPAREIL))
        .unwrap();
    repo.update_volume(zone_id, 40.0).unwrap();
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .set(&crate::audio::volume_limites::cle_securite(zone_id), "0.35")
        .unwrap();
    let url = stream_court().await;
    let (port, journal) = faux_recepteur().await;
    let sortie = sortie(port);
    sortie.ensemencer_depuis_la_base(&db).await;

    for _ in 0..2 {
        let piste = jouer_une_piste(&sortie, &url, &journal).await;
        assert_eq!(
            piste.iter().take(3).cloned().collect::<Vec<_>>(),
            vec!["RECORD".to_string(), volume(0.35), "RTP".to_string()],
            "le volume borné de la zone, après RECORD et avant l'audio : {piste:?}"
        );
    }
    sortie.stop().await.ok();
}

/// PURE verrouillé, aligné sur AirPlay 2 (décision du 11/11) : une lecture
/// neuve part à 0 % et le dit une fois ; la piste enchaînée garde le niveau
/// qu'un geste a donné depuis ; après un arrêt, la lecture suivante repart à
/// 0 %.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pure_verrouille_part_a_zero_et_la_piste_enchainee_garde_le_geste_8855() {
    let url = stream_court().await;
    let (port, journal) = faux_recepteur().await;
    let sortie = sortie(port);
    sortie.set_volume(0.60).await.unwrap();
    sortie.regler_selon_la_zone(true, false);

    let premiere = jouer_une_piste(&sortie, &url, &journal).await;
    assert_eq!(premiere[1], volume(0.0), "{premiere:?}");
    assert!(sortie.prendre_le_depart_muet());
    assert!(!sortie.prendre_le_depart_muet(), "dit une seule fois");

    sortie.set_volume(0.50).await.unwrap();
    let enchainee = jouer_une_piste(&sortie, &url, &journal).await;
    let geste = volume(0.50);
    assert_eq!(
        enchainee.iter().filter(|e| e.starts_with("VOLUME")).last(),
        Some(&geste)
    );
    assert_eq!(
        enchainee
            .iter()
            .position(|e| e == "RECORD")
            .map(|i| enchainee[i + 1].clone()),
        Some(geste.clone()),
        "{enchainee:?}"
    );
    assert!(!sortie.prendre_le_depart_muet());

    sortie.stop().await.unwrap();
    let apres_arret = jouer_une_piste(&sortie, &url, &journal).await;
    assert_eq!(apres_arret[1], volume(0.0), "{apres_arret:?}");
    sortie.stop().await.ok();
}

/// Volume fixe : #2395 inchangé, la sortie n'envoie aucun volume d'elle-même.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn en_volume_fixe_aucun_volume_ne_part_au_depart_2395() {
    let url = stream_court().await;
    let (port, journal) = faux_recepteur().await;
    let sortie = sortie(port);
    sortie.regler_selon_la_zone(false, true);
    let piste = jouer_une_piste(&sortie, &url, &journal).await;
    assert_eq!(piste, vec!["RECORD".to_string(), "RTP".to_string()]);
    sortie.stop().await.ok();
}

/// Attend que le flux en cours finisse de lui-même (fin de file sans `stop`).
async fn attendre_la_fin_du_flux(sortie: &AirplayOutput) {
    let limite = tokio::time::Instant::now() + Duration::from_secs(5);
    while sortie
        .fin_du_dernier_flux
        .lock()
        .unwrap()
        .is_none()
    {
        assert!(tokio::time::Instant::now() < limite, "le flux ne finit pas");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn premier_volume(piste: &[String]) -> Option<String> {
    piste
        .iter()
        .position(|e| e == "RECORD")
        .and_then(|i| piste.get(i + 1).cloned())
}

/// Relecture du 20/11 : une relance interne de l'orchestrateur (transfert,
/// seek rejoué, réaffectation de voies, relance DSP) n'est pas une lecture
/// neuve. En PURE verrouillé, la piste relancée garde le niveau en cours,
/// même si un `stop` précède la relance. Contre-épreuve dans le même banc :
/// le même `stop` sans marque repart à 0 %.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_relance_interne_en_pure_garde_le_niveau_en_cours_8855() {
    let url = stream_court().await;
    let (port, journal) = faux_recepteur().await;
    let sortie = sortie(port);
    sortie.regler_selon_la_zone(true, false);
    let premiere = jouer_une_piste(&sortie, &url, &journal).await;
    assert_eq!(premier_volume(&premiere), Some(volume(0.0)));
    sortie.set_volume(0.50).await.unwrap();

    sortie.marquer_relance_interne();
    sortie.stop().await.unwrap();
    let relance = jouer_une_piste(&sortie, &url, &journal).await;
    assert_eq!(premier_volume(&relance), Some(volume(0.50)), "{relance:?}");
    assert!(!sortie.prendre_le_depart_muet());

    // La marque est consommée : le `stop` suivant est celui de l'utilisateur.
    sortie.stop().await.unwrap();
    let apres_stop = jouer_une_piste(&sortie, &url, &journal).await;
    assert_eq!(premier_volume(&apres_stop), Some(volume(0.0)), "{apres_stop:?}");
    sortie.stop().await.ok();
}

/// Relecture du 20/11 : après une fin de file sans `stop`, une reprise
/// au-delà de `ARRET_LONG_LECTURE_NEUVE` est une lecture neuve (PURE : 0 %).
/// Contre-épreuve : la piste suivante tout de suite après la fin garde le
/// niveau.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apres_une_fin_de_file_longue_la_lecture_repart_a_zero_8855() {
    let url = stream_court().await;
    let (port, journal) = faux_recepteur().await;
    let sortie = sortie(port);
    sortie.regler_selon_la_zone(true, false);
    jouer_une_piste(&sortie, &url, &journal).await;
    sortie.set_volume(0.50).await.unwrap();

    attendre_la_fin_du_flux(&sortie).await;
    let tout_de_suite = jouer_une_piste(&sortie, &url, &journal).await;
    assert_eq!(premier_volume(&tout_de_suite), Some(volume(0.50)), "{tout_de_suite:?}");

    attendre_la_fin_du_flux(&sortie).await;
    *sortie.fin_du_dernier_flux.lock().unwrap() = Some(
        std::time::Instant::now()
            .checked_sub(ARRET_LONG_LECTURE_NEUVE + Duration::from_secs(1))
            .expect("horloge monotone assez avancée"),
    );
    let apres_long_arret = jouer_une_piste(&sortie, &url, &journal).await;
    assert_eq!(
        premier_volume(&apres_long_arret),
        Some(volume(0.0)),
        "{apres_long_arret:?}"
    );
    assert!(sortie.prendre_le_depart_muet());
    sortie.stop().await.ok();
}
