//! #2742 — le crossfeed des pistes de la BIBLIOTHÈQUE sur une zone RÉSEAU.
//!
//! Jusqu'ici, sur une zone réseau dont l'opt-in `dsp_progressif_reseau` n'est
//! pas coché (Tades, 0.9.151, zone DLNA vers un renderer Diretta), seuls les
//! flux des services portaient le crossfeed. Une piste de la bibliothèque
//! partait :
//!
//! - **telle quelle** quand le crossfeed était le seul traitement de la zone :
//!   il est tenu hors de `eq_forces_transcode`, qui renvoie au ré-encodage du
//!   fichier ENTIER avant le premier octet (46 à 62 s mesurés, #3357) ;
//! - **ré-encodée en FLAC** par le fichier entier quand un égaliseur, un
//!   convolveur ou un ReplayGain l'imposait — mais `transcode_source_to_file`
//!   ne portait que ces trois étages : le crossfeed restait dehors.
//!
//! Décision de Bertrand (24/09) : « crossfeed toujours, SANS délai ». Trois
//! cas, et jamais une seconde ajoutée au premier son :
//!
//! 1. **un autre traitement ré-encode déjà la piste** : le crossfeed rejoint
//!    ce ré-encodage ([`crossfeed_cuit_dans_le_fichier`]). Même format, même
//!    chemin, même attente qu'avant — seulement un étage de plus ;
//! 2. **crossfeed seul, renderer qui a ANNONCÉ le LPCM** : la piste part en
//!    WAV progressif, le crossfeed appliqué au fil de l'eau par le relais de
//!    LAT-F1 ([`wav_progressif_consenti`]). ⚠️ Le format servi CHANGE sans
//!    l'opt-in : le crossfeed coché vaut consentement, pour ce seul cas ;
//! 3. **crossfeed seul, renderer sans LPCM** (sonde inconcluante comprise) :
//!    rien ne change, la piste part telle quelle. C'est le seul cas où le
//!    statut garde la réserve `network_progressive_off`.
//!
//! Le crossfeed ne s'applique qu'UNE fois : une sortie `local:` l'applique
//! déjà elle-même (le cumul EQ/ReplayGain de v0.9.139, voir
//! `relais_dsp_progressif`), donc aucun des deux cas n'est ouvert pour elle.
//! Les droits (Premium, greffon installé et activé) et le mode PURE passent
//! par `load_crossfeed_processor`, exactement comme sur le chemin des
//! services. « Bit-perfect strict » ne refuse qu'une conversion de fréquence
//! et ne coupe aucun traitement, sur aucun chemin : il ne gouverne donc pas
//! non plus celui-ci.

use super::PlaybackOrchestrator;
use super::regles::traitement_cuit_dans_le_fichier;

/// Cas 2 — l'opt-in du WAV progressif, tel que la décision doit le lire.
///
/// L'opt-in coché vaut pour tout traitement, comme avant. Sans lui, un
/// crossfeed actif sur une zone RÉSEAU vaut consentement au WAV progressif,
/// mais seulement quand il est le SEUL traitement : si un égaliseur, un
/// convolveur ou un ReplayGain ré-encode déjà la piste, c'est le cas 1, et le
/// format servi ne change pas. Le reste des conditions — renderer qui annonce
/// le LPCM, jamais un DSD — reste celui de `cible_wav_pour_traitement`.
pub(super) fn wav_progressif_consenti(
    opt_in: bool,
    crossfeed_reseau: bool,
    autre_traitement: bool,
) -> bool {
    opt_in || (crossfeed_reseau && !autre_traitement)
}

/// Cas 1 — le ré-encodage par le fichier doit-il cuire le crossfeed ?
///
/// Seulement vers une zone RÉSEAU, et jamais vers une sortie locale (qui
/// l'applique elle-même : [`traitement_cuit_dans_le_fichier`]). Le navigateur
/// et les sorties PULL passent aussi par ce bras ; le statut les déclare
/// `non_local_output`, sans mesure qui dise le contraire : ils n'en
/// reçoivent pas.
pub(super) fn crossfeed_cuit_dans_le_fichier(
    sortie_est_reseau: bool,
    sortie_est_locale: bool,
) -> bool {
    sortie_est_reseau && traitement_cuit_dans_le_fichier(sortie_est_locale)
}

/// Le crossfeed ENTRE dans la clé du cache de transcodage.
///
/// Il change les octets encodés, comme l'égaliseur (LAT-F2) : sans ce
/// brassage, une rendition sans crossfeed — celle du pré-chauffage de
/// `queue.rs`, ou d'avant ce correctif — serait servie à une zone qui l'a
/// coché, et inversement. `None` rend la clé d'avant, inchangée.
pub(super) fn empreinte_avec_crossfeed(
    dsp: Option<[u8; 32]>,
    crossfeed: Option<(f32, f32)>,
) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let Some((amount, delay_ms)) = crossfeed else {
        return dsp;
    };
    let mut h = Sha256::new();
    h.update(b"crossfeed\0");
    h.update(amount.to_le_bytes());
    h.update(delay_ms.to_le_bytes());
    if let Some(d) = dsp {
        h.update(b"dsp\0");
        h.update(d);
    }
    Some(h.finalize().into())
}

/// Les étages des greffons natifs tiers ENTRENT dans la clé du cache de
/// transcodage, pour la même raison que le crossfeed. Empreinte vide : la clé
/// d'avant, inchangée.
pub(super) fn empreinte_avec_etages_tiers(dsp: Option<[u8; 32]>, tiers: &str) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    if tiers.is_empty() {
        return dsp;
    }
    let mut h = Sha256::new();
    h.update(b"native-third-party\0");
    h.update(tiers.as_bytes());
    if let Some(d) = dsp {
        h.update(b"dsp\0");
        h.update(d);
    }
    Some(h.finalize().into())
}

impl PlaybackOrchestrator {
    /// Le crossfeed à cuire dans le fichier ré-encodé, avec son réglage pour
    /// la clé du cache. `None` hors réseau, sur une sortie locale, en PURE,
    /// sans les droits du greffon, ou case décochée.
    pub(super) fn crossfeed_du_fichier(
        &self,
        zone_id: i64,
        sample_rate: u32,
        sortie_est_reseau: bool,
        sortie_est_locale: bool,
    ) -> Option<(crate::audio::crossfeed::CrossfeedProcessor, (f32, f32))> {
        if !crossfeed_cuit_dans_le_fichier(sortie_est_reseau, sortie_est_locale) {
            return None;
        }
        let processeur = self.load_crossfeed_processor(zone_id, sample_rate)?;
        // Un étage casque sans crossfeed intégré (greffons natifs tiers seuls)
        // entre dans la clé sous le réglage nul `(0, 0)`, qu'aucun crossfeed
        // actif ne produit (`amount == 0` rend `None`) ; les étages tiers y
        // entrent par [`empreinte_avec_etages_tiers`].
        let reglage = self.crossfeed_configure(zone_id).unwrap_or((0.0, 0.0));
        Some((processeur, reglage))
    }
}

#[cfg(test)]
mod tests;
