//! #4846 (Didier, fil 1904) — coffret multicanal rangé
//! `…/<Édition>/Multichannel 7.1/01 … .flac` : l'album s'appelait
//! « Multichannel 7.1 ».
//!
//! Les épreuves ouvrent un VRAI FLAC 8 canaux, fabriqué ici octet par octet
//! (aucun encodeur externe, aucun ffmpeg) : un bloc STREAMINFO annonçant
//! 8 canaux / 24 bits / 48 kHz, puis une trame valide de 4 096 échantillons
//! faite de huit sous-trames CONSTANT (silence), CRC-8 et CRC-16 compris.
//! symphonia le décode, lofty y écrit ses commentaires Vorbis, et la lecture
//! passe par la fonction de production `read_metadata`.

use super::*;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::VorbisComments;
use symphonia::core::{
    formats::{FormatOptions, probe::Hint},
    io::MediaSourceStream,
    meta::MetadataOptions,
};

const ECHANTILLONS: u64 = 4096;

fn crc8(octets: &[u8]) -> u8 {
    let mut c = 0u8;
    for &o in octets {
        c ^= o;
        for _ in 0..8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
        }
    }
    c
}

fn crc16(octets: &[u8]) -> u16 {
    let mut c = 0u16;
    for &o in octets {
        c ^= u16::from(o) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
        }
    }
    c
}

/// Un FLAC 7.1 (8 canaux, 24 bits, 48 kHz) de 4 096 échantillons de silence.
fn flac_8_canaux() -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    // STREAMINFO, dernier bloc de métadonnées, 34 octets.
    v.extend_from_slice(&[0x80, 0, 0, 34]);
    v.extend_from_slice(&4096u16.to_be_bytes()); // bloc min
    v.extend_from_slice(&4096u16.to_be_bytes()); // bloc max
    v.extend_from_slice(&[0; 6]); // tailles de trame inconnues
    let champs: u64 = (48_000u64 << 44) | (7 << 41) | (23 << 36) | ECHANTILLONS;
    v.extend_from_slice(&champs.to_be_bytes());
    v.extend_from_slice(&[0; 16]); // MD5 inconnu (permis)
    // En-tête de trame : synchro, bloc 4096 (1100) / 48 kHz (1010),
    // 8 canaux indépendants (0111) / 24 bits (110), trame n° 0.
    let mut trame = vec![0xFF, 0xF8, 0xCA, 0x7C, 0x00];
    trame.push(crc8(&trame));
    for _ in 0..8 {
        // Sous-trame CONSTANT, sans bits perdus, valeur 0 sur 24 bits.
        trame.extend_from_slice(&[0x00, 0, 0, 0]);
    }
    let c = crc16(&trame);
    trame.extend_from_slice(&c.to_be_bytes());
    v.extend(trame);
    v
}

/// `<scratch>/Pink Floyd - The Dark Side Of The Moon/Multichannel 7.1/01 - Speak To Me.flac`,
/// tagué avec les paires données.
fn coffret(
    epreuve: &str,
    tags: &[(&str, &str)],
) -> (crate::test_scratch::ScratchDir, std::path::PathBuf) {
    let racine = crate::test_scratch::scratch_dir(&format!("coffret-4846-{epreuve}"));
    let dossier = racine
        .join("Pink Floyd - The Dark Side Of The Moon")
        .join("Multichannel 7.1");
    std::fs::create_dir_all(&dossier).expect("arborescence du coffret");
    let piste = dossier.join("01 - Speak To Me.flac");
    std::fs::write(&piste, flac_8_canaux()).expect("écriture du FLAC");
    let mut fh = std::fs::File::open(&piste).expect("ouverture");
    let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).expect("lecture FLAC");
    drop(fh);
    let mut vc = VorbisComments::default();
    for (k, v) in tags {
        vc.insert(k.to_string(), v.to_string());
    }
    flac.set_vorbis_comments(vc);
    flac.save_to_path(&piste, WriteOptions::default())
        .expect("écriture des tags");
    (racine, piste)
}

/// Montage : le gabarit est un vrai FLAC 8 canaux, que symphonia décode.
#[test]
fn le_gabarit_est_un_flac_8_canaux_decodable_4846() {
    let (_racine, piste) = coffret("gabarit", &[("TITLE", "Speak To Me")]);
    let mut lecteur = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            MediaSourceStream::new(
                Box::new(std::fs::File::open(&piste).unwrap()),
                Default::default(),
            ),
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .expect("symphonia reconnaît le FLAC");
    let piste_audio = lecteur
        .default_track(symphonia::core::formats::TrackType::Audio)
        .unwrap();
    let Some(symphonia::core::codecs::CodecParameters::Audio(params)) = &piste_audio.codec_params
    else {
        panic!("paramètres audio");
    };
    let mut decodeur = symphonia::default::get_codecs()
        .make_audio_decoder(params, &Default::default())
        .unwrap();
    let mut trames = 0;
    while let Some(paquet) = lecteur.next_packet().unwrap() {
        trames += decodeur.decode(&paquet).unwrap().frames();
    }
    assert_eq!(trames as u64, ECHANTILLONS, "le FLAC fabriqué se décode");
    let meta = read_metadata(&piste).expect("lecture");
    assert_eq!(meta.channels, Some(8), "8 canaux annoncés");
    assert_eq!(meta.sample_rate, Some(48_000));
    assert_eq!(meta.bit_depth, Some(24));
}

/// TÉMOIN VERT — la lecture lofty d'un FLAC 8 canaux n'est PAS en cause :
/// quand ALBUM et ALBUMARTIST sont étiquetés, ce sont eux qui sortent, et le
/// sous-dossier `Multichannel 7.1` n'y change rien.
#[test]
fn un_flac_8_canaux_tague_garde_son_album_et_son_artiste_d_album_4846() {
    let (_racine, piste) = coffret(
        "tague",
        &[
            ("TITLE", "Speak To Me"),
            ("ARTIST", "Pink Floyd"),
            ("ALBUM", "The Dark Side Of The Moon"),
            ("ALBUMARTIST", "Pink Floyd"),
            ("DATE", "1973"),
            ("TRACKNUMBER", "1"),
        ],
    );
    let meta = read_metadata(&piste).expect("lecture");
    assert_eq!(meta.channels, Some(8));
    assert_eq!(meta.album.as_deref(), Some("The Dark Side Of The Moon"));
    assert_eq!(meta.album_artist.as_deref(), Some("Pink Floyd"));
    assert_eq!(meta.artist.as_deref(), Some("Pink Floyd"));
    assert_eq!(meta.title.as_deref(), Some("Speak To Me"));
}

/// Le cas de la capture du fil 1904 : titre, artiste et année sont lus, mais
/// l'album prenait le nom du sous-dossier de MIXAGE. Un dossier
/// `Multichannel 7.1` n'est pas un album : c'est une variante de l'édition
/// au-dessus. L'album se nomme d'après l'édition, variante en qualificatif —
/// pour que la version stéréo et la version 5.1 du même coffret ne se
/// fondent pas en un album aux numéros de piste doublés.
#[test]
fn sans_balise_album_le_sous_dossier_de_mixage_ne_nomme_plus_l_album_4846() {
    let (_racine, piste) = coffret(
        "sans-album",
        &[
            ("TITLE", "Speak To Me"),
            ("ARTIST", "Pink Floyd"),
            ("DATE", "1973"),
            ("TRACKNUMBER", "1"),
        ],
    );
    let meta = read_metadata(&piste).expect("lecture");
    assert_eq!(meta.title.as_deref(), Some("Speak To Me"));
    assert_eq!(meta.artist.as_deref(), Some("Pink Floyd"));
    assert_eq!(
        meta.album.as_deref(),
        Some("Pink Floyd - The Dark Side Of The Moon (Multichannel 7.1)"),
        "#4846 — l'album ne doit plus s'appeler « Multichannel 7.1 » : c'est \
         le nom d'une variante de mixage, l'édition est le dossier au-dessus"
    );
}

#[test]
fn les_noms_de_variante_de_mixage_4846() {
    for nom in [
        "Multichannel 7.1",
        "Multichannel 5.1",
        "Multi-Channel",
        "multicanal 5.1",
        "5.1",
        "7.1.4",
        "Surround 5.1",
        "Stereo",
        "Stéréo",
        "Dolby Atmos",
        "Atmos 7.1.4",
        "Quad",
        "5.1 Mix",
        "Stereo Mix",
        "MCH",
        "DTS 5.1",
        " [Multichannel 7.1] ",
        "(5.1 Surround)",
    ] {
        assert!(
            variante_de_mixage(nom),
            "« {nom} » est une variante de mixage"
        );
    }
    for nom in [
        "Kind of Blue",
        "Stereolab",
        "Quadrophenia",
        "Atmosphere",
        "Dolby",
        "Mix",
        "Stereo Total", // un groupe
        "The Dark Side Of The Moon",
        "CD1",
        "1973",
        "",
    ] {
        assert!(
            !variante_de_mixage(nom),
            "« {nom} » n'est PAS une variante de mixage"
        );
    }
}

#[test]
fn deux_variantes_du_meme_coffret_restent_deux_albums_4846() {
    let (a71, art71, d71) = album_artiste_du_chemin(Path::new(
        "/M/Pink Floyd/The Dark Side Of The Moon/Multichannel 7.1/01.flac",
    ));
    let (a20, art20, d20) = album_artiste_du_chemin(Path::new(
        "/M/Pink Floyd/The Dark Side Of The Moon/Stereo/01.flac",
    ));
    assert_eq!(
        a71.as_deref(),
        Some("The Dark Side Of The Moon (Multichannel 7.1)")
    );
    assert_eq!(a20.as_deref(), Some("The Dark Side Of The Moon (Stereo)"));
    // L'artiste déduit du chemin (fichiers sans balise) est le dossier au-dessus
    // de l'ÉDITION, plus le titre de l'édition.
    assert_eq!(art71.as_deref(), Some("Pink Floyd"));
    assert_eq!(art20.as_deref(), Some("Pink Floyd"));
    assert_eq!((d71, d20), (None, None));

    // Une variante à la racine : pas d'édition au-dessus, on garde la variante.
    let (a, _, _) = album_artiste_du_chemin(Path::new("/Multichannel 7.1/01.flac"));
    assert_eq!(a.as_deref(), Some("Multichannel 7.1"));
}
