//! Ce qu'un HQPlayer débranché coûte au journal, et en tours de boucle (#2566).
//!
//! Second des deux sites frères que le commit de la v0.9.129 nommait comme
//! **non traités** après avoir borné la branche « repos » du poller. Celui-ci
//! est le plus mal loti des trois : il n'avait **ni plafond de journal, ni
//! recul du tout**.
//!
//! ## Ce qui était écrit
//!
//! `spawn_hqplayer_poller` rappelait `discover_and_register` toutes les
//! soixante secondes, quoi qu'il arrive, et journalisait chaque tour :
//!
//! | état de l'hôte | tours / jour | lignes / jour | niveau |
//! |---|---|---|---|
//! | débranché | 1 440 | 1 440 `hqplayer_poll_failed` | `DEBUG` |
//! | qui répond | 1 440 | 1 440 `hqplayer_poll_registered` | **`INFO`** |
//!
//! Le second cas est le plus grave : `INFO` traverse les filtres par défaut, et
//! une intégration qui marche parfaitement écrivait mille quatre cents lignes
//! par jour pour dire qu'elle marchait toujours.
//!
//! Le sondeur Squeezebox, dix lignes plus haut dans le même fichier, avait déjà
//! le recul — écrit en clair, pour un incident nommé (Yacine, une Daphile
//! éteinte). Ce fichier éprouve la cadence désormais partagée par les deux, et
//! la comptabilité de journal désormais partagée par les trois sites.
//!
//! ## Ce que ça donne
//!
//! | fenêtre 24 h, hôte débranché | tours | lignes |
//! |---|---|---|
//! | avant | 1 440 | 1 440 |
//! | après | **146** | **10** |
//!
//! ## Pourquoi un binaire de test à lui seul, et un seul essai dedans
//!
//! Leçon déjà payée par `tune-core/tests/journal_descriptif_illisible.rs` :
//! `tracing` met en cache, **pour tout le processus**, la décision « ce point
//! d'appel intéresse-t-il quelqu'un ? ». Un abonné posé au milieu d'un binaire
//! qui lance des tests en parallèle se voit priver d'évènements de façon
//! imprévisible, et la capture revient vide sans prévenir.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier ne serait jamais
//! compilé sans sa cible `[[test]]` dans `tune-server/Cargo.toml`. Voir
//! `tests_orphelins.rs`, qui refuse tout fichier non enregistré.
//!
//! ## Aucune attente réelle
//!
//! La cadence est une fonction pure (`prochain_intervalle_sondage`) et la
//! fenêtre de vingt-quatre heures est parcourue par une horloge entière. Rien
//! ne dort : éprouver un recul de dix minutes par de vraies attentes ferait un
//! test qui dure la journée et qui clignote.
use std::sync::{Arc, Mutex};

use tune_core::poller::ECHECS_SONDAGE_DETAILLES;
use tune_core::poller::JournalSondage;
use tune_server::background::{
    SONDAGE_INTERVALLE_BASE_SECS, SONDAGE_INTERVALLE_MAX_SECS, journaliser_echec_hqplayer,
    journaliser_succes_hqplayer, prochain_intervalle_sondage,
};

const HOTE: &str = "192.168.1.42";
const ERREUR: &str = "hqplayer: connection refused";
/// Vingt-quatre heures, la fenêtre sur laquelle le coût se lit.
const FENETRE_SECS: u64 = 24 * 3600;

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn lignes(texte: &str, marqueur: &str) -> usize {
    texte.lines().filter(|l| l.contains(marqueur)).count()
}

fn lignes_d_echec(texte: &str) -> usize {
    lignes(texte, "hqplayer_poll_failed") + lignes(texte, "hqplayer_poll_still_failing")
}

#[test]
fn un_hqplayer_debranche_ne_tourne_plus_ni_ne_parle_toutes_les_minutes() {
    let capture = JournalCapture::default();
    // DEBUG : c'est le niveau du sondeur en échec. Un abonné plus haut ne
    // verrait pas les lignes qu'on compte et rendrait le test vert pour la
    // mauvaise raison.
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    // ── Témoin 1 : un HQPlayer qui répond ne parle qu'une fois ───────────
    //
    // Vert des deux côtés pour ce qui compte — la découverte EST annoncée —,
    // rouge avant pour le reste : c'étaient 1 440 lignes `INFO` par jour.
    let mut journal = JournalSondage::default();
    let mut deja_annonce = false;
    for _ in 0..1_000 {
        journaliser_succes_hqplayer(&mut journal, HOTE, &mut deja_annonce);
    }
    let texte = capture.texte();
    assert_eq!(
        lignes(&texte, "hqplayer_poll_registered"),
        1,
        "mille tours réussis annoncent la découverte UNE fois — journal :\n{texte}"
    );
    assert!(
        texte.contains(HOTE),
        "la ligne de découverte doit nommer l'hôte :\n{texte}"
    );

    // ── Témoin 2 : une vraie panne est toujours dite, en entier ──────────
    //
    // C'est ce qu'on ne doit pas perdre en bornant le volume : la première
    // erreur, avec son texte, son hôte et la cadence de repli.
    let repere = capture.texte().len();
    let mut journal = JournalSondage::default();
    journaliser_echec_hqplayer(&mut journal, HOTE, &ERREUR, 120);
    let texte = capture.texte();
    let texte = &texte[repere..];
    assert_eq!(
        lignes(texte, "hqplayer_poll_failed"),
        1,
        "un échec isolé doit produire une ligne détaillée et une seule — journal :\n{texte}"
    );
    assert!(
        texte.contains(ERREUR),
        "la ligne doit porter l'erreur telle quelle — c'est elle qu'on lit :\n{texte}"
    );
    assert!(
        texte.contains("next_retry_secs=120"),
        "la ligne doit dire quand le prochain essai aura lieu :\n{texte}"
    );

    // ── La cadence, fonction pure ────────────────────────────────────────
    //
    // Le recul double à chaque échec et plafonne ; le moindre succès ramène au
    // plein rythme. Compté, jamais attendu.
    assert_eq!(
        prochain_intervalle_sondage(SONDAGE_INTERVALLE_BASE_SECS, true),
        120,
        "le premier échec double la cadence"
    );
    assert_eq!(prochain_intervalle_sondage(120, true), 240);
    assert_eq!(prochain_intervalle_sondage(240, true), 480);
    assert_eq!(
        prochain_intervalle_sondage(480, true),
        SONDAGE_INTERVALLE_MAX_SECS,
        "le recul plafonne à dix minutes"
    );
    assert_eq!(
        prochain_intervalle_sondage(SONDAGE_INTERVALLE_MAX_SECS, true),
        SONDAGE_INTERVALLE_MAX_SECS,
        "et il y reste : un plancher de fréquence, pas un arrêt"
    );
    assert_eq!(
        prochain_intervalle_sondage(SONDAGE_INTERVALLE_MAX_SECS, false),
        SONDAGE_INTERVALLE_BASE_SECS,
        "un hôte qui revient est rappelé au plein rythme dès le tour suivant"
    );

    // ── La mesure : vingt-quatre heures d'hôte débranché ─────────────────
    //
    // Horloge entière, pas d'attente. On compte les tours ET les lignes.
    let repere = capture.texte().len();
    let mut journal = JournalSondage::default();
    let mut intervalle = SONDAGE_INTERVALLE_BASE_SECS;
    let mut horloge = 0u64;
    let mut tours = 0u64;
    while horloge < FENETRE_SECS {
        tours += 1;
        intervalle = prochain_intervalle_sondage(intervalle, true);
        journaliser_echec_hqplayer(&mut journal, HOTE, &ERREUR, intervalle);
        horloge += intervalle;
    }
    let texte = capture.texte();
    let texte = &texte[repere..];

    let tours_avant = FENETRE_SECS / SONDAGE_INTERVALLE_BASE_SECS;
    assert_eq!(
        tours_avant, 1_440,
        "avant : un tour toutes les 60 s, sans fin"
    );
    assert_eq!(
        tours, 146,
        "après : le recul ramène la journée à 146 tours, soit 146 connexions \
         perdues au lieu de 1 440"
    );
    // 5 détaillées + paliers 8/16/32/64/128 = 10.
    assert_eq!(
        lignes(texte, "hqplayer_poll_failed"),
        ECHECS_SONDAGE_DETAILLES as usize,
        "les {ECHECS_SONDAGE_DETAILLES} premiers échecs sont détaillés, et eux \
         seuls — journal :\n{texte}"
    );
    assert_eq!(
        lignes(texte, "hqplayer_poll_still_failing"),
        5,
        "au-delà, seuls les paliers 8/16/32/64/128 parlent — journal :\n{texte}"
    );
    assert_eq!(
        lignes_d_echec(texte),
        10,
        "une journée d'hôte débranché ne doit plus coûter que 10 lignes \
         (c'était {tours_avant}) — journal :\n{texte}"
    );
    assert_eq!(
        journal.echecs(),
        tours as u32,
        "le compte reste exact même quand le journal se tait"
    );

    // Le total n'est jamais perdu : le dernier récapitulatif le porte.
    let dernier = texte
        .lines()
        .filter(|l| l.contains("hqplayer_poll_still_failing"))
        .next_back()
        .expect("il doit rester au moins un récapitulatif");
    assert!(
        dernier.contains("echecs=128"),
        "le récapitulatif doit porter le TOTAL d'échecs, or :\n{dernier}"
    );
    assert!(
        dernier.contains(ERREUR),
        "et l'erreur, qui est ce qu'on lit :\n{dernier}"
    );

    // ── Le retour : une clôture qui porte le total exact ─────────────────
    let repere = capture.texte().len();
    let mut deja_annonce = true;
    journaliser_succes_hqplayer(&mut journal, HOTE, &mut deja_annonce);
    let texte = capture.texte();
    let texte = &texte[repere..];
    assert_eq!(
        lignes(texte, "hqplayer_poll_registered"),
        1,
        "le retour d'un hôte après une panne durable se dit — journal :\n{texte}"
    );
    assert!(
        texte.contains(&format!("echecs={tours}")),
        "la clôture doit porter le total exact ({tours}), pas le dernier palier \
         (128) — journal :\n{texte}"
    );

    // Et une fois revenu, il se tait de nouveau.
    let repere = capture.texte().len();
    for _ in 0..1_000 {
        journaliser_succes_hqplayer(&mut journal, HOTE, &mut deja_annonce);
    }
    assert!(
        capture.texte()[repere..].is_empty(),
        "un hôte qui répond mille tours de suite n'a plus rien à dire, or :\n{}",
        &capture.texte()[repere..]
    );
}
