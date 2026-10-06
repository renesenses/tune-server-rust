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
//!
//! **Les angles morts comblés pour #5677.** Le relevé ne disait ni l'état de
//! la machine (charge, pression, échange, E/S, quota du cgroup), ni lesquels
//! des fils `tokio-runtime-w` étaient des fils de travail pris, ni les
//! lectures SQLite en cours — seulement le verrou d'écriture. Et il restait
//! sur la machine : le rapport joint au ticket ne le portait pas. Désormais :
//! - le seuil passe à 5 s ;
//! - le relevé porte l'état du moteur tokio ([`travailleurs`], métriques
//!   stables), les lectures SQLite en cours
//!   ([`tune_core::db::lectures_en_cours`]), et deux photos de la machine à
//!   une seconde d'écart prises pendant le gel ([`systeme`]), avec la pile
//!   noyau des fils en cause quand les droits le permettent ;
//! - le WARN `gel_executeur_detecte` porte un résumé d'une ligne ;
//! - le rapport de bogue (`diagnostic.md`) joint les derniers relevés
//!   ([`derniers_releves`]) ;
//! - la capture des piles du détenteur du verrou d'écriture est armée dès le
//!   premier gel.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub mod systeme;
pub mod travailleurs;

/// Cadence du battement.
pub const PERIODE_BATTEMENT: Duration = Duration::from_millis(500);

/// Âge du battement au-delà duquel l'exécuteur est dit gelé. Cinq secondes
/// (dix jusqu'à #5677) : aucune tâche légitime ne garde TOUS les fils aussi
/// longtemps, et les coupures de Tune OS commencent à 5 s de silence. Une
/// machine à genoux (charge, échange mémoire) peut désormais déclencher un
/// relevé : c'est voulu, la section « machine » le dira.
pub const SEUIL_GEL: Duration = Duration::from_secs(5);

/// Écart entre les deux photos de la machine prises pendant un gel.
pub const FENETRE_DE_MESURE: Duration = Duration::from_secs(1);

/// Taille au-delà de laquelle un relevé joint au rapport de bogue est coupé.
pub const PLAFOND_RELEVE_JOINT: usize = 24 * 1024;

static DOSSIER_RETENU: OnceLock<PathBuf> = OnceLock::new();

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
    handle: tokio::runtime::Handle,
    fenetre: Duration,
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
            handle: handle.clone(),
            fenetre: FENETRE_DE_MESURE,
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
        // Le gel suivant portera la pile du détenteur du verrou d'écriture.
        tune_core::db::verrou_ecriture::armer_les_piles();
        let (texte, resume) = rapport_complet(age, Some(&self.handle), self.fenetre);
        let chemin = ecrire_le_releve(&self.dossier, &texte);
        tracing::warn!(
            age_battement_ms = age.as_millis() as u64,
            releve = %chemin.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|e| format!("non écrit : {e}")),
            resume = %resume,
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
    let _ = DOSSIER_RETENU.set(dossier.clone());
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

/// Le texte du relevé, sans les sections qui demandent le moteur ou une
/// fenêtre de mesure. Public pour les témoins.
pub fn rapport(age_battement: Duration) -> String {
    rapport_complet(age_battement, None, Duration::ZERO).0
}

/// Le relevé complet et son résumé d'une ligne. `handle` : le moteur dont on
/// lit les métriques ; `fenetre` : l'écart entre les deux photos de la
/// machine (zéro : une seule photo, sans débits).
pub fn rapport_complet(
    age_battement: Duration,
    handle: Option<&tokio::runtime::Handle>,
    fenetre: Duration,
) -> (String, String) {
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
    // Les deux photos de la machine d'abord : l'état noyau de chaque fil sert
    // aux sections qui suivent.
    let photos = if fenetre.is_zero() {
        let p = systeme::photo();
        systeme::Fenetre {
            avant: p.clone(),
            apres: p,
        }
    } else {
        systeme::fenetre(fenetre)
    };
    // Les tid que le relevé met en cause : leur pile noyau sera lue.
    let mut designes: Vec<i64> = Vec::new();

    let _ = writeln!(s, "\n== verrou d'écriture SQLite ==");
    let releves = tune_core::db::verrou_ecriture::Sentinelle::globale().releves();
    if releves.is_empty() {
        let _ = writeln!(s, "aucun verrou SQLite ouvert (moteur PostgreSQL ?)");
    }
    let mut ecriture = String::from("libre");
    for r in releves {
        if let Some(d) = &r.detenteur {
            designes.push(d.tid);
            ecriture = format!("{}@tid{}/{}ms", d.lieu, d.tid, d.depuis.as_millis());
        }
        designes.extend(r.attentes.iter().map(|a| a.tid));
        let _ = write!(s, "{r}");
    }
    let _ = writeln!(
        s,
        "transaction de scan :{}",
        tune_core::db::tx_holder::mention()
    );

    let _ = writeln!(
        s,
        "\n== lectures SQLite en cours (tid, fil, état, SQL ; « attend » = pas encore de connexion) =="
    );
    let mut lecture_max = 0u128;
    match tune_core::db::lectures_en_cours::en_cours() {
        None => {
            let _ = writeln!(s, "registre occupé, non lu");
        }
        Some(v) if v.is_empty() => {
            let _ = writeln!(s, "aucune");
        }
        Some(v) => {
            let en_attente = v.iter().filter(|l| l.execution.is_none()).count();
            let _ = writeln!(
                s,
                "{} en exécution, {en_attente} en attente d'une connexion",
                v.len() - en_attente
            );
            for l in v.iter().take(32) {
                designes.push(l.tid);
                let etat = match l.execution {
                    Some(e) => {
                        lecture_max = lecture_max.max(e.as_millis());
                        format!(
                            "exécute depuis {} ms (attente {} ms)",
                            e.as_millis(),
                            l.depuis.saturating_sub(e).as_millis()
                        )
                    }
                    None => format!("attend depuis {} ms", l.depuis.as_millis()),
                };
                let _ = writeln!(s, "{}\t{}\t{etat}\t{}", l.tid, l.fil, l.sql);
            }
            if v.len() > 32 {
                let _ = writeln!(s, "… et {} de plus", v.len() - 32);
            }
        }
    }

    let _ = writeln!(s, "\n== moteur tokio ==");
    let mut actifs = String::from("?");
    if let Some(h) = handle {
        let m = h.metrics();
        let n = m.num_workers();
        let au_travail = (0..n)
            .filter(|&w| m.worker_park_unpark_count(w) % 2 == 0)
            .count();
        actifs = format!("{au_travail}/{n}");
        let _ = writeln!(
            s,
            "fils de travail : {n} dont {au_travail} au travail ; tâches vivantes : {} ; file globale : {}",
            m.num_alive_tasks(),
            m.global_queue_depth()
        );
    }
    let travailleurs = travailleurs::etat();
    if travailleurs.is_empty() {
        let _ = writeln!(s, "crochets de garage non posés (moteur de test ?)");
    }
    if !travailleurs.is_empty() {
        let _ = writeln!(
            s,
            "(un fil passé par `block_in_place` reste « au travail » ici : son état noyau, \
             entre crochets, le dément s'il dort)"
        );
    }
    for t in &travailleurs {
        let noyau = photos
            .apres
            .fils
            .get(&t.id)
            .map(|f| format!(" [{} {}]", f.etat, f.wchan))
            .unwrap_or_default();
        match t.au_travail_depuis {
            Some(d) => {
                if d >= Duration::from_secs(1) {
                    designes.push(t.id);
                }
                let poll = t
                    .poll
                    .as_ref()
                    .map(|(r, d)| format!(" — poll en cours : {r} depuis {} ms", d.as_millis()))
                    .unwrap_or_default();
                let _ = writeln!(
                    s,
                    "fil {} : au travail depuis {} ms sans se garer{noyau}{poll}",
                    t.id,
                    d.as_millis()
                );
            }
            None => {
                let _ = writeln!(s, "fil {} : garé{noyau}", t.id);
            }
        }
    }

    designes.sort_unstable();
    designes.dedup();
    designes.retain(|t| *t > 0);
    let (machine, resume) = systeme::section(&photos, &designes);
    s.push_str(&machine);
    if !cfg!(target_os = "linux") {
        s.push_str("(état de la machine et liste des fils : Linux seulement)\n");
    }
    let resume = format!(
        "travailleurs_au_travail={actifs} ecriture_sqlite={ecriture} lecture_la_plus_longue_ms={lecture_max} {resume}"
    );
    (s, resume)
}

/// Le dossier des relevés retenu au démarrage, s'il y en a un.
pub fn dossier_retenu() -> Option<&'static Path> {
    DOSSIER_RETENU.get().map(PathBuf::as_path)
}

/// Les `n` relevés les plus récents de `dossier`, le plus récent d'abord :
/// (nom du fichier, texte coupé à [`PLAFOND_RELEVE_JOINT`] octets), et le
/// nombre total de relevés présents.
pub fn derniers_releves(dossier: &Path, n: usize) -> (Vec<(String, String)>, usize) {
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return (Vec::new(), 0);
    };
    let mut noms: Vec<String> = entrees
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|nom| nom.starts_with(PREFIXE_RELEVE) && nom.ends_with(SUFFIXE_RELEVE))
        .collect();
    let total = noms.len();
    noms.sort();
    noms.reverse();
    let releves = noms
        .into_iter()
        .take(n)
        .map(|nom| {
            let mut texte = std::fs::read_to_string(dossier.join(&nom)).unwrap_or_default();
            if texte.len() > PLAFOND_RELEVE_JOINT {
                let mut coupe = PLAFOND_RELEVE_JOINT;
                while !texte.is_char_boundary(coupe) {
                    coupe -= 1;
                }
                texte.truncate(coupe);
                texte.push_str("\n… (coupé)\n");
            }
            (nom, texte)
        })
        .collect();
    (releves, total)
}

/// La section « gels de l'exécuteur » du rapport de bogue : les relevés
/// restaient sur la machine du testeur (ticket 223, 224), le rapport les
/// porte désormais.
pub fn section_du_rapport(dossier: Option<&Path>, n: usize) -> String {
    use std::fmt::Write;
    let mut md = String::from("## Gels de l'exécuteur (relevés automatiques, #4924/#5677)\n");
    let Some(dossier) = dossier else {
        md.push_str("- veille non démarrée\n\n");
        return md;
    };
    let (releves, total) = derniers_releves(dossier, n);
    let _ = writeln!(
        md,
        "- seuil : {} s ; fils de travail : {} ; relevés présents : {total}",
        SEUIL_GEL.as_secs(),
        crate::fils_de_travail::retenu()
    );
    if releves.is_empty() {
        md.push_str("- aucun gel relevé\n\n");
        return md;
    }
    for (nom, texte) in releves {
        let _ = writeln!(md, "\n### {nom}\n```\n{}\n```", texte.trim_end());
    }
    md.push('\n');
    md
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
        // 4 s : le seuil (0,5 s), puis la fenêtre de mesure (1 s) pendant le gel.
        rt.spawn(async { std::thread::sleep(Duration::from_millis(4000)) });
        let debut = Instant::now();
        let mut trouve = Vec::new();
        while debut.elapsed() < Duration::from_millis(3500) && trouve.is_empty() {
            std::thread::sleep(Duration::from_millis(50));
            trouve = fichiers(dir.path());
        }
        assert_eq!(trouve.len(), 1, "un relevé attendu PENDANT le gel");
        let texte = std::fs::read_to_string(&trouve[0]).unwrap();
        assert!(texte.contains("gel_executeur"), "{texte}");
        assert!(texte.contains("verrou d'écriture SQLite"), "{texte}");
        // #5677 : les sections qui manquaient.
        assert!(texte.contains("== lectures SQLite en cours"), "{texte}");
        assert!(
            texte.contains("fils de travail : 1 dont 1 au travail"),
            "le moteur doit se dire pris : {texte}"
        );
        assert!(texte.contains("== machine =="), "{texte}");
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

    /// #5677 : une lecture SQLite en cours figure au relevé, avec son fil.
    #[test]
    fn une_lecture_sqlite_en_cours_figure_au_releve() {
        let _l = tune_core::db::lectures_en_cours::inscrire("SELECT 5677 AS temoin_releve");
        let texte = rapport(Duration::from_secs(6));
        let section = texte
            .split("== lectures SQLite en cours")
            .nth(1)
            .expect("section des lectures");
        assert!(section.contains("SELECT 5677 AS temoin_releve"), "{texte}");
    }

    /// #5677 : le rapport de bogue joint les relevés les plus récents, coupés
    /// au plafond, et dit combien il y en a.
    #[test]
    fn le_rapport_de_bogue_joint_les_derniers_releves() {
        let dir = tempfile::tempdir().unwrap();
        assert!(section_du_rapport(Some(dir.path()), 2).contains("aucun gel relevé"));
        for (i, corps) in ["premier", "deuxieme", "troisieme"].iter().enumerate() {
            let nom = format!("gel-executeur-20261005T1000{i:02}.000Z.txt");
            std::fs::write(dir.path().join(nom), format!("gel_executeur — {corps}")).unwrap();
        }
        let long = "x".repeat(PLAFOND_RELEVE_JOINT + 500);
        std::fs::write(
            dir.path().join("gel-executeur-20261005T100009.000Z.txt"),
            &long,
        )
        .unwrap();
        let md = section_du_rapport(Some(dir.path()), 2);
        assert!(md.contains("relevés présents : 4"), "{md}");
        assert!(
            md.contains("gel-executeur-20261005T100009.000Z.txt"),
            "{md}"
        );
        assert!(md.contains("troisieme"), "{md}");
        assert!(!md.contains("deuxieme"), "seuls les 2 plus récents : {md}");
        assert!(md.contains("… (coupé)"), "{md}");
        assert!(md.len() < 2 * PLAFOND_RELEVE_JOINT, "{}", md.len());
        assert!(section_du_rapport(None, 2).contains("veille non démarrée"));
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
