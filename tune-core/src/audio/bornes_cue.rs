//! #8968 (PR 2) — les bornes d'une tranche CUE, à l'échantillon près.
//!
//! Une feuille CUE place ses pistes en `mm:ss:ff`, où `ff` compte des
//! **frames CD** de 1/75 s : 588 échantillons à 44,1 kHz, exactement. La base
//! les range en millisecondes (`cue_start_ms`, `cue_end_ms`), tronquées par
//! `scanner::cue::parse_cue_time` : `ff * 1000 / 75`. Diviser ensuite ces
//! millisecondes par mille pour chercher dans le fichier image décalait chaque
//! frontière de 0 à 0,67 ms, soit jusqu'à 29 échantillons : la fin d'une piste
//! passait dans la suivante, et un clic pouvait naître à la jointure (#8754,
//! albums live).
//!
//! Les millisecondes de la base ne changent pas de sens — elles restent
//! l'identité de la piste (`idx_tracks_cue_identity`). Mais elles portent la
//! frame CD sans perte : `ms = ⌊F × 1000 / 75⌋` est strictement croissante, et
//! deux frames successives diffèrent de 13 ou 14 ms. La frame se retrouve donc
//! par `F = ⌈3 × ms / 40⌉`, et se VÉRIFIE en refaisant la troncature. Une
//! valeur qui ne vient pas d'une frame CD (chapitres Matroska, fichier de
//! pistes saisi à la main) ne passe pas la vérification et se convertit par
//! l'arrondi ordinaire ; si elle la passe par hasard, la frame retrouvée est à
//! moins d'une milliseconde de la valeur, ce qui n'est pas pire. Aucune
//! migration n'est nécessaire.
//!
//! Conversion frame → échantillon : `F × cadence / 75`, exacte pour toutes
//! les cadences multiples de 75 (44,1, 48, 88,2, 96, 176,4, 192 kHz…), arrondie
//! au plus proche ailleurs.

/// La frame CD que désignent ces millisecondes, si elles viennent de la
/// troncature `F × 1000 / 75` de `scanner::cue::parse_cue_time`.
pub fn frame_cd_de_ms(ms: u64) -> Option<u64> {
    let f = (ms.checked_mul(3)?.checked_add(39)?) / 40;
    (f.checked_mul(1000)? / 75 == ms).then_some(f)
}

/// L'échantillon (trame) du fichier image que désignent ces millisecondes.
pub fn echantillon_de_ms(ms: u64, cadence: u32) -> u64 {
    let c = u128::from(cadence);
    match frame_cd_de_ms(ms) {
        Some(f) => ((u128::from(f) * c + 37) / 75) as u64,
        None => ((u128::from(ms) * c + 500) / 1000) as u64,
    }
}

/// Début et fin (exclue) d'une tranche, en échantillons du fichier image ;
/// `fin` à `None` : jusqu'au bout du fichier. Une fin qui ne dépasse pas le
/// début est traitée comme absente, comme le fait `Track::bornes_cue`.
pub fn tranche_en_echantillons(
    debut_ms: u64,
    fin_ms: Option<u64>,
    cadence: u32,
) -> (u64, Option<u64>) {
    let debut = echantillon_de_ms(debut_ms, cadence);
    let fin = fin_ms
        .map(|f| echantillon_de_ms(f, cadence))
        .filter(|f| *f > debut);
    (debut, fin)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Toutes les frames d'une heure : la troncature de la feuille se
    /// retrouve sans exception, et donne 588 échantillons par frame.
    #[test]
    fn chaque_frame_cd_se_retrouve_depuis_ses_millisecondes_8968() {
        for f in 0..(75 * 3_600u64) {
            let ms = f * 1000 / 75;
            assert_eq!(frame_cd_de_ms(ms), Some(f), "frame {f} ({ms} ms)");
            assert_eq!(echantillon_de_ms(ms, 44_100), f * 588);
            assert_eq!(echantillon_de_ms(ms, 96_000), f * 1_280);
        }
    }

    #[test]
    fn la_borne_de_la_feuille_tombe_sur_l_echantillon_8968() {
        let ms = crate::scanner::cue::parse_cue_time("00:01:37").unwrap();
        assert_eq!(ms, 1_493, "la base garde des millisecondes tronquées");
        assert_eq!(echantillon_de_ms(ms, 44_100), 112 * 588);
        // L'ancienne conversion (ms / 1000 × cadence) tombait 15 trames trop tôt.
        assert_eq!((ms as f64 / 1000.0 * 44_100.0) as u64, 112 * 588 - 15);
        assert_eq!(echantillon_de_ms(ms, 48_000), 112 * 640);
        assert_eq!(echantillon_de_ms(ms, 88_200), 112 * 1_176);
    }

    #[test]
    fn une_milliseconde_qui_n_est_pas_une_frame_s_arrondit_8968() {
        // 1 ms ne vient d'aucune frame (0 → 0 ms, 1 → 13 ms).
        assert_eq!(frame_cd_de_ms(1), None);
        assert_eq!(echantillon_de_ms(1, 44_100), 44);
        assert_eq!(echantillon_de_ms(1_500, 44_100), 66_150);
        // Cadence non multiple de 75 : arrondi au plus proche.
        assert_eq!(echantillon_de_ms(13, 22_050), 294);
        assert_eq!(echantillon_de_ms(13, 32_000), 427); // 426,67
    }

    #[test]
    fn une_fin_avant_le_debut_vaut_jusqu_au_bout_8968() {
        assert_eq!(
            tranche_en_echantillons(2_946, Some(1_493), 44_100),
            (221 * 588, None)
        );
        assert_eq!(
            tranche_en_echantillons(1_493, Some(2_946), 44_100),
            (112 * 588, Some(221 * 588))
        );
    }
}
