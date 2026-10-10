//! La politique de cession à la lecture : réglable, appliquée, comptée
//! (#4681, suite de #4699).
//!
//! #4699 a posé le témoin de lecture et une cadence réduite (5 s entre deux
//! éléments d'une passe freinée). Il restait quatre trous, que ce module
//! referme sans rien dupliquer :
//!
//! | Pendant qu'une zone joue…                 | Réglage                    | Défaut |
//! |-------------------------------------------|----------------------------|--------|
//! | pause entre deux éléments d'une passe     | `pause_between_items_ms`   | 5 000  |
//! | le scan lit peu de fichiers à la fois     | `scan_width`               | 2      |
//! | … et marque une pause entre deux paquets  | `pause_between_files_ms`   | 20     |
//! | une connexion de lecture SQLite réservée  | `sqlite_reserved_readers`  | 1      |
//! | l'écrivain de fond laisse passer d'abord  | `writer_yield_max_ms`      | 100    |
//! | priorité d'E/S basse (Linux, macOS)       | `low_io_priority`          | vrai   |
//! | tout ce qui précède                       | `enabled`                  | vrai   |
//!
//! Le scan lisait jusqu'à 32 fichiers à la fois PENDANT la lecture, entre ses
//! pauses de lot ; le pool de lecture SQLite n'a que trois connexions, et une
//! passe de fond pouvait les tenir toutes pendant que la file ou l'état d'une
//! zone attendait (#5438 : 0,7 à 1,3 s d'attente pour une lecture triviale).
//!
//! ## Qui est « de fond »
//!
//! Un FIL marqué ([`marquer_le_fil_de_fond`]) : les fils de lecture du scan,
//! et tout travail parti par [`super::hors_du_fil_async`] (les écritures des
//! passes). Le reste — l'API, la file, l'état des zones, l'historique — n'est
//! pas marqué : c'est lui qui passe en premier.
//!
//! ## Ce que `enabled = false` ne touche PAS
//!
//! Le témoin de lecture reste exact, et les passes qui DÉCODENT (ReplayGain,
//! empreintes, plage dynamique, CLAP) continuent de tout céder à la lecture,
//! comme la passe d'album : ce sont des gardes de correction, pas des
//! réglages de confort.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// La clé du réglage dans `settings`, écrite par `PATCH /system/config` (un
/// objet JSON, voir [`Politique`]).
pub const CLE_REGLAGE: &str = "background_playback_policy";

/// La politique, telle que `PATCH /system/config` la reçoit et que
/// `GET /system/background-tasks` la publie. Un champ absent prend sa valeur
/// par défaut ; un champ inconnu est refusé.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Politique {
    /// Interrupteur général des freins ci-dessous.
    pub enabled: bool,
    /// Pause entre deux éléments d'une passe freinée (enrichissement, images,
    /// lots de scan, rapports DR), en millisecondes.
    pub pause_between_items_ms: u64,
    /// Pause du scan entre deux paquets de [`Self::scan_width`] fichiers.
    pub pause_between_files_ms: u64,
    /// Combien de fichiers le scan lit à la fois tant qu'une zone joue (32 au
    /// repos, selon le disque).
    pub scan_width: usize,
    /// Connexions de lecture SQLite qu'un fil de fond ne peut pas prendre
    /// pendant la lecture (le pool en a trois).
    pub sqlite_reserved_readers: usize,
    /// Combien de temps un écrivain de fond laisse passer les écrivains de
    /// premier plan qui attendent déjà le verrou d'écriture.
    pub writer_yield_max_ms: u64,
    /// Priorité d'E/S basse pour les lectures de fichiers de fond, là où le
    /// système le permet sans privilège ET de façon réversible.
    pub low_io_priority: bool,
}

/// Les valeurs par défaut.
pub const DEFAUT: Politique = Politique {
    enabled: true,
    pause_between_items_ms: 5_000,
    pause_between_files_ms: 20,
    scan_width: 2,
    sqlite_reserved_readers: 1,
    writer_yield_max_ms: 100,
    low_io_priority: true,
};

impl Default for Politique {
    fn default() -> Self {
        DEFAUT
    }
}

/// Bornes admises — refusées au-delà, jamais ramenées en silence.
pub const PAUSE_ELEMENTS_MAX_MS: u64 = 60_000;
pub const PAUSE_FICHIERS_MAX_MS: u64 = 1_000;
pub const LARGEUR_SCAN_MAX: usize = 32;
/// Le pool de lecture SQLite a trois connexions : en réserver trois
/// interdirait toute lecture de fond pendant la lecture.
pub const RESERVE_SQLITE_MAX: usize = 2;
pub const CESSION_ECRIVAIN_MAX_MS: u64 = 1_000;

impl Politique {
    /// Vérifier les bornes. Le message nomme le champ et sa borne.
    pub fn valider(&self) -> Result<(), String> {
        let refus = |champ: &str, borne: String| Err(format!("{champ} : {borne}"));
        if self.pause_between_items_ms > PAUSE_ELEMENTS_MAX_MS {
            return refus(
                "pause_between_items_ms",
                format!("0 à {PAUSE_ELEMENTS_MAX_MS}"),
            );
        }
        if self.pause_between_files_ms > PAUSE_FICHIERS_MAX_MS {
            return refus(
                "pause_between_files_ms",
                format!("0 à {PAUSE_FICHIERS_MAX_MS}"),
            );
        }
        if self.scan_width == 0 || self.scan_width > LARGEUR_SCAN_MAX {
            return refus("scan_width", format!("1 à {LARGEUR_SCAN_MAX}"));
        }
        if self.sqlite_reserved_readers > RESERVE_SQLITE_MAX {
            return refus(
                "sqlite_reserved_readers",
                format!("0 à {RESERVE_SQLITE_MAX}"),
            );
        }
        if self.writer_yield_max_ms > CESSION_ECRIVAIN_MAX_MS {
            return refus(
                "writer_yield_max_ms",
                format!("0 à {CESSION_ECRIVAIN_MAX_MS}"),
            );
        }
        Ok(())
    }

    /// Lire une politique depuis le JSON d'un `PATCH`. `null` vaut le défaut.
    pub fn depuis_json(v: &serde_json::Value) -> Result<Self, String> {
        if v.is_null() {
            return Ok(DEFAUT);
        }
        // La boucle générique du PATCH range un objet en texte : on accepte
        // aussi ce texte, pour relire la base par le même chemin.
        let p: Politique = match v.as_str() {
            Some(texte) => serde_json::from_str(texte).map_err(|e| e.to_string())?,
            None => serde_json::from_value(v.clone()).map_err(|e| e.to_string())?,
        };
        p.valider()?;
        Ok(p)
    }

    /// La pause entre deux éléments, en `Duration`.
    pub fn pause_entre_elements(&self) -> Duration {
        Duration::from_millis(self.pause_between_items_ms)
    }
}

static POLITIQUE: RwLock<Politique> = RwLock::new(DEFAUT);

/// La politique en vigueur. Une lecture de verrou, sans requête.
pub fn politique() -> Politique {
    *POLITIQUE.read().unwrap_or_else(|e| e.into_inner())
}

/// Mettre une politique en vigueur, MAINTENANT. Journalise en INFO ce qui
/// change (et rien si rien ne change).
pub fn regler(p: Politique) {
    let avant = {
        let mut g = POLITIQUE.write().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut *g, p)
    };
    if avant != p {
        tracing::info!(
            enabled = p.enabled,
            pause_between_items_ms = p.pause_between_items_ms,
            pause_between_files_ms = p.pause_between_files_ms,
            scan_width = p.scan_width,
            sqlite_reserved_readers = p.sqlite_reserved_readers,
            writer_yield_max_ms = p.writer_yield_max_ms,
            low_io_priority = p.low_io_priority,
            "politique_de_lecture_reglee"
        );
    }
}

/// Relire le réglage en base au démarrage. Absent : le défaut, sans bruit.
/// Illisible ou hors bornes : le défaut, avec un avertissement — une ligne mal
/// écrite ne doit ni empêcher de démarrer ni désarmer les freins.
pub fn hydrater(backend: &Arc<dyn DbBackend>) {
    let brut = SettingsRepo::with_backend(backend.clone())
        .get(CLE_REGLAGE)
        .ok()
        .flatten();
    let p = match brut {
        None => DEFAUT,
        Some(texte) => match Politique::depuis_json(&serde_json::Value::String(texte)) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(erreur = %e, "politique_de_lecture_illisible_defaut_retenu");
                DEFAUT
            }
        },
    };
    regler(p);
}

// ---------------------------------------------------------------------------
// Les fils de fond
// ---------------------------------------------------------------------------

thread_local! {
    static FOND: Cell<u32> = const { Cell::new(0) };
}

/// Le fil courant travaille pour une passe de fond tant que cette garde vit.
/// Ni `Send` ni `Sync` : elle appartient au fil qui l'a prise.
pub struct FilDeFond {
    _pas_send: PhantomData<*const ()>,
}

impl Drop for FilDeFond {
    fn drop(&mut self) {
        FOND.with(|f| f.set(f.get().saturating_sub(1)));
    }
}

/// Marquer le fil courant comme travaillant pour une passe de fond.
pub fn marquer_le_fil_de_fond() -> FilDeFond {
    FOND.with(|f| f.set(f.get() + 1));
    FilDeFond {
        _pas_send: PhantomData,
    }
}

/// Le fil courant travaille-t-il pour une passe de fond ?
pub fn fil_de_fond() -> bool {
    FOND.with(|f| f.get() > 0)
}

/// La politique s'applique-t-elle maintenant ? Une zone joue et les freins
/// sont armés.
pub fn freins_actifs() -> Option<Politique> {
    if !super::lecture_en_cours() {
        return None;
    }
    let p = politique();
    p.enabled.then_some(p)
}

/// La politique s'applique-t-elle maintenant AU FIL COURANT ?
pub fn freins_actifs_pour_ce_fil() -> Option<Politique> {
    if !fil_de_fond() {
        return None;
    }
    freins_actifs()
}

// ---------------------------------------------------------------------------
// Le scan : moins de fichiers à la fois, une pause entre deux paquets
// ---------------------------------------------------------------------------

/// Combien de fichiers le scan lit à la fois MAINTENANT : `Some(n)` pendant la
/// lecture, `None` au repos (le scan garde sa largeur à lui).
pub fn largeur_du_scan() -> Option<usize> {
    freins_actifs().map(|p| p.scan_width.max(1))
}

/// Pause du scan entre deux paquets de fichiers, pendant la lecture. Rend
/// `true` si elle a dormi. Bloquante : le scan lit sur des fils à lui.
pub fn ceder_entre_deux_fichiers_bloquant(id: &'static str, fichiers: usize) -> bool {
    let Some(p) = freins_actifs() else {
        return false;
    };
    super::noter_cedee(id);
    COMPTEURS
        .fichiers_cedes
        .fetch_add(fichiers as u64, Ordering::Relaxed);
    if p.pause_between_files_ms > 0 {
        std::thread::sleep(Duration::from_millis(p.pause_between_files_ms));
    }
    true
}

// ---------------------------------------------------------------------------
// SQLite : la lecture passe d'abord
// ---------------------------------------------------------------------------

/// Combien de connexions de lecture un fil de fond peut tenir ENSEMBLE, sur
/// un pool de `taille` : `Some(plafond)` si le fil courant est de fond et
/// qu'une zone joue, `None` sinon (pas de plafond).
pub fn plafond_des_lectures_de_fond(taille: usize) -> Option<usize> {
    let p = freins_actifs_pour_ce_fil()?;
    if p.sqlite_reserved_readers == 0 {
        return None;
    }
    Some(taille.saturating_sub(p.sqlite_reserved_readers).max(1))
}

/// Combien de temps un écrivain de fond laisse passer les écrivains de
/// premier plan : `Some(durée)` si le fil courant est de fond et qu'une zone
/// joue.
pub fn cession_de_l_ecrivain() -> Option<Duration> {
    let p = freins_actifs_pour_ce_fil()?;
    (p.writer_yield_max_ms > 0).then(|| Duration::from_millis(p.writer_yield_max_ms))
}

/// Noter qu'une lecture de fond a attendu la réserve.
pub fn noter_lecture_differee(attente: Duration) {
    COMPTEURS.lectures_differees.fetch_add(1, Ordering::Relaxed);
    COMPTEURS
        .lectures_differees_ms
        .fetch_add(attente.as_millis() as u64, Ordering::Relaxed);
}

/// Noter qu'un écrivain de fond a laissé passer un écrivain de premier plan.
pub fn noter_ecriture_differee(attente: Duration) {
    COMPTEURS
        .ecritures_differees
        .fetch_add(1, Ordering::Relaxed);
    COMPTEURS
        .ecritures_differees_ms
        .fetch_add(attente.as_millis() as u64, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Priorité d'E/S basse, réversible
// ---------------------------------------------------------------------------

/// Garde de priorité basse du fil courant, rendue au `drop`.
///
/// Seulement ce qui se défait sans privilège :
/// * **Linux** : classe d'E/S best-effort niveau 7 (`ioprio_set`), puis la
///   valeur d'avant. PAS de `nice` : un compte ordinaire ne peut pas le
///   relever ensuite, et ces fils servent d'autres travaux (pool bloquant).
///   Les fils du scan sont déjà à `nice 10` + best-effort 7 en permanence.
/// * **macOS** : l'état « arrière-plan » du fil (`PRIO_DARWIN_BG`), qui
///   abaisse le processeur ET bride les E/S, puis l'état normal.
/// * Ailleurs : rien.
///
/// Jamais sur un fil qui écrit en base : voir le module parent (inversion de
/// priorité sur le verrou d'écriture unique).
pub struct PrioriteBasse {
    #[cfg(target_os = "linux")]
    precedente: Option<libc::c_long>,
    #[cfg(target_os = "macos")]
    active: bool,
    _pas_send: PhantomData<*const ()>,
}

impl PrioriteBasse {
    /// Une garde qui ne fait rien.
    fn neutre() -> Self {
        PrioriteBasse {
            #[cfg(target_os = "linux")]
            precedente: None,
            #[cfg(target_os = "macos")]
            active: false,
            _pas_send: PhantomData,
        }
    }

    /// La priorité a-t-elle effectivement été baissée ?
    pub fn appliquee(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.precedente.is_some()
        }
        #[cfg(target_os = "macos")]
        {
            self.active
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            false
        }
    }
}

#[cfg(target_os = "linux")]
mod ioprio {
    pub const CLASSE_BE: libc::c_long = 2;
    pub const CLASSE_IDLE: libc::c_long = 3;
    pub const DECALAGE: libc::c_long = 13;
    pub const QUI_PROCESSUS: libc::c_long = 1;

    pub fn tid() -> libc::c_long {
        // SAFETY: gettid n'a ni argument ni effet de bord.
        unsafe { libc::syscall(libc::SYS_gettid) }
    }
    pub fn lire() -> libc::c_long {
        // SAFETY: lecture de la priorité d'E/S du fil courant.
        unsafe { libc::syscall(libc::SYS_ioprio_get, QUI_PROCESSUS, tid()) }
    }
    pub fn ecrire(valeur: libc::c_long) -> bool {
        // SAFETY: modifie la priorité d'E/S du seul fil courant.
        unsafe { libc::syscall(libc::SYS_ioprio_set, QUI_PROCESSUS, tid(), valeur) == 0 }
    }
}

/// Baisser la priorité d'E/S du fil courant si une zone joue et que la
/// politique le demande ; sinon une garde neutre.
pub fn baisser_pendant_la_lecture() -> PrioriteBasse {
    match freins_actifs() {
        Some(p) if p.low_io_priority => baisser(),
        _ => PrioriteBasse::neutre(),
    }
}

fn baisser() -> PrioriteBasse {
    #[cfg(target_os = "linux")]
    {
        let avant = ioprio::lire();
        if avant < 0 {
            return PrioriteBasse::neutre();
        }
        let classe = avant >> ioprio::DECALAGE;
        let niveau = avant & 0xff;
        // Déjà aussi bas (les fils du scan, ou un fil en classe IDLE).
        if classe == ioprio::CLASSE_IDLE || (classe == ioprio::CLASSE_BE && niveau >= 7) {
            return PrioriteBasse::neutre();
        }
        if !ioprio::ecrire((ioprio::CLASSE_BE << ioprio::DECALAGE) | 7) {
            return PrioriteBasse::neutre();
        }
        COMPTEURS.priorite_basse.fetch_add(1, Ordering::Relaxed);
        PrioriteBasse {
            precedente: Some(avant),
            _pas_send: PhantomData,
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: change l'état d'arrière-plan du seul fil courant (who = 0).
        let ok =
            unsafe { libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG) == 0 };
        if ok {
            COMPTEURS.priorite_basse.fetch_add(1, Ordering::Relaxed);
        }
        PrioriteBasse {
            active: ok,
            _pas_send: PhantomData,
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        PrioriteBasse::neutre()
    }
}

impl Drop for PrioriteBasse {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(avant) = self.precedente.take() {
            ioprio::ecrire(avant);
        }
        #[cfg(target_os = "macos")]
        if self.active {
            // SAFETY: rend au fil courant son état normal.
            unsafe {
                libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, 0);
            }
        }
    }
}

/// La priorité d'E/S du fil courant, telle que le noyau la voit (Linux :
/// `(classe, niveau)`), pour les témoins. `None` ailleurs.
pub fn priorite_d_e_s_du_fil() -> Option<(i64, i64)> {
    #[cfg(target_os = "linux")]
    {
        let v = ioprio::lire();
        (v >= 0).then_some(((v >> ioprio::DECALAGE) as i64, (v & 0xff) as i64))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// Les compteurs
// ---------------------------------------------------------------------------

struct Compteurs {
    fichiers_cedes: AtomicU64,
    lectures_differees: AtomicU64,
    lectures_differees_ms: AtomicU64,
    ecritures_differees: AtomicU64,
    ecritures_differees_ms: AtomicU64,
    priorite_basse: AtomicU64,
}

static COMPTEURS: Compteurs = Compteurs {
    fichiers_cedes: AtomicU64::new(0),
    lectures_differees: AtomicU64::new(0),
    lectures_differees_ms: AtomicU64::new(0),
    ecritures_differees: AtomicU64::new(0),
    ecritures_differees_ms: AtomicU64::new(0),
    priorite_basse: AtomicU64::new(0),
};

/// Ce que la politique a fait depuis le démarrage — la mesure qui relie une
/// coupure à un frein, ou à son absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ReleveDesFreins {
    /// Fichiers que le scan a lus en paquet réduit, suivis d'une pause.
    pub scan_files_throttled: u64,
    /// Lectures SQLite de fond qui ont attendu la réserve, et combien en tout.
    pub sqlite_reads_deferred: u64,
    pub sqlite_reads_deferred_ms: u64,
    /// Écritures de fond qui ont laissé passer un écrivain de premier plan.
    pub sqlite_writes_deferred: u64,
    pub sqlite_writes_deferred_ms: u64,
    /// Fois où la priorité d'E/S d'un fil a effectivement été baissée.
    pub low_io_priority_applied: u64,
}

pub fn releve_des_freins() -> ReleveDesFreins {
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    ReleveDesFreins {
        scan_files_throttled: l(&COMPTEURS.fichiers_cedes),
        sqlite_reads_deferred: l(&COMPTEURS.lectures_differees),
        sqlite_reads_deferred_ms: l(&COMPTEURS.lectures_differees_ms),
        sqlite_writes_deferred: l(&COMPTEURS.ecritures_differees),
        sqlite_writes_deferred_ms: l(&COMPTEURS.ecritures_differees_ms),
        low_io_priority_applied: l(&COMPTEURS.priorite_basse),
    }
}

/// Remettre la politique par défaut et les compteurs à zéro (essais).
pub fn oublier_pour_les_essais() {
    *POLITIQUE.write().unwrap_or_else(|e| e.into_inner()) = DEFAUT;
    for a in [
        &COMPTEURS.fichiers_cedes,
        &COMPTEURS.lectures_differees,
        &COMPTEURS.lectures_differees_ms,
        &COMPTEURS.ecritures_differees,
        &COMPTEURS.ecritures_differees_ms,
        &COMPTEURS.priorite_basse,
    ] {
        a.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_defaut_fait_l_aller_retour_et_les_bornes_sont_tenues() {
        let json = serde_json::to_value(DEFAUT).unwrap();
        assert_eq!(Politique::depuis_json(&json).unwrap(), DEFAUT);
        assert_eq!(
            Politique::depuis_json(&serde_json::Value::Null).unwrap(),
            DEFAUT
        );
        // Un champ seul : le reste par défaut.
        let p = Politique::depuis_json(&serde_json::json!({"scan_width": 4})).unwrap();
        assert_eq!(p.scan_width, 4);
        assert_eq!(p.pause_between_items_ms, DEFAUT.pause_between_items_ms);
        // Le texte que la boucle du PATCH range en base se relit.
        let texte = serde_json::Value::String(r#"{"enabled":false}"#.into());
        assert!(!Politique::depuis_json(&texte).unwrap().enabled);
        for mauvais in [
            serde_json::json!({"scan_width": 0}),
            serde_json::json!({"scan_width": 33}),
            serde_json::json!({"sqlite_reserved_readers": 3}),
            serde_json::json!({"pause_between_items_ms": 60_001}),
            serde_json::json!({"pause_between_files_ms": 1_001}),
            serde_json::json!({"writer_yield_max_ms": 1_001}),
            serde_json::json!({"turbo": true}),
            serde_json::json!("pas du json"),
            serde_json::json!(12),
        ] {
            assert!(Politique::depuis_json(&mauvais).is_err(), "{mauvais}");
        }
    }

    #[test]
    fn le_marquage_du_fil_s_empile_et_se_defait() {
        assert!(!fil_de_fond());
        {
            let _a = marquer_le_fil_de_fond();
            assert!(fil_de_fond());
            {
                let _b = marquer_le_fil_de_fond();
                assert!(fil_de_fond());
            }
            assert!(fil_de_fond());
        }
        assert!(!fil_de_fond());
        // Un autre fil n'hérite de rien.
        let _a = marquer_le_fil_de_fond();
        assert!(!std::thread::spawn(fil_de_fond).join().unwrap());
    }
}
