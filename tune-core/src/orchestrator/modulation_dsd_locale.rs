//! rc14 — le suréchantillonnage vers le DSD de TESS, branché sur la sortie
//! LOCALE (voir `tess::modulation_dsd` pour l'architecture et les règles).
//!
//! - [`PlaybackOrchestrator::capacites_dsd_de_zone`] : ce que la sortie fait
//!   de chaque cible, SANS sonder le pilote ni attendre une sortie occupée ni
//!   mesurer la machine (mesure en tâche de fond), pour `GET/PUT
//!   /zones/{id}/tess` ;
//! - [`PlaybackOrchestrator::decider_la_modulation_dsd`] : à la lecture d'une
//!   piste PCM de la bibliothèque, après le DoP anticipé des sources DSD ;
//!   sert le flux modulé, ou laisse le chemin PCM d'avant (repli dit au
//!   chemin du signal) ;
//! - [`PlaybackOrchestrator::relancer_apres_rupture_dsd`] : la sortie n'a pas
//!   pu porter le DoP (rééchantillonnage) : la piste est relancée à la même
//!   position, et repart en PCM ;
//! - [`PlaybackOrchestrator::relancer_si_flux_dsd_module`] : un réglage de
//!   la chaîne (égaliseur, convolution…) change pendant la modulation : la
//!   piste est relancée à la même position avec le nouveau réglage.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tracing::{info, warn};

use super::*;
use crate::tess::chaine::{ID_UPSAMPLING, Upsampling};
use crate::tess::modulation_dsd::{
    self as modulation, BilanDuFlux, CapacitesDsd, CleFlux, EtatModulation, FaitsSortieDsd,
    ModulationEnService, MotifDsdRefuse, PlanModulation, ProducteurDsd, TransportModulation,
};
use crate::tess::resolution::{
    CodeEtape, ContexteExecution, ControleVolume, SourceFlux, VolumeZone,
};
use crate::tess::zones::TransportZone;

/// Tampon de la session d'un flux modulé, en blocs : il borne l'avance du
/// producteur, donc le retard du volume (Q9 : « réponse plus lente »).
const BLOCS_DE_SESSION: usize = 8;
/// Taille d'un bloc PCM décodé (≈ 46 ms en stéréo 44,1 kHz, 32 bits).
const OCTETS_PAR_BLOC_PCM: usize = 16_384;

/// Ce que l'orchestrateur sait de la sortie d'une zone, pour la décision.
struct FaitsDeZone {
    sortie_locale: bool,
    dsd_mode: String,
    plafond_zone_hz: Option<u32>,
    natif: SortieDsdLocale,
    /// Nom du périphérique local (réponses DoP et ruptures retenues).
    peripherique: Option<String>,
    /// Faux tant que la sortie de la zone n'a jamais pu être lue (registre ou
    /// sortie occupés, nom jamais retenu) : les capacités sont « inconnues ».
    connue: bool,
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

    /// Un porteur DoP déjà refusé à cette cadence : par la sonde exclusive
    /// (#7265), ou par une rupture constatée à l'ouverture.
    fn dop_refuse_connu(&self, cadence_dop: u32, voies: u16) -> Option<MotifDsdRefuse> {
        let nom = self.peripherique.as_deref()?;
        if modulation::rupture_connue(nom, cadence_dop, voies) {
            return Some(MotifDsdRefuse::CarrierRateNotSupported);
        }
        matches!(
            crate::audio::dop_ecarte_7265::accueil_exclusif_connu(nom, cadence_dop, voies),
            Some(crate::audio::dop_ecarte_7265::AccueilDuDop::ExclusifRefuse { .. })
        )
        .then_some(MotifDsdRefuse::DopRefusedByOutput)
    }

    fn pures<'a>(
        &'a self,
        natif: &'a dyn Fn(u32, u16) -> Result<(), &'static str>,
        dop_refuse: &'a dyn Fn(u32, u16) -> Option<MotifDsdRefuse>,
        temps_reel: Option<[f64; 3]>,
    ) -> FaitsSortieDsd<'a> {
        FaitsSortieDsd {
            sortie_locale: self.sortie_locale,
            dsd_mode: &self.dsd_mode,
            plafond_zone_hz: self.plafond_zone_hz,
            natif,
            dop_refuse,
            temps_reel,
        }
    }
}

/// Ce que l'on sait du périphérique d'une sortie locale.
enum NomDuPeripherique {
    Connu(String),
    /// Aucune sortie inscrite (zone sans périphérique, bancs d'essai).
    Absent,
    /// Sortie occupée et jamais lue : rien de sûr à publier.
    Inconnu,
}

/// Le nom du périphérique d'une sortie locale, retenu à la première lecture
/// réussie : les routes ne l'attendent jamais derrière une sortie occupée.
fn noms_retenus() -> &'static Mutex<HashMap<String, String>> {
    static N: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    N.get_or_init(Default::default)
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

/// Le volume de la zone tel que la sortie locale l'appliquerait (consigne
/// linéaire, verrou Pure, sourdine, trim), pour le modulateur.
fn gain_de_volume(volume: f64, muet: bool, trim_db: f64) -> f64 {
    if muet {
        return 0.0;
    }
    let trim = if trim_db < 0.0 {
        10f64.powf(trim_db / 20.0)
    } else {
        1.0
    };
    (volume.clamp(0.0, 1.0) * trim).clamp(0.0, 1.0)
}

/// Ce dont le fil du producteur a besoin pour suivre le volume de la zone,
/// sans verrou ni base à chaque bloc : le volume et la sourdine publiés au
/// changement (valeur atomique), le trim et le verrou Pure lus une fois.
struct SuiviDuVolume {
    publie: Arc<modulation::VolumePublie>,
    trim_db: f64,
    verrou_pure: bool,
}

impl SuiviDuVolume {
    fn gain(&self) -> f64 {
        let volume = if self.verrou_pure {
            1.0
        } else {
            self.publie.volume()
        };
        gain_de_volume(volume, self.publie.muet(), self.trim_db)
    }
}

/// Le lien d'enchaînement d'une piste : la session de la piste précédente,
/// dont le producteur déposera son état.
struct Enchainement {
    precedente: String,
    plan: PlanModulation,
}

/// Délai d'attente maximal de l'état de la piste précédente : elle est
/// pré-armée ~30 s avant la fin, et son producteur finit ~2,5 s avant.
const ATTENTE_DU_RELAIS: Duration = Duration::from_secs(60);

/// Le fil du producteur : en-tête, état de la piste précédente s'il y a un
/// lien (attendu ICI, au premier bloc réel), décodage en entiers 32 bits
/// (mono dupliqué en stéréo), greffons de la zone, `f64`, volume de la zone,
/// modulateur, porteur. Rend le producteur à la FIN NORMALE de la piste (pour
/// la suivante), et dit si l'état précédent a été repris.
#[allow(clippy::too_many_arguments)]
fn produire_le_flux_dsd(
    fichier: String,
    seek_s: f64,
    duree_s: Option<f64>,
    duree_servie_ms: u64,
    neuf: ProducteurDsd,
    enchainement: Option<Enchainement>,
    au_repris: &dyn Fn(),
    mut dsp: StreamingDsp,
    mono: bool,
    volume: SuiviDuVolume,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    data_ready: Arc<tokio::sync::Notify>,
    rt: tokio::runtime::Handle,
) -> Result<ProducteurDsd, String> {
    let _actif = modulation::ProducteurActif::tenir();
    let plan = neuf.plan();
    let delai = crate::http::streamer::SESSION_IDLE_TIMEOUT;
    let envoyer = |bloc: Vec<u8>| -> Result<(), String> {
        match rt.block_on(tokio::time::timeout(delai, tx.send(bloc))) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err("session fermée".into()),
            Err(_) => Err("délai d'envoi dépassé".into()),
        }
    };
    envoyer(neuf.entete(Some(duree_servie_ms)))?;
    data_ready.notify_one();
    // Enchaînement : l'état de la piste précédente, au moment où celle-ci
    // commence vraiment à moduler — pas à sa résolution, 30 s plus tôt.
    let repris = enchainement.and_then(|e| {
        modulation::attendre_le_relais(&e.precedente, e.plan, ATTENTE_DU_RELAIS, &|| tx.is_closed())
    });
    let gain_fixe = neuf.gain_fixe();
    let mut producteur = match repris {
        Some(mut p) => {
            au_repris();
            // Même modulateur : la rampe suit un éventuel changement.
            p.definir_gain_fixe(gain_fixe);
            p.definir_volume(volume.gain());
            p
        }
        None => {
            let mut p = neuf;
            // Modulateur neuf : le niveau est posé AVANT le premier
            // échantillon (jamais une milliseconde à 0 dB).
            p.fixer_volume(volume.gain());
            p
        }
    };

    let voies = usize::from(plan.voies);
    let (tx_pcm, mut rx_pcm) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let (niveaux_tx, _niveaux_rx) = tokio::sync::mpsc::unbounded_channel();
    let canaux = u32::from(plan.voies);
    let cadence = plan.cadence_pcm;
    let decodeur = rt.spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming_tranche(
            &fichier,
            Some(cadence),
            // Le porteur est stéréo : une source mono est dupliquée.
            Some(canaux),
            Some(32),
            tx_pcm,
            OCTETS_PAR_BLOC_PCM,
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
        // Q9 : le volume juste avant le modulateur, relu à chaque bloc sans
        // verrou (valeur publiée) ; la rampe de 10 ms de T11b évite la marche.
        producteur.definir_volume(volume.gain());
        let mut pcm: Vec<u8> = reste.drain(..utiles).collect();
        dsp.process_flux(&mut pcm, 32);
        sortie.clear();
        traiter(&mut pcm, &mut producteur, &mut sortie)?;
        if !sortie.is_empty() {
            envoyer(std::mem::take(&mut sortie))?;
        }
    }
    // Fin de piste : la queue du convolveur. Le retard du suréchantillonneur
    // attend une éventuelle suivante liée.
    let mut queue = dsp.terminer_flux(32);
    sortie.clear();
    if !queue.is_empty() {
        traiter(&mut queue, &mut producteur, &mut sortie)?;
    }
    if !sortie.is_empty() {
        envoyer(sortie)?;
    }
    match rt.block_on(decodeur) {
        Ok(Ok(_)) => Ok(producteur),
        Ok(Err(e)) => Err(format!("décodage : {e}")),
        Err(e) => Err(format!("décodage interrompu : {e}")),
    }
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
            connue: true,
        };
        if !sortie_locale {
            return faits;
        }
        let nom = self.nom_du_peripherique_local(device_id.as_deref());
        faits.connue = !matches!(nom, NomDuPeripherique::Inconnu);
        faits.peripherique = match nom {
            NomDuPeripherique::Connu(n) => Some(n),
            _ => None,
        };
        if faits.dsd_mode == "native" {
            faits.natif = if sonder {
                self.sortie_dsd_locale(device_id.as_deref()).await
            } else {
                self.sortie_dsd_locale_connue(faits.peripherique.as_deref())
            };
        }
        faits
    }

    /// Le nom du périphérique, sans jamais attendre : registre ou sortie
    /// tenus par une lecture → le nom retenu la dernière fois ; jamais vu →
    /// [`NomDuPeripherique::Inconnu`] (et non une réponse fausse).
    fn nom_du_peripherique_local(&self, device_id: Option<&str>) -> NomDuPeripherique {
        let Some(device_id) = device_id else {
            return NomDuPeripherique::Absent;
        };
        let retenu = || {
            noms_retenus()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(device_id)
                .cloned()
        };
        #[cfg(feature = "local-audio")]
        {
            let Ok(registre) = self.outputs.try_lock() else {
                return retenu().map_or(NomDuPeripherique::Inconnu, NomDuPeripherique::Connu);
            };
            let Some(sortie) = registre.get(device_id) else {
                // Aucune sortie inscrite à ce nom : rien de propre au
                // périphérique à refuser.
                return NomDuPeripherique::Absent;
            };
            drop(registre);
            let lu = sortie.try_lock().ok().and_then(|sortie| {
                sortie
                    .as_any()
                    .downcast_ref::<crate::outputs::local::LocalOutput>()
                    .map(|l| l.nom_du_peripherique().to_string())
            });
            match lu {
                Some(nom) => {
                    noms_retenus()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(device_id.to_owned(), nom.clone());
                    NomDuPeripherique::Connu(nom)
                }
                None => retenu().map_or(NomDuPeripherique::Inconnu, NomDuPeripherique::Connu),
            }
        }
        #[cfg(not(feature = "local-audio"))]
        {
            retenu().map_or(NomDuPeripherique::Absent, NomDuPeripherique::Connu)
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

    /// rc14 — les capacités DSD de la zone (`dsd_capabilities`). Peu
    /// coûteuses : quelques lectures de la base, aucune sonde, aucun verrou
    /// de sortie attendu (nom du périphérique retenu), mesure de la machine
    /// en cache (lancée en tâche de fond la première fois, jamais attendue).
    pub async fn capacites_dsd_de_zone(&self, zone_id: i64) -> CapacitesDsd {
        let faits = self.faits_dsd_de_zone(zone_id, false).await;
        if faits.sortie_locale {
            modulation::lancer_la_mesure_en_fond();
        }
        let natif = |c: u32, v: u16| faits.natif(c, v);
        let dop_refuse = |c: u32, v: u16| faits.dop_refuse_connu(c, v);
        modulation::capacites(
            &faits.pures(&natif, &dop_refuse, modulation::mesure_de_la_machine()),
            faits.connue,
        )
    }

    /// La piste qui joue sur la zone est-elle un flux modulé vers le DSD ?
    pub(super) async fn modulation_dsd_en_service(&self, zone_id: i64) -> bool {
        let etat = self.playback.get_state(zone_id).await;
        let Some(np) = etat.now_playing.as_ref() else {
            return false;
        };
        matches!(
            modulation::etat_pour(zone_id, np.stream_id.as_deref(), np.track_id),
            Some(EtatModulation::EnService(_))
        )
    }

    /// rc14 — la sortie a refusé le porteur DoP d'un flux modulé (le
    /// périphérique s'est ouvert à une autre cadence : les marqueurs seraient
    /// détruits) ? Vérification seule, rapide : le sondeur lance ensuite
    /// [`Self::relancer_apres_rupture_dsd`] en tâche DÉTACHÉE et n'arrête pas
    /// la zone.
    pub async fn rupture_dsd_a_relancer(&self, zone_id: i64) -> bool {
        if !self.modulation_dsd_en_service(zone_id).await {
            return false;
        }
        let device_id = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);
        match self.nom_du_peripherique_local(device_id.as_deref()) {
            NomDuPeripherique::Connu(nom) => {
                modulation::rupture_recente(&nom, Duration::from_secs(15))
            }
            _ => false,
        }
    }

    /// La relance : la piste repart à sa position, et la décision la fait
    /// passer en PCM (`carrier_rate_not_supported`) au lieu du silence.
    pub async fn relancer_apres_rupture_dsd(&self, zone_id: i64) {
        let etat = self.playback.get_state(zone_id).await;
        let position = super::dsp::position_a_reprendre(&etat);
        warn!(zone_id, position, "tess_dsd_porteur_refuse_relance_en_pcm");
        if let Err(e) =
            Box::pin(self.replay_zone_at_position(zone_id, position, "tess_dsd_porteur_refuse"))
                .await
        {
            warn!(zone_id, error = %e, "tess_dsd_relance_apres_rupture_refusee");
        }
    }

    /// rc14 — un réglage de la chaîne a changé pendant une piste modulée :
    /// la sortie ne traite pas un porteur DSD, c'est le producteur qui porte
    /// les greffons. La piste est relancée à la même position (anti-rebond et
    /// plancher de [`Self::schedule_eq_replay`]). `true` : relance programmée.
    pub async fn relancer_si_flux_dsd_module(self: &Arc<Self>, zone_id: i64) -> bool {
        if !self.modulation_dsd_en_service(zone_id).await {
            return false;
        }
        info!(zone_id, "tess_dsd_reglage_change_relance");
        self.schedule_eq_replay(zone_id)
    }

    /// Le préampli que l'égaliseur désactivé garde dans la sortie locale (dB
    /// < 0), pour le reporter dans le flux modulé. Lu en ATTENDANT la sortie,
    /// comme le fait déjà la protection DoP sur ce même chemin de résolution
    /// (`attenuation_logicielle_demandee_pour_dop`) : une valeur sûre, jamais
    /// un 0 dB par défaut parce que la sortie était occupée.
    async fn preampli_garde_db(&self, device_id: Option<&str>) -> f64 {
        #[cfg(feature = "local-audio")]
        {
            let Some(id) = device_id else {
                return 0.0;
            };
            let sortie = { self.outputs.lock().await.get(id) };
            let Some(sortie) = sortie else {
                return 0.0;
            };
            let garde = sortie.lock().await;
            garde
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
                .and_then(|l| l.preampli_garde())
                .map(|g| g.db)
                .filter(|db| *db < 0.0)
                .unwrap_or(0.0)
        }
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = device_id;
            0.0
        }
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
        // moteur installé, PURE, « Bypass all ») et donne le headroom à
        // appliquer avant le modulateur. Les fermetures (non `Send`) vivent
        // dans ce bloc, hors des `await`.
        let (effective, headroom_db) = {
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
            let r = lecture
                .chaine
                .resoudre(&crate::tess::catalogue_de_reference(), &ctx);
            let effective = r
                .etapes
                .iter()
                .any(|e| e.code == CodeEtape::Upsampling && e.effective);
            (effective, r.headroom_db.min(0.0))
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
            modulation::noter(
                zone_id,
                CleFlux {
                    track_id: piste,
                    stream_id: None,
                },
                EtatModulation::Repli(motif),
            );
            Ok(None)
        };
        let faits = self.faits_dsd_de_zone(zone_id, true).await;
        let temps_reel = match modulation::mesure_de_la_machine() {
            Some(m) => Some(m),
            None => tokio::task::spawn_blocking(modulation::mesurer_la_machine)
                .await
                .ok(),
        };
        let plan = {
            let natif = |c: u32, v: u16| faits.natif(c, v);
            let dop_refuse = |c: u32, v: u16| faits.dop_refuse_connu(c, v);
            modulation::planifier(
                &faits.pures(&natif, &dop_refuse, temps_reel),
                rate,
                cadence_pcm,
                voies,
            )
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
                .accueil_du_dop_local(req.output_device_id.as_deref(), cadence_dop, plan.voies)
                .await;
            if crate::audio::dop_ecarte_7265::decider(&accueil, cadence_dop, plan.voies).is_some() {
                return repli(MotifDsdRefuse::DopRefusedByOutput);
            }
        }
        match self
            .servir_la_modulation_dsd(req, track, file_path, plan, headroom_db, tranche)
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
        headroom_db: f64,
        tranche: Option<&super::resolve_local::TrancheCue>,
    ) -> Result<ResolvedStream, MotifDsdRefuse> {
        let zone_id = req.zone_id;
        // Enchaînement : seulement la SUIVANTE de la file annoncée (lien
        // explicite), depuis son début, d'une piste modulée compatible. L'état
        // est repris au premier bloc réel, dans le fil du producteur.
        let lien = if req.seek_ms.unwrap_or(0) == 0 {
            modulation::prendre_le_lien(zone_id, req.track_id, plan)
        } else {
            None
        };
        // Le modulateur neuf, prêt au cas où rien ne continue. Conception du
        // NTF et des filtres : hors du fil asynchrone.
        let mut neuf = tokio::task::spawn_blocking(move || ProducteurDsd::new(plan))
            .await
            .map_err(|_| MotifDsdRefuse::ModulatorFailed)?
            .map_err(|e| {
                warn!(zone_id, error = %e, "tess_dsd_modulateur_non_construit");
                MotifDsdRefuse::ModulatorFailed
            })?;
        let preampli_db = self
            .preampli_garde_db(req.output_device_id.as_deref())
            .await;
        neuf.definir_gain_fixe(10f64.powf((headroom_db + preampli_db) / 20.0));
        // Le volume : publié tel qu'il est maintenant, puis suivi sans verrou.
        let etat = self.playback.get_state(zone_id).await;
        modulation::publier_volume(zone_id, etat.volume, etat.muted);
        let volume = SuiviDuVolume {
            publie: modulation::volume_de_la_zone(zone_id),
            trim_db: super::regles::gain_trim_db_enregistre(&self.db, zone_id),
            verrou_pure: crate::audio::audiophile::effective_volume(&self.db, zone_id, 0.0) >= 1.0,
        };
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
        let (session_id, tx, data_ready) = self
            .streamer
            .create_session(info, true, BLOCS_DE_SESSION)
            .await;
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
        // Le relais de CETTE piste, ouvert avant toute résolution de la
        // suivante.
        modulation::ouvrir_le_relais(zone_id, &session_id, plan);
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
            voies_source = plan.voies_source,
            transport = ?plan.transport,
            headroom_db,
            preampli_db,
            lien = ?lien,
            greffons_dans_le_flux,
            mono,
            "tess_dsd_modulation_lancee"
        );
        let en_service = ModulationEnService {
            rate: plan.taux,
            rate_hz: plan.cadence_dsd,
            transport: plan.transport,
            order: modulation::ORDRE_DU_MODULATEUR,
            carrier_rate_hz: plan.cadence_du_porteur(),
            source_rate_hz: plan.cadence_pcm,
            source_channels: plan.voies_source,
            channels: plan.voies,
            plugins_in_stream: greffons_dans_le_flux || mono,
            headroom_db,
            continued: false,
        };
        let cle = CleFlux {
            track_id: req.track_id,
            stream_id: Some(session_id.clone()),
        };
        modulation::noter(
            zone_id,
            cle.clone(),
            EtatModulation::EnService(en_service.clone()),
        );
        let fichier = file_path.to_string();
        let session = session_id.clone();
        let enchainement = lien.map(|precedente| Enchainement { precedente, plan });
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            let au_repris = || {
                let mut e = en_service.clone();
                e.continued = true;
                modulation::noter(zone_id, cle.clone(), EtatModulation::EnService(e));
            };
            match produire_le_flux_dsd(
                fichier,
                seek_s,
                duree_s,
                duree_servie_ms,
                neuf,
                enchainement,
                &au_repris,
                dsp,
                mono,
                volume,
                tx,
                data_ready,
                rt,
            ) {
                Ok(producteur) => {
                    let bilan = BilanDuFlux {
                        reinitialisations: producteur.reinitialisations(),
                        ecretages: producteur.ecretages(),
                        entree_max: producteur.entree_max(),
                    };
                    modulation::noter_bilan(&session, bilan);
                    if bilan.reinitialisations > 0 {
                        warn!(zone_id, ?bilan, "tess_dsd_modulateur_instable");
                    } else {
                        tracing::debug!(zone_id, ?bilan, "tess_dsd_modulation_terminee");
                    }
                    modulation::deposer_au_relais(&session, producteur);
                }
                Err(e) => {
                    modulation::abandonner_le_relais(&session);
                    warn!(zone_id, error = %e, "tess_dsd_modulation_interrompue");
                }
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
