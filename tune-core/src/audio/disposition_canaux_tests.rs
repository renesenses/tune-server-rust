//! Témoins de la disposition déclarée : un fichier SYNTHÉTIQUE par format
//! (WAV, FLAC, DSF, DFF), écrit octet par octet, puis le routage qui en
//! découle vers 2, 6 et 8 voies.
use super::*;
use std::io::Write;

fn ecrire(octets: &[u8], ext: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join(format!("piste.{ext}"));
    std::fs::File::create(&chemin)
        .unwrap()
        .write_all(octets)
        .unwrap();
    (dir, chemin)
}

/// WAV WAVE_FORMAT_EXTENSIBLE, `canaux` canaux, masque `masque`, 4 trames.
fn wav(canaux: u16, masque: u32) -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
    fmt.extend_from_slice(&canaux.to_le_bytes());
    fmt.extend_from_slice(&48_000u32.to_le_bytes());
    fmt.extend_from_slice(&(48_000u32 * 3 * u32::from(canaux)).to_le_bytes());
    fmt.extend_from_slice(&(3 * canaux).to_le_bytes());
    fmt.extend_from_slice(&24u16.to_le_bytes());
    fmt.extend_from_slice(&22u16.to_le_bytes());
    fmt.extend_from_slice(&24u16.to_le_bytes());
    fmt.extend_from_slice(&masque.to_le_bytes());
    fmt.extend_from_slice(&[
        0x01, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71,
    ]);
    let data = vec![0u8; 4 * 3 * usize::from(canaux)];
    let mut v = b"RIFF\0\0\0\0WAVE".to_vec();
    // Un bloc inconnu avant `fmt `, de taille impaire : il faut le sauter.
    v.extend_from_slice(b"JUNK\x03\0\0\0abc\0");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    v.extend_from_slice(&fmt);
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

/// FLAC : STREAMINFO (`canaux`), puis un VORBIS_COMMENT portant `tag`.
fn flac(canaux: u16, tag: Option<&str>) -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    let mut info = vec![0u8; 34];
    info[10] = 0x0B;
    info[11] = 0xB8;
    info[12] = ((canaux - 1) as u8) << 1;
    info[13] = 0x70;
    v.push(0x00);
    v.extend_from_slice(&[0, 0, 34]);
    v.extend_from_slice(&info);
    let mut c = Vec::new();
    let vendeur = b"reference libFLAC 1.4.3";
    c.extend_from_slice(&(vendeur.len() as u32).to_le_bytes());
    c.extend_from_slice(vendeur);
    let tags: Vec<String> = ["TITLE=Quad".to_string()]
        .into_iter()
        .chain(tag.map(|t| format!("WAVEFORMATEXTENSIBLE_CHANNEL_MASK={t}")))
        .collect();
    c.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for t in &tags {
        c.extend_from_slice(&(t.len() as u32).to_le_bytes());
        c.extend_from_slice(t.as_bytes());
    }
    v.push(0x84);
    v.extend_from_slice(&(c.len() as u32).to_be_bytes()[1..]);
    v.extend_from_slice(&c);
    v
}

/// DSF : « DSD » (28 octets) puis « fmt » (52 octets) de type `t`.
fn dsf(t: u32, canaux: u32) -> Vec<u8> {
    let mut v = b"DSD ".to_vec();
    v.extend_from_slice(&28u64.to_le_bytes());
    v.extend_from_slice(&[0u8; 16]);
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&52u64.to_le_bytes());
    for x in [1u32, 0, t, canaux, 2_822_400, 1] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend_from_slice(&0u64.to_le_bytes());
    v.extend_from_slice(&4096u32.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v
}

/// DFF : FRM8/DSD, FVER, puis PROP/SND avec FS et CHNL `ids`.
fn dff(ids: &[&[u8; 4]]) -> Vec<u8> {
    let bloc = |id: &[u8; 4], corps: &[u8]| {
        let mut b = id.to_vec();
        b.extend_from_slice(&(corps.len() as u64).to_be_bytes());
        b.extend_from_slice(corps);
        if corps.len() % 2 == 1 {
            b.push(0);
        }
        b
    };
    let mut chnl = (ids.len() as u16).to_be_bytes().to_vec();
    for id in ids {
        chnl.extend_from_slice(*id);
    }
    let mut snd = b"SND ".to_vec();
    snd.extend(bloc(b"FS  ", &2_822_400u32.to_be_bytes()));
    snd.extend(bloc(b"CHNL", &chnl));
    let mut corps = b"DSD ".to_vec();
    corps.extend(bloc(b"FVER", &[1, 5, 0, 0]));
    corps.extend(bloc(b"PROP", &snd));
    bloc(b"FRM8", &corps)
}

fn lu(octets: Vec<u8>, ext: &str) -> Option<Disposition> {
    let (_dir, chemin) = ecrire(&octets, ext);
    lire_le_fichier(&chemin)
}

#[test]
fn wav_extensible_le_masque_est_lu() {
    assert_eq!(
        lu(wav(4, 0x33), "wav").unwrap().positions(),
        [FL, FR, BL, BR]
    );
    // 4 canaux en 3.1 : FL FR FC LFE.
    assert_eq!(
        lu(wav(4, 0x0F), "wav").unwrap().positions(),
        [FL, FR, FC, LFE]
    );
    // 5.1 « latéral ».
    assert_eq!(
        lu(wav(6, 0x60F), "wav").unwrap().positions(),
        [FL, FR, FC, LFE, SL, SR]
    );
    // Masque nul ou incohérent : rien n'est déclaré.
    assert!(lu(wav(4, 0), "wav").is_none());
    assert!(lu(wav(4, 0x3F), "wav").is_none());
}

#[test]
fn flac_le_commentaire_de_masque_est_lu() {
    assert_eq!(
        lu(flac(4, Some("0x0107")), "flac").unwrap().positions(),
        [FL, FR, FC, BC]
    );
    assert_eq!(
        lu(flac(4, Some("51")), "flac").unwrap().positions(),
        [FL, FR, BL, BR]
    );
    assert!(lu(flac(4, None), "flac").is_none());
}

#[test]
fn dsf_le_type_de_canaux_est_lu() {
    assert_eq!(lu(dsf(4, 4), "dsf").unwrap().positions(), [FL, FR, BL, BR]);
    assert_eq!(lu(dsf(5, 4), "dsf").unwrap().positions(), [FL, FR, FC, LFE]);
    assert_eq!(
        lu(dsf(7, 6), "dsf").unwrap().positions(),
        [FL, FR, FC, LFE, BL, BR]
    );
    assert!(
        lu(dsf(5, 6), "dsf").is_none(),
        "type et nombre de canaux se contredisent"
    );
}

#[test]
fn dff_la_liste_chnl_est_lue_dans_l_ordre_du_flux() {
    assert_eq!(
        lu(dff(&[b"SLFT", b"SRGT", b"LS  ", b"RS  "]), "dff")
            .unwrap()
            .positions(),
        [FL, FR, BL, BR]
    );
    // Un ordre inhabituel est rendu tel quel : c'est celui du flux.
    assert_eq!(
        lu(dff(&[b"C   ", b"LFE ", b"SLFT", b"SRGT"]), "dff")
            .unwrap()
            .positions(),
        [FC, LFE, FL, FR]
    );
}

fn canal(v: &[f32], n: usize, c: usize) -> Vec<f32> {
    v.iter().skip(c).step_by(n).copied().collect()
}

fn router(d: &Disposition, sortie: u16, entree: &[f32]) -> Vec<f32> {
    let m = matrice_de_routage(d, sortie).unwrap();
    let n = d.positions().len();
    entree
        .chunks_exact(n)
        .flat_map(|t| {
            m.chunks_exact(n)
                .map(|l| l.iter().zip(t).map(|(c, x)| c * x).sum::<f32>())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// LE défaut : un 4.0 vers 6 voies met ses arrière sur les arrière — jamais
/// sur le centre ni le LFE — par une simple recopie (gain 1).
#[test]
fn quad_vers_6_voies_arriere_sur_arriere_centre_et_lfe_muets() {
    let quad = Disposition::par_defaut(4).unwrap();
    let entree: Vec<f32> = (0..8)
        .flat_map(|t| [0.1, 0.2, 0.3, 0.4].map(|x| x + t as f32 / 100.0))
        .collect();
    let six = router(&quad, 6, &entree);
    assert_eq!(canal(&six, 6, 0), canal(&entree, 4, 0), "FL");
    assert_eq!(canal(&six, 6, 1), canal(&entree, 4, 1), "FR");
    assert!(canal(&six, 6, 2).iter().all(|x| *x == 0.0), "FC muet");
    assert!(canal(&six, 6, 3).iter().all(|x| *x == 0.0), "LFE muet");
    assert_eq!(canal(&six, 6, 4), canal(&entree, 4, 2), "BL sur BL");
    assert_eq!(canal(&six, 6, 5), canal(&entree, 4, 3), "BR sur BR");
}

/// Vers la stéréo : les voies arrière sont MIXÉES de leur côté à −3 dB,
/// normalisé — pas perdues, et sans écrêtage.
#[test]
fn quad_vers_stereo_mixe_l_arriere_de_son_cote() {
    let m = matrice_de_routage(&Disposition::par_defaut(4).unwrap(), 2).unwrap();
    let s = 1.0 + K;
    let attendu = [1.0 / s, 0.0, K / s, 0.0, 0.0, 1.0 / s, 0.0, K / s];
    for (a, b) in m.iter().zip(attendu) {
        assert!((a - b).abs() < 1e-6, "{m:?}");
    }
    let plein = router(&Disposition::par_defaut(4).unwrap(), 2, &[1.0; 4]);
    assert!(plein.iter().all(|x| *x <= 1.0 + 1e-6));
}

/// La disposition DÉCLARÉE change le routage : un DSF 4 canaux de type 5
/// (3.1 : FL FR FC LFE) vers 6 voies garde son centre et son LFE à leur
/// place, là où l'ordre par défaut (quad) les aurait envoyés à l'arrière.
#[test]
fn un_3_1_declare_garde_centre_et_lfe() {
    let d = depuis_dsf(5, 4).unwrap();
    let six = router(&d, 6, &[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(six, [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);
    // Vers la stéréo : le centre à −3 dB des deux côtés, le LFE écarté.
    let deux = router(&d, 2, &[0.0, 0.0, 0.5, 0.9]);
    assert!((deux[0] - deux[1]).abs() < 1e-7 && deux[0] > 0.0);
    assert!(deux[0] < 0.5 * K + 1e-6, "le LFE n'est pas replié dans G/D");
}

#[test]
fn le_5_1_lateral_tombe_sur_les_arriere_d_une_sortie_6_voies() {
    let d = Disposition::depuis_masque(0x60F, 6).unwrap();
    let six = router(&d, 6, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    assert_eq!(six, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    let huit = router(&d, 8, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    assert_eq!(
        huit,
        [1.0, 2.0, 3.0, 4.0, 0.0, 0.0, 5.0, 6.0],
        "SL/SR sur SL/SR d'un 7.1"
    );
}

#[test]
fn par_defaut_et_au_dela_de_8_voies() {
    assert!(Disposition::par_defaut(4).unwrap().est_par_defaut());
    assert!(!depuis_dsf(5, 4).unwrap().est_par_defaut());
    assert!(matrice_de_routage(&Disposition::par_defaut(4).unwrap(), 10).is_none());
}

/// Par l'adaptation commune (`audio/channels`) : un 4.0 vers la stéréo ne
/// perd plus ses voies arrière.
#[test]
fn l_adaptation_commune_ne_perd_plus_l_arriere_d_un_4_0() {
    let deux = crate::audio::channels::adapt_channels_f32(&[0.0, 0.0, 0.5, 0.0], 4, 2).unwrap();
    assert!(deux[0] > 0.2, "BL doit s'entendre à gauche : {deux:?}");
    assert_eq!(deux[1], 0.0, "rien à droite");
    let six = crate::audio::channels::adapt_channels_i32(&[1, 2, 3, 4], 4, 6, 24).unwrap();
    assert_eq!(
        six,
        [1, 2, 0, 0, 3, 4],
        "4.0 vers 6 voies, en entiers aussi"
    );
}

// ---------------------------------------------------------------------------
// Le badge sous la pochette suit la disposition DÉCLARÉE (« Catherine of
// Aragon », Rick Wakeman : FLAC 4.0, masque 0x0033, affiché « 5.1 »).
// ---------------------------------------------------------------------------

fn piste(chemin: &std::path::Path, canaux: i32) -> crate::db::models::Track {
    let mut t = crate::db::models::Track::new("Catherine of Aragon".into());
    t.file_path = Some(chemin.to_string_lossy().into_owned());
    t.channels = canaux;
    t
}

fn badge_json(t: &crate::db::models::Track) -> serde_json::Value {
    t.to_json()["channel_badge"].clone()
}

#[test]
fn badge_flac_quad_masque_0x33_est_4_0() {
    let (_d, c) = ecrire(&flac(4, Some("0x0033")), "flac");
    assert_eq!(badge_json(&piste(&c, 4)), serde_json::json!("4.0"));
}

#[test]
fn badge_flac_masque_0x3f_est_5_1() {
    let (_d, c) = ecrire(&flac(6, Some("0x003F")), "flac");
    assert_eq!(badge_json(&piste(&c, 6)), serde_json::json!("5.1"));
}

#[test]
fn badge_flac_masque_0x63f_est_7_1() {
    let (_d, c) = ecrire(&flac(8, Some("0x063F")), "flac");
    assert_eq!(badge_json(&piste(&c, 8)), serde_json::json!("7.1"));
}

#[test]
fn badge_suit_le_masque_et_non_le_compte() {
    // 6 canaux sans LFE (FL FR FC BL BR BC) : un 6.0, pas un 5.1.
    let (_d, c) = ecrire(&flac(6, Some("0x0137")), "flac");
    assert_eq!(badge_json(&piste(&c, 6)), serde_json::json!("6.0"));
    // 5.0 (FL FR FC BL BR) et 5.1.2 (hauteurs avant).
    let (_d2, c2) = ecrire(&flac(5, Some("0x0037")), "flac");
    assert_eq!(badge_json(&piste(&c2, 5)), serde_json::json!("5.0"));
    let (_d3, c3) = ecrire(&flac(8, Some("0x503F")), "flac");
    assert_eq!(badge_json(&piste(&c3, 8)), serde_json::json!("5.1.2"));
    // WAV extensible quadriphonique.
    let (_d4, c4) = ecrire(&wav(4, 0x33), "wav");
    assert_eq!(badge_json(&piste(&c4, 4)), serde_json::json!("4.0"));
}

#[test]
fn badge_sans_declaration_suit_le_nombre_de_canaux() {
    let (_d, c) = ecrire(&flac(4, None), "flac");
    assert_eq!(badge_json(&piste(&c, 4)), serde_json::json!("4.0"));
    let mut sans_fichier = crate::db::models::Track::new("x".into());
    for (n, attendu) in [
        (3, "3.0"),
        (4, "4.0"),
        (5, "5.0"),
        (6, "5.1"),
        (7, "6.1"),
        (8, "7.1"),
    ] {
        sans_fichier.channels = n;
        assert_eq!(
            badge_json(&sans_fichier),
            serde_json::json!(attendu),
            "{n} canaux"
        );
    }
    sans_fichier.channels = 2;
    assert_eq!(badge_json(&sans_fichier), serde_json::Value::Null);
    // Un masque qui ne compte pas les canaux du fichier ne fait pas foi.
    let (_d5, c5) = ecrire(&flac(4, Some("0x003F")), "flac");
    assert_eq!(badge_json(&piste(&c5, 4)), serde_json::json!("4.0"));
}

#[test]
fn badge_de_disposition_direct() {
    let d = |m: u32, n: u16| Disposition::depuis_masque(m, n).unwrap().badge();
    assert_eq!(d(0x33, 4).as_deref(), Some("4.0"));
    assert_eq!(d(0x3F, 6).as_deref(), Some("5.1"));
    assert_eq!(d(0x60F, 6).as_deref(), Some("5.1"));
    assert_eq!(d(0x63F, 8).as_deref(), Some("7.1"));
    assert_eq!(d(0x3, 2), None);
    assert_eq!(d(0x4, 1), None);
}
