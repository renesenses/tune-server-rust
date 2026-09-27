//! Parametric equalizer host facade. Active EQ modifies samples; f64 arithmetic
//! does not imply bit-perfection. Both bundled and signed native providers use
//! the same persisted profile and producer locations (including radio).
use super::ecretage::{CompteurDEcretage, EtageEcretant, Portee, REGISTRE, dire_fin, dire_premier};
use std::sync::atomic::{AtomicBool, Ordering};
pub use tune_plugin_equalizer::{
    DEBIT_DE_REFERENCE_HZ, EqBandSpec, EqProcessStats, EqProfile, HeadroomMode, ListeningMode,
    RoomSize, SpeakerPlacement,
};
use tune_plugin_native::stage::Stage;
/// #5215 — durée de la rampe qui accompagne une bascule d'égaliseur EN VOL.
///
/// Couper l'égaliseur retirait d'un coup un préampli de −12,56 dB : au casque,
/// une marche de 12 dB (Levente Toth, fil 1974). Le mutex relu à chaque
/// paquet rendait le remplacement instantané, donc la marche aussi.
///
/// 200 ms, parce que c'est l'ordre de la constante d'intégration de la
/// sonie : en deçà, l'oreille entend encore une MARCHE (20 ms suffisent
/// contre le clic, pas contre le sursaut) ; au-delà, le geste « couper l'EQ
/// pour comparer » paraît retardé. La rampe ne sert QUE les bascules d'un
/// réglage en cours de lecture ([`EqProcessor::prendre_la_releve`]) : un
/// début de piste passe par `LocalOutput::set_eq`, sans rampe.
pub const RAMPE_DE_BASCULE_MS: u32 = 200;

/// #5215 — écart de niveau (préampli ou niveau moyen, en dB) au-delà duquel
/// un remplacement d'égaliseur est fondu plutôt qu'instantané.
///
/// En deçà, rien ne change : un cran de curseur reste immédiat (#1725), son
/// biquad hérite de l'historique et ne claque pas.
pub const SEUIL_DE_RAMPE_DB: f64 = 1.0;

/// Fondu enchaîné en cours : le signal passe de `depart` (l'égaliseur qu'on
/// quitte, ou le signal SEC quand `None`) au traitement courant.
struct Fondu {
    depart: Option<Box<EqProcessor>>,
    total: usize,
    fait: usize,
    tampon: Vec<f32>,
}

pub struct EqProcessor {
    engine: Engine,
    sample_rate: u32,
    channels: u16,
    fondu: Option<Fondu>,
    /// #5227 — gain linéaire appliqué APRÈS le filtre : la compensation de
    /// niveau, portée par l'égaliseur au lieu du volume. Voir
    /// [`Self::porter_la_compensation`].
    compensation: f32,
    /// #4685 — niveau moyen du filtre, réserve comprise, calculé UNE fois à
    /// la construction (voir [`Self::gain_moyen_db`]).
    gain_moyen_db: f64,
    clipping: CompteurDEcretage,
    closed: AtomicBool,
}
enum Engine {
    Bundled(tune_plugin_equalizer::EqProcessor),
    Native(Stage),
    Unavailable,
    /// #5215 — égaliseur COUPÉ en vol : ne filtre rien, ne sert qu'à porter
    /// le fondu depuis l'égaliseur qu'on vient de quitter. Retiré par la
    /// chaîne locale dès le fondu fini (voir [`EqProcessor::est_neutre_au_repos`]).
    Neutre,
}
impl EqProcessor {
    pub fn new(profile: &EqProfile, sample_rate: u32, channels: u16) -> Self {
        let engine = if tune_plugin_native::failure("equalizer").is_some() {
            Engine::Unavailable
        } else if let Some(provider) = tune_plugin_native::provider("equalizer") {
            match serde_json::to_value(profile)
                .map_err(|e| e.to_string())
                .and_then(|settings| {
                    Stage::prepare(provider, sample_rate, channels, &settings)
                        .map_err(|e| e.to_string())
                }) {
                Ok(stage) => Engine::Native(stage),
                Err(error) => {
                    tracing::error!(%error,"native_equalizer_prepare_failed");
                    Engine::Unavailable
                }
            }
        } else {
            Engine::Bundled(tune_plugin_equalizer::EqProcessor::new(
                profile,
                sample_rate,
                channels,
            ))
        };
        // Calculé depuis le PROFIL, pour les deux moteurs : le greffon natif
        // signé exécute la même arithmétique que le moteur embarqué (même
        // crate), et ne publie pas d'autre porte. Un moteur indisponible ne
        // filtre rien — il n'a donc rien à compenser.
        let gain_moyen_db = if matches!(engine, Engine::Unavailable | Engine::Neutre) {
            0.0
        } else {
            profile.gain_moyen_db_at(channels, f64::from(sample_rate))
        };
        Self {
            engine,
            sample_rate,
            channels,
            fondu: None,
            compensation: 1.0,
            gain_moyen_db,
            clipping: Default::default(),
            closed: AtomicBool::new(false),
        }
    }

    fn neutre(sample_rate: u32, channels: u16) -> Self {
        Self {
            engine: Engine::Neutre,
            sample_rate,
            channels,
            fondu: None,
            compensation: 1.0,
            gain_moyen_db: 0.0,
            clipping: Default::default(),
            closed: AtomicBool::new(false),
        }
    }

    /// #5215 — `neuf` remplace `precedent` PENDANT la lecture.
    ///
    /// Hérite de l'historique des filtres comme avant (#1725), et arme en plus
    /// un fondu enchaîné de [`RAMPE_DE_BASCULE_MS`] quand la bascule change le
    /// niveau : activation, coupure, ou préampli / niveau moyen déplacé de
    /// plus de [`SEUIL_DE_RAMPE_DB`]. Le fondu mélange les DEUX sorties
    /// (ancienne et nouvelle chaîne), il suit donc le préampli comme la
    /// courbe, sans marche ni clic.
    ///
    /// Une coupure rend un égaliseur `Neutre` qui porte le fondu vers le
    /// signal sec ; la chaîne locale le retire une fois le fondu fini.
    pub fn prendre_la_releve(neuf: Option<Self>, precedent: Option<Self>) -> Option<Self> {
        match (neuf, precedent) {
            (None, None) => None,
            (Some(mut neuf), None) => {
                neuf.armer_le_fondu(None);
                Some(neuf)
            }
            (None, Some(precedent)) => {
                if precedent.est_neutre_au_repos() {
                    return None;
                }
                if matches!(precedent.engine, Engine::Neutre) {
                    // Déjà en train de descendre vers le sec : on le laisse finir.
                    return Some(precedent);
                }
                let mut neutre = Self::neutre(precedent.sample_rate, precedent.channels);
                neutre.armer_le_fondu(Some(precedent));
                Some(neutre)
            }
            (Some(mut neuf), Some(mut precedent)) => {
                neuf.inherit_state_from(&precedent);
                let compatible = neuf.sample_rate == precedent.sample_rate
                    && neuf.channels == precedent.channels;
                if !compatible {
                    return Some(neuf);
                }
                if neuf.ecart_de_niveau_db(&precedent) > SEUIL_DE_RAMPE_DB {
                    neuf.armer_le_fondu(Some(precedent));
                } else if let Some(fondu) = precedent.fondu.take() {
                    // Petit cran pendant un fondu : le nouveau reprend le fondu
                    // là où il en est, au lieu de sauter à sa fin.
                    neuf.fondu = Some(fondu);
                }
                Some(neuf)
            }
        }
    }

    fn armer_le_fondu(&mut self, depart: Option<Self>) {
        let total =
            (u64::from(self.sample_rate) * u64::from(RAMPE_DE_BASCULE_MS) / 1000).max(1) as usize;
        self.fondu = Some(Fondu {
            depart: depart.map(Box::new),
            total,
            fait: 0,
            tampon: Vec::new(),
        });
    }

    fn ecart_de_niveau_db(&self, autre: &Self) -> f64 {
        let preamp = (0..self.channels.max(1))
            .map(|c| (self.preamp_db(c).unwrap_or(0.0) - autre.preamp_db(c).unwrap_or(0.0)).abs())
            .fold(0.0_f64, f64::max);
        preamp.max((self.gain_moyen_db - autre.gain_moyen_db).abs())
    }

    /// #5227 — faire porter la compensation de niveau par CET égaliseur.
    ///
    /// Rendue par le volume, la compensation changeait au paquet suivant du
    /// RAPPEL, alors que l'égaliseur s'applique avant l'anneau : à
    /// l'activation, le volume remontait ~2 s avant que le préampli ne soit
    /// entendu — une bouffée de +12 dB. Multipliée ici, dans le même
    /// échantillon que le filtre, elle traverse l'anneau AVEC lui, et chaque
    /// côté du fondu de [`Self::prendre_la_releve`] porte la sienne. Le
    /// volume n'en garde que le rabot à l'unité (`LocalOutput`).
    ///
    /// `facteur` est linéaire, borné à ≥ 1 : ne sert qu'à rendre ce que la
    /// réserve retire.
    pub fn porter_la_compensation(&mut self, facteur: f64) {
        self.compensation = if facteur.is_finite() {
            facteur.clamp(1.0, 64.0) as f32
        } else {
            1.0
        };
    }

    /// #5227 — la compensation que porte cet égaliseur (1,0 = aucune).
    pub fn compensation_portee(&self) -> f64 {
        f64::from(self.compensation)
    }

    /// #5215 — un fondu de bascule est-il en cours ?
    pub fn en_fondu(&self) -> bool {
        self.fondu.is_some()
    }

    /// #5215 — égaliseur COUPÉ : `Neutre`, fondu en cours ou non. Il ne
    /// compte pas comme un égaliseur monté.
    pub fn est_neutre(&self) -> bool {
        matches!(self.engine, Engine::Neutre)
    }

    /// #5215 — égaliseur coupé dont le fondu est fini : identité exacte, à
    /// retirer de la chaîne.
    pub fn est_neutre_au_repos(&self) -> bool {
        self.est_neutre() && self.fondu.is_none()
    }

    /// #4685 — ce que cet égaliseur, réserve automatique comprise, fait
    /// gagner ou perdre au niveau MOYEN, en dB, sur un bruit rose. Négatif
    /// dès qu'une bande pousse : la réserve anti-écrêtage retire plus que la
    /// courbe ne rend en moyenne. Voir `EqProfile::gain_moyen_db_at`.
    pub fn gain_moyen_db(&self) -> f64 {
        self.gain_moyen_db
    }
    pub fn process_pcm(&mut self, pcm: &mut [u8], depth: u16) -> EqProcessStats {
        // #4407 — une relève en vol sur un porteur d'OCTETS (relais DSP
        // progressif d'un flux réseau) : le fondu de `prendre_la_releve` ne
        // vit que dans le chemin flottant. Le temps du fondu (200 ms), le
        // bloc y passe ; ensuite, retour au chemin entier habituel.
        if self.fondu.is_some() {
            return self.process_pcm_en_fondu(pcm, depth);
        }
        match &mut self.engine {
            Engine::Bundled(p) => p.process_pcm(pcm, depth),
            Engine::Native(p) => record_native(&mut self.clipping, p.process_pcm(pcm, depth)),
            Engine::Unavailable | Engine::Neutre => EqProcessStats::default(),
        }
    }
    /// #4407 — [`Self::process_pcm`] pendant un fondu de relève : PCM entier
    /// petit-boutiste (16, 24 ou 32 bits) → flottant → fondu → entier. Un bloc
    /// qui n'est pas un nombre entier de trames, ou une profondeur inconnue,
    /// abandonne le fondu plutôt que de décaler les états par canal.
    fn process_pcm_en_fondu(&mut self, pcm: &mut [u8], depth: u16) -> EqProcessStats {
        let octets = usize::from(depth / 8);
        let trame = octets * usize::from(self.channels.max(1));
        if !(2..=4).contains(&octets) || pcm.is_empty() || !pcm.len().is_multiple_of(trame) {
            self.fondu = None;
            return self.process_pcm(pcm, depth);
        }
        let echelle = (1i64 << (depth - 1)) as f64;
        let mut flottants: Vec<f32> = pcm
            .chunks_exact(octets)
            .map(|s| {
                let mut b = [0u8; 4];
                b[4 - octets..].copy_from_slice(s);
                // Aligné en haut d'un i32 puis ramené : l'extension de signe
                // est faite par le décalage arithmétique.
                (f64::from(i32::from_le_bytes(b) >> (32 - 8 * octets)) / echelle) as f32
            })
            .collect();
        let stats = self.process_interleaved(&mut flottants);
        for (s, v) in pcm.chunks_exact_mut(octets).zip(flottants) {
            let entier = (f64::from(v) * echelle)
                .round()
                .clamp(-echelle, echelle - 1.0) as i32;
            s.copy_from_slice(&entier.to_le_bytes()[..octets]);
        }
        stats
    }
    pub fn process_interleaved(&mut self, samples: &mut [f32]) -> EqProcessStats {
        let Some(mut fondu) = self.fondu.take() else {
            return self.traiter_sans_fondu(samples);
        };
        fondu.tampon.clear();
        fondu.tampon.extend_from_slice(samples);
        if let Some(depart) = fondu.depart.as_mut() {
            depart.process_interleaved(&mut fondu.tampon);
        }
        let stats = self.traiter_sans_fondu(samples);
        let canaux = self.channels.max(1) as usize;
        for (i, (trame, avant)) in samples
            .chunks_mut(canaux)
            .zip(fondu.tampon.chunks(canaux))
            .enumerate()
        {
            let t = ((fondu.fait + i + 1) as f32 / fondu.total as f32).min(1.0);
            for (s, a) in trame.iter_mut().zip(avant) {
                *s = *a + (*s - *a) * t;
            }
        }
        fondu.fait += samples.len() / canaux;
        if fondu.fait < fondu.total {
            self.fondu = Some(fondu);
        }
        stats
    }
    fn traiter_sans_fondu(&mut self, samples: &mut [f32]) -> EqProcessStats {
        let stats = match &mut self.engine {
            Engine::Bundled(p) => p.process_interleaved(samples),
            Engine::Native(p) => record_native(&mut self.clipping, p.process_f32(samples)),
            Engine::Unavailable | Engine::Neutre => EqProcessStats::default(),
        };
        // #5227 — après le filtre, donc après ses compteurs d'écrêtage : le
        // rendu flottant dépasse l'unité ici, le volume le ramène au rappel.
        if self.compensation != 1.0 {
            for s in samples.iter_mut() {
                *s *= self.compensation;
            }
        }
        stats
    }
    pub fn is_enabled(&self) -> bool {
        match &self.engine {
            Engine::Bundled(p) => p.is_enabled(),
            Engine::Native(p) => p.info["enabled"].as_bool().unwrap_or(false),
            Engine::Unavailable | Engine::Neutre => false,
        }
    }
    pub fn response(&self, sample_rate: u32) -> serde_json::Value {
        match &self.engine {
            Engine::Bundled(p) => p.response(sample_rate),
            Engine::Native(p) => p.info["response"].clone(),
            Engine::Unavailable | Engine::Neutre => serde_json::Value::Null,
        }
    }
    pub fn preamp_db(&self, channel: u16) -> Option<f64> {
        match &self.engine {
            Engine::Bundled(p) => p.preamp_db(channel),
            Engine::Native(p) => p.info["preamp_db"].get(channel as usize)?.as_f64(),
            Engine::Unavailable | Engine::Neutre => None,
        }
    }
    pub fn process_stats(&self) -> EqProcessStats {
        match &self.engine {
            Engine::Bundled(p) => p.process_stats(),
            Engine::Native(p) => EqProcessStats {
                overs: p.report.clipped_samples,
                non_finite_samples: p.report.non_finite_samples,
            },
            Engine::Unavailable | Engine::Neutre => EqProcessStats::default(),
        }
    }
    pub fn ecretage(&self) -> super::ecretage::CompteurDEcretage {
        match &self.engine {
            Engine::Bundled(p) => p.ecretage(),
            Engine::Native(_) => self.clipping,
            Engine::Unavailable | Engine::Neutre => Default::default(),
        }
    }
    pub fn inherit_state_from(&mut self, previous: &Self) {
        match (&mut self.engine, &previous.engine) {
            (Engine::Bundled(new), Engine::Bundled(old)) => new.inherit_state_from(old),
            (Engine::Native(new), Engine::Native(old)) => {
                if let Err(error) = new.inherit(old) {
                    tracing::debug!(%error,"native_eq_history_not_compatible");
                } else {
                    self.clipping = previous.clipping;
                    previous.closed.store(true, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }
}
impl Drop for EqProcessor {
    fn drop(&mut self) {
        if matches!(self.engine, Engine::Native(_))
            && !self.closed.swap(true, Ordering::Relaxed)
            && self.clipping.echantillons_ecretes > 0
        {
            REGISTRE.egaliseur.piste_close();
            dire_fin(EtageEcretant::Egaliseur, Portee::Piste, &self.clipping);
        }
    }
}
fn record_native(
    previous: &mut CompteurDEcretage,
    result: Result<tune_plugin_sdk::audio::ProcessReport, tune_plugin_sdk::Error>,
) -> EqProcessStats {
    match result {
        Ok(report) => {
            if let Some(c) = report.clipping {
                let next = CompteurDEcretage {
                    echantillons_vus: c.samples_seen,
                    echantillons_ecretes: c.clipped_samples,
                    exces_max_lsb: c.max_excess_lsb,
                    crete_max: f64::from_bits(c.max_peak_bits),
                    premier_ecretage_a: c.first_clip,
                };
                REGISTRE.egaliseur.absorber(previous, &next);
                if previous.echantillons_ecretes == 0 && next.echantillons_ecretes > 0 {
                    dire_premier(EtageEcretant::Egaliseur, Portee::Piste, &next);
                }
                *previous = next;
            }
            EqProcessStats {
                overs: report.clipped_samples,
                non_finite_samples: report.non_finite_samples,
            }
        }
        Err(error) => {
            tracing::error!(%error,"native_equalizer_processing_failed");
            EqProcessStats::default()
        }
    }
}

#[cfg(test)]
mod rampe_de_bascule_5215 {
    use super::*;

    fn egaliseur(gain: f64) -> EqProcessor {
        let profil = EqProfile {
            enabled: true,
            bands: vec![EqBandSpec {
                freq: 8000.0,
                gain,
                q: 1.0,
                band_type: "peak".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        EqProcessor::new(&profil, 48_000, 2)
    }

    /// Un cran de curseur reste IMMÉDIAT (#1725) : pas de fondu sous le seuil.
    #[test]
    fn un_petit_cran_reste_instantane() {
        let neuf = EqProcessor::prendre_la_releve(Some(egaliseur(12.3)), Some(egaliseur(12.0)))
            .expect("un égaliseur reste monté");
        assert!(
            !neuf.en_fondu(),
            "0,3 dB de préampli ne doit pas armer de rampe"
        );
    }

    /// Un préampli déplacé de plusieurs dB est fondu.
    #[test]
    fn un_gros_ecart_de_preampli_arme_la_rampe() {
        let neuf = EqProcessor::prendre_la_releve(Some(egaliseur(3.0)), Some(egaliseur(12.0)))
            .expect("un égaliseur reste monté");
        assert!(neuf.en_fondu());
    }

    /// Un petit cran PENDANT une rampe la reprend là où elle en est.
    #[test]
    fn un_petit_cran_pendant_la_rampe_la_poursuit() {
        let active = EqProcessor::prendre_la_releve(Some(egaliseur(12.0)), None).unwrap();
        assert!(active.en_fondu());
        let neuf = EqProcessor::prendre_la_releve(Some(egaliseur(12.3)), Some(active)).unwrap();
        assert!(
            neuf.en_fondu(),
            "la rampe d'activation ne doit pas sauter à sa fin"
        );
    }

    /// Couper rend un neutre qui ne compte pas comme un égaliseur actif.
    #[test]
    fn couper_rend_un_neutre_en_fondu() {
        let coupe = EqProcessor::prendre_la_releve(None, Some(egaliseur(12.0))).unwrap();
        assert!(coupe.est_neutre() && coupe.en_fondu() && !coupe.is_enabled());
        assert_eq!(coupe.gain_moyen_db(), 0.0);
        assert!(EqProcessor::prendre_la_releve(None, None).is_none());
    }
}
