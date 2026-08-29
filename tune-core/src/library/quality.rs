//! Comparer la qualité de deux fichiers qui portent le même morceau.
//!
//! Une seule règle, partagée par tout ce qui doit choisir entre deux copies :
//! « Disponible en meilleure qualité » (`routes/library/better_quality.rs`) et
//! le repli d'affichage/lecture d'un album
//! ([`crate::db::track_repo::dedup_display_tracks`]). Deux barèmes finiraient
//! par diverger, et l'écran proposerait alors une variante que la lecture
//! n'irait pas chercher.

/// Score de qualité comparable entre formats : `(sans_perte, débit)`.
///
/// Un sans-perte bat toujours un avec-perte ; à famille égale,
/// `sample_rate × bit_depth` départage. Le DSD porte `bit_depth = 1` :
/// DSD64 (2,8 M) bat le CD (0,7 M) et le 96/24 (2,3 M), mais s'incline
/// devant le 192/24 (4,6 M) — arbitrage assumé, documenté, et testé.
pub fn score_qualite(
    format: Option<&str>,
    sample_rate: Option<i64>,
    bit_depth: Option<i64>,
) -> (bool, i64) {
    // La même liste que le filtre « Lossy » de la bibliothèque.
    const AVEC_PERTE: [&str; 5] = ["mp3", "aac", "ogg", "opus", "wma"];
    let sans_perte = format
        .map(|f| !AVEC_PERTE.contains(&f.to_lowercase().as_str()))
        .unwrap_or(false);
    let sr = sample_rate.unwrap_or(44100).max(1);
    let bd = bit_depth.unwrap_or(16).max(1);
    (sans_perte, sr.saturating_mul(bd))
}

#[cfg(test)]
mod tests {
    use super::score_qualite;

    #[test]
    fn sans_perte_bat_avec_perte_meme_a_debit_inferieur() {
        assert!(
            score_qualite(Some("flac"), Some(44100), Some(16))
                > score_qualite(Some("mp3"), Some(48000), Some(24))
        );
    }

    #[test]
    fn a_famille_egale_le_debit_departage() {
        assert!(
            score_qualite(Some("flac"), Some(96000), Some(24))
                > score_qualite(Some("flac"), Some(44100), Some(16))
        );
    }

    #[test]
    fn le_dsd_se_place_entre_le_cd_et_le_192_24() {
        let dsd64 = score_qualite(Some("dsf"), Some(2_822_400), Some(1));
        assert!(dsd64 > score_qualite(Some("flac"), Some(44100), Some(16)));
        assert!(dsd64 > score_qualite(Some("flac"), Some(96000), Some(24)));
        assert!(score_qualite(Some("flac"), Some(192_000), Some(24)) > dsd64);
    }

    #[test]
    fn deux_fichiers_identiques_ont_le_meme_score() {
        let a = score_qualite(Some("flac"), Some(44100), Some(16));
        assert_eq!(a, score_qualite(Some("FLAC"), Some(44100), Some(16)));
    }

    #[test]
    fn aiff_bat_aac_meme_quand_l_aac_annonce_une_cadence_plus_haute() {
        // Le cas du rapport #1362 : un CD rippé en AIFF et le même morceau
        // récupéré ailleurs en AAC. Quoi qu'annonce l'AAC, c'est l'AIFF qui
        // doit gagner — sans quoi « garder le meilleur » jouerait le pire.
        assert!(
            score_qualite(Some("aiff"), Some(44100), Some(16))
                > score_qualite(Some("aac"), Some(48000), Some(24))
        );
    }
}
