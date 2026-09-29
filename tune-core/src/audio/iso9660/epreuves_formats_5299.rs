//! #5299, suite — AIFF, DSF, DFF, APE, WavPack, Opus et Matroska rangés dans
//! une image ISO de données s'indexent et se lisent comme hors de l'image.
//!
//! La 0.9.168 ne livrait de l'image que FLAC, MP3, M4A, Ogg et WAV : les
//! lecteurs propres aux autres formats ouvraient le fichier PAR SON CHEMIN
//! (`File::open`), qu'une image ne fournit pas. Ils passent désormais par
//! [`super::ouvrir_fichier`].
//!
//! Aucun fichier sous droits : les fixtures sont des signaux synthétiques du
//! dépôt (sinusoïdes, modulateur sigma-delta écrit dans
//! `tests/fixtures/dsd/generer_fixtures_dsd.py`), l'Opus est encodé ici d'une
//! sinusoïde, le Matroska est ré-emballé ici depuis un FLAC de référence.

use super::fabrique::{self, Noms};
use super::*;
use std::path::{Path, PathBuf};

const DOSSIER: &str = "Un artiste - Formats variés";

/// (nom interne, octets) des fichiers de l'image, un par format retenu.
fn fichiers_synthetiques() -> Vec<(String, Vec<u8>)> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let lire = |relatif: &str| std::fs::read(fixtures.join(relatif)).unwrap();
    // Une seconde de sinusoïde stéréo à 440 Hz, 48 kHz, encodée en Opus.
    let pcm: Vec<i16> = (0..48_000)
        .flat_map(|n| {
            let v =
                (8_000.0 * (2.0 * std::f64::consts::PI * 440.0 * n as f64 / 48_000.0).sin()) as i16;
            [v, v]
        })
        .collect();
    let opus = crate::audio::opus_ogg::encode_ogg_opus(&pcm, 2, 128, 48_000).unwrap();
    let mka = crate::audio::matroska::muxer_de_test::mka_depuis_flac(
        &fixtures.join("flac/ref_16_44100_stereo.flac"),
        2,
        true,
        &[],
        &[],
    );
    vec![
        (
            "01 - Un titre en AIFF.aiff".into(),
            lire("aiff/ref_16_44100_stereo.aiff"),
        ),
        (
            "02 - Un titre en DSF.dsf".into(),
            lire("dsd/ref_dsd64_stereo.dsf"),
        ),
        (
            "03 - Un titre en DFF.dff".into(),
            lire("dsd/ref_dsd64_stereo.dff"),
        ),
        (
            "04 - Un titre en APE.ape".into(),
            lire("ape/sine_16s_c3000.ape"),
        ),
        (
            "05 - Un titre en WavPack.wv".into(),
            lire("wavpack/mono_8_44100.wv"),
        ),
        ("06 - Un titre en Opus.opus".into(), opus),
        ("07 - Un titre en Matroska.mka".into(), mka),
    ]
}

struct Banc {
    _dossier: tempfile::TempDir,
    racine: PathBuf,
    image: PathBuf,
    /// (nom interne complet, octets, copie hors de l'image).
    pistes: Vec<(String, Vec<u8>, PathBuf)>,
}

fn banc() -> Banc {
    // Sous `target/`, jamais sous le dossier temporaire du système : le
    // parcours écarte tout ce qui y vit. Nom unique : Shrek est partagée.
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    std::fs::create_dir_all(&parent).unwrap();
    let dossier = tempfile::Builder::new()
        .prefix("tune-iso-formats-5299-")
        .tempdir_in(&parent)
        .unwrap();
    let racine = dossier.path().join("bibliotheque");
    let hors = dossier.path().join("hors-image").join(DOSSIER);
    std::fs::create_dir_all(&racine).unwrap();
    std::fs::create_dir_all(&hors).unwrap();
    let mut pistes = Vec::new();
    let mut contenu = Vec::new();
    for (nom, octets) in fichiers_synthetiques() {
        let copie = hors.join(&nom);
        std::fs::write(&copie, &octets).unwrap();
        let interne = format!("{DOSSIER}/{nom}");
        contenu.push((interne.clone(), octets.clone()));
        pistes.push((interne, octets, copie));
    }
    let image = racine.join("Disque de donnees.iso");
    std::fs::write(&image, fabrique::iso(&contenu, Noms::Joliet)).unwrap();
    Banc {
        _dossier: dossier,
        racine,
        image,
        pistes,
    }
}

fn virtuel(b: &Banc, interne: &str) -> String {
    chemin_virtuel(&b.image, interne)
}

/// Le parcours admet chacun de ces formats comme piste de l'image, et n'en
/// écarte aucun sous `iso-format-non-lu`.
#[test]
fn le_parcours_admet_les_sept_formats_de_l_image_5299() {
    let b = banc();
    let r = crate::scanner::walker::list_audio_files(&[b.racine.to_string_lossy().into_owned()]);
    let mut vus: Vec<String> = r
        .files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    vus.sort();
    let mut attendus: Vec<String> = b.pistes.iter().map(|(i, _, _)| virtuel(&b, i)).collect();
    attendus.sort();
    assert_eq!(
        vus, attendus,
        "#5299 : AIFF, DSF, DFF, APE, WavPack, Opus et Matroska de l'image doivent entrer"
    );
    assert_eq!(
        r.skipped_by_ext.get(CLE_RAPPORT_FORMAT_DANS_IMAGE),
        None,
        "#5299 : aucun format lu hors de l'image ne doit être écarté dans l'image : {:?}",
        r.skipped_paths
    );
}

/// Le format d'un nom interne, pour nommer chaque verdict.
fn format_de(interne: &str) -> &str {
    interne.rsplit('.').next().unwrap_or(interne)
}

/// Échoue en nommant CHAQUE format en défaut, pas seulement le premier : la
/// contre-épreuve doit montrer que chaque format dépend du correctif.
fn verdict(defauts: Vec<String>) {
    assert!(
        defauts.is_empty(),
        "#5299 : {} format(s) en défaut dans l'image :\n{}",
        defauts.len(),
        defauts.join("\n")
    );
}

/// Les propriétés lues dans l'image sont celles du même fichier hors de
/// l'image ; la taille est la taille propre du fichier interne.
#[test]
fn les_metadonnees_des_sept_formats_se_lisent_dans_l_image_5299() {
    let b = banc();
    let dans: Vec<PathBuf> = b
        .pistes
        .iter()
        .map(|(i, _, _)| PathBuf::from(virtuel(&b, i)))
        .collect();
    let hors: Vec<PathBuf> = b.pistes.iter().map(|(_, _, c)| c.clone()).collect();
    let (lus, _) = crate::scanner::walker::scan_files_parallel(&dans, true, None);
    let (refs, _) = crate::scanner::walker::scan_files_parallel(&hors, false, None);
    let mut defauts = Vec::new();
    for (interne, octets, copie) in &b.pistes {
        let f = format_de(interne);
        let v = virtuel(&b, interne);
        let lu = lus.iter().find(|l| l.path == v);
        let reference = refs
            .iter()
            .find(|l| Path::new(&l.path) == copie.as_path())
            .and_then(|l| l.metadata.as_ref())
            .expect("métadonnées hors de l'image");
        let Some(lu) = lu else {
            defauts.push(format!("{f} : absent du résultat"));
            continue;
        };
        if let Some(refus) = &lu.unsupported {
            defauts.push(format!("{f} : refusé : {refus:?}"));
            continue;
        }
        let Some(m) = lu.metadata.as_ref() else {
            defauts.push(format!("{f} : métadonnées illisibles"));
            continue;
        };
        let proprietes = |m: &crate::metadata::TrackMetadata| {
            (m.sample_rate, m.channels, m.bit_depth, m.duration_ms)
        };
        if proprietes(m) != proprietes(reference) {
            defauts.push(format!(
                "{f} : (cadence, canaux, bits, durée) {:?} dans l'image, {:?} hors de l'image",
                proprietes(m),
                proprietes(reference)
            ));
        } else if m.sample_rate.is_none() || !m.duration_ms.is_some_and(|d| d > 0) {
            defauts.push(format!(
                "{f} : cadence ou durée absente : {:?}",
                proprietes(m)
            ));
        }
        if lu.file_size != octets.len() as u64 {
            defauts.push(format!(
                "{f} : taille {} au lieu de {}",
                lu.file_size,
                octets.len()
            ));
        }
    }
    verdict(defauts);
}

/// Décodage entier : dans l'image, les mêmes échantillons que hors de l'image.
#[test]
fn le_decodage_des_sept_formats_rend_le_meme_pcm_dans_l_image_5299() {
    let b = banc();
    let mut defauts = Vec::new();
    for (interne, _, copie) in &b.pistes {
        let f = format_de(interne);
        let hors =
            crate::audio::decode::decode_to_pcm(copie.to_str().unwrap(), None, None, 0.0, 0.0)
                .unwrap();
        assert!(
            !hors.samples_i32.is_empty(),
            "{f} : rien de décodé hors de l'image"
        );
        match crate::audio::decode::decode_to_pcm(&virtuel(&b, interne), None, None, 0.0, 0.0) {
            Err(e) => defauts.push(format!("{f} : ne se décode pas : {e}")),
            Ok(dans) => {
                if (dans.sample_rate, dans.channels, dans.bit_depth)
                    != (hors.sample_rate, hors.channels, hors.bit_depth)
                    || dans.samples_i32 != hors.samples_i32
                {
                    defauts.push(format!(
                        "{f} : {} échantillons dans l'image, {} hors de l'image, contenus différents",
                        dans.samples_i32.len(),
                        hors.samples_i32.len()
                    ));
                }
            }
        }
    }
    verdict(defauts);
}

/// Les premiers octets PCM du chemin de LECTURE (le décodage progressif que
/// l'orchestrateur emprunte) sont ceux du fichier hors de l'image.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_lecture_progressive_rend_les_premiers_octets_pcm_5299() {
    const PREMIERS: usize = 16 * 1024;
    async fn premiers_octets(chemin: String) -> Result<Vec<u8>, String> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let travail = tokio::task::spawn_blocking(move || {
            crate::audio::decode::decode_to_pcm_streaming(&chemin, None, None, tx, 4096)
        });
        let mut recu = Vec::new();
        while recu.len() < PREMIERS {
            match rx.recv().await {
                Some(bloc) => recu.extend_from_slice(&bloc),
                None => break,
            }
        }
        drop(rx);
        travail.await.unwrap()?;
        recu.truncate(PREMIERS);
        Ok(recu)
    }
    let b = banc();
    let mut defauts = Vec::new();
    for (interne, _, copie) in &b.pistes {
        let f = format_de(interne);
        let hors = premiers_octets(copie.to_string_lossy().into_owned())
            .await
            .unwrap();
        assert!(!hors.is_empty(), "{f} : aucun octet PCM hors de l'image");
        match premiers_octets(virtuel(&b, interne)).await {
            Err(e) => defauts.push(format!("{f} : ne se lit pas : {e}")),
            Ok(dans) if dans != hors => defauts.push(format!(
                "{f} : {} octets PCM dans l'image, {} hors de l'image, contenus différents",
                dans.len(),
                hors.len()
            )),
            Ok(_) => {}
        }
    }
    verdict(defauts);
}
