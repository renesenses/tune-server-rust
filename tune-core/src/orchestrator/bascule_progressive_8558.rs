//! #8558 (volet B) — une piste trop lourde pour le bras FICHIER part en
//! session WAV progressive.
//!
//! Forum, fil 2777 : un FLAC 192/24 vers un Chromecast Audio (cible FLAC,
//! donc bras fichier : la piste entière décodée en mémoire avant le premier
//! octet) faisait tuer le serveur par le noyau sur une VM de 4 Gio sans swap.
//! Le volet A borne les copies du bras fichier, mais sa mémoire reste
//! proportionnelle à la durée et à la cadence : une piste assez longue la
//! dépasse toujours.
//!
//! Le bras progressif, lui, ne matérialise jamais la piste (décodage et
//! rééchantillonnage au fil de l'eau, traitement de zone par le relais
//! `spawn_streaming_dsp_relay`, Content-Length exact tiré de la durée). C'est
//! celui que le DSD emprunte déjà vers une zone réseau — sur le même
//! Chromecast Audio, le DSD passait. On l'emprunte donc dès que le pic
//! estimé du bras fichier dépasse la moitié de la mémoire disponible.

use crate::audio::formats::AudioFormat;

/// Octets d'un échantillon décodé (`i32`) dans le bras fichier.
const OCTETS_PAR_ECHANTILLON: u64 = 4;

/// Pic du bras fichier rapporté à la piste décodée en `i32`. Mesuré après le
/// volet A par `tests/transcodage_memoire_bornee_8558.rs` : sous 2.
const FACTEUR_DE_PIC: u64 = 2;

/// Sans mesure de la mémoire disponible (macOS, Windows), au-delà de ce pic
/// estimé on bascule quand même : 2 Gio, soit ~22 min de 192 kHz stéréo.
pub(super) const PIC_MAX_SANS_MESURE: u64 = 2 << 30;

/// Le pic de mémoire estimé du bras fichier pour une piste de `duree_ms` à
/// `cadence` Hz décodée sur `canaux` canaux.
pub(super) fn pic_estime_du_bras_fichier(duree_ms: u64, cadence: u32, canaux: u16) -> u64 {
    duree_ms
        .saturating_mul(u64::from(cadence))
        .saturating_mul(u64::from(canaux))
        / 1000
        * OCTETS_PAR_ECHANTILLON
        * FACTEUR_DE_PIC
}

/// Le pic dépasse-t-il ce que la machine peut donner ? La moitié de la
/// mémoire disponible au plus : le reste du serveur, le cache et la piste
/// suivante (pré-chargement gapless) vivent à côté.
pub(super) fn trop_lourd_pour_le_bras_fichier(pic: u64, disponible: Option<u64>) -> bool {
    match disponible {
        Some(d) => pic > d / 2,
        None => pic > PIC_MAX_SANS_MESURE,
    }
}

/// La décision entière, pure : une sortie RÉSEAU, une source qui n'est pas
/// du DSD (il part déjà en WAV progressif), et un pic trop lourd.
pub(super) fn basculer_en_progressif(
    is_network_output: bool,
    source: AudioFormat,
    duree_ms: i64,
    cadence_decodee: u32,
    canaux: u16,
    disponible: Option<u64>,
) -> bool {
    is_network_output
        && source != AudioFormat::Dsd
        && duree_ms > 0
        && trop_lourd_pour_le_bras_fichier(
            pic_estime_du_bras_fichier(duree_ms as u64, cadence_decodee, canaux),
            disponible,
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIO: u64 = 1 << 30;

    /// Le cas du fil 2777 : FLAC 192/24 stéréo long (15 min) vers un
    /// Chromecast, VM de 4 Gio dont ~3,3 Gio disponibles.
    #[test]
    fn un_flac_192_long_sur_une_vm_de_4_gio_bascule_8558() {
        let dispo = Some(33 * GIO / 10);
        assert!(basculer_en_progressif(
            true,
            AudioFormat::Flac,
            15 * 60_000,
            192_000,
            2,
            dispo
        ));
        // Contre-épreuve : la même piste sur une machine de 16 Gio reste au
        // bras fichier (rien ne change là où rien ne cassait).
        assert!(!basculer_en_progressif(
            true,
            AudioFormat::Flac,
            15 * 60_000,
            192_000,
            2,
            Some(14 * GIO)
        ));
    }

    #[test]
    fn un_cd_de_quatre_minutes_ne_bascule_jamais_8558() {
        assert!(!basculer_en_progressif(
            true,
            AudioFormat::Flac,
            4 * 60_000,
            44_100,
            2,
            Some(GIO)
        ));
        assert!(!basculer_en_progressif(
            true,
            AudioFormat::Flac,
            4 * 60_000,
            44_100,
            2,
            None
        ));
    }

    #[test]
    fn ni_sortie_locale_ni_dsd_ni_duree_inconnue_8558() {
        let peu = Some(64 << 20);
        assert!(!basculer_en_progressif(
            false,
            AudioFormat::Flac,
            600_000,
            192_000,
            2,
            peu
        ));
        assert!(!basculer_en_progressif(
            true,
            AudioFormat::Dsd,
            600_000,
            176_400,
            2,
            peu
        ));
        assert!(!basculer_en_progressif(
            true,
            AudioFormat::Flac,
            0,
            192_000,
            2,
            peu
        ));
        assert!(!basculer_en_progressif(
            true,
            AudioFormat::Flac,
            -1,
            192_000,
            2,
            peu
        ));
    }

    #[test]
    fn sans_mesure_le_plafond_fixe_s_applique_8558() {
        // 192 kHz stéréo : 3 072 000 o/s de pic estimé ; 2 Gio ≈ 699 s.
        assert!(!basculer_en_progressif(
            true,
            AudioFormat::Flac,
            600_000,
            192_000,
            2,
            None
        ));
        assert!(basculer_en_progressif(
            true,
            AudioFormat::Flac,
            800_000,
            192_000,
            2,
            None
        ));
    }

    #[test]
    fn le_pic_estime_ne_deborde_pas_8558() {
        assert_eq!(
            pic_estime_du_bras_fichier(1000, 192_000, 2),
            192_000 * 2 * 4 * 2
        );
        let _ = pic_estime_du_bras_fichier(u64::MAX, u32::MAX, u16::MAX);
    }
}
