//! #5481 — la fréquence de sortie d'une source DSD dans le Convertisseur.
//!
//! Xavier Joly (Reivax66, 0.9.168) : « L'encodage en Hi-Res se fait en 24/96
//! ce qui n'est pas précisé [...] ceux qui veulent convertir des DSD en Flac
//! préféreraient du 24/192. »
//!
//! Un flux DSD n'a pas de « fréquence d'origine » au sens PCM : c'est un flux
//! d'un bit à 2,8224 MHz (DSD64) ou plus. Le préréglage Hi-Res envoie
//! `sample_rate: null` (« fréquence d'origine ») ; le convertisseur héritait
//! donc de la politique de la LECTURE, `choose_output_rate` : 176,4 kHz pour
//! DSD64, 352,8 kHz pour DSD128 et au-delà. Rien ne le disait, et rien ne le
//! choisissait pour la conversion.
//!
//! ## Le choix : 176,4 kHz par défaut, pour tous les rangs DSD
//!
//! La demande est « 24/192 ». On retient **176,4 kHz** (la classe 192 de la
//! famille 44,1) plutôt que 192 kHz tout rond, pour une raison mesurable :
//!
//! - Le décimateur ([`tune_core::audio::dsd_to_pcm::DsdToPcmStreamer`])
//!   travaille à rapport ENTIER. 2 822 400 / 176 400 = 16 tout juste ; DSD128
//!   donne 32, DSD256 64. Aucune seconde étape.
//! - 192 kHz depuis DSD64 est un rapport de 14,7. Il faudrait décimer à
//!   352,8 kHz puis rééchantillonner d'un rapport 160/147 — un second filtre
//!   pour zéro information en plus : au-delà de ~50 kHz, un DSD ne porte que
//!   le bruit repoussé par sa mise en forme.
//! - 352,8 kHz (le défaut hérité pour DSD128+) double la taille des fichiers
//!   pour un gain nul à l'écoute, et nombre de lecteurs et de DAC ne le
//!   lisent pas.
//!
//! Un DSD de la famille 48 kHz (3,072 MHz, rare) reçoit 192 kHz, pour la même
//! raison de rapport entier.
//!
//! ## Une fréquence choisie qui ne divise pas la fréquence DSD
//!
//! Le décimateur tronquait le rapport : `2 822 400 / 192 000 = 14` au lieu de
//! 14,7. Le PCM sortait donc à 201,6 kHz, étiqueté 192 kHz : un fichier 5 %
//! trop long, joué un demi-ton trop grave. Une fréquence qui ne divise pas la
//! source est désormais décodée à la plus petite fréquence ENTIÈRE supérieure
//! ou égale (352,8 kHz pour 192 kHz), puis rééchantillonnée vers la cible.

use std::path::Path;

use tune_core::audio::decode::{DecodedAudio, decode_to_pcm};

/// Fréquence de sortie par défaut d'une source DSD de la famille 44,1 kHz.
pub(super) const FREQUENCE_DSD_PAR_DEFAUT: u32 = 176_400;

/// Les fréquences que l'écran propose pour le préréglage Hi-Res, en plus de
/// « Auto » (`sample_rate: null`). Toutes sont produites correctement depuis
/// n'importe quel rang DSD : directement quand elles divisent la source, par
/// une décimation entière puis un rééchantillonnage sinon.
pub(super) const FREQUENCES_PROPOSEES: [u32; 5] = [88_200, 96_000, 176_400, 192_000, 352_800];

/// Le plus petit rapport de décimation qu'on s'autorise : 8 (DSD64 →
/// 352,8 kHz). En dessous, le filtre passe-bas du décimateur coupe trop près
/// de la bande utile.
const DECIMATION_MINIMALE: u32 = 8;

/// La fréquence DSD (bits par seconde et par canal) d'une source, ou `None`
/// si ce n'est pas un DSD — ou si son en-tête est illisible, auquel cas le
/// décodeur ordinaire rendra lui-même l'erreur.
pub(super) fn frequence_dsd(entree: &str) -> Option<u32> {
    let chemin = Path::new(entree);
    let ext = chemin.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "dsf" => tune_core::audio::dsf::parse_dsf(entree)
            .ok()
            .map(|i| i.sample_rate),
        "dff" => tune_core::audio::dff::parse_dff(entree)
            .ok()
            .map(|i| i.sample_rate),
        "iso" if tune_core::audio::sacd::est_extension_iso(chemin) => {
            tune_core::audio::sacd::parametres_de_lecture(chemin)
                .ok()
                .map(|(taux, _)| taux)
        }
        _ => None,
    }
}

/// La fréquence de sortie d'une source DSD quand l'appelant n'en demande
/// aucune (préréglage Hi-Res, « Auto »).
pub(super) fn frequence_par_defaut(dsd: u32) -> u32 {
    if dsd.is_multiple_of(FREQUENCE_DSD_PAR_DEFAUT) {
        FREQUENCE_DSD_PAR_DEFAUT
    } else if dsd.is_multiple_of(192_000) {
        192_000
    } else {
        tune_core::audio::dsd_to_pcm::choose_output_rate(dsd)
    }
}

/// La fréquence à laquelle DÉCIMER pour obtenir `cible` sans rapport
/// fractionnaire : `cible` elle-même si elle divise `dsd`, sinon la plus petite
/// fréquence `dsd / 2^k` (rapport ≥ 8) qui lui est supérieure ou égale — on
/// rééchantillonne alors vers le bas, jamais vers le haut.
pub(super) fn frequence_de_decimation(dsd: u32, cible: u32) -> u32 {
    if cible > 0 && dsd.is_multiple_of(cible) && dsd / cible >= DECIMATION_MINIMALE {
        return cible;
    }
    let mut rapport = DECIMATION_MINIMALE;
    let mut retenue = dsd / rapport;
    while dsd.is_multiple_of(rapport * 2) && dsd / (rapport * 2) >= cible {
        rapport *= 2;
        retenue = dsd / rapport;
    }
    retenue
}

/// Décode une source DSD à la fréquence `cible` (ou au défaut du
/// convertisseur), sans jamais demander au décimateur un rapport fractionnaire.
pub(super) fn decoder_un_dsd(
    entree: &str,
    dsd: u32,
    cible: Option<u32>,
) -> Result<DecodedAudio, String> {
    let sortie = cible.unwrap_or_else(|| frequence_par_defaut(dsd));
    let decimation = frequence_de_decimation(dsd, sortie);
    let mut decode = decode_to_pcm(entree, Some(decimation), None, 0.0, f64::MAX)?;
    if decode.sample_rate != sortie {
        decode.samples_i32 = tune_core::audio::resample::resample_i32(
            &decode.samples_i32,
            decode.bit_depth,
            decode.channels as u16,
            decode.sample_rate,
            sortie,
        );
        decode.sample_rate = sortie;
    }
    Ok(decode)
}

/// Un format réellement écrit par une conversion (#5481).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FormatEcrit {
    pub sample_rate: u32,
    /// `None` pour un format avec perte (MP3, AAC, Opus), qui n'a pas de
    /// profondeur au sens PCM.
    pub bit_depth: Option<u8>,
}

/// Relit la fréquence et la profondeur d'un fichier produit. `None` si le
/// fichier est illisible : le compteur de la conversion, lui, a déjà avancé.
pub(super) fn format_ecrit(chemin: &Path) -> Option<FormatEcrit> {
    use lofty::file::AudioFile;
    let fichier = lofty::read_from_path(chemin).ok()?;
    let proprietes = fichier.properties();
    Some(FormatEcrit {
        sample_rate: proprietes.sample_rate()?,
        bit_depth: proprietes.bit_depth(),
    })
}

/// Le champ `output_formats` de `GET /converter/status/{id}`.
pub(super) fn formats_en_json(formats: &[FormatEcrit]) -> serde_json::Value {
    formats
        .iter()
        .map(|f| serde_json::json!({"sample_rate": f.sample_rate, "bit_depth": f.bit_depth}))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DSD64: u32 = 2_822_400;
    const DSD128: u32 = 5_644_800;
    const DSD256: u32 = 11_289_600;

    /// Un DSF stéréo minimal : `octets_par_canal` octets de motif alterné par
    /// canal (un bloc de 4096 octets par canal et par tranche, comme le format
    /// l'impose).
    fn ecrire_dsf(chemin: &Path, taux: u32, octets_par_canal: usize) {
        let canaux = 2u32;
        let bloc = 4096usize;
        let blocs = octets_par_canal.div_ceil(bloc);
        let donnees = blocs * bloc * canaux as usize;
        let echantillons = (octets_par_canal as u64) * 8;
        let mut f = Vec::new();
        f.extend_from_slice(b"DSD ");
        f.extend_from_slice(&28u64.to_le_bytes());
        let total = 28 + 52 + 12 + donnees as u64;
        f.extend_from_slice(&total.to_le_bytes());
        f.extend_from_slice(&0u64.to_le_bytes());
        f.extend_from_slice(b"fmt ");
        f.extend_from_slice(&52u64.to_le_bytes());
        f.extend_from_slice(&1u32.to_le_bytes()); // version
        f.extend_from_slice(&0u32.to_le_bytes()); // DSD brut
        f.extend_from_slice(&2u32.to_le_bytes()); // stéréo
        f.extend_from_slice(&canaux.to_le_bytes());
        f.extend_from_slice(&taux.to_le_bytes());
        f.extend_from_slice(&1u32.to_le_bytes()); // 1 bit
        f.extend_from_slice(&echantillons.to_le_bytes());
        f.extend_from_slice(&(bloc as u32).to_le_bytes());
        f.extend_from_slice(&0u32.to_le_bytes());
        f.extend_from_slice(b"data");
        f.extend_from_slice(&(12 + donnees as u64).to_le_bytes());
        // 0x69 = 01101001 : un motif qui n'est ni silence ni saturation.
        f.extend(std::iter::repeat_n(0x69u8, donnees));
        std::fs::write(chemin, f).unwrap();
    }

    #[test]
    fn le_defaut_est_176_4_khz_pour_tous_les_rangs_dsd() {
        assert_eq!(frequence_par_defaut(DSD64), 176_400);
        assert_eq!(frequence_par_defaut(DSD128), 176_400);
        assert_eq!(frequence_par_defaut(DSD256), 176_400);
        // La famille 48 kHz garde un rapport entier.
        assert_eq!(frequence_par_defaut(3_072_000), 192_000);
    }

    #[test]
    fn une_frequence_qui_ne_divise_pas_la_source_passe_par_une_decimation_entiere() {
        // Divise : décodée telle quelle.
        assert_eq!(frequence_de_decimation(DSD64, 176_400), 176_400);
        assert_eq!(frequence_de_decimation(DSD64, 88_200), 88_200);
        assert_eq!(frequence_de_decimation(DSD64, 44_100), 44_100);
        // Ne divise pas : la plus petite fréquence entière au-dessus.
        assert_eq!(frequence_de_decimation(DSD64, 192_000), 352_800);
        assert_eq!(frequence_de_decimation(DSD64, 96_000), 176_400);
        assert_eq!(frequence_de_decimation(DSD64, 48_000), 88_200);
        assert_eq!(frequence_de_decimation(DSD128, 192_000), 352_800);
        // Jamais un rapport sous 8 : 705,6 kHz depuis DSD64 serait 4.
        assert_eq!(frequence_de_decimation(DSD64, 705_600), 352_800);
        for dsd in [DSD64, DSD128, DSD256] {
            for cible in FREQUENCES_PROPOSEES {
                let d = frequence_de_decimation(dsd, cible);
                assert!(dsd.is_multiple_of(d), "{dsd} / {d} doit être entier");
                assert!(dsd / d >= DECIMATION_MINIMALE);
            }
        }
    }

    #[test]
    fn un_dsf_se_reconnait_a_son_entete() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.dsf");
        ecrire_dsf(&p, DSD128, 4096);
        assert_eq!(frequence_dsd(p.to_str().unwrap()), Some(DSD128));
        let w = dir.path().join("a.wav");
        std::fs::write(&w, b"RIFF").unwrap();
        assert_eq!(frequence_dsd(w.to_str().unwrap()), None);
    }

    /// Nombre d'échantillons par canal attendu pour `secondes` à `taux`, à la
    /// latence des filtres près (quelques centaines d'échantillons au plus).
    fn assez_proche(obtenu: usize, attendu: f64) -> bool {
        (obtenu as f64 - attendu).abs() <= attendu * 0.002 + 512.0
    }

    /// Contre-épreuve incluse : le décodage DIRECT à 192 kHz — ce que faisait
    /// le convertisseur — rend 5 % d'échantillons de trop. Le nouveau chemin
    /// rend la bonne durée.
    #[test]
    fn un_dsd64_converti_en_192_khz_garde_sa_duree() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.dsf");
        // 1 seconde de DSD64 : 2 822 400 bits par canal.
        ecrire_dsf(&p, DSD64, (DSD64 / 8) as usize);
        let entree = p.to_str().unwrap();

        let direct = decode_to_pcm(entree, Some(192_000), None, 0.0, f64::MAX).unwrap();
        let par_canal_direct = direct.samples_i32.len() / direct.channels as usize;
        assert!(
            !assez_proche(par_canal_direct, 192_000.0),
            "contre-épreuve : le décodage direct à 192 kHz devait être faux, il rend {par_canal_direct}"
        );
        assert!(assez_proche(par_canal_direct, 201_600.0));

        let d = decoder_un_dsd(entree, DSD64, Some(192_000)).unwrap();
        assert_eq!(d.sample_rate, 192_000);
        let par_canal = d.samples_i32.len() / d.channels as usize;
        assert!(
            assez_proche(par_canal, 192_000.0),
            "1 s à 192 kHz attendue, {par_canal} échantillons par canal"
        );
    }

    #[test]
    fn sans_frequence_demandee_un_dsd128_sort_en_176_4_khz() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.dsf");
        ecrire_dsf(&p, DSD128, (DSD128 / 8 / 4) as usize); // 0,25 s
        let entree = p.to_str().unwrap();
        // Contre-épreuve : la politique de LECTURE, que le convertisseur
        // héritait, rend 352,8 kHz.
        let lecture = decode_to_pcm(entree, None, None, 0.0, f64::MAX).unwrap();
        assert_eq!(lecture.sample_rate, 352_800);

        let d = decoder_un_dsd(entree, DSD128, None).unwrap();
        assert_eq!(d.sample_rate, 176_400);
        assert_eq!(d.bit_depth, 24);
        let par_canal = d.samples_i32.len() / d.channels as usize;
        assert!(assez_proche(par_canal, 176_400.0 / 4.0));
    }

    /// Le fichier écrit est relu, pas supposé : un FLAC 24/176,4 est annoncé
    /// comme tel.
    #[test]
    fn le_format_ecrit_se_relit_sur_le_fichier() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.flac");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pcm = vec![0u8; 176_400 / 10 * 2 * 3];
        let octets = rt.block_on(async {
            let mut e = tune_core::audio::encoder::AudioEncoder::new("flac", 176_400, 24, 2);
            e.start().await.unwrap();
            e.write(&pcm).await.unwrap();
            e.finish().await.unwrap()
        });
        std::fs::write(&p, octets).unwrap();
        assert_eq!(
            format_ecrit(&p),
            Some(FormatEcrit {
                sample_rate: 176_400,
                bit_depth: Some(24)
            })
        );
        assert_eq!(format_ecrit(&dir.path().join("absent.flac")), None);
    }

    /// L'écran lit la fréquence réellement écrite dans `/status`, et le choix
    /// des fréquences dans `/presets`.
    #[tokio::test]
    async fn statut_et_preglages_annoncent_la_frequence() {
        use super::super::list_presets;
        let formats = [FormatEcrit {
            sample_rate: 176_400,
            bit_depth: Some(24),
        }];
        assert_eq!(
            formats_en_json(&formats),
            serde_json::json!([{"sample_rate": 176_400, "bit_depth": 24}])
        );
        // Et c'est bien ce champ que le statut rend.
        let source = include_str!("converter.rs");
        assert_eq!(
            source
                .matches("\"output_formats\": dsd::formats_en_json(&job.formats_ecrits)")
                .count(),
            1
        );

        let presets = list_presets().await.0;
        let hires = presets
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "flac-hires")
            .unwrap();
        assert!(hires["sample_rate"].is_null(), "Auto reste le défaut");
        assert_eq!(hires["dsd_sample_rate"], 176_400);
        assert_eq!(
            hires["sample_rate_choices"],
            serde_json::json!(FREQUENCES_PROPOSEES)
        );
    }

    /// De bout en bout, par le greffon Convertisseur et son hôte — le chemin
    /// qu'emprunte `POST /converter/start` : le préréglage Hi-Res
    /// (`sample_rate: null`, 24 bits) d'un DSD128 écrit un FLAC 24/176,4, et
    /// une fréquence de 192 kHz choisie garde la durée de la source.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_hi_res_d_un_dsd_ecrit_un_flac_juste() {
        use lofty::file::AudioFile;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("01 - Orbits.dsf");
        ecrire_dsf(&source, DSD128, (DSD128 / 8 / 2) as usize); // 0,5 s
        let convertir = |sortie: std::path::PathBuf, sample_rate: Option<u32>| {
            let source = source.clone();
            tokio::task::spawn_blocking(move || {
                crate::routes::premium_audio_host::run(
                    &tune_plugin_converter::Converter,
                    &source,
                    &sortie,
                    &serde_json::json!({
                        "format": "flac", "quality": "5",
                        "sample_rate": sample_rate, "bit_depth": 24
                    }),
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                )
            })
        };

        let auto = dir.path().join("auto.flac");
        convertir(auto.clone(), None).await.unwrap().unwrap();
        assert_eq!(
            format_ecrit(&auto),
            Some(FormatEcrit {
                sample_rate: 176_400,
                bit_depth: Some(24)
            }),
            "Hi-Res « Auto » depuis un DSD128"
        );

        let choisi = dir.path().join("192.flac");
        convertir(choisi.clone(), Some(192_000))
            .await
            .unwrap()
            .unwrap();
        let fichier = lofty::read_from_path(&choisi).unwrap();
        assert_eq!(fichier.properties().sample_rate(), Some(192_000));
        let ms = fichier.properties().duration().as_millis() as i64;
        assert!(
            (ms - 500).abs() <= 5,
            "0,5 s de DSD128 en 192 kHz : {ms} ms écrits"
        );
    }
}
