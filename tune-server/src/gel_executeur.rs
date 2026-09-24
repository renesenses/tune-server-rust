//! Détecteur de gel de l'exécuteur tokio, et relevé automatique (#4924).
//!
//! Sur le .18, les gels durent de 10 à 30 minutes et se referment seuls.
//! `gdb` y exige `sudo`, que seul Bertrand peut taper : la pile prise APRÈS
//! le retour montrait un exécuteur sain, et n'a rien appris. Il faut que le
//! serveur relève lui-même son état PENDANT le gel.
//!
//! Deux pièces, et la seconde ne dépend jamais de l'exécuteur :
//!
//! - un **battement** : une tâche tokio qui date un compteur toutes les
//!   [`PERIODE_BATTEMENT`]. Tant que l'exécuteur tourne — un fil libre, des
//!   minuteries qui partent — le battement est frais ;
//! - une **veille** sur un `std::thread` à elle, qui regarde l'âge du
//!   battement. Au-delà de [`SEUIL_GEL`], elle écrit un fichier
//!   `gel-executeur-<horodatage>.txt` à côté du journal, PUIS le dit en WARN
//!   (le fichier d'abord : si c'est le journal qui est pris, le fichier sort
//!   quand même). Le relevé est repris à 60 s et à 5 min du même gel, pour
//!   voir s'il change, et la fin du gel est dite avec sa durée.
//!
//! Le relevé porte : le détenteur du verrou d'écriture SQLite et ceux qui
//! l'attendent ([`tune_core::db::verrou_ecriture`], avec la pile du
//! détenteur une fois les piles armées), la transaction de scan déclarée
//! ([`tune_core::db::tx_holder`]), et, sous Linux, chaque fil du processus
//! avec son état, son `wchan` et son temps processeur — sans `ptrace`, par
//! `/proc/self/task`. Le `tid` du détenteur se retrouve dans cette liste :
//! son `wchan` dit ce qu'IL attend.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Cadence du battement.
pub const PERIODE_BATTEMENT: Duration = Duration::from_millis(500);

/// Âge du battement au-delà duquel l'exécuteur est dit gelé. Dix secondes :
/// aucune tâche légitime ne garde TOUS les fils aussi longtemps, et une
/// pause du processeur (charge, échange mémoire) reste en deçà.
pub const SEUIL_GEL: Duration = Duration::from_secs(10);

/// Âges du gel auxquels le relevé est (re)pris.
const PALIERS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_secs(60),
    Duration::from_secs(300),
];

/// Plafond de fichiers de relevé par processus : un serveur qui gèlerait en
/// boucle ne doit pas remplir le disque.
const PLAFOND_RELEVES: usize = 30;

/// La veille : son battement et ses réglages.
pub struct Veille {
    origine: Instant,
    battement_ms: AtomicU64,
    seuil: Duration,
    dossier: PathBuf,
}

impl Veille {
    /// Démarrer le battement sur l'exécuteur `handle`, et la veille sur son
    /// propre fil. `dossier` reçoit les relevés.
    pub fn demarrer(
        handle: &tokio::runtime::Handle,
        seuil: Duration,
        periode_veille: Duration,
        dossier: PathBuf,
    ) -> Arc<Veille> {
        let veille = Arc::new(Veille {
            origine: Instant::now(),
            battement_ms: AtomicU64::new(0),
            seuil,
            dossier,
        });
        let pour_le_battement = Arc::downgrade(&veille);
        handle.spawn(async move {
            let mut cadence = tokio::time::interval(PERIODE_BATTEMENT);
            cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                cadence.tick().await;
                let Some(v) = pour_le_battement.upgrade() else {
                    return;
                };
                v.battre();
            }
        });
        let pour_la_veille = Arc::downgrade(&veille);
        let _ = std::thread::Builder::new()
            .name("tune-gel-veille".into())
            .spawn(move || {
                let mut episode: Option<Episode> = None;
                let mut releves = 0usize;
                loop {
                    std::thread::sleep(periode_veille);
                    let Some(v) = pour_la_veille.upgrade() else {
                        return;
                    };
                    v.tour(&mut episode, &mut releves);
                }
            });
        veille
    }

    fn battre(&self) {
        self.battement_ms
            .store(self.origine.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn age_du_battement(&self) -> Duration {
        let dernier = Duration::from_millis(self.battement_ms.load(Ordering::Relaxed));
        self.origine.elapsed().saturating_sub(dernier)
    }

    fn tour(&self, episode: &mut Option<Episode>, releves: &mut usize) {
        let age = self.age_du_battement();
        if age < self.seuil {
            if let Some(e) = episode.take() {
                tracing::warn!(
                    duree_ms = e.debut.elapsed().as_millis() as u64 + self.seuil.as_millis() as u64,
                    releves = e.paliers_faits,
                    "gel_executeur_termine"
                );
            }
            return;
        }
        let e = episode.get_or_insert_with(|| Episode {
            debut: Instant::now(),
            paliers_faits: 0,
        });
        let Some(palier) = PALIERS.get(e.paliers_faits) else {
            return;
        };
        if e.debut.elapsed() < *palier {
            return;
        }
        e.paliers_faits += 1;
        if *releves >= PLAFOND_RELEVES {
            return;
        }
        *releves += 1;
        let texte = rapport(age);
        let chemin = ecrire_le_releve(&self.dossier, &texte);
        tracing::warn!(
            age_battement_ms = age.as_millis() as u64,
            releve = %chemin.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|e| format!("non écrit : {e}")),
            "gel_executeur_detecte"
        );
    }
}

struct Episode {
    debut: Instant,
    paliers_faits: usize,
}

/// Démarrer la veille de production, relevés à côté du journal.
pub fn demarrer_en_production(dossier: PathBuf) -> Arc<Veille> {
    Veille::demarrer(
        &tokio::runtime::Handle::current(),
        SEUIL_GEL,
        Duration::from_secs(1),
        dossier,
    )
}

fn ecrire_le_releve(dossier: &Path, texte: &str) -> Result<PathBuf, String> {
    let horodatage = horodatage().replace([':', '-'], "");
    let chemin = dossier.join(format!("gel-executeur-{horodatage}.txt"));
    std::fs::create_dir_all(dossier).map_err(|e| e.to_string())?;
    std::fs::write(&chemin, texte).map_err(|e| e.to_string())?;
    Ok(chemin)
}

fn horodatage() -> String {
    let format = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    time::OffsetDateTime::now_utc()
        .format(&format)
        .unwrap_or_else(|_| "?".into())
}

/// Le texte du relevé. Public pour la route de diagnostic et les témoins.
pub fn rapport(age_battement: Duration) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "gel_executeur — {} — battement vieux de {} ms (pid {}, version {})",
        horodatage(),
        age_battement.as_millis(),
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
    );
    let _ = writeln!(s, "\n== verrou d'écriture SQLite ==");
    let releves = tune_core::db::verrou_ecriture::Sentinelle::globale().releves();
    if releves.is_empty() {
        let _ = writeln!(s, "aucun verrou SQLite ouvert (moteur PostgreSQL ?)");
    }
    for r in releves {
        let _ = write!(s, "{r}");
    }
    let _ = writeln!(
        s,
        "transaction de scan :{}",
        tune_core::db::tx_holder::mention()
    );
    let _ = writeln!(
        s,
        "\n== fils du processus (tid, nom, état, wchan, utime+stime en tops) =="
    );
    s.push_str(&fils_du_processus());
    s
}

/// Chaque fil du processus, lu dans `/proc/self/task` — sans `ptrace`.
#[cfg(target_os = "linux")]
pub fn fils_du_processus() -> String {
    let mut lignes = Vec::new();
    let Ok(taches) = std::fs::read_dir("/proc/self/task") else {
        return "/proc/self/task illisible\n".into();
    };
    for t in taches.flatten() {
        let p = t.path();
        let tid = t.file_name().to_string_lossy().to_string();
        let lire = |f: &str| {
            std::fs::read_to_string(p.join(f))
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        let nom = lire("comm");
        let wchan = lire("wchan");
        let stat = lire("stat");
        // Après le « (nom) » : état, puis les champs numérotés de proc(5) —
        // utime et stime sont les 14e et 15e, soit les 12e et 13e après l'état.
        let apres = stat.rsplit_once(')').map(|(_, r)| r.trim()).unwrap_or("");
        let champs: Vec<&str> = apres.split_whitespace().collect();
        let etat = champs.first().copied().unwrap_or("?");
        let cpu: u64 = [11, 12]
            .iter()
            .filter_map(|i| champs.get(*i).and_then(|v| v.parse::<u64>().ok()))
            .sum();
        lignes.push((
            tid.parse::<i64>().unwrap_or(0),
            format!("{tid}\t{nom}\t{etat}\t{wchan}\t{cpu}"),
        ));
    }
    lignes.sort_by_key(|(t, _)| *t);
    let mut s: String = lignes.into_iter().map(|(_, l)| l + "\n").collect();
    s.push_str(&format!("({} fils)\n", s.lines().count()));
    s
}

#[cfg(not(target_os = "linux"))]
pub fn fils_du_processus() -> String {
    "liste des fils disponible sous Linux seulement\n".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fichiers(dossier: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dossier)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    }

    /// Un exécuteur d'UN fil, occupé par une tâche qui ne cède pas : la
    /// veille, sur son propre fil, écrit le relevé pendant le gel.
    #[test]
    fn un_executeur_fige_laisse_un_releve_ecrit_pendant_le_gel() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let veille = Veille::demarrer(
            rt.handle(),
            Duration::from_millis(500),
            Duration::from_millis(50),
            dir.path().to_path_buf(),
        );
        std::thread::sleep(Duration::from_millis(300));
        // Le gel : l'unique fil dort sans rendre la main.
        rt.spawn(async { std::thread::sleep(Duration::from_millis(2500)) });
        let debut = Instant::now();
        let mut trouve = Vec::new();
        while debut.elapsed() < Duration::from_secs(2) && trouve.is_empty() {
            std::thread::sleep(Duration::from_millis(50));
            trouve = fichiers(dir.path());
        }
        assert_eq!(trouve.len(), 1, "un relevé attendu PENDANT le gel");
        let texte = std::fs::read_to_string(&trouve[0]).unwrap();
        assert!(texte.contains("gel_executeur"), "{texte}");
        assert!(texte.contains("verrou d'écriture SQLite"), "{texte}");
        #[cfg(target_os = "linux")]
        assert!(
            texte.contains("tune-gel-veille"),
            "la liste des fils doit y être : {texte}"
        );
        drop(veille);
        rt.shutdown_timeout(Duration::from_secs(5));
    }

    /// Un exécuteur qui tourne : aucun relevé.
    #[test]
    fn un_executeur_sain_ne_laisse_aucun_releve() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let _veille = Veille::demarrer(
            rt.handle(),
            // Seuil large : sous la charge de toute la suite, un fil peut attendre
            // son tour plusieurs centaines de ms sans que l'exécuteur soit gelé.
            Duration::from_secs(2),
            Duration::from_millis(50),
            dir.path().to_path_buf(),
        );
        std::thread::sleep(Duration::from_millis(3000));
        assert!(fichiers(dir.path()).is_empty());
        rt.shutdown_timeout(Duration::from_secs(5));
    }
}
