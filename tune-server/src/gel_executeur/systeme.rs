//! L'état de la machine pendant un gel de l'exécuteur (#5677).
//!
//! Le relevé de #4924 disait ce que faisaient les fils de Tune, pas ce que
//! subissait la machine. Or les gels de Tune OS (ticket 224) arrivent sur un
//! processeur unique, avec scan, analyse et synchronisation en fond : une
//! machine à genoux (échange mémoire, disque saturé, processeur volé par
//! l'hyperviseur, quota du cgroup) et un fil de Tune qui bloque l'exécuteur
//! se ressemblent dans le journal. Ils ne se ressemblent plus ici.
//!
//! Deux photos à une seconde d'écart, prises par la veille PENDANT le gel :
//! les compteurs cumulés (temps processeur par fil, `iowait`, `steal`, pages
//! échangées, octets lus et écrits, étranglement du cgroup) deviennent des
//! débits sur cette seconde. Tout est lu dans `/proc` et `/sys/fs/cgroup`,
//! sans `ptrace` ni outil externe. Aucune donnée personnelle : des compteurs,
//! des noms de fils et des symboles du noyau.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::time::{Duration, Instant};

/// Ce que dit une photo d'un fil.
#[derive(Debug, Clone, Default)]
pub struct Fil {
    pub nom: String,
    pub etat: String,
    pub wchan: String,
    /// utime + stime, en tops d'horloge.
    pub tops: u64,
    /// Politique d'ordonnancement (0 = normal, 1 = FIFO, 2 = RR…).
    pub politique: u32,
    pub priorite_rt: u32,
}

/// Une photo des compteurs. Les champs absents (`None`, vides) sont ceux que
/// la plateforme ou les droits ne donnent pas.
#[derive(Debug, Clone)]
pub struct Photo {
    pub prise: Instant,
    /// Ligne `cpu` de `/proc/stat` : user nice system idle iowait irq softirq steal.
    pub cpu: Option<[u64; 8]>,
    /// pswpin, pswpout, pgmajfault de `/proc/vmstat`.
    pub vmstat: Option<[u64; 3]>,
    /// read_bytes, write_bytes de `/proc/self/io`.
    pub io: Option<[u64; 2]>,
    /// nr_throttled, throttled_usec du `cpu.stat` du cgroup.
    pub etranglement: Option<[u64; 2]>,
    pub fils: BTreeMap<i64, Fil>,
}

/// Les deux photos d'un gel.
pub struct Fenetre {
    pub avant: Photo,
    pub apres: Photo,
}

/// Le résumé d'une ligne, repris dans le WARN `gel_executeur_detecte` pour
/// qu'un journal exporté sans le fichier de relevé le porte quand même.
#[derive(Debug, Clone, Default)]
pub struct Resume {
    pub charge_1min: Option<String>,
    pub pression_cpu: Option<String>,
    pub pression_io: Option<String>,
    pub pression_memoire: Option<String>,
    pub iowait_pct: Option<u64>,
    pub steal_pct: Option<u64>,
    pub pages_echangees: Option<u64>,
    pub etrangle_ms: Option<u64>,
    /// Fil le plus gourmand sur la fenêtre : « nom/tid=pct% ».
    pub fil_le_plus_gourmand: Option<String>,
    /// Nombre de fils en sommeil ininterruptible (état D : disque, réseau).
    pub fils_en_d: usize,
    /// Nombre de fils prêts à tourner (état R). Sur un seul processeur, c'est
    /// le partage : un fil de travail parmi N fils R n'a qu'un N-ième du
    /// processeur.
    pub fils_en_r: usize,
}

fn lire(chemin: &str) -> Option<String> {
    std::fs::read_to_string(chemin).ok()
}

fn tops_par_seconde() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: sysconf n'a pas d'effet de bord.
        let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if t > 0 {
            return t as u64;
        }
    }
    100
}

/// Le chemin du cgroup v2 du processus, sous `/sys/fs/cgroup`.
fn cgroup() -> Option<String> {
    let texte = lire("/proc/self/cgroup")?;
    let chemin = texte.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(format!("/sys/fs/cgroup{}", chemin.trim()))
}

fn champs_nommes<const N: usize>(texte: &str, noms: [&str; N], separateur: char) -> [u64; N] {
    let mut v = [0u64; N];
    for ligne in texte.lines() {
        let mut it = ligne.splitn(2, separateur);
        let (Some(cle), Some(val)) = (it.next(), it.next()) else {
            continue;
        };
        if let Some(i) = noms.iter().position(|n| *n == cle.trim()) {
            v[i] = val
                .split_whitespace()
                .next()
                .and_then(|x| x.parse().ok())
                .unwrap_or(0);
        }
    }
    v
}

fn fils() -> BTreeMap<i64, Fil> {
    let mut m = BTreeMap::new();
    let Ok(taches) = std::fs::read_dir("/proc/self/task") else {
        return m;
    };
    for t in taches.flatten() {
        let Ok(tid) = t.file_name().to_string_lossy().parse::<i64>() else {
            continue;
        };
        let p = t.path();
        let lire_f = |f: &str| {
            std::fs::read_to_string(p.join(f))
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        let stat = lire_f("stat");
        // Après le « (nom) » : l'état est le champ 3 de proc(5) ; utime et
        // stime les 14 et 15, rt_priority et policy les 40 et 41.
        let apres = stat.rsplit_once(')').map(|(_, r)| r.trim()).unwrap_or("");
        let c: Vec<&str> = apres.split_whitespace().collect();
        let n = |i: usize| c.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        m.insert(
            tid,
            Fil {
                nom: lire_f("comm"),
                etat: c.first().copied().unwrap_or("?").to_string(),
                wchan: lire_f("wchan"),
                tops: n(11) + n(12),
                priorite_rt: n(37) as u32,
                politique: n(38) as u32,
            },
        );
    }
    m
}

/// Prendre une photo (Linux ; ailleurs, les champs restent vides).
pub fn photo() -> Photo {
    let cpu = lire("/proc/stat").and_then(|t| {
        let l = t.lines().find(|l| l.starts_with("cpu "))?.to_string();
        let v: Vec<u64> = l
            .split_whitespace()
            .skip(1)
            .filter_map(|x| x.parse().ok())
            .collect();
        let mut a = [0u64; 8];
        for (i, x) in v.iter().take(8).enumerate() {
            a[i] = *x;
        }
        Some(a)
    });
    let vmstat =
        lire("/proc/vmstat").map(|t| champs_nommes(&t, ["pswpin", "pswpout", "pgmajfault"], ' '));
    let io = lire("/proc/self/io").map(|t| champs_nommes(&t, ["read_bytes", "write_bytes"], ':'));
    let etranglement = cgroup()
        .and_then(|c| lire(&format!("{c}/cpu.stat")))
        .map(|t| champs_nommes(&t, ["nr_throttled", "throttled_usec"], ' '));
    Photo {
        prise: Instant::now(),
        cpu,
        vmstat,
        io,
        etranglement,
        fils: fils(),
    }
}

/// Deux photos à `ecart` d'intervalle, sur le fil appelant (la veille).
pub fn fenetre(ecart: Duration) -> Fenetre {
    let avant = photo();
    std::thread::sleep(ecart);
    Fenetre {
        avant,
        apres: photo(),
    }
}

fn delta<const N: usize>(a: &Option<[u64; N]>, b: &Option<[u64; N]>) -> Option<[u64; N]> {
    let (a, b) = (a.as_ref()?, b.as_ref()?);
    let mut d = [0u64; N];
    for i in 0..N {
        d[i] = b[i].saturating_sub(a[i]);
    }
    Some(d)
}

fn premiere_ligne_avg10(texte: &str, genre: &str) -> Option<String> {
    texte
        .lines()
        .find(|l| l.starts_with(genre))
        .and_then(|l| l.split_whitespace().find(|c| c.starts_with("avg10=")))
        .map(|c| c.trim_start_matches("avg10=").to_string())
}

fn pression(ressource: &str) -> Option<String> {
    lire(&format!("/proc/pressure/{ressource}"))
}

/// Les fils dont la pile noyau vaut d'être lue : en état D, au travail sur
/// la fenêtre, ou désignés par le relevé (détenteur, lectures, fils de
/// travail pris). Bornés à `au_plus`.
fn fils_a_examiner(f: &Fenetre, designes: &[i64], au_plus: usize) -> Vec<i64> {
    let mut v: Vec<(u64, i64)> = f
        .apres
        .fils
        .iter()
        .filter_map(|(tid, fil)| {
            let d = f
                .avant
                .fils
                .get(tid)
                .map(|a| fil.tops.saturating_sub(a.tops))
                .unwrap_or(0);
            let interesse = fil.etat == "D" || d > 0 || designes.contains(tid);
            interesse.then_some((
                // Les désignés et les fils en D d'abord, puis les plus gourmands.
                if designes.contains(tid) || fil.etat == "D" {
                    u64::MAX
                } else {
                    d
                },
                *tid,
            ))
        })
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    v.into_iter().take(au_plus).map(|(_, t)| t).collect()
}

/// La section « système » du relevé, et son résumé d'une ligne. `designes` :
/// les tid que le reste du relevé met en cause, dont on veut la pile noyau.
pub fn section(f: &Fenetre, designes: &[i64]) -> (String, Resume) {
    let mut s = String::new();
    let mut r = Resume::default();
    let secondes = f
        .apres
        .prise
        .duration_since(f.avant.prise)
        .as_secs_f64()
        .max(0.001);
    let tps = tops_par_seconde() as f64;

    let _ = writeln!(s, "\n== machine ==");
    let _ = writeln!(
        s,
        "processeurs vus (available_parallelism) : {} ; fils de travail retenus : {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        crate::fils_de_travail::retenu()
    );
    if let Some(status) = lire("/proc/self/status") {
        for cle in [
            "Cpus_allowed_list",
            "Threads",
            "VmRSS",
            "VmSwap",
            "voluntary_ctxt_switches",
            "nonvoluntary_ctxt_switches",
        ] {
            if let Some(l) = status.lines().find(|l| l.starts_with(&format!("{cle}:"))) {
                let _ = writeln!(s, "{}", l.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        }
    }
    if let Some(c) = cgroup() {
        let _ = writeln!(
            s,
            "cgroup : cpu.max={} memory.max={} memory.current={}",
            lire(&format!("{c}/cpu.max"))
                .unwrap_or_else(|| "?".into())
                .trim(),
            lire(&format!("{c}/memory.max"))
                .unwrap_or_else(|| "?".into())
                .trim(),
            lire(&format!("{c}/memory.current"))
                .unwrap_or_else(|| "?".into())
                .trim(),
        );
    }
    if let Some(l) = lire("/proc/loadavg") {
        let l = l.trim().to_string();
        r.charge_1min = l.split_whitespace().next().map(str::to_string);
        let _ = writeln!(s, "charge (/proc/loadavg) : {l}");
    }
    for (ressource, champ) in [
        ("cpu", &mut r.pression_cpu),
        ("io", &mut r.pression_io),
        ("memory", &mut r.pression_memoire),
    ] {
        match pression(ressource) {
            Some(t) => {
                *champ = premiere_ligne_avg10(&t, "some");
                for l in t.lines() {
                    let _ = writeln!(s, "pression {ressource} : {l}");
                }
            }
            None => {
                let _ = writeln!(s, "pression {ressource} : indisponible (PSI absent)");
            }
        }
    }
    if let Some(m) = lire("/proc/meminfo") {
        let v = champs_nommes(
            &m,
            [
                "MemTotal",
                "MemAvailable",
                "SwapTotal",
                "SwapFree",
                "Dirty",
                "Writeback",
            ],
            ':',
        );
        let _ = writeln!(
            s,
            "mémoire (Mio) : totale {} disponible {} ; échange {} dont libre {} ; sale {} en écriture {}",
            v[0] / 1024,
            v[1] / 1024,
            v[2] / 1024,
            v[3] / 1024,
            v[4] / 1024,
            v[5] / 1024
        );
    }

    let _ = writeln!(s, "\n== sur {:.1} s pendant le gel ==", secondes);
    if let Some(d) = delta(&f.avant.cpu, &f.apres.cpu) {
        let total: u64 = d.iter().sum::<u64>().max(1);
        let pct = |x: u64| x * 100 / total;
        r.iowait_pct = Some(pct(d[4]));
        r.steal_pct = Some(pct(d[7]));
        let _ = writeln!(
            s,
            "processeur (machine) : user {}% system {}% iowait {}% steal {}% idle {}%",
            pct(d[0] + d[1]),
            pct(d[2] + d[5] + d[6]),
            pct(d[4]),
            pct(d[7]),
            pct(d[3])
        );
    }
    if let Some(d) = delta(&f.avant.vmstat, &f.apres.vmstat) {
        r.pages_echangees = Some(d[0] + d[1]);
        let _ = writeln!(
            s,
            "échange : {} pages lues, {} pages écrites ; {} défauts majeurs",
            d[0], d[1], d[2]
        );
    }
    if let Some(d) = delta(&f.avant.io, &f.apres.io) {
        let _ = writeln!(
            s,
            "E/S du processus : {} Kio lus, {} Kio écrits",
            d[0] / 1024,
            d[1] / 1024
        );
    }
    if let Some(d) = delta(&f.avant.etranglement, &f.apres.etranglement) {
        r.etrangle_ms = Some(d[1] / 1000);
        let _ = writeln!(
            s,
            "quota processeur du cgroup : {} étranglement(s), {} ms étranglés",
            d[0],
            d[1] / 1000
        );
    }

    let _ = writeln!(
        s,
        "\n== fils du processus (tid, nom, état, wchan, % d'un processeur sur la fenêtre, politique/prio rt) =="
    );
    let mut gourmand: Option<(u64, String)> = None;
    for (tid, fil) in &f.apres.fils {
        let d = f
            .avant
            .fils
            .get(tid)
            .map(|a| fil.tops.saturating_sub(a.tops))
            .unwrap_or(0);
        let pct = (d as f64 / tps / secondes * 100.0).round() as u64;
        if fil.etat == "D" {
            r.fils_en_d += 1;
        }
        if fil.etat == "R" {
            r.fils_en_r += 1;
        }
        if d > 0 && gourmand.as_ref().is_none_or(|(p, _)| pct > *p) {
            gourmand = Some((pct, format!("{}/{tid}={pct}%", fil.nom)));
        }
        let politique = match fil.politique {
            0 => "normal".to_string(),
            1 => format!("FIFO/{}", fil.priorite_rt),
            2 => format!("RR/{}", fil.priorite_rt),
            3 => "batch".to_string(),
            5 => "idle".to_string(),
            p => format!("{p}/{}", fil.priorite_rt),
        };
        let _ = writeln!(
            s,
            "{tid}\t{}\t{}\t{}\t{pct}%\t{politique}",
            fil.nom, fil.etat, fil.wchan
        );
    }
    let _ = writeln!(s, "({} fils)", f.apres.fils.len());
    r.fil_le_plus_gourmand = gourmand.map(|(_, t)| t);

    let examines = fils_a_examiner(f, designes, 16);
    if !examines.is_empty() {
        let _ = writeln!(
            s,
            "\n== piles noyau (/proc/self/task/<tid>/stack ; lisibles en root seulement) =="
        );
        for tid in examines {
            let nom = f
                .apres
                .fils
                .get(&tid)
                .map(|x| x.nom.as_str())
                .unwrap_or("?");
            match std::fs::read_to_string(format!("/proc/self/task/{tid}/stack")) {
                Ok(p) if !p.trim().is_empty() => {
                    let _ = writeln!(s, "-- {tid} {nom}");
                    for l in p.lines().take(16) {
                        let _ = writeln!(s, "   {l}");
                    }
                }
                Ok(_) => {
                    let _ = writeln!(s, "-- {tid} {nom} : pile vide (fil en espace utilisateur)");
                }
                Err(e) => {
                    let _ = writeln!(s, "-- {tid} {nom} : illisible ({e})");
                }
            }
        }
    }
    (s, r)
}

impl std::fmt::Display for Resume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let o = |v: &Option<String>| v.clone().unwrap_or_else(|| "?".into());
        let n = |v: &Option<u64>| v.map(|x| x.to_string()).unwrap_or_else(|| "?".into());
        write!(
            f,
            "charge={} psi_cpu={} psi_io={} psi_mem={} iowait%={} steal%={} pages_echangees={} etrangle_ms={} fils_en_D={} fils_en_R={} plus_gourmand={}",
            o(&self.charge_1min),
            o(&self.pression_cpu),
            o(&self.pression_io),
            o(&self.pression_memoire),
            n(&self.iowait_pct),
            n(&self.steal_pct),
            n(&self.pages_echangees),
            n(&self.etrangle_ms),
            self.fils_en_d,
            self.fils_en_r,
            o(&self.fil_le_plus_gourmand),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_champs_nommes_se_lisent_dans_les_deux_formats() {
        let vmstat = "pgpgin 1\npswpin 7\npswpout 9\npgmajfault 3\n";
        assert_eq!(
            champs_nommes(vmstat, ["pswpin", "pswpout", "pgmajfault"], ' '),
            [7, 9, 3]
        );
        let io = "rchar: 5\nread_bytes: 4096\nwrite_bytes: 8192\n";
        assert_eq!(
            champs_nommes(io, ["read_bytes", "write_bytes"], ':'),
            [4096, 8192]
        );
        let meminfo = "MemTotal:        2048000 kB\nMemAvailable:     512000 kB\n";
        assert_eq!(
            champs_nommes(meminfo, ["MemTotal", "MemAvailable"], ':'),
            [2048000, 512000]
        );
    }

    #[test]
    fn la_pression_avg10_se_lit() {
        let t = "some avg10=42.50 avg60=10.00 avg300=1.00 total=123\nfull avg10=30.00 avg60=0 avg300=0 total=9\n";
        assert_eq!(premiere_ligne_avg10(t, "some").as_deref(), Some("42.50"));
        assert_eq!(premiere_ligne_avg10(t, "full").as_deref(), Some("30.00"));
    }

    /// Un fil qui brûle du processeur pendant la fenêtre en sort comme le plus
    /// gourmand (Linux : c'est là que `/proc/self/task` existe).
    #[cfg(target_os = "linux")]
    #[test]
    fn un_fil_qui_tourne_pendant_la_fenetre_est_le_plus_gourmand() {
        let fin = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f2 = fin.clone();
        let h = std::thread::Builder::new()
            .name("brule-5677".into())
            .spawn(move || {
                let mut x = 0u64;
                while !f2.load(std::sync::atomic::Ordering::Relaxed) {
                    x = x.wrapping_add(1);
                    std::hint::black_box(x);
                }
            })
            .unwrap();
        let fen = fenetre(Duration::from_millis(600));
        fin.store(true, std::sync::atomic::Ordering::Relaxed);
        h.join().unwrap();
        let (texte, resume) = section(&fen, &[]);
        assert!(texte.contains("brule-5677"), "{texte}");
        assert!(texte.contains("== machine =="), "{texte}");
        // Sous la charge de toute la suite, le fil peut ne pas avoir eu tout
        // un processeur ; il doit au moins avoir été vu au travail.
        let ligne = texte
            .lines()
            .find(|l| l.contains("brule-5677") && l.contains('%'))
            .unwrap();
        assert!(!ligne.contains("\t0%\t"), "{ligne}");
        assert!(resume.fil_le_plus_gourmand.is_some(), "{resume}");
    }
}
