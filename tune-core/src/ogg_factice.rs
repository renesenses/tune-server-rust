//! Des flux Ogg fabriqués pour les tests des radios Ogg : un Ogg Opus encodé
//! ici même par libopus (une sinusoïde), et un Ogg Vorbis tiré de la fixture
//! `test_vorbis.ogg` dont on récrit les commentaires. Chaque flux porte ses
//! propres `ARTIST` / `TITLE`, pour fabriquer un Ogg chaîné à la manière
//! d'Icecast : un flux logique par titre, mis bout à bout.
//!
//! Aucun outil externe : les pages sont écrites ici, CRC compris.

/// CRC-32 des pages Ogg : polynôme 0x04C11DB7, sans réflexion, départ à 0.
fn crc_ogg(octets: &[u8]) -> u32 {
    let mut crc: u32 = 0;
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

/// Une page Ogg portant des paquets ENTIERS (chacun tient dans la page).
fn page(serial: u32, sequence: u32, granule: u64, drapeaux: u8, paquets: &[&[u8]]) -> Vec<u8> {
    let mut segments: Vec<u8> = Vec::new();
    for p in paquets {
        let mut reste = p.len();
        while reste >= 255 {
            segments.push(255);
            reste -= 255;
        }
        segments.push(reste as u8);
    }
    assert!(segments.len() <= 255, "trop de segments pour une page");
    let mut o = Vec::new();
    o.extend_from_slice(b"OggS");
    o.push(0);
    o.push(drapeaux);
    o.extend_from_slice(&granule.to_le_bytes());
    o.extend_from_slice(&serial.to_le_bytes());
    o.extend_from_slice(&sequence.to_le_bytes());
    o.extend_from_slice(&[0; 4]);
    o.push(segments.len() as u8);
    o.extend_from_slice(&segments);
    for p in paquets {
        o.extend_from_slice(p);
    }
    let crc = crc_ogg(&o);
    o[22..26].copy_from_slice(&crc.to_le_bytes());
    o
}

/// Un en-tête de commentaires : `magie`, vendeur, puis `CLÉ=valeur`.
fn commentaires(magie: &[u8], balises: &[(&str, &str)], bit_de_cadrage: bool) -> Vec<u8> {
    let mut o = magie.to_vec();
    let vendeur = b"tune-tests";
    o.extend_from_slice(&(vendeur.len() as u32).to_le_bytes());
    o.extend_from_slice(vendeur);
    o.extend_from_slice(&(balises.len() as u32).to_le_bytes());
    for (cle, valeur) in balises {
        let c = format!("{cle}={valeur}");
        o.extend_from_slice(&(c.len() as u32).to_le_bytes());
        o.extend_from_slice(c.as_bytes());
    }
    if bit_de_cadrage {
        o.push(1);
    }
    o
}

/// Un flux logique Ogg Opus stéréo 48 kHz : `secondes` de sinusoïde à
/// 440 Hz, encodés par libopus en trames de 20 ms, avec ces balises.
pub(crate) fn ogg_opus(serial: u32, secondes: f32, balises: &[(&str, &str)]) -> Vec<u8> {
    const PRE_SKIP: u16 = 312;
    let mut tete = b"OpusHead".to_vec();
    tete.push(1); // version
    tete.push(2); // canaux
    tete.extend_from_slice(&PRE_SKIP.to_le_bytes());
    tete.extend_from_slice(&48_000u32.to_le_bytes());
    tete.extend_from_slice(&0i16.to_le_bytes());
    tete.push(0); // famille de projection 0 : mono ou stéréo

    let mut o = page(serial, 0, 0, 0x02, &[&tete]);
    o.extend(page(
        serial,
        1,
        0,
        0,
        &[&commentaires(b"OpusTags", balises, false)],
    ));

    let mut encodeur = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
        .expect("encodeur opus");
    let trames = ((secondes * 50.0) as usize).max(1);
    let mut paquets: Vec<Vec<u8>> = Vec::with_capacity(trames);
    for t in 0..trames {
        let mut pcm = Vec::with_capacity(960 * 2);
        for i in 0..960 {
            let n = (t * 960 + i) as f32;
            let s = 0.4 * (2.0 * std::f32::consts::PI * 440.0 * n / 48_000.0).sin();
            pcm.push(s);
            pcm.push(s);
        }
        let mut sortie = vec![0u8; 4000];
        let n = encodeur.encode_float(&pcm, &mut sortie).expect("encodage");
        sortie.truncate(n);
        paquets.push(sortie);
    }
    let mut granule = u64::from(PRE_SKIP);
    let lots: Vec<&[Vec<u8>]> = paquets.chunks(10).collect();
    for (i, lot) in lots.iter().enumerate() {
        granule += 960 * lot.len() as u64;
        let refs: Vec<&[u8]> = lot.iter().map(|p| p.as_slice()).collect();
        let fin = if i + 1 == lots.len() { 0x04 } else { 0 };
        o.extend(page(serial, 2 + i as u32, granule, fin, &refs));
    }
    o
}

/// Les pages d'un fichier Ogg : (granule, drapeaux, table des segments, corps).
fn pages(octets: &[u8]) -> Vec<(u64, u8, Vec<u8>, Vec<u8>)> {
    let mut v = Vec::new();
    let mut i = 0;
    while i + 27 <= octets.len() {
        assert_eq!(&octets[i..i + 4], b"OggS", "page Ogg attendue à {i}");
        let drapeaux = octets[i + 5];
        let granule = u64::from_le_bytes(octets[i + 6..i + 14].try_into().unwrap());
        let n = usize::from(octets[i + 26]);
        let segments = octets[i + 27..i + 27 + n].to_vec();
        let taille: usize = segments.iter().map(|s| usize::from(*s)).sum();
        let debut = i + 27 + n;
        v.push((
            granule,
            drapeaux,
            segments,
            octets[debut..debut + taille].to_vec(),
        ));
        i = debut + taille;
    }
    v
}

/// Un flux logique Ogg Vorbis : la fixture `test_vorbis.ogg` (~2 s), son
/// en-tête de commentaires remplacé par ces balises, ses pages renumérotées
/// sous `serial`.
pub(crate) fn ogg_vorbis(serial: u32, balises: &[(&str, &str)]) -> Vec<u8> {
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/test_vorbis.ogg"
    ))
    .expect("fixture vorbis");
    let toutes = pages(&fixture);
    // Les trois en-têtes (identification, commentaires, configuration),
    // réassemblés depuis les premières pages.
    let mut en_tetes: Vec<Vec<u8>> = Vec::new();
    let mut courant: Vec<u8> = Vec::new();
    let mut consommees = 0;
    'pages: for (_, _, segments, corps) in &toutes {
        consommees += 1;
        let mut pos = 0;
        for s in segments {
            let s = usize::from(*s);
            courant.extend_from_slice(&corps[pos..pos + s]);
            pos += s;
            if s < 255 {
                en_tetes.push(std::mem::take(&mut courant));
                if en_tetes.len() == 3 {
                    assert_eq!(pos, corps.len(), "la configuration finit sa page");
                    break 'pages;
                }
            }
        }
    }
    let mut o = page(serial, 0, 0, 0x02, &[&en_tetes[0]]);
    let tags = commentaires(b"\x03vorbis", balises, true);
    o.extend(page(serial, 1, 0, 0, &[&tags, &en_tetes[2]]));
    for (i, (granule, drapeaux, segments, corps)) in toutes[consommees..].iter().enumerate() {
        let sequence = 2 + i as u32;
        let mut p = Vec::new();
        p.extend_from_slice(b"OggS");
        p.push(0);
        p.push(*drapeaux & !0x02);
        p.extend_from_slice(&granule.to_le_bytes());
        p.extend_from_slice(&serial.to_le_bytes());
        p.extend_from_slice(&sequence.to_le_bytes());
        p.extend_from_slice(&[0; 4]);
        p.push(segments.len() as u8);
        p.extend_from_slice(segments);
        p.extend_from_slice(corps);
        let crc = crc_ogg(&p);
        p[22..26].copy_from_slice(&crc.to_le_bytes());
        o.extend(p);
    }
    o
}

/// Le CRC est celui des pages réelles : la fixture le vérifie.
#[test]
fn le_crc_est_celui_des_pages_de_la_fixture() {
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/test_vorbis.ogg"
    ))
    .unwrap();
    let mut page0 = fixture[..27 + usize::from(fixture[26])].to_vec();
    let taille: usize = page0[27..].iter().map(|s| usize::from(*s)).sum();
    page0.extend_from_slice(&fixture[page0.len()..page0.len() + taille]);
    let attendu = u32::from_le_bytes(page0[22..26].try_into().unwrap());
    page0[22..26].copy_from_slice(&[0; 4]);
    assert_eq!(crc_ogg(&page0), attendu);
}
