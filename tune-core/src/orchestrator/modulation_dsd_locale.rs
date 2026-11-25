//! rc14 — le suréchantillonnage vers le DSD de TESS, branché sur la sortie
//! LOCALE (voir `tess::modulation_dsd` pour l'architecture et les règles).
//!
//! - [`PlaybackOrchestrator::capacites_dsd_de_zone`] : ce que la sortie fait
//!   de chaque cible, SANS sonder le pilote (capacités déjà connues), pour
//!   `GET/PUT /zones/{id}/tess` ;
//! - [`PlaybackOrchestrator::decider_la_modulation_dsd`] : à la lecture d'une
//!   piste PCM de la bibliothèque, après le DoP anticipé des sources DSD ;
//!   sert le flux modulé, ou laisse le chemin PCM d'avant (repli dit au
//!   chemin du signal) ;
//! - [`PlaybackOrchestrator::attenuation_logicielle_en_dsd`] : la protection
//!   du niveau DSD (#8813) appliquée au flux modulé.

use std::sync::Arc;

use tracing::{info, warn};

use super::*;
use crate::tess::chaine::{ID_UPSAMPLING, Upsampling};
use crate::tess::modulation_dsd::{
    self as modulation, CapacitesDsd, EtatModulation, FaitsSortieDsd, ModulationEnService,
    MotifDsdRefuse, PlanModulation, ProducteurDsd, TransportModulation,
};
use crate::tess::resolution::{
    CodeEtape, ContexteExecution, ControleVolume, SourceFlux, VolumeZone,
};
use crate::tess::zones::TransportZone;

/// Ce que l'orchestrateur sait de la sortie d'une zone, pour la décision.
struct FaitsDeZone {
    sortie_locale: bool,
    dsd_mode: String,
    plafond_zone_hz: Option<u32>,
    natif: SortieDsdLocale,
    /// Nom du périphérique local (réponses DoP retenues), s'il y en a un.
    peripherique: Option<String>,
}

impl FaitsDeZone {
    fn natif(&self, cadence: u32, voies: u16) -> Result<(), &'static str> {
        decider_dsd_natif_local_voies(
            &self.natif,
            Some(cadence),
            Some(u32::from(voies)),
            self.plafond_zone_hz,
            false,
        )
        .map_err(MotifDsdServiEnDop::as_str)
    }

    fn dop_refuse_connu(&self, cadence_dop: u32, voies: u16) -> bool {
        self.peripherique.as_deref().is_some_and(|nom| {
            matches!(
                crate::audio::dop_ecarte_7265::accueil_exclusif_connu(nom, cadence_dop, voies),
                Some(crate::audio::dop_ecarte_7265::AccueilDuDop::ExclusifRefuse { .. })
            )
        })
    }
}

/// La cadence (s) et la durée (s) à décoder pour une piste, tranche CUE et
/// déplacement compris, et la durée servie (ms).
fn fenetre_de_decodage(
    tranche: Option<&super::resolve_local::TrancheCue>,
    seek_ms: Option<u64>,
    duree_piste_ms: u64,
) -> (f64, Option<f64>, u64) {
    let seek = seek_ms.unwrap_or(0);
    match tranche {
        Some(t) => {
            let duree = t.duree_ms.map(|d| d.saturating_sub(seek));
            (
                (t.debut_ms + seek) as f64 / 1000.0,
                duree.map(|d| d as f64 / 1000.0),
                duree.unwrap_or_else(|| duree_piste_ms.saturating_sub(seek)),
            )
        }
        None => (
            seek as f64 / 1000.0,
            None,
            duree_piste_ms.saturating_sub(seek),
        ),
    }
}

/// Moyenne des deux voies (repli mono de la zone), en `f64`.
fn replier_en_mono(x: &mut [f64]) {
    for t in x.as_chunks_mut::<2>().0 {
        let m = (t[0] + t[1]) * 0.5;
        t[0] = m;
        t[1] = m;
    }
}

/// Le fil du producteur : décodage en entiers 32 bits (exacts pour 16 et
/// 24 bits), greffons de la zone, `f64`, modulateur, porteur. Rend le nombre
/// de remises à zéro de la boucle et l'entrée maximale du quantificateur.
#[allow(clippy::too_many_arguments)]
fn produire_le_flux_dsd(
    fichier: String,
    seek_s: f64,
    duree_s: Option<f64>,
    duree_servie_ms: u64,
    plan: PlanModulation,
    mut producteur: ProducteurDsd,
    mut dsp: StreamingDsp,
    mono: bool,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    data_ready: Arc<tokio::sync::Notify>,
    rt: tokio::runtime::Handle,
) -> Result<(u64, f64), String> {
    let delai = crate::http::streamer::SESSION_IDLE_TIMEOUT;
    let envoyer = |bloc: Vec<u8>| -> Result<(), String> {
        match rt.block_on(tokio::time::timeout(delai, tx.send(bloc))) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err("session fermée".into()),
            Err(_) => Err("délai d'envoi dépassé".into()),
        }
    };
    envoyer(producteur.entete(Some(duree_servie_ms)))?;
    data_ready.notify_one();

    let voies = usize::from(plan.voies);
    let (tx_pcm, mut rx_pcm) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (niveaux_tx, _niveaux_rx) = tokio::sync::mpsc::unbounded_channel();
    let canaux = u32::from(plan.voies);
    let cadence = plan.cadence_pcm;
    let decodeur = rt.spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming_tranche(
            &fichier,
            Some(cadence),
            Some(canaux),
            Some(32),
            tx_pcm,
            65_536,
            Arc::new(tokio::sync::Notify::new()),
            niveaux_tx,
            seek_s,
            duree_s,
        )
    });

    let trame = 4 * voies;
    let mut reste: Vec<u8> = Vec::new();
    let mut entete_lue = false;
    let mut f64s: Vec<f64> = Vec::new();
    let mut sortie: Vec<u8> = Vec::new();
    let mut traiter = |octets: &mut Vec<u8>,
                       producteur: &mut ProducteurDsd,
                       sortie: &mut Vec<u8>|
     -> Result<(), String> {
        f64s.clear();
        f64s.extend(
            octets
                .as_chunks::<4>()
                .0
                .iter()
                .map(|s| f64::from(i32::from_le_bytes(*s)) / 2_147_483_648.0),
        );
        if mono && voies == 2 {
            replier_en_mono(&mut f64s);
        }
        producteur
            .moduler(&f64s, sortie)
            .map_err(|e| format!("modulateur : {e}"))
    };
    while let Some(bloc) = rx_pcm.blocking_recv() {
        reste.extend_from_slice(&bloc);
        if !entete_lue {
            if reste.len() < 44 {
                continue;
            }
            if &reste[..4] == b"RIFF" {
                reste.drain(..44);
            }
            entete_lue = true;
        }
        let utiles = reste.len() / trame * trame;
        if utiles == 0 {
            continue;
        }
        let mut pcm: Vec<u8> = reste.drain(..utiles).collect();
        dsp.process_flux(&mut pcm, 32);
        sortie.clear();
        traiter(&mut pcm, &mut producteur, &mut sortie)?;
        if !sortie.is_empty() {
            envoyer(std::mem::take(&mut sortie))?;
        }
    }
    // Fin de piste : la queue du convolveur, puis le retard du modulateur.
    let mut queue = dsp.terminer_flux(32);
    sortie.clear();
    if !queue.is_empty() {
        traiter(&mut queue, &mut producteur, &mut sortie)?;
    }
    producteur
        .terminer(&mut sortie)
        .map_err(|e| format!("modulateur : {e}"))?;
    if !sortie.is_empty() {
        envoyer(sortie)?;
    }
    match rt.block_on(decodeur) {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(format!("décodage : {e}")),
        Err(e) => return Err(format!("décodage interrompu : {e}")),
    }
    Ok((producteur.reinitialisations(), producteur.entree_max()))
}

impl PlaybackOrchestrator {
    /// Le réglage DSD, le plafond et la capacité native de la zone. `sonder` :
    /// la capacité native peut être sondée (lecture d'une piste, comme le
    /// DSD natif d'un fichier) ; sinon seule la capacité déjà connue compte
    /// (route de l'API : on ne touche jamais au pilote depuis un écran).
    async fn faits_dsd_de_zone(&self, zone_id: i64, sonder: bool) -> FaitsDeZone {
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten();
        let device_id = zone.as_ref().and_then(|z| z.output_device_id.clone());
        let sortie_locale = device_id
            .as_deref()
            .is_some_and(|d| d.starts_with("local:"));
        let dsd_mode = ZoneRepo::with_backend(self.db.clone()).get_dsd_mode(zone_id);
        let quirks = crate::device_catalog::resolve_zone_quirks(&self.db, zone_id);
        let plafond_zone_hz = crate::device_catalog::combine_max_sample_rate(
            zone.as_ref().and_then(|z| z.max_sample_rate),
            quirks.max_sample_rate,
        );
        let mut faits = FaitsDeZone {
            sortie_locale,
            dsd_mode,
            plafond_zone_hz,
            natif: SortieDsdLocale::NonAsio,
            peripherique: None,
        };
        if !sortie_locale {
            return faits;
        }
        faits.peripherique = self.nom_du_peripherique_local(device_id.as_deref()).await;
        if faits.dsd_mode == "native" {
            faits.natif = if sonder {
                self.sortie_dsd_locale(device_id.as_deref()).await
            } else {
                self.sortie_dsd_locale_connue(faits.peripherique.as_deref())
            };
        }
        faits
    }

    async fn nom_du_peripherique_local(&self, device_id: Option<&str>) -> Option<String> {
        #[cfg(feature = "local-audio")]
        {
            let sortie = { self.outputs.lock().await.get(device_id?) }?;
            let sortie = sortie.lock().await;
            sortie
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
                .map(|l| l.nom_du_peripherique().to_string())
        }
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = device_id;
            None
        }
    }

    /// La capacité native DÉJÀ connue (aucune sonde).
    fn sortie_dsd_locale_connue(&self, nom: Option<&str>) -> SortieDsdLocale {
        use crate::outputs::capacite_dsd_natif::{CapaciteConnue, connue};
        match nom.and_then(connue) {
            Some(CapaciteConnue::Asio { cadences }) => SortieDsdLocale::Asio { cadences },
            #[cfg(feature = "local-audio")]
            Some(CapaciteConnue::AlsaDsdU32 { cadences }) => SortieDsdLocale::Alsa {
                cadences,
                voies: crate::outputs::local::VOIES_SONDEES_DSD_ALSA,
            },
            _ => SortieDsdLocale::NonAsio,
        }
    }

    fn premium_instantane(&self) -> bool {
        self.license
            .as_ref()
            .is_none_or(|license| license.premium_snapshot())
    }

    /// rc14 — les capacités DSD de la zone (`dsd_capabilities`), sans sonder
    /// le pilote. La machine est mesurée une fois (≈ 0,1 à 1 s), hors du fil
    /// asynchrone.
    pub async fn capacites_dsd_de_zone(&self, zone_id: i64) -> CapacitesDsd {
        let faits = self.faits_dsd_de_zone(zone_id, false).await;
        let temps_reel = match modulation::mesure_de_la_machine() {
            Some(m) => Some(m),
            None if faits.sortie_locale => {
                tokio::task::spawn_blocking(modulation::mesurer_la_machine)
                    .await
                    .ok()
            }
            None => None,
        };
        let natif = |c: u32, v: u16| faits.natif(c, v);
        let dop_refuse = |c: u32, v: u16| faits.dop_refuse_connu(c, v);
        let pures = FaitsSortieDsd {
            sortie_locale: faits.sortie_locale,
            dsd_mode: &faits.dsd_mode,
            plafond_zone_hz: faits.plafond_zone_hz,
            natif: &natif,
            dop_refuse: &dop_refuse,
            temps_reel,
        };
        modulation::capacites(
            &pures,
            crate::audio::dop_ecarte_7265::protection_du_niveau_active(&self.db, zone_id),
        )
    }

    /// #8813 appliqué au flux modulé : la zone demande-t-elle une atténuation
    /// logicielle que le DSD perdrait (volume sous 100 %, sourdine, trim
    /// négatif) ? Le ReplayGain et le préampli de l'égaliseur sont, eux,
    /// appliqués dans le flux avant le modulateur. Protection désactivée par
    /// la zone : jamais.
    pub(super) async fn attenuation_logicielle_en_dsd(&self, zone_id: i64) -> bool {
        if !crate::audio::dop_ecarte_7265::protection_du_niveau_active(&self.db, zone_id) {
            return false;
        }
        let etat = self.playback.get_state(zone_id).await;
        let volume_fixe = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .is_some_and(|z| z.fixed_volume);
        etat.muted
            || (!volume_fixe
                && (etat.volume < 1.0 - 1e-6
                    || super::regles::gain_trim_db_enregistre(&self.db, zone_id) < -1e-6))
    }

    /// La piste qui joue sur la zone est-elle un flux modulé vers le DSD ?
    pub(super) async fn modulation_dsd_en_service(&self, zone_id: i64) -> bool {
        let etat = self.playback.get_state(zone_id).await;
        let piste = etat.now_playing.as_ref().and_then(|np| np.track_id);
        matches!(
            modulation::etat_pour(zone_id, piste),
            Some(EtatModulation::EnService(_))
        )
    }

    /// rc14 — une piste PCM de la bibliothèque, sortie locale, chaîne de la
    /// zone vers le DSD : le flux modulé, ou `None` (le chemin PCM d'avant
    /// continue ; le motif du repli est retenu pour le chemin du signal).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn decider_la_modulation_dsd(
        &self,
        req: &PlayRequest,
        track: &crate::db::models::Track,
        file_path: &str,
        source_pcm: bool,
        is_local_output: bool,
        cadence_pcm: u32,
        voies: u16,
        tranche: Option<&super::resolve_local::TrancheCue>,
    ) -> Result<Option<ResolvedStream>, String> {
        let zone_id = req.zone_id;
        let piste = req.track_id;
        if !is_local_output || !source_pcm {
            modulation::oublier(zone_id, piste);
            return Ok(None);
        }
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone());
        // Garde bon marché : une chaîne vers le DSD n'existe que dans la clé
        // de la chaîne (les anciennes clés ne savent pas dire « dsd »). Sans
        // elle, rien n'est lu de plus que sur le chemin d'avant.
        let peut_etre_dsd = settings
            .get(&crate::tess::cle_chaine_zone(zone_id))
            .ok()
            .flatten()
            .is_some_and(|brut| brut.contains("\"dsd\""));
        if !peut_etre_dsd {
            modulation::oublier(zone_id, piste);
            return Ok(None);
        }
        let premium = self.premium_instantane();
        let Ok(lecture) = crate::tess::migration::lire_zone(&settings, zone_id, premium) else {
            modulation::oublier(zone_id, piste);
            return Ok(None);
        };
        let Upsampling::Dsd { rate, .. } = lecture.chaine.fixed.upsampling.clone() else {
            modulation::oublier(zone_id, piste);
            return Ok(None);
        };
        // La résolution de TESS décide si l'étage est effectif (Premium,
        // moteur installé, PURE, « Bypass all ») ; sinon rien ne change. Les
        // fermetures (non `Send`) vivent dans ce bloc, hors des `await`.
        let effective = {
            let disponible = |id: &str| {
                id != ID_UPSAMPLING
                    || crate::audio::premium_plugins::enabled(
                        &settings,
                        crate::audio::premium_plugins::UPSAMPLER,
                    )
            };
            let ctx = ContexteExecution {
                transport: TransportZone::Local,
                source: Some(SourceFlux::Pcm {
                    sample_rate_hz: cadence_pcm,
                    bits: 24,
                    channels: voies,
                }),
                premium,
                greffon_disponible: &disponible,
                pure: lecture.pure,
                volume: VolumeZone {
                    control: ControleVolume::Software,
                    db: 0.0,
                },
                replaygain_actif: false,
            };
            lecture
                .chaine
                .resoudre(&crate::tess::catalogue_de_reference(), &ctx)
                .etapes
                .iter()
                .any(|e| e.code == CodeEtape::Upsampling && e.effective)
        };
        if !effective {
            modulation::oublier(zone_id, piste);
            return Ok(None);
        }
        let repli = |motif: MotifDsdRefuse| {
            warn!(
                zone_id,
                track_id = ?piste,
                rate = ?rate,
                cadence_pcm,
                voies,
                motif = motif.as_str(),
                "tess_dsd_repli_en_pcm"
            );
            modulation::noter(zone_id, piste, EtatModulation::Repli(motif));
            Ok(None)
        };
        if self.attenuation_logicielle_en_dsd(zone_id).await {
            return repli(MotifDsdRefuse::SoftwareVolumeActive);
        }
        let faits = self.faits_dsd_de_zone(zone_id, true).await;
        let temps_reel = tokio::task::spawn_blocking(modulation::mesurer_la_machine)
            .await
            .ok();
        let plan = {
            let natif = |c: u32, v: u16| faits.natif(c, v);
            let dop_refuse = |c: u32, v: u16| faits.dop_refuse_connu(c, v);
            let pures = FaitsSortieDsd {
                sortie_locale: faits.sortie_locale,
                dsd_mode: &faits.dsd_mode,
                plafond_zone_hz: faits.plafond_zone_hz,
                natif: &natif,
                dop_refuse: &dop_refuse,
                temps_reel,
            };
            modulation::planifier(&pures, rate, cadence_pcm, voies)
        };
        let plan = match plan {
            Ok(p) => p,
            Err(motif) => return repli(motif),
        };
        // #7265 — la sortie prend-elle CE porteur DoP (WASAPI) ? Une réponse
        // « non » se retient et se dit ; « je ne sais pas » ne change rien.
        if plan.transport == TransportModulation::Dop {
            let cadence_dop = plan.cadence_du_porteur();
            let accueil = self
                .accueil_du_dop_local(req.output_device_id.as_deref(), cadence_dop, voies)
                .await;
            if crate::audio::dop_ecarte_7265::decider(&accueil, cadence_dop, voies).is_some() {
                return repli(MotifDsdRefuse::DopRefusedByOutput);
            }
        }
        match self
            .servir_la_modulation_dsd(req, track, file_path, plan, tranche)
            .await
        {
            Ok(resolu) => Ok(Some(resolu)),
            Err(m) => repli(m),
        }
    }

    /// Ouvre la session et lance le producteur du flux modulé.
    async fn servir_la_modulation_dsd(
        &self,
        req: &PlayRequest,
        track: &crate::db::models::Track,
        file_path: &str,
        plan: PlanModulation,
        tranche: Option<&super::resolve_local::TrancheCue>,
    ) -> Result<ResolvedStream, MotifDsdRefuse> {
        let zone_id = req.zone_id;
        // Conception du NTF et des filtres : hors du fil asynchrone.
        let producteur = tokio::task::spawn_blocking(move || ProducteurDsd::new(plan))
            .await
            .map_err(|_| MotifDsdRefuse::ModulatorFailed)?
            .map_err(|e| {
                warn!(zone_id, error = %e, "tess_dsd_modulateur_non_construit");
                MotifDsdRefuse::ModulatorFailed
            })?;
        let (seek_s, duree_s, duree_servie_ms) =
            fenetre_de_decodage(tranche, req.seek_ms, track.duration_ms.max(0) as u64);
        let (format, mime, ext, profondeur) = match plan.transport {
            TransportModulation::Dop => ("wav", "audio/wav", "wav", 24u16),
            TransportModulation::Native => ("dsd", crate::audio::dsd_brut::MIME, "dsd", 1),
        };
        let info = StreamInfo {
            format: format.into(),
            mime_type: mime.into(),
            sample_rate: plan.cadence_du_porteur(),
            bit_depth: profondeur,
            channels: plan.voies,
            file_size: None,
            duration_ms: Some(duree_servie_ms),
            ..Default::default()
        };
        let (session_id, tx, data_ready) = self.streamer.create_session(info, true, 128).await;
        if plan.transport == TransportModulation::Dop {
            // L'en-tête WAV part DANS le canal (même règle que le DoP d'un
            // fichier, #1894) : sinon la session en préfixe un second et le
            // marqueur ne tombe plus sur l'octet de poids fort.
            let sessions = self.streamer.sessions_state();
            let sessions = sessions.lock().await;
            if let Some(session) = sessions.get(&session_id) {
                session
                    .wav_header_included
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        // Les greffons de la zone (égaliseur, convolution, crossfeed et
        // ReplayGain) passent DANS le flux : la sortie ne touche pas un
        // porteur DSD.
        let dsp = self.load_streaming_dsp(zone_id, req.piste_rg(), plan.cadence_pcm, plan.voies);
        let greffons_dans_le_flux = dsp.is_active();
        let mono = self.zone_mono_downmix(zone_id);
        info!(
            zone_id,
            file = %file_path,
            cadence_pcm = plan.cadence_pcm,
            cadence_dsd = plan.cadence_dsd,
            porteur = plan.cadence_du_porteur(),
            voies = plan.voies,
            transport = ?plan.transport,
            greffons_dans_le_flux,
            mono,
            "tess_dsd_modulation_lancee"
        );
        let fichier = file_path.to_string();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            match produire_le_flux_dsd(
                fichier,
                seek_s,
                duree_s,
                duree_servie_ms,
                plan,
                producteur,
                dsp,
                mono,
                tx,
                data_ready,
                rt,
            ) {
                Ok((reinitialisations, entree_max)) => {
                    if reinitialisations > 0 {
                        warn!(
                            zone_id,
                            reinitialisations, entree_max, "tess_dsd_modulateur_instable"
                        );
                    } else {
                        tracing::debug!(zone_id, entree_max, "tess_dsd_modulation_terminee");
                    }
                }
                Err(e) => warn!(zone_id, error = %e, "tess_dsd_modulation_interrompue"),
            }
        });
        if let Some(bus) = self
            .event_bus
            .clone()
            .filter(|_| self.levels_attach_allowed(zone_id))
        {
            let play_seq = self.playback.current_play_seq(zone_id).await;
            spawn_local_file_levels_decode_depuis(
                bus,
                self.playback.clone(),
                zone_id,
                play_seq,
                file_path.to_string(),
                tranche.map_or(0, |t| t.debut_ms),
                req.seek_ms.unwrap_or(0),
            );
        }
        modulation::noter(
            zone_id,
            req.track_id,
            EtatModulation::EnService(ModulationEnService {
                rate: plan.taux,
                rate_hz: plan.cadence_dsd,
                transport: plan.transport,
                order: modulation::ORDRE_DU_MODULATEUR,
                carrier_rate_hz: plan.cadence_du_porteur(),
                source_rate_hz: plan.cadence_pcm,
                channels: plan.voies,
                plugins_in_stream: greffons_dans_le_flux || mono,
            }),
        );
        let server_ip = self.server_ip();
        Ok(ResolvedStream {
            url: self.streamer.get_stream_url(&session_id, &server_ip, ext),
            stream_id: Some(session_id),
            title: track.title.clone(),
            artist: track.artist_name.clone(),
            album: track.album_title.clone(),
            duration_ms: Some(track.duration_ms),
            source: "local".into(),
            mime_type: mime.into(),
            sample_rate: Some(plan.cadence_du_porteur()),
            bit_depth: Some(u32::from(profondeur)),
            channels: Some(u32::from(plan.voies)),
            origin_url: None,
            bitrate_kbps: None,
            format_source: None,
            cover_url: self.resolve_cover_url(track.cover_path.as_deref()),
            file_size: None,
        })
    }
}

#[cfg(test)]
#[path = "modulation_dsd_locale_tests.rs"]
mod tests;
