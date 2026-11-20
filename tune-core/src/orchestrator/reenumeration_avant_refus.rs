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

/// #8171 — une sortie locale vue par l'énumération à la demande : son
/// identifiant `local:{nom}`, l'hôte audio qui l'a énumérée (`"WASAPI"`,
/// `"CoreAudio"`…) et son identifiant d'endpoint (vide quand l'hôte n'en
/// expose pas).
///
/// L'hôte voyage avec le nom pour la même raison qu'au démarrage (#3230) : un
/// nom WASAPI retrouvé alors qu'ASIO est configuré doit s'ouvrir en WASAPI. Une
/// sortie recréée sans lui partait sur l'hôte ASIO, qui ne connaît pas ce nom,
/// et la lecture échouait encore — jusqu'au redémarrage du PC (fil 2708).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SortieLocaleVue {
    pub(crate) device_id: String,
    pub(crate) hote: String,
    pub(crate) endpoint_id: String,
}

impl SortieLocaleVue {
    /// Une sortie vue, telle qu'un témoin la décrit.
    #[cfg(test)]
    pub(crate) fn wasapi(device_id: &str, endpoint_id: &str) -> Self {
        Self {
            device_id: device_id.to_string(),
            hote: "WASAPI".to_string(),
            endpoint_id: endpoint_id.to_string(),
        }
    }
}

/// Ce qu'on énumère avant de refuser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnumerationAvantRefus {
    /// L'énumération complète du backend configuré (`list_audio_devices_with_backend`),
    /// avec son sondage des formats : le comportement de #4598, inchangé hors
    /// Windows.
    Complete(String),
    /// #8171 — Windows : la liste des endpoints WASAPI, noms et identifiants
    /// seulement, SANS sonder les formats. C'est la lecture que fait déjà
    /// chaque ouverture d'une sortie partagée (`find_device_with_fallback`) :
    /// elle ne touche ni au pilote ASIO, ni au flux qui joue.
    WasapiSansSonder,
}

/// L'énumération du parc local, injectable : rend les sorties vues pour ce
/// qui est demandé.
pub(crate) type EnumerateurDeParcLocal =
    Arc<dyn Fn(&EnumerationAvantRefus) -> Vec<SortieLocaleVue> + Send + Sync>;

/// L'énumérateur de production.
///
/// - `Complete` : `list_audio_devices_with_backend`, qui garde déjà le verrou
///   d'énumération, le cache de 5 s, la porte ASIO bloquée (#4556) et le refus
///   de rouvrir un pilote ASIO occupé (#1267).
/// - `WasapiSansSonder` : `sorties_wasapi_sans_sonder` (#8171), Windows seulement.
///
/// Sous `cfg(test)`, un parc VIDE : aucun test n'énumère le matériel de la
/// machine qui l'exécute (ni ne publie son parc dans l'état global de
/// `outputs::local`). Les témoins de #4598 et #8171 injectent le leur.
pub(crate) fn enumerateur_de_production() -> EnumerateurDeParcLocal {
    #[cfg(all(feature = "local-audio", not(test)))]
    {
        Arc::new(|demande: &EnumerationAvantRefus| match demande {
            EnumerationAvantRefus::Complete(backend) => {
                crate::outputs::local::list_audio_devices_with_backend(backend)
                    .into_iter()
                    .map(|d| SortieLocaleVue {
                        device_id: format!("local:{}", d.name),
                        hote: d.backend,
                        endpoint_id: d.endpoint_id,
                    })
                    .collect()
            }
            EnumerationAvantRefus::WasapiSansSonder => {
                crate::outputs::local::sorties_wasapi_sans_sonder()
                    .into_iter()
                    .map(|(nom, endpoint_id, hote)| SortieLocaleVue {
                        device_id: format!("local:{nom}"),
                        hote,
                        endpoint_id,
                    })
                    .collect()
            }
        })
    }
    #[cfg(any(not(feature = "local-audio"), test))]
    {
        Arc::new(|_: &EnumerationAvantRefus| Vec::new())
    }
}

/// Pourquoi on n'énumère PAS avant de refuser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasDeReenumeration {
    /// Zone réseau : son registre est tenu par SSDP/mDNS, pas par une énumération.
    PasUneSortieLocale,
    /// ASIO configuré hors Windows : aucun hôte à sonder sans risque. Sous
    /// Windows, voir [`EnumerationAvantRefus::WasapiSansSonder`] (#8171).
    BackendAsio,
    /// Une sortie locale joue : l'énumération COMPLÈTE sonde les formats de
    /// chaque périphérique et peut invalider le flux actif (DEvir).
    LectureLocaleEnCours,
}

/// La décision, pure : faut-il ré-énumérer avant de refuser, et quoi ?
///
/// `systeme` est `std::env::consts::OS` en production, un paramètre pour que
/// la règle Windows se juge depuis Linux.
///
/// #8171 (fil 2708) — sous Windows avec ASIO configuré, cette énumération
/// était SAUTÉE (`BackendAsio`), et aussi dès qu'une autre sortie locale
/// jouait. Or c'est exactement le geste du testeur : il écoute sur les
/// haut-parleurs du PC, allume son DAC, choisit la zone du DAC. La seule
/// chose qui pouvait encore enregistrer le DAC était le rescan périodique —
/// qui saute lui aussi tant qu'une sortie locale joue. La zone restait refusée
/// jusqu'au redémarrage du PC. Sous Windows, on lit désormais la liste des
/// endpoints WASAPI sans sonder : ni pilote ASIO ouvert, ni format sondé.
pub(crate) fn plan_de_reenumeration(
    dev_id: &str,
    backend: &str,
    lecture_locale_en_cours: bool,
    systeme: &str,
) -> Result<EnumerationAvantRefus, PasDeReenumeration> {
    if !dev_id.starts_with("local:") {
        return Err(PasDeReenumeration::PasUneSortieLocale);
    }
    let backend = backend.trim();
    if systeme == "windows"
        && (backend.eq_ignore_ascii_case("asio") || backend.eq_ignore_ascii_case("wasapi"))
    {
        return Ok(EnumerationAvantRefus::WasapiSansSonder);
    }
    if backend.eq_ignore_ascii_case("asio") {
        return Err(PasDeReenumeration::BackendAsio);
    }
    if lecture_locale_en_cours {
        return Err(PasDeReenumeration::LectureLocaleEnCours);
    }
    Ok(EnumerationAvantRefus::Complete(backend.to_string()))
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
            if let Ok(statut) = sortie.get_status().await
                && statut.state == crate::outputs::traits::TransportState::Playing
            {
                return true;
            }
        }
        false
    }

    /// #4598 — l'appareil de la zone est-il revenu depuis la dernière
    /// énumération ? `Some` seulement si une énumération à la demande a eu
    /// lieu dans le délai ET qu'elle le voit ; elle rend alors ce qu'elle a vu
    /// (hôte et endpoint compris, #8171).
    pub(super) async fn appareil_local_revenu(
        &self,
        zone_id: i64,
        dev_id: &str,
        delai: std::time::Duration,
    ) -> Option<SortieLocaleVue> {
        let (_, backend) = self.reglages_sortie_locale();
        let lecture = dev_id.starts_with("local:") && self.une_sortie_locale_joue().await;
        let demande = match plan_de_reenumeration(
            dev_id,
            &backend,
            lecture,
            self.systeme_du_parc_local,
        ) {
            Ok(demande) => demande,
            Err(raison) => {
                if raison != PasDeReenumeration::PasUneSortieLocale {
                    info!(zone_id, device = dev_id, raison = ?raison, "local_reenumeration_before_reject_skipped");
                }
                return None;
            }
        };
        let enumerer = self.enumerer_parc_local.clone();
        let demande_tache = demande.clone();
        let debut = std::time::Instant::now();
        let issue = tokio::time::timeout(
            delai,
            tokio::task::spawn_blocking(move || enumerer(&demande_tache)),
        )
        .await;
        let ms = debut.elapsed().as_millis() as u64;
        match issue {
            Ok(Ok(parc)) => {
                let revenue = parc.iter().find(|vue| vue.device_id == dev_id).cloned();
                info!(
                    zone_id,
                    device = dev_id,
                    backend = %backend,
                    enumeration = ?demande,
                    count = parc.len(),
                    found = revenue.is_some(),
                    hote = revenue.as_ref().map(|v| v.hote.as_str()).unwrap_or(""),
                    elapsed_ms = ms,
                    "local_reenumeration_before_reject"
                );
                revenue
            }
            Ok(Err(e)) => {
                warn!(zone_id, device = dev_id, error = %e, "local_reenumeration_before_reject_failed");
                None
            }
            Err(_) => {
                warn!(
                    zone_id,
                    device = dev_id,
                    timeout_ms = delai.as_millis() as u64,
                    "local_reenumeration_before_reject_timed_out"
                );
                None
            }
        }
    }

    /// #8171 — l'appareil vient d'être retrouvé par l'énumération à la
    /// demande : l'enregistrer comme le ferait le rescan, avec l'hôte qui l'a
    /// énuméré et son endpoint, et remettre sa zone en ligne.
    ///
    /// Sans l'hôte, la sortie recréée à la volée (`recreate_local_and_play`)
    /// prenait le backend configuré : ASIO, chez le testeur, pour un nom
    /// WASAPI. L'ouverture échouait (« ASIO device not found »), sans repli —
    /// et sans son, jusqu'au redémarrage.
    ///
    /// Rend `true` quand une sortie a été enregistrée (ou l'était déjà).
    pub(super) async fn reprendre_la_sortie_locale_revenue(
        &self,
        zone_id: i64,
        vue: &SortieLocaleVue,
    ) -> bool {
        let enregistree = self.enregistrer_la_sortie_locale_revenue(vue).await;
        if enregistree {
            let repo = ZoneRepo::with_backend(self.db.clone());
            if let Err(e) = repo.update_online(zone_id, true) {
                warn!(zone_id, error = %e, "local_device_back_zone_online_failed");
            }
            if let Some(ref bus) = self.event_bus {
                bus.emit_typed(
                    crate::event_types::EventType::ZoneUpdated,
                    serde_json::json!({
                        "zone_id": zone_id,
                        "device_id": vue.device_id,
                        "online": true,
                    }),
                );
            }
            info!(
                zone_id,
                device = %vue.device_id,
                hote = %vue.hote,
                endpoint_id = %vue.endpoint_id,
                "local_device_back_registered_on_play"
            );
        }
        enregistree
    }

    /// La sortie `LocalOutput` de l'appareil retrouvé, née comme au rescan
    /// (`tune-server::background::appliquer_le_parc_local`) : hôte d'origine,
    /// endpoint, mode exclusif de SA zone selon la règle #3245/#5353, graine
    /// de volume (#8074).
    #[cfg(feature = "local-audio")]
    async fn enregistrer_la_sortie_locale_revenue(&self, vue: &SortieLocaleVue) -> bool {
        if self.outputs.lock().await.contains(&vue.device_id) {
            return true;
        }
        let Some(nom) = vue.device_id.strip_prefix("local:") else {
            return false;
        };
        let (_, backend_configure) = self.reglages_sortie_locale();
        let exclusif =
            self.exclusif_de_la_sortie_revenue(&vue.device_id, &backend_configure, &vue.hote);
        let sortie = crate::outputs::local::LocalOutput::with_options_and_endpoint(
            nom.to_string(),
            (!vue.endpoint_id.is_empty()).then(|| vue.endpoint_id.clone()),
            exclusif,
            &backend_configure,
        )
        .with_origin_host(&vue.hote);
        ensemencer_la_sortie_locale_neuve(&self.db, &vue.device_id, &sortie).await;
        let mut registre = self.outputs.lock().await;
        // Le rescan a pu l'enregistrer pendant l'énumération : on garde la sienne.
        if !registre.contains(&vue.device_id) {
            registre.register(Box::new(sortie));
        }
        true
    }

    /// Sans `local-audio`, aucune sortie locale ne peut naître : rien n'est
    /// enregistré, et le chemin d'avant (`recreate_local_and_play`, qui le dit)
    /// reste celui de la lecture.
    #[cfg(not(feature = "local-audio"))]
    async fn enregistrer_la_sortie_locale_revenue(&self, _vue: &SortieLocaleVue) -> bool {
        false
    }

    /// #8171 — le mode exclusif de la sortie retrouvée, par la même règle que
    /// le rescan : la demande de SA zone, sinon celle de la machine, et le
    /// `true` posé par le choix d'ASIO ne vaut pas pour un nom WASAPI (#5353).
    #[cfg(feature = "local-audio")]
    fn exclusif_de_la_sortie_revenue(
        &self,
        device_id: &str,
        backend_configure: &str,
        hote: &str,
    ) -> bool {
        let reglages = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone());
        let lire_vrai =
            |v: String| matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes");
        let demande_machine = reglages
            .get("local_exclusive_mode")
            .ok()
            .flatten()
            .map(lire_vrai)
            .unwrap_or_else(|| {
                std::env::var("TUNE_LOCAL_EXCLUSIVE_MODE")
                    .ok()
                    .is_some_and(lire_vrai)
            });
        let arme_par_asio = reglages
            .get(crate::config::REGLAGE_EXCLUSIF_ARME_PAR_ASIO)
            .ok()
            .flatten()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
        let valeur_de_zone = ZoneRepo::with_backend(self.db.clone())
            .get_by_device_id(device_id)
            .ok()
            .flatten()
            .and_then(|zone| zone.id)
            .and_then(|zone_id| {
                reglages
                    .get(&crate::config::cle_exclusif_de_zone(zone_id))
                    .ok()
                    .flatten()
            });
        exclusif_de_la_sortie_revenue_selon(
            backend_configure,
            hote,
            valeur_de_zone.as_deref(),
            demande_machine,
            arme_par_asio,
            cfg!(target_os = "windows"),
        )
    }
}

/// La règle, pure, du mode exclusif d'une sortie retrouvée à la demande
/// (#8171) : celle des deux sites d'enregistrement du serveur
/// (`local_exclusive_mode_du_peripherique`), plateforme en paramètre.
pub(crate) fn exclusif_de_la_sortie_revenue_selon(
    backend_configure: &str,
    hote: &str,
    valeur_de_zone: Option<&str>,
    demande_machine: bool,
    arme_par_asio: bool,
    sous_windows: bool,
) -> bool {
    let demande = crate::config::demande_d_exclusif_de_la_zone(valeur_de_zone, demande_machine);
    crate::config::exclusive_mode_du_peripherique(
        backend_configure,
        Some(hote),
        demande.demande,
        crate::config::temoin_asio_pour_la_demande(arme_par_asio, demande),
        sous_windows,
    )
    .effective
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
            vec![
                SortieLocaleVue::wasapi("local:Haut-parleurs Mac mini", ""),
                SortieLocaleVue::wasapi(DAC, ""),
            ]
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
        orch.enumerer_parc_local =
            Arc::new(|_| vec![SortieLocaleVue::wasapi("local:Haut-parleurs Mac mini", "")]);
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
            vec![SortieLocaleVue::wasapi(DAC, "")]
        });
        let debut = std::time::Instant::now();
        let revenu = orch
            .appareil_local_revenu(1, DAC, std::time::Duration::from_millis(100))
            .await;
        assert!(revenu.is_none(), "passé le délai, on ne conclut rien");
        assert!(debut.elapsed() < std::time::Duration::from_millis(500));
    }

    #[test]
    fn plan_de_reenumeration_ne_sonde_ni_le_reseau_ni_asio_ni_en_lecture_4598() {
        // Hors Windows : la règle de #4598, inchangée.
        for os in ["macos", "linux"] {
            assert_eq!(
                plan_de_reenumeration(DAC, "auto", false, os),
                Ok(EnumerationAvantRefus::Complete("auto".into()))
            );
            assert_eq!(
                plan_de_reenumeration("uuid:dlna-1", "auto", false, os),
                Err(PasDeReenumeration::PasUneSortieLocale)
            );
            assert_eq!(
                plan_de_reenumeration(DAC, " ASIO ", false, os),
                Err(PasDeReenumeration::BackendAsio)
            );
            assert_eq!(
                plan_de_reenumeration(DAC, "auto", true, os),
                Err(PasDeReenumeration::LectureLocaleEnCours)
            );
        }
        // Windows « auto » : la règle de #4598, inchangée elle aussi.
        assert_eq!(
            plan_de_reenumeration(DAC, "auto", false, "windows"),
            Ok(EnumerationAvantRefus::Complete("auto".into()))
        );
        assert_eq!(
            plan_de_reenumeration(DAC, "auto", true, "windows"),
            Err(PasDeReenumeration::LectureLocaleEnCours)
        );
    }

    /// Une zone locale dont une AUTRE sortie locale joue : pas d'énumération
    /// COMPLÈTE (elle pourrait couper ce qui joue), refus d'avant. Hors
    /// Windows : #8171 ne change rien ici.
    #[tokio::test]
    async fn pas_d_enumeration_pendant_qu_une_sortie_locale_joue_4598() {
        let mut orch = orchestrateur();
        orch.systeme_du_parc_local = "macos";
        let appels = Arc::new(AtomicUsize::new(0));
        let compteur = appels.clone();
        orch.enumerer_parc_local = Arc::new(move |_| {
            compteur.fetch_add(1, Ordering::SeqCst);
            vec![SortieLocaleVue::wasapi(DAC, "")]
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

    // ------------------------------------------------------------------
    // #8171 — fil 2708 : Windows 10, ASIO configuré, DAC Denafrips allumé
    // APRÈS le démarrage, pendant que les haut-parleurs du PC jouent.
    // ------------------------------------------------------------------

    const DENAFRIPS: &str = "local:DENAFRIPS USB DAC";
    const HP: &str = "local:Haut-parleurs";

    /// Le parc du testeur : les haut-parleurs du PC au registre (WASAPI), la
    /// zone du DAC hors ligne et absente du registre (DAC éteint au démarrage),
    /// ASIO choisi dans les réglages, la règle Windows.
    async fn parc_du_fil_2708(orch: &mut PlaybackOrchestrator) -> i64 {
        orch.systeme_du_parc_local = "windows";
        crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
            .set("local_audio_backend", "asio")
            .unwrap();
        orch.outputs.lock().await.register(Box::new(
            MockOutput::new(HP, "Haut-parleurs").with_type("local"),
        ));
        let repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = repo
            .create("DENAFRIPS USB DAC", Some("local"), Some(DENAFRIPS))
            .unwrap();
        repo.update_online(zone_id, false).unwrap();
        zone_id
    }

    async fn faire_jouer(orch: &PlaybackOrchestrator, device_id: &str) {
        let reg = orch.outputs.lock().await;
        let arc = reg.get(device_id).unwrap();
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .set_state(crate::outputs::traits::TransportState::Playing)
            .await;
    }

    /// LE témoin de #8171 : sous Windows avec ASIO configuré, et pendant que
    /// les haut-parleurs du PC jouent, la zone du DAC allumé après le
    /// démarrage n'est plus refusée : une énumération WASAPI SANS SONDE le
    /// retrouve.
    ///
    /// Rouge sans correctif : `local_reenumeration_before_reject_skipped`
    /// (`BackendAsio`), aucun appel à l'énumérateur, puis
    /// `zone_output_unavailable` — jusqu'au redémarrage du PC.
    #[tokio::test]
    async fn dac_allume_apres_le_demarrage_sous_asio_n_est_plus_refuse_8171() {
        let mut orch = orchestrateur();
        let demandes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let journal = demandes.clone();
        orch.enumerer_parc_local = Arc::new(move |demande: &EnumerationAvantRefus| {
            journal.lock().unwrap().push(demande.clone());
            vec![
                SortieLocaleVue::wasapi(HP, "{0.0.0.00000000}.{hp}"),
                SortieLocaleVue::wasapi(DENAFRIPS, "{0.0.0.00000000}.{dac}"),
            ]
        });
        let zone_id = parc_du_fil_2708(&mut orch).await;
        faire_jouer(&orch, HP).await;
        let zone = ZoneRepo::with_backend(orch.db.clone())
            .get(zone_id)
            .unwrap()
            .unwrap();

        let verdict = orch.gate_or_rebind_offline_zone(zone_id, &zone).await;

        assert_eq!(
            verdict,
            Ok(None),
            "#8171 : le DAC est allumé et WASAPI le voit ; le refuser oblige le \
             testeur à redémarrer son PC"
        );
        assert_eq!(
            *demandes.lock().unwrap(),
            vec![EnumerationAvantRefus::WasapiSansSonder],
            "une seule énumération, WASAPI et sans sonde : ni pilote ASIO ouvert, \
             ni format sondé pendant que les haut-parleurs jouent"
        );
    }

    /// CONTRE-ÉPREUVE : même parc, DAC toujours éteint ⇒ le refus d'avant,
    /// nommé. Rien n'est enregistré, la zone reste hors ligne, et aucune autre
    /// sortie (les haut-parleurs) ne prend sa place.
    #[tokio::test]
    async fn dac_toujours_eteint_sous_asio_reste_refuse_sans_jouer_ailleurs_8171() {
        let mut orch = orchestrateur();
        orch.enumerer_parc_local =
            Arc::new(|_| vec![SortieLocaleVue::wasapi(HP, "{0.0.0.00000000}.{hp}")]);
        let zone_id = parc_du_fil_2708(&mut orch).await;
        faire_jouer(&orch, HP).await;
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
        assert!(!orch.outputs.lock().await.contains(DENAFRIPS));
        let zone = ZoneRepo::with_backend(orch.db.clone())
            .get(zone_id)
            .unwrap()
            .unwrap();
        assert!(!zone.online, "la zone d'un DAC éteint reste hors ligne");
        assert_eq!(
            zone.output_device_id.as_deref(),
            Some(DENAFRIPS),
            "la zone garde SON appareil : aucun rebond vers les haut-parleurs"
        );
    }

    /// Le rang `(n)` ne glisse pas : la zone « Haut-parleurs (2) » (un second
    /// homonyme, souvent le DAC sous le pilote générique de Windows) n'est pas
    /// déclarée revenue parce qu'un seul « Haut-parleurs » est présent.
    #[tokio::test]
    async fn un_homonyme_absent_n_est_pas_retrouve_sur_le_premier_8171() {
        let mut orch = orchestrateur();
        orch.enumerer_parc_local =
            Arc::new(|_| vec![SortieLocaleVue::wasapi(HP, "{0.0.0.00000000}.{hp}")]);
        parc_du_fil_2708(&mut orch).await;
        let vue = orch
            .appareil_local_revenu(
                1,
                "local:Haut-parleurs (2)",
                DELAI_REENUMERATION_AVANT_REFUS,
            )
            .await;
        assert_eq!(vue, None);
    }

    /// La décision, pure, sous Windows : ASIO et WASAPI passent par la liste
    /// WASAPI sans sonde, lecture en cours ou non. Une zone réseau, jamais.
    #[test]
    fn plan_sous_windows_enumere_wasapi_sans_sonder_meme_en_lecture_8171() {
        for backend in ["asio", " ASIO ", "wasapi", "WASAPI"] {
            for lecture in [false, true] {
                assert_eq!(
                    plan_de_reenumeration(DENAFRIPS, backend, lecture, "windows"),
                    Ok(EnumerationAvantRefus::WasapiSansSonder),
                    "{backend:?} lecture={lecture}"
                );
            }
        }
        assert_eq!(
            plan_de_reenumeration("uuid:dlna-1", "asio", false, "windows"),
            Err(PasDeReenumeration::PasUneSortieLocale)
        );
    }

    /// Le mode exclusif de la sortie retrouvée suit la règle du rescan : sous
    /// ASIO, le `true` posé par le CHOIX d'ASIO ne vaut pas pour un nom WASAPI
    /// (#5353) ; une demande de l'utilisateur, de machine ou de zone, oui.
    #[test]
    fn exclusif_de_la_sortie_revenue_suit_la_regle_du_rescan_8171() {
        // Armé par ASIO seulement : partagé.
        assert!(!exclusif_de_la_sortie_revenue_selon(
            "asio", "WASAPI", None, true, true, true
        ));
        // Demandé par l'utilisateur (sans témoin ASIO) : exclusif WASAPI.
        assert!(exclusif_de_la_sortie_revenue_selon(
            "asio", "WASAPI", None, true, false, true
        ));
        // La zone l'emporte sur la machine, dans les deux sens.
        assert!(!exclusif_de_la_sortie_revenue_selon(
            "asio",
            "WASAPI",
            Some("false"),
            true,
            false,
            true
        ));
        assert!(exclusif_de_la_sortie_revenue_selon(
            "asio",
            "WASAPI",
            Some("true"),
            false,
            true,
            true
        ));
        // Une sortie ASIO reste exclusive par nature.
        assert!(exclusif_de_la_sortie_revenue_selon(
            "asio", "ASIO", None, false, false, true
        ));
    }

    /// GARDE DE SITE — la sortie retrouvée naît avec l'hôte qui l'a énumérée
    /// et son endpoint : c'est ce qui l'ouvre en WASAPI quand ASIO est
    /// configuré. Lue dans le texte, la branche `local-audio` n'étant pas
    /// compilée par la porte `test` de la CI.
    #[test]
    fn la_sortie_revenue_nait_avec_son_hote_et_son_endpoint_8171() {
        let source = include_str!("reenumeration_avant_refus.rs");
        let debut = source
            .find("async fn enregistrer_la_sortie_locale_revenue(&self, vue: &SortieLocaleVue) -> bool {\n        if")
            .expect("la branche local-audio de l'enregistrement");
        let reste = &source[debut..];
        let corps = &reste[..reste.find("\n    }\n").expect("fin du corps")];
        assert!(corps.contains(".with_origin_host(&vue.hote)"), "{corps}");
        assert!(corps.contains("vue.endpoint_id.clone()"), "{corps}");
        assert!(
            corps.contains("ensemencer_la_sortie_locale_neuve("),
            "{corps}"
        );
        let transport = include_str!("transport.rs");
        assert!(
            transport.contains(".reprendre_la_sortie_locale_revenue(zone_id, &vue)"),
            "le garde-fou doit enregistrer la sortie retrouvée avant de laisser jouer"
        );
    }
}
