//! Combien de lignes une panne de sondage coûte au journal (#2566).
//!
//! Frère de `journal_insertion_par_lot.rs` (#2890) et de
//! `journal_descriptif_illisible.rs` (#2665) : même famille, même raison de
//! fond — **la seule chose qu'on aura entre les mains la prochaine fois, c'est
//! le journal**, et un journal noyé ne vaut pas mieux qu'un journal muet.
//!
//! ## Ce qui a été mesuré sur le terrain
//!
//! Dimitri, macOS, v0.9.115, fil forum 1577, message du 27/08/2026 à 14 h 40.
//! Une zone Chromecast au repos a produit :
//!
//! ```text
//! DEBUG tune_core::poller: idle_poll_failed_backing_off
//!     zone_id=4 device=chromecast-11373bd94d730fd5182781bbc87a8973
//!     error=media status: Resource temporarily unavailable (os error 35)
//!     consecutive_errors=78 skip_ticks=32
//! ```
//!
//! …puis la même ligne à `consecutive_errors=79`, 33 secondes plus tard.
//!
//! **Le recul exponentiel n'est pas en cause.** `skip_ticks=32` est son
//! plafond (`2^IDLE_BACKOFF_MAX_SHIFT`), et les 33 s entre deux lignes le
//! confirment : 32 ticks sautés + 1 tick de tentative, à `POLL_INTERVAL_MS`
//! = 1000 ms. Il fait exactement son travail. Ce fichier ne le touche pas et
//! ne le teste pas — `idle_backoff_grows_and_is_capped` s'en charge déjà dans
//! `poller.rs`.
//!
//! Ce qui n'avait **aucun** plafond, c'est le JOURNAL : une ligne par
//! tentative, indéfiniment. Les chiffres qui en découlent :
//!
//! | grandeur | valeur |
//! |---|---|
//! | intervalle entre deux échecs, recul saturé | 33 s |
//! | durée des 79 échecs de Dimitri | 41 min 16 s |
//! | débit de lignes, par zone en panne | ~109 / h |
//! | appareil éteint une nuit (8 h) | ~870 lignes |
//!
//! Et rien ne s'arrête jamais : `consecutive_errors` est un `u8` qui sature à
//! 255 et continue de journaliser. L'export de diagnostic borne pourtant
//! chaque module à un quart de la fenêtre (`QUOTA_PAR_MODULE`, #1974) : 79
//! lignes prennent déjà un tiers du quota de `tune_core::poller`, le module
//! qu'on lit précisément quand une lecture ne démarre pas.
//!
//! ## Pourquoi un binaire de test à lui seul
//!
//! Leçon déjà payée en #2665 puis #2890 : `tracing` met en cache **pour tout
//! le processus** la décision « ce point d'appel intéresse-t-il quelqu'un ? »
//! ainsi que le niveau maximal utile. Un abonné posé au milieu d'une suite qui
//! tourne en parallèle se voit priver d'évènements de façon imprévisible. Ici
//! l'abonné est **global**, ce fichier ne contient **qu'un test**, et il est
//! installé avant tout le reste : le résultat ne dépend d'aucun ordonnancement.
//!
//! `autotests = false` dans `tune-core/Cargo.toml` — la cible est déclarée
//! là-bas, sans quoi ce fichier ne serait jamais compilé.
//!
//! ## Aucun réseau
//!
//! Le test n'ouvre aucune socket et ne parle à aucun appareil. Il appelle
//! `JournalSondageRepos`, qui **est** le point d'émission du sondeur — pas une
//! copie —, et compte les lignes que `tracing` reçoit vraiment.

use std::sync::{Arc, Mutex};

use tune_core::poller::{ECHECS_SONDAGE_DETAILLES, JournalSondageRepos};

/// Le nombre d'échecs consécutifs relevé chez Dimitri.
const ECHECS_DIMITRI: u32 = 79;

/// L'erreur, telle qu'elle apparaissait dans son journal. Reprise à
/// l'identique : c'est le texte que le lecteur cherchera.
const ERREUR: &str = "media status: Resource temporarily unavailable (os error 35)";

const ZONE: i64 = 4;
const APPAREIL: &str = "chromecast-11373bd94d730fd5182781bbc87a8973";

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

/// Compte les lignes du journal capturé qui portent `marqueur`.
fn lignes(texte: &str, marqueur: &str) -> usize {
    texte.lines().filter(|l| l.contains(marqueur)).count()
}

#[test]
fn une_panne_durable_se_dit_quelques_fois_puis_se_recapitule() {
    let capture = JournalCapture::default();
    // DEBUG : c'est le niveau du sondeur. Un abonné plus haut ne verrait rien
    // et rendrait le test vert pour la mauvaise raison.
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    // ── Témoin 1 : un sondage qui réussit n'écrit RIEN ───────────────────
    //
    // Il passe EN PREMIER, et il est vert des deux côtés de la contre-épreuve.
    // C'est le cas de l'écrasante majorité des zones : les rendre bavardes
    // serait une régression bien pire que le bruit qu'on retire.
    let mut nominal = JournalSondageRepos::default();
    for _ in 0..1_000 {
        nominal.succes(ZONE, APPAREIL);
    }
    assert!(
        capture.texte().is_empty(),
        "mille sondages réussis doivent laisser le journal vierge, or il porte :\n{}",
        capture.texte()
    );

    // ── Témoin 2 : un échec ISOLÉ reste dit, en entier ───────────────────
    //
    // Une zone dont l'appareil vient d'être éteint doit encore produire sa
    // ligne complète, avec l'erreur. Vert des deux côtés lui aussi.
    let mut isole = JournalSondageRepos::default();
    isole.echec(ZONE, APPAREIL, &ERREUR, 2);
    let texte = capture.texte();
    assert_eq!(
        lignes(&texte, "idle_poll_failed_backing_off"),
        1,
        "un échec isolé doit produire une ligne détaillée et une seule"
    );
    assert!(
        texte.contains(ERREUR),
        "la ligne détaillée doit porter l'erreur telle quelle, or :\n{texte}"
    );
    assert!(
        texte.contains(APPAREIL),
        "la ligne détaillée doit porter l'appareil, or :\n{texte}"
    );
    // Un échec isolé n'est pas une panne : rien à récapituler, rien à clore.
    isole.succes(ZONE, APPAREIL);
    assert_eq!(
        lignes(&capture.texte(), "idle_poll_recovered"),
        0,
        "un échec isolé qui se rétablit n'a pas de panne à clore"
    );

    // ── La mesure : les 79 échecs de Dimitri ─────────────────────────────
    let repere = capture.texte().len();
    let mut panne = JournalSondageRepos::default();
    for n in 1..=ECHECS_DIMITRI {
        // `skip_ticks` reproduit le recul réel : 2, 4, 8, 16, puis 32 saturé.
        let skip_ticks = 1u8 << n.min(5);
        panne.echec(ZONE, APPAREIL, &ERREUR, skip_ticks);
    }
    let texte = capture.texte();
    let texte = &texte[repere..];

    let detaillees = lignes(texte, "idle_poll_failed_backing_off");
    let recaps = lignes(texte, "idle_poll_still_failing");
    let total = detaillees + recaps;

    // AVANT correctif : 79 lignes, une par tentative.
    // APRÈS : 5 détaillées (échecs 1 à 5) + 4 récapitulatifs aux paliers de
    // doublement (échecs 8, 16, 32, 64) = 9.
    assert_eq!(
        detaillees, ECHECS_SONDAGE_DETAILLES as usize,
        "les {ECHECS_SONDAGE_DETAILLES} premiers échecs doivent être détaillés, \
         et eux seuls — journal :\n{texte}"
    );
    assert_eq!(
        recaps, 4,
        "au-delà du plafond, seuls les paliers 8/16/32/64 parlent — journal :\n{texte}"
    );
    assert_eq!(
        total, 9,
        "{ECHECS_DIMITRI} échecs consécutifs ne doivent plus coûter que 9 lignes \
         (c'était {ECHECS_DIMITRI} avant #2566) — journal :\n{texte}"
    );
    assert!(
        total < ECHECS_DIMITRI as usize,
        "le journal doit être borné, pas proportionnel au nombre de tentatives"
    );

    // Le total n'est jamais perdu : le dernier récapitulatif le porte, et il
    // dit aussi combien de lignes ont été détaillées. Sans cela, plafonner
    // masquerait l'ampleur de la panne — c'est tout ce qui distingue un
    // plafond d'une censure.
    let dernier = texte
        .lines()
        .filter(|l| l.contains("idle_poll_still_failing"))
        .next_back()
        .expect("il doit rester au moins un récapitulatif");
    assert!(
        dernier.contains("echecs=64"),
        "le récapitulatif doit porter le TOTAL d'échecs, or :\n{dernier}"
    );
    assert!(
        dernier.contains(&format!("detaillees={ECHECS_SONDAGE_DETAILLES}")),
        "le récapitulatif doit dire combien de lignes ont été détaillées, or :\n{dernier}"
    );
    assert!(
        dernier.contains(ERREUR),
        "le récapitulatif doit porter l'erreur : c'est elle qu'on lit, or :\n{dernier}"
    );

    // ── La clôture porte le total exact, pas le dernier palier ───────────
    let repere = capture.texte().len();
    panne.succes(ZONE, APPAREIL);
    let texte = capture.texte();
    let texte = &texte[repere..];
    assert_eq!(
        lignes(texte, "idle_poll_recovered"),
        1,
        "la fin d'une panne durable se dit une fois — journal :\n{texte}"
    );
    assert!(
        texte.contains(&format!("echecs={ECHECS_DIMITRI}")),
        "la clôture doit porter le total exact ({ECHECS_DIMITRI}), pas le dernier \
         palier (64) — journal :\n{texte}"
    );

    // ── Une nuit entière : le coût reste logarithmique ───────────────────
    //
    // 8 h à une tentative toutes les 33 s font ~870 échecs. Sans plafond,
    // c'étaient ~870 lignes pour une SEULE zone, à comparer au quart de
    // fenêtre que l'export accorde à tout le module (#1974).
    let repere = capture.texte().len();
    let mut nuit = JournalSondageRepos::default();
    let echecs_nuit = (8 * 3600) / 33; // 872
    for _ in 0..echecs_nuit {
        nuit.echec(ZONE, APPAREIL, &ERREUR, 32);
    }
    let texte = capture.texte();
    let texte = &texte[repere..];
    let total_nuit =
        lignes(texte, "idle_poll_failed_backing_off") + lignes(texte, "idle_poll_still_failing");
    // 5 détaillées + paliers 8/16/32/64/128/256/512 = 12.
    assert_eq!(
        total_nuit, 12,
        "{echecs_nuit} échecs (un appareil éteint 8 h) ne doivent coûter que 12 lignes"
    );
    assert_eq!(
        nuit.echecs(),
        echecs_nuit,
        "le compte réel doit rester exact même quand la trace se tait : \
         `consecutive_errors` sature à 255 en `u8`, pas ce compteur-ci"
    );
}
