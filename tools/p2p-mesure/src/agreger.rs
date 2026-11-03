//! `p2p-mesure agreger <fichiers…>` (cahier § 3.3).
//!
//! Un tableau par croisement réseau du client × opérateur × type de box,
//! plus une ligne « toutes » ; sortie Markdown et CSV.
//!
//! Essais retenus : ceux qui ont un `chemin_final` (poignée de main acceptée).
//! Les autres sont comptés à part, en `erreurs`.
//!
//! **Taux direct pondéré par le volume.** Le journal du § 3.2 ne porte pas le
//! temps passé sur chaque chemin ; on pondère donc chaque essai par son volume
//! et on donne deux bornes :
//! - volume d'un essai : somme des `debit_moyen_mbps × duree_s` de ses flux ;
//!   un essai sans flux (essai court) vaut une minute de 16/44 cadencé
//!   ([`VOLUME_ESSAI_COURT_MBIT`]) ;
//! - borne basse : seuls les essais `direct` comptent comme direct ;
//!   borne haute : les essais `mixte` comptent aussi.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::{Context, Result};

use crate::journal::Essai;
use crate::protocole::FORMATS;
use crate::stats::{mediane, quantile, wilson};

/// 1,0 Mbit/s (16/44 cadencé) pendant 60 s.
pub const VOLUME_ESSAI_COURT_MBIT: f64 = 60.0;

const CHEMINS: [&str; 3] = ["direct", "relais", "mixte"];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cellule {
    pub reseau: String,
    pub operateur: String,
    pub box_: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ligne {
    pub essais: u64,
    pub erreurs: u64,
    /// Essais retenus (avec un `chemin_final`).
    pub n: u64,
    pub direct: u64,
    pub mixte: u64,
    pub taux_direct: Option<f64>,
    pub ic95: Option<(f64, f64)>,
    pub t_direct_p50: Option<f64>,
    pub t_direct_p95: Option<f64>,
    pub part_direct_5s: Option<f64>,
    pub debit_max_median: Option<f64>,
    pub rtt_relais_p50: Option<f64>,
    pub rtt_direct_p50: Option<f64>,
    pub gigue_mediane: Option<f64>,
    pub pondere_bas: Option<f64>,
    pub pondere_haut: Option<f64>,
    /// (format, chemin) → (flux, flux sans vidage)
    pub sans_vidage: BTreeMap<(String, String), (u64, u64)>,
}

pub fn volume_mbit(e: &Essai) -> f64 {
    if e.flux.is_empty() {
        VOLUME_ESSAI_COURT_MBIT
    } else {
        e.flux
            .iter()
            .map(|f| f.debit_moyen_mbps.max(0.0) * f.duree_s as f64)
            .sum()
    }
}

pub fn calculer(essais: &[&Essai]) -> Ligne {
    let mut l = Ligne {
        essais: essais.len() as u64,
        erreurs: essais.iter().filter(|e| e.erreur.is_some()).count() as u64,
        ..Ligne::default()
    };
    let retenus: Vec<&Essai> = essais
        .iter()
        .copied()
        .filter(|e| e.chemin_final.is_some())
        .collect();
    let est = |e: &Essai, c: &str| e.chemin_final.as_deref() == Some(c);
    l.n = retenus.len() as u64;
    l.direct = retenus.iter().filter(|e| est(e, "direct")).count() as u64;
    l.mixte = retenus.iter().filter(|e| est(e, "mixte")).count() as u64;
    if l.n > 0 {
        l.taux_direct = Some(l.direct as f64 / l.n as f64);
        l.ic95 = wilson(l.direct, l.n);
    }

    let t_direct: Vec<f64> = retenus
        .iter()
        .filter_map(|e| e.t_direct_ms.map(|t| t as f64))
        .collect();
    l.t_direct_p50 = quantile(&t_direct, 0.5);
    l.t_direct_p95 = quantile(&t_direct, 0.95);
    if l.n > 0 {
        let vite = retenus
            .iter()
            .filter(|e| e.t_direct_ms.is_some_and(|t| t < 5000))
            .count();
        l.part_direct_5s = Some(vite as f64 / l.n as f64);
    }

    let debits: Vec<f64> = retenus.iter().filter_map(|e| e.debit_max_mbps).collect();
    l.debit_max_median = mediane(&debits);

    let rtt = |f: &dyn Fn(&crate::journal::Rtt) -> Option<f64>| -> Option<f64> {
        let v: Vec<f64> = retenus
            .iter()
            .filter_map(|e| e.rtt.as_ref().and_then(f))
            .collect();
        mediane(&v)
    };
    l.rtt_relais_p50 = rtt(&|r| r.relais.as_ref().map(|q| q.p50));
    l.rtt_direct_p50 = rtt(&|r| r.direct.as_ref().map(|q| q.p50));
    l.gigue_mediane = rtt(&|r| r.gigue_ms);

    let total: f64 = retenus.iter().map(|e| volume_mbit(e)).sum();
    if total > 0.0 {
        let part = |chemins: &[&str]| -> f64 {
            retenus
                .iter()
                .filter(|e| chemins.iter().any(|c| est(e, c)))
                .map(|e| volume_mbit(e))
                .sum::<f64>()
                / total
        };
        l.pondere_bas = Some(part(&["direct"]));
        l.pondere_haut = Some(part(&["direct", "mixte"]));
    }

    for e in &retenus {
        let chemin = e.chemin_final.clone().unwrap_or_default();
        for f in &e.flux {
            let case = l
                .sans_vidage
                .entry((f.format.clone(), chemin.clone()))
                .or_default();
            case.0 += 1;
            if f.sous_debits == 0 {
                case.1 += 1;
            }
        }
    }
    l
}

pub fn cellule(e: &Essai) -> Cellule {
    Cellule {
        reseau: e.client.reseau.clone(),
        operateur: e.client.operateur.clone(),
        box_: e
            .serveur
            .as_ref()
            .map(|s| s.box_.clone())
            .unwrap_or_else(|| "?".into()),
    }
}

pub fn par_cellule(essais: &[Essai]) -> Vec<(Cellule, Ligne)> {
    let mut groupes: BTreeMap<Cellule, Vec<&Essai>> = BTreeMap::new();
    for e in essais {
        groupes.entry(cellule(e)).or_default().push(e);
    }
    let mut sortie: Vec<(Cellule, Ligne)> = groupes
        .into_iter()
        .map(|(c, v)| (c, calculer(&v)))
        .collect();
    let tous: Vec<&Essai> = essais.iter().collect();
    sortie.push((
        Cellule {
            reseau: "toutes".into(),
            operateur: "toutes".into(),
            box_: "toutes".into(),
        },
        calculer(&tous),
    ));
    sortie
}

/// Lit des fichiers JSONL ; rend les essais et le nombre de lignes illisibles.
pub fn lire(textes: &[String]) -> (Vec<Essai>, usize) {
    let mut essais = Vec::new();
    let mut illisibles = 0;
    for texte in textes {
        for ligne in texte.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<Essai>(ligne) {
                Ok(e) => essais.push(e),
                Err(_) => illisibles += 1,
            }
        }
    }
    (essais, illisibles)
}

pub fn lire_fichiers(chemins: &[std::path::PathBuf]) -> Result<(Vec<Essai>, usize)> {
    let mut textes = Vec::new();
    for c in chemins {
        textes.push(
            std::fs::read_to_string(c).with_context(|| format!("lecture de {}", c.display()))?,
        );
    }
    Ok(lire(&textes))
}

fn pct(x: Option<f64>) -> String {
    x.map(|v| format!("{:.0} %", v * 100.0))
        .unwrap_or_else(|| "–".into())
}

fn nombre(x: Option<f64>, decimales: usize) -> String {
    x.map(|v| format!("{v:.decimales$}"))
        .unwrap_or_else(|| "–".into())
}

pub fn markdown(lignes: &[(Cellule, Ligne)], illisibles: usize) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "| Réseau | Opérateur | Box | Essais | Retenus | Direct | Taux direct [IC 95 %] | Mixte | t_direct p50 / p95 (ms) | Direct < 5 s | Débit max médian (Mbit/s) | RTT p50 relais / direct (ms) | Gigue médiane (ms) | Taux pondéré volume (bas – haut) | Erreurs |"
    );
    let _ = writeln!(
        s,
        "|---|---|---|---:|---:|---:|---|---:|---|---:|---:|---|---:|---|---:|"
    );
    for (c, l) in lignes {
        let taux = match (l.taux_direct, l.ic95) {
            (Some(t), Some((b, h))) => {
                format!("{:.0} % [{:.0} ; {:.0}]", t * 100.0, b * 100.0, h * 100.0)
            }
            _ => "–".into(),
        };
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} / {} | {} | {} | {} / {} | {} | {} – {} | {} |",
            c.reseau,
            c.operateur,
            c.box_,
            l.essais,
            l.n,
            l.direct,
            taux,
            l.mixte,
            nombre(l.t_direct_p50, 0),
            nombre(l.t_direct_p95, 0),
            pct(l.part_direct_5s),
            nombre(l.debit_max_median, 1),
            nombre(l.rtt_relais_p50, 1),
            nombre(l.rtt_direct_p50, 1),
            nombre(l.gigue_mediane, 1),
            pct(l.pondere_bas),
            pct(l.pondere_haut),
            l.erreurs,
        );
    }
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "Flux cadencés sans vidage du tampon de 2 s, par format et par chemin :"
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "| Réseau | Opérateur | Box | Format | Chemin | Flux | Sans vidage |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---:|---:|");
    for (c, l) in lignes {
        for ((format, chemin), (n, ok)) in &l.sans_vidage {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {} | {} |",
                c.reseau,
                c.operateur,
                c.box_,
                format,
                chemin,
                n,
                pct(Some(*ok as f64 / *n as f64))
            );
        }
    }
    if illisibles > 0 {
        let _ = writeln!(s);
        let _ = writeln!(s, "{illisibles} ligne(s) illisible(s) ignorée(s).");
    }
    s
}

fn champ_csv(v: &str) -> String {
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

fn opt(x: Option<f64>) -> String {
    x.map(|v| format!("{v:.4}")).unwrap_or_default()
}

pub fn csv(lignes: &[(Cellule, Ligne)]) -> String {
    let mut entete: Vec<String> = [
        "reseau",
        "operateur",
        "box",
        "essais",
        "retenus",
        "direct",
        "mixte",
        "taux_direct",
        "ic95_bas",
        "ic95_haut",
        "t_direct_p50_ms",
        "t_direct_p95_ms",
        "part_direct_moins_5s",
        "debit_max_median_mbps",
        "rtt_relais_p50_ms",
        "rtt_direct_p50_ms",
        "gigue_mediane_ms",
        "taux_pondere_volume_bas",
        "taux_pondere_volume_haut",
        "erreurs",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for (format, _) in FORMATS {
        for chemin in CHEMINS {
            let f = format.replace('/', "_");
            entete.push(format!("flux_{f}_{chemin}_n"));
            entete.push(format!("flux_{f}_{chemin}_sans_vidage"));
        }
    }
    let mut s = entete.join(",");
    s.push('\n');
    for (c, l) in lignes {
        let mut champs = vec![
            champ_csv(&c.reseau),
            champ_csv(&c.operateur),
            champ_csv(&c.box_),
            l.essais.to_string(),
            l.n.to_string(),
            l.direct.to_string(),
            l.mixte.to_string(),
            opt(l.taux_direct),
            opt(l.ic95.map(|i| i.0)),
            opt(l.ic95.map(|i| i.1)),
            opt(l.t_direct_p50),
            opt(l.t_direct_p95),
            opt(l.part_direct_5s),
            opt(l.debit_max_median),
            opt(l.rtt_relais_p50),
            opt(l.rtt_direct_p50),
            opt(l.gigue_mediane),
            opt(l.pondere_bas),
            opt(l.pondere_haut),
            l.erreurs.to_string(),
        ];
        for (format, _) in FORMATS {
            for chemin in CHEMINS {
                match l.sans_vidage.get(&(format.to_string(), chemin.to_string())) {
                    Some((n, ok)) => {
                        champs.push(n.to_string());
                        champs.push(opt(Some(*ok as f64 / *n as f64)));
                    }
                    None => {
                        champs.push("0".into());
                        champs.push(String::new());
                    }
                }
            }
        }
        s.push_str(&champs.join(","));
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une ligne factice au format du § 3.2.
    fn ligne(
        reseau: &str,
        operateur: &str,
        box_: &str,
        chemin: Option<&str>,
        t_direct: Option<u64>,
        flux: &[(&str, f64, u64, u32)],
    ) -> String {
        let flux: Vec<String> = flux
            .iter()
            .map(|(f, d, s, v)| {
                format!(
                    r#"{{"format":"{f}","duree_s":{s},"debit_moyen_mbps":{d},"sous_debits":{v}}}"#
                )
            })
            .collect();
        let chemin = chemin.map(|c| format!("\"{c}\"")).unwrap_or("null".into());
        let t_direct = t_direct.map(|t| t.to_string()).unwrap_or("null".into());
        format!(
            r#"{{"v":1,"campagne":"c1","testeur":"t03","essai":"7f3a","debut":"2026-11-03T18:42:10Z","client":{{"os":"ios","plateforme":"app","reseau":"{reseau}","operateur":"{operateur}","partage":false}},"serveur":{{"site":"domicile","box":"{box_}","famille":"ipv4+ipv6"}},"relais":"mozaiklabs","chemin_final":{chemin},"famille_direct":"ipv6","t_connecte_ms":412,"t_premier_octet_ms":455,"t_direct_ms":{t_direct},"debit_max_mbps":38.2,"flux":[{}],"rtt":{{"relais":{{"p50":61,"p95":88,"p99":90}},"direct":{{"p50":24,"p95":41,"p99":70}},"gigue_ms":4.1,"pertes_ping":0.002}},"batterie":null,"erreur":null}}"#,
            flux.join(",")
        )
    }

    #[test]
    fn la_ligne_d_exemple_du_cahier_se_lit() {
        // L'exemple du § 3.2, tel quel (relais sans p99).
        let exemple = r#"{"v":1,"campagne":"c1","testeur":"t03","essai":"7f3a","debut":"2026-11-03T18:42:10Z","client":{"os":"ios","plateforme":"app","reseau":"5g","operateur":"bouygues","partage":false},"serveur":{"site":"domicile","box":"bbox","famille":"ipv4+ipv6"},"relais":"mozaiklabs","chemin_final":"direct","famille_direct":"ipv6","t_connecte_ms":412,"t_premier_octet_ms":455,"t_direct_ms":1830,"debit_max_mbps":38.2,"flux":[{"format":"24/96","duree_s":300,"debit_moyen_mbps":3.5,"sous_debits":0}],"rtt":{"relais":{"p50":61,"p95":88},"direct":{"p50":24,"p95":41,"p99":70},"gigue_ms":4.1,"pertes_ping":0.002},"batterie":null,"erreur":null}"#;
        let (essais, illisibles) = lire(&[exemple.to_string()]);
        assert_eq!((essais.len(), illisibles), (1, 0));
        assert_eq!(
            essais[0].rtt.as_ref().unwrap().relais.as_ref().unwrap().p99,
            None
        );
        let complet = ligne(
            "5g",
            "bouygues",
            "bbox",
            Some("direct"),
            Some(1830),
            &[("24/96", 3.5, 300, 0)],
        );
        let (essais, illisibles) = lire(&[complet]);
        assert_eq!((essais.len(), illisibles), (1, 0));
    }

    #[test]
    fn taux_direct_et_wilson_par_cellule() {
        let mut lignes = Vec::new();
        for i in 0..20 {
            let chemin = if i < 15 { "direct" } else { "relais" };
            let t = (i < 15).then_some(1000 + i * 100);
            lignes.push(ligne("5g", "free", "freebox", Some(chemin), t, &[]));
        }
        lignes.push(ligne(
            "wifi",
            "aucun",
            "livebox",
            Some("direct"),
            Some(800),
            &[],
        ));
        let (essais, _) = lire(&[lignes.join("\n")]);
        let table = par_cellule(&essais);
        assert_eq!(table.len(), 3, "deux cellules et la ligne « toutes »");
        let (c, l) = &table[0];
        assert_eq!(c.reseau, "5g");
        assert_eq!((l.n, l.direct), (20, 15));
        assert_eq!(l.taux_direct, Some(0.75));
        let (b, h) = l.ic95.unwrap();
        // Wilson 15/20 : [0,5313 ; 0,8881].
        assert!(
            (b - 0.5313).abs() < 1e-4 && (h - 0.8881).abs() < 1e-4,
            "intervalle de Wilson 15/20 attendu [0,5313 ; 0,8881], obtenu [{b} ; {h}]"
        );
        // t_direct : 1000..=2400 par pas de 100 → médiane 1700.
        assert_eq!(l.t_direct_p50, Some(1700.0));
        assert_eq!(l.part_direct_5s, Some(0.75));
        let (_, toutes) = &table[2];
        assert_eq!((toutes.n, toutes.direct), (21, 16));
    }

    #[test]
    fn taux_pondere_par_le_volume() {
        // Un essai long direct à 24/192 (7 Mbit/s × 300 s = 2100 Mbit), un essai
        // long en relais à 16/44 (1 Mbit/s × 300 s = 300 Mbit), un essai court
        // mixte (60 Mbit). Par essai : 1/3 direct ; par volume : bien plus.
        let lignes = [
            ligne(
                "5g",
                "orange",
                "livebox",
                Some("direct"),
                Some(900),
                &[("24/192", 7.0, 300, 0)],
            ),
            ligne(
                "5g",
                "orange",
                "livebox",
                Some("relais"),
                None,
                &[("16/44", 1.0, 300, 2)],
            ),
            ligne("5g", "orange", "livebox", Some("mixte"), Some(3000), &[]),
        ];
        let (essais, _) = lire(&[lignes.join("\n")]);
        let refs: Vec<&Essai> = essais.iter().collect();
        let l = calculer(&refs);
        let total = 2100.0 + 300.0 + 60.0;
        let (bas, haut) = (l.pondere_bas.unwrap(), l.pondere_haut.unwrap());
        assert!(
            (bas - 2100.0 / total).abs() < 1e-9,
            "taux direct pondéré par le volume (borne basse) : attendu {}, obtenu {bas}",
            2100.0 / total
        );
        assert!(
            (haut - 2160.0 / total).abs() < 1e-9,
            "taux direct pondéré par le volume (borne haute) : attendu {}, obtenu {haut}",
            2160.0 / total
        );
        assert!((l.taux_direct.unwrap() - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(
            l.sans_vidage
                .get(&("16/44".to_string(), "relais".to_string())),
            Some(&(1, 0))
        );
        assert_eq!(
            l.sans_vidage
                .get(&("24/192".to_string(), "direct".to_string())),
            Some(&(1, 1))
        );
    }

    #[test]
    fn les_essais_refuses_ne_comptent_pas_dans_le_taux() {
        let mut refuse = ligne("5g", "sfr", "sfrbox", None, None, &[]);
        refuse = refuse.replace(
            r#""erreur":null"#,
            r#""erreur":"refusé par le serveur : jeton refusé""#,
        );
        let lignes = [
            refuse,
            ligne("5g", "sfr", "sfrbox", Some("direct"), Some(500), &[]),
        ];
        let (essais, _) = lire(&[lignes.join("\n")]);
        let refs: Vec<&Essai> = essais.iter().collect();
        let l = calculer(&refs);
        assert_eq!(
            (l.essais, l.n, l.erreurs, l.taux_direct),
            (2, 1, 1, Some(1.0))
        );
    }

    #[test]
    fn sorties_markdown_et_csv() {
        let lignes = [ligne(
            "5g",
            "free",
            "freebox",
            Some("direct"),
            Some(900),
            &[("24/96", 3.5, 300, 0)],
        )];
        let (essais, _) = lire(&[lignes.join("\n")]);
        let table = par_cellule(&essais);
        let md = markdown(&table, 1);
        assert!(
            md.contains("| 5g | free | freebox | 1 | 1 | 1 | 100 % [21 ; 100] |"),
            "{md}"
        );
        assert!(
            md.contains("| 5g | free | freebox | 24/96 | direct | 1 | 100 % |"),
            "{md}"
        );
        assert!(md.contains("1 ligne(s) illisible(s)"));
        let csv = csv(&table);
        let mut l = csv.lines();
        let entete: Vec<&str> = l.next().unwrap().split(',').collect();
        let premiere: Vec<&str> = l.next().unwrap().split(',').collect();
        assert_eq!(entete.len(), premiere.len());
        let i = entete
            .iter()
            .position(|c| *c == "flux_24_96_direct_sans_vidage")
            .unwrap();
        assert_eq!(premiere[i], "1.0000");
        assert_eq!(premiere[0..3], ["5g", "free", "freebox"]);
    }
}
