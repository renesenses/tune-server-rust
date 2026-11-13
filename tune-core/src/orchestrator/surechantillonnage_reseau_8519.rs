//! Discussion #8519 — la décision du suréchantillonneur pour une piste de la
//! bibliothèque servie à une zone RÉSEAU (DLNA, OpenHome). La partie pure vit
//! dans [`crate::audio::surechantillonnage_reseau`] ; ici, ce qui lit la zone,
//! la licence et le `Sink` du renderer.
use super::*;
use crate::audio::surechantillonnage_reseau::{self as sr, CibleReseau, RefusReseau};

impl PlaybackOrchestrator {
    /// Le `Sink` (`GetProtocolInfo`) du renderer DLNA ou OpenHome
    /// `device_id`. `None` : sortie absente du registre, d'un autre type, ou
    /// sonde inconcluante — « je ne sais pas », jamais « rien ».
    pub(super) async fn sink_du_renderer(&self, device_id: &str) -> Option<Vec<String>> {
        if device_id.is_empty() {
            return None;
        }
        let arc = { self.outputs.lock().await.get(device_id) }?;
        let caps = {
            let locked = arc.lock().await;
            let sortie = locked.as_any();
            if let Some(dlna) = sortie.downcast_ref::<crate::outputs::dlna::DlnaOutput>() {
                dlna.probe_capabilities().await
            } else if let Some(oh) =
                sortie.downcast_ref::<crate::outputs::openhome::OpenHomeOutput>()
            {
                oh.probe_capabilities().await
            } else {
                return None;
            }
        };
        caps.probed.then_some(caps.sink)
    }

    /// Le suréchantillonneur entre-t-il dans le flux de cette piste ?
    ///
    /// - `None` : rien à dire — sortie non réseau, ou greffon pas choisi
    ///   pour la zone (désactivé, licence, PURE, test de la machine, cadence
    ///   fixe : le chemin du signal les dit déjà depuis les réglages) ;
    /// - `Some(Err(refus))` : réglé et actif, mais pas pour CE flux, avec la
    ///   raison ;
    /// - `Some(Ok(cible))` : la piste sera décodée à `cible.de` et servie à
    ///   `cible.vers` par le bras fichier.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn decider_le_surechantillonnage_reseau(
        &self,
        req: &PlayRequest,
        source_format: Option<AudioFormat>,
        source: &SourceEtZone,
        sorties: &Sorties,
        forcages: &ForcagesReseau,
        transcodage: &TranscodagePourLaSortie,
        traitement: &Traitement,
        duree_ms: i64,
    ) -> Option<Result<CibleReseau, RefusReseau>> {
        if !sorties.is_network_output {
            return None;
        }
        let pure = self.zone_audiophile(req.zone_id);
        let choix = self
            .decision_du_surechantillonneur(req.zone_id, pure)
            .ok()?;
        let decision = self
            .cible_du_surechantillonnage_reseau(
                req,
                &choix,
                source_format,
                source,
                forcages,
                transcodage,
                traitement,
                duree_ms,
            )
            .await;
        match &decision {
            Ok(c) => info!(
                zone_id = req.zone_id,
                from_sr = c.de,
                to_sr = c.vers,
                filter = %c.reglages.filter,
                "dlna_upsampler_target"
            ),
            Err(r) => info!(
                zone_id = req.zone_id,
                reason = r.code(),
                source_sr = source.sample_rate,
                "dlna_upsampler_not_applied"
            ),
        }
        Some(decision)
    }

    #[allow(clippy::too_many_arguments)]
    async fn cible_du_surechantillonnage_reseau(
        &self,
        req: &PlayRequest,
        choix: &crate::audio::surechantillonnage::ChoixSurechantillonnage,
        source_format: Option<AudioFormat>,
        source: &SourceEtZone,
        forcages: &ForcagesReseau,
        transcodage: &TranscodagePourLaSortie,
        traitement: &Traitement,
        duree_ms: i64,
    ) -> Result<CibleReseau, RefusReseau> {
        if !source
            .zone_output_type
            .as_deref()
            .is_some_and(sr::sortie_prise_en_charge)
        {
            return Err(RefusReseau::SortieNonPriseEnCharge);
        }
        if forcages.dsd_passthrough
            || matches!(source_format, None | Some(AudioFormat::Dsd))
            || source.sample_rate == 0
        {
            return Err(RefusReseau::SourceNonPcm);
        }
        if crate::audio::bitperfect_strict::zone_enabled(&self.db, req.zone_id) {
            return Err(RefusReseau::BitPerfectStrict);
        }
        if traitement.dsp_progressif_wav || traitement.flux_wav_reseau {
            return Err(RefusReseau::FluxProgressif);
        }
        // Le format qui partira sur le fil : WAV pour un renderer sans FLAC,
        // FLAC sinon (la cible que le bras fichier écrit).
        let mime = if transcodage.dlna_needs_wav {
            "audio/wav"
        } else {
            "audio/flac"
        };
        let did = identifiant_du_renderer(
            req.output_device_id.as_deref(),
            source
                .zone
                .as_ref()
                .and_then(|z| z.output_device_id.as_deref()),
        );
        let sink = self.sink_du_renderer(did).await.unwrap_or_default();
        let mut declarees = sr::cadences_declarees(&sink, mime);
        // Le format servi ne dit rien, la zone non plus : ce que le Sink
        // déclare pour les autres formats PCM est ce que son convertisseur
        // accepte.
        if declarees.is_empty() && source.zone_max_sample_rate.is_none() {
            declarees = sr::cadences_declarees_pcm(&sink);
        }
        let acceptees = sr::cadences_acceptees(&declarees, source.zone_max_sample_rate)?;
        let vers = sr::negocier(choix, source.sample_rate, &acceptees)?;
        // La piste entière passe en mémoire à la cadence CIBLE dans le bras
        // fichier (#8558) ; un WAV ne dépasse pas l'en-tête RIFF (#4016).
        let canaux = transcodage.canaux_reduits.unwrap_or(source.channels).max(1);
        if crate::orchestrator::bascule_progressive_8558::basculer_en_progressif(
            true,
            AudioFormat::Flac,
            duree_ms,
            vers,
            canaux,
            crate::orchestrator::bascule_progressive_8558::memoire_disponible(),
        ) {
            return Err(RefusReseau::PisteTropLourde);
        }
        let profondeur = if transcodage.dlna_needs_wav && !forcages.dlna_wav24 {
            16
        } else {
            24
        };
        if transcodage.dlna_needs_wav
            && crate::audio::wav::pcm_depasse_le_plafond_riff(
                duree_ms.max(0) as u64,
                vers,
                canaux,
                profondeur,
            )
        {
            return Err(RefusReseau::PisteTropLourde);
        }
        Ok(CibleReseau {
            de: source.sample_rate,
            vers,
            reglages: choix.reglages.clone(),
        })
    }
}
