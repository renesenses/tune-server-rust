//! #6044 — chaque canal reçoit le bon mélange, sur des signaux synthétiques
//! dont chaque canal est reconnaissable ; l'identité et les permutations sont
//! comparées AU BIT PRÈS.
use serde_json::json;
use tune_plugin_channel_remap::*;
use tune_plugin_sdk::audio::*;

/// Un signal où le canal `c` de la trame `t` vaut une valeur propre à (t, c) :
/// une erreur d'aiguillage ne peut pas passer inaperçue.
fn signal_i32(canaux: usize, trames: usize) -> Vec<i32> {
    (0..trames * canaux)
        .map(|k| {
            let (t, c) = (k / canaux, k % canaux);
            ((t as i64 * 7_919 + c as i64 * 1_000_003) % 8_000_000 - 4_000_000) as i32
        })
        .collect()
}

fn canal<T: Copy>(tampon: &[T], canaux: usize, c: usize) -> Vec<T> {
    tampon.iter().skip(c).step_by(canaux).copied().collect()
}

fn ctx() -> BlockContext {
    BlockContext {
        zone_id: 1,
        generation: 1,
        position_frames: 0,
    }
}

#[test]
fn identite_au_bit_pres_en_entiers_et_en_flottants() {
    for n in [1u16, 2, 4, 6, 8] {
        let m = Matrice::depuis_reglage(&identite(n)).unwrap();
        assert!(
            m.est_identite(),
            "{n} canaux : l'identité doit se reconnaître"
        );
        let entree = signal_i32(usize::from(n), 257);
        assert_eq!(m.appliquer_i32(&entree, 24), entree, "{n} canaux, entiers");
        // Flottants : y compris −0,0 et un sous-normal, comparés par leurs bits.
        let mut f: Vec<f32> = entree.iter().map(|x| *x as f32 / 8_388_608.0).collect();
        f[0] = -0.0;
        if f.len() > 1 {
            f[1] = f32::from_bits(1);
        }
        let rendu = m.appliquer_f32(&f);
        let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&rendu), bits(&f), "{n} canaux, flottants au bit près");
    }
}

#[test]
fn identite_neutre_pour_le_sdk_et_intacte_en_s24() {
    let f = AudioFormat::new(48_000, ChannelLayout::Discrete(6), SampleEncoding::S24Le).unwrap();
    let reglage = serde_json::to_value(identite(6)).unwrap();
    let pc = PlaybackContext {
        zone_id: 1,
        source: SourceKind::Streaming,
        delivery: Delivery::NetworkFile,
        pure: false,
        protected_bitstream: false,
    };
    assert_eq!(
        ChannelRemap.assess(&pc, &reglage).unwrap(),
        Applicability::Bypass(BypassReason::Neutral)
    );
    let mut p = ChannelRemap.prepare(f, 512, &reglage).unwrap();
    let origine: Vec<u8> = (0..512 * 6 * 3).map(|i| (i * 37 % 251) as u8).collect();
    let mut octets = origine.clone();
    p.process(
        &mut AudioBlock::new(f, SamplesMut::S24Le(&mut octets), 512).unwrap(),
        ctx(),
    )
    .unwrap();
    assert_eq!(octets, origine);
}

#[test]
fn echange_gauche_droite_au_bit_pres_en_s24_par_le_sdk() {
    let f = AudioFormat::new(96_000, ChannelLayout::Stereo, SampleEncoding::S24Le).unwrap();
    let reglage = serde_json::to_value(prereglage("swap_lr").unwrap()).unwrap();
    let mut p = ChannelRemap.prepare(f, 256, &reglage).unwrap();
    let origine: Vec<u8> = (0..256 * 2 * 3).map(|i| (i * 53 % 253) as u8).collect();
    let mut octets = origine.clone();
    p.process(
        &mut AudioBlock::new(f, SamplesMut::S24Le(&mut octets), 256).unwrap(),
        ctx(),
    )
    .unwrap();
    for (avant, apres) in origine.chunks_exact(6).zip(octets.chunks_exact(6)) {
        assert_eq!(
            &apres[..3],
            &avant[3..],
            "la gauche doit recevoir la droite"
        );
        assert_eq!(
            &apres[3..],
            &avant[..3],
            "la droite doit recevoir la gauche"
        );
    }
}

#[test]
fn quad_vers_5_1_avant_et_arriere_au_bit_pres_centre_et_lfe_muets() {
    let m = Matrice::du_reglage_arme(&prereglage("quad_to_5_1").unwrap()).unwrap();
    assert!(
        m.est_recopie(),
        "4.0 → 5.1 n'a pas à multiplier un seul échantillon"
    );
    let entree = signal_i32(4, 300);
    let sortie = m.appliquer_i32(&entree, 24);
    assert_eq!(sortie.len(), 300 * 6);
    // FL FR BL BR → FL FR FC LFE BL BR
    assert_eq!(canal(&sortie, 6, 0), canal(&entree, 4, 0), "FL");
    assert_eq!(canal(&sortie, 6, 1), canal(&entree, 4, 1), "FR");
    assert!(canal(&sortie, 6, 2).iter().all(|x| *x == 0), "FC muet");
    assert!(canal(&sortie, 6, 3).iter().all(|x| *x == 0), "LFE muet");
    assert_eq!(canal(&sortie, 6, 4), canal(&entree, 4, 2), "BL");
    assert_eq!(canal(&sortie, 6, 5), canal(&entree, 4, 3), "BR");
}

#[test]
fn quad_vers_7_1_arriere_sur_bl_br() {
    let m = Matrice::du_reglage_arme(&prereglage("quad_to_7_1").unwrap()).unwrap();
    let entree = signal_i32(4, 64);
    let sortie = m.appliquer_i32(&entree, 24);
    assert_eq!(canal(&sortie, 8, 4), canal(&entree, 4, 2));
    assert_eq!(canal(&sortie, 8, 5), canal(&entree, 4, 3));
    for c in [2, 3, 6, 7] {
        assert!(
            canal(&sortie, 8, c).iter().all(|x| *x == 0),
            "canal {c} muet"
        );
    }
}

#[test]
fn quad_vers_stereo_garde_l_arriere_et_n_ecrete_pas() {
    let m = Matrice::du_reglage_arme(&prereglage("quad_to_stereo").unwrap()).unwrap();
    let k = 10f64.powf(f64::from(MOINS_3_DB) / 20.0);
    let somme = 1.0 + k;
    // Chaque voie arrière est ENTENDUE, du bon côté.
    assert!((m.coefficient(0, 0) - 1.0 / somme).abs() < 1e-12);
    assert!((m.coefficient(0, 2) - k / somme).abs() < 1e-12);
    assert_eq!(m.coefficient(0, 1), 0.0, "rien de la droite à gauche");
    assert_eq!(m.coefficient(0, 3), 0.0);
    assert!((m.coefficient(1, 3) - k / somme).abs() < 1e-12);
    assert!((m.attenuation_db()[0] + 20.0 * somme.log10()).abs() < 1e-9);
    // Quatre voies corrélées à pleine échelle : pas d'écrêtage.
    let plein = vec![1.0f32; 4 * 32];
    assert!(m.appliquer_f32(&plein).iter().all(|x| x.abs() <= 1.0));
    // Arrière seul : il sort, atténué, sur son côté.
    let arriere_gauche: Vec<f32> = (0..32).flat_map(|_| [0.0, 0.0, 0.5, 0.0]).collect();
    let sortie = m.appliquer_f32(&arriere_gauche);
    assert!((f64::from(sortie[0]) - 0.5 * k / somme).abs() < 1e-7);
    assert_eq!(sortie[1], 0.0);
}

#[test]
fn cinq_un_vers_stereo_suit_l_itu_et_ecarte_le_lfe() {
    let m = Matrice::du_reglage_arme(&prereglage("5_1_to_stereo_itu").unwrap()).unwrap();
    let k = 10f64.powf(f64::from(MOINS_3_DB) / 20.0);
    let somme = 1.0 + 2.0 * k;
    // Gauche = FL + FC·k + BL·k, normalisé ; LFE et la droite absents.
    let attendu_g = [1.0, 0.0, k, 0.0, k, 0.0].map(|c| c / somme);
    let attendu_d = [0.0, 1.0, k, 0.0, 0.0, k].map(|c| c / somme);
    for i in 0..6 {
        assert!(
            (m.coefficient(0, i) - attendu_g[i]).abs() < 1e-12,
            "G ← {i}"
        );
        assert!(
            (m.coefficient(1, i) - attendu_d[i]).abs() < 1e-12,
            "D ← {i}"
        );
    }
}

#[test]
fn mono_rend_la_demi_somme_sur_les_deux_voies() {
    let m = Matrice::du_reglage_arme(&prereglage("mono").unwrap()).unwrap();
    let entree = [1000, 3000, -400, 800, 7, 9];
    assert_eq!(m.appliquer_i32(&entree, 24), [2000, 2000, 200, 200, 8, 8]);
}

#[test]
fn sans_normalisation_les_gains_restent_et_le_mixage_borne_en_entiers() {
    let mut s = prereglage("mono").unwrap();
    s.normalize = false;
    let m = Matrice::depuis_reglage(&s).unwrap();
    assert_eq!(m.coefficient(0, 0), 1.0);
    assert_eq!(m.attenuation_db(), [0.0, 0.0]);
    // Somme brute : 16 bits, borne à la pleine échelle au lieu d'enrouler.
    assert_eq!(m.appliquer_i32(&[30_000, 30_000], 16), [32_767, 32_767]);
}

#[test]
fn case_vide_est_un_silence_et_reglages_mal_formes_refuses() {
    let mut s = identite(2);
    s.gains_db[1][1] = None;
    let m = Matrice::depuis_reglage(&s).unwrap();
    assert_eq!(m.appliquer_i32(&[5, 6], 24), [5, 0]);

    let mut forme = identite(2);
    forme.gains_db.pop();
    assert_eq!(Matrice::depuis_reglage(&forme), Err(ErreurDeMatrice::Forme));
    let mut gain = identite(2);
    gain.gains_db[0][0] = Some(f32::NAN);
    assert_eq!(Matrice::depuis_reglage(&gain), Err(ErreurDeMatrice::Gain));
    gain.gains_db[0][0] = Some(GAIN_MAX_DB + 0.5);
    assert_eq!(Matrice::depuis_reglage(&gain), Err(ErreurDeMatrice::Gain));
    let mut canaux = identite(2);
    canaux.inputs = 0;
    assert_eq!(
        Matrice::depuis_reglage(&canaux),
        Err(ErreurDeMatrice::Canaux)
    );
    // Éteint : rien à appliquer.
    let mut eteint = prereglage("swap_lr").unwrap();
    eteint.enabled = false;
    assert!(Matrice::du_reglage_arme(&eteint).is_none());
}

#[test]
fn le_sdk_refuse_un_changement_du_nombre_de_canaux() {
    let f = AudioFormat::new(48_000, ChannelLayout::Discrete(4), SampleEncoding::F32).unwrap();
    let reglage = serde_json::to_value(prereglage("quad_to_5_1").unwrap()).unwrap();
    assert!(ChannelRemap.prepare(f, 512, &reglage).is_err());
    let mal_forme = json!({"enabled": true, "inputs": 2, "outputs": 2, "gains_db": [[0.0]]});
    assert!(ChannelRemap.prepare(f, 512, &mal_forme).is_err());
}

#[test]
fn tous_les_prereglages_sont_valides_et_armes() {
    for id in PREREGLAGES {
        let s = prereglage(id).unwrap_or_else(|| panic!("préréglage {id} absent"));
        assert!(s.enabled && s.normalize, "{id}");
        assert_eq!(s.preset.as_deref(), Some(id));
        assert!(Matrice::depuis_reglage(&s).is_ok(), "{id}");
    }
    assert!(prereglage("inconnu").is_none());
    assert_eq!(noms_des_canaux(4), Some(&["FL", "FR", "BL", "BR"][..]));
}
