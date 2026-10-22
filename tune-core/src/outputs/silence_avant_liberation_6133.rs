//! #6133 — silence garanti dans les tampons ASIO dès l'arrêt, et pendant toute
//! l'attente de libération du pilote.
//!
//! Le défaut. ASIO tourne sur un **double tampon** que le pilote rejoue tant
//! qu'il n'est pas arrêté (`ASIOStop`). cpal (vendorisé) ne fait rien d'autre,
//! au `pause()`, que lever un drapeau : son rappel rend la main **sans rien
//! écrire**, et les deux moitiés gardent ce qu'on y a mis en dernier. Or, au
//! Stop, `LocalOutput::stop()` arme `force_silent`, que le rappel ASIO exclusif
//! ne lit pas, et remet `paused` à `false`. Le rappel vide donc l'anneau
//! (encore plein de musique) jusqu'à `release()`, qui appelle `pause()` tout de
//! suite. Les deux moitiés restent pleines de musique, et le pilote les rejoue
//! en boucle pendant l'attente de libération (`teardown_settle_for` : 400 ms à
//! 96 kHz, 600 ms à 192 kHz), jusqu'à la destruction du `Driver`. Le résultat
//! est un bourdonnement à l'arrêt (Didier, SMSL SU-8, forum 2215).
//!
//! Le correctif. Avant de rendre le flux, on **arme** le silence : le rappel
//! écrit des zéros et compte ses blocs. On attend qu'au moins
//! [`BLOCS_MUETS_REQUIS`] blocs muets soient passés (les deux moitiés, plus une
//! marge : le bloc compté en dernier peut être encore en cours d'écriture), et
//! seulement alors on met en pause et on libère. Ce qui reste dans le double
//! tampon, et que le pilote rejoue jusqu'à `ASIOStop`, ce sont des zéros.
//!
//! Ce module ne dépend ni de Windows ni de cpal : le banc plus bas le prouve
//! sur un faux pilote à double tampon qui se comporte comme cpal au `pause()`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Blocs muets à voir passer avant de libérer. Deux couvrent les deux moitiés
/// du double tampon. Le troisième garantit que le deuxième est **fini** : le
/// rappel compte un bloc au début, avant de l'écrire.
pub(crate) const BLOCS_MUETS_REQUIS: u32 = 3;

/// Borne de l'attente. Un tampon ASIO dépasse rarement 8 192 trames, soit
/// 186 ms à 44,1 kHz : trois blocs tiennent dans 600 ms. Si le pilote ne
/// rappelle plus (pilote bloqué, appareil débranché), on libère quand même.
pub(crate) const BORNE_SILENCE_AVANT_LIBERATION: Duration = Duration::from_millis(600);

/// Le drapeau de silence de libération et son compteur de blocs muets,
/// partagés entre `release()` et le rappel temps réel.
#[derive(Debug, Default)]
pub(crate) struct SilenceAvantLiberation {
    arme: AtomicBool,
    blocs_muets: AtomicU32,
}

impl SilenceAvantLiberation {
    /// Dans le rappel, à la place de `paused.load(...)` : `true` = écrire des
    /// zéros dans tout le bloc et rendre la main. Compte le bloc quand le
    /// silence de libération est armé. Sans verrou ni allocation : on est sur le
    /// fil du pilote.
    #[inline]
    pub(crate) fn doit_se_taire(&self, en_pause: bool) -> bool {
        if self.arme.load(Ordering::Acquire) {
            self.blocs_muets.fetch_add(1, Ordering::AcqRel);
            return true;
        }
        en_pause
    }

    /// Arme le silence, attend que le rappel ait écrit
    /// [`BLOCS_MUETS_REQUIS`] blocs muets (ou que `borne` soit écoulée), puis
    /// appelle `liberer` (pause et abandon du flux). Rend `true` si le silence a
    /// été confirmé avant la libération.
    pub(crate) fn taire_puis_liberer(&self, borne: Duration, liberer: impl FnOnce()) -> bool {
        self.blocs_muets.store(0, Ordering::Release);
        self.arme.store(true, Ordering::Release);
        let debut = Instant::now();
        let confirme = loop {
            if self.blocs_muets.load(Ordering::Acquire) >= BLOCS_MUETS_REQUIS {
                break true;
            }
            if debut.elapsed() >= borne {
                break false;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        liberer();
        confirme
    }

    /// Blocs muets écrits depuis l'armement (journal).
    pub(crate) fn blocs_muets(&self) -> u32 {
        self.blocs_muets.load(Ordering::Acquire)
    }
}

/// #6133 — le rappel ASIO en DSD natif : le bloc d'octets DSD tiré de
/// l'anneau, ou le silence DSD (0x69) quand la zone est en pause **ou que le
/// silence de libération est armé**. Rend `true` en cas de famine, comme
/// [`crate::audio::dsd_brut::remplir_tampon_dsd`].
///
/// Le chemin DSD natif rendait son flux par `pause()` puis abandon, sans rien
/// armer : le pilote rejouait le dernier bloc DSD jusqu'à `ASIOStop`, le même
/// défaut que le PCM avant le correctif #6140.
#[inline]
pub(crate) fn remplir_le_bloc_dsd_natif(
    anneau: &crate::audio::dsd_brut::AnneauDsd,
    silence: &SilenceAvantLiberation,
    en_pause: bool,
    tampon: &mut [u8],
) -> bool {
    crate::audio::dsd_brut::remplir_tampon_dsd(anneau, silence.doit_se_taire(en_pause), tampon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// #6133, volet DSD natif : anneau plein de musique, zone pas en pause.
    /// Silence de libération armé, le rappel écrit le silence DSD et compte
    /// ses blocs. Sans armement, il sort la musique.
    #[test]
    fn dsd_natif_le_rappel_arme_ecrit_le_silence_dsd_6133() {
        use crate::audio::dsd_brut::{AnneauDsd, SILENCE_DSD_MSB_FIRST};
        const MUSIQUE_DSD: u8 = 0xA5;
        let anneau = AnneauDsd::new(4096);
        anneau.pousser(&[MUSIQUE_DSD; 4096]);
        let garde = SilenceAvantLiberation::default();
        let mut bloc = [0u8; 256];
        assert!(!remplir_le_bloc_dsd_natif(
            &anneau, &garde, false, &mut bloc
        ));
        assert!(
            bloc.iter().all(|&o| o == MUSIQUE_DSD),
            "hors arrêt, la musique sort"
        );

        // L'armement, sans attendre : `taire_puis_liberer` sur un fil à part,
        // ce fil-ci joue le pilote.
        let garde = Arc::new(garde);
        let anneau = Arc::new(anneau);
        let fil = {
            let garde = garde.clone();
            std::thread::spawn(move || {
                garde.taire_puis_liberer(BORNE_SILENCE_AVANT_LIBERATION, || {})
            })
        };
        let debut = Instant::now();
        let mut blocs = Vec::new();
        while !fil.is_finished() && debut.elapsed() < Duration::from_secs(2) {
            let mut bloc = [0u8; 256];
            remplir_le_bloc_dsd_natif(&anneau, &garde, false, &mut bloc);
            blocs.push(bloc);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            fil.join().unwrap(),
            "le silence doit être confirmé avant la libération"
        );
        let derniers: Vec<_> = blocs
            .iter()
            .rev()
            .take(BLOCS_MUETS_REQUIS as usize)
            .collect();
        assert_eq!(derniers.len(), BLOCS_MUETS_REQUIS as usize);
        for bloc in derniers {
            assert!(
                bloc.iter().all(|&o| o == SILENCE_DSD_MSB_FIRST),
                "#6133 DSD : le double tampon doit finir sur le silence DSD, pas sur la musique"
            );
        }
        assert!(
            anneau.disponible() > 0,
            "l'anneau n'est plus vidé une fois le silence armé"
        );
    }

    const MUSIQUE: i32 = 0x2000_0000;
    const TRAMES: usize = 64;

    type Rappel = Box<dyn FnMut(&mut [i32]) + Send>;

    /// Faux pilote ASIO : deux moitiés, alternées à chaque « bufferSwitch ».
    /// Comme cpal vendorisé : rappel retiré ou flux en pause = la moitié n'est
    /// PAS réécrite, et le pilote la rejoue telle quelle.
    struct FauxPilote {
        moities: Mutex<[Vec<i32>; 2]>,
        en_lecture: AtomicBool,
        rappel: Mutex<Option<Rappel>>,
        tourne: AtomicBool,
    }

    impl FauxPilote {
        fn demarrer(rappel: Rappel) -> (Arc<Self>, std::thread::JoinHandle<()>) {
            let pilote = Arc::new(Self {
                moities: Mutex::new([vec![0; TRAMES], vec![0; TRAMES]]),
                en_lecture: AtomicBool::new(true),
                rappel: Mutex::new(Some(rappel)),
                tourne: AtomicBool::new(true),
            });
            let fil = {
                let pilote = pilote.clone();
                std::thread::spawn(move || {
                    let mut index = 0usize;
                    while pilote.tourne.load(Ordering::Acquire) {
                        index ^= 1;
                        let mut rappel = pilote.rappel.lock().unwrap();
                        if let Some(rappel) = rappel.as_mut()
                            && pilote.en_lecture.load(Ordering::Acquire)
                        {
                            let mut bloc = vec![0; TRAMES];
                            rappel(&mut bloc);
                            pilote.moities.lock().unwrap()[index] = bloc;
                        }
                        drop(rappel);
                        std::thread::sleep(Duration::from_millis(2));
                    }
                })
            };
            (pilote, fil)
        }

        /// `stream.pause()` puis `drop(stream)` : drapeau baissé, rappel retiré.
        fn pause_et_abandon(&self) {
            self.en_lecture.store(false, Ordering::Release);
            *self.rappel.lock().unwrap() = None;
        }

        /// Ce que le pilote rejoue pendant l'attente de libération.
        fn ce_qui_est_rejoue(&self) -> Vec<i32> {
            let moities = self.moities.lock().unwrap();
            moities.iter().flatten().copied().collect()
        }

        fn arreter(&self) {
            self.tourne.store(false, Ordering::Release);
        }
    }

    /// Le rappel de Tune, réduit à l'essentiel : la musique tant qu'on ne doit
    /// pas se taire (l'anneau est plein, `underruns=0` dans le journal).
    fn rappel_tune(garde: Arc<SilenceAvantLiberation>) -> Rappel {
        Box::new(move |bloc: &mut [i32]| {
            if garde.doit_se_taire(false) {
                bloc.fill(0);
                return;
            }
            bloc.fill(MUSIQUE);
        })
    }

    fn laisser_jouer() {
        std::thread::sleep(Duration::from_millis(30));
    }

    /// Le défaut, tel qu'il était : pause puis abandon sans silence préalable.
    /// Les deux moitiés gardent la musique et le pilote la rejoue en boucle
    /// jusqu'à `ASIOStop`.
    #[test]
    fn sans_silence_prealable_le_pilote_rejoue_la_musique_apres_l_arret_6133() {
        let garde = Arc::new(SilenceAvantLiberation::default());
        let (pilote, fil) = FauxPilote::demarrer(rappel_tune(garde));
        laisser_jouer();
        pilote.pause_et_abandon();
        laisser_jouer(); // l'attente de libération : le pilote tourne encore
        let rejoue = pilote.ce_qui_est_rejoue();
        pilote.arreter();
        fil.join().unwrap();
        assert!(
            rejoue.iter().all(|&s| s == MUSIQUE),
            "le faux pilote doit reproduire le défaut : sans silence, la musique reste dans le double tampon"
        );
    }

    /// Le correctif : silence armé, trois blocs muets, puis pause et abandon.
    /// Ce que le pilote rejoue jusqu'à `ASIOStop`, ce sont des zéros.
    #[test]
    fn apres_l_arret_le_double_tampon_ne_contient_que_des_zeros_6133() {
        let garde = Arc::new(SilenceAvantLiberation::default());
        let (pilote, fil) = FauxPilote::demarrer(rappel_tune(garde.clone()));
        laisser_jouer();
        let confirme =
            garde.taire_puis_liberer(BORNE_SILENCE_AVANT_LIBERATION, || pilote.pause_et_abandon());
        laisser_jouer();
        let rejoue = pilote.ce_qui_est_rejoue();
        pilote.arreter();
        fil.join().unwrap();
        let non_nuls = rejoue.iter().filter(|&&s| s != 0).count();
        assert_eq!(
            non_nuls, 0,
            "#6133 : après l'arrêt, le pilote ASIO rejoue {non_nuls} échantillons de musique \
             au lieu du silence (double tampon non réécrit avant pause)"
        );
        assert!(
            confirme,
            "le silence doit être confirmé avant la libération"
        );
        assert!(garde.blocs_muets() >= BLOCS_MUETS_REQUIS);
    }

    /// Pilote qui ne rappelle plus : l'attente est bornée et la libération a
    /// lieu quand même.
    #[test]
    fn un_pilote_muet_ne_bloque_pas_la_liberation_6133() {
        let garde = SilenceAvantLiberation::default();
        let mut libere = false;
        let debut = Instant::now();
        let confirme = garde.taire_puis_liberer(Duration::from_millis(50), || libere = true);
        assert!(!confirme);
        assert!(libere);
        assert!(debut.elapsed() < Duration::from_secs(1));
    }

    /// Hors libération, le rappel suit la pause comme avant.
    #[test]
    fn sans_armement_le_rappel_suit_la_pause_6133() {
        let garde = SilenceAvantLiberation::default();
        assert!(garde.doit_se_taire(true));
        assert!(!garde.doit_se_taire(false));
        assert_eq!(garde.blocs_muets(), 0);
    }
}
