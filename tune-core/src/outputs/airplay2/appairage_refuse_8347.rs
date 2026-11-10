//! Un récepteur qui refuse l'appairage AirPlay 2, et une session restée
//! coincée après un `setup` manqué (#8347).
//!
//! ## Le constat (Devialet Mania, Windows, rc11)
//!
//! Chaque `connect` du démon finit en `connect failed: RTSP error: Setup
//! failed: Pair-setup M4 processing failed: … Server returned error in M4
//! (wrong PIN?)`, puis, à la tentative suivante, `… error in M2`. Le démon
//! (`airplay2-rs`, `Connection::connect_with_pin_pairing`) fait un
//! pair-setup HomeKit « normal » (HKP=3, M1 à M6) avec le code par défaut
//! `3939` : le récepteur compare la preuve SRP à SON code et la refuse en
//! M4. Un récepteur qui vient de refuser une preuve refuse ensuite pendant
//! un temps toute nouvelle session dès M2 (protection contre l'essai de
//! codes). Chaque lecture relançait pourtant un `connect`, toutes les 15 à
//! 25 s, et entretenait ce refus.
//!
//! Et quand un `connect` avait réussi mais que le `setup` RTSP de la
//! lecture avait échoué après sa phase 1, le démon gardait cette connexion :
//! toute lecture suivante rappelait `setup` sur elle et finissait en
//! « Cannot process phase 1 from state SetupPhase1 », message qui cachait la
//! vraie cause.
//!
//! ## Le correctif
//!
//! - Un refus d'appairage (pair-setup ou pair-verify, étape M1 à M6) est
//!   nommé comme tel, avec la marche à suivre ; pendant
//!   [`DELAI_APRES_UN_REFUS`], la lecture le redit sans réécrire au
//!   récepteur. Un appairage par code (`start_pin_pairing`, `submit_pin`) ou
//!   une connexion réussie lève l'attente.
//! - Après un `setup failed` ou un `play failed`, la connexion du démon est
//!   tenue pour inutilisable : la lecture suivante lui envoie `disconnect`
//!   puis rouvre une connexion neuve.

use std::time::{Duration, Instant};

use super::Airplay2Output;

/// Pendant ce délai après un refus d'appairage, la lecture ne recontacte pas
/// le récepteur.
pub(super) const DELAI_APRES_UN_REFUS: Duration = Duration::from_secs(30);

/// Le dernier refus d'appairage du récepteur : quand, et la cause du démon.
#[derive(Clone, Debug)]
pub(super) struct RefusDAppairage {
    pub(super) depuis: Instant,
    pub(super) cause: String,
}

/// L'étape refusée (`pair-setup M4`, `pair-verify M2`…) d'une erreur de
/// `connect` du démon, `None` si l'erreur n'est pas un refus d'appairage.
pub(super) fn etape_refusee(message: &str) -> Option<String> {
    if !message.starts_with("connect failed") {
        return None;
    }
    for (marque, nom) in [
        ("Pair-setup M", "pair-setup"),
        ("Pair-verify M", "pair-verify"),
    ] {
        if let Some(reste) = message.split(marque).nth(1)
            && let Some(chiffre) = reste.chars().next().filter(char::is_ascii_digit)
        {
            return Some(format!("{nom} M{chiffre}"));
        }
    }
    None
}

/// Une erreur de lecture du démon après laquelle sa connexion n'est plus
/// réutilisable : la session RTSP est restée au milieu de son `setup`.
pub(super) fn session_a_rouvrir(message: &str) -> bool {
    message.starts_with("setup failed") || message.starts_with("play failed")
}

impl Airplay2Output {
    /// La phrase d'un refus d'appairage : le récepteur, l'étape, la marche à
    /// suivre, et la cause brute du démon.
    pub(super) fn message_refus_d_appairage(&self, cause: &str) -> String {
        let etape = etape_refusee(cause).unwrap_or_else(|| "pairing".to_string());
        format!(
            "AirPlay 2 receiver \"{}\" refused pairing ({etape}): it did not accept \
             Tune's default AirPlay 2 code. If this speaker or its app uses an \
             AirPlay code or password, enter it with \"Pair\" on the zone. \
             Receiver error: {cause}",
            self.name
        )
    }

    /// Le refus d'appairage encore récent, et le temps qui reste avant de
    /// recontacter le récepteur. `None` une fois le délai passé.
    pub(super) async fn refus_recent(&self) -> Option<(RefusDAppairage, Duration)> {
        let mut refus = self.refus_d_appairage.lock().await;
        let recent = refus.clone()?;
        let ecoule = recent.depuis.elapsed();
        if ecoule >= self.delai_apres_un_refus {
            *refus = None;
            return None;
        }
        Some((recent, self.delai_apres_un_refus - ecoule))
    }

    /// La lecture refusée sans recontacter le récepteur, pendant le délai
    /// qui suit un refus d'appairage.
    pub(super) async fn refus_encore_recent(&self) -> Option<String> {
        let (refus, reste) = self.refus_recent().await?;
        Some(format!(
            "{} (Tune waits {} s before asking the speaker again.)",
            self.message_refus_d_appairage(&refus.cause),
            reste.as_secs().max(1)
        ))
    }
}

#[cfg(test)]
impl Airplay2Output {
    /// Banc d'essai : raccourcit le délai qui suit un refus d'appairage.
    fn avec_delai_apres_un_refus(mut self, delai: Duration) -> Self {
        self.delai_apres_un_refus = delai;
        self
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Mutex;

    use super::super::{Airplay2Output, DaemonProcess};
    use super::{etape_refusee, session_a_rouvrir};
    use crate::outputs::traits::{OutputTarget, PlayMedia};

    /// Les messages exacts du journal de Kevin (rc11, Windows, Devialet
    /// Mania ER29).
    const REFUS_M4: &str = "connect failed: RTSP error: Setup failed: Pair-setup M4 processing \
        failed: Pairing error: Protocol error: Server returned error in M4 (wrong PIN?)";
    const REFUS_M2: &str = "connect failed: RTSP error: Setup failed: Pair-setup M2 processing \
        failed: Pairing error: Protocol error: Server returned error in M2";
    const PHASE_1: &str =
        "setup failed: RTSP error: Setup failed: Cannot process phase 1 from state SetupPhase1";

    fn mania() -> Airplay2Output {
        Airplay2Output::new(
            "Mania 2".into(),
            "127.0.0.1".into(),
            7000,
            "fixture-8347".into(),
            "00:11:22:33:44:55".into(),
        )
    }

    fn piste() -> PlayMedia<'static> {
        PlayMedia {
            url: "/tmp/fixture-8347.flac",
            mime_type: "audio/flac",
            title: Some("Fixture"),
            ..Default::default()
        }
    }

    /// Un faux démon qui note les ordres reçus (`status` excepté) et répond
    /// ce que `reponse(commande, ordres_deja_recus)` lui dit.
    async fn faux_daemon<F>(sortie: &Airplay2Output, reponse: F) -> Arc<Mutex<Vec<String>>>
    where
        F: Fn(&str, &[String]) -> Vec<serde_json::Value> + Send + 'static,
    {
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
            let mut lignes = BufReader::new(daemon_stdin).lines();
            while let Ok(Some(ligne)) = lignes.next_line().await {
                let commande: serde_json::Value = serde_json::from_str(&ligne).unwrap();
                let nom = commande["cmd"].as_str().unwrap().to_string();
                if nom == "status" {
                    continue;
                }
                let deja = notees.lock().await.clone();
                notees.lock().await.push(nom.clone());
                for evenement in reponse(&nom, &deja) {
                    let ligne = format!("{}\n", serde_json::to_string(&evenement).unwrap());
                    daemon_stdout.write_all(ligne.as_bytes()).await.unwrap();
                }
            }
        });
        recues
    }

    fn erreur(message: &str) -> serde_json::Value {
        serde_json::json!({"event": "error", "message": message})
    }

    /// Un démon tenu par une Mania qui refuse toujours l'appairage : `connect`
    /// rend le refus, `play` « not connected ».
    async fn demon_d_une_mania_qui_refuse(sortie: &Airplay2Output) -> Arc<Mutex<Vec<String>>> {
        faux_daemon(sortie, |commande, deja| match commande {
            "connect" if deja.iter().any(|c| c == "connect") => vec![erreur(REFUS_M2)],
            "connect" => vec![erreur(REFUS_M4)],
            "play" => vec![erreur("not connected")],
            _ => vec![],
        })
        .await
    }

    #[test]
    fn l_etape_refusee_se_lit_dans_l_erreur_du_demon_8347() {
        assert_eq!(etape_refusee(REFUS_M4).as_deref(), Some("pair-setup M4"));
        assert_eq!(etape_refusee(REFUS_M2).as_deref(), Some("pair-setup M2"));
        assert_eq!(
            etape_refusee(
                "connect failed: RTSP error: Setup failed: Pair-verify M2 processing failed: x"
            )
            .as_deref(),
            Some("pair-verify M2")
        );
        for autre in [
            "not connected",
            "connect failed: Connection refused (os error 111)",
            PHASE_1,
            "pair-pin-start: HTTP 403 Forbidden",
        ] {
            assert_eq!(etape_refusee(autre), None, "{autre}");
        }
        assert!(session_a_rouvrir(PHASE_1));
        assert!(session_a_rouvrir("play failed: Broken pipe"));
        assert!(!session_a_rouvrir(REFUS_M4));
        assert!(!session_a_rouvrir("not connected"));
    }

    /// 🔴 #8347 — la Mania refuse l'appairage en M4 : la lecture le dit
    /// (récepteur, étape, marche à suivre), et la lecture suivante, dans le
    /// délai, ne relance pas l'appairage qui finissait en M2.
    #[tokio::test]
    async fn un_refus_d_appairage_est_nomme_et_n_est_pas_relance_a_chaque_lecture_8347() {
        let sortie = mania();
        let recues = demon_d_une_mania_qui_refuse(&sortie).await;

        let premiere = sortie
            .play_media(&piste())
            .await
            .expect_err("un appairage refusé ne doit pas passer pour une lecture");
        assert!(
            premiere.contains("\"Mania 2\" refused pairing (pair-setup M4)"),
            "le refus d'appairage doit être nommé : {premiere}"
        );
        assert!(
            premiere.contains("\"Pair\""),
            "la marche à suivre doit être dite : {premiere}"
        );

        let seconde = sortie
            .play_media(&piste())
            .await
            .expect_err("la Mania refuse toujours");
        assert!(
            seconde.contains("refused pairing (pair-setup M4)") && seconde.contains("waits"),
            "dans le délai, la lecture redit le refus sans recontacter l'enceinte : {seconde}"
        );
        assert_eq!(
            recues
                .lock()
                .await
                .iter()
                .filter(|c| *c == "connect")
                .count(),
            1,
            "un seul appairage tenté dans le délai : {:?}",
            recues.lock().await
        );
    }

    /// #8347 — le délai passé, la lecture retente l'appairage.
    #[tokio::test]
    async fn le_delai_passe_la_lecture_retente_l_appairage_8347() {
        let sortie = mania().avec_delai_apres_un_refus(Duration::ZERO);
        let recues = demon_d_une_mania_qui_refuse(&sortie).await;

        sortie.play_media(&piste()).await.unwrap_err();
        let seconde = sortie.play_media(&piste()).await.unwrap_err();
        assert!(
            seconde.contains("refused pairing (pair-setup M2)"),
            "{seconde}"
        );
        assert_eq!(
            recues
                .lock()
                .await
                .iter()
                .filter(|c| *c == "connect")
                .count(),
            2
        );
    }

    /// #8347 — un appairage par code lancé par l'utilisateur lève l'attente.
    #[tokio::test]
    async fn l_appairage_par_code_leve_l_attente_8347() {
        let sortie = mania();
        demon_d_une_mania_qui_refuse(&sortie).await;
        sortie.play_media(&piste()).await.unwrap_err();
        assert!(sortie.refus_recent().await.is_some());

        sortie.start_pin_pairing().await.unwrap();
        assert!(
            sortie.refus_recent().await.is_none(),
            "relancer l'appairage soi-même lève l'attente"
        );
    }

    /// #8347 — un mauvais code saisi par l'utilisateur n'est pas un refus du
    /// code par défaut : il ne retient pas la lecture.
    #[tokio::test]
    async fn un_mauvais_code_saisi_ne_bloque_pas_la_lecture_8347() {
        let sortie = mania();
        let recues = demon_d_une_mania_qui_refuse(&sortie).await;
        sortie.start_pin_pairing().await.unwrap();
        sortie.submit_pin("1234").await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !sortie.pairing_status().await.starts_with("failed:") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("le refus du code doit arriver");
        assert!(sortie.refus_recent().await.is_none());
        sortie.play_media(&piste()).await.unwrap_err();
        assert_eq!(
            recues
                .lock()
                .await
                .iter()
                .filter(|c| *c == "connect")
                .count(),
            2,
            "la lecture retente le code par défaut : {:?}",
            recues.lock().await
        );
    }

    /// #8347 — une session qui joue efface le refus : la piste suivante
    /// n'est pas retenue.
    #[tokio::test]
    async fn une_lecture_qui_joue_efface_le_refus_8347() {
        let sortie = mania();
        // Un `connect` refusé pendant que le démon garde une connexion
        // antérieure, qui joue ensuite.
        faux_daemon(&sortie, |commande, _| match commande {
            "volume" => vec![erreur(REFUS_M4), serde_json::json!({"event": "playing"})],
            _ => vec![],
        })
        .await;
        sortie.set_volume(0.5).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while sortie.get_status().await.unwrap().state
                != crate::outputs::traits::TransportState::Playing
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("le démon doit dire qu'il joue");
        assert!(
            sortie.refus_recent().await.is_none(),
            "une session qui joue ne doit pas laisser de refus"
        );
    }

    /// 🔴 #8347 — un `setup` manqué sur une connexion établie : la lecture
    /// suivante lâche cette connexion et en rouvre une neuve, au lieu de
    /// rappeler `setup` sur elle (« Cannot process phase 1 from state
    /// SetupPhase1 », capture de Kevin).
    #[tokio::test]
    async fn apres_un_setup_manque_la_lecture_suivante_rouvre_une_connexion_neuve_8347() {
        let sortie = mania();
        let recues = faux_daemon(&sortie, |commande, deja| {
            let plays = deja.iter().filter(|c| *c == "play").count();
            let rouverte = deja.iter().any(|c| c == "disconnect");
            match commande {
                "connect" => vec![serde_json::json!({"event": "connected", "device": "Mania 2"})],
                "disconnect" => vec![serde_json::json!({"event": "disconnected"})],
                "play" if plays == 0 => {
                    vec![erreur("setup failed: RTSP error: SETUP phase 2 refused")]
                }
                // Le vrai démon rappelle `setup` sur la connexion coincée.
                "play" if !rouverte => vec![erreur(PHASE_1)],
                "play" => vec![serde_json::json!({"event": "playing"})],
                _ => vec![],
            }
        })
        .await;

        let premiere = sortie.play_media(&piste()).await.unwrap_err();
        assert!(premiere.contains("SETUP phase 2 refused"), "{premiere}");

        sortie
            .play_media(&piste())
            .await
            .expect("la lecture suivante doit repartir d'une connexion neuve");
        assert_eq!(
            *recues.lock().await,
            vec!["connect", "play", "disconnect", "connect", "play"]
        );
        sortie.stop().await.unwrap();
    }
}

#[cfg(test)]
mod tests_relecture {
    use super::super::Airplay2Output;
    use std::sync::atomic::Ordering;

    fn mania() -> Airplay2Output {
        Airplay2Output::new(
            "Mania 2".into(),
            "127.0.0.1".into(),
            7000,
            "fixture-8347b".into(),
            "00:11:22:33:44:55".into(),
        )
    }

    /// #8347 — `stop` tue le démon et sa connexion coincée : la lecture
    /// suivante n'a rien à lâcher.
    #[tokio::test]
    async fn stop_oublie_la_session_a_rouvrir_8347() {
        let sortie = mania();
        sortie.session_a_rouvrir.store(true, Ordering::SeqCst);
        crate::outputs::traits::OutputTarget::stop(&sortie)
            .await
            .unwrap();
        assert!(!sortie.session_a_rouvrir.load(Ordering::SeqCst));
    }
}
