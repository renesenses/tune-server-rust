//! Garde thermique des analyses de fond (#1576).
//!
//! Les passes d'analyse (ReplayGain, acoustique) sont le seul travail que ce
//! serveur exécute *à pleine charge sans que personne ne l'ait demandé*. Sur
//! .18 elles ont tenu la machine à ~450 % CPU pendant 75 minutes avant qu'elle
//! ne s'éteigne net — deux fois, journal coupé en pleine ligne, aucune trace
//! noyau, mémoire hors de cause. La sérialisation (#1672) a divisé le pic par
//! deux ; elle ne protège pas une machine mal ventilée qui chauffe quand même.
//!
//! Ce garde lit la température CPU quand le système l'expose et suspend les
//! analyses au-delà d'un seuil, avec hystérésis pour ne pas osciller. Le
//! principe est le même que pour la mémoire : **une analyse facultative ne doit
//! jamais mettre la machine en danger**, et être en retard d'une heure ne coûte
//! rien.

// Hors Linux, la lecture sysfs et ses sélecteurs ne servent qu'aux tests : les
// deux fonctions publiques y rendent `None` sans toucher au disque.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use tracing::{info, warn};

/// Au-dessus de cette température, les analyses s'arrêtent.
///
/// 80 °C est franchement chaud pour un serveur au repos, et encore loin du
/// seuil de throttling des CPU modernes (~100 °C) : on s'arrête avant que le
/// matériel n'ait à se défendre lui-même, pas après.
const PAUSE_ABOVE_C: f64 = 80.0;

/// En dessous de cette température, elles reprennent. L'écart avec le seuil de
/// pause est ce qui empêche l'oscillation : sans lui, une machine posée juste
/// au seuil ferait démarrer/arrêter la passe en boucle.
const RESUME_BELOW_C: f64 = 70.0;

/// Décision d'un tour de garde, pour que l'appelant journalise les
/// *transitions* et non chaque tour de boucle.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Trop chaud : la passe attend. `entering` marque l'entrée en pause.
    Hold { temp_c: f64, entering: bool },
    /// Température acceptable (ou inconnue) : la passe peut travailler.
    /// `leaving` marque la sortie de pause.
    Go { temp_c: Option<f64>, leaving: bool },
}

/// Garde à hystérésis. Une instance par passe : chacune journalise ses propres
/// transitions, et une passe déjà en pause ne parle plus jusqu'au retour au
/// frais.
#[derive(Default)]
pub struct ThermalGate {
    holding: bool,
}

impl ThermalGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lit la température et tranche. Sans capteur lisible (macOS, Windows,
    /// conteneur sans `/sys` monté), renvoie toujours `Go` : un garde qui ne
    /// peut pas mesurer ne doit pas inventer une raison de bloquer.
    pub fn check(&mut self) -> Verdict {
        self.decide(cpu_temp_celsius())
    }

    /// Logique pure, séparée de la lecture du système pour être testable
    /// partout — c'est là que vivent les bugs d'hystérésis.
    fn decide(&mut self, temp_c: Option<f64>) -> Verdict {
        let Some(t) = temp_c else {
            let leaving = self.holding;
            self.holding = false;
            return Verdict::Go {
                temp_c: None,
                leaving,
            };
        };
        if self.holding {
            if t <= RESUME_BELOW_C {
                self.holding = false;
                Verdict::Go {
                    temp_c: Some(t),
                    leaving: true,
                }
            } else {
                Verdict::Hold {
                    temp_c: t,
                    entering: false,
                }
            }
        } else if t >= PAUSE_ABOVE_C {
            self.holding = true;
            Verdict::Hold {
                temp_c: t,
                entering: true,
            }
        } else {
            Verdict::Go {
                temp_c: Some(t),
                leaving: false,
            }
        }
    }

    /// Applique le verdict : journalise les transitions et dit si la passe doit
    /// attendre. Factorisé ici pour que les deux sweeps se comportent — et
    /// s'expriment — à l'identique.
    pub fn should_hold(&mut self, sweep: &str) -> bool {
        match self.check() {
            Verdict::Hold {
                temp_c,
                entering: true,
            } => {
                warn!(
                    sweep,
                    temp_c,
                    pause_above_c = PAUSE_ABOVE_C,
                    "analysis_paused_hot — analyse de fond suspendue, la machine est trop chaude ; la lecture n'est pas affectée"
                );
                true
            }
            Verdict::Hold { .. } => true,
            Verdict::Go {
                temp_c,
                leaving: true,
            } => {
                info!(
                    sweep,
                    temp_c = temp_c.unwrap_or(0.0),
                    "analysis_resumed_cooled — température revenue à la normale"
                );
                false
            }
            Verdict::Go { .. } => false,
        }
    }
}

/// Un relevé de température lu dans sysfs : la puce ou la zone qui l'expose,
/// l'étiquette du capteur quand il en a une, et la valeur en °C.
///
/// La lecture est UNE (`lire_capteurs`), le capteur du paquet CPU aussi
/// (`temperature_paquet_cpu`) : le garde et l'écran « État du serveur » lisent
/// tous deux le processeur d'abord (#5189). Ils ne diffèrent que par leur repli
/// quand aucune sonde CPU n'existe : le maximum des `hwmon` pour le garde, le
/// maximum de toutes les zones pour l'écran.
#[derive(Debug, Clone, PartialEq)]
struct Releve {
    /// Nom de la puce `hwmon` (`coretemp`, `k10temp`, `nvme`…) ou type de la
    /// zone thermique (`x86_pkg_temp`, `acpitz`…).
    capteur: String,
    /// `temp*_label` quand la puce en fournit un (`Package id 0`, `Tctl`…).
    etiquette: Option<String>,
    /// Vrai pour `/sys/class/hwmon`, faux pour `/sys/class/thermal`.
    hwmon: bool,
    celsius: f64,
}

/// Lit tous les capteurs de température exposés sous `racine` (`/sys` en
/// production, une arborescence simulée dans les tests) :
/// `class/hwmon/*/temp*_input` (+ `name`, `temp*_label`) et
/// `class/thermal/thermal_zone*/temp` (+ `type`). Que des lectures de petits
/// fichiers sysfs ; tout ce qui manque ou ne se lit pas est ignoré.
fn lire_capteurs(racine: &std::path::Path) -> Vec<Releve> {
    let lire = |p: std::path::PathBuf| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
    };
    let mut releves = Vec::new();
    if let Ok(puces) = std::fs::read_dir(racine.join("class/hwmon")) {
        for puce in puces.flatten() {
            let dossier = puce.path();
            let capteur = lire(dossier.join("name")).unwrap_or_default();
            let Ok(fichiers) = std::fs::read_dir(&dossier) else {
                continue;
            };
            for f in fichiers.flatten() {
                let nom = f.file_name();
                let nom = nom.to_string_lossy();
                let Some(base) = nom
                    .strip_prefix("temp")
                    .and_then(|r| r.strip_suffix("_input"))
                else {
                    continue;
                };
                if let Some(celsius) = lire(f.path()).as_deref().and_then(parse_millidegrees) {
                    releves.push(Releve {
                        capteur: capteur.clone(),
                        etiquette: lire(dossier.join(format!("temp{base}_label"))),
                        hwmon: true,
                        celsius,
                    });
                }
            }
        }
    }
    if let Ok(zones) = std::fs::read_dir(racine.join("class/thermal")) {
        for zone in zones.flatten() {
            if !zone
                .file_name()
                .to_string_lossy()
                .starts_with("thermal_zone")
            {
                continue;
            }
            let dossier = zone.path();
            if let Some(celsius) = lire(dossier.join("temp"))
                .as_deref()
                .and_then(parse_millidegrees)
            {
                releves.push(Releve {
                    capteur: lire(dossier.join("type")).unwrap_or_default(),
                    etiquette: None,
                    hwmon: false,
                    celsius,
                });
            }
        }
    }
    releves
}

fn maximum<'a>(releves: impl Iterator<Item = &'a Releve>) -> Option<f64> {
    releves.map(|r| r.celsius).reduce(f64::max)
}

/// Le REPLI du garde, quand aucune sonde CPU n'est reconnue : le point le plus
/// chaud des `hwmon`.
///
/// Les noms de puces varient d'une plateforme à l'autre (`soc_thermal` sur
/// bien des SBC, puces de cartes exotiques) et un serveur audio tourne sur tout
/// ça : sans capteur CPU identifiable, le maximum reste la grandeur prudente.
fn plus_chaud_hwmon(releves: &[Releve]) -> Option<f64> {
    maximum(releves.iter().filter(|r| r.hwmon))
}

/// Le choix du GARDE (#5189) : la température du processeur quand une sonde
/// CPU existe — la même que l'écran affiche —, sinon le point le plus chaud
/// des `hwmon`.
///
/// Prendre le maximum de toutes les puces faisait décider le garde sur le GPU
/// intégré (`amdgpu`), un NVMe ou un disque (`drivetemp`) : sur un AMD
/// GX-222GC sans ventilateur, `amdgpu` à 80,0 °C suspendait les analyses
/// alors que `k10temp` lisait 79,4 °C. Les analyses chargent le CPU ; c'est
/// lui que le garde surveille. Dès qu'une sonde CPU existe, aucune autre puce
/// n'entre donc dans la décision.
fn temperature_garde(releves: &[Releve]) -> Option<f64> {
    temperature_paquet_cpu(releves).or_else(|| plus_chaud_hwmon(releves))
}

/// Capteurs qui mesurent le paquet CPU, par ordre de préférence (#5189).
/// `x86_pkg_temp` est une zone thermique (Intel) ; `coretemp` et `k10temp`
/// sont des puces `hwmon` ; le Raspberry Pi expose `cpu-thermal` comme zone et
/// `cpu_thermal` comme `hwmon`.
const CAPTEURS_PAQUET_CPU: &[&str] = &[
    "x86_pkg_temp",
    "coretemp",
    "k10temp",
    "cpu_thermal",
    "cpu-thermal",
];

/// Rang d'une étiquette dans une puce CPU : `Package id N` (Intel) et `Tdie`
/// (AMD, température réelle) d'abord, puis `Tctl` (AMD, peut porter un
/// décalage), puis le reste (cœurs, CCD).
fn rang_etiquette(etiquette: Option<&str>) -> u8 {
    match etiquette {
        Some(e) if e.starts_with("Package id") || e == "Tdie" => 0,
        Some("Tctl") => 1,
        _ => 2,
    }
}

/// Le paquet CPU, quand un capteur de `CAPTEURS_PAQUET_CPU` le désigne ;
/// `None` si aucune sonde CPU n'est reconnue. Partagé par le garde et l'écran.
fn temperature_paquet_cpu(releves: &[Releve]) -> Option<f64> {
    for nom in CAPTEURS_PAQUET_CPU {
        let puce: Vec<&Releve> = releves.iter().filter(|r| r.capteur == *nom).collect();
        let Some(meilleur) = puce
            .iter()
            .map(|r| rang_etiquette(r.etiquette.as_deref()))
            .min()
        else {
            continue;
        };
        return maximum(
            puce.into_iter()
                .filter(|r| rang_etiquette(r.etiquette.as_deref()) == meilleur),
        );
    }
    None
}

/// Le choix de l'ÉCRAN : le paquet CPU quand un capteur le désigne, sinon le
/// maximum de toutes les zones lues. `None` sans aucun capteur.
fn choisir_temperature_processeur(releves: &[Releve]) -> Option<f64> {
    temperature_paquet_cpu(releves).or_else(|| maximum(releves.iter()))
}

/// Température sur laquelle le garde décide, en °C : le processeur d'abord.
#[cfg(target_os = "linux")]
fn cpu_temp_celsius() -> Option<f64> {
    temperature_garde(&lire_capteurs(std::path::Path::new("/sys")))
}

#[cfg(not(target_os = "linux"))]
fn cpu_temp_celsius() -> Option<f64> {
    None
}

/// Température du processeur à AFFICHER (#5189, écran « État du serveur »).
///
/// Même lecture sysfs et même capteur CPU que le garde ; seul le repli
/// diffère : sans capteur CPU identifiable, le maximum des zones. `None` sans capteur
/// (macOS, Windows, conteneur ou machine virtuelle sans `/sys` peuplé).
///
/// Lecture synchrone de fichiers sysfs : l'appelant asynchrone la passe par
/// `spawn_blocking` avec un délai, certains pilotes `hwmon` (disques) pouvant
/// être lents à répondre.
#[cfg(target_os = "linux")]
pub fn cpu_package_temp_celsius() -> Option<f64> {
    choisir_temperature_processeur(&lire_capteurs(std::path::Path::new("/sys")))
}

#[cfg(not(target_os = "linux"))]
pub fn cpu_package_temp_celsius() -> Option<f64> {
    None
}

/// `"57000\n"` → `57.0` °C. Rejette les valeurs hors du domaine physique d'un
/// capteur CPU : certains hwmon exposent des sondes de tension ou des
/// sentinelles (0, valeurs négatives absurdes) dans le même format, et les
/// prendre pour des degrés ferait taire la passe pour rien — ou, pire, la
/// laisserait tourner sur un capteur muet.
fn parse_millidegrees(raw: &str) -> Option<f64> {
    let milli: f64 = raw.trim().parse().ok()?;
    let c = milli / 1000.0;
    (5.0..=125.0).contains(&c).then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_millidegrees_and_rejects_nonsense() {
        assert_eq!(parse_millidegrees("57000\n"), Some(57.0));
        assert_eq!(parse_millidegrees("  81500 "), Some(81.5));
        // Sondes non-CPU / sentinelles : hors domaine physique.
        assert_eq!(parse_millidegrees("0"), None);
        assert_eq!(parse_millidegrees("-40000"), None);
        assert_eq!(parse_millidegrees("900000"), None);
        assert_eq!(parse_millidegrees("pas un nombre"), None);
    }

    #[test]
    fn hysteresis_holds_until_really_cooled() {
        let mut g = ThermalGate::new();
        // Sous le seuil : on travaille.
        assert!(matches!(g.decide(Some(65.0)), Verdict::Go { .. }));
        // Au seuil : on s'arrête, et c'est l'entrée en pause.
        assert_eq!(
            g.decide(Some(80.0)),
            Verdict::Hold {
                temp_c: 80.0,
                entering: true
            }
        );
        // Toujours chaud : on reste en pause, sans re-signaler.
        assert_eq!(
            g.decide(Some(79.0)),
            Verdict::Hold {
                temp_c: 79.0,
                entering: false
            }
        );
        // Entre les deux seuils : l'hystérésis maintient la pause — c'est tout
        // l'intérêt, sinon la passe redémarrerait pour rechauffer aussitôt.
        assert_eq!(
            g.decide(Some(72.0)),
            Verdict::Hold {
                temp_c: 72.0,
                entering: false
            }
        );
        // Vraiment refroidi : reprise, signalée une fois.
        assert_eq!(
            g.decide(Some(69.0)),
            Verdict::Go {
                temp_c: Some(69.0),
                leaving: true
            }
        );
        assert_eq!(
            g.decide(Some(69.0)),
            Verdict::Go {
                temp_c: Some(69.0),
                leaving: false
            }
        );
    }

    #[test]
    fn no_sensor_never_blocks() {
        // macOS, Windows, conteneur sans /sys : la passe doit tourner comme
        // avant. Un garde aveugle qui bloque serait une régression silencieuse.
        let mut g = ThermalGate::new();
        assert_eq!(
            g.decide(None),
            Verdict::Go {
                temp_c: None,
                leaving: false
            }
        );
        // Et s'il perd le capteur en cours de pause, il libère la passe.
        g.decide(Some(85.0));
        assert_eq!(
            g.decide(None),
            Verdict::Go {
                temp_c: None,
                leaving: true
            }
        );
    }

    // ── #5189 : le capteur affiché, sur une arborescence sysfs simulée ─────

    /// Construit `class/hwmon/hwmonN` : `(name, [(index, millidegrés, étiquette)])`.
    fn puce(racine: &std::path::Path, n: usize, name: &str, temps: &[(u8, &str, Option<&str>)]) {
        let d = racine.join(format!("class/hwmon/hwmon{n}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("name"), format!("{name}\n")).unwrap();
        for (i, milli, label) in temps {
            std::fs::write(d.join(format!("temp{i}_input")), format!("{milli}\n")).unwrap();
            if let Some(l) = label {
                std::fs::write(d.join(format!("temp{i}_label")), format!("{l}\n")).unwrap();
            }
        }
    }

    fn zone(racine: &std::path::Path, n: usize, kind: &str, milli: &str) {
        let d = racine.join(format!("class/thermal/thermal_zone{n}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("type"), format!("{kind}\n")).unwrap();
        std::fs::write(d.join("temp"), format!("{milli}\n")).unwrap();
    }

    fn affichee(racine: &std::path::Path) -> Option<f64> {
        choisir_temperature_processeur(&lire_capteurs(racine))
    }

    fn garde(racine: &std::path::Path) -> Option<f64> {
        temperature_garde(&lire_capteurs(racine))
    }

    /// Le cas du testeur (AMD GX-222GC sans ventilateur, forum fil 1972) :
    /// le GPU intégré à 80,0 °C, le CPU à 79,4 °C. Le garde décide sur le CPU,
    /// donc les analyses continuent.
    #[test]
    fn garde_amd_ignore_le_gpu_integre() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        puce(r, 0, "amdgpu", &[(1, "80000", Some("edge"))]);
        puce(r, 1, "k10temp", &[(1, "79375", None)]);
        assert_eq!(garde(r), Some(79.375));
        let mut g = ThermalGate::new();
        assert_eq!(
            g.decide(garde(r)),
            Verdict::Go {
                temp_c: Some(79.375),
                leaving: false
            },
            "le GPU intégré (amdgpu 80 °C) ne doit pas suspendre les analyses"
        );
        // Et le CPU au seuil, lui, les suspend toujours.
        std::fs::write(r.join("class/hwmon/hwmon1/temp1_input"), "80000\n").unwrap();
        assert!(matches!(g.decide(garde(r)), Verdict::Hold { .. }));
    }

    #[test]
    fn garde_intel_coretemp_ignore_nvme_et_drivetemp() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        puce(
            r,
            0,
            "coretemp",
            &[
                (1, "61000", Some("Package id 0")),
                (2, "59000", Some("Core 0")),
            ],
        );
        puce(r, 1, "nvme", &[(1, "84850", Some("Composite"))]);
        puce(r, 2, "drivetemp", &[(1, "82000", None)]);
        puce(r, 3, "nouveau", &[(1, "90000", None)]);
        assert_eq!(garde(r), Some(61.0));
    }

    #[test]
    fn garde_raspberry_pi_cpu_thermal() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        puce(r, 0, "rpi_volt", &[]);
        puce(r, 1, "cpu_thermal", &[(1, "66200", None)]);
        puce(r, 2, "drivetemp", &[(1, "81000", None)]);
        assert_eq!(garde(r), Some(66.2));
    }

    /// Repli : aucune sonde CPU reconnue → le maximum des `hwmon`, quelles
    /// qu'elles soient. Un garde qui n'a que le GPU ou le disque pour mesurer
    /// s'en sert plutôt que de travailler à l'aveugle.
    #[test]
    fn garde_sans_sonde_cpu_retombe_sur_le_maximum_des_hwmon() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        puce(r, 0, "soc_thermal", &[(1, "64000", None)]);
        puce(r, 1, "amdgpu", &[(1, "81000", None)]);
        zone(r, 0, "acpitz", "95000");
        assert_eq!(garde(r), Some(81.0));
        assert_eq!(garde(&r.join("absent")), None);
    }

    /// Machine Intel typique : un NVMe et la zone ACPI plus chauds que le
    /// paquet. L'écran doit montrer le PAQUET, pas le maximum — et le garde
    /// décide lui aussi sur le paquet, pas sur le NVMe.
    #[test]
    fn intel_affiche_le_paquet_et_pas_le_nvme() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        puce(r, 0, "acpitz", &[(1, "27800", None)]);
        puce(
            r,
            1,
            "coretemp",
            &[
                (1, "52000", Some("Package id 0")),
                (2, "58000", Some("Core 0")),
            ],
        );
        puce(r, 2, "nvme", &[(1, "71850", Some("Composite"))]);
        zone(r, 0, "acpitz", "27800");
        assert_eq!(affichee(r), Some(52.0));
        assert_eq!(garde(r), Some(52.0));
    }

    /// Contre-épreuve : sans puce CPU reconnaissable (machine virtuelle,
    /// carte exotique), on retombe sur le maximum de TOUTES les zones, zones
    /// thermiques comprises.
    #[test]
    fn sans_capteur_cpu_identifiable_on_prend_le_maximum_des_zones() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        puce(r, 0, "nvme", &[(1, "41000", None)]);
        zone(r, 0, "acpitz", "47500");
        assert_eq!(affichee(r), Some(47.5));
        // Et la zone ne fait PAS bouger le garde : son repli ne lit que les
        // hwmon, NVMe compris faute de mieux.
        assert_eq!(garde(r), Some(41.0));
    }

    #[test]
    fn la_zone_x86_pkg_temp_prime_sur_coretemp() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        puce(r, 0, "coretemp", &[(2, "60000", Some("Core 0"))]);
        zone(r, 3, "x86_pkg_temp", "49000");
        assert_eq!(affichee(r), Some(49.0));
    }

    #[test]
    fn amd_prefere_tdie_puis_tctl_aux_ccd() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        puce(
            r,
            0,
            "k10temp",
            &[(1, "66000", Some("Tctl")), (3, "70000", Some("Tccd1"))],
        );
        assert_eq!(affichee(r), Some(66.0));
        std::fs::write(r.join("class/hwmon/hwmon0/temp2_input"), "56000\n").unwrap();
        std::fs::write(r.join("class/hwmon/hwmon0/temp2_label"), "Tdie\n").unwrap();
        assert_eq!(affichee(r), Some(56.0));
    }

    #[test]
    fn raspberry_pi_cpu_thermal() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        puce(r, 0, "rpi_volt", &[]);
        puce(r, 1, "cpu_thermal", &[(1, "48312", None)]);
        puce(r, 2, "drivetemp", &[(1, "51000", None)]);
        assert_eq!(affichee(r), Some(48.312));
    }

    /// Conteneur, VM ou autre OS : rien sous `/sys` → `None`, jamais 0 °C.
    #[test]
    fn aucune_arborescence_aucune_valeur() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(affichee(t.path()), None);
        assert_eq!(affichee(&t.path().join("absent")), None);
        // Un capteur muet (sentinelle 0) ne vaut pas une mesure.
        puce(t.path(), 0, "coretemp", &[(1, "0", Some("Package id 0"))]);
        assert_eq!(affichee(t.path()), None);
    }
}
