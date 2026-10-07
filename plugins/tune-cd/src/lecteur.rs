//! L'abstraction « lecteur de disque ».
//!
//! Trois implémentations : Linux (ioctl sur `/dev/sr*`, `linux.rs`), macOS
//! (le volume `cddafs` monté sous `/Volumes`, `cddafs.rs` et `macos.rs`) et
//! Windows (`IOCTL_CDROM_READ_TOC_EX` et `IOCTL_CDROM_RAW_READ`, `windows.rs`)
//! et simulée (en mémoire, pour les tests, `simule.rs`). Tout ce qui est au-dessus — flux,
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

/// Pourquoi un disque n'a pas été éjecté (fil 2135 : un Apple SuperDrive
/// n'a pas de bouton d'éjection, Tune doit pouvoir le faire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurEjection {
    /// Aucun lecteur branché.
    AucunLecteur,
    /// Le lecteur est déjà vide.
    AucunDisque,
    /// Ce lecteur (ou cette plateforme) ne sait pas commander l'éjection.
    NonPrisEnCharge,
    /// Le système a refusé ou échoué (disque occupé, ioctl refusé…).
    Echec(String),
}

impl fmt::Display for ErreurEjection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErreurEjection::AucunLecteur => write!(f, "aucun lecteur de CD branché"),
            ErreurEjection::AucunDisque => write!(f, "aucun disque dans le lecteur"),
            ErreurEjection::NonPrisEnCharge => {
                write!(f, "ce lecteur ne sait pas éjecter le disque")
            }
            ErreurEjection::Echec(r) => write!(f, "{r}"),
        }
    }
}

pub trait LecteurDisque: Send + Sync {
    /// Le chemin du périphérique, pour l'affichage (`/dev/sr0`).
    fn chemin(&self) -> String;
    /// Change quand le lecteur physique suivi est remplacé. Les lecteurs
    /// fixes gardent la valeur par défaut ; le lecteur branchable suit ses
    /// acquisitions, même si la présence reste `Disque` entre deux sondages.
    fn generation_lecteur(&self) -> u64 {
        0
    }
    /// Présence du lecteur et du disque. Doit rester bon marché : elle est
    /// interrogée chaque seconde pendant une lecture pour voir l'éjection.
    fn presence(&self) -> Presence;
    /// La table des pistes du disque inséré.
    fn lire_toc(&self) -> Result<Toc, ErreurCd>;
    /// Lit `nombre` secteurs audio bruts à partir de `lba` dans `sortie`, qui
    /// mesure exactement `nombre × 2 352` octets.
    fn lire_secteurs(&self, lba: u32, nombre: u32, sortie: &mut [u8]) -> Result<(), ErreurCd>;
    /// Éjecte le disque de CE lecteur. Bloquant : à appeler hors de la
    /// boucle asynchrone. Par défaut, non pris en charge.
    fn ejecter_disque(&self) -> Result<(), ErreurEjection> {
        Err(ErreurEjection::NonPrisEnCharge)
    }
}

/// Le lecteur du système, s'il y en a un que Tune sait lire.
///
/// Linux : `TUNE_CD_DEVICE` impose un périphérique ; sinon un
/// [`LecteurBranchable`] qui cherche parmi TOUS les `/dev/srN` celui qui
/// contient un disque, à défaut le premier — et le cherche ENCORE tant qu'il
/// n'y en a pas (#5161 : un lecteur USB branché après le démarrage de Tune,
/// ou un `/dev/sr0` créé par udev après le service, n'était jamais vu).
/// Fil 2135 : avec deux lecteurs branchés, seul le premier `/dev/sr*` était
/// lu, et un disque mis dans l'autre restait invisible ; un lecteur courant
/// VIDE cède désormais la place à celui qui a un disque
/// ([`LecteurBranchable::preferant_le_disque`]).
///
/// macOS : toujours un lecteur, dont la présence dit « aucun lecteur »,
/// « vide » ou « disque » (un lecteur USB se branche à chaud) ;
/// `TUNE_CD_DEVICE` y impose un DOSSIER de volume.
///
/// Windows : lettres de lecteur optique recherchées à chaque branchement ;
/// `TUNE_CD_DEVICE` impose un chemin de périphérique (par ex. `\\.\D:`).
pub fn lecteur_du_systeme() -> Option<Arc<dyn LecteurDisque>> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(chemin) = std::env::var("TUNE_CD_DEVICE") {
            return Some(Arc::new(crate::linux::LecteurLinux::new(chemin)));
        }
        Some(Arc::new(
            LecteurBranchable::new(
                "/dev/sr*",
                INTERVALLE_DE_RECHERCHE,
                Box::new(|| {
                    preferer_un_disque(
                        crate::linux::peripheriques_optiques(std::path::Path::new("/dev"))
                            .into_iter()
                            .map(|c| {
                                Arc::new(crate::linux::LecteurLinux::new(c))
                                    as Arc<dyn LecteurDisque>
                            }),
                    )
                }),
            )
            .preferant_le_disque(),
        ))
    }
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(crate::macos::lecteur_du_systeme()))
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(chemin) = std::env::var("TUNE_CD_DEVICE") {
            return Some(Arc::new(crate::windows::LecteurWindows::new(chemin)));
        }
        Some(Arc::new(LecteurBranchable::new(
            r"\\.\*:",
            INTERVALLE_DE_RECHERCHE,
            Box::new(|| {
                crate::windows::premier_lecteur().map(|c| {
                    Arc::new(crate::windows::LecteurWindows::new(c)) as Arc<dyn LecteurDisque>
                })
            }),
        )))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Au plus une recherche de périphérique par intervalle, quel que soit le
/// nombre d'appels (surveillance toutes les 3 s, `/etat` à chaque écran).
pub const INTERVALLE_DE_RECHERCHE: Duration = Duration::from_secs(2);

/// Ce qui trouve le lecteur branché, s'il y en a un.
pub type Recherche = Box<dyn Fn() -> Option<Arc<dyn LecteurDisque>> + Send + Sync>;

/// Parmi plusieurs lecteurs, dans l'ordre : le premier qui a un disque, sinon
/// le premier vide, sinon le premier tout court (fil 2135).
pub fn preferer_un_disque(
    candidats: impl IntoIterator<Item = Arc<dyn LecteurDisque>>,
) -> Option<Arc<dyn LecteurDisque>> {
    let mut vide = None;
    let mut premier = None;
    for l in candidats {
        match l.presence() {
            Presence::Disque => return Some(l),
            Presence::Vide if vide.is_none() => vide = Some(l),
            _ if premier.is_none() => premier = Some(l),
            _ => {}
        }
    }
    vide.or(premier)
}

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
    /// Fil 2135 : un lecteur courant VIDE est-il remplacé par un autre qui a
    /// un disque ? (Linux, où plusieurs `/dev/srN` coexistent.)
    preferer_le_disque: bool,
    etat: Mutex<EtatRecherche>,
}

#[derive(Default)]
struct EtatRecherche {
    courant: Option<Arc<dyn LecteurDisque>>,
    derniere_recherche: Option<Instant>,
    generation: u64,
}

impl LecteurBranchable {
    pub fn new(motif: impl Into<String>, intervalle: Duration, recherche: Recherche) -> Self {
        Self {
            motif: motif.into(),
            intervalle,
            recherche,
            preferer_le_disque: false,
            etat: Mutex::new(EtatRecherche::default()),
        }
    }

    /// Tant que le lecteur courant est VIDE, la recherche est relancée (au
    /// plus une fois par intervalle) ; si elle trouve un AUTRE lecteur qui a
    /// un disque, il devient le lecteur courant (fil 2135). Un lecteur qui a
    /// un disque n'est jamais quitté : une lecture en cours n'est pas
    /// interrompue par un disque inséré ailleurs.
    pub fn preferant_le_disque(mut self) -> Self {
        self.preferer_le_disque = true;
        self
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
            if e.courant.is_some() {
                e.generation = e.generation.wrapping_add(1);
            }
            if let Some(l) = &e.courant {
                tracing::info!(lecteur = %l.chemin(), "cd_lecteur_detecte");
            }
        }
        e.courant.clone()
    }

    /// `vide` est le lecteur courant et n'a pas de disque : passe à un autre
    /// lecteur qui en a un, s'il y en a un (fil 2135). Rend le nouveau.
    fn basculer_vers_un_disque(
        &self,
        vide: &Arc<dyn LecteurDisque>,
    ) -> Option<Arc<dyn LecteurDisque>> {
        if !self.preferer_le_disque {
            return None;
        }
        let mut e = self.etat.lock().unwrap_or_else(|p| p.into_inner());
        if !e.courant.as_ref().is_some_and(|c| Arc::ptr_eq(c, vide))
            || e.derniere_recherche
                .is_some_and(|t| t.elapsed() < self.intervalle)
        {
            return None;
        }
        e.derniere_recherche = Some(Instant::now());
        let autre = (self.recherche)()?;
        if autre.chemin() == vide.chemin() || autre.presence() != Presence::Disque {
            return None;
        }
        tracing::info!(
            precedent = %vide.chemin(),
            lecteur = %autre.chemin(),
            "cd_lecteur_avec_disque_choisi"
        );
        e.courant = Some(autre.clone());
        e.generation = e.generation.wrapping_add(1);
        Some(autre)
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

    fn generation_lecteur(&self) -> u64 {
        self.etat
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation
    }

    fn presence(&self) -> Presence {
        let Some(l) = self.courant() else {
            return Presence::AucunLecteur;
        };
        let p = l.presence();
        if p == Presence::Vide && self.basculer_vers_un_disque(&l).is_some() {
            return Presence::Disque;
        }
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

    /// Éjecte le disque du lecteur COURANT — celui que `presence` et la
    /// lecture suivent, donc celui qui a un disque s'il y en a un (#5739,
    /// fil 2135). Jamais un autre lecteur de la machine.
    fn ejecter_disque(&self) -> Result<(), ErreurEjection> {
        if self.presence() == Presence::AucunLecteur {
            return Err(ErreurEjection::AucunLecteur);
        }
        self.courant()
            .ok_or(ErreurEjection::AucunLecteur)?
            .ejecter_disque()
    }
}

/// La plateforme a-t-elle une implémentation ?
pub const fn plateforme_prise_en_charge() -> bool {
    cfg!(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows"
    ))
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

    /// Fil 2135 — un lecteur factice NOMMÉ (`/dev/sr0`, `/dev/sr1`) dont on
    /// règle la présence : deux lecteurs branchés en même temps.
    pub(crate) struct LecteurNomme {
        nom: &'static str,
        presence: Mutex<Presence>,
    }

    impl LecteurNomme {
        fn new(nom: &'static str, presence: Presence) -> Arc<Self> {
            Arc::new(Self {
                nom,
                presence: Mutex::new(presence),
            })
        }
        fn mettre(&self, p: Presence) {
            *self.presence.lock().unwrap() = p;
        }
    }

    impl LecteurDisque for LecteurNomme {
        fn chemin(&self) -> String {
            self.nom.into()
        }
        fn presence(&self) -> Presence {
            *self.presence.lock().unwrap()
        }
        fn lire_toc(&self) -> Result<Toc, ErreurCd> {
            match self.presence() {
                Presence::Disque => Ok(toc_du_vecteur()),
                _ => Err(ErreurCd::AucunDisque),
            }
        }
        fn lire_secteurs(&self, _: u32, _: u32, _: &mut [u8]) -> Result<(), ErreurCd> {
            Err(ErreurCd::AucunDisque)
        }
        fn ejecter_disque(&self) -> Result<(), ErreurEjection> {
            let mut p = self.presence.lock().unwrap();
            match *p {
                Presence::AucunLecteur => Err(ErreurEjection::AucunLecteur),
                Presence::Vide => Err(ErreurEjection::AucunDisque),
                Presence::Disque => {
                    *p = Presence::Vide;
                    Ok(())
                }
            }
        }
    }

    /// Le « système » Linux à deux lecteurs : même recherche et même mode
    /// que `lecteur_du_systeme`, sur des lecteurs factices.
    fn deux_lecteurs(
        sr0: Presence,
        sr1: Presence,
    ) -> (
        Arc<LecteurNomme>,
        Arc<LecteurNomme>,
        Arc<AtomicUsize>,
        LecteurBranchable,
    ) {
        let a = LecteurNomme::new("/dev/sr0", sr0);
        let b = LecteurNomme::new("/dev/sr1", sr1);
        let recherches = Arc::new(AtomicUsize::new(0));
        let (ca, cb, n) = (a.clone(), b.clone(), recherches.clone());
        let l = LecteurBranchable::new(
            "/dev/sr*",
            Duration::ZERO,
            Box::new(move || {
                n.fetch_add(1, Ordering::SeqCst);
                preferer_un_disque([
                    ca.clone() as Arc<dyn LecteurDisque>,
                    cb.clone() as Arc<dyn LecteurDisque>,
                ])
            }),
        )
        .preferant_le_disque();
        (a, b, recherches, l)
    }

    /// Fil 2135 : deux lecteurs branchés, le disque dans le SECOND. Avant,
    /// le premier `/dev/sr*` était pris et le disque restait invisible.
    #[test]
    fn le_lecteur_qui_a_un_disque_est_prefere_au_premier() {
        let (_a, _b, _, l) = deux_lecteurs(Presence::Vide, Presence::Disque);
        assert_eq!(l.presence(), Presence::Disque);
        assert_eq!(l.chemin(), "/dev/sr1");
        assert!(l.lire_toc().is_ok());
    }

    /// Fil 2135 : les deux sont vides au démarrage, puis le disque entre
    /// dans le second. Le lecteur courant (vide) lui cède la place, et la
    /// génération change pour que la surveillance republie la source.
    #[test]
    fn un_disque_insere_dans_l_autre_lecteur_est_suivi() {
        let (_a, b, _, l) = deux_lecteurs(Presence::Vide, Presence::Vide);
        assert_eq!(l.presence(), Presence::Vide);
        assert_eq!(l.chemin(), "/dev/sr0");
        let g = l.generation_lecteur();

        b.mettre(Presence::Disque);
        assert_eq!(l.presence(), Presence::Disque);
        assert_eq!(l.chemin(), "/dev/sr1");
        assert_ne!(l.generation_lecteur(), g);
        assert!(l.lire_toc().is_ok());
    }

    /// Un lecteur qui a un disque n'est jamais quitté (une lecture en cours
    /// ne saute pas d'un lecteur à l'autre), et il n'est plus recherché.
    #[test]
    fn le_lecteur_qui_a_un_disque_n_est_pas_quitte() {
        let (_a, _b, recherches, l) = deux_lecteurs(Presence::Disque, Presence::Disque);
        assert_eq!(l.presence(), Presence::Disque);
        assert_eq!(l.chemin(), "/dev/sr0");
        let (g, n) = (l.generation_lecteur(), recherches.load(Ordering::SeqCst));
        for _ in 0..5 {
            assert_eq!(l.presence(), Presence::Disque);
        }
        assert_eq!(l.chemin(), "/dev/sr0");
        assert_eq!(l.generation_lecteur(), g);
        assert_eq!(recherches.load(Ordering::SeqCst), n);
    }

    /// Aucun disque nulle part : le lecteur courant est gardé, sans
    /// changement de génération (pas de republication à chaque tour).
    #[test]
    fn sans_disque_nulle_part_le_lecteur_courant_est_garde() {
        let (_a, _b, _, l) = deux_lecteurs(Presence::Vide, Presence::Vide);
        assert_eq!(l.presence(), Presence::Vide);
        let g = l.generation_lecteur();
        for _ in 0..5 {
            assert_eq!(l.presence(), Presence::Vide);
        }
        assert_eq!(l.chemin(), "/dev/sr0");
        assert_eq!(l.generation_lecteur(), g);
    }

    /// Le lecteur courant est débranché : il est oublié, et l'autre, où l'on
    /// vient de mettre un disque, est retrouvé.
    #[test]
    fn le_lecteur_courant_debranche_cede_la_place_a_l_autre() {
        let (a, b, _, l) = deux_lecteurs(Presence::Vide, Presence::Disque);
        assert_eq!(l.presence(), Presence::Disque);
        assert_eq!(l.chemin(), "/dev/sr1");
        b.mettre(Presence::AucunLecteur);
        a.mettre(Presence::Disque);
        assert_eq!(l.presence(), Presence::Disque);
        assert_eq!(l.chemin(), "/dev/sr0");
    }

    /// Sans `preferant_le_disque` (Windows), rien ne change : le lecteur
    /// trouvé est gardé tant qu'il est branché.
    #[test]
    fn sans_preference_le_premier_lecteur_trouve_est_garde() {
        let a = LecteurNomme::new("A:", Presence::Vide);
        let b = LecteurNomme::new("B:", Presence::Disque);
        let (ca, cb) = (a.clone(), b.clone());
        let premier = AtomicBool::new(true);
        let l = LecteurBranchable::new(
            "*",
            Duration::ZERO,
            Box::new(move || {
                Some(if premier.swap(false, Ordering::SeqCst) {
                    ca.clone() as Arc<dyn LecteurDisque>
                } else {
                    cb.clone() as Arc<dyn LecteurDisque>
                })
            }),
        );
        assert_eq!(l.presence(), Presence::Vide);
        assert_eq!(l.presence(), Presence::Vide);
        assert_eq!(l.chemin(), "A:");
    }

    /// Fil 2135 — deux lecteurs, le disque dans le SECOND : l'éjection vise
    /// le lecteur qui a le disque (celui que la lecture suit), pas le premier
    /// `/dev/sr*`, et le premier n'est pas touché.
    #[test]
    fn l_ejection_vise_le_lecteur_qui_a_le_disque() {
        let (a, b, _, l) = deux_lecteurs(Presence::Vide, Presence::Disque);
        assert_eq!(l.presence(), Presence::Disque);
        assert_eq!(l.ejecter_disque(), Ok(()));
        assert_eq!(b.presence(), Presence::Vide, "/dev/sr1 éjecté");
        assert_eq!(a.presence(), Presence::Vide, "/dev/sr0 intact");
        assert_eq!(l.presence(), Presence::Vide);
        // Plus de disque nulle part : le dire, ne rien tenter.
        assert_eq!(l.ejecter_disque(), Err(ErreurEjection::AucunDisque));
    }

    /// Deux disques : le lecteur courant (`/dev/sr0`) est éjecté, l'autre
    /// reste ; la présence passe alors au disque de `/dev/sr1`.
    #[test]
    fn avec_deux_disques_seul_le_lecteur_courant_est_ejecte() {
        let (a, b, _, l) = deux_lecteurs(Presence::Disque, Presence::Disque);
        assert_eq!(l.presence(), Presence::Disque);
        assert_eq!(l.chemin(), "/dev/sr0");
        assert_eq!(l.ejecter_disque(), Ok(()));
        assert_eq!(a.presence(), Presence::Vide);
        assert_eq!(b.presence(), Presence::Disque);
        assert_eq!(l.presence(), Presence::Disque);
        assert_eq!(l.chemin(), "/dev/sr1");
    }

    #[test]
    fn sans_lecteur_branche_l_ejection_le_dit() {
        let systeme = Arc::new(SystemeFactice::default());
        let l = systeme.lecteur(Duration::ZERO);
        assert_eq!(l.ejecter_disque(), Err(ErreurEjection::AucunLecteur));
    }

    #[test]
    fn preferer_un_disque_suit_l_ordre_disque_vide_absent() {
        let absent = LecteurNomme::new("absent", Presence::AucunLecteur);
        let vide = LecteurNomme::new("vide", Presence::Vide);
        let disque = LecteurNomme::new("disque", Presence::Disque);
        let choix = |v: &[&Arc<LecteurNomme>]| {
            preferer_un_disque(v.iter().map(|l| (*l).clone() as Arc<dyn LecteurDisque>))
                .map(|l| l.chemin())
        };
        assert_eq!(choix(&[&absent, &vide, &disque]).as_deref(), Some("disque"));
        assert_eq!(choix(&[&absent, &vide]).as_deref(), Some("vide"));
        assert_eq!(choix(&[&absent]).as_deref(), Some("absent"));
        assert_eq!(choix(&[]), None);
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
