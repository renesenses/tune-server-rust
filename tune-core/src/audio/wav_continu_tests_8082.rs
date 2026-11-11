//! #8082 — le WAV continu : en-tête exact, PCM des pistes bout à bout à
//! l'octet près, tranches `Range` à cheval sur une frontière, départ décalé.
use super::*;

const FLAC_16: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flac/ref_16_44100_stereo.flac"
);
const FLAC_24: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flac/ref_24_96000_stereo.flac"
);

/// Un WAV 16 bits stéréo dont chaque échantillon est connu d'avance.
fn wav(chemin: &Path, trames: u32, graine: i32) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(trames as usize * 4);
    for t in 0..trames as i32 {
        for c in 0..2 {
            let v = ((t.wrapping_mul(37) + c * 1000 + graine) % 30_000) as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut f = crate::audio::wav::build_wav_header_with_data_size(2, 44_100, 16, pcm.len() as u32)
        .to_vec();
    f.extend_from_slice(&pcm);
    std::fs::write(chemin, f).unwrap();
    pcm
}

fn tout(v: &WavContinu, debut: u64, longueur: u64) -> Vec<u8> {
    let mut out = Vec::new();
    v.lire(debut, longueur, &mut |m| {
        out.extend_from_slice(&m);
        Ok(())
    })
    .unwrap();
    out
}

fn segments(chemins: &[(&Path, u64)]) -> Vec<Segment> {
    chemins
        .iter()
        .map(|(c, t)| Segment {
            fichier: c.to_path_buf(),
            depart: 0,
            trames: *t,
        })
        .collect()
}

#[test]
fn deux_wav_bout_a_bout_a_l_octet_pres_8082() {
    let bac = crate::test_scratch::scratch_dir("flux-continu-8082");
    let (a, b) = (bac.join("a.wav"), bac.join("b.wav"));
    let pcm_a = wav(&a, 30_001, 1);
    let pcm_b = wav(&b, 20_003, 7);
    let sa = sonder(&a).unwrap();
    assert_eq!(sa.trames, 30_001);
    assert_eq!(
        sa.format,
        FormatPcm {
            cadence: 44_100,
            profondeur: 16,
            canaux: 2
        }
    );
    let v = WavContinu::nouveau(sa.format, segments(&[(&a, 30_001), (&b, 20_003)])).unwrap();
    let attendu_pcm: Vec<u8> = pcm_a.iter().chain(&pcm_b).copied().collect();
    assert_eq!(
        v.total(),
        44 + attendu_pcm.len() as u64,
        "somme exacte des pistes"
    );
    let corps = tout(&v, 0, v.total());
    assert_eq!(&corps[..4], b"RIFF");
    assert_eq!(
        u32::from_le_bytes(corps[40..44].try_into().unwrap()) as usize,
        attendu_pcm.len(),
        "le chunk data annonce la somme"
    );
    assert!(
        corps[44..] == attendu_pcm[..],
        "PCM bout à bout, sans un échantillon de plus ou de moins"
    );

    // Tranches : à cheval sur la frontière, au milieu d'une trame, dans
    // l'en-tête, hors fichier.
    let frontiere = 44 + pcm_a.len() as u64;
    for (debut, longueur) in [
        (frontiere - 1_001, 2_002),
        (frontiere, 10),
        (45, 7),
        (10, 100),
        (v.total() - 3, 100),
        (frontiere - 3, 3),
    ] {
        let attendu: Vec<u8> = {
            let complet = [&corps[..]].concat();
            let fin = (debut + longueur).min(v.total()) as usize;
            complet[debut as usize..fin].to_vec()
        };
        assert_eq!(
            tout(&v, debut, longueur),
            attendu,
            "tranche {debut}+{longueur}"
        );
    }
}

#[test]
fn depart_decale_dans_la_premiere_piste_8082() {
    let bac = crate::test_scratch::scratch_dir("flux-continu-depart-8082");
    let (a, b) = (bac.join("a.wav"), bac.join("b.wav"));
    let pcm_a = wav(&a, 30_000, 3);
    let pcm_b = wav(&b, 10_000, 5);
    let format = sonder(&a).unwrap().format;
    let v = WavContinu::nouveau(
        format,
        vec![
            Segment {
                fichier: a.clone(),
                depart: 12_345,
                trames: 30_000 - 12_345,
            },
            Segment {
                fichier: b.clone(),
                depart: 0,
                trames: 10_000,
            },
        ],
    )
    .unwrap();
    let attendu: Vec<u8> = pcm_a[12_345 * 4..].iter().chain(&pcm_b).copied().collect();
    assert!(tout(&v, 0, v.total())[44..] == attendu[..]);
    assert_eq!(
        v.bornes_ms(),
        vec![(0, 400), (400, 227)],
        "frontières et durées servies, en ms"
    );
}

#[test]
fn flac_decode_sans_traitement_et_somme_exacte_8082() {
    for chemin in [FLAC_16, FLAC_24] {
        let p = Path::new(chemin);
        let s = sonder(p).unwrap();
        let ref_ = crate::audio::decode::decode_to_pcm(chemin, None, None, 0.0, 0.0).unwrap();
        let pcm = ref_.pcm_bytes();
        assert_eq!(
            s.trames * s.format.octets_par_trame(),
            pcm.len() as u64,
            "{chemin} : STREAMINFO = trames décodées"
        );
        let v = WavContinu::nouveau(
            s.format,
            segments(&[(p, s.trames), (p, s.trames), (p, s.trames)]),
        )
        .unwrap();
        let corps = tout(&v, 0, v.total());
        assert_eq!(corps.len() as u64, 44 + 3 * pcm.len() as u64);
        for k in 0..3 {
            let d = 44 + k * pcm.len();
            assert!(
                corps[d..d + pcm.len()] == pcm[..],
                "{chemin} : piste {k} intacte"
            );
        }
        // Une tranche au milieu de la deuxième piste (recherche FLAC).
        let debut = 44 + pcm.len() as u64 + 3 * pcm.len() as u64 / 5 + 1;
        assert_eq!(
            tout(&v, debut, 9_999),
            corps[debut as usize..debut as usize + 9_999].to_vec(),
            "{chemin} : tranche après recherche"
        );
    }
}

#[test]
fn une_piste_avec_perte_est_refusee_8082() {
    let bac = crate::test_scratch::scratch_dir("flux-continu-refus-8082");
    let f = bac.join("x.wav");
    // WAV flottant 32 bits : pas d'entier, pas de flux continu.
    let mut h = crate::audio::wav::build_wav_header_with_data_size(2, 44_100, 32, 8).to_vec();
    h[20..22].copy_from_slice(&3u16.to_le_bytes());
    h.extend_from_slice(&[0u8; 8]);
    std::fs::write(&f, h).unwrap();
    assert!(sonder(&f).is_err());
}

#[test]
fn le_plafond_riff_est_respecte_8082() {
    let format = FormatPcm {
        cadence: 192_000,
        profondeur: 24,
        canaux: 2,
    };
    let max = WavContinu::trames_max(&format);
    assert!(WavContinu::nouveau(format, segments(&[(Path::new("/x"), max)])).is_ok());
    assert!(
        WavContinu::nouveau(
            format,
            segments(&[(Path::new("/x"), max), (Path::new("/y"), 1)])
        )
        .is_err()
    );
}
