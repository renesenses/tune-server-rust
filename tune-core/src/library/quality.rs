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

/// Cadence 1 bit du DSD64 : la base de tous les paliers de la famille 44,1 kHz
/// (`2 822 400 = 64 × 44 100`). Même valeur que `audio/dsf.rs` et
/// `audio/dff.rs` figent dans leurs tests.
pub const CADENCE_DSD64: i64 = 2_822_400;

/// Cadence 1 bit du DSD64 « 48k » (`3 072 000 = 64 × 48 000`), la famille
/// minoritaire des fichiers gravés sur base 48 kHz.
pub const CADENCE_DSD64_48K: i64 = 3_072_000;

/// En deçà, ce n'est pas du DSD. C'est la borne basse que `audio/dsf.rs` et
/// `audio/dff.rs` appliquent déjà à l'en-tête (« unexpected DSD sample rate »).
const PLANCHER_DSD: i64 = 2_000_000;

/// Un libellé de cadence tel que l'écran peut l'afficher : `DSD64`, `96 kHz`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibelleCadence {
    /// La valeur brute de la colonne, celle que le filtre `sample_rate=` reçoit.
    pub value: i64,
    /// Le texte à afficher.
    pub label: String,
    /// Vrai pour un palier DSD (la piste porte alors `bit_depth = 1`).
    pub dsd: bool,
}

/// Libellé d'une cadence d'échantillonnage (#4171).
///
/// Le scan écrit dans `tracks.sample_rate` — puis, par `MAX` dans
/// `albums.sample_rate` — la cadence 1 bit BRUTE d'un `.dsf`/`.dff` :
/// 2 822 400 pour du DSD64, 5 644 800 pour du DSD128. La facette des
/// fréquences rendait ces entiers tels quels, et l'écran n'en faisait rien
/// (Cyrille Moutia, fil 1792) : il ne les reconnaissait ni comme des kHz ni
/// comme du DSD.
///
/// Les paliers se nomment par leur rapport à la base : `DSD64`, `DSD128`,
/// `DSD256`, `DSD512`, `DSD1024`. La famille 48 kHz (3 072 000, …) reçoit un
/// suffixe `(48k)` — deux cadences DIFFÉRENTES ne doivent jamais porter le même
/// libellé, sinon on refabrique les deux lignes « DSD » indiscernables de
/// #1612. Une cadence ≥ 2 MHz hors de ces deux grilles reste du DSD, libellé
/// en MHz. Le PCM est rendu en kHz, décimale seulement quand il y en a une
/// (`44.1 kHz`, `48 kHz`) ; le séparateur est le point, à localiser côté client.
pub fn libelle_cadence(sample_rate: i64) -> LibelleCadence {
    let dsd = sample_rate >= PLANCHER_DSD;
    let label = if dsd {
        if sample_rate % CADENCE_DSD64 == 0 {
            format!("DSD{}", 64 * (sample_rate / CADENCE_DSD64))
        } else if sample_rate % CADENCE_DSD64_48K == 0 {
            format!("DSD{} (48k)", 64 * (sample_rate / CADENCE_DSD64_48K))
        } else {
            format!("DSD {} MHz", sample_rate as f64 / 1_000_000.0)
        }
    } else if sample_rate >= 1000 {
        format!("{} kHz", sample_rate as f64 / 1000.0)
    } else {
        format!("{sample_rate} Hz")
    };
    LibelleCadence {
        value: sample_rate,
        label,
        dsd,
    }
}

/// Libellé d'une profondeur de bits : `1` est le marqueur du DSD (le scan
/// écrit `bit_depth = 1` pour tout `.dsf`/`.dff`), et doit se lire comme tel
/// plutôt que comme « 1 bit » tout court.
pub fn libelle_profondeur(bit_depth: i64) -> String {
    match bit_depth {
        1 => "1 bit (DSD)".to_string(),
        n => format!("{n} bits"),
    }
}

#[cfg(test)]
mod tests {
    use super::{libelle_cadence, libelle_profondeur, score_qualite};

    /// #4171 : les quatre paliers de l'issue, la famille 48k distinguée, et le
    /// PCM lisible — chaque libellé porte la valeur brute, pour que le filtre
    /// `sample_rate=<valeur>` reste celui que le client connaît.
    #[test]
    fn les_paliers_dsd_sont_nommes_et_marques_dsd() {
        for (valeur, attendu) in [
            (2_822_400, "DSD64"),
            (5_644_800, "DSD128"),
            (11_289_600, "DSD256"),
            (22_579_200, "DSD512"),
            (45_158_400, "DSD1024"),
            (3_072_000, "DSD64 (48k)"),
            (6_144_000, "DSD128 (48k)"),
        ] {
            let l = libelle_cadence(valeur);
            assert_eq!(l.label, attendu, "{valeur}");
            assert!(l.dsd, "{valeur} est du DSD");
            assert_eq!(l.value, valeur);
        }
        // Hors grille mais ≥ 2 MHz : toujours du DSD, jamais confondu avec un
        // palier voisin.
        let l = libelle_cadence(2_900_000);
        assert!(l.dsd);
        assert_eq!(l.label, "DSD 2.9 MHz");
    }

    #[test]
    fn le_pcm_se_lit_en_khz_sans_decimale_inutile() {
        for (valeur, attendu) in [
            (44_100, "44.1 kHz"),
            (48_000, "48 kHz"),
            (88_200, "88.2 kHz"),
            (96_000, "96 kHz"),
            (176_400, "176.4 kHz"),
            (192_000, "192 kHz"),
            (352_800, "352.8 kHz"),
            (384_000, "384 kHz"),
            (705_600, "705.6 kHz"),
            (22_050, "22.05 kHz"),
        ] {
            let l = libelle_cadence(valeur);
            assert_eq!(l.label, attendu, "{valeur}");
            assert!(!l.dsd, "{valeur} n'est pas du DSD");
        }
        assert_eq!(libelle_cadence(800).label, "800 Hz");
    }

    #[test]
    fn la_profondeur_un_bit_se_lit_comme_du_dsd() {
        assert_eq!(libelle_profondeur(1), "1 bit (DSD)");
        assert_eq!(libelle_profondeur(16), "16 bits");
        assert_eq!(libelle_profondeur(24), "24 bits");
    }

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
