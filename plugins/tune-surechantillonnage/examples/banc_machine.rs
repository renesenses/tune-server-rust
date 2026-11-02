//! Banc #7614 sur machine réelle : débit par cœur et par chemin SIMD,
//! facteur temps réel et latence du catalogue à ×8 et ×16, et les mesures
//! demandées par la note de conception (Q7) : délai de groupe en fonction de
//! la fréquence, pré-écho fenêtré, bruit ultrasonore à 16 et 24 bits, gain
//! unitaire par phase, réponse mesurée contre réponse de conception.
//!
//! N'ajoute AUCUNE fonction à la crate : ce pilote compose son API publique
//! (`Surechantillonneur`, `tester_la_machine_sur`, `produit_scalaire`,
//! `ps_scalaire`, `latence`, `gain_sinus_db`, `reponse_frequentielle`,
//! `bruit_de_quantification`) ; les seuls calculs ajoutés ici sont des
//! lectures de la réponse impulsionnelle (délai de groupe par la formule
//! Re{DFT(n·h)/DFT(h)}, énergie avant le pic).
//!
//! Usage : `cargo run --release --example banc_machine -- <dossier> [--debit]`.
//! Aucun son : tout reste en mémoire, rien n'est envoyé à une sortie.

use std::fmt::Write as _;
use std::time::Instant;

use tune_surechantillonnage::conception::{Conception, Phase, par_id};
use tune_surechantillonnage::dither::{Prng, Quantificateur, formeurs_pour};
use tune_surechantillonnage::mesures::{
    bruit_de_quantification, gain_sinus_db, latence, niveau_a, reponse_frequentielle,
};
use tune_surechantillonnage::polyphase::{chemin_simd, produit_scalaire, ps_scalaire};
use tune_surechantillonnage::{Surechantillonneur, tester_la_machine_sur};

type Ps = fn(&[f64], &[f64]) -> f64;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--debit` : seulement la section 1 (pour la répéter à charge basse).
    let debit_seul = args.iter().any(|a| a == "--debit");
    let dossier = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "banc-machine".into());
    std::fs::create_dir_all(&dossier).expect("dossier");
    let fr = |x: f64, d: usize| format!("{x:.d$}").replace('.', ",");
    let mut md = String::new();
    let _ = writeln!(md, "Chemin SIMD détecté : `{}`.\n", chemin_simd().nom());

    // Catalogue de la note : 48, 256, 1 024, 4 096, en phase linéaire et
    // minimale. La crate n'a pas de « precision-longue-pm » : on l'obtient
    // en changeant le champ `phase` (la transformation est celle du
    // catalogue, `Conception::noyau`).
    let mut conceptions: Vec<Conception> = Vec::new();
    for id in ["resolution", "polyvalent", "precision", "precision-longue"] {
        let c = par_id(id).unwrap();
        let pm = if id == "precision-longue" {
            Conception {
                id: "precision-longue-pm",
                nom: "Précision longue, phase minimale",
                phase: Phase::Minimale,
                ..c.clone()
            }
        } else {
            par_id(&format!("{id}-pm")).unwrap()
        };
        conceptions.push(c);
        conceptions.push(pm);
    }
    let phase_txt = |c: &Conception| if c.phase == Phase::Lineaire { "linéaire" } else { "minimale" };

    // ── 1. Débit du produit scalaire polyphase, par chemin, par portée ──
    // Même accès mémoire que `Etage::traiter` : `l` phases de `portée`
    // coefficients, historique doublé, stéréo entrelacée.
    let mesure_noyau = |f: Ps, portee: usize, l: usize, duree: f64| -> f64 {
        let canaux = 2;
        let mut prng = Prng::nouveau(portee as u64 ^ l as u64);
        let coefs: Vec<f64> = (0..l * portee).map(|_| prng.tpdf()).collect();
        let entree: Vec<f64> = (0..4096 * canaux).map(|_| prng.tpdf() * 0.5).collect();
        let mut hist = vec![0.0; canaux * 2 * portee];
        let mut acc = 0.0;
        let mut w = 0usize;
        let mut passes = Vec::new();
        for _ in 0..3 {
            let debut = Instant::now();
            let mut mac = 0u64;
            while debut.elapsed().as_secs_f64() < duree {
                for trame in entree.chunks_exact(canaux) {
                    for (ch, &x) in trame.iter().enumerate() {
                        let b = ch * 2 * portee;
                        hist[b + w] = x;
                        hist[b + w + portee] = x;
                    }
                    w += 1;
                    if w == portee {
                        w = 0;
                    }
                    for ph in 0..l {
                        let c = &coefs[ph * portee..ph * portee + portee];
                        for ch in 0..canaux {
                            let b = ch * 2 * portee;
                            acc += f(c, &hist[b + w..b + w + portee]);
                        }
                    }
                }
                mac += (4096 * l * canaux * portee) as u64;
            }
            passes.push(mac as f64 / debut.elapsed().as_secs_f64() / 1e9);
        }
        std::hint::black_box(acc);
        passes.sort_by(|a, b| a.partial_cmp(b).unwrap());
        passes[1] // médiane de trois passes
    };
    let chemins: [(&str, Ps); 2] = [("avx2+fma", produit_scalaire), ("scalaire", ps_scalaire)];
    let portees = [48usize, 256, 1024, 2048, 4096];
    let _ = writeln!(md, "## 1. Débit du noyau polyphase, f64, 1 fil (GMAC/s, médiane de 3 × 2 s)\n");
    let _ = writeln!(md, "| chemin | L | 48 | 256 | 1 024 | 2 048 | 4 096 | coefficients 4 096 × L |");
    let _ = writeln!(md, "|---|---|---|---|---|---|---|---|");
    let mut csv1 = String::from("chemin;l;portee;gmac_s\n");
    for (nom, f) in chemins {
        for l in [8usize, 16] {
            let mut ligne = format!("| {nom} | ×{l} |");
            for &p in &portees {
                let g = mesure_noyau(f, p, l, 2.0);
                let _ = write!(ligne, " {} |", fr(g, 2));
                let _ = writeln!(csv1, "{nom};{l};{p};{g:.4}");
                eprintln!("noyau {nom} ×{l} portée {p} : {g:.3} GMAC/s");
            }
            let _ = writeln!(md, "{ligne} {} Kio |", 4096 * l * 8 / 1024);
        }
    }
    std::fs::write(format!("{dossier}/noyau.csv"), csv1).unwrap();

    // 1 bis. Huit fils simultanés (débit par fil sous charge, fréquence
    // tous cœurs), chemin AVX2.
    let _ = writeln!(md, "\n### 1 bis. Huit fils simultanés, avx2+fma (GMAC/s par fil, moyenne ; min–max)\n");
    let _ = writeln!(md, "| portée | L | par fil | min – max | total 8 fils |");
    let _ = writeln!(md, "|---|---|---|---|---|");
    for (p, l) in [(256usize, 8usize), (1024, 8), (1024, 16), (4096, 8), (4096, 16)] {
        let r: Vec<f64> = std::thread::scope(|s| {
            let h: Vec<_> = (0..8)
                .map(|_| s.spawn(move || mesure_noyau(produit_scalaire, p, l, 2.0)))
                .collect();
            h.into_iter().map(|j| j.join().unwrap()).collect()
        });
        let moy = r.iter().sum::<f64>() / 8.0;
        let (mi, ma) = r.iter().fold((f64::MAX, 0.0_f64), |(a, b), &v| (a.min(v), b.max(v)));
        let _ = writeln!(
            md,
            "| {p} | ×{l} | {} | {} – {} | {} |",
            fr(moy, 2),
            fr(mi, 2),
            fr(ma, 2),
            fr(moy * 8.0, 1)
        );
        eprintln!("8 fils portée {p} ×{l} : {moy:.3} GMAC/s par fil");
    }

    if debit_seul {
        std::fs::write(format!("{dossier}/resultats.md"), &md).unwrap();
        println!("{md}");
        return;
    }

    // ── 2. Facteur temps réel et latence du catalogue ──
    let rapports = [(44_100u32, 352_800u32), (44_100, 705_600), (48_000, 384_000), (48_000, 768_000)];
    let rapport = tester_la_machine_sur(&conceptions, &rapports, 2, 1.0, 3.0);
    let _ = writeln!(
        md,
        "\n## 2. Interpolateur complet (`Surechantillonneur`, stéréo, chemin `{}`, 1 fil)\n",
        rapport.chemin_simd
    );
    let _ = writeln!(
        md,
        "| conception | portée | phase | de → vers | × temps réel | GMAC/s | mémoire noyau | latence théor. (lin.) | pic | ½ énergie |"
    );
    let _ = writeln!(md, "|---|---|---|---|---|---|---|---|---|---|");
    let mut csv2 = String::from("conception;portee;phase;de;vers;facteur_tr;gmac_s;memoire;lat_theo_ms;pic_ms;moitie_ms\n");
    for c in &conceptions {
        for &(de, vers) in &rapports {
            let r = rapport
                .resultats
                .iter()
                .find(|r| r.filtre == c.id && r.de == de && r.vers == vers)
                .unwrap();
            let mut sur = Surechantillonneur::nouveau(c, de, vers, 1).unwrap();
            let lat = latence(&mut sur);
            let _ = writeln!(
                md,
                "| {} | {} | {} | {de} → {vers} | {} | {} | {} Kio | {} ms | {} ms | {} ms |",
                c.id,
                c.portee,
                phase_txt(c),
                fr(r.facteur_temps_reel, 1),
                fr(r.gmac_s, 2),
                r.memoire_noyaux_octets / 1024,
                fr(lat.theorique_ms, 3),
                fr(lat.pic_ms, 3),
                fr(lat.moitie_energie_ms, 3),
            );
            let _ = writeln!(
                csv2,
                "{};{};{};{de};{vers};{:.3};{:.4};{};{:.4};{:.4};{:.4}",
                c.id,
                c.portee,
                phase_txt(c),
                r.facteur_temps_reel,
                r.gmac_s,
                r.memoire_noyaux_octets,
                lat.theorique_ms,
                lat.pic_ms,
                lat.moitie_energie_ms
            );
        }
        eprintln!("catalogue {} : fait", c.id);
    }
    std::fs::write(format!("{dossier}/catalogue.csv"), csv2).unwrap();

    // ── 3. Mesures Q7 sur le noyau et sur la sortie ──
    let points_dg: Vec<f64> = (0..121).map(|i| 20.0 * 1000f64.powf(i as f64 / 120.0)).collect();
    let _ = writeln!(md, "\n## 3. Mesures Q7 (44,1 kHz → ×8 et ×16)\n");
    let mut t_dg = String::from(
        "### 3a. Délai de groupe (ms) et excursion 20 Hz – 20 kHz\n\n| conception | phase | ×L | 20 Hz | 1 kHz | 5 kHz | 10 kHz | 15 kHz | 20 kHz | excursion 20 Hz–20 kHz |\n|---|---|---|---|---|---|---|---|---|---|\n",
    );
    let mut t_pe = String::from(
        "### 3b. Pré-écho (réponse impulsionnelle de l'étage 1)\n\nFenêtre : tout ce qui précède le pic de plus de 0,1 ms (resp. 1 ms). Énergie en dB sous l'énergie totale ; crête en dB sous le pic. Étendue : avance sur le pic du premier coefficient à plus de −120 dB du pic.\n\n| conception | phase | ×L | énergie > 0,1 ms avant | crête > 0,1 ms avant | énergie > 1 ms avant | crête > 1 ms avant | étendue à −120 dB |\n|---|---|---|---|---|---|---|---|\n",
    );
    let mut t_gu = String::from(
        "### 3c. Gain unitaire par phase (entrée continue plate à 1e-7, régime établi)\n\nÉcart maximal du gain d'une phase au gain unité, en ppm et en dB ; l'écart entre phases module le continu à la cadence d'entrée (image à k·Fs).\n\n| conception | phase | ×L | écart max (ppm) | écart max (dB) | phase la plus éloignée |\n|---|---|---|---|---|---|\n",
    );
    let mut t_rv = String::from(
        "### 3d. Réponse mesurée (sinus à travers l'interpolateur, `gain_sinus_db`) contre réponse de conception (spectre du noyau)\n\n| conception | phase | ×L | mesuré 1 kHz | conçu 1 kHz | mesuré 20 kHz | conçu 20 kHz | mesuré 21 kHz | conçu 21 kHz | écart max (1, 5, 10, 15, 19, 20, 21 kHz) |\n|---|---|---|---|---|---|---|---|---|---|\n",
    );
    let mut csv_dg = String::from("conception;l;hz;delai_ms\n");
    for c in &conceptions {
        for &(de, vers) in &[(44_100u32, 352_800u32), (44_100, 705_600)] {
            let l = (vers / de) as usize;
            let fs = (de as usize * l) as f64;
            let mut sur = Surechantillonneur::nouveau(c, de, vers, 1).unwrap();
            let h: Vec<f64> = sur.noyau_etage1().to_vec();

            // 3a. Délai de groupe τ(ω) = Re{ Σ n·h[n]e^{−jωn} / Σ h[n]e^{−jωn} }.
            let delai = |f: f64| -> f64 {
                let w = 2.0 * std::f64::consts::PI * f / fs;
                let (mut nr, mut ni, mut dr, mut di) = (0.0, 0.0, 0.0, 0.0);
                for (n, &v) in h.iter().enumerate() {
                    let (s, co) = (w * n as f64).sin_cos();
                    dr += v * co;
                    di -= v * s;
                    nr += n as f64 * v * co;
                    ni -= n as f64 * v * s;
                }
                (nr * dr + ni * di) / (dr * dr + di * di) / fs * 1000.0
            };
            let courbe: Vec<f64> = points_dg.iter().map(|&f| delai(f)).collect();
            for (f, d) in points_dg.iter().zip(&courbe) {
                let _ = writeln!(csv_dg, "{};{l};{f:.2};{d:.6}", c.id);
            }
            let (mi, ma) = courbe.iter().fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
            let _ = writeln!(
                t_dg,
                "| {} | {} | ×{l} | {} | {} | {} | {} | {} | {} | {} µs |",
                c.id,
                phase_txt(c),
                fr(courbe[0], 4),
                fr(delai(1000.0), 4),
                fr(delai(5000.0), 4),
                fr(delai(10_000.0), 4),
                fr(delai(15_000.0), 4),
                fr(delai(20_000.0), 4),
                fr((ma - mi) * 1000.0, 1),
            );

            // 3b. Pré-écho.
            let (pic, hp) = h
                .iter()
                .enumerate()
                .fold((0usize, 0.0_f64), |a, (i, &v)| if v.abs() > a.1 { (i, v.abs()) } else { a });
            let e_tot: f64 = h.iter().map(|v| v * v).sum();
            let avant = |ms: f64| -> (f64, f64) {
                let k = pic as f64 - ms * fs / 1000.0;
                if k <= 0.0 {
                    return (f64::NEG_INFINITY, f64::NEG_INFINITY);
                }
                let k = k as usize;
                let e: f64 = h[..k].iter().map(|v| v * v).sum();
                let cr = h[..k].iter().fold(0.0_f64, |m, v| m.max(v.abs()));
                (
                    10.0 * (e / e_tot).max(1e-30).log10(),
                    20.0 * (cr / hp).max(1e-15).log10(),
                )
            };
            let (e01, c01) = avant(0.1);
            let (e1, c1) = avant(1.0);
            let premier = h.iter().position(|v| v.abs() > hp * 1e-6).unwrap_or(pic);
            let db = |x: f64| if x.is_finite() { format!("{} dB", fr(x, 1)) } else { "—".into() };
            let _ = writeln!(
                t_pe,
                "| {} | {} | ×{l} | {} | {} | {} | {} | {} ms |",
                c.id,
                phase_txt(c),
                db(e01),
                db(c01),
                db(e1),
                db(c1),
                fr((pic - premier) as f64 / fs * 1000.0, 3),
            );

            // 3c. Gain unitaire par phase : continu plat à 1e-7.
            let n_in = 2 * c.portee + 512;
            let mut sortie = Vec::new();
            sur.traiter(&vec![1e-7; n_in], &mut sortie);
            let debut = (c.portee + 16) * l;
            let mut pire = (0usize, 0.0_f64);
            for p in 0..l {
                for (k, &y) in sortie.iter().enumerate().skip(debut) {
                    if k % l == p {
                        let e = y / 1e-7 - 1.0;
                        if e.abs() > pire.1.abs() {
                            pire = (p, e);
                        }
                    }
                }
            }
            let _ = writeln!(
                t_gu,
                "| {} | {} | ×{l} | {} | {} | {} |",
                c.id,
                phase_txt(c),
                fr(pire.1 * 1e6, 3),
                format!("{:.2e}", 20.0 * (1.0 + pire.1).log10()).replace('.', ","),
                pire.0
            );

            // 3d. Mesuré contre conçu (l'état est remplacé : gain_sinus_db
            // saute 2 × portée trames, plus que l'historique).
            let rep = reponse_frequentielle(&h, l, de, 1 << 18);
            let mut ecart_max = 0.0_f64;
            let mut val = |f: f64| -> (f64, f64) {
                let m = gain_sinus_db(&mut sur, f);
                let d = niveau_a(&rep, f);
                ecart_max = ecart_max.max((m - d).abs());
                (m, d)
            };
            let (m1, d1) = val(1000.0);
            let _ = val(5000.0);
            let _ = val(10_000.0);
            let _ = val(15_000.0);
            let _ = val(19_000.0);
            let (m20, d20) = val(20_000.0);
            let (m21, d21) = val(21_000.0);
            let _ = writeln!(
                t_rv,
                "| {} | {} | ×{l} | {} | {} | {} | {} | {} | {} | {} dB |",
                c.id,
                phase_txt(c),
                fr(m1, 4),
                fr(d1, 4),
                fr(m20, 4),
                fr(d20, 4),
                fr(m21, 3),
                fr(d21, 3),
                format!("{ecart_max:.1e}").replace('.', ","),
            );
            eprintln!("Q7 {} ×{l} : fait", c.id);
        }
    }
    std::fs::write(format!("{dossier}/delai_de_groupe.csv"), csv_dg).unwrap();
    for t in [&t_dg, &t_pe, &t_gu, &t_rv] {
        md.push_str(t);
        md.push('\n');
    }

    // 3e. Bruit ultrasonore intégré 20 kHz – Fs/2, 16 et 24 bits, par
    // formeur et par cadence (sinus 1 kHz à −20 dBFS, 2 s ; puissance
    // rapportée à 1, un sinus pleine échelle vaut −3,01 dB).
    let _ = writeln!(
        md,
        "### 3e. Bruit de quantification : 20 Hz–20 kHz et ultrasonore 20 kHz–Fs/2 (dBFS, 16 / 24 bits)\n\nPlafond de la note (Q3) : −60 dBFS ultrasonore.\n\n| cadence | formeur | ordre | 16 b audible | 16 b ultrason | 24 b audible | 24 b ultrason |\n|---|---|---|---|---|---|---|"
    );
    let mut csv3 = String::from("cadence;formeur;bits;total_dbfs;audible_dbfs;ultrason_dbfs\n");
    for cadence in [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000] {
        let mut liste: Vec<(String, usize, Option<_>)> = vec![("TPDF seul".to_string(), 0, None)];
        for f in formeurs_pour(cadence) {
            liste.push((f.id.to_string(), f.ordre(), Some(f)));
        }
        for (nom, ordre, f) in &liste {
            let mut cells = Vec::new();
            for bits in [16u32, 24] {
                let mut q = Quantificateur::nouveau(bits, true, f.as_ref(), 1, 0x7614);
                let b = bruit_de_quantification(cadence, &mut q, 2.0);
                let ultra: f64 = b
                    .spectre
                    .iter()
                    .filter(|p| p.0 > 20_000.0)
                    .map(|p| 10f64.powf(p.1 / 10.0))
                    .sum();
                let u = 10.0 * ultra.max(1e-40).log10();
                let _ = writeln!(
                    csv3,
                    "{cadence};{nom};{bits};{:.3};{:.3};{u:.3}",
                    b.total_dbfs, b.bande_audible_dbfs
                );
                cells.push((b.bande_audible_dbfs, u));
            }
            let _ = writeln!(
                md,
                "| {cadence} | {nom} | {ordre} | {} | {} | {} | {} |",
                fr(cells[0].0, 1),
                fr(cells[0].1, 1),
                fr(cells[1].0, 1),
                fr(cells[1].1, 1)
            );
        }
        eprintln!("bruit {cadence} : fait");
    }
    std::fs::write(format!("{dossier}/bruit.csv"), csv3).unwrap();

    std::fs::write(format!("{dossier}/resultats.md"), &md).unwrap();
    println!("{md}");
}
