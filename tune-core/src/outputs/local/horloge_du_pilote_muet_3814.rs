//! #3814 — une sourdine n'est pas un rappel mort, et l'horloge du pilote doit
//! savoir les distinguer.
//!
//! # Ce que ce banc mesure
//!
//! Le sondeur tranche entre « le pilote ne réclame plus rien » et « tout va
//! bien » sur UN rapport : `duree_ms` (l'horloge du pilote, tirée de
//! `RingStarvation::stream_ms`) contre `ecoule_ms` (le temps réel). Or
//! `stream_ms` ne s'alimente que depuis `RingBuf::pop_mapped`, et le rappel
//! cpal PARTAGÉ a deux sorties anticipées qui rendent la main avant `pop` :
//! la sourdine établie (pause, silence forcé) et la garde de
//! pré-remplissage.
//!
//! Conséquence, avant le correctif : pendant toute une sourdine le rappel
//! tourne, sert ses périodes, remplit le DAC de zéros — et l'horloge du
//! pilote ne bouge pas d'une milliseconde. Le sondeur y lit la signature
//! exacte d'un rappel mort et écrit `rappel_pilote_arrete`, « la panne est en
//! aval de l'anneau », sur une sortie intacte.
//!
//! C'est le faux rouge que #3814 pose comme sa preuve centrale : `duree_ms=128`
//! sur au moins une seconde de temps réel, lu comme « le rappel ne tourne
//! plus ». Sur le chemin de Dimitri — macOS, zone non exclusive, donc cpal
//! partagé — cette lecture n'était pas sûre.
//!
//! # Pourquoi ce banc-ci et pas un test de texte
//!
//! Rien n'est simulé : l'anneau est le `RingBuf` de production, le compteur
//! est le `RingStarvation` que le rappel incrémente vraiment, le rappel est
//! `render_local_shared_f32_callback` — le corps réel des DEUX flux cpal
//! partagés — et le verdict est rendu par le `SuiviFamine` du sondeur, celui
//! qui écrit la ligne. Le seul élément absent est cpal lui-même, qui ne fait
//! qu'appeler le rappel période après période : c'est ce que fait la boucle.
//!
//! # Contre-épreuve
//!
//! [`un_rappel_reellement_arrete_est_toujours_denonce`] tient l'autre bord :
//! si le correctif se contentait de compter toutes les périodes, un rappel
//! qui cesse d'être appelé — le vrai défaut que le détecteur cherche —
//! deviendrait invisible. Il ne l'est pas : personne n'appelle plus le
//! rappel, donc personne ne compte, et `rappel_pilote_arrete` tombe.

use super::*;
use crate::poller::decisions::{FamineAnneau, SuiviFamine};

const TAUX: u32 = 44_100;
const CANAUX: u16 = 2;
/// Échantillons entrelacés d'une période. 512 trames stéréo ≈ 11,6 ms — la
/// taille de période que CoreAudio sert couramment sur un DAC USB.
const PERIODE: usize = 1_024;
/// Périodes à jouer pour couvrir une seconde de temps réel à cette cadence.
/// 88 200 échantillons entrelacés par seconde / 1 024 ≈ 86.
const PERIODES_PAR_SECONDE: usize = 86;

/// Le banc : l'anneau de production, son compteur, et les témoins que le
/// rappel relit à chaque période.
struct Banc {
    ring: Arc<RingBuf>,
    famine: Arc<RingStarvation>,
    volume: Arc<AtomicU32>,
    paused: Arc<AtomicBool>,
    silent: Arc<AtomicBool>,
    data_started: Arc<AtomicBool>,
    ramp: crate::audio::soft_mute::SoftMuteRamp,
    sortie: Vec<f32>,
}

impl Banc {
    fn monter() -> Self {
        let famine = Arc::new(RingStarvation::new());
        famine.begin_stream(TAUX, CANAUX);
        let capacite = TAUX as usize * CANAUX as usize * 2;
        let ring = Arc::new(RingBuf::new_metered(capacite, famine.clone()));
        Self {
            ring,
            famine,
            volume: Arc::new(AtomicU32::new(1_000)),
            paused: Arc::new(AtomicBool::new(false)),
            silent: Arc::new(AtomicBool::new(false)),
            // Le pré-remplissage est déjà franchi : ce banc mesure la
            // sourdine EN COURS de lecture, pas le démarrage.
            data_started: Arc::new(AtomicBool::new(true)),
            ramp: crate::audio::soft_mute::SoftMuteRamp::new(TAUX, CANAUX),
            sortie: vec![0.0; PERIODE],
        }
    }

    /// Remettre dans l'anneau de quoi servir `periodes` périodes entières.
    fn nourrir(&self, periodes: usize) {
        let bloc = vec![0.25f32; PERIODE * periodes];
        self.ring.push(&bloc);
    }

    /// UNE période du rappel réel. `armed_ms = 0` : rampe désarmée, coupure
    /// franche — l'état de DoP, de PURE et de sortie exclusive, et celui que
    /// toute rampe atteint de toute façon en 20 ms. La sourdine est donc
    /// établie dès la première période, ce qui rend le banc déterministe.
    fn periode(&mut self) -> usize {
        render_local_shared_f32_callback(
            &self.ring,
            &self.volume,
            &self.paused,
            &self.silent,
            &self.data_started,
            &mut self.ramp,
            0,
            0,
            &mut self.sortie,
        )
    }

    fn periodes(&mut self, combien: usize) {
        for _ in 0..combien {
            self.periode();
        }
    }

    /// Armer le compteur : `RingStarvation::record` ne compte rien tant qu'un
    /// premier rappel n'a pas été servi EN ENTIER. Sans ça le banc mesurerait
    /// le silence de démarrage, pas la sourdine.
    fn armer(&mut self) {
        self.nourrir(4);
        self.periodes(4);
        assert!(
            self.famine.snapshot().stream_ms > 0,
            "le compteur doit être armé avant que le banc mesure quoi que ce soit"
        );
    }

    fn stream_ms(&self) -> u64 {
        self.famine.snapshot().stream_ms
    }
}

/// Le cœur du dossier : une sourdine de trois secondes, rappel bien vivant,
/// ne doit PAS être dénoncée comme un pilote arrêté.
///
/// Trois secondes et pas une : `TICKS_AVANT_ARRET = 3` laisse passer deux
/// ticks en retard avant d'écrire quoi que ce soit — c'est la garde posée
/// pour les trous inter-pistes. Le faux rouge n'apparaît qu'au troisième.
///
/// ROUGE avant le correctif : `stream_ms` reste figé pendant les trois
/// secondes, `SuiviFamine` lit 0 ms d'horloge de pilote sur 3 000 ms de temps
/// réel et rend `RappelArrete`.
#[test]
fn une_sourdine_etablie_nest_pas_un_rappel_arrete() {
    let mut banc = Banc::monter();
    let mut suivi = SuiviFamine::default();

    banc.armer();
    suivi.observer(banc.famine.snapshot(), 0);

    // Silence forcé : le rappel continue d'être appelé à la même cadence, il
    // remplit simplement le tampon de zéros sans tirer de l'anneau.
    banc.silent.store(true, Ordering::Relaxed);

    let mut constats = Vec::new();
    for _ in 0..3 {
        banc.nourrir(PERIODES_PAR_SECONDE);
        banc.periodes(PERIODES_PAR_SECONDE);
        if let Some(constat) = suivi.observer(banc.famine.snapshot(), 1_000) {
            constats.push(constat);
        }
    }

    let arrets: Vec<_> = constats
        .iter()
        .filter(|c| matches!(c, FamineAnneau::RappelArrete(_)))
        .collect();
    assert!(
        arrets.is_empty(),
        "un rappel vivant mais muet a été dénoncé comme arrêté : {constats:?}"
    );
}

/// La même sourdine, vue à la source : l'horloge du pilote doit avancer à peu
/// près comme le temps réel, parce que le pilote a bel et bien consommé ces
/// périodes.
///
/// Le témoin ci-dessus prouve que le sondeur ne dit plus de bêtise ; celui-ci
/// prouve POURQUOI, et le ferait échouer un correctif qui se contenterait de
/// museler la ligne dans le sondeur au lieu de réparer le compteur.
#[test]
fn lhorloge_du_pilote_avance_pendant_une_sourdine() {
    let mut banc = Banc::monter();
    banc.armer();
    let avant = banc.stream_ms();

    banc.silent.store(true, Ordering::Relaxed);
    banc.nourrir(PERIODES_PAR_SECONDE);
    banc.periodes(PERIODES_PAR_SECONDE);

    let avance = banc.stream_ms() - avant;
    assert!(
        (900..=1_100).contains(&avance),
        "une seconde de sourdine doit avancer l'horloge du pilote d'environ \
         1 000 ms, elle a avancé de {avance} ms"
    );
}

/// Une sourdine n'est pas une famine d'anneau, et le correctif ne doit pas
/// remplacer un faux rouge par un autre : ni `events` ni `missing_samples` ne
/// bougent quand le rappel se tait délibérément.
#[test]
fn une_sourdine_nest_pas_comptee_comme_une_famine() {
    let mut banc = Banc::monter();
    banc.armer();
    let avant = banc.famine.snapshot();

    banc.silent.store(true, Ordering::Relaxed);
    banc.periodes(PERIODES_PAR_SECONDE);

    let apres = banc.famine.snapshot();
    assert_eq!(
        apres.events, avant.events,
        "une sourdine délibérée n'est pas un rappel servi à court"
    );
    assert_eq!(
        apres.missing_samples, avant.missing_samples,
        "une sourdine délibérée n'envoie pas d'échantillons manquants au bilan"
    );
}

/// La forme exacte de la ligne de Dimitri : une micro-famine d'anneau, puis
/// une sourdine, le tout sur le même flux.
///
/// Avant le correctif, l'épisode se refermait sur un `duree_ms` ridicule —
/// 128 ms chez lui — parce que la seconde de sourdine ne comptait pas, et le
/// verdict basculait sur « le rappel s'est arrêté ». Après, l'épisode se
/// referme sur `Fin` avec une durée honnête : la famine a bien eu lieu, elle
/// a bien été brève, et le pilote n'a jamais cessé de tourner.
#[test]
fn une_microfamine_suivie_dune_sourdine_se_ferme_sur_fin() {
    let mut banc = Banc::monter();
    let mut suivi = SuiviFamine::default();

    banc.armer();
    suivi.observer(banc.famine.snapshot(), 0);

    // L'anneau se vide : quelques périodes servies à court, comme les quatre
    // de Dimitri.
    banc.periodes(4);
    let ouverture = suivi.observer(banc.famine.snapshot(), 1_000);
    assert!(
        matches!(ouverture, Some(FamineAnneau::Debut(_))),
        "la famine d'anneau doit s'ouvrir : {ouverture:?}"
    );

    // Puis le rappel se tait — et continue de tourner.
    banc.silent.store(true, Ordering::Relaxed);
    banc.nourrir(PERIODES_PAR_SECONDE);
    banc.periodes(PERIODES_PAR_SECONDE);

    let fermeture = suivi.observer(banc.famine.snapshot(), 1_000);
    match fermeture {
        Some(FamineAnneau::Fin(ep)) => assert!(
            ep.duree_ms >= 900,
            "l'épisode se ferme sur une horloge de pilote crédible, pas sur \
             les 128 ms de #3814 : duree_ms={} ecoule_ms={}",
            ep.duree_ms,
            ep.ecoule_ms
        ),
        autre => panic!("un rappel vivant doit fermer l'épisode sur `Fin`, pas sur {autre:?}"),
    }
}

/// Contre-épreuve : le détecteur voit-il encore ce qu'il a été écrit pour
/// voir ?
///
/// Ici le rappel n'est plus appelé du tout — le cas que #3814 soupçonne chez
/// Dimitri, et celui que Yacine a produit sur ALSA le 13/09. Aucune période
/// n'est servie, donc rien n'est compté, et `rappel_pilote_arrete` doit
/// tomber au troisième tick.
///
/// Sans ce témoin, « compter les périodes muettes » pourrait être écrit d'une
/// manière qui compte aussi les périodes qui n'existent pas, et le détecteur
/// serait aveugle sans que rien ne le dise.
#[test]
fn un_rappel_reellement_arrete_est_toujours_denonce() {
    let mut banc = Banc::monter();
    let mut suivi = SuiviFamine::default();

    banc.armer();
    suivi.observer(banc.famine.snapshot(), 0);

    // Le rappel cesse d'être appelé : aucune période, mais le temps passe.
    let mut constats = Vec::new();
    for _ in 0..3 {
        if let Some(constat) = suivi.observer(banc.famine.snapshot(), 1_000) {
            constats.push(constat);
        }
    }

    assert!(
        constats
            .iter()
            .any(|c| matches!(c, FamineAnneau::RappelArrete(_))),
        "un rappel qui ne tourne plus doit toujours être dénoncé : {constats:?}"
    );
}
