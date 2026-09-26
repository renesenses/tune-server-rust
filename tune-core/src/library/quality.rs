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

/// Le JUMEAU SQL de [`score_qualite`], pour les prédicats qui doivent choisir
/// entre deux copies SANS remonter les lignes en mémoire (#4101).
///
/// ## Pourquoi un jumeau, alors que l'en-tête de ce module interdit deux barèmes
///
/// Justement pour qu'il n'y en ait qu'un. Le repli d'affichage
/// ([`crate::db::track_repo::dedup_display_tracks`]) trie en Rust une liste
/// DÉJÀ rendue : c'est possible sur les pistes d'UN album, pas sur une vue
/// paginée de toute la bibliothèque, où la liste et son `total` doivent
/// exclure le même ensemble ou la pagination saute des pages. Le prédicat qui
/// sert cette vue est donc du SQL — et il est écrit ICI, à côté du barème
/// qu'il transcrit, avec une épreuve qui compare les deux verdicts terme à
/// terme (`le_barème_sql_dit_la_même_chose_que_le_barème_rust`). Écrit
/// ailleurs, il aurait divergé au premier correctif.
///
/// Rend l'expression `1`/`0` de « cette copie est sans perte ». Un `format`
/// NUL vaut `0`, comme le `unwrap_or(false)` de [`score_qualite`].
pub fn sql_sans_perte(alias: &str) -> String {
    format!(
        "(CASE WHEN {alias}.format IS NOT NULL AND LOWER({alias}.format)          NOT IN ('mp3', 'aac', 'ogg', 'opus', 'wma') THEN 1 ELSE 0 END)"
    )
}

/// Le débit comparable de [`score_qualite`] : `sample_rate × bit_depth`, avec
/// les mêmes valeurs par défaut (44 100 et 16) et le même plancher à 1.
pub fn sql_debit(alias: &str) -> String {
    format!(
        "((CASE WHEN COALESCE({alias}.sample_rate, 44100) < 1 THEN 1          ELSE COALESCE({alias}.sample_rate, 44100) END)          * (CASE WHEN COALESCE({alias}.bit_depth, 16) < 1 THEN 1          ELSE COALESCE({alias}.bit_depth, 16) END))"
    )
}

/// « La copie `a` est STRICTEMENT de meilleure qualité que la copie `b`. »
/// L'ordre lexicographique du couple `(sans_perte, débit)`, écrit en deux
/// termes plutôt qu'en comparaison de n-uplets : les valeurs de ligne
/// (`(x, y) > (z, w)`) n'existent pas dans toutes les versions de SQLite que
/// les testeurs font tourner.
pub fn sql_strictement_meilleure(a: &str, b: &str) -> String {
    let (sa, sb) = (sql_sans_perte(a), sql_sans_perte(b));
    let (da, db) = (sql_debit(a), sql_debit(b));
    format!("({sa} > {sb} OR ({sa} = {sb} AND {da} > {db}))")
}

/// « Les deux copies se valent » — le cas où il faut un départage stable.
pub fn sql_meme_score(a: &str, b: &str) -> String {
    let (sa, sb) = (sql_sans_perte(a), sql_sans_perte(b));
    let (da, db) = (sql_debit(a), sql_debit(b));
    format!("({sa} = {sb} AND {da} = {db})")
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
    use super::{
        libelle_cadence, libelle_profondeur, score_qualite, sql_meme_score,
        sql_strictement_meilleure,
    };

    /// 🔴 #4101 — **la seule épreuve qui empêche les deux barèmes de diverger.**
    ///
    /// Le repli d'affichage trie en Rust ; le prédicat de la vue paginée trie
    /// en SQL. Les deux doivent rendre le MÊME verdict sur chaque paire, sinon
    /// l'écran montrerait une copie que la lecture n'irait pas chercher —
    /// exactement ce que l'en-tête de ce module interdit.
    ///
    /// Le SQL est évalué par le vrai moteur, sur la vraie table `tracks` : une
    /// transcription relue à l'œil ne prouve rien.
    #[test]
    fn le_bareme_sql_dit_la_meme_chose_que_le_bareme_rust() {
        use crate::db::backend::{DbBackend, ToSqlValue};
        use crate::db::models::Track;
        use crate::db::sqlite::SqliteDb;
        use crate::db::track_repo::TrackRepo;

        // Les copies du rapport #1362 et les arbitrages documentés plus haut :
        // sans-perte contre avec-perte, débit à famille égale, DSD, et les
        // colonnes absentes d'une base ancienne.
        type Copie = (&'static str, Option<&'static str>, Option<i32>, Option<i32>);
        const COPIES: [Copie; 8] = [
            ("aiff 44/16", Some("aiff"), Some(44100), Some(16)),
            ("aac 48/24", Some("aac"), Some(48000), Some(24)),
            ("flac 44/16", Some("flac"), Some(44100), Some(16)),
            ("flac 96/24", Some("flac"), Some(96000), Some(24)),
            ("flac 192/24", Some("flac"), Some(192_000), Some(24)),
            ("dsf DSD64", Some("dsf"), Some(2_822_400), Some(1)),
            ("mp3 44/16", Some("mp3"), Some(44100), Some(16)),
            ("sans colonnes", None, None, None),
        ];

        let db = SqliteDb::open_in_memory().expect("base en mémoire");
        db.init_schema().expect("schéma");
        let repo = TrackRepo::new(db.clone());
        let mut ids: Vec<i64> = Vec::new();
        for (nom, format, sr, bd) in COPIES {
            let mut t = Track::new(nom.to_string());
            t.format = format.map(str::to_string);
            t.sample_rate = sr;
            t.bit_depth = bd;
            t.file_path = Some(format!("/musique/{nom}.bin"));
            ids.push(repo.create(&t).expect("piste"));
        }

        let sql = format!(
            "SELECT {}, {} FROM tracks a, tracks b WHERE a.id = ? AND b.id = ?",
            sql_strictement_meilleure("a", "b"),
            sql_meme_score("a", "b"),
        );
        for (i, (nom_a, fa, sa, ba)) in COPIES.iter().enumerate() {
            for (j, (nom_b, fb, sb, bb)) in COPIES.iter().enumerate() {
                let params: [&dyn ToSqlValue; 2] = [&ids[i], &ids[j]];
                let cols = db
                    .query_one(&sql, &params)
                    .expect("requête de comparaison")
                    .expect("une ligne");
                let sql_meilleure = cols[0].as_i64().unwrap_or(0) != 0;
                let sql_egales = cols[1].as_i64().unwrap_or(0) != 0;

                let rust_a = score_qualite(*fa, sa.map(i64::from), ba.map(i64::from));
                let rust_b = score_qualite(*fb, sb.map(i64::from), bb.map(i64::from));

                assert_eq!(
                    sql_meilleure,
                    rust_a > rust_b,
                    "« {nom_a} » vs « {nom_b} » : le barème SQL et le barème Rust \
                     ne disent pas la même chose sur « strictement meilleure »"
                );
                assert_eq!(
                    sql_egales,
                    rust_a == rust_b,
                    "« {nom_a} » vs « {nom_b} » : le barème SQL et le barème Rust \
                     ne disent pas la même chose sur « se valent »"
                );
            }
        }
    }

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
