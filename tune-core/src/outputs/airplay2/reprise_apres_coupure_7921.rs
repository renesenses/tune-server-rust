//! Une enceinte AirPlay 2 qui a fermé la connexion pendant la pause se
//! reprend au lieu d'échouer en « Broken pipe » (#7921).
//!
//! ## Le défaut
//!
//! Fil 2590 (rc10, Docker) : lecture, pause, volume réglé pendant la pause,
//! reprise. Le daemon répond deux fois
//! `resume failed: Connection error: Broken pipe (os error 32)`.
//!
//! `airplay-daemon` (dépôt `airplay2-rs`, révision épinglée par la CI)
//! n'envoie jamais le `POST /feedback` qu'un récepteur AirPlay 2 attend toutes
//! les deux secondes environ. Pendant la lecture, le flux RTP occupe la
//! session ; en pause, plus rien ne passe et le récepteur referme la
//! connexion RTSP. Le daemon garde pourtant sa `Connection` : `resume` écrit
//! `RECORD` sur une socket morte, d'où EPIPE. Les volumes de la pause
//! partent sur la même socket et le daemon ignore leurs échecs (`.ok()`).
//!
//! Côté Tune, rien ne rouvrait la session. `ensure_connected` se contentait
//! d'un processus daemon vivant, et l'orchestrateur, sans signal, rendait
//! l'erreur à l'auditeur.
//!
//! ## Le correctif
//!
//! Une erreur du daemon qui dit que la socket vers le récepteur est tombée
//! ([`connexion_coupee`]) marque la connexion comme perdue. La sortie le dit
//! alors par `device_released_on_pause` (même contrat que #4177 et #6062) :
//! l'orchestrateur rétablit la piste à la position conservée, et
//! `play_media` rouvre la session (`disconnect`, puis `connect` confirmé par
//! le daemon, puis le volume mémorisé) avant d'envoyer `play`. L'erreur ne
//! remonte que si la reconnexion échoue, et elle nomme le récepteur et la
//! cause.
//!
//! Le remède de fond, garder la session vivante par `/feedback`, appartient
//! au daemon (`airplay2-rs`), pas à ce dépôt.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::oneshot;
use tracing::{info, warn};

use super::{Airplay2Output, PendingTransport, TransportConfirmation};

/// Le `connect` du daemon refait le jumelage transitoire puis attend la
/// réponse du récepteur : plus long qu'une commande de transport.
const RECONNEXION_TIMEOUT: Duration = Duration::from_secs(20);

/// Les erreurs du daemon qui disent que la socket RTSP vers le récepteur est
/// tombée. `airplay_core::Error::Connection` enveloppe toute erreur d'E/S de
/// cette socket sous le préfixe « Connection error: » ; les deux suivants
/// couvrent un message qui la nommerait sans ce préfixe.
pub(super) fn connexion_coupee(message: &str) -> bool {
    ["Connection error:", "Broken pipe", "Connection reset"]
        .iter()
        .any(|marque| message.contains(marque))
}

impl Airplay2Output {
    /// Le refus d'une reprise sur place quand la connexion est déjà tombée.
    pub(super) fn connexion_fermee_par_le_recepteur(&self) -> String {
        format!(
            "AirPlay 2 receiver \"{}\" closed the connection during the pause; \
             the session must be reopened",
            self.name
        )
    }

    /// Rouvre la session d'un daemon vivant dont le récepteur a fermé la
    /// connexion : `disconnect` (le daemon lâche la socket morte), `connect`
    /// attendu jusqu'à `connected`, puis le volume mémorisé, réglé peut-être
    /// pendant la pause.
    pub(super) async fn rouvrir_la_session(&self) -> Result<(), String> {
        let _command_guard = self.transport_command.lock().await;
        warn!(
            device = %self.name,
            "airplay2: the receiver closed the connection, reopening the session"
        );
        self.send(&serde_json::json!({"cmd": "disconnect"})).await?;

        let (response, receiver) = oneshot::channel();
        *self.pending_transport.lock().await = Some(PendingTransport {
            expected: TransportConfirmation::Connected,
            response,
        });
        if let Err(error) = self.send(&self.commande_connect()).await {
            self.pending_transport.lock().await.take();
            return Err(error);
        }
        let verdict = match tokio::time::timeout(RECONNEXION_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("daemon response channel closed after connect".to_string()),
            Err(_) => {
                self.pending_transport.lock().await.take();
                Err(format!(
                    "daemon did not confirm connect within {} s",
                    RECONNEXION_TIMEOUT.as_secs()
                ))
            }
        };
        if let Err(cause) = verdict {
            return Err(format!(
                "AirPlay 2 receiver \"{}\" closed the connection and could not be \
                 reconnected: {cause}",
                self.name
            ));
        }

        let niveau = if self.muted.load(Ordering::SeqCst) {
            0.0
        } else {
            *self.volume.lock().await
        };
        self.send(&serde_json::json!({"cmd": "volume", "level": niveau}))
            .await
            .ok();
        info!(device = %self.name, "airplay2: session reopened");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{Mutex, Notify};

    use super::super::{Airplay2Output, DaemonProcess};
    use crate::outputs::traits::{OutputTarget, PlayMedia, TransportState};

    /// Un faux récepteur AirPlay local : il note chaque connexion et chaque
    /// requête, répond `200` à chacune, et referme la PREMIÈRE connexion
    /// juste après avoir répondu au `FLUSH` de la pause.
    struct Recepteur {
        adresse: std::net::SocketAddr,
        journal: Arc<Mutex<Vec<String>>>,
        ferme: Arc<Notify>,
    }

    async fn recepteur_qui_ferme_pendant_la_pause() -> Recepteur {
        recepteur(false).await
    }

    /// `disparait` : l'enceinte quitte le réseau après la première
    /// connexion ; toute reconnexion est refusée.
    async fn recepteur(disparait: bool) -> Recepteur {
        let ecoute = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecoute.local_addr().unwrap();
        let journal = Arc::new(Mutex::new(Vec::new()));
        let ferme = Arc::new(Notify::new());
        let (j, f) = (journal.clone(), ferme.clone());
        tokio::spawn(async move {
            let mut numero = 0;
            while let Ok((socket, _)) = ecoute.accept().await {
                numero += 1;
                j.lock().await.push(format!("connexion {numero}"));
                let (j, f) = (j.clone(), f.clone());
                tokio::spawn(async move {
                    let mut socket = BufReader::new(socket);
                    let mut ligne = String::new();
                    loop {
                        ligne.clear();
                        match socket.read_line(&mut ligne).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let requete = ligne.trim().to_string();
                        j.lock().await.push(format!("{numero}: {requete}"));
                        if socket.get_mut().write_all(b"200\n").await.is_err() {
                            break;
                        }
                        if numero == 1 && requete == "FLUSH" {
                            // La session inactive est refermée par l'enceinte.
                            drop(socket);
                            f.notify_one();
                            return;
                        }
                    }
                });
                if disparait {
                    break;
                }
            }
        });
        Recepteur {
            adresse,
            journal,
            ferme,
        }
    }

    /// Une requête RTSP réduite à une ligne : écrire, puis lire la réponse.
    /// Une socket refermée par le récepteur donne la vraie erreur d'E/S.
    async fn rtsp(conn: &mut BufReader<TcpStream>, requete: &str) -> std::io::Result<()> {
        conn.get_mut()
            .write_all(format!("{requete}\n").as_bytes())
            .await?;
        conn.get_mut().flush().await?;
        let mut reponse = String::new();
        if conn.read_line(&mut reponse).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "early eof",
            ));
        }
        Ok(())
    }

    /// Un faux `airplay-daemon`, fidèle au vrai (`airplay2-rs`, révision
    /// épinglée) : une commande à la fois, une `Connection` gardée tant
    /// qu'on ne la lâche pas, aucun `/feedback`, les échecs du volume
    /// ignorés, et les erreurs d'E/S préfixées « Connection error: ». Il part
    /// connecté, comme après le `connect` de `ensure_connected`.
    async fn daemon_relie_au(output: &Airplay2Output, recepteur: std::net::SocketAddr) {
        let (server_stdin, daemon_stdin) = tokio::io::duplex(4096);
        let (mut daemon_stdout, server_stdout) = tokio::io::duplex(4096);
        *output.process.lock().await = Some(DaemonProcess {
            child: None,
            stdin: Box::new(server_stdin),
        });
        // Un daemon de banc deja relie au recepteur (#7441).
        *output.pairing.lock().await = crate::outputs::airplay2::PairingPhase::Connected;
        output.start_event_reader(BufReader::new(server_stdout));

        let mut conn = Some(BufReader::new(TcpStream::connect(recepteur).await.unwrap()));
        tokio::spawn(async move {
            let mut lignes = BufReader::new(daemon_stdin).lines();
            while let Ok(Some(ligne)) = lignes.next_line().await {
                let commande: serde_json::Value = serde_json::from_str(&ligne).unwrap();
                let nom = commande["cmd"].as_str().unwrap().to_string();
                let erreur = |m: String| serde_json::json!({"event": "error", "message": m});
                let evenement = match nom.as_str() {
                    "connect" => match TcpStream::connect(recepteur).await {
                        Ok(s) => {
                            conn = Some(BufReader::new(s));
                            Some(serde_json::json!({"event": "connected"}))
                        }
                        Err(e) => Some(erreur(format!("connect failed: Connection error: {e}"))),
                    },
                    "disconnect" => conn
                        .take()
                        .map(|_| serde_json::json!({"event": "disconnected"})),
                    "play" | "pause" | "resume" => match conn.as_mut() {
                        None => Some(erreur("not connected".into())),
                        Some(c) => {
                            let (requete, ok, ko) = match nom.as_str() {
                                "play" => ("SETUP RECORD", "playing", "setup failed"),
                                "pause" => ("FLUSH", "paused", "pause failed"),
                                _ => ("RECORD", "playing", "resume failed"),
                            };
                            Some(match rtsp(c, requete).await {
                                Ok(()) => serde_json::json!({"event": ok}),
                                Err(e) => erreur(format!("{ko}: Connection error: {e}")),
                            })
                        }
                    },
                    "volume" => match conn.as_mut() {
                        Some(c) => {
                            let niveau = commande["level"].as_f64().unwrap_or(0.5);
                            rtsp(c, &format!("SET_PARAMETER volume={niveau}"))
                                .await
                                .ok();
                            Some(serde_json::json!({"event": "volume_set", "volume": niveau}))
                        }
                        None => None,
                    },
                    "status" => Some(serde_json::json!({"event": "status", "position_s": 0.0})),
                    "stop" => Some(serde_json::json!({"event": "stopped"})),
                    autre => Some(erreur(format!("unknown command: {autre}"))),
                };
                if let Some(evenement) = evenement {
                    let ligne = format!("{}\n", serde_json::to_string(&evenement).unwrap());
                    daemon_stdout.write_all(ligne.as_bytes()).await.unwrap();
                }
            }
        });
    }

    fn enceinte(recepteur: std::net::SocketAddr) -> Airplay2Output {
        Airplay2Output::new(
            "Enceinte du salon".into(),
            recepteur.ip().to_string(),
            recepteur.port(),
            "fixture-7921".into(),
            "00:11:22:33:44:55".into(),
        )
    }

    fn piste() -> PlayMedia<'static> {
        PlayMedia {
            url: "/tmp/fixture-7921.flac",
            mime_type: "audio/flac",
            title: Some("Fixture"),
            ..Default::default()
        }
    }

    async fn attendre_la_lecture(output: &Airplay2Output) -> TransportState {
        for _ in 0..200 {
            let etat = output.get_status().await.unwrap().state;
            if etat == TransportState::Playing {
                return etat;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        output.get_status().await.unwrap().state
    }

    /// La séquence du fil 2590 : lecture, pause, l'enceinte ferme la
    /// connexion, volume réglé pendant la pause, reprise.
    ///
    /// La reprise suit la conduite de l'orchestrateur (`reprendre`,
    /// transport.rs) : un refus de `checked_resume` est rendu à l'auditeur,
    /// SAUF si la sortie dit avoir perdu sa session pendant la pause
    /// (`device_released_on_pause`) ; la piste est alors relancée par
    /// `play_media` (`retablir_a_la_position`, #6062).
    #[tokio::test]
    async fn la_reprise_apres_une_connexion_fermee_pendant_la_pause_reprend_7921() {
        let recepteur = recepteur_qui_ferme_pendant_la_pause().await;
        let output = enceinte(recepteur.adresse);
        daemon_relie_au(&output, recepteur.adresse).await;

        output.play_media(&piste()).await.unwrap();
        assert_eq!(attendre_la_lecture(&output).await, TransportState::Playing);
        output.pause().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), recepteur.ferme.notified())
            .await
            .expect("le faux récepteur doit fermer la connexion à la pause");
        for niveau in [0.48, 0.3, 0.1] {
            output.set_volume(niveau).await.unwrap();
        }

        let reprise = match output.checked_resume().await {
            Ok(()) => Ok(()),
            Err(_refus) if output.device_released_on_pause() => {
                output.play_media(&piste()).await.map_err(|e| e.to_string())
            }
            Err(refus) => Err(refus.to_string()),
        };

        assert_eq!(
            reprise,
            Ok(()),
            "la reprise rend l'erreur du daemon au lieu de rouvrir la session"
        );
        assert_eq!(
            attendre_la_lecture(&output).await,
            TransportState::Playing,
            "la session rouverte doit rejouer la piste"
        );
        // `play_media` n'attend pas le verdict du daemon : on attend que le
        // récepteur ait reçu la lecture sur la session rouverte.
        for _ in 0..200 {
            if recepteur
                .journal
                .lock()
                .await
                .iter()
                .any(|l| l == "2: SETUP RECORD")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let journal = recepteur.journal.lock().await.clone();
        let rouverte = journal
            .iter()
            .position(|l| l == "connexion 2")
            .unwrap_or_else(|| panic!("aucune nouvelle connexion au récepteur : {journal:?}"));
        let suite: Vec<&str> = journal[rouverte + 1..]
            .iter()
            .map(String::as_str)
            .filter(|l| l.starts_with("2: "))
            .collect();
        assert_eq!(
            suite,
            ["2: SET_PARAMETER volume=0.1", "2: SETUP RECORD"],
            "le volume réglé pendant la pause part avant la lecture : {journal:?}"
        );
    }

    /// Le refus de la reprise sur place n'écrit plus rien sur la socket
    /// morte : une seconde reprise ne rend pas un second « Broken pipe ».
    #[tokio::test]
    async fn une_seconde_reprise_n_ecrit_pas_sur_la_socket_morte_7921() {
        let recepteur = recepteur_qui_ferme_pendant_la_pause().await;
        let output = enceinte(recepteur.adresse);
        daemon_relie_au(&output, recepteur.adresse).await;

        output.play_media(&piste()).await.unwrap();
        assert_eq!(attendre_la_lecture(&output).await, TransportState::Playing);
        output.pause().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), recepteur.ferme.notified())
            .await
            .unwrap();

        let premiere = output.resume().await.unwrap_err();
        assert!(premiere.contains("Connection error"), "{premiere}");
        assert!(output.device_released_on_pause());
        let seconde = output.resume().await.unwrap_err();
        assert!(seconde.contains("closed the connection"), "{seconde}");
        assert!(seconde.contains("Enceinte du salon"), "{seconde}");
    }

    /// Une reconnexion refusée remonte, avec le récepteur et la cause, et la
    /// connexion reste à rouvrir pour l'essai suivant.
    #[tokio::test]
    async fn une_reconnexion_refusee_nomme_le_recepteur_et_la_cause_7921() {
        let recepteur = recepteur(true).await;
        let output = enceinte(recepteur.adresse);
        daemon_relie_au(&output, recepteur.adresse).await;
        output.play_media(&piste()).await.unwrap();
        assert_eq!(attendre_la_lecture(&output).await, TransportState::Playing);
        output.pause().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), recepteur.ferme.notified())
            .await
            .unwrap();
        output.resume().await.unwrap_err();
        assert!(output.device_released_on_pause());

        let erreur = output.play_media(&piste()).await.unwrap_err();
        assert!(erreur.contains("Enceinte du salon"), "{erreur}");
        assert!(erreur.contains("could not be reconnected"), "{erreur}");
        assert!(erreur.contains("connect failed"), "{erreur}");
        assert!(output.device_released_on_pause());
    }

    #[test]
    fn seules_les_coupures_de_la_socket_comptent_7921() {
        assert!(super::connexion_coupee(
            "resume failed: Connection error: Broken pipe (os error 32)"
        ));
        assert!(super::connexion_coupee(
            "setup failed: Connection error: Connection reset by peer (os error 104)"
        ));
        assert!(!super::connexion_coupee("resume failed: fixture"));
        assert!(!super::connexion_coupee(
            "pause failed: Operation timed out"
        ));
        assert!(!super::connexion_coupee(
            "pair-pin-start: HTTP 403 Forbidden"
        ));
    }
}
