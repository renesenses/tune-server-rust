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
//!
//! Trois gardes, posées à la relecture de la PR #8567 :
//! - **sans mesure, pas de bascule** (macOS, Windows, `/proc` illisible) :
//!   un seuil fixe faisait basculer une 192 kHz de 12 min sur 32 Gio ;
//! - **un rendu déjà en cache est servi tel quel** : il ne coûte aucune
//!   mémoire (vérifié par l'appelant, `resolve_local.rs`) ;
//! - **le renderer doit accepter le WAV** : [`profondeur_de_bascule`].

use crate::audio::formats::AudioFormat;

/// Octets d'un échantillon décodé (`i32`) dans le bras fichier.
const OCTETS_PAR_ECHANTILLON: u64 = 4;

/// Pic du bras fichier rapporté à la piste décodée en `i32`. Mesuré après le
/// volet A par `tests/transcodage_memoire_bornee_8558.rs` : sous 2.
const FACTEUR_DE_PIC: u64 = 2;

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
/// suivante (pré-chargement gapless) vivent à côté. Sans mesure, NON : on
/// ne bascule pas à l'aveugle (rien ne change là où rien ne se mesure).
pub(super) fn trop_lourd_pour_le_bras_fichier(pic: u64, disponible: Option<u64>) -> bool {
    match disponible {
        Some(d) => pic > d / 2,
        None => false,
    }
}

/// La profondeur du WAV de bascule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProfondeurDeBascule {
    /// Celle que les branches habituelles de `decider_le_format_de_sortie`
    /// donnent à une cible WAV (24 bits au plus).
    Native,
    /// 16 bits imposés : le renderer n'a annoncé que le LPCM 16 bits.
    Seize,
}

/// Le WAV que ce renderer accepte, ou `None` : pas de bascule, le bras
/// fichier actuel reste la voie.
///
/// - `deja_wav` : la cible était DÉJÀ du WAV (`dlna_needs_wav`) — le renderer
///   le reçoit aujourd'hui par le fichier, seule la voie change ;
/// - source plus profonde que 16 bits : 24 bits seulement si le renderer
///   annonce le WAV/L24 (`accepte_hi_res`, sonde `dlna_accepte_lpcm`) ou si
///   la zone a armé `dlna_wav24`, et jamais sous `dlna_cap_16bit` ; sinon
///   16 bits si le LPCM 16 bits est annoncé (`accepte_16`) ; sinon rien —
///   un DLNA « FLAC seulement » ne reçoit pas de WAV (famille #1137) ;
/// - source 16 bits : le LPCM 16 bits annoncé suffit.
///
/// Un Chromecast n'a pas de Sink : `dlna_accepte_lpcm` répond par son type
/// (`capacite_lpcm_par_sortie`), « WAV (LPCM) » documenté par Google sans
/// profondeur — donc 16 bits, jamais 24.
pub(super) fn profondeur_de_bascule(
    deja_wav: bool,
    source_hi_res: bool,
    dlna_cap_16bit: bool,
    dlna_wav24: bool,
    accepte_hi_res: bool,
    accepte_16: bool,
) -> Option<ProfondeurDeBascule> {
    if deja_wav {
        Some(ProfondeurDeBascule::Native)
    } else if !source_hi_res {
        accepte_16.then_some(ProfondeurDeBascule::Native)
    } else if !dlna_cap_16bit && (dlna_wav24 || accepte_hi_res) {
        Some(ProfondeurDeBascule::Native)
    } else if accepte_16 {
        Some(ProfondeurDeBascule::Seize)
    } else {
        None
    }
}

/// Cadence la plus haute qu'on ose servir en WAV à un Chromecast : celle que
/// Google documente pour son décodage sans perte (FLAC 96 kHz / 24 bits).
/// Au-delà, rien ne l'établit pour le WAV : pas de bascule.
pub(super) const CADENCE_MAX_CHROMECAST: u32 = 96_000;

#[cfg(test)]
thread_local! {
    /// Mémoire disponible imposée par un test, sur son seul fil.
    static MEMOIRE_IMPOSEE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Impose la mémoire disponible vue par la bascule, sur le fil du test.
#[cfg(test)]
pub(super) fn imposer_la_memoire_disponible(octets: Option<u64>) {
    MEMOIRE_IMPOSEE.with(|m| m.set(octets));
}

/// La mémoire disponible que la bascule consulte.
pub(super) fn memoire_disponible() -> Option<u64> {
    #[cfg(test)]
    if let Some(m) = MEMOIRE_IMPOSEE.with(|m| m.get()) {
        return Some(m);
    }
    crate::chemins_de_travail::memoire_disponible()
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
    fn sans_mesure_pas_de_bascule_8558() {
        // macOS / Windows : une 192 kHz de 12 min restait au bras fichier
        // seulement sous un seuil fixe ; désormais, sans mesure, jamais.
        assert!(!basculer_en_progressif(
            true,
            AudioFormat::Flac,
            12 * 60_000,
            192_000,
            2,
            None
        ));
        assert!(!basculer_en_progressif(
            true,
            AudioFormat::Flac,
            3 * 3_600_000,
            384_000,
            2,
            None
        ));
    }

    #[test]
    fn le_renderer_doit_accepter_le_wav_8558() {
        use ProfondeurDeBascule::*;
        // Cible déjà WAV : seule la voie change.
        assert_eq!(
            profondeur_de_bascule(true, true, false, false, false, false),
            Some(Native)
        );
        // DLNA « FLAC seulement » (ni WAV, ni L16/L24, ni opt-in) : rien.
        assert_eq!(
            profondeur_de_bascule(false, true, false, false, false, false),
            None
        );
        assert_eq!(
            profondeur_de_bascule(false, false, false, false, false, false),
            None
        );
        // WAV/L24 annoncé : 24 bits.
        assert_eq!(
            profondeur_de_bascule(false, true, false, false, true, true),
            Some(Native)
        );
        // Opt-in `dlna_wav24` : 24 bits.
        assert_eq!(
            profondeur_de_bascule(false, true, false, true, false, false),
            Some(Native)
        );
        // L16 seul (ou Chromecast) : 16 bits.
        assert_eq!(
            profondeur_de_bascule(false, true, false, false, false, true),
            Some(Seize)
        );
        // `dlna_cap_16bit` l'emporte sur le L24 annoncé.
        assert_eq!(
            profondeur_de_bascule(false, true, true, true, true, true),
            Some(Seize)
        );
        assert_eq!(
            profondeur_de_bascule(false, true, true, false, true, false),
            None
        );
        // Source 16 bits : le LPCM 16 bits suffit.
        assert_eq!(
            profondeur_de_bascule(false, false, false, false, false, true),
            Some(Native)
        );
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
