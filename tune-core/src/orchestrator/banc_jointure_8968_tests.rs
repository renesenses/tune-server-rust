//! #8968 (PR 0) — banc de mesure de la JOINTURE entre deux pistes.
//!
//! Un même signal de référence (un sinus glissant stéréo, 200 → 2 000 Hz, qui
//! ne se répète jamais : un décalage d'un échantillon se voit) est coupé en
//! deux fichiers, ou découpé en trois pistes par une feuille CUE. Chaque
//! chemin de lecture testable sans matériel décode les morceaux ; le banc les
//! remet bout à bout et compare au signal d'origine :
//!
//! * **écart de longueur** : trames rendues moins trames attendues, par
//!   morceau. Une amorce d'encodeur non retirée, un remplissage de fin, une
//!   borne CUE arrondie s'y lisent directement ;
//! * **décalage** : où le morceau commence vraiment dans le signal, par
//!   rapport à où il devrait commencer (recherche du meilleur alignement à
//!   ±3 000 trames) ;
//! * **erreur à la jointure** : écart crête au signal de référence, sur
//!   ±256 trames autour de chaque frontière du flux recollé (pleine échelle
//!   = 1,0). Un blanc ou un saut s'y voit comme une erreur de l'ordre de
//!   l'amplitude du signal (0,4).
//!
//! « Sans blanc » veut dire : écarts de longueur nuls, décalages nuls, et
//! erreur à la jointure nulle pour un format sans perte (sous un seuil pour un
//! format avec perte, dont le décodage n'est jamais exact).
//!
//! Chemins mesurés (aucun matériel, aucun son) :
//!
//! * **piste à piste** : la session de décodage progressif
//!   (`decode_to_pcm_streaming_tranche`, sans borne), celle qui alimente les
//!   sorties locales et le transcodage réseau ;
//! * **décodage complet** : `decode_to_pcm`, le bras « fichier » du
//!   transcodage (renderer qui exige `Content-Length`) ;
//! * **tranche CUE** : les mêmes deux chemins, avec le début et la durée que
//!   `resolve_local` tire aujourd'hui des millisecondes de la base ;
//! * **flux continu #8082** : `flux_continu_8082::assembler`, puis le WAV
//!   virtuel lu d'un bout à l'autre.
//!
//! Un cas qui échoue aujourd'hui est marqué [`Attendu::EchecConnu`] avec la PR
//! qui le corrige : le banc VÉRIFIE qu'il échoue encore. Le jour où il passe,
//! le banc rougit et demande de retirer le marqueur — il ne peut pas devenir
//! silencieusement faux.
//!
//! Formats : FLAC (encodeur de Tune), WAV, AAC en M4A (encodeur AAC-LC de
//! `rusty_aac`, conteneur MP4 écrit ici avec `iTunSMPB` ou `elst`), Opus
//! (libopus, conteneur Ogg écrit ici) et CUE (image FLAC ou WAV). **MP3 : aucun
//! encodeur MP3 dans le dépôt** (ni en Rust, ni en dépendance) ; le banc ne
//! peut donc pas couper un signal connu en deux MP3. Il mesure à la place la
//! longueur décodée de la fixture `test.mp3` (en-tête LAME : 576 trames de
//! délai, 1 404 de remplissage, 44 100 utiles), la propriété dont dépend la
//! jointure. Les fichiers sont écrits dans un dossier temporaire et ne sont
//! jamais joués.

use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use super::flux_continu_8082;

/// Cadence des fichiers PCM et AAC du banc.
const CADENCE: u32 = 44_100;
/// Opus vit à 48 kHz.
const CADENCE_OPUS: u32 = 48_000;
/// Amplitude du signal de référence (pleine échelle = 1,0).
const AMPLITUDE: f64 = 0.4;
/// Recherche de l'alignement d'un morceau : ± tant de trames.
const RECHERCHE: i64 = 3_000;
/// Fenêtre de mesure de l'erreur autour d'une frontière.
const FENETRE_JOINTURE: usize = 256;
/// Seuil de l'erreur à la jointure pour un format AVEC perte. Mesuré au banc
/// (voir la PR) : le décodage AAC ou Opus d'un sinus glissant s'écarte de
/// quelques centièmes de la référence ; un blanc ou une jointure décalée
/// approche l'amplitude (0,4).
const SEUIL_AVEC_PERTE: f64 = 0.15;

// ---------------------------------------------------------------------------
// Le signal de référence
// ---------------------------------------------------------------------------

/// Sinus glissant stéréo entrelacé, `trames` trames à `cadence`.
fn reference(cadence: u32, trames: usize) -> Vec<f64> {
    let duree = trames as f64 / f64::from(cadence);
    let (f0, f1) = (200.0, 2_000.0);
    let mut v = Vec::with_capacity(trames * 2);
    for n in 0..trames {
        let t = n as f64 / f64::from(cadence);
        let phase = std::f64::consts::TAU * (f0 * t + (f1 - f0) * t * t / (2.0 * duree));
        v.push(AMPLITUDE * phase.sin());
        v.push(AMPLITUDE * (phase + 1.0).sin() * 0.9);
    }
    v
}

fn en_i16(signal: &[f64]) -> Vec<i16> {
    signal
        .iter()
        .map(|x| (x * 32_767.0).round() as i16)
        .collect()
}

/// La référence telle que la porte un fichier 16 bits (quantifiée).
fn reference_16(cadence: u32, trames: usize) -> Vec<f64> {
    en_i16(&reference(cadence, trames))
        .into_iter()
        .map(|s| f64::from(s) / 32_768.0)
        .collect()
}

// ---------------------------------------------------------------------------
// Fabrique des fichiers
// ---------------------------------------------------------------------------

fn octets_le(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn ecrire_wav(chemin: &Path, cadence: u32, pcm: &[i16]) {
    let donnees = octets_le(pcm);
    let mut f =
        crate::audio::wav::build_wav_header_with_data_size(2, cadence, 16, donnees.len() as u32)
            .to_vec();
    f.extend_from_slice(&donnees);
    std::fs::write(chemin, f).unwrap();
}

fn ecrire_flac(chemin: &Path, cadence: u32, pcm: &[i16]) {
    let mut e = crate::audio::encoder::AudioEncoder::new("flac", cadence, 16, 2);
    e.start_sync().unwrap();
    e.write_sync(&octets_le(pcm)).unwrap();
    std::fs::write(chemin, e.finish_sync().unwrap()).unwrap();
}

/// Ce que le conteneur MP4 dit de l'amorce et du remplissage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GaplessMp4 {
    /// Métadonnée iTunes `----:com.apple.iTunes:iTunSMPB` (iTunes, afconvert).
    ItunSmpb,
    /// Liste d'édition `moov/trak/edts/elst` (ffmpeg, la plupart des muxeurs).
    Elst,
}

/// Délai de l'encodeur AAC-LC de `rusty_aac` : un bloc de 1 024 trames
/// (`encode_stream` code `ceil(n / 1024) + 1` blocs, le premier sur un passé
/// nul). Vérifié par [`l_encodeur_aac_du_banc_retarde_de_1024_trames`].
const AMORCE_AAC_DU_BANC: u32 = 1_024;

fn boite(nom: &[u8; 4], contenu: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(contenu.len() + 8);
    b.extend_from_slice(&(contenu.len() as u32 + 8).to_be_bytes());
    b.extend_from_slice(nom);
    b.extend_from_slice(contenu);
    b
}

fn boite_pleine(nom: &[u8; 4], version_drapeaux: u32, contenu: &[u8]) -> Vec<u8> {
    let mut c = version_drapeaux.to_be_bytes().to_vec();
    c.extend_from_slice(contenu);
    boite(nom, &c)
}

fn descripteur(tag: u8, contenu: &[u8]) -> Vec<u8> {
    let mut d = vec![tag, contenu.len() as u8];
    d.extend_from_slice(contenu);
    d
}

const MATRICE: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

/// Écrit un M4A AAC-LC stéréo : `ftyp`, `mdat`, puis `moov`. L'amorce et le
/// remplissage de l'encodeur sont décrits par `gapless`, comme le ferait
/// iTunes (`iTunSMPB`) ou ffmpeg (`elst`).
fn ecrire_m4a(chemin: &Path, cadence: u32, signal: &[f64], gapless: GaplessMp4) {
    let utiles = (signal.len() / 2) as u64;
    let mut e = rusty_aac::AacEncoder::new(rusty_aac::AacEncoderConfig {
        bitrate_bps: 192_000,
        ..Default::default()
    });
    let f32s: Vec<f32> = signal.iter().map(|x| *x as f32).collect();
    e.push_pcm(&f32s, 2, cadence).expect("encodage AAC-LC");
    e.finish();
    let mut paquets = Vec::new();
    while let Ok(p) = e.next_packet() {
        paquets.push(p.data);
    }
    let total = paquets.len() as u64 * 1_024;
    let amorce = u64::from(AMORCE_AAC_DU_BANC);
    assert!(
        total >= amorce + utiles,
        "l'encodeur a rendu trop peu de blocs"
    );
    let remplissage = total - amorce - utiles;

    let ftyp = boite(b"ftyp", b"M4A \0\0\x02\0M4A mp42isom");
    let mdat_contenu: Vec<u8> = paquets.iter().flatten().copied().collect();
    let mdat = boite(b"mdat", &mdat_contenu);
    let decalage_mdat = (ftyp.len() + 8) as u32;

    // mvhd : échelle de temps du film = cadence (la durée d'edition s'y
    // exprime en trames).
    let mut mvhd = Vec::new();
    for v in [0u32, 0, cadence, utiles as u32, 0x0001_0000] {
        mvhd.extend_from_slice(&v.to_be_bytes());
    }
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes());
    mvhd.extend_from_slice(&[0u8; 10]);
    for m in MATRICE {
        mvhd.extend_from_slice(&m.to_be_bytes());
    }
    mvhd.extend_from_slice(&[0u8; 24]);
    mvhd.extend_from_slice(&2u32.to_be_bytes());
    let mvhd = boite_pleine(b"mvhd", 0, &mvhd);

    let mut tkhd = Vec::new();
    for v in [0u32, 0, 1, 0, utiles as u32, 0, 0] {
        tkhd.extend_from_slice(&v.to_be_bytes());
    }
    tkhd.extend_from_slice(&[0, 0, 0, 0, 0x01, 0x00, 0, 0]);
    for m in MATRICE {
        tkhd.extend_from_slice(&m.to_be_bytes());
    }
    tkhd.extend_from_slice(&[0u8; 8]);
    let tkhd = boite_pleine(b"tkhd", 7, &tkhd);

    let mut mdhd = Vec::new();
    for v in [0u32, 0, cadence, total as u32] {
        mdhd.extend_from_slice(&v.to_be_bytes());
    }
    mdhd.extend_from_slice(&[0x55, 0xC4, 0, 0]);
    let mdhd = boite_pleine(b"mdhd", 0, &mdhd);
    let mut hdlr = vec![0u8; 4];
    hdlr.extend_from_slice(b"soun");
    hdlr.extend_from_slice(&[0u8; 13]);
    let hdlr = boite_pleine(b"hdlr", 0, &hdlr);

    let asc = rusty_aac::encode::audio_specific_config_bytes(cadence, 2);
    let mut config = vec![0x40, 0x15, 0, 0, 0];
    config.extend_from_slice(&384_000u32.to_be_bytes());
    config.extend_from_slice(&192_000u32.to_be_bytes());
    config.extend_from_slice(&descripteur(0x05, &asc));
    let mut es = vec![0, 1, 0];
    es.extend_from_slice(&descripteur(0x04, &config));
    es.extend_from_slice(&descripteur(0x06, &[0x02]));
    let esds = boite_pleine(b"esds", 0, &descripteur(0x03, &es));
    let mut mp4a = vec![0u8; 6];
    mp4a.extend_from_slice(&1u16.to_be_bytes());
    mp4a.extend_from_slice(&[0u8; 8]);
    mp4a.extend_from_slice(&2u16.to_be_bytes());
    mp4a.extend_from_slice(&16u16.to_be_bytes());
    mp4a.extend_from_slice(&[0u8; 4]);
    mp4a.extend_from_slice(&(cadence << 16).to_be_bytes());
    mp4a.extend_from_slice(&esds);
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend_from_slice(&boite(b"mp4a", &mp4a));
    let stsd = boite_pleine(b"stsd", 0, &stsd);
    let mut stts = 1u32.to_be_bytes().to_vec();
    stts.extend_from_slice(&(paquets.len() as u32).to_be_bytes());
    stts.extend_from_slice(&1_024u32.to_be_bytes());
    let stts = boite_pleine(b"stts", 0, &stts);
    let mut stsc = 1u32.to_be_bytes().to_vec();
    for v in [1u32, paquets.len() as u32, 1] {
        stsc.extend_from_slice(&v.to_be_bytes());
    }
    let stsc = boite_pleine(b"stsc", 0, &stsc);
    let mut stsz = 0u32.to_be_bytes().to_vec();
    stsz.extend_from_slice(&(paquets.len() as u32).to_be_bytes());
    for p in &paquets {
        stsz.extend_from_slice(&(p.len() as u32).to_be_bytes());
    }
    let stsz = boite_pleine(b"stsz", 0, &stsz);
    let mut stco = 1u32.to_be_bytes().to_vec();
    stco.extend_from_slice(&decalage_mdat.to_be_bytes());
    let stco = boite_pleine(b"stco", 0, &stco);
    let stbl = boite(b"stbl", &[stsd, stts, stsc, stsz, stco].concat());
    let dref = boite_pleine(
        b"dref",
        0,
        &[1u32.to_be_bytes().to_vec(), boite_pleine(b"url ", 1, &[])].concat(),
    );
    let dinf = boite(b"dinf", &dref);
    let smhd = boite_pleine(b"smhd", 0, &[0u8; 4]);
    let minf = boite(b"minf", &[smhd, dinf, stbl].concat());
    let mdia = boite(b"mdia", &[mdhd, hdlr, minf].concat());

    let mut trak = tkhd;
    if gapless == GaplessMp4::Elst {
        let mut elst = 1u32.to_be_bytes().to_vec();
        elst.extend_from_slice(&(utiles as u32).to_be_bytes());
        elst.extend_from_slice(&(amorce as i32).to_be_bytes());
        elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        trak.extend_from_slice(&boite(b"edts", &boite_pleine(b"elst", 0, &elst)));
    }
    trak.extend_from_slice(&mdia);
    let trak = boite(b"trak", &trak);

    let mut moov = [mvhd, trak].concat();
    if gapless == GaplessMp4::ItunSmpb {
        let valeur = format!(
            " 00000000 {amorce:08X} {remplissage:08X} {utiles:016X} 00000000 00000000 \
             00000000 00000000 00000000 00000000 00000000 00000000"
        );
        let mut donnees = 1u32.to_be_bytes().to_vec();
        donnees.extend_from_slice(&0u32.to_be_bytes());
        donnees.extend_from_slice(valeur.as_bytes());
        let libre = boite(
            b"----",
            &[
                boite_pleine(b"mean", 0, b"com.apple.iTunes"),
                boite_pleine(b"name", 0, b"iTunSMPB"),
                boite(b"data", &donnees),
            ]
            .concat(),
        );
        let mut hdlr_meta = vec![0u8; 4];
        hdlr_meta.extend_from_slice(b"mdirappl");
        hdlr_meta.extend_from_slice(&[0u8; 9]);
        let meta = boite_pleine(
            b"meta",
            0,
            &[boite_pleine(b"hdlr", 0, &hdlr_meta), boite(b"ilst", &libre)].concat(),
        );
        moov.extend_from_slice(&boite(b"udta", &meta));
    }
    let moov = boite(b"moov", &moov);
    std::fs::write(chemin, [ftyp, mdat, moov].concat()).unwrap();
}

/// CRC des pages Ogg (polynôme 0x04C11DB7, sans réflexion).
fn crc_ogg(octets: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &o in octets {
        crc ^= u32::from(o) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn page_ogg(numero: u32, granule: u64, drapeaux: u8, paquets: &[&[u8]]) -> Vec<u8> {
    let mut segments = Vec::new();
    for p in paquets {
        let mut reste = p.len();
        loop {
            let s = reste.min(255);
            segments.push(s as u8);
            reste -= s;
            if s < 255 {
                break;
            }
        }
    }
    let mut page = b"OggS".to_vec();
    page.push(0);
    page.push(drapeaux);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&0x5444_4E45u32.to_le_bytes());
    page.extend_from_slice(&numero.to_le_bytes());
    page.extend_from_slice(&0u32.to_le_bytes());
    page.push(segments.len() as u8);
    page.extend_from_slice(&segments);
    for p in paquets {
        page.extend_from_slice(p);
    }
    let crc = crc_ogg(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

/// Écrit un Ogg Opus stéréo 48 kHz. Contrairement à
/// `audio::opus_ogg::encode_ogg_opus`, l'encodeur est VIDÉ (des trames de
/// silence le poussent au-delà de son anticipation) : sans cela les
/// `pre_skip` dernières trames du signal ne sont jamais codées, et le banc
/// mesurerait la fabrique, pas la lecture.
fn ecrire_opus(chemin: &Path, pcm: &[i16]) {
    let mut e = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
        .expect("encodeur opus");
    e.set_bitrate(opus::Bitrate::Bits(192_000)).unwrap();
    let pre_skip = e.get_lookahead().unwrap() as u64;
    let utiles = (pcm.len() / 2) as u64;
    let a_coder = utiles + pre_skip;
    let mut paquets: Vec<Vec<u8>> = Vec::new();
    let mut trame = vec![0i16; 960 * 2];
    let mut sortie = vec![0u8; 4_000];
    let mut pos = 0u64;
    while pos < a_coder {
        for i in 0..960u64 {
            let n = (pos + i) as usize;
            let (g, d) = if n < pcm.len() / 2 {
                (pcm[2 * n], pcm[2 * n + 1])
            } else {
                (0, 0)
            };
            trame[2 * i as usize] = g;
            trame[2 * i as usize + 1] = d;
        }
        let n = e.encode(&trame, &mut sortie).expect("encodage opus");
        paquets.push(sortie[..n].to_vec());
        pos += 960;
    }
    let mut tete = b"OpusHead".to_vec();
    tete.push(1);
    tete.push(2);
    tete.extend_from_slice(&(pre_skip as u16).to_le_bytes());
    tete.extend_from_slice(&48_000u32.to_le_bytes());
    tete.extend_from_slice(&0i16.to_le_bytes());
    tete.push(0);
    let mut balises = b"OpusTags".to_vec();
    balises.extend_from_slice(&4u32.to_le_bytes());
    balises.extend_from_slice(b"banc");
    balises.extend_from_slice(&0u32.to_le_bytes());
    let mut o = page_ogg(0, 0, 0x02, &[&tete]);
    o.extend(page_ogg(1, 0, 0, &[&balises]));
    let lots: Vec<&[Vec<u8>]> = paquets.chunks(25).collect();
    let mut faits = 0u64;
    for (i, lot) in lots.iter().enumerate() {
        faits += 960 * lot.len() as u64;
        let dernier = i + 1 == lots.len();
        // La dernière page porte la longueur EXACTE : pre-skip + utiles.
        let granule = if dernier { pre_skip + utiles } else { faits };
        let refs: Vec<&[u8]> = lot.iter().map(|p| p.as_slice()).collect();
        o.extend(page_ogg(
            2 + i as u32,
            granule,
            if dernier { 0x04 } else { 0 },
            &refs,
        ));
    }
    std::fs::write(chemin, o).unwrap();
}

// ---------------------------------------------------------------------------
// Les chemins de lecture
// ---------------------------------------------------------------------------

/// Session de décodage progressif, 16 bits, stéréo, à la cadence du fichier
/// (`resolve_local` passe toujours la cadence de sortie : sans elle la borne
/// de fin d'une tranche ne se calcule pas) ; `(début, durée)` en secondes
/// comme les passe `resolve_local`. Rend le signal stéréo entrelacé, pleine
/// échelle 1,0.
async fn par_la_session(
    chemin: &Path,
    cadence: u32,
    debut_s: f64,
    duree_s: Option<f64>,
) -> Vec<f64> {
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    let pret = std::sync::Arc::new(tokio::sync::Notify::new());
    let (niveaux, _niveaux_rx) = tokio::sync::mpsc::unbounded_channel();
    let fp = chemin.to_string_lossy().into_owned();
    let tache = tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming_tranche(
            &fp,
            Some(cadence),
            Some(2),
            Some(16),
            tx,
            32_768,
            pret,
            niveaux,
            debut_s,
            duree_s,
        )
    });
    let mut octets = Vec::new();
    while let Some(bloc) = rx.recv().await {
        octets.extend_from_slice(&bloc);
    }
    tache.await.unwrap().expect("décodage progressif");
    assert!(
        octets.len() >= 44 && &octets[..4] == b"RIFF",
        "en-tête WAV attendu"
    );
    octets[44..]
        .chunks_exact(2)
        .map(|c| f64::from(i16::from_le_bytes([c[0], c[1]])) / 32_768.0)
        .collect()
}

/// Décodage complet (`decode_to_pcm`), le bras « fichier » du transcodage.
fn par_le_decodage_complet(chemin: &Path, debut_s: f64, duree_s: f64) -> Vec<f64> {
    let d = crate::audio::decode::decode_to_pcm(
        &chemin.to_string_lossy(),
        None,
        Some(2),
        debut_s,
        duree_s,
    )
    .expect("décodage complet");
    let echelle = f64::from(1u32 << (d.bit_depth - 1));
    d.samples_i32
        .iter()
        .map(|s| f64::from(*s) / echelle)
        .collect()
}

/// Le flux continu #8082, assemblé comme en production, lu d'un bout à
/// l'autre puis recoupé à ses frontières.
fn par_le_flux_continu(chemins: &[PathBuf]) -> Vec<Vec<f64>> {
    use crate::http::streamer::SourceVirtuelle;
    let suivantes = chemins[1..]
        .iter()
        .enumerate()
        .map(|(i, c)| (i as i64 + 1, i as i64 + 11, c.clone()))
        .collect();
    let f = flux_continu_8082::assembler((0, 10, chemins[0].clone()), 0, suivantes)
        .expect("flux continu assemblé");
    assert_eq!(f.lignes.len(), chemins.len(), "toutes les pistes admises");
    let mut octets = Vec::new();
    f.wav
        .lire(0, f.wav.total(), &mut |m| {
            octets.extend_from_slice(&m);
            Ok(())
        })
        .unwrap();
    assert_eq!(f.wav.format().profondeur, 16);
    let tout: Vec<f64> = octets[44..]
        .chunks_exact(2)
        .map(|c| f64::from(i16::from_le_bytes([c[0], c[1]])) / 32_768.0)
        .collect();
    let mut morceaux = Vec::new();
    let mut pos = 0usize;
    for s in f.wav.segments() {
        let n = s.trames as usize * 2;
        morceaux.push(tout[pos..pos + n].to_vec());
        pos += n;
    }
    morceaux
}

// ---------------------------------------------------------------------------
// La mesure
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Mesure {
    /// Trames rendues moins trames attendues, par morceau.
    ecarts_de_longueur: Vec<i64>,
    /// Décalage du contenu de chaque morceau : positif, le premier échantillon
    /// attendu n'arrive qu'après tant de trames (amorce non retirée, ou
    /// tranche qui commence trop tôt) ; négatif, le morceau commence trop
    /// tard.
    decalages: Vec<i64>,
    /// Écart crête au signal de référence autour des frontières du flux
    /// recollé.
    erreur_jointure: f64,
}

impl Mesure {
    fn sans_blanc(&self, seuil: f64) -> bool {
        self.ecarts_de_longueur.iter().all(|e| *e == 0)
            && self.decalages.iter().all(|d| *d == 0)
            && self.erreur_jointure <= seuil
    }
}

impl std::fmt::Display for Mesure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "écarts de longueur {:?} trames, décalages {:?} trames, erreur à la jointure {:.4}",
            self.ecarts_de_longueur, self.decalages, self.erreur_jointure
        )
    }
}

/// Décalage du morceau `m` (stéréo entrelacé) par rapport à la référence,
/// attendu à la trame `debut` : le `d` qui minimise l'écart quadratique entre
/// `m[n]` et `reference[debut + n - d]`, sur une fenêtre prise au milieu du
/// morceau (loin d'une éventuelle amorce).
fn decalage(reference: &[f64], m: &[f64], debut: usize) -> i64 {
    let trames = m.len() / 2;
    let (de, a) = if trames > 12_000 {
        (4_000, 8_000)
    } else {
        (trames / 3, trames / 3 + (trames / 3).min(4_000))
    };
    let total = (reference.len() / 2) as i64;
    let mut meilleur = (f64::INFINITY, 0i64);
    for d in -RECHERCHE..=RECHERCHE {
        let mut somme = 0.0;
        let mut ok = true;
        for n in de..a {
            let r = debut as i64 + n as i64 - d;
            if r < 0 || r >= total {
                ok = false;
                break;
            }
            let r = r as usize;
            let e0 = m[2 * n] - reference[2 * r];
            let e1 = m[2 * n + 1] - reference[2 * r + 1];
            somme += e0 * e0 + e1 * e1;
        }
        if ok && somme < meilleur.0 {
            meilleur = (somme, d);
        }
    }
    meilleur.1
}

/// Mesure des morceaux décodés contre la référence, chaque morceau étant
/// attendu à `(début, trames)` dans la référence.
fn mesurer(reference: &[f64], morceaux: &[Vec<f64>], attendus: &[(usize, usize)]) -> Mesure {
    assert_eq!(morceaux.len(), attendus.len());
    let ecarts_de_longueur = morceaux
        .iter()
        .zip(attendus)
        .map(|(m, (_, t))| (m.len() / 2) as i64 - *t as i64)
        .collect();
    let decalages = morceaux
        .iter()
        .zip(attendus)
        .map(|(m, (d, _))| decalage(reference, m, *d))
        .collect();
    // Le flux recollé, aligné sur le début du premier morceau attendu.
    let recolle: Vec<f64> = morceaux.iter().flatten().copied().collect();
    let origine = attendus[0].0;
    let mut erreur: f64 = 0.0;
    let mut frontiere = 0usize;
    for m in &morceaux[..morceaux.len() - 1] {
        frontiere += m.len() / 2;
        let de = frontiere.saturating_sub(FENETRE_JOINTURE);
        let a = (frontiere + FENETRE_JOINTURE)
            .min(recolle.len() / 2)
            .min(reference.len() / 2 - origine);
        for n in de..a {
            for c in 0..2 {
                let e = (recolle[2 * n + c] - reference[2 * (origine + n) + c]).abs();
                erreur = erreur.max(e);
            }
        }
    }
    Mesure {
        ecarts_de_longueur,
        decalages,
        erreur_jointure: erreur,
    }
}

/// Ce qu'on attend d'un cas.
#[derive(Clone, Copy, Debug)]
enum Attendu {
    SansBlanc,
    /// Échoue aujourd'hui ; la PR nommée le corrige.
    EchecConnu(&'static str),
}

fn juger(cas: &str, m: &Mesure, seuil: f64, attendu: Attendu) {
    eprintln!("BANC-8968 | {cas} | {m}");
    match attendu {
        Attendu::SansBlanc => assert!(
            m.sans_blanc(seuil),
            "BANC-8968 {cas} : la jointure n'est pas exacte (seuil {seuil}) : {m}"
        ),
        Attendu::EchecConnu(pr) => assert!(
            !m.sans_blanc(seuil),
            "BANC-8968 {cas} : marqué attendu-en-échec ({pr}) mais la jointure est \
             maintenant exacte : retirer le marqueur. {m}"
        ),
    }
}

// ---------------------------------------------------------------------------
// Deux fichiers coupés dans le même signal
// ---------------------------------------------------------------------------

/// 3 s et 123 trames, coupé à une trame quelconque (ni multiple de 1 024, ni
/// de 588, ni de 960).
const TRAMES: usize = 132_423;
const COUPE: usize = 61_111;
const TRAMES_OPUS: usize = 144_123;
const COUPE_OPUS: usize = 66_667;

struct Paire {
    _bac: crate::test_scratch::ScratchDir,
    chemins: Vec<PathBuf>,
    reference: Vec<f64>,
    attendus: Vec<(usize, usize)>,
    cadence: u32,
}

fn paire(format: &str) -> Paire {
    let bac = crate::test_scratch::scratch_dir(&format!("banc-jointure-8968-{format}"));
    let (cadence, trames, coupe) = if format == "opus" {
        (CADENCE_OPUS, TRAMES_OPUS, COUPE_OPUS)
    } else {
        (CADENCE, TRAMES, COUPE)
    };
    let signal = reference(cadence, trames);
    let (a, b) = signal.split_at(coupe * 2);
    let ext = match format {
        "m4a-itunsmpb" | "m4a-elst" => "m4a",
        autre => autre,
    };
    let chemins: Vec<PathBuf> = ["a", "b"]
        .iter()
        .map(|n| bac.join(format!("{n}.{ext}")))
        .collect();
    for (moitie, chemin) in [a, b].iter().zip(&chemins) {
        match format {
            "wav" => ecrire_wav(chemin, cadence, &en_i16(moitie)),
            "flac" => ecrire_flac(chemin, cadence, &en_i16(moitie)),
            "m4a-itunsmpb" => ecrire_m4a(chemin, cadence, moitie, GaplessMp4::ItunSmpb),
            "m4a-elst" => ecrire_m4a(chemin, cadence, moitie, GaplessMp4::Elst),
            "opus" => ecrire_opus(chemin, &en_i16(moitie)),
            autre => panic!("format {autre}"),
        }
    }
    let reference = if matches!(format, "wav" | "flac") {
        reference_16(cadence, trames)
    } else {
        signal
    };
    Paire {
        _bac: bac,
        chemins,
        reference,
        attendus: vec![(0, coupe), (coupe, trames - coupe)],
        cadence,
    }
}

async fn paire_piste_a_piste(format: &str, seuil: f64, attendu: Attendu) {
    let p = paire(format);
    let mut morceaux = Vec::new();
    for c in &p.chemins {
        morceaux.push(par_la_session(c, p.cadence, 0.0, None).await);
    }
    let m = mesurer(&p.reference, &morceaux, &p.attendus);
    juger(
        &format!("{format} × piste à piste (session)"),
        &m,
        seuil,
        attendu,
    );
}

fn paire_decodage_complet(format: &str, seuil: f64, attendu: Attendu) {
    let p = paire(format);
    let morceaux: Vec<Vec<f64>> = p
        .chemins
        .iter()
        .map(|c| par_le_decodage_complet(c, 0.0, 0.0))
        .collect();
    let m = mesurer(&p.reference, &morceaux, &p.attendus);
    juger(&format!("{format} × décodage complet"), &m, seuil, attendu);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn banc_8968_wav_piste_a_piste() {
    paire_piste_a_piste("wav", 0.0, Attendu::SansBlanc).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn banc_8968_flac_piste_a_piste() {
    paire_piste_a_piste("flac", 0.0, Attendu::SansBlanc).await;
}

#[test]
fn banc_8968_flac_decodage_complet() {
    paire_decodage_complet("flac", 0.0, Attendu::SansBlanc);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn banc_8968_aac_itunsmpb_piste_a_piste() {
    paire_piste_a_piste("m4a-itunsmpb", SEUIL_AVEC_PERTE, Attendu::SansBlanc).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn banc_8968_aac_elst_piste_a_piste() {
    paire_piste_a_piste("m4a-elst", SEUIL_AVEC_PERTE, Attendu::SansBlanc).await;
}

#[test]
fn banc_8968_aac_itunsmpb_decodage_complet() {
    paire_decodage_complet("m4a-itunsmpb", SEUIL_AVEC_PERTE, Attendu::SansBlanc);
}

#[test]
fn banc_8968_aac_elst_decodage_complet() {
    paire_decodage_complet("m4a-elst", SEUIL_AVEC_PERTE, Attendu::SansBlanc);
}

/// Opus : contrairement à ce que supposait l'étude de #8968, le décodeur de
/// Tune (`decode_opus_to_pcm`, libopus) n'applique NI le pre-skip de
/// `OpusHead` (312 trames d'amorce rendues) NI la granule de la dernière page
/// (le remplissage du dernier paquet de 960 rendu). Mesuré par ce banc ; le
/// correctif n'est dans aucune des PR 1 et 2 de #8968.
const OPUS_PRE_SKIP: &str =
    "Opus : pre-skip et granule de fin non appliqués (hors PR 1 et 2 de #8968)";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn banc_8968_opus_piste_a_piste() {
    paire_piste_a_piste("opus", SEUIL_AVEC_PERTE, Attendu::EchecConnu(OPUS_PRE_SKIP)).await;
}

#[test]
fn banc_8968_opus_decodage_complet() {
    paire_decodage_complet("opus", SEUIL_AVEC_PERTE, Attendu::EchecConnu(OPUS_PRE_SKIP));
}

#[test]
fn banc_8968_wav_flux_continu() {
    let p = paire("wav");
    let m = mesurer(&p.reference, &par_le_flux_continu(&p.chemins), &p.attendus);
    juger("wav × flux continu #8082", &m, 0.0, Attendu::SansBlanc);
}

#[test]
fn banc_8968_flac_flux_continu() {
    let p = paire("flac");
    let m = mesurer(&p.reference, &par_le_flux_continu(&p.chemins), &p.attendus);
    juger("flac × flux continu #8082", &m, 0.0, Attendu::SansBlanc);
}

/// Étalonnage du banc lui-même : sans aucune métadonnée, le M4A rendu tel
/// quel commence 1 024 trames en retard. Si ce n'était plus le cas (autre
/// version de `rusty_aac`), [`AMORCE_AAC_DU_BANC`] mentirait dans les
/// métadonnées écrites, et les cas AAC ne mesureraient plus rien.
#[test]
fn l_encodeur_aac_du_banc_retarde_de_1024_trames() {
    let bac = crate::test_scratch::scratch_dir("banc-jointure-8968-etalon-aac");
    let signal = reference(CADENCE, TRAMES);
    let chemin = bac.join("etalon.m4a");
    // Lu par symphonia seul, qui n'applique ni `iTunSMPB` ni `elst` : le
    // flux brut, amorce comprise.
    ecrire_m4a(&chemin, CADENCE, &signal, GaplessMp4::ItunSmpb);
    let brut = decoder_par_symphonia_seul(&chemin);
    let d = decalage(&signal, &brut, 0);
    assert_eq!(
        d,
        i64::from(AMORCE_AAC_DU_BANC),
        "délai de l'encodeur AAC du banc : {d} trames"
    );
}

/// Symphonia seul, sans rien de Tune : ce que le décodeur rend du conteneur.
fn decoder_par_symphonia_seul(chemin: &Path) -> Vec<f64> {
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    let f = std::fs::File::open(chemin).unwrap();
    let mss = MediaSourceStream::new(Box::new(f), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .unwrap();
    let piste = format.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = &piste.codec_params else {
        panic!("pas de paramètres audio");
    };
    let mut dec = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .unwrap();
    let mut v: Vec<f32> = Vec::new();
    let mut tampon: Vec<f32> = Vec::new();
    while let Ok(Some(p)) = format.next_packet() {
        let d = dec.decode(&p).unwrap();
        tampon.clear();
        d.copy_to_vec_interleaved::<f32>(&mut tampon);
        v.extend_from_slice(&tampon);
    }
    v.into_iter().map(f64::from).collect()
}

// ---------------------------------------------------------------------------
// Longueur décodée des fixtures du dépôt (fichiers d'outils tiers)
// ---------------------------------------------------------------------------

fn fixture(nom: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(nom)
}

async fn longueur_decodee(nom: &str, attendues: u64, attendu: Attendu) {
    let chemin = fixture(nom);
    let session = (par_la_session(&chemin, CADENCE, 0.0, None).await.len() / 2) as i64;
    let complet = (par_le_decodage_complet(&chemin, 0.0, 0.0).len() / 2) as i64;
    let m = Mesure {
        ecarts_de_longueur: vec![session - attendues as i64, complet - attendues as i64],
        decalages: vec![0, 0],
        erreur_jointure: 0.0,
    };
    juger(
        &format!("{nom} × longueur (session, décodage complet)"),
        &m,
        0.0,
        attendu,
    );
}

/// `test.mp3` (ffmpeg, en-tête LAME : délai 576, remplissage 1 404, 40
/// trames MPEG) porte exactement 44 100 trames utiles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn banc_8968_mp3_lame_longueur() {
    longueur_decodee("test.mp3", 44_100, Attendu::SansBlanc).await;
}

/// `test.m4a` (ffmpeg) : `elst` à 1 024 trames d'amorce, 1 s utile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn banc_8968_m4a_elst_ffmpeg_longueur() {
    longueur_decodee("test.m4a", 44_100, Attendu::SansBlanc).await;
}

/// `aac320.m4a` (encodeur Apple) : `iTunSMPB` = amorce 0x840 (2 112),
/// remplissage 0x228 (552), 0x40998 (264 600) trames utiles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn banc_8968_m4a_itunsmpb_apple_longueur() {
    longueur_decodee("aac320.m4a", 264_600, Attendu::SansBlanc).await;
}

// ---------------------------------------------------------------------------
// Une image et sa feuille CUE
// ---------------------------------------------------------------------------

/// Trois pistes dans une image de 4 s et 123 trames. Les frontières tombent
/// sur des frames CD qui ne valent pas un nombre entier de millisecondes :
/// 00:01:37 = 112 frames = 1 493,33 ms = 65 856 trames, 00:02:71 = 221 frames
/// = 2 946,67 ms = 129 948 trames.
const TRAMES_IMAGE: usize = 4 * 44_100 + 123;
const FEUILLE: &str = "FILE \"image.EXT\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n  \
                       TRACK 02 AUDIO\n    INDEX 01 00:01:37\n  TRACK 03 AUDIO\n    \
                       INDEX 01 00:02:71\n";
const FRONTIERES_VRAIES: [usize; 2] = [112 * 588, 221 * 588];

struct Image {
    _bac: crate::test_scratch::ScratchDir,
    chemin: PathBuf,
    reference: Vec<f64>,
    /// `(début_ms, fin_ms)` de chaque piste, tels que la base les range
    /// (`scanner::cue`, puis `cue_bibliotheque`).
    bornes_ms: Vec<(u64, Option<u64>)>,
    attendus: Vec<(usize, usize)>,
}

fn image(ext: &str) -> Image {
    let bac = crate::test_scratch::scratch_dir(&format!("banc-jointure-8968-cue-{ext}"));
    let chemin = bac.join(format!("image.{ext}"));
    let pcm = en_i16(&reference(CADENCE, TRAMES_IMAGE));
    match ext {
        "wav" => ecrire_wav(&chemin, CADENCE, &pcm),
        "flac" => ecrire_flac(&chemin, CADENCE, &pcm),
        autre => panic!("image {autre}"),
    }
    let feuille = crate::scanner::cue::parse_cue_sheet(&FEUILLE.replace("EXT", ext));
    let bornes_ms: Vec<(u64, Option<u64>)> = feuille
        .tracks
        .iter()
        .map(|t| (t.start_ms, t.end_ms))
        .collect();
    assert_eq!(
        bornes_ms,
        vec![(0, Some(1_493)), (1_493, Some(2_946)), (2_946, None)],
        "la feuille range des millisecondes tronquées"
    );
    let [f1, f2] = FRONTIERES_VRAIES;
    Image {
        _bac: bac,
        chemin,
        reference: reference_16(CADENCE, TRAMES_IMAGE),
        bornes_ms,
        attendus: vec![(0, f1), (f1, f2 - f1), (f2, TRAMES_IMAGE - f2)],
    }
}

/// Début et durée de la tranche, en secondes, comme `resolve_local` les
/// tire de `TrancheCue` (#8968 : à l'échantillon près, `TrancheCue::en_secondes`).
fn tranche_en_secondes(debut_ms: u64, fin_ms: Option<u64>) -> (f64, Option<f64>) {
    super::resolve_local::TrancheCue {
        debut_ms,
        duree_ms: fin_ms.map(|f| f.saturating_sub(debut_ms)),
    }
    .en_secondes(0, CADENCE)
}

async fn cue_par_la_session(ext: &str, attendu: Attendu) {
    let im = image(ext);
    let mut morceaux = Vec::new();
    for (debut, fin) in &im.bornes_ms {
        let (s, d) = tranche_en_secondes(*debut, *fin);
        morceaux.push(par_la_session(&im.chemin, CADENCE, s, d).await);
    }
    let m = mesurer(&im.reference, &morceaux, &im.attendus);
    juger(&format!("CUE {ext} × tranches (session)"), &m, 0.0, attendu);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn banc_8968_cue_flac_tranches_session() {
    cue_par_la_session("flac", Attendu::SansBlanc).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn banc_8968_cue_wav_tranches_session() {
    cue_par_la_session("wav", Attendu::SansBlanc).await;
}

#[test]
fn banc_8968_cue_flac_tranches_decodage_complet() {
    let im = image("flac");
    let morceaux: Vec<Vec<f64>> = im
        .bornes_ms
        .iter()
        .map(|(debut, fin)| {
            let (s, d) = tranche_en_secondes(*debut, *fin);
            par_le_decodage_complet(&im.chemin, s, d.unwrap_or(0.0))
        })
        .collect();
    let m = mesurer(&im.reference, &morceaux, &im.attendus);
    juger(
        "CUE flac × tranches (décodage complet)",
        &m,
        0.0,
        Attendu::SansBlanc,
    );
}

/// Flux continu #8082 et tranches CUE (#8968, PR 2) : les trois pistes de la
/// feuille sont admises, assemblées comme en production
/// (`chemin_admissible` puis `assembler_avec_tranches`), et le WAV virtuel lu
/// d'un bout à l'autre doit rendre l'image à l'échantillon près, frontières
/// comprises.
#[test]
fn banc_8968_cue_flux_continu() {
    use crate::db::backend::{DbBackend, ToSqlValue};
    use crate::http::streamer::SourceVirtuelle;
    let im = image("flac");
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: std::sync::Arc<dyn DbBackend> = std::sync::Arc::new(db);
    let media = im.chemin.to_string_lossy().into_owned();
    for (i, (debut, fin)) in im.bornes_ms.iter().enumerate() {
        let id = i as i64 + 1;
        let debut = *debut as i64;
        let fin = fin.map(|f| f as i64);
        db.execute(
            "INSERT INTO tracks (id, title, format, duration_ms, sample_rate, bit_depth, \
             channels, cue_media_path, cue_start_ms, cue_end_ms) \
             VALUES (?, 'Piste', 'flac', 1000, 44100, 16, 2, ?, ?, ?)",
            &[
                &id as &dyn ToSqlValue,
                &media as &dyn ToSqlValue,
                &debut as &dyn ToSqlValue,
                &fin as &dyn ToSqlValue,
            ],
        )
        .unwrap();
    }
    let mut candidates: Vec<_> = (1..=3)
        .map(|id| {
            let a = flux_continu_8082::chemin_admissible(&db, id).expect("tranche CUE admise");
            (id - 1, id, PathBuf::from(a.chemin), a.tranche)
        })
        .collect();
    let premiere = candidates.remove(0);
    let f = flux_continu_8082::assembler_avec_tranches(premiere, 0, candidates)
        .expect("flux continu assemblé");
    assert_eq!(f.lignes.len(), 3, "les trois tranches dans le flux");
    let mut octets = Vec::new();
    f.wav
        .lire(0, f.wav.total(), &mut |m| {
            octets.extend_from_slice(&m);
            Ok(())
        })
        .unwrap();
    let tout: Vec<f64> = octets[44..]
        .chunks_exact(2)
        .map(|c| f64::from(i16::from_le_bytes([c[0], c[1]])) / 32_768.0)
        .collect();
    let mut morceaux = Vec::new();
    let mut pos = 0usize;
    for s in f.wav.segments() {
        let n = s.trames as usize * 2;
        morceaux.push(tout[pos..pos + n].to_vec());
        pos += n;
    }
    let m = mesurer(&im.reference, &morceaux, &im.attendus);
    juger("CUE flac × flux continu #8082", &m, 0.0, Attendu::SansBlanc);
}
