//! #6385 — importer un fichier d'égaliseur venu d'ailleurs, éprouvé sur de
//! VRAIS fichiers publiés : une configuration Equalizer APO et un profil OPRA
//! (`tests/fixtures/egaliseur_6385/`, origine et licence dans `LISEZMOI.md`),
//! plus le HD 650 d'AutoEq déjà en fixture (#1405).
//!
//! Chaque erreur porte une clé stable (`ErreurAutoEq::cle`) que la route rend
//! en `code` : c'est ce que le client traduit.

use tune_core::audio::autoeq::{ErreurAutoEq, analyser, analyser_fichier, format_du_fichier};

const APO: &str = include_str!("fixtures/egaliseur_6385/apo_rew_stereo_lg_tv.txt");
const OPRA: &str = include_str!("fixtures/egaliseur_6385/opra_sennheiser_hd650_oratory1990.json");
const HD_650: &str = include_str!("fixtures/autoeq/sennheiser_hd_650.txt");

/// Le fichier APO sans sa ligne `Device:` — la seule directive qu'il porte en
/// dehors de `Channel:` et `Filter`. Le reste est intact.
fn apo_sans_device() -> &'static str {
    APO.split_once('\n').expect("fichier multiligne").1
}

fn proche(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// --- Equalizer APO ---

/// Les 35 filtres REW du fichier, gauche puis droite, valeur par valeur aux
/// deux bouts de chaque section.
#[test]
fn la_configuration_apo_reelle_donne_ses_bandes_canal_par_canal() {
    let profil = analyser_fichier(apo_sans_device()).expect("configuration APO valide");
    assert_eq!(profil.bandes.len(), 35, "16 filtres à gauche, 19 à droite");
    assert_eq!(profil.preamp_db, 0.0, "le fichier ne porte pas de Preamp");
    assert!(profil.ignores.is_empty());

    let gauche: Vec<_> = profil
        .bandes
        .iter()
        .filter(|b| b.channel == Some(0))
        .collect();
    let droite: Vec<_> = profil
        .bandes
        .iter()
        .filter(|b| b.channel == Some(1))
        .collect();
    assert_eq!((gauche.len(), droite.len()), (16, 19));
    assert!(profil.bandes.iter().all(|b| b.band_type == "peak"));

    // `Filter  1: ON  PK       Fc   300.0 Hz  Gain -10.10 dB  Q  1.000`
    assert!(
        proche(gauche[0].freq, 300.0) && proche(gauche[0].gain, -10.10) && proche(gauche[0].q, 1.0)
    );
    // `Filter 16: ON  PK       Fc    1415 Hz  Gain  -1.10 dB  Q  4.936`
    assert!(
        proche(gauche[15].freq, 1415.0)
            && proche(gauche[15].gain, -1.10)
            && proche(gauche[15].q, 4.936)
    );
    // `Filter  1: ON  PK       Fc    8689 Hz  Gain  -8.30 dB  Q  1.780`
    assert!(
        proche(droite[0].freq, 8689.0)
            && proche(droite[0].gain, -8.30)
            && proche(droite[0].q, 1.78)
    );
    // `Filter 19: ON  PK       Fc    1216 Hz  Gain  -2.20 dB  Q  5.000`
    assert!(
        proche(droite[18].freq, 1216.0)
            && proche(droite[18].gain, -2.20)
            && proche(droite[18].q, 5.0)
    );
}

/// Le fichier tel que publié commence par `Device: …`, une directive que Tune
/// ne lit pas : il est REFUSÉ en nommant la ligne 1 et la clé, jamais importé
/// à moitié.
#[test]
fn la_ligne_device_du_fichier_apo_reel_est_refusee_et_nommee() {
    let erreur = analyser_fichier(APO).expect_err("Device: n'est pas lu");
    assert_eq!(erreur, ErreurAutoEq::LigneIncomprise { ligne: 1 });
    assert_eq!(erreur.cle(), "unrecognized_line");
    assert_eq!(erreur.ligne(), Some(1));
}

/// Les types d'Equalizer APO que l'analyseur accepte sont ceux que le message
/// d'erreur annonce : il citait sept types quand l'analyseur en lit treize.
#[test]
fn le_message_dun_type_inconnu_cite_tous_les_types_lus() {
    let erreur = analyser("Filter 1: ON BP Fc 1000 Hz Gain 3 dB Q 1\n").unwrap_err();
    assert_eq!(erreur.cle(), "unknown_filter_type");
    let message = erreur.to_string();
    for t in ["PK", "PEQ", "LSC", "HSC", "LP", "LPQ", "HP", "HPQ", "NO"] {
        assert!(message.contains(t), "{t} absent du message : {message}");
    }
}

// --- AutoEq ---

/// Une ligne invalide dans un vrai profil AutoEq : la clé et la ligne sont
/// nommées, pas seulement une phrase.
#[test]
fn une_ligne_invalide_dun_profil_autoeq_reel_donne_une_erreur_nommee() {
    let abime = HD_650.replace("Filter 3: ON PK Fc 118 Hz", "Filter 3: ON PK Fc cent Hz");
    let erreur = analyser_fichier(&abime).unwrap_err();
    assert_eq!(erreur.cle(), "invalid_number");
    assert_eq!(erreur.ligne(), Some(4));

    let erreur = analyser_fichier(&format!("{HD_650}Delay: 10 ms\n")).unwrap_err();
    assert_eq!(erreur.cle(), "unrecognized_line");
    assert_eq!(erreur.ligne(), Some(HD_650.lines().count() + 1));
}

// --- OPRA ---

#[test]
fn le_profil_opra_reel_donne_ses_bandes_son_gain_son_q_et_son_preampli() {
    assert_eq!(format_du_fichier(OPRA), "opra");
    let profil = analyser_fichier(OPRA).expect("profil OPRA valide");
    assert!(
        proche(profil.preamp_db, -9.3),
        "gain_db global : {}",
        profil.preamp_db
    );
    assert_eq!(profil.bandes.len(), 10);
    let types: Vec<&str> = profil.bandes.iter().map(|b| b.band_type.as_str()).collect();
    assert_eq!(
        types,
        [
            "peak",
            "peak",
            "low_shelf",
            "peak",
            "peak",
            "high_shelf",
            "peak",
            "peak",
            "peak",
            "high_shelf"
        ]
    );
    let b = &profil.bandes;
    assert!(proche(b[0].freq, 20.0) && proche(b[0].gain, 4.0) && proche(b[0].q, 1.1));
    assert!(proche(b[2].freq, 105.0) && proche(b[2].gain, 5.5) && proche(b[2].q, 0.71));
    assert!(proche(b[9].freq, 11000.0) && proche(b[9].gain, -2.5) && proche(b[9].q, 0.71));
    assert!(b.iter().all(|b| b.channel.is_none()));
    // Comme le `Preamp` d'AutoEq : la marge de Tune le couvre, sans cumuler.
    assert!(profil.marge_de_tune_couvre_le_preamp());
}

/// Une ligne `eq` de la base `database_v1.jsonl` enveloppe le même objet.
#[test]
fn une_entree_de_la_base_opra_se_lit_comme_son_info_json() {
    let data: serde_json::Value = serde_json::from_str(OPRA).unwrap();
    let ligne =
        serde_json::json!({"type": "eq", "id": "sennheiser_hd650_x", "data": data}).to_string();
    let profil = analyser_fichier(&ligne).expect("entrée JSONL valide");
    assert_eq!(profil.bandes.len(), 10);
    assert!(proche(profil.preamp_db, -9.3));
}

/// Une bande invalide dans le vrai profil OPRA : la bande et la clé sont
/// nommées, et rien n'est importé.
#[test]
fn une_bande_invalide_dun_profil_opra_reel_donne_une_erreur_nommee() {
    let abime = OPRA.replacen("\"peak_dip\"", "\"band_pass\"", 2);
    // Le premier remplacement vise la bande 1.
    let erreur = analyser_fichier(&abime).unwrap_err();
    assert_eq!(erreur.cle(), "unknown_filter_type");
    assert_eq!(erreur.bande(), Some(1));
    assert!(erreur.to_string().contains("bande 1"), "{erreur}");

    let erreur = analyser_fichier(&OPRA.replacen("\"q\": 1.1", "\"q\": 0", 1)).unwrap_err();
    assert_eq!(erreur.cle(), "value_out_of_range");
    assert_eq!(erreur.bande(), Some(1));

    let erreur = analyser_fichier("{ pas du json").unwrap_err();
    assert_eq!(erreur.cle(), "invalid_json");
}

/// Un passe-haut OPRA à 24 dB/oct n'est pas un biquad : refusé, pas approché.
#[test]
fn une_pente_que_le_biquad_ne_fait_pas_est_refusee() {
    let texte = r#"{"author":"x","type":"parametric_eq","parameters":{"gain_db":0,
        "bands":[{"type":"high_pass","frequency":20,"slope":24}]}}"#;
    let erreur = analyser_fichier(texte).unwrap_err();
    assert_eq!(erreur.cle(), "unsupported_slope");
    assert_eq!(erreur.bande(), Some(1));

    let texte = texte.replace("\"slope\":24", "\"slope\":12");
    let profil = analyser_fichier(&texte).expect("12 dB/oct est le biquad");
    assert_eq!(profil.bandes[0].band_type, "high_pass");
    assert!(proche(profil.bandes[0].q, std::f64::consts::FRAC_1_SQRT_2));
}
