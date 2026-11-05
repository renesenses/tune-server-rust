//! #6385 — importer un fichier d'égaliseur venu d'ailleurs, éprouvé sur de
//! VRAIS fichiers publiés : trois configurations Equalizer APO et un profil OPRA
//! (`tests/fixtures/egaliseur_6385/`, origine et licence dans `LISEZMOI.md`),
//! plus le HD 650 d'AutoEq déjà en fixture (#1405).
//!
//! Chaque erreur porte une clé stable (`ErreurAutoEq::cle`) que la route rend
//! en `code` : c'est ce que le client traduit.

use tune_core::audio::autoeq::{
    ErreurAutoEq, FiltreIgnore, RaisonIgnore, analyser, analyser_fichier, format_du_fichier,
};

const APO: &str = include_str!("fixtures/egaliseur_6385/apo_rew_stereo_lg_tv.txt");
const APO_SHP9500: &str = include_str!("fixtures/egaliseur_6385/apo_philips_shp9500.txt");
const APO_HYPERX: &str = include_str!("fixtures/egaliseur_6385/apo_hyperx_cloud_ii_device.txt");
const OPRA: &str = include_str!("fixtures/egaliseur_6385/opra_sennheiser_hd650_oratory1990.json");
const HD_650: &str = include_str!("fixtures/autoeq/sennheiser_hd_650.txt");

fn proche(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// --- Equalizer APO ---

/// Le `Device:` de tête d'un fichier APO, tel que le compte rendu le nomme.
fn device_ligne_1() -> FiltreIgnore {
    FiltreIgnore {
        ligne: 1,
        type_filtre: None,
        raison: RaisonIgnore::Peripherique,
    }
}

/// Le fichier APO Philips SHP9500 tel que publié (CRLF, sans saut de ligne
/// final) : son `Preamp`, ses plateaux `LSC`/`HSC` et ses crêtes arrivent
/// valeur par valeur, sans rien écarter.
#[test]
fn la_configuration_apo_reelle_avec_preamp_simporte_telle_quelle() {
    assert_eq!(format_du_fichier(APO_SHP9500), "parametric_eq");
    let profil = analyser_fichier(APO_SHP9500).expect("configuration APO valide");
    assert!(
        proche(profil.preamp_db, -12.5),
        "Preamp: -12.5 dB — lu : {}",
        profil.preamp_db
    );
    assert_eq!(profil.bandes.len(), 10);
    assert!(profil.ignores.is_empty());
    let types: Vec<&str> = profil.bandes.iter().map(|b| b.band_type.as_str()).collect();
    assert_eq!(
        types,
        [
            "low_shelf",
            "peak",
            "low_shelf",
            "peak",
            "peak",
            "peak",
            "peak",
            "peak",
            "peak",
            "high_shelf"
        ]
    );
    let b = &profil.bandes;
    // `Filter: ON LSC Fc 35 Hz Gain 7 dB Q 0.71`
    assert!(proche(b[0].freq, 35.0) && proche(b[0].gain, 7.0) && proche(b[0].q, 0.71));
    // `Filter: ON PK Fc 95 Hz Gain -1.5 dB Q 1`
    assert!(proche(b[1].freq, 95.0) && proche(b[1].gain, -1.5) && proche(b[1].q, 1.0));
    // `Filter: ON PK Fc 3000 Hz Gain -1.9 dB Q 4`
    assert!(proche(b[6].freq, 3000.0) && proche(b[6].gain, -1.9) && proche(b[6].q, 4.0));
    // `Filter: ON HSC Fc 11000 Hz Gain 0 dB Q 0.71` (dernière ligne, sans CRLF)
    assert!(proche(b[9].freq, 11000.0) && proche(b[9].gain, 0.0) && proche(b[9].q, 0.71));
    assert!(b.iter().all(|b| b.channel.is_none()));
}

/// Le fichier APO HyperX Cloud II tel que publié : sa ligne `Device:` est
/// écartée et NOMMÉE (ligne 1), ses commentaires sautés, son `Preamp` et ses
/// dix filtres lus.
#[test]
fn la_ligne_device_dun_fichier_apo_reel_est_ecartee_et_nommee() {
    let profil = analyser_fichier(APO_HYPERX).expect("configuration APO valide");
    assert_eq!(profil.ignores, [device_ligne_1()]);
    assert_eq!(profil.ignores[0].raison.cle(), "device_selection");
    assert!(
        proche(profil.preamp_db, -6.4),
        "Preamp: -6.4 dB — lu : {}",
        profil.preamp_db
    );
    assert_eq!(profil.bandes.len(), 10);
    let b = &profil.bandes;
    // `Filter 1: ON LSC Fc 105 Hz Gain 4.1 dB Q 0.70`
    assert_eq!(b[0].band_type, "low_shelf");
    assert!(proche(b[0].freq, 105.0) && proche(b[0].gain, 4.1) && proche(b[0].q, 0.7));
    // `Filter 6: ON HSC Fc 10000 Hz Gain -7.0 dB Q 0.70`
    assert_eq!(b[5].band_type, "high_shelf");
    assert!(proche(b[5].freq, 10000.0) && proche(b[5].gain, -7.0) && proche(b[5].q, 0.7));
    // `Filter 10: ON PK Fc 1123 Hz Gain 1.1 dB Q 3.29`
    assert_eq!(b[9].band_type, "peak");
    assert!(proche(b[9].freq, 1123.0) && proche(b[9].gain, 1.1) && proche(b[9].q, 3.29));
}

/// Un second `Device:`, ou un `Device:` après un réglage, découperait le
/// fichier par périphérique : refusé et nommé, jamais fondu en un profil.
#[test]
fn un_device_qui_nest_pas_en_tete_reste_refuse() {
    let deux =
        format!("{APO_HYPERX}\nDevice: Haut-parleurs\nFilter 11: ON PK Fc 50 Hz Gain 2 dB Q 1\n");
    let erreur = analyser_fichier(&deux).unwrap_err();
    assert_eq!(erreur.cle(), "unrecognized_line");
    assert_eq!(erreur.ligne(), Some(APO_HYPERX.lines().count() + 2));

    let erreur =
        analyser("Preamp: -3 dB\nDevice: Casque\nFilter 1: ON PK Fc 100 Hz Gain 2 dB Q 1\n")
            .unwrap_err();
    assert_eq!(erreur, ErreurAutoEq::LigneIncomprise { ligne: 2 });
}

/// Les 35 filtres REW du fichier, gauche puis droite, valeur par valeur aux
/// deux bouts de chaque section. Le `Device:` de tête est écarté et nommé ;
/// la limite de 31 bandes est celle de la route (témoin côté serveur).
#[test]
fn la_configuration_apo_reelle_donne_ses_bandes_canal_par_canal() {
    let profil = analyser_fichier(APO).expect("configuration APO valide");
    assert_eq!(profil.bandes.len(), 35, "16 filtres à gauche, 19 à droite");
    assert_eq!(profil.preamp_db, 0.0, "le fichier ne porte pas de Preamp");
    assert_eq!(profil.ignores, [device_ligne_1()]);

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
