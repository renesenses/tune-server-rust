//! L'abstraction « lecteur de disque ».
//!
//! Trois implémentations : Linux (ioctl sur `/dev/sr*`, `linux.rs`), macOS
//! (le volume `cddafs` monté sous `/Volumes`, `cddafs.rs` et `macos.rs`) et
//! simulée (en mémoire, pour les tests, `simule.rs`). Windows
//! (`IOCTL_CDROM_RAW_READ`) reste à faire. Tout ce qui est au-dessus — flux,
//! identifiant, routes — ne connaît que ce trait.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::toc::Toc;

/// Ce que le lecteur dit de lui-même.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// Le périphérique n'existe pas (ou ne s'ouvre pas).
    AucunLecteur,
    /// Le lecteur est là, sans disque lisible (tiroir ouvert, vide, pas prêt).
    Vide,
    /// Un disque est inséré et prêt.
    Disque,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurCd {
    /// Plus de disque : tiroir ouvert, éjection. Ne se rejoue pas.
    AucunDisque,
    /// Échec de lecture d'une plage de secteurs. Se rejoue.
    Lecture { lba: u32, raison: String },
    /// Toute autre erreur (périphérique absent, TOC illisible…).
    Autre(String),
}

impl fmt::Display for ErreurCd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErreurCd::AucunDisque => write!(f, "aucun disque dans le lecteur"),
            ErreurCd::Lecture { lba, raison } => {
                write!(f, "lecture du secteur {lba} impossible : {raison}")
            }
            ErreurCd::Autre(r) => write!(f, "{r}"),
        }
    }
}

pub trait LecteurDisque: Send + Sync {
    /// Le chemin du périphérique, pour l'affichage (`/dev/sr0`).
    fn chemin(&self) -> String;
    /// Présence du lecteur et du disque. Doit rester bon marché : elle est
    /// interrogée chaque seconde pendant une lecture pour voir l'éjection.
    fn presence(&self) -> Presence;
    /// La table des pistes du disque inséré.
    fn lire_toc(&self) -> Result<Toc, ErreurCd>;
    /// Lit `nombre` secteurs audio bruts à partir de `lba` dans `sortie`, qui
    /// mesure exactement `nombre × 2 352` octets.
    fn lire_secteurs(&self, lba: u32, nombre: u32, sortie: &mut [u8]) -> Result<(), ErreurCd>;
}

/// Le lecteur du système, s'il y en a un que Tune sait lire.
///
/// Linux : `TUNE_CD_DEVICE` impose un périphérique ; sinon un
/// [`LecteurBranchable`] qui cherche le premier `/dev/sr0..3` présent, comme
/// la détection de `/cd-rip/drives` — et le cherche ENCORE tant qu'il n'y en
/// a pas (#5161 : un lecteur USB branché après le démarrage de Tune, ou un
/// `/dev/sr0` créé par udev après le service, n'était jamais vu).
///
/// macOS : toujours un lecteur, dont la présence dit « aucun lecteur »,
/// « vide » ou « disque » (un lecteur USB se branche à chaud) ;
/// `TUNE_CD_DEVICE` y impose un DOSSIER de volume.
pub fn lecteur_du_systeme() -> Option<Arc<dyn LecteurDisque>> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(chemin) = std::env::var("TUNE_CD_DEVICE") {
            return Some(Arc::new(crate::linux::LecteurLinux::new(chemin)));
        }
        Some(Arc::new(LecteurBranchable::new(
            "/dev/sr*",
            INTERVALLE_DE_RECHERCHE,
            Box::new(|| {
                (0..4)
                    .map(|i| format!("/dev/sr{i}"))
                    .find(|c| std::path::Path::new(c).exists())
                    .map(|c| Arc::new(crate::linux::LecteurLinux::new(c)) as Arc<dyn LecteurDisque>)
            }),
        )))
    }
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(crate::macos::lecteur_du_systeme()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // Windows : pas encore (#4863).
        None
    }
}

/// Au plus une recherche de périphérique par intervalle, quel que soit le
/// nombre d'appels (surveillance toutes les 3 s, `/etat` à chaque écran).
pub const INTERVALLE_DE_RECHERCHE: Duration = Duration::from_secs(2);

/// Ce qui trouve le lecteur branché, s'il y en a un.
pub type Recherche = Box<dyn Fn() -> Option<Arc<dyn LecteurDisque>> + Send + Sync>;

/// Un lecteur qui peut se brancher APRÈS le démarrage (#5161).
///
/// Tant qu'aucun lecteur n'est trouvé, chaque question (`presence`,
/// `lire_toc`…) relance la recherche, au plus une fois par `intervalle`, et
/// répond « aucun lecteur ». Un lecteur trouvé est gardé ; s'il disparaît
/// (débranché), il est oublié et la recherche reprend aussitôt — il peut
/// revenir sous un autre nom (`/dev/sr1`).
pub struct LecteurBranchable {
    /// Ce que `chemin` affiche tant que rien n'est branché.
    motif: String,
    intervalle: Duration,
    recherche: Recherche,
    etat: Mutex<EtatRecherche>,
}

#[derive(Default)]
struct EtatRecherche {
    courant: Option<Arc<dyn LecteurDisque>>,
    derniere_recherche: Option<Instant>,
}

impl LecteurBranchable {
    pub fn new(motif: impl Into<String>, intervalle: Duration, recherche: Recherche) -> Self {
        Self {
            motif: motif.into(),
            intervalle,
            recherche,
            etat: Mutex::new(EtatRecherche::default()),
        }
    }

    /// Le lecteur branché, en le cherchant si l'intervalle est écoulé.
    fn courant(&self) -> Option<Arc<dyn LecteurDisque>> {
        let mut e = self.etat.lock().unwrap_or_else(|p| p.into_inner());
        if e.courant.is_none()
            && e.derniere_recherche
                .is_none_or(|t| t.elapsed() >= self.intervalle)
        {
            e.derniere_recherche = Some(Instant::now());
            e.courant = (self.recherche)();
            if let Some(l) = &e.courant {
                tracing::info!(lecteur = %l.chemin(), "cd_lecteur_detecte");
            }
        }
        e.courant.clone()
    }

    /// Oublie `l` s'il est toujours le lecteur courant ; la prochaine
    /// question relance la recherche sans attendre l'intervalle.
    fn oublier(&self, l: &Arc<dyn LecteurDisque>) {
        let mut e = self.etat.lock().unwrap_or_else(|p| p.into_inner());
        if e.courant.as_ref().is_some_and(|c| Arc::ptr_eq(c, l)) {
            tracing::info!(lecteur = %l.chemin(), "cd_lecteur_debranche");
            e.courant = None;
            e.derniere_recherche = None;
        }
    }
}

impl LecteurDisque for LecteurBranchable {
    fn chemin(&self) -> String {
        let e = self.etat.lock().unwrap_or_else(|p| p.into_inner());
        e.courant
            .as_ref()
            .map(|l| l.chemin())
            .unwrap_or_else(|| self.motif.clone())
    }

    fn presence(&self) -> Presence {
        let Some(l) = self.courant() else {
            return Presence::AucunLecteur;
        };
        let p = l.presence();
        if p != Presence::AucunLecteur {
            return p;
        }
        self.oublier(&l);
        self.courant()
            .map(|l| l.presence())
            .unwrap_or(Presence::AucunLecteur)
    }

    fn lire_toc(&self) -> Result<Toc, ErreurCd> {
        self.courant().ok_or(ErreurCd::AucunDisque)?.lire_toc()
    }

    fn lire_secteurs(&self, lba: u32, nombre: u32, sortie: &mut [u8]) -> Result<(), ErreurCd> {
        self.courant()
            .ok_or(ErreurCd::AucunDisque)?
            .lire_secteurs(lba, nombre, sortie)
    }
}

/// La plateforme a-t-elle une implémentation ?
pub const fn plateforme_prise_en_charge() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

#[cfg(test)]
pub(crate) mod tests {
    //! #5161 — un lecteur branché APRÈS le démarrage du greffon, par un
    //! fournisseur factice : Shrek n'a pas de lecteur.

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::discid::tests::toc_du_vecteur;
    use crate::simule::LecteurSimule;

    /// Un « système » où l'on branche et débranche un lecteur simulé.
    #[derive(Default)]
    pub(crate) struct SystemeFactice {
        pub branche: AtomicBool,
        pub recherches: AtomicUsize,
        /// Le dernier lecteur « branché », pour pouvoir le débrancher.
        pub dernier: Mutex<Option<Arc<LecteurSimule>>>,
    }

    impl SystemeFactice {
        pub(crate) fn lecteur(self: &Arc<Self>, intervalle: Duration) -> LecteurBranchable {
            let s = self.clone();
            LecteurBranchable::new(
                "factice",
                intervalle,
                Box::new(move || {
                    s.recherches.fetch_add(1, Ordering::SeqCst);
                    if !s.branche.load(Ordering::SeqCst) {
                        return None;
                    }
                    let l = Arc::new(LecteurSimule::new(toc_du_vecteur()));
                    *s.dernier.lock().unwrap() = Some(l.clone());
                    Some(l as Arc<dyn LecteurDisque>)
                }),
            )
        }
    }

    #[test]
    fn un_lecteur_branche_apres_le_demarrage_est_trouve() {
        let systeme = Arc::new(SystemeFactice::default());
        let l = systeme.lecteur(Duration::ZERO);
        assert_eq!(l.presence(), Presence::AucunLecteur);
        assert_eq!(l.chemin(), "factice");
        assert!(matches!(l.lire_toc(), Err(ErreurCd::AucunDisque)));

        systeme.branche.store(true, Ordering::SeqCst);
        assert_eq!(l.presence(), Presence::Disque);
        assert_eq!(l.chemin(), "simulé");
        assert!(l.lire_toc().is_ok());
        // Trouvé : on ne le cherche plus.
        let n = systeme.recherches.load(Ordering::SeqCst);
        l.presence();
        assert_eq!(systeme.recherches.load(Ordering::SeqCst), n);
    }

    /// La recherche est BORNÉE : pas plus d'une par intervalle, quel que
    /// soit le nombre de questions.
    #[test]
    fn la_recherche_est_bornee_par_l_intervalle() {
        let systeme = Arc::new(SystemeFactice::default());
        let l = systeme.lecteur(Duration::from_secs(3_600));
        for _ in 0..10 {
            assert_eq!(l.presence(), Presence::AucunLecteur);
        }
        assert_eq!(systeme.recherches.load(Ordering::SeqCst), 1);
    }

    /// Un lecteur débranché puis rebranché est retrouvé.
    #[test]
    fn un_lecteur_debranche_puis_rebranche_est_retrouve() {
        let systeme = Arc::new(SystemeFactice::default());
        systeme.branche.store(true, Ordering::SeqCst);
        let l = systeme.lecteur(Duration::ZERO);
        assert_eq!(l.presence(), Presence::Disque);

        // Débranché : le lecteur simulé courant se dit « aucun lecteur ».
        systeme.branche.store(false, Ordering::SeqCst);
        systeme
            .dernier
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .debrancher();
        assert_eq!(l.presence(), Presence::AucunLecteur);
        assert_eq!(l.chemin(), "factice");

        systeme.branche.store(true, Ordering::SeqCst);
        assert_eq!(l.presence(), Presence::Disque);
    }

    /// Le chemin du SYSTÈME : sans périphérique (Shrek n'a pas de `/dev/sr*`),
    /// Linux rend quand même un lecteur, qui se dit « aucun lecteur » —
    /// avant #5161, `None`, et plus aucune détection jusqu'au redémarrage.
    #[cfg(target_os = "linux")]
    #[test]
    fn sous_linux_il_y_a_toujours_un_lecteur_a_surveiller() {
        if std::env::var("TUNE_CD_DEVICE").is_ok() {
            return;
        }
        let l = lecteur_du_systeme().expect("un lecteur à surveiller, même sans /dev/sr*");
        let branche = (0..4).any(|i| std::path::Path::new(&format!("/dev/sr{i}")).exists());
        if !branche {
            assert_eq!(l.presence(), Presence::AucunLecteur);
            assert_eq!(l.chemin(), "/dev/sr*");
        }
    }
}
