//! #4598 — ré-énumérer le parc local AVANT de refuser une zone locale.
//!
//! Le garde-fou `gate_or_rebind_offline_zone` refuse une zone `local:` hors
//! ligne et absente du registre vivant. Il LIT ce registre et ne le rafraîchit
//! jamais ; seule une boucle périodique le remplit en régime établi (120 s sous
//! macOS/Windows, 600 s sous Linux). Mesuré chez Cyrille (fil 1861, 0.9.158
//! macOS) : `play_rejected_zone_offline` à 13:37:27 sur un parc énuméré 47 s
//! plus tôt, et le DAC iFi apparaît dans l'énumération suivante, 12 s après le
//! refus — « Vérifiez qu'elle est branchée et allumée », alors qu'elle l'est.
//!
//! Remède, au plus une fois par clic et seulement sur le chemin du refus : une
//! énumération bornée dans le temps ; si l'appareil y est, la lecture est
//! laissée passer (`recreate_local_and_play` sait ouvrir un périphérique qui
//! n'est pas au registre, comme pour le parc vide de #3737). Sinon, ou si
//! l'énumération n'a pas eu lieu, le refus reste exactement celui d'avant.

use super::*;

/// Délai de garde de l'énumération à la demande. Mesuré chez le testeur : une
/// énumération CoreAudio complète de 13 sorties prend ~1,1 s. Au-delà, le clic
/// retombe sur le refus d'avant plutôt que d'attendre un pilote qui bloque.
pub(crate) const DELAI_REENUMERATION_AVANT_REFUS: std::time::Duration =
    std::time::Duration::from_secs(3);

/// L'énumération du parc local, injectable : rend les identifiants
/// `local:{nom}` des sorties vues pour le backend demandé.
pub(crate) type EnumerateurDeParcLocal = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

/// L'énumérateur de production : `list_audio_devices_with_backend`, qui garde
/// déjà le verrou d'énumération, le cache de 5 s, la porte ASIO bloquée
/// (#4556) et le refus de rouvrir un pilote ASIO occupé (#1267).
///
/// Sous `cfg(test)`, un parc VIDE : aucun test n'énumère le matériel de la
/// machine qui l'exécute (ni ne publie son parc dans l'état global de
/// `outputs::local`). Les témoins de #4598 injectent le leur.
pub(crate) fn enumerateur_de_production() -> EnumerateurDeParcLocal {
    #[cfg(all(feature = "local-audio", not(test)))]
    {
        Arc::new(|backend: &str| {
            crate::outputs::local::list_audio_devices_with_backend(backend)
                .into_iter()
                .map(|d| format!("local:{}", d.name))
                .collect()
        })
    }
    #[cfg(any(not(feature = "local-audio"), test))]
    {
        Arc::new(|_: &str| Vec::new())
    }
}

/// Pourquoi on n'énumère PAS avant de refuser (`Ok(())` : on énumère).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasDeReenumeration {
    /// Zone réseau : son registre est tenu par SSDP/mDNS, pas par une énumération.
    PasUneSortieLocale,
    /// ASIO configuré : sonder ASIO depuis un clic peut tuer le pilote (#1267)
    /// ou bloquer derrière une autre application (#4168), et une énumération
    /// WASAPI de repli ne porte pas les noms ASIO (#4556).
    BackendAsio,
    /// Une sortie locale joue : énumérer WASAPI sonde les formats de chaque
    /// périphérique et peut invalider le flux actif (DEvir).
    LectureLocaleEnCours,
}

/// La décision, pure : faut-il ré-énumérer avant de refuser ?
pub(crate) fn plan_de_reenumeration(
    dev_id: &str,
    backend: &str,
    lecture_locale_en_cours: bool,
) -> Result<(), PasDeReenumeration> {
    if !dev_id.starts_with("local:") {
        return Err(PasDeReenumeration::PasUneSortieLocale);
    }
    if backend.trim().eq_ignore_ascii_case("asio") {
        return Err(PasDeReenumeration::BackendAsio);
    }
    if lecture_locale_en_cours {
        return Err(PasDeReenumeration::LectureLocaleEnCours);
    }
    Ok(())
}

impl PlaybackOrchestrator {
    /// Une sortie `local:` du registre est-elle en train de jouer ?
    async fn une_sortie_locale_joue(&self) -> bool {
        let sorties: Vec<_> = {
            let registre = self.outputs.lock().await;
            registre
                .list()
                .into_iter()
                .filter(|id| id.starts_with("local:"))
                .filter_map(|id| registre.get(&id))
                .collect()
        };
        for sortie in sorties {
            let sortie = sortie.lock().await;
            if let Ok(statut) = sortie.get_status().await {
                if statut.state == crate::outputs::traits::TransportState::Playing {
                    return true;
                }
            }
        }
        false
    }

    /// #4598 — l'appareil de la zone est-il revenu depuis la dernière
    /// énumération ? `true` seulement si une énumération à la demande a eu
    /// lieu dans le délai ET qu'elle le voit.
    pub(super) async fn appareil_local_revenu(
        &self,
        zone_id: i64,
        dev_id: &str,
        delai: std::time::Duration,
    ) -> bool {
        let (_, backend) = self.reglages_sortie_locale();
        let lecture = dev_id.starts_with("local:") && self.une_sortie_locale_joue().await;
        if let Err(raison) = plan_de_reenumeration(dev_id, &backend, lecture) {
            if raison != PasDeReenumeration::PasUneSortieLocale {
                info!(zone_id, device = dev_id, raison = ?raison, "local_reenumeration_before_reject_skipped");
            }
            return false;
        }
        let enumerer = self.enumerer_parc_local.clone();
        let backend_tache = backend.clone();
        let debut = std::time::Instant::now();
        let issue = tokio::time::timeout(
            delai,
            tokio::task::spawn_blocking(move || enumerer(&backend_tache)),
        )
        .await;
        let ms = debut.elapsed().as_millis() as u64;
        match issue {
            Ok(Ok(parc)) => {
                let revenu = parc.iter().any(|id| id == dev_id);
                info!(
                    zone_id,
                    device = dev_id,
                    backend = %backend,
                    count = parc.len(),
                    found = revenu,
                    elapsed_ms = ms,
                    "local_reenumeration_before_reject"
                );
                revenu
            }
            Ok(Err(e)) => {
                warn!(zone_id, device = dev_id, error = %e, "local_reenumeration_before_reject_failed");
                false
            }
            Err(_) => {
                warn!(
                    zone_id,
                    device = dev_id,
                    timeout_ms = delai.as_millis() as u64,
                    "local_reenumeration_before_reject_timed_out"
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::zone_repo::ZoneRepo;
    use crate::outputs::mock::MockOutput;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn orchestrateur() -> PlaybackOrchestrator {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        PlaybackOrchestrator::new(
            Arc::new(db),
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            None,
        )
    }

    const DAC: &str = "local:iFi (by AMR) HD USB Audio ";

    /// Une zone locale hors ligne, son DAC absent du registre, et UN autre
    /// appareil local au registre (le parc n'est pas vide : la garde #3737
    /// ne s'applique pas, on est bien sur le chemin du refus).
    async fn zone_dont_le_dac_est_absent(orch: &PlaybackOrchestrator) -> i64 {
        orch.outputs.lock().await.register(Box::new(
            MockOutput::new("local:Haut-parleurs Mac mini", "Haut-parleurs Mac mini")
                .with_type("local"),
        ));
        let repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = repo
            .create("iFi (by AMR) HD USB Audio", Some("local"), Some(DAC))
            .unwrap();
        repo.update_online(zone_id, false).unwrap();
        zone_id
    }

    /// Le témoin du fil 1861 : le DAC est apparu depuis la dernière
    /// énumération périodique. Le clic doit passer, sans attendre le tick.
    ///
    /// Rouge attendu sans correctif : `zone_output_unavailable`.
    #[tokio::test]
    async fn un_dac_apparu_depuis_la_derniere_enumeration_n_est_plus_refuse_4598() {
        let mut orch = orchestrateur();
        let appels = Arc::new(AtomicUsize::new(0));
        let compteur = appels.clone();
        orch.enumerer_parc_local = Arc::new(move |_| {
            compteur.fetch_add(1, Ordering::SeqCst);
            vec!["local:Haut-parleurs Mac mini".into(), DAC.into()]
        });
        let zone_id = zone_dont_le_dac_est_absent(&orch).await;
        let zone = ZoneRepo::with_backend(orch.db.clone())
            .get(zone_id)
            .unwrap()
            .unwrap();
        let verdict = orch.gate_or_rebind_offline_zone(zone_id, &zone).await;
        assert_eq!(
            verdict,
            Ok(None),
            "le DAC est branché et l'énumération le voit : le refus sur un parc \
             périmé est le défaut de #4598"
        );
        assert_eq!(
            appels.load(Ordering::SeqCst),
            1,
            "une seule énumération par clic"
        );
    }

    /// CONTRE-ÉPREUVE : l'énumération à la demande ne voit toujours pas
    /// l'appareil ⇒ le refus d'avant, inchangé.
    #[tokio::test]
    async fn un_dac_toujours_absent_reste_refuse_4598() {
        let mut orch = orchestrateur();
        orch.enumerer_parc_local = Arc::new(|_| vec!["local:Haut-parleurs Mac mini".into()]);
        let zone_id = zone_dont_le_dac_est_absent(&orch).await;
        let zone = ZoneRepo::with_backend(orch.db.clone())
            .get(zone_id)
            .unwrap()
            .unwrap();
        let verdict = orch.gate_or_rebind_offline_zone(zone_id, &zone).await;
        assert!(
            verdict
                .as_ref()
                .is_err_and(|e| e.starts_with("zone_output_unavailable")),
            "{verdict:?}"
        );
    }

    /// Une énumération qui BLOQUE (pilote tenu par une autre application,
    /// #4168) ne tient pas le clic au-delà du délai : refus d'avant.
    #[tokio::test]
    async fn une_enumeration_qui_bloque_retombe_sur_le_refus_dans_le_delai_4598() {
        let mut orch = orchestrateur();
        orch.enumerer_parc_local = Arc::new(|_| {
            std::thread::sleep(std::time::Duration::from_millis(600));
            vec![DAC.into()]
        });
        let debut = std::time::Instant::now();
        let revenu = orch
            .appareil_local_revenu(1, DAC, std::time::Duration::from_millis(100))
            .await;
        assert!(!revenu, "passé le délai, on ne conclut rien");
        assert!(debut.elapsed() < std::time::Duration::from_millis(500));
    }

    #[test]
    fn plan_de_reenumeration_ne_sonde_ni_le_reseau_ni_asio_ni_en_lecture_4598() {
        assert_eq!(plan_de_reenumeration(DAC, "auto", false), Ok(()));
        assert_eq!(plan_de_reenumeration(DAC, "wasapi", false), Ok(()));
        assert_eq!(
            plan_de_reenumeration("uuid:dlna-1", "auto", false),
            Err(PasDeReenumeration::PasUneSortieLocale)
        );
        assert_eq!(
            plan_de_reenumeration(DAC, " ASIO ", false),
            Err(PasDeReenumeration::BackendAsio)
        );
        assert_eq!(
            plan_de_reenumeration(DAC, "auto", true),
            Err(PasDeReenumeration::LectureLocaleEnCours)
        );
    }

    /// Une zone locale dont une AUTRE sortie locale joue : pas d'énumération
    /// (elle pourrait couper ce qui joue), refus d'avant.
    #[tokio::test]
    async fn pas_d_enumeration_pendant_qu_une_sortie_locale_joue_4598() {
        let mut orch = orchestrateur();
        let appels = Arc::new(AtomicUsize::new(0));
        let compteur = appels.clone();
        orch.enumerer_parc_local = Arc::new(move |_| {
            compteur.fetch_add(1, Ordering::SeqCst);
            vec![DAC.into()]
        });
        let zone_id = zone_dont_le_dac_est_absent(&orch).await;
        {
            let reg = orch.outputs.lock().await;
            let arc = reg.get("local:Haut-parleurs Mac mini").unwrap();
            let sortie = arc.lock().await;
            let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
            mock.set_state(crate::outputs::traits::TransportState::Playing)
                .await;
        }
        let zone = ZoneRepo::with_backend(orch.db.clone())
            .get(zone_id)
            .unwrap()
            .unwrap();
        assert!(
            orch.gate_or_rebind_offline_zone(zone_id, &zone)
                .await
                .is_err()
        );
        assert_eq!(appels.load(Ordering::SeqCst), 0);
    }
}
