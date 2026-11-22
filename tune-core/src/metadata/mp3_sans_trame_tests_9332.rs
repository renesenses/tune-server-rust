//! #9332 — un `.mp3` sans aucune trame audio : une balise ID3v2 (pochette de
//! 139 Ko comprise), puis rien. lofty le refuse (« File contains an invalid
//! frame »), et le repli par le nom de fichier l'indexait quand même : la
//! bibliothèque montrait une piste « lisible » qui ne joue rien.
//!
//! Même règle que l'Ogg sans audio (#4412) : écarté, et nommé dans le
//! rapport de scan. Le fichier est FABRIQUÉ ici octet par octet (ID3v2.4,
//! TIT2 + APIC) ; aucun fichier de testeur.

use super::*;
use crate::scanner::walker::{EcrituresDuLot, scan_files_batched, scan_files_parallel};

fn scratch() -> crate::test_scratch::ScratchDir {
    crate::test_scratch::scratch_dir_in(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("target"),
        "mp3-sans-trame-9332",
    )
}

fn syncsafe(n: usize) -> [u8; 4] {
    [
        ((n >> 21) & 0x7f) as u8,
        ((n >> 14) & 0x7f) as u8,
        ((n >> 7) & 0x7f) as u8,
        (n & 0x7f) as u8,
    ]
}

fn trame_id3(id: &[u8; 4], donnees: &[u8]) -> Vec<u8> {
    let mut f = id.to_vec();
    f.extend_from_slice(&syncsafe(donnees.len()));
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(donnees);
    f
}

/// Une balise ID3v2.4 seule : un titre et une pochette de `pochette` octets.
fn id3_sans_audio(pochette: usize) -> Vec<u8> {
    let mut trames = trame_id3(b"TIT2", b"\x03Titre 9332");
    let mut apic = b"\x03image/jpeg\x00\x03\x00".to_vec();
    apic.extend(std::iter::repeat_n(0xAAu8, pochette));
    trames.extend(trame_id3(b"APIC", &apic));
    let mut octets = b"ID3\x04\x00\x00".to_vec();
    octets.extend_from_slice(&syncsafe(trames.len()));
    octets.extend(trames);
    octets
}

fn poser(racine: &Path, nom: &str, octets: &[u8]) -> std::path::PathBuf {
    let chemin = racine.join(nom);
    std::fs::write(&chemin, octets).unwrap();
    chemin
}

#[test]
fn un_mp3_sans_aucune_trame_audio_n_est_pas_une_piste_9332() {
    let dir = scratch();
    let vide = poser(&dir, "01 - Sans audio.mp3", &id3_sans_audio(139_000));
    assert!(
        std::fs::metadata(&vide).unwrap().len() > 139_000,
        "le cas signalé : environ 139 Ko"
    );
    let erreur = try_read_metadata(&vide).expect_err(
        "#9332 : un MP3 sans aucune trame audio ne doit pas entrer par le repli du nom de fichier",
    );
    assert!(erreur.contains("MP3 sans aucune trame audio"), "{erreur}");
}

#[test]
fn le_scan_l_ecarte_et_le_nomme_et_garde_le_vrai_mp3_9332() {
    let dir = scratch();
    let vide = poser(&dir, "01 - Sans audio.mp3", &id3_sans_audio(139_000));
    let vrai = poser(
        &dir,
        "02 - Vrai.mp3",
        include_bytes!("../../tests/fixtures/test.mp3"),
    );
    let chemins = vec![vide, vrai];
    let verifier = |stats: &crate::scanner::walker::ScanStats| {
        assert_eq!(stats.total_files, 2);
        assert_eq!(stats.metadata_ok, 1, "seul le vrai MP3 est une piste");
        assert_eq!(stats.metadata_failed, 1);
        assert_eq!(stats.failed_paths.len(), 1);
        assert!(
            stats.failed_paths[0].contains("01 - Sans audio.mp3 (MP3 sans aucune trame audio"),
            "{:?}",
            stats.failed_paths
        );
    };
    let (fichiers, stats) = scan_files_parallel(&chemins, false, None);
    verifier(&stats);
    let lu = |nom: &str| {
        fichiers
            .iter()
            .find(|f| f.path.ends_with(nom))
            .unwrap()
            .metadata
            .is_some()
    };
    assert!(!lu("01 - Sans audio.mp3"));
    assert!(lu("02 - Vrai.mp3"), "contre-épreuve : un vrai MP3 reste lu");

    let mut par_lots = Vec::new();
    let stats = scan_files_batched(&chemins, false, 1, |f, _, _| {
        par_lots.extend(f);
        EcrituresDuLot::SANS_PERTE
    });
    verifier(&stats);
    assert_eq!(par_lots.iter().filter(|f| f.metadata.is_some()).count(), 1);
}

/// La sonde lit au plus `OGG_FALLBACK_PROBE_BYTES` : une balise plus grande
/// (pochette énorme) ne permet pas de conclure, et un doute ne retire jamais
/// une piste — même règle que l'Ogg au commentaire géant (#4412).
#[test]
fn une_balise_plus_grande_que_la_sonde_ne_fait_pas_conclure_9332() {
    let dir = scratch();
    let geant = poser(
        &dir,
        "03 - Pochette geante.mp3",
        &id3_sans_audio(OGG_FALLBACK_PROBE_BYTES as usize + 4096),
    );
    assert_eq!(ogg_fallback_has_audio(&geant), None);
}
