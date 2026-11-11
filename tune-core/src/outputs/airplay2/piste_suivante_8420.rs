//! #8420 — la piste suivante d'une playlist AirPlay 2.
//!
//! Le démon (`airplay2-rs`, `crates/airplay-daemon`) refait `conn.setup()` à
//! chaque `play`, et la session RTSP n'accepte la phase 1 du setup que depuis
//! l'état `Paired` (`airplay-rtsp/src/session.rs`). Après un premier titre
//! elle est en `Playing`, et `Connection::stop()` ne l'y ramène pas : le
//! deuxième `play` était refusé (« Cannot process phase 1 from state
//! Playing ») et la zone s'arrêtait à chaque fin de titre (fil 2590).
//!
//! `play_media` note donc qu'un `play` est parti sur la connexion
//! (`session_consommee`) ; `ensure_connected` referme alors cette connexion
//! (`disconnect`) et en rouvre une neuve (`connect`, par pair-verify) avant
//! le `play` suivant, par le chemin déjà suivi après un setup manqué (#8347).

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Mutex;

    use super::super::{Airplay2Output, DaemonProcess};
    use crate::outputs::traits::{OutputTarget, PlayMedia};

    /// Le message exact du journal du fil 2590 (rc11, Xiaomi Sound 2 Max).
    const PHASE_1_EN_LECTURE: &str =
        "setup failed: RTSP error: Setup failed: Cannot process phase 1 from state Playing";

    fn xiaomi() -> Airplay2Output {
        Airplay2Output::new(
            "Xiaomi Sound 2 Max".into(),
            "127.0.0.1".into(),
            7000,
            "fixture-8420".into(),
            "00:11:22:33:44:20".into(),
        )
    }

    fn piste(titre: &'static str) -> PlayMedia<'static> {
        PlayMedia {
            url: "/tmp/fixture-8420.flac",
            mime_type: "audio/flac",
            title: Some(titre),
            ..Default::default()
        }
    }

    /// État de la session RTSP que tient le faux démon, comme dans
    /// `airplay-rtsp` : pas de connexion, `Paired` après `connect`,
    /// `Playing` après le setup d'un `play`.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Session {
        Aucune,
        Paired,
        Playing,
    }

    /// Un faux démon fidèle au vrai sur ce point : `play` refait le setup,
    /// refusé hors de `Paired` ; `stop` ne ramène pas la session en
    /// `Paired` ; seul `disconnect` + `connect` en rend une neuve. Rend les
    /// ordres reçus (`status` et `volume` exceptés).
    async fn demon_d_une_seule_lecture_par_session(
        sortie: &Airplay2Output,
    ) -> Arc<Mutex<Vec<String>>> {
        let (server_stdin, daemon_stdin) = tokio::io::duplex(4096);
        let (mut daemon_stdout, server_stdout) = tokio::io::duplex(4096);
        *sortie.process.lock().await = Some(DaemonProcess {
            child: None,
            stdin: Box::new(server_stdin),
        });
        sortie.start_event_reader(BufReader::new(server_stdout));

        let recues = Arc::new(Mutex::new(Vec::new()));
        let notees = recues.clone();
        tokio::spawn(async move {
            let mut session = Session::Aucune;
            let mut lignes = BufReader::new(daemon_stdin).lines();
            while let Ok(Some(ligne)) = lignes.next_line().await {
                let commande: serde_json::Value = serde_json::from_str(&ligne).unwrap();
                let nom = commande["cmd"].as_str().unwrap().to_string();
                if nom != "status" && nom != "volume" {
                    notees.lock().await.push(nom.clone());
                }
                let evenement = match (nom.as_str(), session) {
                    ("connect", _) => {
                        session = Session::Paired;
                        Some(serde_json::json!({"event": "connected", "auth": "pair_verify"}))
                    }
                    ("play", Session::Aucune) => {
                        Some(serde_json::json!({"event": "error", "message": "not connected"}))
                    }
                    ("play", Session::Paired) => {
                        session = Session::Playing;
                        Some(serde_json::json!({"event": "playing"}))
                    }
                    ("play", Session::Playing) => {
                        Some(serde_json::json!({"event": "error", "message": PHASE_1_EN_LECTURE}))
                    }
                    ("pause", Session::Playing) => Some(serde_json::json!({"event": "paused"})),
                    ("resume", Session::Playing) => Some(serde_json::json!({"event": "playing"})),
                    ("stop", Session::Aucune) => None,
                    ("stop", _) => Some(serde_json::json!({"event": "stopped"})),
                    ("disconnect", Session::Aucune) => None,
                    ("disconnect", _) => {
                        session = Session::Aucune;
                        Some(serde_json::json!({"event": "disconnected"}))
                    }
                    _ => None,
                };
                if let Some(evenement) = evenement {
                    let ligne = format!("{}\n", serde_json::to_string(&evenement).unwrap());
                    daemon_stdout.write_all(ligne.as_bytes()).await.unwrap();
                }
            }
        });
        recues
    }

    /// 🔴 #8420 — deux titres d'une playlist à la suite : le second part
    /// d'une connexion neuve au lieu de refaire `setup` sur la session qui
    /// joue le premier.
    #[tokio::test]
    async fn le_titre_suivant_d_une_playlist_part_d_une_session_neuve_8420() {
        let sortie = xiaomi();
        let recues = demon_d_une_seule_lecture_par_session(&sortie).await;

        sortie
            .play_media(&piste("Premier titre"))
            .await
            .expect("le premier titre doit jouer");
        sortie
            .play_media(&piste("Deuxième titre"))
            .await
            .expect("le titre suivant ne doit pas refaire setup sur une session en Playing");
        sortie
            .play_media(&piste("Troisième titre"))
            .await
            .expect("ni le suivant");

        assert_eq!(
            *recues.lock().await,
            vec![
                "connect",
                "play",
                "disconnect",
                "connect",
                "play",
                "disconnect",
                "connect",
                "play"
            ]
        );
        sortie.stop().await.unwrap();
    }

    /// #8420 — pause et reprise restent sur la session qui joue : seule une
    /// nouvelle piste la rouvre.
    #[tokio::test]
    async fn pause_et_reprise_ne_rouvrent_pas_la_session_8420() {
        let sortie = xiaomi();
        let recues = demon_d_une_seule_lecture_par_session(&sortie).await;

        sortie.play_media(&piste("Premier titre")).await.unwrap();
        sortie.pause().await.unwrap();
        sortie.resume().await.unwrap();
        assert_eq!(
            *recues.lock().await,
            vec!["connect", "play", "pause", "resume"]
        );
        sortie.stop().await.unwrap();
    }

    /// #8420 — `stop` tue le démon et sa session : la lecture suivante
    /// n'a rien à refermer.
    #[tokio::test]
    async fn stop_oublie_la_session_consommee_8420() {
        let sortie = xiaomi();
        sortie.session_consommee.store(true, Ordering::SeqCst);
        sortie.stop().await.unwrap();
        assert!(!sortie.session_consommee.load(Ordering::SeqCst));
    }
}
