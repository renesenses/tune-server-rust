//! rc14 — le suréchantillonnage PCM sur une sortie RÉSEAU de la famille UPnP
//! (DLNA, OpenHome).
//!
//! Jusqu'ici le greffon `upsampler` ne tournait que sur la sortie locale, et
//! TESS l'éteignait sur toute zone réseau (`not_applied_on_transport`). Il est
//! désormais porté par le **fichier refait** (`transcodage`) d'une piste de la
//! bibliothèque :
//!
//! - **cadence cible** : la plus haute que le lecteur ANNONCE dans son
//!   `GetProtocolInfo` (`rate=` des entrées FLAC, ou WAV/L16/L24 selon ce que
//!   Tune lui servira), multiple entier de la source, sous les plafonds du
//!   réglage, de la zone et du test de la machine
//!   ([`crate::audio::surechantillonnage::cible_reseau`]) ;
//! - **format de transport** : la règle existante du fichier refait (FLAC si
//!   le lecteur lit le FLAC, WAV sinon). Une zone dont l'opt-in du relais
//!   progressif est armé part, pour une piste suréchantillonnée, par le
//!   fichier refait (le relais progressif ne porte pas encore l'étage) ;
//! - **refus motivés**, publiés par TESS (`delivery.upsampling`) et par le
//!   chemin du signal : PURE, source DSD, lecteur muet sur ses cadences,
//!   lecteur sans sonde (BluOS, Chromecast, Squeezebox), service ou radio,
//!   flux progressif imposé par le format, bit-perfect strict.
//!
//! La DÉCISION de la dernière piste résolue est gardée par zone
//! ([`PlaybackOrchestrator::surechantillonnage_reseau_de_la_zone`]) : la route TESS et le chemin du signal la lisent sans
//! sonder le lecteur.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

use super::PlaybackOrchestrator;
use crate::audio::formats::AudioFormat;
use crate::audio::surechantillonnage::{self as su, UpsamplerSettings, refus_reseau};

/// L'étage décidé pour une piste : réglages du greffon, cadences d'entrée et
/// de sortie.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SurechPlanifie {
    pub(crate) reglages: UpsamplerSettings,
    pub(crate) de: u32,
    pub(crate) vers: u32,
    /// Les cadences que le lecteur annonce (triées, sans doublon).
    pub(crate) proposees: Vec<u32>,
    /// La cible a été lue pour un fichier FLAC (sinon WAV/LPCM).
    pub(crate) flac: bool,
    /// Le verdict du test de la machine : de quoi estimer le délai du
    /// fichier refait (`delai_estime_du_fichier_s`).
    pub(crate) verdict: su::VerdictMachine,
}

impl SurechPlanifie {
    /// Ce que l'étage change au fichier servi, pour sa clé de cache : le
    /// filtre et les deux cadences (la cible est déjà dans la clé, la source
    /// et le filtre non).
    pub(crate) fn empreinte(&self, dsp: Option<[u8; 32]>) -> Option<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"surechantillonnage\0");
        h.update(self.reglages.filter.as_bytes());
        h.update(self.de.to_le_bytes());
        h.update(self.vers.to_le_bytes());
        if let Some(d) = dsp {
            h.update(b"dsp\0");
            h.update(d);
        }
        Some(h.finalize().into())
    }

    /// La réserve fixe du filtre à ce rapport, en dB (≤ 0).
    pub(crate) fn reserve_db(&self) -> f64 {
        su::greffon::headroom_db(&self.reglages.filter, self.vers / self.de.max(1)).unwrap_or(0.0)
    }
}

/// L'état publié du suréchantillonnage réseau d'une zone (`delivery.upsampling`
/// de `GET /zones/{id}/tess`, `upsampling` du chemin du signal).
pub use crate::tess::reseau_upnp::SurechantillonnageUpnp as EtatSurechReseau;

/// L'état publié d'un étage appliqué.
pub(crate) fn etat_applique(p: &SurechPlanifie) -> EtatSurechReseau {
    EtatSurechReseau {
        active: true,
        reason: None,
        from_hz: Some(p.de),
        to_hz: Some(p.vers),
        filter: Some(p.reglages.filter.clone()),
        headroom_db: Some((p.reserve_db() * 100.0).round() / 100.0),
        offered_rates_hz: p.proposees.clone(),
    }
}

fn miroir() -> &'static std::sync::Mutex<std::collections::HashMap<i64, EtatSurechReseau>> {
    static M: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<i64, EtatSurechReseau>>,
    > = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

fn miroir_pour_le_chemin_du_signal(zone_id: i64, etat: Option<EtatSurechReseau>) {
    let mut m = miroir().lock().unwrap_or_else(|e| e.into_inner());
    match etat {
        Some(e) => {
            m.insert(zone_id, e);
        }
        None => {
            m.remove(&zone_id);
        }
    }
}

/// Le chemin du signal (`GET /zones/{id}/signal-path`) n'a pas
/// l'orchestrateur : il lit ce miroir, tenu par
/// [`PlaybackOrchestrator::noter_le_surechantillonnage_reseau`] (un seul
/// orchestrateur par processus).
pub fn etat_publie_pour_le_chemin_du_signal(zone_id: i64) -> Option<EtatSurechReseau> {
    miroir()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&zone_id)
        .cloned()
}

impl PlaybackOrchestrator {
    /// Garde la décision de la dernière piste résolue pour la zone. `None` :
    /// suréchantillonneur désactivé, rien à dire.
    pub(crate) fn noter_le_surechantillonnage_reseau(
        &self,
        zone_id: i64,
        etat: Option<EtatSurechReseau>,
    ) {
        miroir_pour_le_chemin_du_signal(zone_id, etat.clone());
        let mut m = self.surech_reseau.lock().unwrap_or_else(|e| e.into_inner());
        match etat {
            Some(e) => {
                m.insert(zone_id, e);
            }
            None => {
                m.remove(&zone_id);
            }
        }
    }

    /// La décision de la dernière piste de la bibliothèque résolue pour la
    /// zone (`None` : désactivé, ou rien résolu depuis le démarrage).
    pub fn surechantillonnage_reseau_de_la_zone(&self, zone_id: i64) -> Option<EtatSurechReseau> {
        self.surech_reseau
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&zone_id)
            .cloned()
    }
}

/// Les cadences explicitement annoncées (`rate=`) par les entrées du Sink
/// dont le MIME de base est celui que Tune servira : `audio/flac` (et
/// `audio/x-flac`) pour un fichier FLAC, `audio/wav`, `audio/L16` ou
/// `audio/L24` pour du WAV/LPCM. Triées, sans doublon. Une entrée sans
/// `rate=` ne dit rien : elle est ignorée.
pub fn cadences_annoncees(sink: &[String], flac: bool) -> Vec<u32> {
    let mut v: Vec<u32> = sink
        .iter()
        .filter_map(|entree| {
            let champs: Vec<&str> = entree.splitn(4, ':').collect();
            if champs.len() < 3 {
                return None;
            }
            let mut params = champs[2].split(';');
            let base = params
                .next()?
                .trim()
                .to_ascii_lowercase()
                .replace("audio/x-", "audio/");
            let du_format = if flac {
                base == "audio/flac"
            } else {
                matches!(base.as_str(), "audio/wav" | "audio/l16" | "audio/l24")
            };
            if !du_format {
                return None;
            }
            Some(params.filter_map(|p| {
                let (nom, valeur) = p.trim().split_once('=')?;
                if !nom.trim().eq_ignore_ascii_case("rate") {
                    return None;
                }
                valeur.trim().parse::<u32>().ok().filter(|r| *r > 0)
            }))
        })
        .flatten()
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Le type de zone porte-t-il une sonde de cadences (`GetProtocolInfo` pour
/// DLNA, `Playlist:1 ProtocolInfo` pour OpenHome) ?
pub(crate) fn type_avec_sonde_de_cadences(type_de_zone: Option<&str>) -> bool {
    use crate::tess::TransportZone as T;
    matches!(
        T::depuis_type_de_sortie(type_de_zone),
        T::Dlna | T::OpenHome
    )
}

/// Note un refus `code` pour la zone, seulement si son suréchantillonneur est
/// activé (désactivé : rien à dire). PURE et les autres écarts du réglage
/// gardent leur propre code.
pub(crate) fn noter_un_refus_si_demande(
    orch: &PlaybackOrchestrator,
    zone_id: i64,
    code: &'static str,
) {
    let pure = orch.zone_audiophile(zone_id);
    match orch.decision_du_surechantillonneur(zone_id, pure) {
        Err(su::Ecarte::Desactive) => orch.noter_le_surechantillonnage_reseau(zone_id, None),
        Err(e) => orch.noter_le_surechantillonnage_reseau(
            zone_id,
            Some(EtatSurechReseau::refuse(e.code(), Vec::new())),
        ),
        Ok(_) => {
            tracing::info!(zone_id, reason = code, "upsampler_network_not_engaged");
            orch.noter_le_surechantillonnage_reseau(
                zone_id,
                Some(EtatSurechReseau::refuse(code, Vec::new())),
            );
        }
    }
}

/// Ce que [`PlaybackOrchestrator::decider_le_surechantillonnage_reseau`] lit
/// de la piste et de la zone.
pub(crate) struct DemandeSurechReseau<'a> {
    pub(crate) zone_id: i64,
    pub(crate) type_de_zone: Option<&'a str>,
    pub(crate) device_id: &'a str,
    pub(crate) source_format: Option<AudioFormat>,
    pub(crate) source_hz: u32,
    pub(crate) plafond_zone: Option<u32>,
    /// Tune servira du FLAC (sinon du WAV/LPCM).
    pub(crate) flac: bool,
}

impl PlaybackOrchestrator {
    /// Les cadences que le lecteur annonce pour le format servi, ou `None` :
    /// sortie hors registre, sans Sink (ni DLNA ni OpenHome), ou sonde
    /// inconcluante.
    pub(crate) async fn cadences_du_lecteur(
        &self,
        device_id: &str,
        flac: bool,
    ) -> Option<Vec<u32>> {
        #[cfg(test)]
        if let Some(sink) = sink_de_test(device_id) {
            return Some(cadences_annoncees(&sink, flac));
        }
        if device_id.is_empty() {
            return None;
        }
        let arc = { self.outputs.lock().await.get(device_id) }?;
        let caps = {
            let locked = arc.lock().await;
            let sortie = locked.as_any();
            if let Some(dlna) = sortie.downcast_ref::<crate::outputs::dlna::DlnaOutput>() {
                dlna.probe_capabilities().await
            } else {
                sortie
                    .downcast_ref::<crate::outputs::openhome::OpenHomeOutput>()?
                    .probe_capabilities()
                    .await
            }
        };
        if !caps.probed {
            return None;
        }
        Some(cadences_annoncees(&caps.sink, flac))
    }

    /// LA décision du suréchantillonnage d'une piste de la bibliothèque vers
    /// une zone de la famille UPnP. `None` : rien à poser (désactivé, ou refus
    /// motivé, gardé pour l'état publié).
    pub(crate) async fn decider_le_surechantillonnage_reseau(
        &self,
        d: DemandeSurechReseau<'_>,
    ) -> Option<SurechPlanifie> {
        let resultat = self.decision_reseau(&d).await;
        match &resultat {
            Ok(p) => {
                tracing::info!(
                    zone_id = d.zone_id,
                    de_hz = p.de,
                    vers_hz = p.vers,
                    filtre = %p.reglages.filter,
                    flac = d.flac,
                    "upsampler_network_engaged"
                );
                self.noter_le_surechantillonnage_reseau(d.zone_id, Some(etat_applique(p)));
            }
            Err(None) => self.noter_le_surechantillonnage_reseau(d.zone_id, None),
            Err(Some((code, proposees))) => {
                tracing::info!(
                    zone_id = d.zone_id,
                    reason = code,
                    rates = ?proposees,
                    "upsampler_network_not_engaged"
                );
                self.noter_le_surechantillonnage_reseau(
                    d.zone_id,
                    Some(EtatSurechReseau::refuse(code, proposees.clone())),
                );
            }
        }
        resultat.ok()
    }

    /// `Err(None)` : désactivé ; `Err(Some(code, cadences))` : refus motivé.
    #[allow(clippy::type_complexity)]
    async fn decision_reseau(
        &self,
        d: &DemandeSurechReseau<'_>,
    ) -> Result<SurechPlanifie, Option<(&'static str, Vec<u32>)>> {
        let pure = self.zone_audiophile(d.zone_id);
        let choix = match self.decision_du_surechantillonneur(d.zone_id, pure) {
            Ok(c) => c,
            Err(su::Ecarte::Desactive) => return Err(None),
            Err(e) => return Err(Some((e.code(), Vec::new()))),
        };
        if d.source_format == Some(AudioFormat::Dsd) {
            return Err(Some((refus_reseau::DSD, Vec::new())));
        }
        if crate::audio::bitperfect_strict::zone_enabled(&self.db, d.zone_id) {
            return Err(Some((refus_reseau::STRICT, Vec::new())));
        }
        if !type_avec_sonde_de_cadences(d.type_de_zone) {
            return Err(Some((refus_reseau::LECTEUR_SANS_SONDE, Vec::new())));
        }
        let proposees = self
            .cadences_du_lecteur(d.device_id, d.flac)
            .await
            .unwrap_or_default();
        match su::cible_reseau(&choix, d.source_hz, &proposees, d.plafond_zone) {
            Ok(vers) => Ok(SurechPlanifie {
                verdict: choix.verdict,
                reglages: choix.reglages,
                de: d.source_hz,
                vers,
                proposees,
                flac: d.flac,
            }),
            Err(code) => Err(Some((code, proposees))),
        }
    }
}

#[cfg(test)]
fn sinks_de_test() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static M: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// Tests : le Sink que le lecteur `device_id` « annonce », sans SOAP.
#[cfg(test)]
pub(crate) fn poser_le_sink_de_test(device_id: &str, sink: &[&str]) {
    sinks_de_test().lock().unwrap().insert(
        device_id.to_owned(),
        sink.iter().map(|s| (*s).to_owned()).collect(),
    );
}

#[cfg(test)]
fn sink_de_test(device_id: &str) -> Option<Vec<String>> {
    sinks_de_test().lock().unwrap().get(device_id).cloned()
}

#[cfg(test)]
#[path = "surechantillonnage_reseau_tests.rs"]
mod tests;
