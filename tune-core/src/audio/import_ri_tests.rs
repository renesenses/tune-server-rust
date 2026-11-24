//! TESS T09a — preuves de l'import : corpus aux formes des exports réels
//! (REW : deux mono L/R par cadence ; Acourate : un stéréo par cadence ;
//! Convolver : `.cfg` + RI), et archives piégées refusées (contre-épreuves).

use super::*;
use std::io::Write;
use zip::write::SimpleFileOptions;

/// Un WAV en mémoire : flottant 32 bits (`bits = 32`) ou PCM 24 bits.
fn wav(canaux: &[Vec<f32>], sr: u32, bits: u16) -> Vec<u8> {
    let n = canaux.iter().map(Vec::len).max().unwrap_or(0);
    let ch = canaux.len() as u16;
    let flottant = bits == 32;
    let octets_ech = usize::from(bits / 8);
    let mut data = Vec::new();
    for i in 0..n {
        for c in canaux {
            let v = c.get(i).copied().unwrap_or(0.0);
            if flottant {
                data.extend_from_slice(&v.to_le_bytes());
            } else {
                let m = (f64::from(v) * 8_388_607.0).round() as i32;
                data.extend_from_slice(&m.to_le_bytes()[..3]);
            }
        }
    }
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&(if flottant { 3u16 } else { 1u16 }).to_le_bytes());
    w.extend_from_slice(&ch.to_le_bytes());
    w.extend_from_slice(&sr.to_le_bytes());
    w.extend_from_slice(&(sr * u32::from(ch) * octets_ech as u32).to_le_bytes());
    w.extend_from_slice(&(ch * bits / 8).to_le_bytes());
    w.extend_from_slice(&bits.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    w
}

fn ri(n: usize, a: f32) -> Vec<f32> {
    (0..n)
        .map(|i| if i == 0 { a } else { a * 0.5f32.powi(i as i32) })
        .collect()
}

fn archive(fichiers: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut tampon = Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut tampon);
        let o = SimpleFileOptions::default();
        for (nom, contenu) in fichiers {
            z.start_file(*nom, o).unwrap();
            z.write_all(contenu).unwrap();
        }
        z.finish().unwrap();
    }
    tampon.into_inner()
}

#[test]
fn un_wav_seul_donne_un_jeu_d_une_reponse() {
    let j = importer(
        &wav(&[ri(64, 0.9), ri(64, 0.8)], 96_000, 32),
        "room_96k.wav",
    )
    .unwrap();
    assert_eq!(j.reponses.len(), 1);
    assert_eq!(j.reponses[0].sample_rate, 96_000);
    assert_eq!(j.reponses[0].taps.len(), 2);
    assert_eq!(j.reponses[0].taps[0][0], 0.9);
}

#[test]
fn un_zip_de_wav_par_cadence_maquette_a1() {
    // La maquette A1 : living-room.zip = room_44k, room_48k, room_96k.
    let z = archive(&[
        (
            "living-room/room_44k.wav",
            wav(&[ri(32, 0.5), ri(32, 0.4)], 44_100, 24),
        ),
        (
            "living-room/room_48k.wav",
            wav(&[ri(32, 0.6), ri(32, 0.4)], 48_000, 32),
        ),
        (
            "living-room/room_96k.wav",
            wav(&[ri(64, 0.7), ri(64, 0.4)], 96_000, 32),
        ),
        ("__MACOSX/living-room/._room_44k.wav", b"junk".to_vec()),
        ("living-room/notes.txt", b"REW export".to_vec()),
    ]);
    let j = importer(&z, "living-room.zip").unwrap();
    let cadences: Vec<_> = j.reponses.iter().map(|r| r.sample_rate).collect();
    assert_eq!(cadences, [44_100, 48_000, 96_000]);
    assert!(j.reponses.iter().all(|r| r.taps.len() == 2));
    assert_eq!(j.indice_principal(), Some(1), "48 kHz devient le principal");
    assert!(j.ecartes.is_empty(), "{:?}", j.ecartes);
}

#[test]
fn deux_mono_gauche_droite_par_cadence_deviennent_un_stereo_export_rew() {
    let z = archive(&[
        ("filter_R_48k.wav", wav(&[ri(40, 0.3)], 48_000, 32)),
        ("filter_L_48k.wav", wav(&[ri(50, 0.9)], 48_000, 32)),
        ("Left 96k.wav", wav(&[ri(40, 0.8)], 96_000, 32)),
        ("Right 96k.wav", wav(&[ri(40, 0.2)], 96_000, 32)),
    ]);
    let j = importer(&z, "rew.zip").unwrap();
    assert_eq!(j.reponses.len(), 2);
    for r in &j.reponses {
        assert_eq!(r.origine, Origine::PaireMono);
        assert_eq!(r.taps.len(), 2);
        assert_eq!(r.taps[0].len(), r.taps[1].len());
    }
    assert_eq!(j.reponses[0].taps[0][0], 0.9, "gauche à gauche");
    assert_eq!(j.reponses[0].taps[1][0], 0.3, "droite à droite");
}

#[test]
fn deux_fichiers_a_la_meme_cadence_sans_cote_sont_refuses() {
    let z = archive(&[
        ("a.wav", wav(&[ri(8, 0.3), ri(8, 0.3)], 48_000, 32)),
        ("b.wav", wav(&[ri(8, 0.3), ri(8, 0.3)], 48_000, 32)),
    ]);
    assert_eq!(importer(&z, "x.zip").unwrap_err().raison, "duplicate_rate");
}

const CFG_96: &str =
    "96000 2 2 0\n0 0\n0 0\nroom_96k.wav\n0\n0.0\n0.0\nroom_96k.wav\n1\n1.0\n1.0\n";

#[test]
fn un_zip_de_cfg_convolver_avec_leurs_ri() {
    let cfg_48 = "48000 2 2 0\r\n0 0\r\n0.5 0\r\n\r\nL48.wav\r\n0\r\n0.0\r\n0.0\r\nR48.wav\r\n0\r\n1.0\r\n1.0\r\n";
    let z = archive(&[
        ("conv/room_96k.cfg", CFG_96.as_bytes().to_vec()),
        (
            "conv/room_96k.wav",
            wav(&[ri(16, 0.7), ri(16, 0.6)], 96_000, 32),
        ),
        ("conv/room_48k.cfg", cfg_48.as_bytes().to_vec()),
        ("conv/L48.wav", wav(&[ri(16, 0.9)], 48_000, 32)),
        ("conv/R48.wav", wav(&[ri(16, 0.8)], 48_000, 32)),
        ("conv/inutile.wav", wav(&[ri(16, 0.1)], 44_100, 32)),
    ]);
    let j = importer(&z, "convolver.zip").unwrap();
    assert_eq!(j.reponses.len(), 2);
    let r48 = &j.reponses[0];
    assert_eq!((r48.sample_rate, r48.origine), (48_000, Origine::Cfg));
    // Retard de sortie de 0,5 ms sur le canal 0 : 24 échantillons à 48 kHz.
    assert_eq!(r48.taps[0][..24], [0.0f32; 24]);
    assert_eq!(r48.taps[0][24], 0.9);
    assert_eq!(r48.taps[1][0], 0.8);
    assert_eq!(r48.taps[0].len(), r48.taps[1].len());
    let r96 = &j.reponses[1];
    assert_eq!(r96.taps[0][0], 0.7);
    assert_eq!(r96.taps[1][0], 0.6);
    assert_eq!(j.ecartes.len(), 1, "le wav non référencé est signalé");
}

#[test]
fn un_cfg_qui_melange_les_canaux_est_refuse() {
    let cfg = "96000 2 2 0\n0 0\n0 0\nroom_96k.wav\n0\n0.0 1.0\n0.0\nroom_96k.wav\n1\n1.0\n1.0\n";
    let z = archive(&[
        ("a.cfg", cfg.as_bytes().to_vec()),
        ("room_96k.wav", wav(&[ri(16, 0.7), ri(16, 0.6)], 96_000, 32)),
    ]);
    assert_eq!(importer(&z, "x.zip").unwrap_err().raison, "matrix_routing");
}

#[test]
fn un_cfg_qui_reference_un_fichier_hors_de_l_archive_est_refuse() {
    let cfg = CFG_96.replace("room_96k.wav", "../../etc/passwd.wav");
    let z = archive(&[
        ("sous/a.cfg", cfg.into_bytes()),
        ("room_96k.wav", wav(&[ri(16, 0.7), ri(16, 0.6)], 96_000, 32)),
    ]);
    assert_eq!(importer(&z, "x.zip").unwrap_err().raison, "unsafe_path");
}

#[test]
fn un_cfg_a_une_autre_cadence_que_son_wav_est_refuse() {
    let z = archive(&[
        ("a.cfg", CFG_96.as_bytes().to_vec()),
        ("room_96k.wav", wav(&[ri(16, 0.7), ri(16, 0.6)], 48_000, 32)),
    ]);
    assert_eq!(importer(&z, "x.zip").unwrap_err().raison, "invalid_cfg");
}

// ---------------------------------------------------------------------------
// Archives piégées (contre-épreuves de sécurité)
// ---------------------------------------------------------------------------

#[test]
fn piege_chemin_avec_point_point() {
    let z = archive(&[("../../evil.wav", wav(&[ri(8, 0.5)], 48_000, 32))]);
    assert_eq!(importer(&z, "x.zip").unwrap_err().raison, "unsafe_path");
}

#[test]
fn piege_chemin_absolu() {
    let z = archive(&[("/etc/evil.wav", wav(&[ri(8, 0.5)], 48_000, 32))]);
    assert_eq!(importer(&z, "x.zip").unwrap_err().raison, "unsafe_path");
}

#[test]
fn piege_lien_symbolique() {
    let mut tampon = Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut tampon);
        z.add_symlink("room_48k.wav", "/etc/passwd", SimpleFileOptions::default())
            .unwrap();
        z.finish().unwrap();
    }
    assert_eq!(
        importer(&tampon.into_inner(), "x.zip").unwrap_err().raison,
        "symlink"
    );
}

#[test]
fn piege_bombe_de_decompression() {
    // 100 Mio de zéros : quelques centaines de kio compressés. Refusée sur
    // les octets RÉELLEMENT décompressés, sans tout décompresser.
    let mut tampon = Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut tampon);
        z.start_file("bombe.wav", SimpleFileOptions::default())
            .unwrap();
        let zeros = vec![0u8; 1 << 20];
        for _ in 0..100 {
            z.write_all(&zeros).unwrap();
        }
        z.finish().unwrap();
    }
    let octets = tampon.into_inner();
    assert!(
        octets.len() < 2 << 20,
        "la bombe est petite : {}",
        octets.len()
    );
    let refus = importer(&octets, "x.zip").unwrap_err();
    assert!(
        matches!(refus.raison, "too_large" | "compression_ratio"),
        "{refus}"
    );
}

#[test]
fn piege_rapport_de_compression() {
    // 8 Mio de zéros : sous le plafond de taille, mais un rapport absurde.
    let mut tampon = Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut tampon);
        z.start_file("plat.wav", SimpleFileOptions::default())
            .unwrap();
        z.write_all(&vec![0u8; 8 << 20]).unwrap();
        z.finish().unwrap();
    }
    assert_eq!(
        importer(&tampon.into_inner(), "x.zip").unwrap_err().raison,
        "compression_ratio"
    );
}

#[test]
fn piege_trop_d_entrees() {
    let fichiers: Vec<(String, Vec<u8>)> = (0..ENTREES_MAX + 1)
        .map(|i| (format!("f{i}.txt"), vec![b'x']))
        .collect();
    let refs: Vec<(&str, Vec<u8>)> = fichiers
        .iter()
        .map(|(n, c)| (n.as_str(), c.clone()))
        .collect();
    assert_eq!(
        importer(&archive(&refs), "x.zip").unwrap_err().raison,
        "too_many_entries"
    );
}

#[test]
fn piege_wav_aux_cadences_hors_bornes() {
    let z = archive(&[("a.wav", wav(&[ri(8, 0.5)], 4_000_000, 32))]);
    // Écarté (pas d'autre réponse) : l'archive n'a aucun filtre utilisable.
    assert_eq!(importer(&z, "x.zip").unwrap_err().raison, "no_filter");
}

#[test]
fn pas_une_archive() {
    assert_eq!(
        importer(b"PK\x03\x04garbage", "x.zip").unwrap_err().raison,
        "not_a_zip"
    );
    assert_eq!(
        importer(b"pas un wav", "x.wav").unwrap_err().raison,
        "bad_wav"
    );
}

#[test]
fn le_jeu_ecrit_se_relit_a_l_identique() {
    // Ce que la route écrit (`ecrire_ri_wav`) est ce que la convolution relit.
    let j = importer(
        &archive(&[
            ("filter_L.wav", wav(&[ri(20, 0.9)], 44_100, 32)),
            ("filter_R.wav", wav(&[ri(30, 0.3)], 44_100, 32)),
        ]),
        "x.zip",
    )
    .unwrap();
    let dossier = crate::test_scratch::scratch_dir("import-ri-t09a");
    let chemin = dossier.path().join("zone_1_sr44100.wav");
    let r = &j.reponses[0];
    crate::audio::convolver::ecrire_ri_wav(chemin.to_str().unwrap(), &r.taps, r.sample_rate)
        .unwrap();
    let (taps, sr) = Convolver::read_ir_taps(chemin.to_str().unwrap()).unwrap();
    assert_eq!(sr, 44_100);
    assert_eq!(taps, r.taps);
}

// ---------------------------------------------------------------------------
// Relecture de #9451 : cas supplémentaires
// ---------------------------------------------------------------------------

/// Marque la première entrée comme chiffrée (bit 0 des drapeaux, en-tête
/// local et répertoire central) : la forme d'une entrée ZipCrypto.
fn marquer_chiffree(mut z: Vec<u8>) -> Vec<u8> {
    assert_eq!(&z[..4], b"PK\x03\x04");
    z[6] |= 1;
    let central = z
        .windows(4)
        .position(|w| w == b"PK\x01\x02")
        .expect("répertoire central");
    z[central + 8] |= 1;
    z
}

#[test]
fn piege_entree_chiffree() {
    let z = marquer_chiffree(archive(&[("room_48k.wav", wav(&[ri(8, 0.5)], 48_000, 32))]));
    assert_eq!(importer(&z, "x.zip").unwrap_err().raison, "encrypted");
}

#[test]
fn piege_chemins_windows() {
    for nom in [
        "C:\\evil.wav",
        "C:/evil.wav",
        "\\evil.wav",
        "..\\..\\evil.wav",
        "a\\..\\..\\evil.wav",
    ] {
        let z = archive(&[(nom, wav(&[ri(8, 0.5)], 48_000, 32))]);
        assert_eq!(
            importer(&z, "x.zip").unwrap_err().raison,
            "unsafe_path",
            "{nom}"
        );
    }
}

#[test]
fn un_cfg_en_chemin_absolu_est_refuse() {
    for cible in ["/etc/room_96k.wav", "C:\\room_96k.wav", "\\room_96k.wav"] {
        let cfg = CFG_96.replace("room_96k.wav", cible);
        let z = archive(&[
            ("a.cfg", cfg.into_bytes()),
            ("room_96k.wav", wav(&[ri(16, 0.7), ri(16, 0.6)], 96_000, 32)),
        ]);
        assert_eq!(
            importer(&z, "x.zip").unwrap_err().raison,
            "unsafe_path",
            "{cible}"
        );
    }
}

#[test]
fn les_noms_unicode_sont_acceptes() {
    let z = archive(&[
        (
            "salon-é/réponse_48k_gauche.wav",
            wav(&[ri(8, 0.9)], 48_000, 32),
        ),
        (
            "salon-é/réponse_48k_droite.wav",
            wav(&[ri(8, 0.4)], 48_000, 32),
        ),
        (
            "salon-é/房间_96k.wav",
            wav(&[ri(8, 0.5), ri(8, 0.5)], 96_000, 32),
        ),
    ]);
    let j = importer(&z, "salon.zip").unwrap();
    assert_eq!(j.reponses.len(), 2);
    assert_eq!(j.reponses[0].taps[0][0], 0.9, "gauche à gauche");
}

#[test]
fn un_zip_imbrique_n_est_pas_ouvert() {
    let interieur = archive(&[("room_48k.wav", wav(&[ri(8, 0.5)], 48_000, 32))]);
    let z = archive(&[("filtres.zip", interieur)]);
    assert_eq!(importer(&z, "x.zip").unwrap_err().raison, "no_filter");
}

/// Un WAV dont l'en-tête annonce plus de prises qu'une réponse admise est
/// refusé AVANT décodage ; un en-tête qui annonce plus de données que le
/// fichier n'en porte est ramené à ce qui est présent.
#[test]
fn un_wav_dont_l_en_tete_ment() {
    // 33 canaux annoncés : au-delà du plafond.
    let mut trop_de_canaux = wav(&[ri(8, 0.5)], 48_000, 32);
    trop_de_canaux[22..24].copy_from_slice(&33u16.to_le_bytes());
    let e = importer(&trop_de_canaux, "a.wav").unwrap_err();
    assert_eq!(e.raison, "bad_wav", "{e}");
    // Taille de données annoncée à 4 Gio, 8 prises présentes : lu tel quel.
    let mut taille_fausse = wav(&[ri(8, 0.5)], 48_000, 32);
    let n = taille_fausse.len();
    taille_fausse[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
    let j = importer(&taille_fausse, "b.wav").unwrap();
    assert_eq!(j.reponses[0].taps[0].len(), (n - 44) / 4);
    // Plus de prises que le plafond, réellement présentes : refusé avant
    // l'allocation des canaux décodés.
    let longues = vec![0.0f32; par_cadence::PRISES_MAX + 1];
    let e = importer(&wav(&[longues], 48_000, 32), "c.wav").unwrap_err();
    assert_eq!(e.raison, "bad_wav", "{e}");
}

// ---------------------------------------------------------------------------
// Relecture de suivi de #9451 : mémoire du mode `.cfg`
// ---------------------------------------------------------------------------

/// Un bruit déterministe (peu compressible), `n` prises.
fn bruit(n: usize) -> Vec<f32> {
    let mut x = 0x2545_f491_u32;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 0.01
        })
        .collect()
}

/// Un `.cfg` à `canaux` entrées et sorties, chaque chemin citant le même
/// fichier `ri.wav` (canal 0).
fn cfg_diagonal(sr: u32, canaux: usize, retard_ms: &str) -> String {
    let zeros = vec!["0"; canaux].join(" ");
    let retards = vec![retard_ms; canaux].join(" ");
    let mut t = format!("{sr} {canaux} {canaux} 0\n{retards}\n{zeros}\n");
    for c in 0..canaux {
        t.push_str(&format!("ri.wav\n0\n{c}.0\n{c}.0\n"));
    }
    t
}

#[test]
fn un_cfg_qui_multiplie_un_meme_fichier_depasse_le_budget_du_jeu() {
    // 32 sorties × 600 000 prises = 19,2 M échantillons, au-delà du budget
    // du jeu, depuis UN seul fichier de 2,4 Mo.
    let z = archive(&[
        ("a.cfg", cfg_diagonal(48_000, 32, "0").into_bytes()),
        ("ri.wav", wav(&[bruit(600_000)], 48_000, 32)),
    ]);
    let e = importer(&z, "x.zip").unwrap_err();
    assert_eq!(e.raison, "too_large", "{e}");
    // Contre-épreuve : 8 sorties tiennent dans le budget.
    let z = archive(&[
        ("a.cfg", cfg_diagonal(48_000, 8, "0").into_bytes()),
        ("ri.wav", wav(&[bruit(600_000)], 48_000, 32)),
    ]);
    let j = importer(&z, "x.zip").unwrap();
    assert_eq!(j.reponses[0].taps.len(), 8);
}

#[test]
fn un_retard_qui_depasse_les_prises_admises_est_refuse_avant_allocation() {
    let n = par_cadence::PRISES_MAX - 1_000;
    let z = archive(&[
        ("a.cfg", cfg_diagonal(48_000, 1, "1000").into_bytes()),
        ("ri.wav", wav(&[bruit(n)], 48_000, 32)),
    ]);
    assert_eq!(importer(&z, "x.zip").unwrap_err().raison, "invalid_cfg");
}

#[test]
fn un_cfg_a_une_cadence_inusuelle_est_refuse() {
    let z = archive(&[
        ("a.cfg", cfg_diagonal(32_000, 1, "0").into_bytes()),
        ("ri.wav", wav(&[ri(8, 0.5)], 32_000, 32)),
    ]);
    assert_eq!(importer(&z, "x.zip").unwrap_err().raison, "invalid_cfg");
}

#[test]
fn trop_de_cfg_sont_refuses() {
    let fichiers: Vec<(String, Vec<u8>)> = (0..REPONSES_MAX + 1)
        .map(|i| {
            (
                format!("f{i}.cfg"),
                cfg_diagonal(48_000, 1, "0").into_bytes(),
            )
        })
        .chain(std::iter::once((
            "ri.wav".to_string(),
            wav(&[ri(8, 0.5)], 48_000, 32),
        )))
        .collect();
    let refs: Vec<(&str, Vec<u8>)> = fichiers
        .iter()
        .map(|(n, c)| (n.as_str(), c.clone()))
        .collect();
    assert_eq!(
        importer(&archive(&refs), "x.zip").unwrap_err().raison,
        "too_many_entries"
    );
}
