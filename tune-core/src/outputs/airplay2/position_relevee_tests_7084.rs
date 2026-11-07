//! #7084 — Devialet Mania en AirPlay 2 : la position oscillait entre 1 et 2 s
//! sans fin.
//!
//! Le démon ne donne sa position qu'en réponse à `{"cmd":"status"}`, que Tune
//! n'envoyait jamais : la position publiée restait à 0 et l'écran se recalait
//! en boucle sur ce 0 (voir `poller/demarrage_fige_5522.rs`). Ces témoins
//! jouent contre un faux démon qui répond comme le vrai
//! (`airplay-daemon/src/main.rs`, bras `"status"`).

use super::composant_manquant_7084::{CODE_AIRPLAY2_COMPONENT_MISSING, manque_pour};
use super::*;

fn sortie(device_id: &str) -> Airplay2Output {
    Airplay2Output::new(
        "Mania".into(),
        "127.0.0.1".into(),
        7000,
        device_id.into(),
        "00:11:22:33:44:55".into(),
    )
}

/// Un faux démon dont le récepteur avance d'une seconde et demie à chaque
/// `status`. Note le nom de chaque commande reçue.
async fn demon_dont_le_recepteur_avance(output: &Airplay2Output) -> Arc<Mutex<Vec<String>>> {
    let (server_stdin, daemon_stdin) = tokio::io::duplex(4096);
    let (mut daemon_stdout, server_stdout) = tokio::io::duplex(4096);
    *output.process.lock().await = Some(DaemonProcess {
        child: None,
        stdin: Box::new(server_stdin),
    });
    // Un daemon de banc deja relie au recepteur (#7441).
    *output.pairing.lock().await = crate::outputs::airplay2::PairingPhase::Connected;
    output.start_event_reader(BufReader::new(server_stdout));

    let commandes = Arc::new(Mutex::new(Vec::new()));
    let notees = commandes.clone();
    tokio::spawn(async move {
        let mut position_s = 0.0_f64;
        let mut lignes = BufReader::new(daemon_stdin).lines();
        while let Ok(Some(ligne)) = lignes.next_line().await {
            let commande: serde_json::Value = serde_json::from_str(&ligne).unwrap();
            let nom = commande["cmd"].as_str().unwrap().to_string();
            notees.lock().await.push(nom.clone());
            let evenement = match nom.as_str() {
                "play" => {
                    position_s = 0.0;
                    serde_json::json!({"event": "playing"})
                }
                "status" => {
                    position_s += 1.5;
                    serde_json::json!({
                        "event": "status",
                        "position_s": position_s,
                        "state": "Playing",
                        "device": "Mania",
                    })
                }
                autre => serde_json::json!({
                    "event": "error",
                    "message": format!("unexpected command: {autre}"),
                }),
            };
            let ligne = format!("{}\n", serde_json::to_string(&evenement).unwrap());
            daemon_stdout.write_all(ligne.as_bytes()).await.unwrap();
        }
    });
    commandes
}

/// Le témoin de la boucle : tant que le récepteur avance, la position publiée
/// avance avec lui, et la piste n'est jamais relancée au début.
#[tokio::test]
async fn la_position_suit_le_recepteur_sans_relancer_la_piste_7084() {
    let output = sortie("airplay-mania-7084-position");
    let commandes = demon_dont_le_recepteur_avance(&output).await;

    output
        .play_media(&PlayMedia {
            url: "/tmp/mania-7084.flac",
            mime_type: "audio/flac",
            title: Some("FLAC"),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut positions = Vec::new();
    for _ in 0..6 {
        let statut = output.get_status().await.unwrap();
        assert_eq!(statut.state, TransportState::Playing);
        positions.push(statut.position_ms);
        // Le temps que la réponse du démon arrive au lecteur d'événements,
        // comme entre deux tours du sondeur.
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let derniere = *positions.last().unwrap();
    assert!(
        derniere > 0,
        "la position AirPlay 2 est restée à 0 alors que le récepteur avance : \
         l'écran se recale en boucle au début de la piste (#7084) ; positions = {positions:?}"
    );
    assert!(
        positions.windows(2).all(|p| p[1] >= p[0]) && derniere > positions[1],
        "la position doit avancer sans retomber : {positions:?}"
    );
    let commandes = commandes.lock().await;
    assert_eq!(
        commandes.iter().filter(|c| *c == "play").count(),
        1,
        "la piste a été relancée : {commandes:?}"
    );
}

/// En pause, rien n'est demandé au démon : la position reste celle de la
/// pause.
#[tokio::test]
async fn en_pause_la_position_n_est_pas_demandee_7084() {
    let output = sortie("airplay-mania-7084-pause");
    let commandes = demon_dont_le_recepteur_avance(&output).await;
    output.playing.store(true, Ordering::SeqCst);
    output.paused.store(true, Ordering::SeqCst);

    let statut = output.get_status().await.unwrap();
    assert_eq!(statut.state, TransportState::Paused);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(commandes.lock().await.is_empty());
}

/// Le démon disparu après le démarrage (antivirus) : lancer la lecture rend
/// le message « composant AirPlay 2 manquant », et la zone est signalée.
#[tokio::test]
async fn un_demon_introuvable_dit_le_composant_manquant_7084() {
    // Un dossier à soi, supprimé à la fin du test (#3030), dans lequel le
    // démon n'existe pas.
    let dossier = crate::test_scratch::scratch_dir("tune-7084-absent");
    let absent = dossier.path().join(daemon_exe_name());
    // La substitution vaut pour tout le processus de test de tune-core : aucun
    // autre test de la caisse ne lance le démon aujourd'hui ; un test qui le
    // ferait devra déclarer son propre substitut avant de lancer la lecture.
    crate::services_factices::substituer_programme(DAEMON_BINARY, absent.to_str().unwrap());
    let device_id = "airplay-mania-7084-absent";
    let output = sortie(device_id);

    let erreur = output
        .play_media(&PlayMedia {
            url: "/tmp/mania-7084.flac",
            mime_type: "audio/flac",
            ..Default::default()
        })
        .await
        .unwrap_err();

    assert!(
        erreur.starts_with(CODE_AIRPLAY2_COMPONENT_MISSING),
        "un airplay-daemon introuvable doit dire le composant manquant (#7084), obtenu : {erreur}"
    );
    assert!(manque_pour(device_id), "la zone doit être signalée");
}
