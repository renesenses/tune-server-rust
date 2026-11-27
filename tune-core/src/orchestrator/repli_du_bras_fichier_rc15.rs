//! rc15 — un fichier refait trop lent ne laisse plus la zone muette.
//!
//! Le .18, rc14 officielle, zone 10 (Eversolo DMP-A8 en DLNA) réglée sur
//! `zone_10_fixed_sample_rate=768000`, FLAC 44,1 kHz : le lecteur annonce le
//! FLAC, la cible est donc le FLAC refait par le BRAS FICHIER, la piste
//! entière décodée, rééchantillonnée à 768 kHz, encodée et écrite avant le
//! premier octet. Cet hôte décode à ×1,93 le temps réel : une piste de 4 min
//! demandait plus que ses 123 s de budget. Le journal :
//!
//! ```text
//! transcode_required … target=Flac sample_rate=768000 motifs=["needs_downsample"]
//! transcode_to_temp_file_start … transcode_budget_selected budget_s=123
//! transcode_timeout_host_too_slow … host_realtime_factor=1.93
//! play_resolution_failed zone_id=10 error=transcode timeout after 123s …
//! ```
//!
//! puis plus AUCUNE commande DLNA : ni `SetAVTransportURI`, ni `Play`. Entre
//! deux, chaque nouvel appui sur Lecture préemptait le fichier en cours
//! (#3444) et en relançait un autre, tout aussi lent. La ligne
//! `pretranscode_abandonne_fichiers_nettoyes` du même journal est celle d'un
//! pré-transcodage Qobuz (192 → 768 kHz) lancé trois minutes plus tôt, qu'une
//! lecture plus récente avait déjà remplacé : elle n'est que le ménage de
//! #6718, pas la cause.
//!
//! Le correctif tient en trois temps, tous bornés au cas où le fichier refait
//! RÉÉCHANTILLONNE (cadence fixe ou plafond de zone) une source PCM :
//!
//! 1. **avant** de lancer le fichier, la mesure retenue sur cet hôte (même
//!    cadence source, même cadence cible) projette le délai : s'il ne tient
//!    pas, on part directement sur le repli ;
//! 2. **pendant**, les premières secondes mesurent le facteur
//!    (`BudgetAdaptatif::delai_max`, `FinDeTranscodage::TropLent`) ;
//! 3. **après** un abandon (mesure, budget), le repli au lieu du silence.
//!
//! Le repli, dans l'ordre :
//! - le **flux WAV progressif** à la MÊME cadence (le relais de #8558, déjà
//!   servi aux DSD et aux pistes trop lourdes), si le lecteur annonce le LPCM
//!   et que l'hôte tient le temps réel avec marge ;
//! - sinon la **cadence native** de la piste (plafond de zone gardé), sans
//!   la cadence fixe.
//!
//! Aucun étage de gain n'est ajouté : les deux replis appliquent la chaîne de
//! zone qui existait déjà (égaliseur, ReplayGain, compensation, tous ≤ 1).
//!
//! Le seuil de 3 s est celui de la PR #9602 pour le fichier refait du
//! suréchantillonneur (`DELAI_MAX_AVANT_LA_PREMIERE_NOTE_S`, décision du
//! 26/11) : la première note n'attend jamais plus que cela quand un flux
//! progressif peut la servir tout de suite.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Le délai au-delà duquel on n'attend pas le fichier refait quand le flux
/// progressif peut servir la même cadence tout de suite. Même valeur que la
/// PR #9602 (`surechantillonnage::DELAI_MAX_AVANT_LA_PREMIERE_NOTE_S`).
pub(super) const DELAI_MAX_AVANT_LA_PREMIERE_NOTE_S: f64 = 3.0;

/// Le facteur temps réel en dessous duquel le flux progressif à la cadence
/// demandée ne tiendrait pas (le décodeur doit devancer le lecteur, avec une
/// marge pour le reste du serveur). En dessous : cadence native.
pub(super) const MARGE_TEMPS_REEL_DU_FLUX: f64 = 1.25;

/// Le préfixe de l'erreur par laquelle `transcoder_vers_fichier` rend la
/// main quand un repli est possible : `transcoder_la_piste` le reconnaît.
pub(super) const BRAS_FICHIER_A_REPLIER: &str = "bras_fichier_trop_lent_repli: ";

/// Ce qui sert la piste quand le fichier refait ne tient pas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Repli {
    /// Le WAV progressif, à la même cadence et à cette profondeur.
    FluxProgressif(super::bascule_progressive_8558::ProfondeurDeBascule),
    /// La cadence native de la piste (plafond de zone gardé).
    CadenceNative,
}

impl Repli {
    /// Le nom stable du repli, pour le journal et l'avis de zone.
    pub(super) fn code(self) -> &'static str {
        match self {
            Repli::FluxProgressif(_) => "progressive_stream",
            Repli::CadenceNative => "native_rate",
        }
    }
}

/// Le repli à prendre : le flux progressif si le lecteur accepte le WAV
/// (`profondeur_wav`) et que l'hôte tient le temps réel avec marge (ou qu'on
/// ne l'a pas mesuré) ; sinon la cadence native.
pub(super) fn repli_a_prendre(
    profondeur_wav: Option<super::bascule_progressive_8558::ProfondeurDeBascule>,
    facteur: Option<f64>,
) -> Repli {
    match profondeur_wav {
        Some(p) if facteur.is_none_or(|f| f >= MARGE_TEMPS_REEL_DU_FLUX) => {
            Repli::FluxProgressif(p)
        }
        _ => Repli::CadenceNative,
    }
}

/// Le délai accordé au fichier refait avant de prendre le repli : 3 s quand
/// le flux progressif peut servir la même cadence, sinon `None` (le budget
/// de #3140 décide, et son dépassement mène à la cadence native).
pub(super) fn delai_max(flux_possible: bool) -> Option<Duration> {
    flux_possible.then(|| Duration::from_secs_f64(DELAI_MAX_AVANT_LA_PREMIERE_NOTE_S))
}

/// Jugé AVANT de lancer : le fichier d'une piste de `piste_s` secondes, à
/// `facteur` fois le temps réel, tiendrait-il dans `delai` ? `None` (rien de
/// mesuré, durée inconnue) : on lance, et la mesure des premières secondes
/// tranchera.
pub(super) fn ne_tient_pas_d_avance(piste_s: f64, facteur: Option<f64>, delai: Duration) -> bool {
    match facteur {
        Some(f) if f > 0.0 && piste_s > 0.0 => piste_s / f > delai.as_secs_f64(),
        _ => false,
    }
}

/// Le facteur temps réel du fichier refait mesuré sur CET hôte, par couple
/// (cadence source, cadence cible) : le coût du rééchantillonnage en dépend.
/// Vit le temps du processus.
fn facteurs() -> &'static Mutex<HashMap<(u32, u32), f64>> {
    static F: OnceLock<Mutex<HashMap<(u32, u32), f64>>> = OnceLock::new();
    F.get_or_init(Default::default)
}

/// Retient la dernière mesure pour `(de, vers)`.
pub(super) fn retenir_le_facteur(de: u32, vers: u32, facteur: f64) {
    if facteur.is_finite() && facteur > 0.0 {
        facteurs().lock().unwrap().insert((de, vers), facteur);
    }
}

/// La dernière mesure retenue pour `(de, vers)`.
pub(super) fn facteur_retenu(de: u32, vers: u32) -> Option<f64> {
    facteurs().lock().unwrap().get(&(de, vers)).copied()
}

#[cfg(test)]
mod tests {
    use super::super::bascule_progressive_8558::ProfondeurDeBascule;
    use super::*;

    #[test]
    fn le_flux_progressif_d_abord_si_le_lecteur_prend_le_wav_et_que_l_hote_tient() {
        let p = Some(ProfondeurDeBascule::Native);
        assert_eq!(
            repli_a_prendre(p, Some(1.93)),
            Repli::FluxProgressif(ProfondeurDeBascule::Native),
            "le .18 (×1,93) tient le flux à 768 kHz"
        );
        assert_eq!(
            repli_a_prendre(p, None),
            Repli::FluxProgressif(ProfondeurDeBascule::Native)
        );
        assert_eq!(
            repli_a_prendre(p, Some(1.1)),
            Repli::CadenceNative,
            "sous la marge, le flux progressif calerait : cadence native"
        );
        assert_eq!(
            repli_a_prendre(None, Some(10.0)),
            Repli::CadenceNative,
            "un lecteur FLAC seul ne reçoit jamais de WAV"
        );
    }

    #[test]
    fn le_jugement_d_avance_ne_tranche_que_sur_une_mesure() {
        let d = delai_max(true).unwrap();
        assert_eq!(d, Duration::from_secs(3));
        assert!(ne_tient_pas_d_avance(237.6, Some(1.93), d), "le .18");
        assert!(!ne_tient_pas_d_avance(237.6, Some(100.0), d));
        assert!(!ne_tient_pas_d_avance(237.6, None, d));
        assert!(!ne_tient_pas_d_avance(0.0, Some(0.5), d));
        assert_eq!(delai_max(false), None);
    }

    #[test]
    fn la_mesure_est_retenue_par_couple_de_cadences() {
        retenir_le_facteur(44_101, 768_001, 1.93);
        retenir_le_facteur(44_101, 768_001, f64::NAN);
        assert_eq!(facteur_retenu(44_101, 768_001), Some(1.93));
        assert_eq!(facteur_retenu(96_001, 768_001), None);
    }
}
