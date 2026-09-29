//! #5297 — une piste d'image SACD emprunte le chemin DSD EXISTANT.
//!
//! L'épreuve ne compare pas le PCM à une valeur figée : elle compare ce que
//! rend la piste lue DANS L'ISO à ce que rend le même DSD rangé dans un
//! `.dff` — format que Tune lisait déjà, par les mêmes convertisseurs. Si
//! l'ISO passe par un second chemin, choisit la mauvaise trame, se trompe
//! d'ordre des bits ou joue le disque entier, les deux sorties divergent.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::sacd::fabrique::{ImageFabriquee, octets_attendus};
use super::sacd::{OCTETS_PAR_TRAME_ET_CANAL, TRAMES_PAR_SECONDE, trames_en_ms};

const CADENCE: u32 = 176_400;

/// Un DSDIFF minimal, écrit ici à la main : FRM8, PROP (FS, CHNL, CMPR `DSD `),
/// puis le chunk `DSD ` qui porte `dsd` tel quel.
fn ecrire_dff(chemin: &Path, dsd: &[u8]) {
    let mut prop = Vec::new();
    prop.extend_from_slice(b"SND ");
    prop.extend_from_slice(b"FS  ");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(&2_822_400u32.to_be_bytes());
    prop.extend_from_slice(b"CHNL");
    prop.extend_from_slice(&2u64.to_be_bytes());
    prop.extend_from_slice(&2u16.to_be_bytes());
    prop.extend_from_slice(b"CMPR");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(b"DSD ");
    let mut f = Vec::new();
    f.extend_from_slice(b"FRM8");
    f.extend_from_slice(&(4 + 12 + prop.len() as u64 + 12 + dsd.len() as u64).to_be_bytes());
    f.extend_from_slice(b"DSD ");
    f.extend_from_slice(b"PROP");
    f.extend_from_slice(&(prop.len() as u64).to_be_bytes());
    f.extend_from_slice(&prop);
    f.extend_from_slice(b"DSD ");
    f.extend_from_slice(&(dsd.len() as u64).to_be_bytes());
    f.extend_from_slice(dsd);
    std::fs::write(chemin, f).unwrap();
}

struct Banc {
    _dossier: tempfile::TempDir,
    iso: PathBuf,
    bornes: Vec<(u32, u32)>,
    dossier: PathBuf,
}

fn banc() -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let iso = dossier.path().join("Kind of Blue.iso");
    let bornes = super::sacd::fabrique::ecrire(&iso, &ImageFabriquee::deux_pistes());
    Banc {
        dossier: dossier.path().to_path_buf(),
        _dossier: dossier,
        iso,
        bornes,
    }
}

impl Banc {
    /// Le `.dff` qui porte les trames `[debut, fin)` de la zone.
    fn dff(&self, debut: u32, fin: u32) -> String {
        let chemin = self.dossier.join(format!("trames-{debut}-{fin}.dff"));
        ecrire_dff(&chemin, &octets_attendus(debut, fin));
        chemin.to_string_lossy().into_owned()
    }

    fn iso(&self) -> &str {
        self.iso.to_str().unwrap()
    }
}

fn secondes(trames: u32) -> f64 {
    f64::from(trames) / f64::from(TRAMES_PAR_SECONDE)
}

/// Les deux sorties coïncident échantillon par échantillon, sauf la traîne du
/// filtre : l'ISO continue sur la piste suivante là où le `.dff` s'arrête.
fn meme_pcm(nom: &str, iso: &[i32], dff: &[i32], attendus: usize) {
    assert!(
        iso.len() + 2 * 2048 >= attendus && dff.len() + 2 * 2048 >= attendus,
        "{nom} : {} et {} échantillons pour {attendus} attendus",
        iso.len(),
        dff.len()
    );
    let n = iso.len().min(dff.len()).saturating_sub(2 * 1024);
    assert!(n > attendus / 2, "{nom} : trop peu d'échantillons comparés");
    let premier_ecart = (0..n).find(|&i| iso[i] != dff[i]);
    assert_eq!(
        premier_ecart, None,
        "{nom} : le PCM tiré de l'ISO diverge de celui du DFF de la même piste"
    );
    assert!(iso[..n].iter().any(|&v| v != 0), "{nom} : PCM muet");
}

/// Le décodage complet (`decode_to_pcm`, celui du transcodage par fichier,
/// des analyses et des aperçus) lit chaque piste de l'ISO comme le DFF de
/// ses trames.
#[test]
fn une_piste_iso_se_decode_en_pcm_comme_le_dff_de_ses_trames() {
    let b = banc();
    for &(debut, fin) in &b.bornes {
        let duree = secondes(fin - debut);
        let iso = super::decode::decode_to_pcm(
            b.iso(),
            Some(CADENCE),
            None,
            // La position telle que la base la garde : des millisecondes.
            trames_en_ms(debut) as f64 / 1000.0,
            duree,
        )
        .expect("une piste d'ISO SACD en DSD brut se décode");
        let dff = super::decode::decode_to_pcm(&b.dff(debut, fin), Some(CADENCE), None, 0.0, duree)
            .unwrap();
        assert_eq!(iso.sample_rate, CADENCE);
        assert_eq!(iso.channels, 2);
        let attendus = (duree * f64::from(CADENCE)).round() as usize * 2;
        meme_pcm(
            &format!("piste {debut}..{fin}"),
            &iso.samples_i32,
            &dff.samples_i32,
            attendus,
        );
    }
}

/// Le déplacement : partir de 2 trames et 5 ms dans la piste 2 rend ce que
/// rend le DFF des trames restantes, déplacé de 5 ms.
#[test]
fn le_deplacement_dans_une_piste_iso_part_du_bon_instant() {
    let b = banc();
    let (debut, fin) = b.bornes[1];
    let depart = debut + 2;
    let seek_s = (trames_en_ms(depart) + 5) as f64 / 1000.0;
    let reste = secondes(fin - depart) - 0.005;
    let iso = super::decode::decode_to_pcm(b.iso(), Some(CADENCE), None, seek_s, reste).unwrap();
    let dff = super::decode::decode_to_pcm(
        &b.dff(depart, fin),
        Some(CADENCE),
        None,
        // Le reste à retrancher, sur l'horloge exacte de la trame (1/75 s).
        seek_s - secondes(depart),
        reste,
    )
    .unwrap();
    let attendus = (reste * f64::from(CADENCE)).round() as usize * 2;
    meme_pcm("déplacement", &iso.samples_i32, &dff.samples_i32, attendus);
}

/// Le DoP d'une tranche d'ISO est, octet pour octet, celui du DFF de la
/// piste : ni la piste voisine, ni le disque entier.
#[test]
fn le_dop_d_une_piste_iso_est_celui_du_dff_de_la_piste() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _garde = rt.enter();
    let b = banc();
    let lire = |chemin: &str, ext: &str, tranche: Option<(u64, u64)>| -> Vec<u8> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let mut premier = false;
        match tranche {
            Some((debut_ms, fin_ms)) => super::decode::decode_dsd_to_dop_streaming_tranche(
                chemin,
                ext,
                debut_ms,
                Some(fin_ms),
                tx,
                65536,
                &mut premier,
                &None,
                rt.handle(),
            ),
            None => super::decode::decode_dsd_to_dop_streaming(
                chemin,
                ext,
                tx,
                65536,
                &mut premier,
                &None,
                rt.handle(),
            ),
        }
        .expect("DoP");
        let mut tout = Vec::new();
        while let Ok(bloc) = rx.try_recv() {
            tout.extend(bloc);
        }
        tout
    };
    for &(debut, fin) in &b.bornes {
        let iso = lire(
            b.iso(),
            "iso",
            Some((trames_en_ms(debut), trames_en_ms(fin))),
        );
        let dff = lire(&b.dff(debut, fin), "dff", None);
        // 24 bits × 2 canaux par trame DoP, 2 octets DSD par canal et trame.
        let attendus = (fin - debut) as usize * OCTETS_PAR_TRAME_ET_CANAL / 2 * 6;
        assert_eq!(iso.len(), attendus, "trames {debut}..{fin} : longueur DoP");
        assert!(iso == dff, "trames {debut}..{fin} : DoP différent du DFF");
    }
}

/// Le bras progressif (`decode_to_pcm_streaming_tranche`, celui des zones
/// réseau et du navigateur) : même PCM que le DFF, en-tête WAV compris.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_bras_progressif_lit_la_tranche_iso_comme_le_dff() {
    let b = Arc::new(banc());
    let (debut, fin) = b.bornes[1];
    let duree = secondes(fin - debut);
    let dff = b.dff(debut, fin);
    let decoder = |chemin: String, seek_s: f64| async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        let pret = Arc::new(tokio::sync::Notify::new());
        let (niveaux, _garde) = tokio::sync::mpsc::unbounded_channel();
        let tache = tokio::task::spawn_blocking(move || {
            super::decode::decode_to_pcm_streaming_tranche(
                &chemin,
                Some(CADENCE),
                Some(2),
                Some(24),
                tx,
                32768,
                pret,
                niveaux,
                seek_s,
                Some(duree),
            )
        });
        let mut tout = Vec::new();
        while let Some(bloc) = rx.recv().await {
            tout.extend(bloc);
        }
        tache.await.unwrap().expect("décodage progressif");
        tout
    };
    let iso = decoder(b.iso().to_string(), trames_en_ms(debut) as f64 / 1000.0).await;
    let dff = decoder(dff, 0.0).await;
    assert_eq!(&iso[..44], &dff[..44], "même en-tête WAV");
    let en_i32 = |octets: &[u8]| -> Vec<i32> {
        octets[44..]
            .chunks_exact(3)
            .map(|c| i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8)
            .collect()
    };
    let attendus = (duree * f64::from(CADENCE)).round() as usize * 2;
    meme_pcm("progressif", &en_i32(&iso), &en_i32(&dff), attendus);
}

/// Une image DST est refusée par le décodeur avec son motif, jamais lue.
#[test]
fn le_decodeur_refuse_une_image_dst_en_la_nommant() {
    let dossier = tempfile::tempdir().unwrap();
    let iso = dossier.path().join("dst.iso");
    let mut image = ImageFabriquee::deux_pistes();
    image.stereo_dst = true;
    super::sacd::fabrique::ecrire(&iso, &image);
    let erreur = super::decode::decode_to_pcm(iso.to_str().unwrap(), Some(CADENCE), None, 0.0, 0.1)
        .err()
        .expect("une image DST ne se décode pas");
    assert!(
        erreur.contains(super::sacd::MOTIF_ISO_SACD_DST),
        "le refus doit nommer le DST : {erreur}"
    );
}
