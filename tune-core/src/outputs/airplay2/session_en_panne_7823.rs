//! Une session AirPlay 2 dont le `setup` a échoué ne reçoit plus de pause
//! (#7823).
//!
//! ## Le défaut
//!
//! Récepteur muet (testeur, rc10 Linux, `device=ShairportSync`) : le daemon
//! répond à `play` par `setup failed: Operation timed out`. `play_media` avait
//! déjà rendu la main en publiant « lecture », l'erreur n'était rangée que
//! dans l'état de jumelage, et chaque pause partait quand même vers le
//! daemon. Celui-ci traite ses commandes une à une, chacune attend 10 s le
//! récepteur : d'où un « pause failed: Operation timed out » toutes les 10 s.
//! Le serveur abandonnait au bout de 5 s, la pause restait dans la file du
//! daemon, et l'appui suivant s'y empilait.
//!
//! ## Le correctif
//!
//! Une erreur qui dit que la session n'existe pas ([`casse_la_session`]) remet
//! la sortie à l'arrêt et pose sa cause ; un délai dépassé la pose aussi.
//! Tant qu'elle est posée, pause et reprise sont refusées tout de suite, avec
//! une phrase qui nomme le récepteur et la cause, sans rien écrire au daemon.
//! Le `play` suivant ou `stop` l'efface.

use super::Airplay2Output;

/// Les erreurs du daemon (`airplay-daemon`, commandes `connect` et `play`)
/// qui disent qu'aucune session ne joue. `pause failed` ou `resume failed`
/// n'en sont pas : la session peut encore exister.
pub(super) fn casse_la_session(message: &str) -> bool {
    [
        "setup failed",
        "play failed",
        "connect failed",
        "not connected",
        "decode error",
    ]
    .iter()
    .any(|prefixe| message.starts_with(prefixe))
}

impl Airplay2Output {
    /// La phrase d'un refus de transport sur une session en panne : le
    /// récepteur par son nom, la cause brute du daemon.
    pub(super) fn recepteur_muet(&self, cause: &str) -> String {
        format!(
            "AirPlay 2 receiver \"{}\" is not responding: {cause}. \
             Start playback again to reconnect it.",
            self.name
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Mutex;

    use super::super::{Airplay2Output, DaemonProcess};
    use crate::outputs::traits::{OutputTarget, PlayMedia, TransportState};

    fn recepteur() -> Airplay2Output {
        Airplay2Output::new(
            "Salon ShairportSync".into(),
            "127.0.0.1".into(),
            7000,
            "fixture-7823".into(),
            "00:11:22:33:44:55".into(),
        )
    }

    fn piste() -> PlayMedia<'static> {
        PlayMedia {
            url: "/tmp/fixture-7823.flac",
            mime_type: "audio/flac",
            title: Some("Fixture"),
            ..Default::default()
        }
    }

    /// Un faux daemon qui note chaque commande reçue et répond ce que
    /// `reponse(commande, rang)` lui dit, `rang` comptant les `play` déjà
    /// reçus. `None` : il ne répond rien, comme un daemon qui attend encore
    /// son récepteur.
    async fn faux_daemon<F>(sortie: &Airplay2Output, reponse: F) -> Arc<Mutex<Vec<String>>>
    where
        F: Fn(&str, usize) -> Option<serde_json::Value> + Send + 'static,
    {
        let (server_stdin, daemon_stdin) = tokio::io::duplex(4096);
        let (mut daemon_stdout, server_stdout) = tokio::io::duplex(4096);
        *sortie.process.lock().await = Some(DaemonProcess {
            child: None,
            stdin: Box::new(server_stdin),
        });
        // Un daemon de banc deja relie au recepteur (#7441).
        *sortie.pairing.lock().await = crate::outputs::airplay2::PairingPhase::Connected;
        sortie.start_event_reader(BufReader::new(server_stdout));

        let recues = Arc::new(Mutex::new(Vec::new()));
        let notees = recues.clone();
        tokio::spawn(async move {
            let mut lignes = BufReader::new(daemon_stdin).lines();
            let mut plays = 0;
            while let Ok(Some(ligne)) = lignes.next_line().await {
                let commande: serde_json::Value = serde_json::from_str(&ligne).unwrap();
                let nom = commande["cmd"].as_str().unwrap().to_string();
                // `status` est la lecture de position ajoutée par #7084 après
                // un `play` : elle ne pilote rien, on ne compte que les ordres.
                // #8402 — ni le volume de départ.
                if nom != "status" && nom != "volume" {
                    notees.lock().await.push(nom.clone());
                }
                let evenement = reponse(&nom, plays);
                if nom == "play" {
                    plays += 1;
                }
                if let Some(evenement) = evenement {
                    let ligne = format!("{}\n", serde_json::to_string(&evenement).unwrap());
                    daemon_stdout.write_all(ligne.as_bytes()).await.unwrap();
                }
            }
        });
        recues
    }

    fn setup_echoue() -> serde_json::Value {
        serde_json::json!({
            "event": "error",
            "message": "setup failed: Operation timed out",
        })
    }

    /// Laisse au lecteur d'événements le temps de lire la réponse au `play`.
    async fn etat_apres_le_play(sortie: &Airplay2Output) -> TransportState {
        for _ in 0..100 {
            let etat = sortie.get_status().await.unwrap().state;
            if etat == TransportState::Stopped {
                return etat;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        sortie.get_status().await.unwrap().state
    }

    #[tokio::test]
    async fn apres_un_setup_echoue_aucune_pause_ne_part_vers_le_daemon_7823() {
        let sortie = recepteur();
        let recues = faux_daemon(&sortie, |commande, _| match commande {
            "play" => Some(setup_echoue()),
            "pause" => Some(serde_json::json!({"event": "paused"})),
            _ => None,
        })
        .await;

        sortie.play_media(&piste()).await.expect_err(
            "#7441 : un setup échoué fait échouer le play, il ne se déguise plus en lecture",
        );
        assert_eq!(
            etat_apres_le_play(&sortie).await,
            TransportState::Stopped,
            "après « setup failed », la zone AirPlay 2 se dit encore en lecture"
        );

        let erreur = sortie
            .pause()
            .await
            .expect_err("une pause a été acceptée sur une session dont le setup a échoué");
        assert!(
            erreur.contains("Salon ShairportSync"),
            "l'erreur doit nommer le récepteur : {erreur}"
        );
        assert!(
            erreur.contains("setup failed: Operation timed out"),
            "l'erreur doit dire la cause : {erreur}"
        );
        let erreur = sortie.resume().await.unwrap_err();
        assert!(erreur.contains("setup failed"), "{erreur}");
        assert_eq!(
            *recues.lock().await,
            vec!["play"],
            "pause ou reprise écrites au daemon après un setup échoué"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn apres_un_delai_depasse_les_pauses_ne_s_empilent_plus_7823() {
        let sortie = recepteur();
        let recues = faux_daemon(&sortie, |commande, _| match commande {
            "play" => Some(serde_json::json!({"event": "playing"})),
            // Le daemon attend son récepteur : aucune réponse.
            _ => None,
        })
        .await;

        sortie.play_media(&piste()).await.unwrap();
        let premiere = sortie.pause().await.unwrap_err();
        assert!(premiere.contains("Salon ShairportSync"), "{premiere}");
        assert!(premiere.contains("did not confirm pause"), "{premiere}");

        let seconde = sortie.pause().await.unwrap_err();
        assert!(seconde.contains("did not confirm pause"), "{seconde}");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pauses = recues
            .lock()
            .await
            .iter()
            .filter(|c| c.as_str() == "pause")
            .count();
        assert_eq!(
            pauses, 1,
            "une seconde pause s'est empilée dans la file d'un daemon qui ne répond plus"
        );
    }

    #[tokio::test]
    async fn un_nouveau_play_reussi_rend_la_pause_7823() {
        let sortie = recepteur();
        let recues = faux_daemon(&sortie, |commande, plays| match (commande, plays) {
            ("play", 0) => Some(setup_echoue()),
            ("play", _) => Some(serde_json::json!({"event": "playing"})),
            ("pause", _) => Some(serde_json::json!({"event": "paused"})),
            _ => None,
        })
        .await;

        sortie.play_media(&piste()).await.expect_err(
            "#7441 : un setup échoué fait échouer le play, il ne se déguise plus en lecture",
        );
        assert_eq!(etat_apres_le_play(&sortie).await, TransportState::Stopped);
        assert!(sortie.pause().await.is_err());

        sortie.play_media(&piste()).await.unwrap();
        sortie
            .pause()
            .await
            .expect("le play suivant doit rouvrir la session");
        assert_eq!(
            sortie.get_status().await.unwrap().state,
            TransportState::Paused
        );
        // #8347 — la connexion dont le setup a échoué est lâchée
        // (`disconnect`) et rouverte (`connect`) avant le nouveau `play`.
        assert_eq!(
            *recues.lock().await,
            vec!["play", "disconnect", "connect", "play", "pause"]
        );
    }

    #[test]
    fn seules_les_erreurs_sans_session_cassent_la_session_7823() {
        assert!(super::casse_la_session("setup failed: Operation timed out"));
        assert!(super::casse_la_session("not connected"));
        assert!(super::casse_la_session("play failed: broken pipe"));
        assert!(!super::casse_la_session(
            "pause failed: Operation timed out"
        ));
        assert!(!super::casse_la_session("resume failed: fixture"));
        assert!(!super::casse_la_session("unknown command: status"));
    }
}
