//! La reprise après une pause rouvre la session AirPlay 2 que le récepteur a
//! fermée, et le volume réglé pendant la pause attend la reprise (#7921).
//!
//! ## Le défaut
//!
//! Enceinte AirPlay 2 jumelée (rc10, Docker) : pendant une pause de moins
//! d'une minute, la connexion du daemon vers l'enceinte se ferme. Le daemon
//! garde pourtant sa connexion morte et répond à `resume` par « resume
//! failed: Connection error: Broken pipe (os error 32) ». Cette erreur
//! n'était reconnue nulle part : `device_released_on_pause()` restait à
//! `false`, l'orchestrateur ne rétablissait pas la piste (#6062) et
//! l'utilisateur voyait l'erreur, deux fois de suite. Un `play` ne rouvrait
//! rien non plus : `ensure_connected` se contentait d'un daemon vivant.
//!
//! En même temps, chaque mouvement du curseur de volume partait au daemon :
//! `set_volume` rendait la main sitôt la ligne écrite, sans attendre
//! `volume_set`, si bien que la file de volume de l'orchestrateur (#5662) ne
//! retenait rien et qu'une trentaine de commandes s'empilaient devant la
//! reprise.
//!
//! ## Le correctif
//!
//! - Une erreur qui dit la connexion fermée ([`connexion_perdue`]) pose
//!   `session_perdue` et fait tomber la phase de connexion. La sortie répond
//!   alors `true` à `device_released_on_pause()` : l'orchestrateur rétablit la
//!   piste à la position par un `play`, qui renvoie d'abord `connect` au
//!   daemon. L'erreur n'est montrée que si cette reconnexion échoue.
//! - Le volume demandé quand rien ne joue (pause, session perdue) est gardé et
//!   envoyé une seule fois, après le `play` ou le `resume` confirmé. En
//!   lecture, `set_volume` attend `volume_set` (au plus
//!   `VOLUME_CONFIRMATION_TIMEOUT`).

use std::sync::atomic::Ordering;

use tokio::sync::oneshot;
use tracing::{debug, warn};

use super::{
    Airplay2Output, PairingPhase, PendingTransport, TransportConfirmation,
    VOLUME_CONFIRMATION_TIMEOUT,
};

/// Les erreurs du daemon qui disent que la connexion au récepteur est fermée
/// ou absente : la session est à rouvrir, pas à reprendre sur place.
pub(super) fn connexion_perdue(message: &str) -> bool {
    message == "not connected"
        || [
            "Broken pipe",
            "os error 32",
            "Connection reset",
            "os error 104",
            "Connection error",
            "connection closed",
            "Connection aborted",
        ]
        .iter()
        .any(|motif| message.contains(motif))
}

impl Airplay2Output {
    /// La session joue-t-elle, connexion ouverte ? Sinon un volume attend.
    pub(super) async fn session_joue(&self) -> bool {
        self.playing.load(Ordering::SeqCst)
            && !self.paused.load(Ordering::SeqCst)
            && !self.session_perdue.load(Ordering::SeqCst)
            && self.panne.lock().await.is_none()
            && *self.pairing.lock().await == PairingPhase::Connected
    }

    /// Écrit `volume` au daemon et attend son `volume_set`. Le daemon ne
    /// répond rien quand il n'a pas de connexion : passé le délai, la valeur
    /// est gardée pour la reprise, sans mettre la session en panne. L'appelant
    /// tient `transport_command`.
    pub(super) async fn envoyer_le_volume(&self, volume: f64) -> Result<(), String> {
        let (response, receiver) = oneshot::channel();
        *self.pending_transport.lock().await = Some(PendingTransport {
            expected: TransportConfirmation::VolumeSet,
            response,
        });
        if let Err(error) = self
            .send(&serde_json::json!({
                "cmd": "volume",
                "level": volume,
            }))
            .await
        {
            self.pending_transport.lock().await.take();
            return Err(error);
        }
        match tokio::time::timeout(VOLUME_CONFIRMATION_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("airplay2: daemon response channel closed after volume".into()),
            Err(_) => {
                self.pending_transport.lock().await.take();
                warn!(device = %self.name, volume, "airplay2: volume not confirmed, kept for the next resume");
                *self.volume_en_attente.lock().await = Some(volume);
                Ok(())
            }
        }
    }

    /// Envoie, une seule fois, le volume gardé pendant que rien ne jouait.
    /// L'appelant tient `transport_command`.
    pub(super) async fn appliquer_le_volume_en_attente(&self) {
        let Some(volume) = self.volume_en_attente.lock().await.take() else {
            return;
        };
        debug!(device = %self.name, volume, "airplay2: applying the volume set while paused");
        if let Err(error) = self.envoyer_le_volume(volume).await {
            warn!(device = %self.name, error = %error, "airplay2: volume after resume failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Mutex;

    use super::super::{Airplay2Output, DaemonProcess, PairingPhase};
    use crate::outputs::traits::{OutputTarget, PlayMedia, TransportState};

    fn enceinte() -> Airplay2Output {
        Airplay2Output::new(
            "Enceinte jumelée".into(),
            "127.0.0.1".into(),
            7000,
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

    const BROKEN_PIPE: &str = "Connection error: Broken pipe (os error 32)";

    /// Faux daemon à état, fidèle à `airplay-daemon/src/main.rs` : une
    /// commande à la fois ; une connexion fermée par le récepteur reste
    /// `Some` côté daemon, et chaque commande y échoue en « Broken pipe »,
    /// sauf `volume`, dont l'erreur est avalée (`set_volume(..).ok()`) avant
    /// d'émettre `volume_set`. `connect` rouvre une connexion neuve, ou
    /// échoue si `connect_ok` est faux. Rend les commandes reçues (`status`
    /// exclu) et l'interrupteur qui ferme la connexion.
    async fn faux_daemon(
        sortie: &Airplay2Output,
        connect_ok: bool,
    ) -> (Arc<Mutex<Vec<serde_json::Value>>>, Arc<AtomicBool>) {
        let (server_stdin, daemon_stdin) = tokio::io::duplex(64 * 1024);
        let (mut daemon_stdout, server_stdout) = tokio::io::duplex(64 * 1024);
        *sortie.process.lock().await = Some(DaemonProcess {
            child: None,
            stdin: Box::new(server_stdin),
        });
        *sortie.pairing.lock().await = PairingPhase::Connected;
        sortie.start_event_reader(BufReader::new(server_stdout));

        let recues = Arc::new(Mutex::new(Vec::new()));
        let coupee = Arc::new(AtomicBool::new(false));
        let notees = recues.clone();
        let coupure = coupee.clone();
        tokio::spawn(async move {
            let mut lignes = BufReader::new(daemon_stdin).lines();
            while let Ok(Some(ligne)) = lignes.next_line().await {
                let commande: serde_json::Value = serde_json::from_str(&ligne).unwrap();
                let nom = commande["cmd"].as_str().unwrap().to_string();
                if nom != "status" {
                    notees.lock().await.push(commande.clone());
                }
                let morte = coupure.load(Ordering::SeqCst);
                let erreur = |m: String| serde_json::json!({"event": "error", "message": m});
                let evenement = match nom.as_str() {
                    "connect" if connect_ok => {
                        coupure.store(false, Ordering::SeqCst);
                        serde_json::json!({"event": "connected", "device": "Enceinte"})
                    }
                    "connect" => erreur("connect failed: No route to host (os error 113)".into()),
                    "play" if morte => erreur(format!("setup failed: {BROKEN_PIPE}")),
                    "play" => serde_json::json!({"event": "playing"}),
                    "pause" if morte => erreur(format!("pause failed: {BROKEN_PIPE}")),
                    "pause" => serde_json::json!({"event": "paused"}),
                    "resume" if morte => erreur(format!("resume failed: {BROKEN_PIPE}")),
                    "resume" => serde_json::json!({"event": "playing"}),
                    "volume" => serde_json::json!({
                        "event": "volume_set",
                        "volume": commande["level"],
                    }),
                    "status" => serde_json::json!({"event": "status", "position_s": 12.0}),
                    autre => erreur(format!("unknown command: {autre}")),
                };
                let ligne = format!("{}\n", serde_json::to_string(&evenement).unwrap());
                if daemon_stdout.write_all(ligne.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        (recues, coupee)
    }

    async fn noms(recues: &Arc<Mutex<Vec<serde_json::Value>>>) -> Vec<String> {
        recues
            .lock()
            .await
            .iter()
            .map(|c| c["cmd"].as_str().unwrap().to_string())
            .collect()
    }

    async fn volumes(recues: &Arc<Mutex<Vec<serde_json::Value>>>) -> Vec<f64> {
        recues
            .lock()
            .await
            .iter()
            .filter(|c| c["cmd"] == "volume")
            .map(|c| c["level"].as_f64().unwrap())
            .collect()
    }

    /// Le témoin du défaut : connexion fermée pendant la pause, la reprise
    /// échoue en « Broken pipe ». La sortie doit alors le dire à
    /// l'orchestrateur (`device_released_on_pause`), ne plus écrire de
    /// `resume` à une connexion morte, et le `play` du rétablissement doit
    /// rouvrir la session par `connect` avant de jouer.
    #[tokio::test]
    async fn une_connexion_fermee_pendant_la_pause_se_rouvre_a_la_reprise_7921() {
        let sortie = enceinte();
        let (recues, coupee) = faux_daemon(&sortie, true).await;

        sortie.play_media(&piste()).await.unwrap();
        sortie.pause().await.unwrap();
        assert!(!sortie.device_released_on_pause());

        // Le récepteur ferme la connexion pendant la pause.
        coupee.store(true, Ordering::SeqCst);
        let erreur = sortie.resume().await.unwrap_err();
        assert!(erreur.contains("Broken pipe"), "{erreur}");
        assert!(
            sortie.device_released_on_pause(),
            "après « resume failed: Broken pipe », la sortie AirPlay 2 ne dit pas \
             avoir perdu sa session : l'orchestrateur ne la rouvre pas et \
             l'utilisateur voit l'erreur (#7921)"
        );
        assert_eq!(
            sortie.get_status().await.unwrap().state,
            TransportState::Paused,
            "la zone reste en pause, à sa position, jusqu'au rétablissement"
        );

        // Un second appui ne réécrit pas `resume` à une connexion morte.
        assert!(sortie.resume().await.is_err());
        assert_eq!(noms(&recues).await, vec!["play", "pause", "resume"]);

        // Le rétablissement de l'orchestrateur : un `play` à la position.
        sortie
            .play_media(&piste())
            .await
            .expect("le play du rétablissement doit rouvrir la session et jouer");
        assert_eq!(
            noms(&recues).await,
            vec!["play", "pause", "resume", "connect", "play"],
            "le play du rétablissement doit d'abord renvoyer `connect` au daemon"
        );
        assert!(!sortie.device_released_on_pause());
        assert_eq!(
            sortie.get_status().await.unwrap().state,
            TransportState::Playing
        );
        assert_eq!(sortie.pairing_status().await, "connected");
    }

    /// Si la reconnexion est refusée, le `play` du rétablissement échoue avec
    /// la cause du récepteur : c'est la seule erreur que l'utilisateur voit.
    #[tokio::test]
    async fn une_reconnexion_refusee_dit_sa_cause_7921() {
        let sortie = enceinte();
        let (_recues, coupee) = faux_daemon(&sortie, false).await;

        sortie.play_media(&piste()).await.unwrap();
        sortie.pause().await.unwrap();
        coupee.store(true, Ordering::SeqCst);
        assert!(sortie.resume().await.is_err());
        assert!(sortie.device_released_on_pause());

        let erreur = sortie
            .play_media(&piste())
            .await
            .expect_err("une reconnexion refusée ne doit pas passer pour une lecture");
        assert!(
            erreur.contains("connect failed: No route to host"),
            "l'erreur doit dire pourquoi la session n'a pas pu se rouvrir : {erreur}"
        );
        assert!(erreur.contains("Enceinte jumelée"), "{erreur}");
    }

    /// Une trentaine de mouvements du curseur pendant la pause : aucun n'est
    /// écrit au daemon, et la dernière valeur l'est une seule fois, à la
    /// reprise.
    #[tokio::test]
    async fn le_volume_regle_en_pause_est_applique_une_fois_a_la_reprise_7921() {
        let sortie = enceinte();
        let (recues, _coupee) = faux_daemon(&sortie, true).await;

        sortie.play_media(&piste()).await.unwrap();
        sortie.pause().await.unwrap();
        for pas in 30..60 {
            sortie.set_volume(pas as f64 / 100.0).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            volumes(&recues).await,
            Vec::<f64>::new(),
            "des commandes de volume sont parties au daemon pendant la pause (#7921)"
        );
        assert_eq!(sortie.get_status().await.unwrap().volume, 0.59);

        sortie.resume().await.unwrap();
        assert_eq!(
            volumes(&recues).await,
            vec![0.59],
            "la reprise doit appliquer une seule fois le dernier volume réglé en pause"
        );
        assert_eq!(
            noms(&recues).await,
            vec!["play", "pause", "resume", "volume"]
        );
    }

    /// Le volume réglé pendant une pause dont la connexion s'est fermée est
    /// appliqué par la session rouverte.
    #[tokio::test]
    async fn le_volume_regle_en_pause_suit_la_session_rouverte_7921() {
        let sortie = enceinte();
        let (recues, coupee) = faux_daemon(&sortie, true).await;

        sortie.play_media(&piste()).await.unwrap();
        sortie.pause().await.unwrap();
        coupee.store(true, Ordering::SeqCst);
        sortie.set_volume(0.42).await.unwrap();
        assert!(sortie.resume().await.is_err());
        sortie.play_media(&piste()).await.unwrap();

        assert_eq!(volumes(&recues).await, vec![0.42]);
        assert_eq!(
            noms(&recues).await,
            vec!["play", "pause", "resume", "connect", "play", "volume"]
        );
    }

    /// En lecture, `set_volume` ne rend la main qu'au `volume_set` du daemon :
    /// c'est ce qui laisse la file de volume de l'orchestrateur (#5662) ne
    /// garder que la dernière valeur au lieu de tout empiler.
    #[tokio::test]
    async fn en_lecture_le_volume_attend_la_confirmation_du_daemon_7921() {
        let sortie = Arc::new(enceinte());
        let (server_stdin, daemon_stdin) = tokio::io::duplex(64 * 1024);
        let (mut daemon_stdout, server_stdout) = tokio::io::duplex(64 * 1024);
        *sortie.process.lock().await = Some(DaemonProcess {
            child: None,
            stdin: Box::new(server_stdin),
        });
        *sortie.pairing.lock().await = PairingPhase::Connected;
        sortie.start_event_reader(BufReader::new(server_stdout));
        let (volume_ecrit, mut volume_vu) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (liberer, mut libere) = tokio::sync::mpsc::unbounded_channel::<()>();
        tokio::spawn(async move {
            let mut lignes = BufReader::new(daemon_stdin).lines();
            while let Ok(Some(ligne)) = lignes.next_line().await {
                let commande: serde_json::Value = serde_json::from_str(&ligne).unwrap();
                let evenement = match commande["cmd"].as_str().unwrap() {
                    "play" => serde_json::json!({"event": "playing"}),
                    "volume" => {
                        volume_ecrit.send(()).unwrap();
                        libere.recv().await;
                        serde_json::json!({"event": "volume_set"})
                    }
                    _ => continue,
                };
                let ligne = format!("{}\n", serde_json::to_string(&evenement).unwrap());
                daemon_stdout.write_all(ligne.as_bytes()).await.unwrap();
            }
        });

        sortie.play_media(&piste()).await.unwrap();
        let en_vol = {
            let sortie = sortie.clone();
            tokio::spawn(async move { sortie.set_volume(0.5).await })
        };
        volume_vu.recv().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !en_vol.is_finished(),
            "set_volume a rendu la main avant le volume_set du daemon : la file de \
             volume ne coalesce rien et les commandes s'empilent (#7921)"
        );
        liberer.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), en_vol)
            .await
            .expect("set_volume doit finir au volume_set")
            .unwrap()
            .unwrap();
    }

    #[test]
    fn les_erreurs_de_connexion_fermee_sont_reconnues_7921() {
        assert!(super::connexion_perdue(&format!(
            "resume failed: {BROKEN_PIPE}"
        )));
        assert!(super::connexion_perdue(
            "pause failed: Connection reset by peer (os error 104)"
        ));
        assert!(super::connexion_perdue("not connected"));
        assert!(!super::connexion_perdue("resume failed: fixture"));
        assert!(!super::connexion_perdue(
            "setup failed: Operation timed out"
        ));
        assert!(!super::connexion_perdue(
            "pause failed: Operation timed out"
        ));
    }
}
