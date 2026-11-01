//! Le banc : `banc [dossier] [--rapide]`. Écrit les CSV, les SVG et
//! `resume.md` dans le dossier (défaut `banc/`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use tune_surechantillonnage::dither::{Quantificateur, formeurs_pour, ntf_db};
use tune_surechantillonnage::mesures::{
    bilan, bruit_de_quantification, impulsion, latence, rejet_des_images, reponse_frequentielle,
    thd_n_db,
};
use tune_surechantillonnage::svg::{Serie, courbe, csv};
use tune_surechantillonnage::{Surechantillonneur, catalogue, chemin_simd, plan, tester_la_machine};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rapide = args.iter().any(|a| a == "--rapide");
    let dossier: PathBuf = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map_or_else(|| PathBuf::from("banc"), PathBuf::from);
    std::fs::create_dir_all(&dossier).expect("création du dossier du banc");
    let mut md = String::new();
    let _ = writeln!(md, "# Banc #7614 — résumé des mesures\n");
    let _ = writeln!(md, "Chemin SIMD : `{}`. Mode rapide : {rapide}.\n", chemin_simd().nom());

    filtres(&dossier, rapide, &mut md);
    dither(&dossier, rapide, &mut md);
    machine(&dossier, rapide, &mut md);

    std::fs::write(dossier.join("resume.md"), md).expect("écriture du résumé");
    println!("Banc écrit dans {}", dossier.display());
}

fn filtres(dossier: &Path, rapide: bool, md: &mut String) {
    let rapports: &[(u32, u32)] = if rapide {
        &[(44_100, 352_800)]
    } else {
        &[(44_100, 352_800), (48_000, 384_000), (44_100, 384_000)]
    };
    let _ = writeln!(md, "## Filtres\n");
    let _ = writeln!(
        md,
        "| filtre | de → vers | plan | −0,1 dB | −3 dB | −6 dB | ondulation ≤ 20 kHz | att. à Nyq. | att. ≥ Nyq. | att. ≥ 1,5 Nyq. | rejet images | THD+N 20–20k | THD+N jusqu'à vers/2 | latence théor. | pic | ½ énergie | MAC/trame | mémoire |"
    );
    let _ = writeln!(md, "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    let mut reponses_44: Vec<(String, Vec<(f64, f64)>)> = Vec::new();
    for c in catalogue() {
        for &(de, vers) in rapports {
            let p = plan(de, vers).unwrap();
            let mut sur = Surechantillonneur::nouveau(&c, de, vers, 1).unwrap();
            let nfft = if rapide { 1 << 16 } else { 1 << 18 };
            let rep = reponse_frequentielle(sur.noyau_etage1(), p.l1, de, nfft);
            let b = bilan(&rep, de);
            let base = format!("{}_{}_{}", c.id, de / 100, vers / 100);
            csv(&dossier.join(format!("reponse_{base}.csv")), "hz;db", &rep).unwrap();
            courbe(
                &dossier.join(format!("reponse_{base}.svg")),
                &format!("{} — réponse en fréquence, {de} → {vers} Hz", c.nom),
                "Hz",
                "dB",
                &[Serie { nom: c.id, points: &rep }],
                true,
                -200.0,
                5.0,
            )
            .unwrap();
            if de == 44_100 && vers == 352_800 {
                reponses_44.push((c.id.to_string(), rep.clone()));
                let imp = impulsion(sur.noyau_etage1(), p.l1, de);
                csv(&dossier.join(format!("impulsion_{}.csv", c.id)), "ms;h", &imp).unwrap();
                // Zoom ±3 ms autour du pic pour la lecture.
                let pic = imp
                    .iter()
                    .fold((0.0, 0.0), |a, &(t, v)| if v.abs() > a.1.abs() { (t, v) } else { a })
                    .0;
                let zoom: Vec<(f64, f64)> = imp
                    .iter()
                    .copied()
                    .filter(|&(t, _)| (t - pic).abs() <= 3.0)
                    .collect();
                let (ymin, ymax) = zoom
                    .iter()
                    .fold((0.0_f64, 0.0_f64), |(lo, hi), &(_, v)| (lo.min(v), hi.max(v)));
                courbe(
                    &dossier.join(format!("impulsion_{}.svg", c.id)),
                    &format!("{} — réponse impulsionnelle (±3 ms)", c.nom),
                    "ms",
                    "amplitude",
                    &[Serie { nom: c.id, points: &zoom }],
                    false,
                    ymin * 1.1,
                    ymax * 1.1,
                )
                .unwrap();
            }
            let images = rejet_des_images(&mut Surechantillonneur::nouveau(&c, de, vers, 1).unwrap());
            csv(&dossier.join(format!("balayage_{base}.csv")), "hz;db", &images.spectre).unwrap();
            let thd_bande = thd_n_db(
                &mut Surechantillonneur::nouveau(&c, de, vers, 1).unwrap(),
                1000.0,
                0.891,
                20.0,
                20_000.0,
                None,
            );
            let thd_large = thd_n_db(
                &mut Surechantillonneur::nouveau(&c, de, vers, 1).unwrap(),
                1000.0,
                0.891,
                20.0,
                vers as f64 / 2.0,
                None,
            );
            let lat = latence(&mut sur);
            let plan_txt = if p.deux_etages() {
                format!("×{} puis {}/{}", p.l1, p.l2, p.m2)
            } else {
                format!("×{}", p.l1)
            };
            let _ = writeln!(
                md,
                "| {} | {de} → {vers} | {plan_txt} | {:.0} Hz | {:.0} Hz | {:.0} Hz | {:.5} dB | {:.1} dB | {:.1} dB | {:.1} dB | {:.1} dB | {:.1} dB | {:.1} dB | {:.2} ms | {:.2} ms | {:.2} ms | {:.0} | {:.1} Mio |",
                c.id,
                b.f_moins_0_1_db,
                b.f_moins_3_db,
                b.f_moins_6_db,
                b.ondulation_db,
                b.attenuation_a_nyquist_db,
                b.attenuation_au_dela_nyquist_db,
                b.attenuation_au_dela_1_5_nyquist_db,
                images.rejet_db,
                thd_bande,
                thd_large,
                lat.theorique_ms,
                lat.pic_ms,
                lat.moitie_energie_ms,
                sur.mac_par_trame_de_sortie(),
                sur.memoire_noyaux_octets() as f64 / 1_048_576.0,
            );
            eprintln!("filtre {} {de}→{vers} : fait", c.id);
        }
    }
    let series: Vec<Serie<'_>> = reponses_44
        .iter()
        .map(|(id, pts)| Serie { nom: id, points: pts })
        .collect();
    courbe(
        &dossier.join("reponses_comparees_441_3528.svg"),
        "Toutes les conceptions — 44,1 → 352,8 kHz",
        "Hz",
        "dB",
        &series,
        true,
        -200.0,
        5.0,
    )
    .unwrap();
    // Zoom sur la bande de transition.
    let zoom: Vec<(String, Vec<(f64, f64)>)> = reponses_44
        .iter()
        .map(|(id, pts)| {
            (
                id.clone(),
                pts.iter().copied().filter(|p| p.0 >= 10_000.0 && p.0 <= 40_000.0).collect(),
            )
        })
        .collect();
    let series: Vec<Serie<'_>> = zoom.iter().map(|(id, pts)| Serie { nom: id, points: pts }).collect();
    courbe(
        &dossier.join("transition_comparee_441_3528.svg"),
        "Bande de transition, 10–40 kHz — 44,1 → 352,8 kHz",
        "Hz",
        "dB",
        &series,
        false,
        -160.0,
        2.0,
    )
    .unwrap();
    md.push('\n');
}

fn dither(dossier: &Path, rapide: bool, md: &mut String) {
    let _ = writeln!(md, "## Dither et mise en forme du bruit (16 bits, sinus 1 kHz à −20 dBFS)\n");
    let _ = writeln!(
        md,
        "| cadence | formeur | ordre | gain total théorique | bruit total | bruit 20 Hz–20 kHz | pondéré ATH | référence |"
    );
    let _ = writeln!(md, "|---|---|---|---|---|---|---|---|");
    let duree = if rapide { 2.0 } else { 6.0 };
    for &cadence in &[44_100u32, 352_800] {
        let mut series_ntf: Vec<(String, Vec<(f64, f64)>)> = Vec::new();
        let mut series_bruit: Vec<(String, Vec<(f64, f64)>)> = Vec::new();
        for bits in [16u32, 24] {
            let mut q = Quantificateur::nouveau(bits, true, None, 1, 1);
            let b = bruit_de_quantification(cadence, &mut q, duree);
            let _ = writeln!(
                md,
                "| {cadence} | TPDF seul, {bits} bits | 0 | 0,0 dB | {:.1} dBFS | {:.1} dBFS | {:.1} dB | Lipshitz et al. 1991 |",
                b.total_dbfs, b.bande_audible_dbfs, b.pondere_ath_db
            );
            if bits == 16 {
                series_bruit.push(("tpdf".into(), b.spectre.clone()));
            }
        }
        for f in formeurs_pour(cadence) {
            let mut q = Quantificateur::nouveau(16, true, Some(&f), 1, 1);
            let b = bruit_de_quantification(cadence, &mut q, duree);
            let _ = writeln!(
                md,
                "| {cadence} | {} | {} | {:.1} dB | {:.1} dBFS | {:.1} dBFS | {:.1} dB | {} |",
                f.nom,
                f.ordre(),
                f.gain_de_bruit_total_db(),
                b.total_dbfs,
                b.bande_audible_dbfs,
                b.pondere_ath_db,
                f.reference
            );
            let ntf = ntf_db(&f.coefficients, cadence, 1024);
            csv(&dossier.join(format!("ntf_{}_{}.csv", f.id, cadence / 100)), "hz;db", &ntf).unwrap();
            csv(&dossier.join(format!("bruit_{}_{}.csv", f.id, cadence / 100)), "hz;db", &b.spectre).unwrap();
            series_ntf.push((f.id.to_string(), ntf));
            series_bruit.push((f.id.to_string(), b.spectre));
        }
        let s: Vec<Serie<'_>> = series_ntf.iter().map(|(id, p)| Serie { nom: id, points: p }).collect();
        courbe(
            &dossier.join(format!("ntf_{}.svg", cadence / 100)),
            &format!("Fonctions de transfert du bruit à {cadence} Hz"),
            "Hz",
            "dB",
            &s,
            true,
            -40.0,
            40.0,
        )
        .unwrap();
        let s: Vec<Serie<'_>> = series_bruit.iter().map(|(id, p)| Serie { nom: id, points: p }).collect();
        courbe(
            &dossier.join(format!("bruit_{}.svg", cadence / 100)),
            &format!("Spectre de l'erreur de quantification 16 bits à {cadence} Hz (Welch 8192)"),
            "Hz",
            "dB par bin",
            &s,
            true,
            -180.0,
            -60.0,
        )
        .unwrap();
        eprintln!("dither {cadence} : fait");
    }
    md.push('\n');
}

fn machine(dossier: &Path, rapide: bool, md: &mut String) {
    let rapport = tester_la_machine(2, if rapide { 0.2 } else { 0.5 }, 3.0);
    let _ = writeln!(
        md,
        "## Test de la machine (stéréo, marge ×{}, chemin `{}`)\n",
        rapport.marge, rapport.chemin_simd
    );
    let _ = writeln!(md, "| filtre | de → vers | × temps réel | GMAC/s | mémoire noyaux | admis |");
    let _ = writeln!(md, "|---|---|---|---|---|---|");
    let mut c = String::from("filtre;de;vers;facteur_temps_reel;gmac_s;memoire_octets;admis\n");
    for r in &rapport.resultats {
        let _ = writeln!(
            md,
            "| {} | {} → {} | {:.1} | {:.2} | {:.1} Mio | {} |",
            r.filtre,
            r.de,
            r.vers,
            r.facteur_temps_reel,
            r.gmac_s,
            r.memoire_noyaux_octets as f64 / 1_048_576.0,
            if r.admis { "oui" } else { "NON" }
        );
        let _ = writeln!(
            c,
            "{};{};{};{:.3};{:.3};{};{}",
            r.filtre, r.de, r.vers, r.facteur_temps_reel, r.gmac_s, r.memoire_noyaux_octets, r.admis
        );
    }
    std::fs::write(dossier.join("machine.csv"), c).unwrap();
    eprintln!("machine : fait");
}
