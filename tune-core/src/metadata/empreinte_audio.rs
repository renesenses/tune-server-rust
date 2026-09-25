//! L'empreinte de la PARTIE AUDIO d'un fichier — tout ce qui n'est pas une
//! balise.
//!
//! Sert de garde à « Écrire dans les fichiers » (tranche 4 du chantier
//! « édition des albums, compilations et coffrets », GO de Bertrand du
//! 25/09/2026) : l'écriture se fait sur une COPIE, et la copie ne remplace
//! l'original que si l'empreinte de son audio est identique, octet pour
//! octet, à celle de l'original. Une écriture de balises qui toucherait au
//! son — un bogue de lofty, un conteneur mal compris — est ainsi refusée
//! avant d'avoir abîmé quoi que ce soit.
//!
//! Ce qui est « l'audio », conteneur par conteneur :
//!
//! | conteneur              | partie hachée                                        |
//! |------------------------|------------------------------------------------------|
//! | FLAC                   | les trames, après le dernier bloc de métadonnées     |
//! | MP3, AAC (ADTS), APE, WavPack | le fichier moins ID3v2 en tête, ID3v1 et APEv2 en queue |
//! | MP4 / M4A (AAC, ALAC)  | le contenu des atomes `mdat`                         |
//! | WAV (RIFF)             | le contenu du bloc `data`                            |
//! | AIFF / AIFC            | le contenu du bloc `SSND`                            |
//! | Ogg Vorbis, Opus       | les paquets du flux APRÈS ses en-têtes (3 / 2)      |
//!
//! Pour l'Ogg, les PAGES changent forcément quand le commentaire grandit
//! (numéros de séquence, sommes de contrôle) : ce sont les PAQUETS audio qui
//! doivent rester identiques, et ce sont eux qu'on hache.
//!
//! `Ok(None)` : conteneur que ce module ne sait pas découper (DSF, DFF, WMA…).
//! L'appelant doit alors REFUSER d'écrire, jamais écrire sans garde.
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use sha2::{Digest, Sha256};

/// Une empreinte SHA-256 de la partie audio.
pub type Empreinte = [u8; 32];

/// Les extensions dont l'audio sait être isolé des balises.
pub fn format_empreinte_gere(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "flac"
            | "mp3"
            | "aac"
            | "ape"
            | "wv"
            | "m4a"
            | "mp4"
            | "m4b"
            | "alac"
            | "wav"
            | "aif"
            | "aiff"
            | "aifc"
            | "ogg"
            | "oga"
            | "opus"
    )
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// L'empreinte de la partie audio de `path`, `Ok(None)` si le conteneur n'est
/// pas découpable ici, `Err` si le fichier est illisible ou mal formé.
pub fn empreinte_audio(path: &Path) -> Result<Option<Empreinte>, String> {
    let ext = extension(path);
    if !format_empreinte_gere(path) {
        return Ok(None);
    }
    let mut f = File::open(path).map_err(|e| format!("empreinte : ouverture : {e}"))?;
    let taille = f
        .metadata()
        .map_err(|e| format!("empreinte : taille : {e}"))?
        .len();
    let mut h = Sha256::new();
    match ext.as_str() {
        "flac" => flac(&mut f, taille, &mut h)?,
        "mp3" | "aac" | "ape" | "wv" => nu(&mut f, taille, &mut h)?,
        "m4a" | "mp4" | "m4b" | "alac" => mp4(&mut f, taille, &mut h)?,
        "wav" => riff(&mut f, taille, &mut h)?,
        "aif" | "aiff" | "aifc" => aiff(&mut f, taille, &mut h)?,
        "ogg" | "oga" | "opus" => {
            let mut lecteur = BufReader::new(f);
            ogg(&mut lecteur, &mut h)?
        }
        _ => return Ok(None),
    }
    Ok(Some(h.finalize().into()))
}

fn lire_exact(f: &mut File, pos: u64, buf: &mut [u8]) -> Result<(), String> {
    f.seek(SeekFrom::Start(pos))
        .and_then(|_| f.read_exact(buf))
        .map_err(|e| format!("empreinte : lecture à {pos} : {e}"))
}

/// Hache `longueur` octets à partir de `debut`.
fn hacher(f: &mut File, debut: u64, longueur: u64, h: &mut Sha256) -> Result<(), String> {
    f.seek(SeekFrom::Start(debut))
        .map_err(|e| format!("empreinte : positionnement : {e}"))?;
    let mut reste = longueur;
    let mut buf = vec![0u8; 64 * 1024];
    while reste > 0 {
        let n = (buf.len() as u64).min(reste) as usize;
        f.read_exact(&mut buf[..n])
            .map_err(|e| format!("empreinte : lecture : {e}"))?;
        h.update(&buf[..n]);
        reste -= n as u64;
    }
    Ok(())
}

/// La taille totale d'une balise ID3v2 commençant à `pos` (en-tête, corps,
/// pied éventuel), ou `None` s'il n'y en a pas.
fn taille_id3v2(f: &mut File, pos: u64, taille: u64) -> Result<Option<u64>, String> {
    if pos + 10 > taille {
        return Ok(None);
    }
    let mut t = [0u8; 10];
    lire_exact(f, pos, &mut t)?;
    if &t[..3] != b"ID3" {
        return Ok(None);
    }
    let corps = t[6..10]
        .iter()
        .fold(0u64, |acc, b| (acc << 7) | u64::from(b & 0x7f));
    let pied = if t[5] & 0x10 != 0 { 10 } else { 0 };
    Ok(Some(10 + corps + pied))
}

/// MP3, AAC ADTS, APE, WavPack : le fichier, moins les balises en tête et en
/// queue.
fn nu(f: &mut File, taille: u64, h: &mut Sha256) -> Result<(), String> {
    let mut debut = 0u64;
    // Plusieurs ID3v2 à la suite existent (fichiers retagués deux fois).
    while let Some(n) = taille_id3v2(f, debut, taille)? {
        debut += n;
    }
    let mut fin = taille;
    if fin >= debut + 128 {
        let mut t = [0u8; 3];
        lire_exact(f, fin - 128, &mut t)?;
        if &t == b"TAG" {
            fin -= 128;
        }
    }
    if fin >= debut + 32 {
        let mut pied = [0u8; 32];
        lire_exact(f, fin - 32, &mut pied)?;
        if &pied[..8] == b"APETAGEX" {
            let corps = u64::from(u32::from_le_bytes(pied[12..16].try_into().unwrap()));
            let drapeaux = u32::from_le_bytes(pied[20..24].try_into().unwrap());
            let entete = if drapeaux & 0x8000_0000 != 0 { 32 } else { 0 };
            let total = corps + entete;
            if total > fin - debut {
                return Err("empreinte : balise APE plus grande que le fichier".into());
            }
            fin -= total;
        }
    }
    if fin < debut {
        return Err("empreinte : balises plus grandes que le fichier".into());
    }
    hacher(f, debut, fin - debut, h)
}

fn flac(f: &mut File, taille: u64, h: &mut Sha256) -> Result<(), String> {
    let mut pos = 0u64;
    while let Some(n) = taille_id3v2(f, pos, taille)? {
        pos += n;
    }
    let mut marque = [0u8; 4];
    lire_exact(f, pos, &mut marque)?;
    if &marque != b"fLaC" {
        return Err("empreinte : pas de signature fLaC".into());
    }
    pos += 4;
    loop {
        let mut t = [0u8; 4];
        lire_exact(f, pos, &mut t)?;
        let dernier = t[0] & 0x80 != 0;
        let longueur = (u64::from(t[1]) << 16) | (u64::from(t[2]) << 8) | u64::from(t[3]);
        pos += 4 + longueur;
        if pos > taille {
            return Err("empreinte : bloc FLAC au-delà de la fin".into());
        }
        if dernier {
            break;
        }
    }
    hacher(f, pos, taille - pos, h)
}

fn mp4(f: &mut File, taille: u64, h: &mut Sha256) -> Result<(), String> {
    let mut pos = 0u64;
    let mut vus = 0usize;
    while pos + 8 <= taille {
        let mut t = [0u8; 8];
        lire_exact(f, pos, &mut t)?;
        let mut longueur = u64::from(u32::from_be_bytes(t[..4].try_into().unwrap()));
        let mut entete = 8u64;
        if longueur == 1 {
            let mut e = [0u8; 8];
            lire_exact(f, pos + 8, &mut e)?;
            longueur = u64::from_be_bytes(e);
            entete = 16;
        } else if longueur == 0 {
            longueur = taille - pos;
        }
        if longueur < entete || pos + longueur > taille {
            return Err(format!("empreinte : atome MP4 mal formé à {pos}"));
        }
        if &t[4..8] == b"mdat" {
            hacher(f, pos + entete, longueur - entete, h)?;
            vus += 1;
        }
        pos += longueur;
    }
    if vus == 0 {
        return Err("empreinte : aucun atome mdat".into());
    }
    Ok(())
}

/// Parcourt les blocs d'un RIFF (petit-boutiste) ou d'un FORM (gros-boutiste)
/// et hache le contenu du bloc `cible`.
fn blocs(
    f: &mut File,
    taille: u64,
    h: &mut Sha256,
    gros_boutiste: bool,
    cible: &[u8; 4],
) -> Result<(), String> {
    let mut pos = 12u64;
    while pos + 8 <= taille {
        let mut t = [0u8; 8];
        lire_exact(f, pos, &mut t)?;
        let octets: [u8; 4] = t[4..8].try_into().unwrap();
        let longueur = u64::from(if gros_boutiste {
            u32::from_be_bytes(octets)
        } else {
            u32::from_le_bytes(octets)
        });
        if pos + 8 + longueur > taille {
            return Err(format!("empreinte : bloc au-delà de la fin à {pos}"));
        }
        if &t[..4] == cible {
            return hacher(f, pos + 8, longueur, h);
        }
        pos += 8 + longueur + (longueur & 1);
    }
    Err(format!(
        "empreinte : bloc {} introuvable",
        String::from_utf8_lossy(cible)
    ))
}

fn riff(f: &mut File, taille: u64, h: &mut Sha256) -> Result<(), String> {
    let mut t = [0u8; 12];
    lire_exact(f, 0, &mut t)?;
    if &t[..4] != b"RIFF" || &t[8..12] != b"WAVE" {
        return Err("empreinte : pas un RIFF/WAVE".into());
    }
    blocs(f, taille, h, false, b"data")
}

fn aiff(f: &mut File, taille: u64, h: &mut Sha256) -> Result<(), String> {
    let mut t = [0u8; 12];
    lire_exact(f, 0, &mut t)?;
    if &t[..4] != b"FORM" || !(&t[8..12] == b"AIFF" || &t[8..12] == b"AIFC") {
        return Err("empreinte : pas un FORM/AIFF".into());
    }
    blocs(f, taille, h, true, b"SSND")
}

/// Ogg : les paquets du premier flux logique, à partir du premier paquet
/// AUDIO (3 en-têtes pour Vorbis, 2 pour Opus).
fn ogg<R: Read>(r: &mut R, h: &mut Sha256) -> Result<(), String> {
    let mut serie: Option<u32> = None;
    let mut paquet = 0usize;
    let mut entetes: Option<usize> = None;
    let mut premier_paquet: Vec<u8> = Vec::new();
    loop {
        let mut t = [0u8; 27];
        match r.read_exact(&mut t) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("empreinte : page Ogg : {e}")),
        }
        if &t[..4] != b"OggS" {
            return Err("empreinte : page Ogg sans capture OggS".into());
        }
        let numero_serie = u32::from_le_bytes(t[14..18].try_into().unwrap());
        let mut table = vec![0u8; usize::from(t[26])];
        r.read_exact(&mut table)
            .map_err(|e| format!("empreinte : table de segments : {e}"))?;
        let charge: usize = table.iter().map(|&l| usize::from(l)).sum();
        let mut corps = vec![0u8; charge];
        r.read_exact(&mut corps)
            .map_err(|e| format!("empreinte : corps de page : {e}"))?;
        let serie_du_flux = *serie.get_or_insert(numero_serie);
        if numero_serie != serie_du_flux {
            continue;
        }
        let mut pos = 0usize;
        for &l in &table {
            let seg = &corps[pos..pos + usize::from(l)];
            pos += usize::from(l);
            if paquet == 0 {
                premier_paquet.extend_from_slice(seg);
            }
            if let Some(n) = entetes
                && paquet >= n
            {
                h.update(seg);
            }
            if l < 255 {
                if paquet == 0 {
                    entetes = Some(if premier_paquet.starts_with(b"\x01vorbis") {
                        3
                    } else if premier_paquet.starts_with(b"OpusHead") {
                        2
                    } else {
                        return Err("empreinte : flux Ogg ni Vorbis ni Opus".into());
                    });
                }
                paquet += 1;
            }
        }
    }
    match entetes {
        Some(n) if paquet > n => Ok(()),
        _ => Err("empreinte : aucun paquet audio Ogg".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(nom: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(nom)
    }

    #[test]
    fn chaque_fixture_courante_a_une_empreinte() {
        for nom in [
            "test.flac",
            "test.mp3",
            "test.m4a",
            "test.opus",
            "test_vorbis.ogg",
            "test.wav",
            "test.aiff",
            "alac/ref_16_44100_stereo.m4a",
            "ape/sine_16s_c3000.ape",
            "wavpack/rip_16_44100_stereo.wv",
        ] {
            let e = empreinte_audio(&fixture(nom));
            assert!(matches!(e, Ok(Some(_))), "{nom} : {e:?}");
        }
    }

    #[test]
    fn dsf_et_dff_ne_sont_pas_decoupes() {
        assert_eq!(
            empreinte_audio(&fixture("dsd/ref_dsd64_stereo.dsf")),
            Ok(None)
        );
        assert_eq!(
            empreinte_audio(&fixture("dsd/ref_dsd64_stereo.dff")),
            Ok(None)
        );
        // `test.ogg` est un FLAC en Ogg : ni Vorbis ni Opus, son audio n'est
        // pas découpé ici — refus, donc aucune écriture sans garde.
        assert!(empreinte_audio(&fixture("test.ogg")).is_err());
    }

    /// Contre-épreuve : un seul octet d'audio changé change l'empreinte — et
    /// un octet changé DANS une balise ne la change pas.
    #[test]
    fn un_octet_d_audio_change_l_empreinte_un_octet_de_balise_non() {
        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("t.mp3");
        let mut octets = std::fs::read(fixture("test.mp3")).unwrap();
        std::fs::write(&cible, &octets).unwrap();
        let avant = empreinte_audio(&cible).unwrap().unwrap();

        // Un octet dans l'ID3v2 de tête (après ses 10 octets d'en-tête), s'il
        // y en a un ; sinon on en fabrique un.
        let mut balise = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
        balise.extend_from_slice(&[0u8; 10]);
        let mut avec_balise = balise.clone();
        avec_balise.extend_from_slice(&octets);
        std::fs::write(&cible, &avec_balise).unwrap();
        assert_eq!(empreinte_audio(&cible).unwrap().unwrap(), avant);
        avec_balise[15] = 0x55; // dans le rembourrage de la balise
        std::fs::write(&cible, &avec_balise).unwrap();
        assert_eq!(empreinte_audio(&cible).unwrap().unwrap(), avant);

        let milieu = octets.len() / 2;
        octets[milieu] ^= 0x01;
        std::fs::write(&cible, &octets).unwrap();
        assert_ne!(empreinte_audio(&cible).unwrap().unwrap(), avant);
    }

    #[test]
    fn un_octet_d_audio_change_l_empreinte_sur_chaque_conteneur() {
        for nom in [
            "test.flac",
            "test.m4a",
            "test_vorbis.ogg",
            "test.opus",
            "test.wav",
            "test.aiff",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let cible = dir.path().join(nom);
            let mut octets = std::fs::read(fixture(nom)).unwrap();
            std::fs::write(&cible, &octets).unwrap();
            let avant = empreinte_audio(&cible).unwrap().unwrap();
            // Le milieu du fichier est de l'audio dans chacune de ces
            // fixtures (l'audio y pèse l'essentiel du fichier).
            let milieu = octets.len() / 2;
            octets[milieu] ^= 0x01;
            std::fs::write(&cible, &octets).unwrap();
            assert_ne!(
                empreinte_audio(&cible).ok().flatten(),
                Some(avant),
                "{nom} : un octet d'audio changé n'a pas changé l'empreinte"
            );
        }
    }
}
