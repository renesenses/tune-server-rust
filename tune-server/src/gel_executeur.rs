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
//!   `gel-executeur-<horodatage>.txt` dans `diagnostics/gels/` du dossier de
//!   données (voir [`dossier_des_releves`]), PUIS le dit en WARN
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
//!
//! **Où vont les relevés (fil 2117/2124, #5677).** Ils allaient à côté du
//! journal, et sans journal dans `/tmp/tune-gel-executeur-<uid>/`. Sur Tune OS
//! le service a `PrivateTmp=yes` et `ProtectHome=yes` : pas de journal dans
//! `$HOME`, donc le `/tmp` privé du service — invisible hors du service et
//! EFFACÉ à chaque redémarrage. Le testeur a perdu ses relevés. Ils vont
//! désormais dans `<dossier de données>/diagnostics/gels/` : `TUNE_DATA_DIR`,
//! sinon le dossier de la base réellement ouverte ; le dossier temporaire
//! n'est plus qu'un repli quand celui-là n'est pas inscriptible. Sur le disque,
//! seuls les [`PLAFOND_RELEVES`] plus récents sont gardés.

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

/// Plafond de fichiers de relevé, par processus ET sur le disque : un serveur
/// qui gèlerait en boucle ne doit pas remplir le disque, et le dossier de
/// données, lui, survit aux redémarrages — seuls les plus récents y restent.
pub const PLAFOND_RELEVES: usize = 30;

/// Sous-dossier du dossier de données qui reçoit les relevés.
pub const SOUS_DOSSIER_DES_RELEVES: &str = "diagnostics/gels";

const PREFIXE_RELEVE: &str = "gel-executeur-";
const SUFFIXE_RELEVE: &str = ".txt";

/// Le dossier de données de Tune, résolu comme celui de la base : la variable
/// `TUNE_DATA_DIR` quand elle est posée et non vide, sinon le dossier de
/// `db_path` (la base réellement ouverte, relative au répertoire courant
/// `cwd` si elle l'est). Fonction pure : l'environnement lui est passé.
pub fn dossier_de_donnees(tune_data_dir: Option<&str>, db_path: &str, cwd: &Path) -> PathBuf {
    if let Some(d) = tune_data_dir.filter(|d| !d.trim().is_empty()) {
        let d = PathBuf::from(d);
        return if d.is_absolute() { d } else { cwd.join(d) };
    }
    let base = Path::new(db_path);
    let base = if base.is_absolute() {
        base.to_path_buf()
    } else {
        cwd.join(base)
    };
    match base.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => cwd.to_path_buf(),
    }
}

/// Le dossier qui recevra les relevés : `<donnees>/diagnostics/gels` s'il
/// est inscriptible (créé et sondé par l'écriture d'un fichier), sinon
/// `repli` — le dossier temporaire par compte.
pub fn dossier_des_releves(donnees: &Path, repli: PathBuf) -> PathBuf {
    let voulu = donnees.join(SOUS_DOSSIER_DES_RELEVES);
    match sonder_l_ecriture(&voulu) {
        Ok(()) => voulu,
        Err(e) => {
            tracing::warn!(
                voulu = %voulu.display(),
                repli = %repli.display(),
                error = %e,
                "gel_executeur_dossier_de_donnees_non_inscriptible"
            );
            repli
        }
    }
}

fn sonder_l_ecriture(dossier: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dossier)?;
    let sonde = dossier.join(format!(".sonde-{}", std::process::id()));
    std::fs::write(&sonde, b"")?;
    let _ = std::fs::remove_file(&sonde);
    Ok(())
}

/// Ne garder dans `dossier` que les `garder` relevés les plus récents (le nom
/// porte l'horodatage UTC, l'ordre des noms est celui du temps). Seuls les
/// fichiers `gel-executeur-*.txt` sont touchés.
pub fn elaguer_les_releves(dossier: &Path, garder: usize) {
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return;
    };
    let mut releves: Vec<PathBuf> = entrees
        .flatten()
        .filter(|e| {
            let nom = e.file_name();
            let nom = nom.to_string_lossy();
            nom.starts_with(PREFIXE_RELEVE) && nom.ends_with(SUFFIXE_RELEVE)
        })
        .map(|e| e.path())
        .collect();
    if releves.len() <= garder {
        return;
    }
    releves.sort();
    let trop = releves.len() - garder;
    for vieux in &releves[..trop] {
        let _ = std::fs::remove_file(vieux);
    }
}

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

/// Démarrer la veille de production ; `dossier` vient de [`dossier_des_releves`].
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
    let chemin = dossier.join(format!("{PREFIXE_RELEVE}{horodatage}{SUFFIXE_RELEVE}"));
    std::fs::create_dir_all(dossier).map_err(|e| e.to_string())?;
    std::fs::write(&chemin, texte).map_err(|e| e.to_string())?;
    elaguer_les_releves(dossier, PLAFOND_RELEVES);
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
        tune_core::version(),
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

    /// Fil 2117/2124 : `TUNE_DATA_DIR` d'abord (Tune OS le pose), une valeur
    /// vide ne compte pas, puis le dossier de la base réellement ouverte.
    #[test]
    fn le_dossier_de_donnees_suit_tune_data_dir_puis_la_base() {
        let cwd = Path::new("/opt/tune");
        assert_eq!(
            dossier_de_donnees(Some("/opt/tune/data"), "/ailleurs/tune.db", cwd),
            PathBuf::from("/opt/tune/data")
        );
        assert_eq!(
            dossier_de_donnees(Some("  "), "/var/lib/tune/tune.db", cwd),
            PathBuf::from("/var/lib/tune")
        );
        assert_eq!(
            dossier_de_donnees(None, "/data/tune.db", cwd),
            PathBuf::from("/data")
        );
        assert_eq!(
            dossier_de_donnees(None, "tune.db", Path::new("/var/lib/tune")),
            PathBuf::from("/var/lib/tune")
        );
        assert_eq!(
            dossier_de_donnees(Some("donnees"), "tune.db", cwd),
            PathBuf::from("/opt/tune/donnees")
        );
    }

    /// Le cas du testeur : un dossier de données inscriptible reçoit les
    /// relevés dans `diagnostics/gels/`, et le repli temporaire reste vide.
    #[test]
    fn un_dossier_de_donnees_inscriptible_recoit_les_releves() {
        let donnees = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let repli = tmp.path().join("tune-gel-executeur-0");
        let choisi = dossier_des_releves(donnees.path(), repli.clone());
        assert_eq!(choisi, donnees.path().join("diagnostics").join("gels"));
        assert!(choisi.is_dir());
        assert!(
            fichiers(&choisi).is_empty(),
            "la sonde ne doit rien laisser"
        );
        let ecrit = ecrire_le_releve(&choisi, "gel_executeur — témoin").unwrap();
        assert!(ecrit.starts_with(donnees.path()), "{}", ecrit.display());
        assert!(!repli.exists(), "le repli temporaire ne sert pas ici");
    }

    /// Un dossier de données qu'on ne peut pas créer (un FICHIER à sa place :
    /// le refus tient aussi en root) : repli sur le dossier temporaire.
    #[test]
    fn un_dossier_de_donnees_non_inscriptible_replie_sur_le_temporaire() {
        let tmp = tempfile::tempdir().unwrap();
        let donnees = tmp.path().join("donnees");
        std::fs::write(&donnees, b"pas un dossier").unwrap();
        let repli = tmp.path().join("tune-gel-executeur-0");
        assert_eq!(dossier_des_releves(&donnees, repli.clone()), repli);
    }

    /// Le dossier de données survit aux redémarrages : il ne doit garder que
    /// les PLAFOND_RELEVES relevés les plus récents, et rien d'autre n'y est
    /// effacé.
    #[test]
    fn le_dossier_des_releves_reste_borne_aux_plus_recents() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..PLAFOND_RELEVES + 5 {
            let nom = format!("gel-executeur-20260101T0000{i:02}.000Z.txt");
            std::fs::write(dir.path().join(nom), "ancien").unwrap();
        }
        std::fs::write(dir.path().join("note.txt"), "à garder").unwrap();
        let neuf = ecrire_le_releve(dir.path(), "gel_executeur — neuf").unwrap();
        let mut releves: Vec<String> = fichiers(dir.path())
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .filter(|n| n.starts_with("gel-executeur-"))
            .collect();
        releves.sort();
        assert_eq!(releves.len(), PLAFOND_RELEVES, "{releves:?}");
        assert!(neuf.exists(), "le relevé neuf est gardé");
        // Les six plus anciens (00 à 05) sont partis.
        assert_eq!(releves[0], "gel-executeur-20260101T000006.000Z.txt");
        assert!(dir.path().join("note.txt").exists());
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
