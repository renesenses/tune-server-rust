//! #4354 — repli à 176,4 kHz d'une conversion DSD → PCM trop lente.
//!
//! Décision de Bertrand du 12/10/2026 : quand le pré-tampon d'une zone DLNA
//! n'est pas atteint pendant qu'elle reçoit un DSD décimé en PCM au-dessus de
//! 176,4 kHz (DSD128 et plus → 352,8 kHz), Tune **retombe à 176,4 kHz** et le
//! dit dans le chemin du signal, au lieu de lancer un son haché.
//!
//! Le constat vient de la barrière de pré-tampon (`initial_prebuffer_done
//! reached=false`), AVANT le `Play` : rien n'a encore été entendu. La piste
//! est alors résolue une seconde fois à 176,4 kHz, et toutes les pistes DSD
//! suivantes de la zone le sont d'emblée. Le repli vit en mémoire, par zone,
//! jusqu'au redémarrage du serveur : un hôte trop lent le reste, et un
//! redémarrage (nouvelle version, autre charge) redonne sa chance à 352,8 kHz.
//!
//! Limite assumée : à 176,4 kHz, un DSD128 se décime par 32 avec un FIR de
//! 512 coefficients, contre 16 et 256 à 352,8 kHz. Le calcul du filtre par
//! seconde d'audio est donc le même ; ce qui est divisé par deux, c'est le
//! débit à produire, écrire et servir (2 116 800 → 1 058 400 octets/s en
//! 24 bits stéréo). Les mesures du .42 (#4354, 19/09) montrent que le FIR
//! n'expliquait pas le 0,60× : la cause, en aval, n'est pas établie.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// La cadence de repli : celle d'un DSD64 décimé, que la famille 44,1 garde.
pub const CADENCE_DE_REPLI_HZ: u32 = 176_400;

/// Le message que `envoyer_a_la_sortie` rend à `play_inner` pour lui dire de
/// résoudre la piste une seconde fois, repli posé. Jamais affiché.
pub const REJOUER_AVEC_LE_REPLI: &str = "__repli_dsd_4354_rejouer__";

/// Un repli posé sur une zone : de quelle cadence, vers laquelle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepliDsd {
    pub de_hz: u32,
    pub vers_hz: u32,
}

impl RepliDsd {
    /// La raison, lisible, que le chemin du signal affiche.
    #[must_use]
    pub fn raison(&self) -> String {
        format!(
            "Repli à {} kHz : la conversion DSD vers {} kHz n'a pas rempli le pré-tampon \
             à temps sur cet hôte",
            khz(self.vers_hz),
            khz(self.de_hz)
        )
    }
}

fn khz(hz: u32) -> String {
    let k = f64::from(hz) / 1000.0;
    // « 176,4 », « 352,8 » : une décimale, virgule française.
    format!("{k:.1}").replace('.', ",")
}

fn registre() -> &'static Mutex<HashMap<i64, RepliDsd>> {
    static R: OnceLock<Mutex<HashMap<i64, RepliDsd>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Faut-il retomber ? Seulement pour un DSD (fichier `.dsf`, `.dff`, `.iso`)
/// servi en WAV/LPCM décimé (pas de DoP, pas de passthrough) au-dessus de
/// 176,4 kHz, quand la barrière n'a pas été atteinte.
#[must_use]
pub fn decider(
    pre_tampon_atteint: bool,
    chemin_source: Option<&str>,
    mime_servi: &str,
    cadence_servie: Option<u32>,
    transport_pcm: bool,
) -> Option<RepliDsd> {
    if pre_tampon_atteint || !transport_pcm {
        return None;
    }
    let source_dsd = chemin_source
        .and_then(|c| std::path::Path::new(c).extension())
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "dsf" | "dff" | "iso"));
    let pcm_servi = mime_servi.to_ascii_lowercase().contains("wav");
    let cadence = cadence_servie?;
    (source_dsd && pcm_servi && cadence > CADENCE_DE_REPLI_HZ).then_some(RepliDsd {
        de_hz: cadence,
        vers_hz: CADENCE_DE_REPLI_HZ,
    })
}

/// Pose le repli de la zone.
pub fn enregistrer(zone_id: i64, repli: RepliDsd) {
    if let Ok(mut r) = registre().lock() {
        r.insert(zone_id, repli);
    }
}

/// Le repli posé sur la zone, s'il y en a un.
#[must_use]
pub fn repli_de_la_zone(zone_id: i64) -> Option<RepliDsd> {
    registre()
        .lock()
        .ok()
        .and_then(|r| r.get(&zone_id).copied())
}

/// Retire le repli de la zone (tests, ou zone supprimée).
pub fn oublier(zone_id: i64) {
    if let Ok(mut r) = registre().lock() {
        r.remove(&zone_id);
    }
}

/// La cadence à servir pour un DSD décimé sur cette zone : `cadence`, ou la
/// cadence de repli si un repli est posé et qu'elle est dépassée.
#[must_use]
pub fn cadence_apres_repli(zone_id: i64, cadence: u32) -> u32 {
    match repli_de_la_zone(zone_id) {
        Some(r) if cadence > r.vers_hz => r.vers_hz,
        _ => cadence,
    }
}

/// Bancs : ralentit la lecture DSD de CE fichier, pour simuler un hôte trop
/// lent sans toucher aux autres tests qui décodent du DSD en parallèle.
#[cfg(test)]
fn freins() -> &'static Mutex<HashMap<String, std::time::Duration>> {
    static F: OnceLock<Mutex<HashMap<String, std::time::Duration>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Bancs : chaque bloc DSD lu dans `chemin` coûtera `pause`.
#[cfg(test)]
pub(crate) fn freiner_pour_test(chemin: &str, pause: std::time::Duration) {
    if let Ok(mut f) = freins().lock() {
        f.insert(chemin.to_string(), pause);
    }
}

/// Bancs : appelé par le lecteur DSD à chaque bloc.
#[cfg(test)]
pub(crate) fn frein_de_test(chemin: &str) {
    let pause = freins().lock().ok().and_then(|f| f.get(chemin).copied());
    if let Some(p) = pause {
        std::thread::sleep(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_dsd128_decime_sous_le_pre_tampon_retombe_a_176_4() {
        assert_eq!(
            decider(false, Some("/m/a.DSF"), "audio/wav", Some(352_800), true),
            Some(RepliDsd {
                de_hz: 352_800,
                vers_hz: 176_400
            })
        );
    }

    #[test]
    fn pas_de_repli_hors_du_cas_vise() {
        // Pré-tampon atteint.
        assert_eq!(
            decider(true, Some("a.dsf"), "audio/wav", Some(352_800), true),
            None
        );
        // DoP : les bits DSD sont emballés, on ne les décime pas.
        assert_eq!(
            decider(false, Some("a.dsf"), "audio/wav", Some(352_800), false),
            None
        );
        // Déjà à 176,4 (DSD64).
        assert_eq!(
            decider(false, Some("a.dsf"), "audio/wav", Some(176_400), true),
            None
        );
        // Une source PCM lente n'est pas concernée.
        assert_eq!(
            decider(false, Some("a.flac"), "audio/wav", Some(352_800), true),
            None
        );
        // DSD servi brut.
        assert_eq!(
            decider(false, Some("a.dsf"), "audio/x-dsf", Some(352_800), true),
            None
        );
    }

    #[test]
    fn la_raison_nomme_les_deux_cadences() {
        let r = RepliDsd {
            de_hz: 352_800,
            vers_hz: 176_400,
        };
        assert_eq!(
            r.raison(),
            "Repli à 176,4 kHz : la conversion DSD vers 352,8 kHz n'a pas rempli le \
             pré-tampon à temps sur cet hôte"
        );
    }

    #[test]
    fn le_repli_plafonne_la_cadence_de_la_zone_seulement() {
        let zone = -4_354_001;
        let autre = -4_354_002;
        enregistrer(
            zone,
            RepliDsd {
                de_hz: 352_800,
                vers_hz: 176_400,
            },
        );
        assert_eq!(cadence_apres_repli(zone, 352_800), 176_400);
        assert_eq!(cadence_apres_repli(zone, 88_200), 88_200);
        assert_eq!(cadence_apres_repli(autre, 352_800), 352_800);
        oublier(zone);
        assert_eq!(cadence_apres_repli(zone, 352_800), 352_800);
    }
}
