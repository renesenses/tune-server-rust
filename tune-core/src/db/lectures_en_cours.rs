//! Les lectures SQLite en cours, pour le relevé d'un gel de l'exécuteur
//! (#5677).
//!
//! Le relevé de #4924 dit qui tient le verrou d'ÉCRITURE. Il ne disait rien
//! des lectures : or, dans le rapport du 03/10 (ticket 224), deux lectures de
//! 12,6 et 11,8 s (`attente_ms=0`) couvrent presque tout un gel, et rien ne
//! dit sur quel fil elles tournaient, ni si elles tournaient encore quand le
//! battement s'est arrêté. `slow_query` ne parle qu'APRÈS la fin de la
//! requête.
//!
//! Chaque lecture passée par `query_one` / `query_many` (le chemin de tous
//! les dépôts) s'inscrit ici le temps de son exécution : le `tid` du fil, le
//! début, l'extrait du SQL (texte de la requête avec ses `?`, jamais les
//! valeurs liées). Le relevé du gel lit la liste depuis son propre fil.
//!
//! Coût : une prise de mutex sans contention et une petite allocation par
//! lecture, à côté d'une requête SQLite. Rien n'est gardé une fois la
//! lecture finie.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Longueur gardée de l'extrait SQL : de quoi reconnaître la requête, comme
/// `slow_query`.
pub const EXTRAIT_SQL: usize = 160;

struct Lecture {
    tid: i64,
    fil: String,
    debut: Instant,
    /// Quand la connexion a été obtenue ; `None` : la lecture attend encore
    /// une connexion du pool.
    execution: Option<Instant>,
    sql: String,
}

fn registre() -> &'static Mutex<HashMap<u64, Lecture>> {
    static R: OnceLock<Mutex<HashMap<u64, Lecture>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

static PROCHAIN: AtomicU64 = AtomicU64::new(1);

/// Une lecture inscrite ; elle se désinscrit en tombant (y compris sur une
/// erreur ou une panique de la requête).
pub struct Inscrite(u64);

impl Inscrite {
    /// La connexion est obtenue : la lecture passe de l'attente à l'exécution.
    pub fn executer(&self) {
        if let Ok(mut r) = registre().lock()
            && let Some(l) = r.get_mut(&self.0)
        {
            l.execution = Some(Instant::now());
        }
    }
}

impl Drop for Inscrite {
    fn drop(&mut self) {
        if let Ok(mut r) = registre().lock() {
            r.remove(&self.0);
        }
    }
}

/// Inscrire la lecture de `sql` qui commence sur le fil courant.
pub fn inscrire(sql: &str) -> Inscrite {
    let jeton = PROCHAIN.fetch_add(1, Ordering::Relaxed);
    let lecture = Lecture {
        tid: super::verrou_ecriture::tid_courant(),
        fil: std::thread::current().name().unwrap_or("?").to_string(),
        debut: Instant::now(),
        execution: None,
        sql: sql.chars().take(EXTRAIT_SQL * 2).collect(),
    };
    if let Ok(mut r) = registre().lock() {
        r.insert(jeton, lecture);
    }
    Inscrite(jeton)
}

/// Une lecture en cours, telle que le relevé la montre.
#[derive(Debug, Clone)]
pub struct LectureEnCours {
    pub tid: i64,
    pub fil: String,
    /// Depuis l'inscription (attente d'une connexion comprise).
    pub depuis: Duration,
    /// Temps d'exécution, connexion obtenue ; `None` : attend encore une
    /// connexion du pool (le fil dort, il ne consomme rien).
    pub execution: Option<Duration>,
    /// SQL replié sur une ligne et tronqué à [`EXTRAIT_SQL`] caractères.
    pub sql: String,
}

/// Les lectures en cours, la plus ancienne d'abord. `try_lock` : le relevé
/// d'un gel ne doit jamais attendre (le mutex n'est tenu que le temps d'une
/// insertion ; s'il est pris, on réessaie quelques fois).
pub fn en_cours() -> Option<Vec<LectureEnCours>> {
    let mut garde = None;
    for _ in 0..20 {
        if let Ok(g) = registre().try_lock() {
            garde = Some(g);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let r = garde?;
    let mut v: Vec<LectureEnCours> = r
        .values()
        .map(|l| LectureEnCours {
            tid: l.tid,
            fil: l.fil.clone(),
            depuis: l.debut.elapsed(),
            execution: l.execution.map(|t| t.elapsed()),
            sql: l
                .sql
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(EXTRAIT_SQL)
                .collect(),
        })
        .collect();
    drop(r);
    v.sort_by_key(|l| std::cmp::Reverse(l.depuis));
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_lecture_est_visible_pendant_qu_elle_tourne_puis_disparait() {
        let marque = "SELECT 5677 /* lectures_en_cours */";
        let inscrite = inscrire(marque);
        std::thread::sleep(Duration::from_millis(20));
        let vues = en_cours().expect("registre lisible");
        let la_mienne = vues
            .iter()
            .find(|l| l.sql.contains("5677 /* lectures_en_cours"))
            .expect("la lecture en cours doit être listée");
        assert!(la_mienne.depuis >= Duration::from_millis(20));
        assert!(la_mienne.execution.is_none(), "pas encore de connexion");
        inscrite.executer();
        let vues = en_cours().expect("registre lisible");
        let la_mienne = vues
            .iter()
            .find(|l| l.sql.contains("5677 /* lectures_en_cours"))
            .unwrap();
        assert!(la_mienne.execution.is_some(), "connexion obtenue");
        drop(inscrite);
        let vues = en_cours().expect("registre lisible");
        assert!(
            !vues
                .iter()
                .any(|l| l.sql.contains("5677 /* lectures_en_cours")),
            "une lecture finie ne doit plus être listée"
        );
    }

    #[test]
    fn l_extrait_est_replie_et_tronque() {
        let long = format!(
            "SELECT\n  a,\n  b FROM t WHERE x = ?1 /* 5677-tronque */ {}",
            "y".repeat(500)
        );
        let _i = inscrire(&long);
        let vues = en_cours().unwrap();
        let l = vues
            .iter()
            .find(|l| l.sql.contains("5677-tronque"))
            .unwrap();
        assert!(!l.sql.contains('\n'));
        assert!(l.sql.chars().count() <= EXTRAIT_SQL);
        assert!(l.sql.starts_with("SELECT a, b FROM t"));
    }
}
