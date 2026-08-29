//! Registre des executions automatisees — qui a tourne, quand, combien de
//! temps, et avec quel verdict (#2080).
//!
//! Tune lance seul une vingtaine de passes : scan de demarrage, ReplayGain,
//! enrichissement, analyse acoustique, battement de coeur, nettoyages. Aucune
//! ne laissait de trace INTERROGEABLE. Le journal defile et se perd ; quand un
//! utilisateur ecrit « ca n'a rien fait », on ne peut ni le confirmer ni
//! l'infirmer. Ce module est la reponse.
//!
//! # Ce que ce registre n'est pas
//!
//! Ce n'est pas un journal de plus. Une ligne `info!` ne survit pas au
//! redemarrage, ne se filtre pas par passe, et ne se borne pas. Ici :
//!
//! * **ca survit** — c'est une table, relue apres redemarrage ;
//! * **c'est borne** — [`RETENTION_EXECUTIONS_PAR_PASSE`] par passe et
//!   [`RETENTION_JOURS`] d'age. Une table d'observabilite qui grossit sans fin
//!   finit par couter plus cher que ce qu'elle observe ;
//! * **c'est interrogeable** — [`TaskRunRepo::lister`] et
//!   [`TaskRunRepo::resume`], exposees par la route `/system/task-runs`.
//!
//! # Les deux defauts que ce registre ne reproduit pas
//!
//! **Une passe interrompue restait « en cours » a jamais** (#2002). Un
//! avancement d'enrichissement etait ecrit `running` en base, et le `done` de
//! fin etait pose APRES la boucle : un redemarrage le sautait, et le reglage
//! affirmait pour toujours qu'une passe tournait pendant que le fil qui
//! l'ecrivait n'existait plus — bouton de relance grise sur une passe morte.
//! Ici, chaque ligne porte le [`boot_id`] de l'incarnation du processus qui l'a
//! ecrite, et [`TaskRunRepo::clore_orphelines`] ferme au demarrage tout ce qui
//! reste `en_cours` sous un AUTRE boot. Aucune passe ne survit au processus qui
//! la portait : le demarrage est la seule preuve necessaire.
//!
//! **Un compteur relatif ne permet pas de verifier ce qu'il affirme**
//! (PR #2632, `uptime_seconds`). « Il y a trois heures » ne se recoupe avec
//! rien. Tous les horodatages d'ici sont **absolus**, UTC, ISO-8601, poses par
//! l'expression « maintenant » du moteur.
//!
//! La duree, elle, est mesuree sur une horloge **monotone** cote Rust et non
//! par difference des deux dates : un changement d'heure systeme pendant une
//! passe longue rendrait la soustraction absurde, negative meme.
//!
//! # Confidentialite
//!
//! Ce registre contient des **compteurs et des verdicts**, pas des donnees. Ni
//! chemin de fichier, ni cle, ni jeton. Le champ libre `detail` passe par
//! [`detail_sans_donnees`] avant toute ecriture — c'est une garde, pas une
//! excuse pour y verser n'importe quoi.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Nombre d'executions conservees PAR PASSE. Au-dela, les plus anciennes sont
/// effacees a la fin de chaque execution de cette passe.
///
/// 50 : de quoi couvrir plusieurs jours sur les passes horaires (battement de
/// coeur) et plusieurs mois sur les passes quotidiennes, tout en bornant la
/// table a ~50 lignes x nombre de passes cablees. Une ligne pese quelques
/// dizaines d'octets : le registre entier tient sous 100 Ko meme si toutes les
/// passes recensees sont un jour cablees.
pub const RETENTION_EXECUTIONS_PAR_PASSE: i64 = 50;

/// Age maximal d'une execution, en jours. Applique au demarrage.
///
/// Sans cette seconde borne, une passe cablee puis retiree laisserait ses 50
/// dernieres lignes indefiniment — et un registre qui presente comme actuel un
/// verdict vieux d'un an ment plus qu'il n'informe.
pub const RETENTION_JOURS: i64 = 30;

/// Longueur maximale du champ `detail`. Au-dela, on tronque : ce champ est un
/// verdict court, pas un journal.
pub const DETAIL_MAX: usize = 200;

// ─── Noms de passes ──────────────────────────────────────────────────────
//
// Des CONSTANTES et jamais des litteraux : l'ecrivain et le lecteur d'une
// meme passe ne peuvent pas diverger sur une faute de frappe, et la liste
// ci-dessous est le recensement executable de ce qui est cable.

/// Scan de la bibliotheque au demarrage (`tune-server/src/auto_scan.rs`).
pub const TACHE_SCAN_DEMARRAGE: &str = "scan_demarrage";

/// Analyse ReplayGain de fond (`tune-core/src/audio/replaygain.rs`).
pub const TACHE_REPLAYGAIN: &str = "replaygain";

/// Battement de coeur vers mozaiklabs.fr (`tune-server/src/background.rs`).
pub const TACHE_BATTEMENT_COEUR: &str = "battement_coeur";

/// Les passes cablees a ce jour. Sert au test de recensement et a la route de
/// lecture, qui rend une ligne « jamais executee » pour une passe connue mais
/// sans historique — l'absence de ligne serait autrement indistinguable d'une
/// passe qu'on aurait oublie de cabler.
pub const TACHES_CABLEES: [&str; 3] = [
    TACHE_SCAN_DEMARRAGE,
    TACHE_REPLAYGAIN,
    TACHE_BATTEMENT_COEUR,
];

// ─── Verdicts ────────────────────────────────────────────────────────────

/// Ce qu'une execution a donne.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// La passe tourne. Le seul etat qu'un redemarrage peut rendre mensonger —
    /// d'ou [`TaskRunRepo::clore_orphelines`].
    EnCours,
    /// Terminee, du travail a ete fait.
    Succes,
    /// Terminee, il n'y avait rien a faire. **Ce n'est pas un echec**, et c'est
    /// precisement la reponse a « ca n'a rien fait » : elle a tourne, elle n'a
    /// rien trouve.
    RienAFaire,
    /// Terminee en erreur.
    Echec,
    /// Jamais terminee : le processus qui la portait a disparu.
    Interrompu,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::EnCours => "en_cours",
            Verdict::Succes => "succes",
            Verdict::RienAFaire => "rien_a_faire",
            Verdict::Echec => "echec",
            Verdict::Interrompu => "interrompu",
        }
    }
}

// ─── Identite de l'incarnation du processus ──────────────────────────────

static BOOT_ID: OnceLock<String> = OnceLock::new();

/// Identifiant de CETTE incarnation du processus, stable pour toute sa duree
/// de vie et jamais reutilise par la suivante.
///
/// C'est la piece qui rend la passe orpheline detectable sans compteur relatif
/// ni delai de grace : une ligne `en_cours` portant un autre `boot_id` que le
/// notre a ete ecrite par un processus qui n'est plus.
///
/// Compose de l'horodatage absolu du demarrage (nanosecondes depuis l'epoque)
/// et du PID. Pas de dependance a une crate d'UUID pour ca : la paire est
/// unique en pratique — deux processus ne peuvent pas partager un PID au meme
/// instant — et elle est LISIBLE, ce qu'un UUID n'est pas.
pub fn boot_id() -> &'static str {
    BOOT_ID.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{nanos:x}-{}", std::process::id())
    })
}

// ─── Nettoyage du champ libre ────────────────────────────────────────────

/// Un jeton ressemble-t-il a une cle, un hachage ou un identifiant secret ?
///
/// Deux motifs, choisis pour ne pas mordre sur du francais ni sur du
/// `snake_case` :
///
/// * 20 caracteres ou plus AVEC au moins un chiffre ET une majuscule — la forme
///   d'une cle d'API ou d'un base64 ;
/// * 32 caracteres ou plus tout en hexadecimal — la forme d'un hachage.
fn ressemble_a_un_secret(jeton: &str) -> bool {
    let n = jeton.len();
    if n >= 20
        && jeton.chars().any(|c| c.is_ascii_digit())
        && jeton.chars().any(|c| c.is_ascii_uppercase())
    {
        return true;
    }
    n >= 32 && jeton.chars().all(|c| c.is_ascii_hexdigit())
}

/// Retire d'un verdict libre tout ce qui pourrait etre une donnee personnelle.
///
/// Le registre est fait pour etre lu, exporte et colle dans un ticket. Il ne
/// doit contenir ni chemin de fichier, ni cle, ni jeton — des compteurs et des
/// verdicts, rien d'autre.
///
/// Deux masques, et une troncature :
///
/// * tout jeton contenant `/` ou `\` devient `[chemin]` — cela couvre les
///   chemins POSIX, les chemins Windows et les URL ;
/// * tout jeton qui [`ressemble_a_un_secret`] devient `[masque]` ;
/// * le resultat est tronque a [`DETAIL_MAX`] caracteres.
///
/// Consequence assumee : un `12/34` ecrit dans `detail` sera masque. C'est
/// voulu — les compteurs ont leur colonne (`items`), le champ libre n'est pas
/// la pour ca.
pub fn detail_sans_donnees(brut: &str) -> String {
    let nettoye = brut
        .split_whitespace()
        .map(|jeton| {
            if jeton.contains('/') || jeton.contains('\\') {
                "[chemin]"
            } else if ressemble_a_un_secret(jeton) {
                "[masque]"
            } else {
                jeton
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    // Troncature sur une frontiere de caractere : `detail` peut porter des
    // accents, et couper au milieu d'un UTF-8 paniquerait.
    if nettoye.chars().count() <= DETAIL_MAX {
        return nettoye;
    }
    nettoye.chars().take(DETAIL_MAX).collect()
}

// ─── Le modele rendu a la lecture ────────────────────────────────────────

/// Une execution, telle que la route la rend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRun {
    pub boot_id: String,
    pub task: String,
    pub seq: i64,
    /// Horodatage ABSOLU UTC ISO-8601 du debut. Jamais un « il y a N secondes ».
    pub started_at: String,
    /// Horodatage absolu de la fin, ou `None` si la passe tourne encore.
    ///
    /// Sur une execution `interrompu`, c'est l'instant ou le registre l'a
    /// DECLAREE close au demarrage suivant, pas celui ou elle est morte — on ne
    /// le connait pas.
    pub finished_at: Option<String>,
    /// Duree mesuree sur horloge monotone. `None` sur une execution en cours,
    /// et `None` sur une execution interrompue : on n'a jamais vu sa fin, et
    /// une duree calculee par soustraction des deux dates y compterait tout le
    /// temps d'arret du serveur.
    pub duration_ms: Option<i64>,
    pub outcome: String,
    /// Nombre d'elements traites, quand la passe sait le dire.
    pub items: Option<i64>,
    /// Verdict court, deja passe par [`detail_sans_donnees`].
    pub detail: Option<String>,
}

// ─── SQL ─────────────────────────────────────────────────────────────────

/// Constructeurs SQL agnostiques du moteur.
pub mod sql {
    use super::SqlDialect;

    pub fn prochain_seq<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM task_runs WHERE boot_id = {} AND task = {}",
            d.placeholder(1),
            d.placeholder(2),
        )
    }

    pub fn ouvrir<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome) \
             VALUES ({}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.now_iso8601(),
            d.placeholder(4),
        )
    }

    pub fn fermer<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE task_runs \
                SET finished_at = {}, duration_ms = {}, outcome = {}, items = {}, detail = {} \
              WHERE boot_id = {} AND task = {} AND seq = {}",
            d.now_iso8601(),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
        )
    }

    /// Fermer toute execution laissee `en_cours` par une AUTRE incarnation.
    ///
    /// `duration_ms` reste NULL : on n'a jamais vu la fin de cette passe, et la
    /// difference des deux dates compterait tout le temps d'arret du serveur.
    pub fn clore_orphelines<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE task_runs \
                SET outcome = {}, finished_at = {}, detail = {} \
              WHERE outcome = {} AND boot_id <> {}",
            d.placeholder(1),
            d.now_iso8601(),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
        )
    }

    /// Retention par nombre : ne garder que les N dernieres executions d'une
    /// passe.
    ///
    /// Ecrit en `<` sur la plus ancienne des N gardees plutot qu'en `NOT IN` :
    /// portable SQLite/PostgreSQL, et sans sous-requete correlee. Une egalite
    /// d'horodatage a la frontiere fait garder une ligne de trop — la borne est
    /// un plafond, pas un compte exact.
    pub fn purger_par_nombre<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM task_runs \
              WHERE task = {} \
                AND started_at < (SELECT MIN(started_at) FROM ( \
                        SELECT started_at FROM task_runs WHERE task = {} \
                         ORDER BY started_at DESC LIMIT {} \
                    ) AS gardees)",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        )
    }

    pub fn purger_par_age<D: SqlDialect>(d: &D, jours: i64) -> String {
        format!(
            "DELETE FROM task_runs WHERE NOT ({})",
            d.since_days("started_at", jours)
        )
    }

    pub const COLONNES: &str =
        "boot_id, task, seq, started_at, finished_at, duration_ms, outcome, items, detail";

    /// `LIMIT` est PARAMETREE et non interpolee : la limite vient d'une requete
    /// HTTP. `SqlDialect::limit_offset` n'accepte que des litteraux, d'ou le
    /// placeholder ecrit a la main.
    pub fn lister_tout<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {COLONNES} FROM task_runs ORDER BY started_at DESC, task ASC LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn lister_par_tache<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {COLONNES} FROM task_runs WHERE task = {} \
              ORDER BY started_at DESC, seq DESC LIMIT {}",
            d.placeholder(1),
            d.placeholder(2),
        )
    }
}

// ─── Le registre ─────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct TaskRunRepo {
    db: Arc<dyn DbBackend>,
}

impl TaskRunRepo {
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    fn dialect_sql<F1, F2>(&self, sqlite: F1, postgres: F2) -> String
    where
        F1: FnOnce(&SqliteDialect) -> String,
        F2: FnOnce(&PostgresDialect) -> String,
    {
        match self.db.engine() {
            Engine::Sqlite => sqlite(&SqliteDialect),
            Engine::Postgres => postgres(&PostgresDialect),
        }
    }

    /// Ouvrir une execution. Rend un temoin qui la ferme — explicitement par
    /// [`Execution::terminer`], ou en `interrompu` s'il est detruit sans ca
    /// (retour anticipe, `?`, deroulement de panique).
    ///
    /// Une ecriture qui echoue ne doit JAMAIS empecher la passe de tourner :
    /// le registre observe, il ne gouverne pas. En cas d'echec d'insertion le
    /// temoin est rendu inerte et la passe continue.
    pub fn ouvrir(&self, task: &'static str) -> Execution {
        let seq = self.prochain_seq(task);
        let inerte = Execution {
            repo: None,
            task,
            seq: 0,
            depart: Instant::now(),
            clos: true,
        };

        let Some(seq) = seq else {
            return inerte;
        };

        let sql = self.dialect_sql(sql::ouvrir, sql::ouvrir);
        let bid = boot_id();
        let etat = Verdict::EnCours.as_str();
        let params: [&dyn ToSqlValue; 4] = [&bid, &task, &seq, &etat];
        if let Err(e) = self.db.execute(&sql, &params) {
            tracing::warn!(task, error = %e, "registre_ouverture_echouee");
            return inerte;
        }

        Execution {
            repo: Some(self.clone()),
            task,
            seq,
            depart: Instant::now(),
            clos: false,
        }
    }

    fn prochain_seq(&self, task: &str) -> Option<i64> {
        let sql = self.dialect_sql(sql::prochain_seq, sql::prochain_seq);
        let bid = boot_id();
        let params: [&dyn ToSqlValue; 2] = [&bid, &task];
        match self.db.query_one(&sql, &params) {
            Ok(Some(cols)) => Some(cols.first().and_then(|v| v.as_i64()).unwrap_or(1)),
            Ok(None) => Some(1),
            Err(e) => {
                tracing::warn!(task, error = %e, "registre_seq_illisible");
                None
            }
        }
    }

    fn fermer(
        &self,
        task: &str,
        seq: i64,
        verdict: Verdict,
        duration_ms: Option<i64>,
        items: Option<i64>,
        detail: Option<String>,
    ) {
        let sql = self.dialect_sql(sql::fermer, sql::fermer);
        let bid = boot_id();
        let etat = verdict.as_str();
        let detail = detail.map(|d| detail_sans_donnees(&d));
        let params: [&dyn ToSqlValue; 7] =
            [&duration_ms, &etat, &items, &detail, &bid, &task, &seq];
        if let Err(e) = self.db.execute(&sql, &params) {
            tracing::warn!(task, error = %e, "registre_fermeture_echouee");
            return;
        }
        self.purger_tache(task);
    }

    /// Declarer interrompue toute execution qu'une AUTRE incarnation du
    /// processus a laissee ouverte.
    ///
    /// A appeler UNE fois au demarrage, avant que la moindre passe n'ouvre sa
    /// ligne. Aucune passe ne survit au processus qui la portait : etre ici
    /// suffit a prouver que ces lignes mentent. Pas de delai de grace, pas
    /// d'horodatage a comparer — c'est exactement le raisonnement de
    /// `marquer_enrichissements_interrompus` (#2002), applique au registre.
    ///
    /// Le filtre `boot_id <> <le notre>` est une garde de re-entrance : si
    /// cette fonction etait rappelee en cours de route, elle ne fermerait pas
    /// une passe VIVANTE de ce processus.
    ///
    /// ⚠️ Deux serveurs Tune partageant la meme base PostgreSQL se fermeraient
    /// mutuellement leurs lignes. C'est deja vrai de `scan_status` et des
    /// avancements d'enrichissement ; ce registre n'aggrave rien et ne pretend
    /// pas resoudre ce cas (incident du .15, service en double).
    pub fn clore_orphelines(&self) -> Result<usize, String> {
        let sql = self.dialect_sql(sql::clore_orphelines, sql::clore_orphelines);
        let interrompu = Verdict::Interrompu.as_str();
        let en_cours = Verdict::EnCours.as_str();
        let bid = boot_id();
        let detail = "close au demarrage : le processus qui la portait a disparu";
        let params: [&dyn ToSqlValue; 4] = [&interrompu, &detail, &en_cours, &bid];
        self.db.execute(&sql, &params)
    }

    /// Retention par age. A appeler au demarrage, apres
    /// [`Self::clore_orphelines`] — dans cet ordre, une orpheline tres ancienne
    /// est d'abord fermee proprement, puis effacee si elle depasse l'age. Dans
    /// l'autre ordre elle serait effacee sans avoir jamais ete close, et le
    /// compte de fermetures du journal ne dirait plus la verite.
    pub fn purger_par_age(&self) -> Result<usize, String> {
        let sql = self.dialect_sql(
            |d| sql::purger_par_age(d, RETENTION_JOURS),
            |d| sql::purger_par_age(d, RETENTION_JOURS),
        );
        self.db.execute(&sql, &[])
    }

    /// Retention par nombre, pour une passe. Appelee a chaque fermeture.
    fn purger_tache(&self, task: &str) {
        let sql = self.dialect_sql(sql::purger_par_nombre, sql::purger_par_nombre);
        let params: [&dyn ToSqlValue; 3] = [&task, &task, &RETENTION_EXECUTIONS_PAR_PASSE];
        if let Err(e) = self.db.execute(&sql, &params) {
            tracing::debug!(task, error = %e, "registre_purge_echouee");
        }
    }

    pub fn lister(&self, task: Option<&str>, limite: i64) -> Result<Vec<TaskRun>, String> {
        let limite = limite.clamp(1, 500);
        let rows = match task {
            Some(t) => {
                let sql = self.dialect_sql(sql::lister_par_tache, sql::lister_par_tache);
                let params: [&dyn ToSqlValue; 2] = [&t, &limite];
                self.db.query_many(&sql, &params)?
            }
            None => {
                let sql = self.dialect_sql(sql::lister_tout, sql::lister_tout);
                let params: [&dyn ToSqlValue; 1] = [&limite];
                self.db.query_many(&sql, &params)?
            }
        };
        Ok(rows.iter().map(|r| ligne_en_execution(r)).collect())
    }

    /// La derniere execution de chaque passe cablee — la vue qui repond a
    /// « est-ce que ca tourne ? ».
    ///
    /// Une passe connue mais sans historique est ABSENTE du resultat plutot
    /// qu'inventee : c'est au lecteur de la distinguer, et
    /// [`TACHES_CABLEES`] lui donne la liste de reference.
    pub fn resume(&self) -> Result<Vec<TaskRun>, String> {
        let mut derniers = Vec::new();
        for task in TACHES_CABLEES {
            if let Some(r) = self.lister(Some(task), 1)?.into_iter().next() {
                derniers.push(r);
            }
        }
        Ok(derniers)
    }
}

fn ligne_en_execution(r: &[super::backend::SqlValue]) -> TaskRun {
    TaskRun {
        boot_id: r.first().and_then(|v| v.as_string()).unwrap_or_default(),
        task: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        seq: r.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
        started_at: r.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
        finished_at: r.get(4).and_then(|v| v.as_string()),
        duration_ms: r.get(5).and_then(|v| v.as_i64()),
        outcome: r.get(6).and_then(|v| v.as_string()).unwrap_or_default(),
        items: r.get(7).and_then(|v| v.as_i64()),
        detail: r.get(8).and_then(|v| v.as_string()),
    }
}

// ─── Le temoin d'execution ───────────────────────────────────────────────

/// Temoin d'une execution ouverte. Le detruire sans avoir appele
/// [`Execution::terminer`] inscrit `interrompu` — c'est ce qui couvre les
/// retours anticipes et les paniques, exactement comme `ScanStatusGuard` rend
/// `scan_status` a `idle` sur tous les chemins de sortie.
///
/// Ce que le `Drop` NE couvre PAS : l'arret du processus. Une tache tokio
/// abandonnee a l'extinction ne deroule pas ses destructeurs. C'est
/// precisement le cas que [`TaskRunRepo::clore_orphelines`] rattrape au
/// demarrage suivant.
pub struct Execution {
    repo: Option<TaskRunRepo>,
    task: &'static str,
    seq: i64,
    depart: Instant,
    clos: bool,
}

impl Execution {
    /// Fermer l'execution avec son verdict.
    ///
    /// `items` : nombre d'elements traites quand la passe sait le dire, `None`
    /// sinon. `detail` : verdict court — il passe par [`detail_sans_donnees`].
    pub fn terminer(mut self, verdict: Verdict, items: Option<i64>, detail: Option<&str>) {
        let duree = self.depart.elapsed().as_millis().min(i64::MAX as u128) as i64;
        if let Some(repo) = self.repo.as_ref() {
            repo.fermer(
                self.task,
                self.seq,
                verdict,
                Some(duree),
                items,
                detail.map(|d| d.to_string()),
            );
        }
        self.clos = true;
    }

    /// Raccourci : la passe a tourne et n'avait rien a faire. Le verdict qui
    /// repond a « ca n'a rien fait » — oui, elle a tourne ; non, il n'y avait
    /// rien.
    pub fn rien_a_faire(self, detail: Option<&str>) {
        self.terminer(Verdict::RienAFaire, Some(0), detail);
    }

    /// Raccourci : la passe a echoue.
    pub fn echec(self, detail: &str) {
        self.terminer(Verdict::Echec, None, Some(detail));
    }
}

impl Drop for Execution {
    fn drop(&mut self) {
        if self.clos {
            return;
        }
        if let Some(repo) = self.repo.as_ref() {
            repo.fermer(
                self.task,
                self.seq,
                Verdict::Interrompu,
                None,
                None,
                Some("temoin detruit sans verdict".to_string()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn repo() -> TaskRunRepo {
        TaskRunRepo::with_backend(base())
    }

    // ─── Une passe normale ───────────────────────────────────────────────

    #[test]
    fn une_passe_normale_laisse_un_debut_une_fin_une_duree_et_un_verdict() {
        let r = repo();
        let e = r.ouvrir(TACHE_REPLAYGAIN);

        // Pendant qu'elle tourne, la ligne existe et se lit `en_cours`.
        let en_vol = r.lister(Some(TACHE_REPLAYGAIN), 10).unwrap();
        assert_eq!(en_vol.len(), 1);
        assert_eq!(en_vol[0].outcome, "en_cours");
        assert!(en_vol[0].finished_at.is_none());

        e.terminer(Verdict::Succes, Some(42), Some("42 pistes analysees"));

        let apres = r.lister(Some(TACHE_REPLAYGAIN), 10).unwrap();
        assert_eq!(apres.len(), 1);
        let run = &apres[0];
        assert_eq!(run.outcome, "succes");
        assert_eq!(run.items, Some(42));
        assert_eq!(run.detail.as_deref(), Some("42 pistes analysees"));
        assert!(run.finished_at.is_some(), "la fin doit etre horodatee");
        assert!(run.duration_ms.is_some(), "la duree doit etre mesuree");
        assert_eq!(run.boot_id, boot_id());
    }

    #[test]
    fn les_horodatages_sont_absolus_et_non_un_compteur_relatif() {
        // PR #2632 : `uptime_seconds` a du recevoir une date d'ancrage parce
        // qu'un compteur relatif ne permet pas de verifier ce qu'il affirme.
        let r = repo();
        r.ouvrir(TACHE_BATTEMENT_COEUR)
            .terminer(Verdict::Succes, None, None);

        let run = &r.lister(Some(TACHE_BATTEMENT_COEUR), 1).unwrap()[0];
        for date in [Some(run.started_at.clone()), run.finished_at.clone()] {
            let d = date.expect("les deux bornes sont horodatees");
            assert_eq!(d.len(), 20, "ISO-8601 UTC compact : {d}");
            assert!(d.ends_with('Z'), "horodatage non UTC : {d}");
            assert_eq!(&d[4..5], "-", "ce n'est pas une date : {d}");
            assert!(
                d.starts_with("20"),
                "une date absolue commence par son siecle, pas par un delta : {d}"
            );
        }
    }

    #[test]
    fn rien_a_faire_n_est_pas_un_echec() {
        // « Ca n'a rien fait » doit pouvoir recevoir une reponse : elle a
        // tourne, elle n'a rien trouve.
        let r = repo();
        r.ouvrir(TACHE_REPLAYGAIN)
            .rien_a_faire(Some("aucune piste sans ReplayGain"));

        let run = &r.lister(Some(TACHE_REPLAYGAIN), 1).unwrap()[0];
        assert_eq!(run.outcome, "rien_a_faire");
        assert_eq!(run.items, Some(0));
        assert!(run.duration_ms.is_some());
    }

    #[test]
    fn un_temoin_detruit_sans_verdict_se_ferme_en_interrompu() {
        // Retour anticipe, `?`, deroulement de panique : le `Drop` couvre les
        // sorties du processus VIVANT.
        let r = repo();
        {
            let _e = r.ouvrir(TACHE_SCAN_DEMARRAGE);
        }
        let run = &r.lister(Some(TACHE_SCAN_DEMARRAGE), 1).unwrap()[0];
        assert_eq!(run.outcome, "interrompu");
        assert!(run.duration_ms.is_none());
    }

    // ─── Une passe interrompue par un redemarrage ────────────────────────

    #[test]
    fn une_passe_orpheline_est_close_au_demarrage_suivant() {
        // Le defaut #2002 : une passe interrompue par un redemarrage laissait
        // « running » inscrit a jamais, verrouillant le bouton sur une passe
        // morte. On simule le processus mort en ecrivant sa ligne sous un
        // AUTRE `boot_id`, puisque le notre est fige pour ce test.
        let db = base();
        let r = TaskRunRepo::with_backend(db.clone());
        let mort = "0-boot-precedent";
        db.execute(
            "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome) \
             VALUES (?, ?, 1, '2026-08-01T10:00:00Z', 'en_cours')",
            &[&mort as &dyn ToSqlValue, &TACHE_REPLAYGAIN],
        )
        .unwrap();

        assert_eq!(
            r.lister(Some(TACHE_REPLAYGAIN), 10).unwrap()[0].outcome,
            "en_cours",
            "temoin : avant le demarrage, la base affirme qu'elle tourne"
        );

        let closes = r.clore_orphelines().unwrap();
        assert_eq!(closes, 1);

        let run = &r.lister(Some(TACHE_REPLAYGAIN), 10).unwrap()[0];
        assert_eq!(run.outcome, "interrompu");
        assert!(
            run.finished_at.is_some(),
            "une orpheline close porte l'instant ou on l'a declaree close"
        );
        assert!(
            run.duration_ms.is_none(),
            "on n'a jamais vu sa fin : une duree ici compterait le temps d'arret du serveur"
        );
        assert_eq!(run.started_at, "2026-08-01T10:00:00Z", "le debut est garde");
    }

    #[test]
    fn clore_les_orphelines_ne_touche_pas_une_passe_vivante_de_ce_processus() {
        // Garde de re-entrance : rappeler la fermeture en cours de route ne
        // doit pas tuer une passe qui tourne VRAIMENT.
        let r = repo();
        let e = r.ouvrir(TACHE_REPLAYGAIN);

        assert_eq!(r.clore_orphelines().unwrap(), 0);
        assert_eq!(
            r.lister(Some(TACHE_REPLAYGAIN), 1).unwrap()[0].outcome,
            "en_cours"
        );

        e.terminer(Verdict::Succes, Some(1), None);
    }

    #[test]
    fn une_execution_deja_terminee_n_est_pas_retouchee_au_demarrage() {
        let db = base();
        let r = TaskRunRepo::with_backend(db.clone());
        db.execute(
            "INSERT INTO task_runs (boot_id, task, seq, started_at, finished_at, duration_ms, outcome, items) \
             VALUES ('0-boot-precedent', ?, 1, '2026-08-01T10:00:00Z', '2026-08-01T10:05:00Z', 300000, 'succes', 12)",
            &[&TACHE_REPLAYGAIN as &dyn ToSqlValue],
        )
        .unwrap();

        assert_eq!(r.clore_orphelines().unwrap(), 0);
        let run = &r.lister(Some(TACHE_REPLAYGAIN), 1).unwrap()[0];
        assert_eq!(run.outcome, "succes");
        assert_eq!(run.duration_ms, Some(300000));
        assert_eq!(run.items, Some(12));
    }

    // ─── La retention ────────────────────────────────────────────────────

    #[test]
    fn la_retention_borne_le_nombre_d_executions_par_passe() {
        let db = base();
        let r = TaskRunRepo::with_backend(db.clone());

        // Des horodatages DISTINCTS et croissants : la purge trie par
        // `started_at`, et des lignes ex aequo la feraient garder trop.
        let n = RETENTION_EXECUTIONS_PAR_PASSE + 20;
        for i in 0..n {
            let jour = 1 + (i % 28);
            let heure = i / 28;
            let debut = format!("2026-01-{jour:02}T{heure:02}:00:00Z");
            db.execute(
                "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome) \
                 VALUES ('semis', ?, ?, ?, 'succes')",
                &[&TACHE_REPLAYGAIN as &dyn ToSqlValue, &i, &debut],
            )
            .unwrap();
        }
        assert_eq!(
            r.lister(Some(TACHE_REPLAYGAIN), 500).unwrap().len() as i64,
            n
        );

        // La purge part de la fermeture d'une execution : on en ouvre une.
        r.ouvrir(TACHE_REPLAYGAIN)
            .terminer(Verdict::Succes, Some(0), None);

        let restant = r.lister(Some(TACHE_REPLAYGAIN), 500).unwrap();
        assert_eq!(
            restant.len() as i64,
            RETENTION_EXECUTIONS_PAR_PASSE,
            "la table doit etre ramenee au plafond, pas laissee a {n}"
        );
        // Et ce sont bien les plus RECENTES qui restent.
        assert_eq!(restant[0].boot_id, boot_id());
    }

    #[test]
    fn la_retention_ne_touche_pas_les_autres_passes() {
        let db = base();
        let r = TaskRunRepo::with_backend(db.clone());
        for i in 0..(RETENTION_EXECUTIONS_PAR_PASSE + 10) {
            let jour = 1 + (i % 28);
            let heure = i / 28;
            let debut = format!("2026-01-{jour:02}T{heure:02}:00:00Z");
            db.execute(
                "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome) \
                 VALUES ('semis', ?, ?, ?, 'succes')",
                &[&TACHE_REPLAYGAIN as &dyn ToSqlValue, &i, &debut],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome) \
             VALUES ('semis', ?, 1, '2026-01-01T00:00:00Z', 'succes')",
            &[&TACHE_BATTEMENT_COEUR as &dyn ToSqlValue],
        )
        .unwrap();

        r.ouvrir(TACHE_REPLAYGAIN)
            .terminer(Verdict::Succes, Some(0), None);

        assert_eq!(
            r.lister(Some(TACHE_BATTEMENT_COEUR), 500).unwrap().len(),
            1,
            "purger une passe ne doit pas effacer l'historique d'une autre"
        );
    }

    #[test]
    fn la_retention_par_age_efface_le_vieux_et_garde_le_recent() {
        let db = base();
        let r = TaskRunRepo::with_backend(db.clone());
        db.execute(
            "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome) \
             VALUES ('semis', ?, 1, '2020-01-01T00:00:00Z', 'succes')",
            &[&TACHE_REPLAYGAIN as &dyn ToSqlValue],
        )
        .unwrap();
        r.ouvrir(TACHE_BATTEMENT_COEUR)
            .terminer(Verdict::Succes, None, None);

        assert_eq!(r.purger_par_age().unwrap(), 1);
        assert!(r.lister(Some(TACHE_REPLAYGAIN), 10).unwrap().is_empty());
        assert_eq!(r.lister(Some(TACHE_BATTEMENT_COEUR), 10).unwrap().len(), 1);
    }

    // ─── Confidentialite ─────────────────────────────────────────────────

    #[test]
    fn un_chemin_de_fichier_n_atteint_jamais_la_base() {
        let r = repo();
        r.ouvrir(TACHE_SCAN_DEMARRAGE).terminer(
            Verdict::Succes,
            Some(3),
            Some("illisible /Users/bertrand/Musique/Nina Simone.flac"),
        );
        let d = r.lister(Some(TACHE_SCAN_DEMARRAGE), 1).unwrap()[0]
            .detail
            .clone()
            .unwrap();
        assert!(
            !d.contains("bertrand"),
            "chemin personnel inscrit en base : {d}"
        );
        assert!(!d.contains('/'), "{d}");
        assert!(d.contains("[chemin]"), "{d}");
    }

    #[test]
    fn un_chemin_windows_et_une_url_sont_masques_aussi() {
        assert_eq!(
            detail_sans_donnees("echec C:\\Users\\Jean\\Music"),
            "echec [chemin]"
        );
        assert_eq!(
            detail_sans_donnees("appel https://mozaiklabs.fr/api/v1/heartbeat refuse"),
            "appel [chemin] refuse"
        );
    }

    #[test]
    fn une_cle_ou_un_hachage_est_masque() {
        assert_eq!(
            detail_sans_donnees("licence TUNE7fbA92c4E1d0Bb35aa refusee"),
            "licence [masque] refusee"
        );
        assert_eq!(
            detail_sans_donnees("empreinte 9e107d9d372bb6826bd81d3542a419d6"),
            "empreinte [masque]"
        );
    }

    #[test]
    fn un_verdict_ordinaire_traverse_le_filtre_intact() {
        // Contre-epreuve du masquage : s'il mordait sur du francais courant,
        // le registre deviendrait illisible et les tests ci-dessus ne
        // prouveraient rien.
        for phrase in [
            "42 pistes analysees, 3 illisibles",
            "aucune piste sans ReplayGain",
            "battement de coeur accepte",
            "interrompu par l'utilisateur",
            "replaygain_analysis_terminee sans erreur",
        ] {
            assert_eq!(detail_sans_donnees(phrase), phrase, "mordu : {phrase}");
        }
    }

    #[test]
    fn le_detail_est_tronque_et_ne_coupe_pas_un_caractere_accentue() {
        let long = "é".repeat(DETAIL_MAX + 50);
        let coupe = detail_sans_donnees(&long);
        assert_eq!(coupe.chars().count(), DETAIL_MAX);
    }

    // ─── Interrogeabilite ────────────────────────────────────────────────

    #[test]
    fn le_resume_rend_la_derniere_execution_de_chaque_passe_cablee() {
        let db = base();
        let r = TaskRunRepo::with_backend(db.clone());
        db.execute(
            "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome, items) \
             VALUES ('vieux', ?, 1, '2026-08-01T10:00:00Z', 'succes', 1)",
            &[&TACHE_REPLAYGAIN as &dyn ToSqlValue],
        )
        .unwrap();
        r.ouvrir(TACHE_REPLAYGAIN)
            .terminer(Verdict::Succes, Some(9), None);
        r.ouvrir(TACHE_BATTEMENT_COEUR)
            .terminer(Verdict::Echec, None, Some("hote injoignable"));

        let resume = r.resume().unwrap();
        assert_eq!(resume.len(), 2, "une passe jamais executee reste absente");
        let rg = resume.iter().find(|x| x.task == TACHE_REPLAYGAIN).unwrap();
        assert_eq!(rg.items, Some(9), "c'est la PLUS RECENTE qui est rendue");
        let hb = resume
            .iter()
            .find(|x| x.task == TACHE_BATTEMENT_COEUR)
            .unwrap();
        assert_eq!(hb.outcome, "echec");
    }

    #[test]
    fn deux_executions_de_la_meme_passe_ne_se_marchent_pas_dessus() {
        let r = repo();
        r.ouvrir(TACHE_BATTEMENT_COEUR)
            .terminer(Verdict::Succes, Some(1), None);
        r.ouvrir(TACHE_BATTEMENT_COEUR)
            .terminer(Verdict::Echec, None, Some("hote injoignable"));

        let l = r.lister(Some(TACHE_BATTEMENT_COEUR), 10).unwrap();
        assert_eq!(l.len(), 2);
        let seqs: Vec<i64> = l.iter().map(|x| x.seq).collect();
        assert!(seqs.contains(&1) && seqs.contains(&2), "{seqs:?}");
    }

    #[test]
    fn le_boot_id_est_stable_pour_le_processus() {
        assert_eq!(boot_id(), boot_id());
        assert!(boot_id().contains('-'), "{}", boot_id());
    }
}
