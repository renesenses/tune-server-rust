//! #9040 — témoins de la plage de sonie (LRA) et du DR, sur des signaux
//! générés en mémoire : aucun fichier, aucun outil externe.

use super::*;
use crate::audio::analyzer::{duree_sonie, plage_et_dr_de};

const HZ: usize = 48_000;

/// Sinus stéréo de 1 kHz, `niveaux` = (dBFS crête, secondes) mis bout à bout
/// — les signaux d'EBU Tech 3342.
fn sinus_par_paliers(niveaux: &[(f64, f64)]) -> Vec<f64> {
    let mut v = Vec::new();
    let mut n = 0usize;
    for (db, s) in niveaux {
        let a = 10f64.powf(db / 20.0);
        for _ in 0..(s * HZ as f64) as usize {
            let x = a * (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / HZ as f64).sin();
            v.push(x);
            v.push(x);
            n += 1;
        }
    }
    v
}

/// Bruit rose déterministe (générateur congruentiel + filtre de Paul Kellet),
/// stéréo décorrélé, écart type ≈ `sigma`.
fn bruit_rose(secondes: f64, hz: usize, sigma: f64, graine: u64) -> Vec<f64> {
    let mut etat = graine;
    let mut blanc = move || {
        etat = etat
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // Somme de 4 uniformes : quasi gaussien, centré.
        let mut s = 0.0;
        for k in 0..4 {
            let u = ((etat >> (16 * k)) & 0xFFFF) as f64 / 65535.0;
            s += u - 0.5;
        }
        s * 1.732
    };
    let n = (secondes * hz as f64) as usize;
    let mut sortie = Vec::with_capacity(2 * n);
    let mut b = [[0.0f64; 7]; 2];
    for _ in 0..n {
        for canal in &mut b {
            let w = blanc();
            canal[0] = 0.99886 * canal[0] + w * 0.0555179;
            canal[1] = 0.99332 * canal[1] + w * 0.0750759;
            canal[2] = 0.96900 * canal[2] + w * 0.1538520;
            canal[3] = 0.86650 * canal[3] + w * 0.3104856;
            canal[4] = 0.55000 * canal[4] + w * 0.5329522;
            canal[5] = -0.7616 * canal[5] - w * 0.0168980;
            let rose = canal.iter().sum::<f64>() + w * 0.5362;
            canal[6] = w * 0.115926;
            sortie.push(rose);
        }
    }
    let ecart = (sortie.iter().map(|x| x * x).sum::<f64>() / sortie.len() as f64).sqrt();
    sortie.iter().map(|x| x * sigma / ecart).collect()
}

/// Gain (dB) appliqué par tranches de `secondes` : `gains[i % len]`.
fn par_tranches(signal: &[f64], hz: usize, secondes: f64, gains_db: &[f64]) -> Vec<f64> {
    let trames = (secondes * hz as f64) as usize;
    signal
        .chunks(2)
        .enumerate()
        .flat_map(|(i, t)| {
            let g = 10f64.powf(gains_db[(i / trames) % gains_db.len()] / 20.0);
            t.iter().map(move |x| x * g).collect::<Vec<_>>()
        })
        .collect()
}

/// Limiteur « brique » : écrête à `seuil`, puis remonte au même pic que
/// l'entrée — un master poussé au maximum.
fn limite(signal: &[f64], seuil: f64) -> Vec<f64> {
    let pic = signal.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    signal
        .iter()
        .map(|x| x.clamp(-seuil, seuil) * pic / seuil)
        .collect()
}

fn lra_de(signal: &[f64], hz: usize) -> f64 {
    plage_et_dr_de(signal, hz, 2)
        .0
        .expect("plage de sonie mesurée")
        .lra_lu
        .expect("au moins une fenêtre")
}

// ── EBU Tech 3342, cas 1 à 4 : LRA attendue ±1 LU ─────────────────────────
//
// Des paliers stables : les centiles tombent DANS les paliers, la valeur
// attendue est donc atteinte au dixième près, bien mieux que la tolérance de
// la norme.

fn verifier_tech_3342(niveaux: &[(f64, f64)], attendu: f64) {
    let lra = lra_de(&sinus_par_paliers(niveaux), HZ);
    eprintln!("Tech 3342 {niveaux:?} : LRA = {lra} (attendu {attendu})");
    assert!(
        (lra - attendu).abs() <= 0.2,
        "LRA {lra} au lieu de {attendu} ±0,2 (norme : ±1)"
    );
}

#[test]
fn tech_3342_cas_1_vingt_puis_trente_dbfs_donne_10_lu() {
    verifier_tech_3342(&[(-20.0, 20.0), (-30.0, 20.0)], 10.0);
}

#[test]
fn tech_3342_cas_2_vingt_puis_quinze_dbfs_donne_5_lu() {
    verifier_tech_3342(&[(-20.0, 20.0), (-15.0, 20.0)], 5.0);
}

#[test]
fn tech_3342_cas_3_quarante_puis_vingt_dbfs_donne_20_lu() {
    verifier_tech_3342(&[(-40.0, 20.0), (-20.0, 20.0)], 20.0);
}

/// La porte relative fait tout ici : les paliers à −50 dBFS sont à plus de
/// 20 LU sous la sonie du programme et sortent du calcul. Sans elle, la LRA
/// vaudrait 30.
#[test]
fn tech_3342_cas_4_la_porte_relative_ecarte_les_paliers_a_moins_50_donne_15_lu() {
    verifier_tech_3342(
        &[
            (-50.0, 20.0),
            (-35.0, 20.0),
            (-20.0, 20.0),
            (-35.0, 20.0),
            (-50.0, 20.0),
        ],
        15.0,
    );
}

// ── Bruit rose : compressé ou limité contre non compressé ──────────────────

/// Un bruit rose de niveau constant n'a presque pas de plage de sonie.
#[test]
fn bruit_rose_constant_a_une_lra_proche_de_zero() {
    let lra = lra_de(&bruit_rose(60.0, 44_100, 0.1, 7), 44_100);
    eprintln!("bruit rose constant : LRA = {lra}");
    assert!(lra <= 1.0, "LRA {lra}");
}

/// Passages forts et calmes à 12 dB d'écart : LRA ≈ 12. Le même programme
/// compressé 4:1 sur ses passages calmes (−12 → −3 dB) : LRA ≈ 3.
#[test]
fn compresser_les_passages_calmes_resserre_la_lra() {
    let rose = bruit_rose(80.0, 44_100, 0.1, 11);
    let brut = par_tranches(&rose, 44_100, 10.0, &[0.0, -12.0]);
    let comprime = par_tranches(&rose, 44_100, 10.0, &[0.0, -3.0]);
    let (lra_brut, lra_comprime) = (lra_de(&brut, 44_100), lra_de(&comprime, 44_100));
    eprintln!("LRA brut = {lra_brut}, comprimé = {lra_comprime}");
    assert!((11.0..=13.0).contains(&lra_brut), "brut {lra_brut}");
    assert!(
        (2.0..=4.0).contains(&lra_comprime),
        "comprimé {lra_comprime}"
    );
}

/// Le DR (TT) d'un bruit rose non compressé contre le même bruit limité.
/// Un bruit gaussien a des crêtes vers 4,5 σ : DR ≈ 20·log₁₀(4,5/√2) ≈ 10.
/// Écrêté à 1,5 σ puis remonté, ses crêtes ne dépassent plus le corps du son
/// que de peu : DR ≈ 20·log₁₀(1,5/(√2·0,85)) ≈ 2.
#[test]
fn limiter_un_bruit_rose_fait_tomber_son_dr() {
    let rose = bruit_rose(60.0, 44_100, 0.1, 3);
    let limite = limite(&rose, 0.15);
    let (_, dr_brut) = plage_et_dr_de(&rose, 44_100, 2);
    let (_, dr_limite) = plage_et_dr_de(&limite, 44_100, 2);
    let (dr_brut, dr_limite) = (dr_brut.unwrap(), dr_limite.unwrap());
    eprintln!("DR brut = {dr_brut}, limité = {dr_limite}");
    assert!((8..=13).contains(&dr_brut), "brut DR{dr_brut}");
    assert!(dr_limite <= 4, "limité DR{dr_limite}");
    assert!(dr_brut >= dr_limite + 5);
}

/// DR et LRA ne mesurent pas la même chose : alterner les niveaux (LRA ≈ 12)
/// ne change presque pas le DR, qui ne regarde que les passages forts ;
/// limiter ne change presque pas la LRA d'un bruit constant.
#[test]
fn le_dr_ne_voit_pas_la_lra_et_la_lra_ne_voit_pas_le_limiteur() {
    let rose = bruit_rose(80.0, 44_100, 0.1, 5);
    let alterne = par_tranches(&rose, 44_100, 10.0, &[0.0, -12.0]);
    let (_, dr_constant) = plage_et_dr_de(&rose, 44_100, 2);
    let (_, dr_alterne) = plage_et_dr_de(&alterne, 44_100, 2);
    let (dr_constant, dr_alterne) = (dr_constant.unwrap(), dr_alterne.unwrap());
    eprintln!("DR constant = {dr_constant}, alterné = {dr_alterne}");
    assert!(dr_constant.abs_diff(dr_alterne) <= 1);
    let lra_limite = lra_de(&limite(&rose, 0.15), 44_100);
    assert!(lra_limite <= 1.0, "LRA limitée {lra_limite}");
}

// ── Album : fusion des histogrammes ────────────────────────────────────────

/// Deux pistes de niveau constant, LRA 0 chacune, à −20 et −30 dBFS : leur
/// album a une LRA de 10. Ni la moyenne ni le maximum des LRA de piste ne la
/// donnent.
#[test]
fn la_lra_d_un_album_se_calcule_sur_toutes_ses_fenetres() {
    let a = plage_et_dr_de(&sinus_par_paliers(&[(-20.0, 20.0)]), HZ, 2)
        .0
        .unwrap();
    let b = plage_et_dr_de(&sinus_par_paliers(&[(-30.0, 20.0)]), HZ, 2)
        .0
        .unwrap();
    assert_eq!((a.lra_lu, b.lra_lu), (Some(0.0), Some(0.0)));
    let mut album = Histogramme::default();
    album.fusionner(&a.histogramme);
    album.fusionner(&b.histogramme);
    let lra = album.lra().unwrap();
    eprintln!("album : LRA = {lra}");
    assert!((lra - 10.0).abs() <= 0.1, "album {lra}");
}

/// L'album d'une seule piste a la LRA de la piste, à une case (0,1 LU) près.
#[test]
fn l_histogramme_d_une_piste_rend_sa_lra_a_une_case_pres() {
    let rose = bruit_rose(80.0, 44_100, 0.1, 13);
    let p = plage_et_dr_de(
        &par_tranches(&rose, 44_100, 7.0, &[0.0, -6.0, -15.0]),
        44_100,
        2,
    )
    .0
    .unwrap();
    let h = p.histogramme.lra().unwrap();
    let l = p.lra_lu.unwrap();
    eprintln!("piste {l} / histogramme {h}");
    assert!((h - l).abs() <= 0.11);
}

#[test]
fn l_histogramme_se_relit_tel_qu_il_s_ecrit_et_refuse_une_chaine_douteuse() {
    let p = plage_et_dr_de(&sinus_par_paliers(&[(-20.0, 5.0), (-23.0, 5.0)]), HZ, 2)
        .0
        .unwrap();
    let brut = p.histogramme.encoder();
    assert_eq!(Histogramme::decoder(&brut), Some(p.histogramme.clone()));
    assert_eq!(Histogramme::decoder(""), Some(Histogramme::default()));
    assert_eq!(Histogramme::default().encoder(), "");
    // Cases creuses : les zéros intermédiaires s'écrivent et se relisent.
    let creux = Histogramme::de(&[-20.0, -20.3]);
    assert_eq!(creux.encoder(), "-203:1,0,0,1");
    assert_eq!(Histogramme::decoder("-203:1,0,0,1"), Some(creux));
    assert_eq!(Histogramme::decoder("-200:3;abc"), None);
    assert_eq!(Histogramme::decoder("-200:-3"), None);
    assert_eq!(Histogramme::decoder("abc"), None);
}

/// A1 de la relecture — une piste de moins de 3 s ou muette est MESURÉE :
/// une plage sans fenêtre (LRA nulle, histogramme vide), pas une absence qui
/// laisserait la LRA de son album vide à jamais.
#[test]
fn une_piste_de_moins_de_trois_secondes_ou_muette_a_une_plage_sans_fenetre() {
    for signal in [sinus_par_paliers(&[(-20.0, 2.5)]), vec![0.0; 2 * 10 * HZ]] {
        let p = plage_et_dr_de(&signal, HZ, 2).0.expect("plage mesurée");
        assert_eq!(p.lra_lu, None);
        assert_eq!(p.histogramme.fenetres(), 0);
        assert_eq!(p.histogramme.encoder(), "");
    }
}

// ── Coût ───────────────────────────────────────────────────────────────────

/// Le coût des sous-blocs dans l'accumulateur de sonie (pondération K, blocs
/// de 400 ms, crête vraie), sur 10 minutes de bruit rose stéréo. Le décodage,
/// qui domine la passe, n'est pas compté : la part rapportée est un MAJORANT
/// de la part de la passe. `cargo test … cout_des_sous_blocs -- --ignored
/// --nocapture`, en profil release.
#[test]
#[ignore = "banc de coût, à lancer à la main"]
fn cout_des_sous_blocs() {
    for hz in [44_100usize, 96_000] {
        let signal = bruit_rose(600.0, hz, 0.1, 17);
        let mut sans = std::time::Duration::MAX;
        let mut avec = std::time::Duration::MAX;
        for _ in 0..5 {
            sans = sans.min(duree_sonie(&signal, hz, 2, false));
            avec = avec.min(duree_sonie(&signal, hz, 2, true));
        }
        eprintln!(
            "{hz} Hz, 10 min stéréo : sans {:?}, avec {:?}, surcoût {:+.1} %",
            sans,
            avec,
            100.0 * (avec.as_secs_f64() / sans.as_secs_f64() - 1.0)
        );
    }
}

/// A3 de la relecture — la taille stockée de l'histogramme d'une piste
/// réelle, forme compacte contre l'ancienne `case:compte;…`. Les fichiers
/// (jamais versionnés) sont passés par `TUNE_PISTES_REELLES`, séparés par
/// `|`. `cargo test … taille_de_l_histogramme -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "mesure sur des fichiers locaux, à lancer à la main"]
async fn taille_de_l_histogramme_d_une_piste_reelle() {
    let Ok(liste) = std::env::var("TUNE_PISTES_REELLES") else {
        return;
    };
    for chemin in liste.split('|').filter(|c| !c.is_empty()) {
        let m = crate::audio::analyzer::mesurer_tout(chemin)
            .await
            .expect("mesure");
        let p = m.plage_de_sonie.expect("plage");
        let compacte = p.histogramme.encoder();
        let ancienne: usize = compacte
            .split_once(':')
            .map(|(premier, comptes)| {
                let premier: i32 = premier.parse().unwrap();
                comptes
                    .split(',')
                    .enumerate()
                    .filter(|(_, n)| *n != "0")
                    .map(|(i, n)| format!("{}:{n};", premier + i as i32).len())
                    .sum()
            })
            .unwrap_or(0);
        eprintln!(
            "{} : LRA {:?} LU, DR {:?}, {} fenêtres, {} cases ; compacte {} octets, ancienne {} octets",
            std::path::Path::new(chemin)
                .file_name()
                .unwrap()
                .to_string_lossy(),
            p.lra_lu,
            m.valeurs.and_then(|v| v.3),
            p.histogramme.fenetres(),
            compacte.split(',').count(),
            compacte.len(),
            ancienne
        );
    }
}
