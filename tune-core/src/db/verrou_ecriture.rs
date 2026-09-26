//! Le verrou de la connexion d'écriture SQLite, surveillé (#4924).
//!
//! ## Le constat
//!
//! Sur le .18 (0.9.163), le serveur gèle une dizaine de minutes, environ une
//! fois par heure : plus d'API, même en local, journal muet (jusqu'à
//! `memory_diagnostics`), processeur à zéro, 10 à 13 fils en
//! `futex_wait_queue`. Au réveil, six minuteries indépendantes partent dans
//! la même demi-seconde, et la passe d'album du ReplayGain dit avoir ATTENDU
//! 614 s pour une quarantaine d'écritures de clés.
//!
//! La connexion d'écriture SQLite est un `std::sync::Mutex` **synchrone**.
//! Un fil de l'exécuteur tokio qui veut écrire pendant qu'un autre la tient
//! s'endort dans un `futex` en gardant son cœur d'exécuteur : les tâches de
//! sa file, le pilote d'E/S et les minuteries qu'il aurait fait tourner
//! attendent avec lui. Huit écrivains suffisent à immobiliser les huit
//! fils du .18. Plus personne n'accepte de connexion : c'est le gel observé.
//!
//! ## Ce que ce module change
//!
//! 1. **L'attente sort de l'exécuteur.** Quand le verrou est pris, l'attente
//!    passe par [`attendre_hors_executeur`] : `block_in_place` rend le cœur
//!    d'exécuteur à un autre fil avant de s'endormir. Les écrivains attendent
//!    toujours, mais l'API, les minuteries et le journal continuent. Le
//!    chemin libre (`try_lock` réussi) ne change pas d'un octet. C'est ce que
//!    le moteur PostgreSQL fait déjà à chaque appel (`backend.rs`,
//!    `block_in_place` autour de `Handle::block_on`).
//! 2. **Le détenteur est nommé.** Chaque prise note son lieu
//!    (`#[track_caller]`), son fil et son identifiant de fil noyau ; chaque
//!    attente aussi. Une [`Sentinelle`], sur un fil À ELLE, hors de
//!    l'exécuteur, dit en WARN toute détention qui dépasse
//!    [`SEUIL_DETENTION_LONGUE`], puis la rappelle toutes les
//!    [`RAPPEL_DETENTION`] tant qu'elle dure. Le fil qui rend le verrou dit
//!    la durée totale.
//! 3. **La pile, dès le premier incident.** Capturer une pile à chaque prise
//!    coûterait quelques microsecondes par écriture : on ne le fait qu'une
//!    fois ARMÉ, soit par `TUNE_PILES_VERROU_ECRITURE=1`, soit dès la
//!    première détention longue observée. Le gel du .18 revient chaque heure :
//!    le suivant porte alors la pile complète de son détenteur.
//!
//! Ce module ne prétend pas avoir trouvé QUI tient le verrou dix minutes :
//! il rend ce détenteur nommable, et empêche qu'il fige tout le serveur.

use std::backtrace::Backtrace;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LockResult, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError, Weak};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::db::transaction_du_lot::{ATTENTE_MAX_FIN_DU_LOT, CESSION_MAX, TransactionDuLot};

/// Au-delà, une détention du verrou d'écriture est dite en WARN.
///
/// Une écriture SQLite ordinaire tient le verrou quelques millisecondes ; un
/// lot de scan quelques centaines. Une seconde, c'est déjà une seconde
/// pendant laquelle tous les autres écrivains attendent.
pub const SEUIL_DETENTION_LONGUE: Duration = Duration::from_secs(1);

/// Rappel d'une détention qui dure encore, pour que le journal suive un gel.
pub const RAPPEL_DETENTION: Duration = Duration::from_secs(30);

/// Cadence de la sentinelle : quatre lectures par seconde d'un emplacement
/// sous un petit verrou. Rien d'autre.
const PERIODE_SENTINELLE: Duration = Duration::from_millis(250);

/// Capture des piles : armée par l'environnement ou par un premier incident.
static PILES_ARMEES: AtomicBool = AtomicBool::new(false);

fn piles_armees() -> bool {
    static ENV: OnceLock<()> = OnceLock::new();
    ENV.get_or_init(|| {
        if std::env::var("TUNE_PILES_VERROU_ECRITURE").is_ok_and(|v| v == "1" || v == "true") {
            PILES_ARMEES.store(true, Ordering::Relaxed);
        }
    });
    PILES_ARMEES.load(Ordering::Relaxed)
}

fn armer_les_piles() {
    if !PILES_ARMEES.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "ecriture_sqlite_piles_armees — les prochaines prises du verrou d'écriture \
             porteront la pile de leur détenteur (#4924)"
        );
    }
}

/// Identifiant noyau du fil courant : celui que `/proc/<pid>/task/<tid>` et
/// `gdb` connaissent. 0 hors Linux.
pub fn tid_courant() -> i64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: gettid n'a pas d'argument et ne peut pas échouer.
        unsafe { libc::syscall(libc::SYS_gettid) }
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Faire une attente synchrone SANS garder un cœur de l'exécuteur tokio.
///
/// Sur un fil de l'exécuteur multi-fil, `block_in_place` confie la file du
/// fil à un remplaçant avant d'appeler `f`. Partout ailleurs — fil ordinaire,
/// fil de `spawn_blocking`, exécuteur mono-fil des essais où `block_in_place`
/// paniquerait — `f` est appelé tel quel.
pub fn attendre_hors_executeur<R>(f: impl FnOnce() -> R) -> R {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Qui tient, ou qui attend : lieu, fil, depuis quand.
struct Prise {
    jeton: u64,
    lieu: &'static Location<'static>,
    fil: std::thread::Thread,
    tid: i64,
    depuis: Instant,
    pile: Option<Backtrace>,
    dernier_signalement: Option<Instant>,
}

impl Prise {
    fn nouvelle(jeton: u64, lieu: &'static Location<'static>, pile: bool) -> Self {
        Self {
            jeton,
            lieu,
            fil: std::thread::current(),
            tid: tid_courant(),
            depuis: Instant::now(),
            pile: pile.then(Backtrace::force_capture),
            dernier_signalement: None,
        }
    }

    fn nom_du_fil(&self) -> String {
        self.fil.name().unwrap_or("?").to_string()
    }

    fn photo(&self) -> PhotoPrise {
        PhotoPrise {
            lieu: self.lieu.to_string(),
            fil: self.nom_du_fil(),
            tid: self.tid,
            depuis: self.depuis.elapsed(),
            pile: self.pile.as_ref().map(|p| p.to_string()),
        }
    }
}

/// Instantané d'une prise ou d'une attente, pour le journal et le relevé de gel.
#[derive(Debug, Clone)]
pub struct PhotoPrise {
    /// `fichier:ligne:colonne` de l'appel à `lock()`.
    pub lieu: String,
    pub fil: String,
    pub tid: i64,
    pub depuis: Duration,
    /// Pile du détenteur au moment de la prise, si les piles étaient armées.
    pub pile: Option<String>,
}

/// L'état surveillé d'UN verrou d'écriture.
struct Etat {
    detention: Mutex<Option<Prise>>,
    attentes: Mutex<Vec<Prise>>,
    jetons: AtomicU64,
    seuil: Duration,
    /// Qui a laissé une transaction ouverte, et qui attend qu'elle se ferme
    /// (voir [`crate::db::transaction_du_lot`]).
    lot: TransactionDuLot,
}

impl Etat {
    fn jeton(&self) -> u64 {
        self.jetons.fetch_add(1, Ordering::Relaxed)
    }
}

/// Relevé d'un verrou d'écriture : son détenteur et ceux qui l'attendent.
#[derive(Debug, Clone, Default)]
pub struct ReleveVerrou {
    pub detenteur: Option<PhotoPrise>,
    pub attentes: Vec<PhotoPrise>,
}

impl std::fmt::Display for ReleveVerrou {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detenteur {
            None => writeln!(f, "détenteur : aucun")?,
            Some(d) => {
                writeln!(
                    f,
                    "détenteur : {} (fil « {} », tid {}) depuis {} ms",
                    d.lieu,
                    d.fil,
                    d.tid,
                    d.depuis.as_millis()
                )?;
                match &d.pile {
                    Some(p) => writeln!(f, "pile du détenteur :\n{p}")?,
                    None => writeln!(
                        f,
                        "pile du détenteur : non capturée (piles pas encore armées)"
                    )?,
                }
            }
        }
        writeln!(f, "en attente : {}", self.attentes.len())?;
        for a in &self.attentes {
            writeln!(
                f,
                "  - {} (fil « {} », tid {}) depuis {} ms",
                a.lieu,
                a.fil,
                a.tid,
                a.depuis.as_millis()
            )?;
        }
        Ok(())
    }
}

/// La connexion d'écriture SQLite derrière un verrou surveillé.
///
/// Même forme d'emploi que l'`Arc<Mutex<Connection>>` qu'elle remplace :
/// `db.connection().lock().unwrap()` rend une garde qui se déréférence en
/// [`Connection`], et chaque appel est maintenant nommé et surveillé.
#[derive(Clone)]
pub struct VerrouEcriture {
    connexion: Arc<Mutex<Connection>>,
    etat: Arc<Etat>,
}

impl VerrouEcriture {
    /// Un verrou surveillé par la [`Sentinelle::globale`], au seuil de
    /// production.
    pub fn new(connexion: Arc<Mutex<Connection>>) -> Self {
        let v = Self::sans_sentinelle(connexion, SEUIL_DETENTION_LONGUE);
        Sentinelle::globale().surveiller(&v);
        v
    }

    /// Un verrou que seule une sentinelle explicite surveillera (essais).
    pub fn sans_sentinelle(connexion: Arc<Mutex<Connection>>, seuil: Duration) -> Self {
        Self {
            connexion,
            etat: Arc::new(Etat {
                detention: Mutex::new(None),
                attentes: Mutex::new(Vec::new()),
                jetons: AtomicU64::new(1),
                seuil,
                lot: TransactionDuLot::default(),
            }),
        }
    }

    /// Le `Mutex` brut, pour la base en mémoire dont les « lecteurs » sont
    /// la même connexion.
    pub fn connexion_partagee(&self) -> Arc<Mutex<Connection>> {
        self.connexion.clone()
    }

    /// Prendre la connexion d'écriture pour ÉCRIRE.
    ///
    /// Libre : un `try_lock`, le relevé du détenteur, rien d'autre. Prise :
    /// l'appelant est inscrit parmi les attentes et attend HORS de
    /// l'exécuteur (voir [`attendre_hors_executeur`]).
    ///
    /// Si un AUTRE fil a laissé une transaction ouverte sur la connexion — un
    /// lot de scan entre deux de ses appels — l'appelant attend qu'elle se
    /// ferme au lieu d'écrire dedans (voir [`crate::db::transaction_du_lot`]).
    /// Le lot lui cède la place entre deux fichiers
    /// ([`Self::ceder_aux_ecrivains`]). Au-delà de
    /// [`ATTENTE_MAX_FIN_DU_LOT`], il écrit comme avant.
    #[track_caller]
    pub fn lock(&self) -> LockResult<EcritureTenue<'_>> {
        let lieu = Location::caller();
        let debut = Instant::now();
        let limite = debut + ATTENTE_MAX_FIN_DU_LOT;
        let mut inscrit = false;
        loop {
            let tenue = self.prendre(lieu);
            let autocommit = match &tenue {
                Ok(t) => t.is_autocommit(),
                Err(p) => p.get_ref().is_autocommit(),
            };
            let attendre = self.etat.lot.ouverte_par_un_autre(autocommit);
            if !attendre || Instant::now() >= limite {
                if inscrit {
                    self.etat.lot.desinscrire();
                    let attendu = debut.elapsed();
                    if attendre {
                        tracing::warn!(
                            lieu = %lieu,
                            attendu_ms = attendu.as_millis() as u64,
                            "ecriture_sqlite_entre_dans_une_transaction_etrangere"
                        );
                    } else if attendu >= Duration::from_millis(10) {
                        tracing::info!(
                            lieu = %lieu,
                            attendu_ms = attendu.as_millis() as u64,
                            "ecriture_sqlite_a_attendu_la_transaction_du_lot"
                        );
                    }
                }
                return tenue;
            }
            if !inscrit {
                self.etat.lot.inscrire();
                inscrit = true;
            }
            drop(tenue);
            attendre_hors_executeur(|| self.etat.lot.attendre_la_fermeture(limite));
        }
    }

    /// Prendre la connexion d'écriture pour LIRE, sans attendre la fin d'une
    /// transaction ouverte par un autre fil : les lectures fortes servent la
    /// lecture audio (file d'attente, zones) et ne doivent pas patienter
    /// derrière un lot de scan.
    #[track_caller]
    pub fn lock_sans_attendre_le_lot(&self) -> LockResult<EcritureTenue<'_>> {
        self.prendre(Location::caller())
    }

    /// Point de cession d'un lot : à appeler par le fil qui tient une
    /// transaction ouverte, entre deux unités de travail.
    ///
    /// Si un écrivain attend la fermeture de la transaction, le lot valide
    /// ce qu'il a fait (`COMMIT`), laisse passer les écrivains inscrits
    /// (au plus [`CESSION_MAX`]), puis rouvre sa transaction
    /// (`BEGIN IMMEDIATE`). Sans écrivain en attente, ou hors de sa propre
    /// transaction, ne fait rien. Rend `true` s'il a cédé.
    ///
    /// Le lot perd son atomicité : ce qu'il a validé avant la cession ne sera
    /// pas annulé par un `ROLLBACK` ultérieur. Le scan n'en dépend pas — son
    /// seul `ROLLBACK` suit un `COMMIT` refusé.
    #[track_caller]
    pub fn ceder_aux_ecrivains(&self) -> bool {
        let lieu = Location::caller();
        if self.etat.lot.en_attente() == 0 {
            return false;
        }
        let debut = Instant::now();
        {
            let conn = match self.prendre(lieu) {
                Ok(c) => c,
                Err(p) => p.into_inner(),
            };
            if conn.is_autocommit() || !self.etat.lot.est_au_fil_courant() {
                return false;
            }
            if let Err(e) = conn.execute_batch("COMMIT") {
                tracing::warn!(error = %e, lieu = %lieu, "lot_cession_commit_refuse");
                let _ = conn.execute_batch("ROLLBACK");
            }
        }
        while self.etat.lot.en_attente() > 0 && debut.elapsed() < CESSION_MAX {
            std::thread::sleep(Duration::from_millis(1));
        }
        {
            let conn = match self.prendre(lieu) {
                Ok(c) => c,
                Err(p) => p.into_inner(),
            };
            if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
                tracing::warn!(error = %e, lieu = %lieu, "lot_cession_begin_refuse");
            }
        }
        tracing::debug!(
            lieu = %lieu,
            cede_ms = debut.elapsed().as_millis() as u64,
            "lot_cede_aux_ecrivains"
        );
        true
    }

    fn prendre(&self, lieu: &'static Location<'static>) -> LockResult<EcritureTenue<'_>> {
        let (garde, empoisonnee) = match self.connexion.try_lock() {
            Ok(g) => (g, false),
            Err(TryLockError::Poisoned(p)) => (p.into_inner(), true),
            Err(TryLockError::WouldBlock) => {
                let jeton = self.etat.jeton();
                if let Ok(mut a) = self.etat.attentes.lock() {
                    a.push(Prise::nouvelle(jeton, lieu, false));
                }
                let r = attendre_hors_executeur(|| self.connexion.lock());
                if let Ok(mut a) = self.etat.attentes.lock() {
                    a.retain(|p| p.jeton != jeton);
                }
                match r {
                    Ok(g) => (g, false),
                    Err(p) => (p.into_inner(), true),
                }
            }
        };
        let jeton = self.etat.jeton();
        if let Ok(mut d) = self.etat.detention.lock() {
            *d = Some(Prise::nouvelle(jeton, lieu, piles_armees()));
        }
        let tenue = EcritureTenue {
            garde: Some(garde),
            etat: &self.etat,
            jeton,
        };
        if empoisonnee {
            Err(PoisonError::new(tenue))
        } else {
            Ok(tenue)
        }
    }

    /// Le détenteur courant et les attentes, sans rien bloquer d'autre que
    /// deux petits verrous de relevé.
    pub fn releve(&self) -> ReleveVerrou {
        releve_de(&self.etat)
    }
}

fn releve_de(etat: &Etat) -> ReleveVerrou {
    ReleveVerrou {
        detenteur: etat
            .detention
            .lock()
            .ok()
            .and_then(|d| d.as_ref().map(Prise::photo)),
        attentes: etat
            .attentes
            .lock()
            .map(|a| a.iter().map(Prise::photo).collect())
            .unwrap_or_default(),
    }
}

/// La connexion d'écriture tenue. Rendue au `drop`.
pub struct EcritureTenue<'a> {
    // `Option` pour RENDRE la connexion avant d'effacer le relevé : l'inverse
    // laisserait un instant où le relevé dit « personne » alors qu'on tient.
    garde: Option<MutexGuard<'a, Connection>>,
    etat: &'a Etat,
    jeton: u64,
}

impl std::ops::Deref for EcritureTenue<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.garde
            .as_deref()
            .expect("connexion d'écriture déjà rendue")
    }
}

impl std::ops::DerefMut for EcritureTenue<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.garde
            .as_deref_mut()
            .expect("connexion d'écriture déjà rendue")
    }
}

impl Drop for EcritureTenue<'_> {
    fn drop(&mut self) {
        // AVANT de rendre la connexion : noter qui laisse une transaction
        // ouverte, ou signaler qu'elle s'est fermée. Après, un autre fil
        // pourrait la prendre entre le `BEGIN` et cette inscription.
        if let Some(g) = self.garde.as_ref() {
            self.etat.lot.au_rendu(g.is_autocommit());
        }
        self.garde.take();
        // Le jeton garde d'effacer la prise d'un SUIVANT qui aurait pris le
        // verrou entre la ligne du dessus et celle-ci.
        let prise = self.etat.detention.lock().ok().and_then(|mut d| {
            if d.as_ref().is_some_and(|p| p.jeton == self.jeton) {
                d.take()
            } else {
                None
            }
        });
        let Some(prise) = prise else { return };
        let tenue = prise.depuis.elapsed();
        if tenue >= self.etat.seuil {
            armer_les_piles();
            tracing::warn!(
                lieu = %prise.lieu,
                fil = %prise.nom_du_fil(),
                tid = prise.tid,
                tenue_ms = tenue.as_millis() as u64,
                "ecriture_sqlite_detention_longue"
            );
        }
    }
}

/// Ce que la sentinelle signale d'une détention qui dure.
#[derive(Debug, Clone)]
pub struct Signalement {
    pub detenteur: PhotoPrise,
    /// Nombre d'appelants qui attendent le verrou à cet instant.
    pub attentes: usize,
}

type Puits = Box<dyn Fn(&Signalement) + Send + Sync>;

/// Le fil de surveillance des verrous d'écriture — un `std::thread`, jamais
/// une tâche tokio : il doit parler justement quand l'exécuteur ne le peut
/// plus.
pub struct Sentinelle {
    surveilles: Mutex<Vec<Weak<Etat>>>,
    puits: Puits,
}

impl Sentinelle {
    /// La sentinelle du processus, démarrée au premier verrou créé. Elle dit
    /// ses signalements en WARN.
    pub fn globale() -> &'static Arc<Sentinelle> {
        static GLOBALE: OnceLock<Arc<Sentinelle>> = OnceLock::new();
        GLOBALE.get_or_init(|| {
            Sentinelle::demarrer(
                PERIODE_SENTINELLE,
                Box::new(|s: &Signalement| {
                    tracing::warn!(
                        lieu = %s.detenteur.lieu,
                        fil = %s.detenteur.fil,
                        tid = s.detenteur.tid,
                        tenue_ms = s.detenteur.depuis.as_millis() as u64,
                        attentes = s.attentes,
                        pile = s.detenteur.pile.as_deref().unwrap_or("non capturée"),
                        "ecriture_sqlite_toujours_tenue"
                    );
                }),
            )
        })
    }

    /// Démarrer une sentinelle sur son propre fil. Le fil s'arrête quand la
    /// dernière référence à la sentinelle tombe.
    pub fn demarrer(periode: Duration, puits: Puits) -> Arc<Sentinelle> {
        let s = Arc::new(Sentinelle {
            surveilles: Mutex::new(Vec::new()),
            puits,
        });
        let faible = Arc::downgrade(&s);
        let _ = std::thread::Builder::new()
            .name("tune-verrou-veille".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(periode);
                    let Some(s) = faible.upgrade() else { return };
                    s.tour();
                }
            });
        s
    }

    pub fn surveiller(&self, verrou: &VerrouEcriture) {
        if let Ok(mut v) = self.surveilles.lock() {
            v.retain(|w| w.strong_count() > 0);
            v.push(Arc::downgrade(&verrou.etat));
        }
    }

    /// Les relevés de tous les verrous encore vivants.
    pub fn releves(&self) -> Vec<ReleveVerrou> {
        self.vivants().iter().map(|e| releve_de(e)).collect()
    }

    fn vivants(&self) -> Vec<Arc<Etat>> {
        self.surveilles
            .lock()
            .map(|v| v.iter().filter_map(Weak::upgrade).collect())
            .unwrap_or_default()
    }

    fn tour(&self) {
        for etat in self.vivants() {
            if let Some(s) = examiner(&etat) {
                (self.puits)(&s);
            }
        }
    }
}

/// Une détention à signaler ? La première fois au seuil, puis toutes les
/// [`RAPPEL_DETENTION`].
fn examiner(etat: &Etat) -> Option<Signalement> {
    let attentes = etat.attentes.lock().map(|a| a.len()).unwrap_or(0);
    let mut d = etat.detention.lock().ok()?;
    let prise = d.as_mut()?;
    if prise.depuis.elapsed() < etat.seuil {
        return None;
    }
    let du = prise
        .dernier_signalement
        .is_none_or(|t| t.elapsed() >= RAPPEL_DETENTION);
    if !du {
        return None;
    }
    prise.dernier_signalement = Some(Instant::now());
    let photo = prise.photo();
    drop(d);
    armer_les_piles();
    Some(Signalement {
        detenteur: photo,
        attentes,
    })
}

#[cfg(test)]
#[path = "verrou_ecriture_tests_4924.rs"]
mod tests;
