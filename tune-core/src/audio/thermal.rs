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

/// Température CPU la plus élevée exposée par le système, en °C.
///
/// Lit `/sys/class/hwmon/*/temp*_input` (millidegrés). On prend le **maximum**
/// des capteurs plutôt qu'un capteur nommé : les noms varient d'une plateforme
/// à l'autre (`coretemp` sur Intel, `k10temp` sur AMD, `cpu_thermal` sur
/// Raspberry Pi, `soc_thermal` sur bien des SBC) et un serveur audio tourne sur
/// tout ça. Le maximum est aussi la grandeur qui décide : c'est le point le
/// plus chaud qui éteint une machine, pas la moyenne.
#[cfg(target_os = "linux")]
fn cpu_temp_celsius() -> Option<f64> {
    let mut hottest: Option<f64> = None;
    let entries = std::fs::read_dir("/sys/class/hwmon").ok()?;
    for hwmon in entries.flatten() {
        let Ok(sensors) = std::fs::read_dir(hwmon.path()) else {
            continue;
        };
        for sensor in sensors.flatten() {
            let name = sensor.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("temp") && name.ends_with("_input")) {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(sensor.path())
                && let Some(c) = parse_millidegrees(&raw)
            {
                hottest = Some(hottest.map_or(c, |h: f64| h.max(c)));
            }
        }
    }
    hottest
}

#[cfg(not(target_os = "linux"))]
fn cpu_temp_celsius() -> Option<f64> {
    None
}

/// `"57000\n"` → `57.0` °C. Rejette les valeurs hors du domaine physique d'un
/// capteur CPU : certains hwmon exposent des sondes de tension ou des
/// sentinelles (0, valeurs négatives absurdes) dans le même format, et les
/// prendre pour des degrés ferait taire la passe pour rien — ou, pire, la
/// laisserait tourner sur un capteur muet.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
}
