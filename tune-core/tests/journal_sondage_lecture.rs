//! Ce qu'une panne de sondage coûte au journal d'une zone **en lecture** (#2566).
//!
//! Frère de `journal_sondage_repos.rs`, qui a borné la branche « repos » en
//! v0.9.129. Le commit qui l'a fait nommait lui-même deux sites laissés de
//! côté ; celui-ci est le premier des deux, et c'est le plus bavard des trois.
//!
//! ## Les trois causes possibles, et laquelle était en jeu
//!
//! `Resource temporarily unavailable (os error 35)` est `EAGAIN`. Sur une
//! socket non bloquante ce n'est pas une erreur, et soixante-dix-neuf de suite
//! dans un journal désignent l'un de trois défauts :
//!
//! 1. **un `EAGAIN` compté comme un échec** — écarté : sur le chemin Cast, cet
//!    `EAGAIN` est produit par le `set_read_timeout` de notre propre
//!    `DeadlineTcpStream`. C'est une expiration de délai VOLONTAIRE, donc un
//!    vrai échec, et la #2690 lui a déjà rendu son nom (« Cast command deadline
//!    elapsed ») ;
//! 2. **une boucle qui tourne à vide** — écartée pour la CADENCE : le recul
//!    exponentiel plafonnait déjà (`skip_ticks=32` au repos, 33 s entre deux
//!    lignes chez Dimitri), et la #2263 a par ailleurs fait retomber au repos
//!    les zones arrêtées ou en pause. Le nombre de TOURS est correct ;
//! 3. **un vrai défaut de connexion noyé** — c'est bien lui, et le bruit qui le
//!    noyait est le sujet de ce fichier : le journal, lui, n'avait aucun
//!    plafond. Une ligne par tour, indéfiniment.
//!
//! ## Le chiffre de la branche « lecture »
//!
//! `poll_failed_backing_off` (`tune-core/src/poller.rs`) recule de
//! `1 << min(n, 4)` ticks, soit **16 ticks au plafond**. Un tour coûte donc
//! 1 tick de tentative + 16 sautés = **17 s** à `POLL_INTERVAL_MS` = 1000 ms.
//!
//! | fenêtre | tours de boucle | lignes AVANT | lignes APRÈS |
//! |---|---|---|---|
//! | les 79 échecs de Dimitri | 79 | 79 | **9** |
//! | 8 h (un appareil laissé muet une nuit) | 1 694 | 1 694 | **13** |
//!
//! Le nombre de tours est le **même** des deux côtés : ce passage ne touche ni
//! la cadence, ni le recul, ni les compteurs qui décident. Il ne touche que le
//! volume du journal.
//!
//! ## Pourquoi un binaire de test à lui seul
//!
//! Leçon déjà payée en #2665, #2890 puis #2566 : `tracing` met en cache **pour
//! tout le processus** la décision « ce point d'appel intéresse-t-il quelqu'un
//! ? ». Un abonné posé au milieu d'une suite qui tourne en parallèle se voit
//! priver d'évènements de façon imprévisible. Ici l'abonné est **global**, ce
//! fichier ne contient **qu'un test**, et il est installé avant tout le reste.
//!
//! `autotests = false` dans `tune-core/Cargo.toml` : la cible est déclarée
//! là-bas, sans quoi ce fichier ne serait jamais compilé.
//!
//! ## Aucun réseau, aucune attente
//!
//! Le Chromecast est **factice** : il n'ouvre aucune socket, il compte ses
//! tours et rend l'erreur telle qu'elle figure dans le journal de Dimitri.
//! Aucun `sleep` non plus — une cadence éprouvée par de vraies attentes est un
//! test qui dure une heure et qui clignote. Les fenêtres sont calculées, les
//! tours sont comptés.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tune_core::poller::{ECHECS_SONDAGE_DETAILLES, JournalSondage};
use tune_output_api::{OutputStatus, OutputTarget, TransportState};

/// Le nombre d'échecs consécutifs relevé chez Dimitri (fil forum 1577).
const ECHECS_DIMITRI: u32 = 79;

/// L'erreur, telle qu'elle apparaît dans son journal.
///
/// Recopiée en clair, et **pas** fabriquée par `io::Error::from_raw_os_error`,
/// pour une raison de fond : `EAGAIN` vaut 35 sur macOS et 11 sur Linux, et le
/// texte système diffère. Fabriquer l'errno rendrait le test dépendant de la
/// machine qui le joue ; c'est ce texte-ci qu'on cherchera dans un journal.
const ERREUR_EAGAIN: &str = "media status: Resource temporarily unavailable (os error 35)";

/// Une vraie panne de connexion — l'appareil est éteint, la socket est refusée.
/// Elle doit être dite, toujours, et elle est le témoin de non-régression.
const ERREUR_CONNEXION: &str = "chromecast connect: Connection refused";

const ZONE: i64 = 4;
const APPAREIL: &str = "chromecast-11373bd94d730fd5182781bbc87a8973";

/// Un tour de la branche « lecture » coûte 1 tick de tentative + le recul
/// saturé (`1 << min(n, 4)` = 16 ticks), à `POLL_INTERVAL_MS` = 1000 ms.
const SECONDES_PAR_TOUR: u64 = 17;

// ─────────────────────────── le Chromecast factice ───────────────────────────

/// Ce que l'appareil répond, tour après tour.
enum Reponse {
    /// Il lit, et il le dit correctement.
    Lecture,
    /// Il échoue, toujours de la même façon — c'est la définition d'une panne
    /// durable.
    Echec(&'static str),
}

/// Un Chromecast qui ne parle à personne, compte ses tours, et rend ce qu'on
/// lui a demandé de rendre.
struct ChromecastFactice {
    reponse: Mutex<Reponse>,
    tours: AtomicUsize,
}

impl ChromecastFactice {
    fn en_echec(erreur: &'static str) -> Self {
        Self {
            reponse: Mutex::new(Reponse::Echec(erreur)),
            tours: AtomicUsize::new(0),
        }
    }

    fn qui_lit() -> Self {
        Self {
            reponse: Mutex::new(Reponse::Lecture),
            tours: AtomicUsize::new(0),
        }
    }

    /// Combien de fois le sondeur l'a réellement interrogé.
    fn tours(&self) -> usize {
        self.tours.load(Ordering::SeqCst)
    }

    fn repond_desormais(&self) {
        *self.reponse.lock().unwrap() = Reponse::Lecture;
    }
}

#[async_trait::async_trait]
impl OutputTarget for ChromecastFactice {
    fn name(&self) -> &str {
        "Chromecast de Dimitri"
    }
    fn device_id(&self) -> &str {
        APPAREIL
    }
    fn output_type(&self) -> &str {
        "chromecast"
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn is_available(&self) -> bool {
        true
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        self.tours.fetch_add(1, Ordering::SeqCst);
        match *self.reponse.lock().unwrap() {
            Reponse::Echec(e) => Err(e.to_string()),
            Reponse::Lecture => Ok(OutputStatus {
                state: TransportState::Playing,
                position_ms: 42_000,
                duration_ms: 300_000,
                ..Default::default()
            }),
        }
    }
}

// ───────────────────────────── capture du journal ─────────────────────────────

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

/// Toutes les lignes que la branche « lecture » sait produire pour un échec.
fn lignes_d_echec(texte: &str) -> usize {
    lignes(texte, "poll_failed_backing_off") + lignes(texte, "poll_still_failing")
}

/// Un tour de la branche « lecture », réduit à ce que ce fichier éprouve :
/// interroger l'appareil, tenir les compteurs qui décident, et journaliser.
///
/// Les compteurs sont ceux du site d'appel réel — `consecutive_errors` en `u8`
/// saturant, `backoff_remaining = 1 << min(n, 4)` — et ils sont tenus AVANT le
/// journal, exactement comme dans `poller.rs`.
#[derive(Default)]
struct Compteurs {
    consecutive_errors: u8,
    total_errors: u64,
    backoff_remaining: u8,
}

async fn un_tour(
    appareil: &ChromecastFactice,
    journal: &mut JournalSondage,
    compteurs: &mut Compteurs,
) -> Option<OutputStatus> {
    match appareil.get_status().await {
        Ok(s) => {
            compteurs.consecutive_errors = 0;
            journal.succes_lecture(ZONE, appareil.device_id());
            Some(s)
        }
        Err(e) => {
            compteurs.consecutive_errors = compteurs.consecutive_errors.saturating_add(1);
            compteurs.total_errors += 1;
            compteurs.backoff_remaining = 1u8 << compteurs.consecutive_errors.min(4);
            let backoff = compteurs.backoff_remaining;
            journal.echec_lecture(ZONE, appareil.device_id(), &e, backoff);
            None
        }
    }
}

#[test]
fn une_panne_de_sondage_en_lecture_se_dit_quelques_fois_puis_se_recapitule() {
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

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("aucune E/S, aucun temps : un ordonnanceur nu suffit");

    // ── Témoin 1 : une zone qui lit normalement n'écrit RIEN ─────────────
    //
    // Il passe EN PREMIER et il est vert des deux côtés de la contre-épreuve.
    // C'est le cas de l'écrasante majorité des zones : les rendre bavardes
    // serait une régression bien pire que le bruit qu'on retire.
    //
    // Et l'état de lecture doit rester correctement rapporté — c'est la
    // seconde moitié du témoin : plafonner le journal ne doit pas troquer du
    // bruit contre un statut perdu.
    let sain = ChromecastFactice::qui_lit();
    let mut journal = JournalSondage::default();
    let mut compteurs = Compteurs::default();
    rt.block_on(async {
        for _ in 0..1_000 {
            let s = un_tour(&sain, &mut journal, &mut compteurs)
                .await
                .expect("un appareil qui répond doit rendre son statut");
            assert_eq!(s.state, TransportState::Playing, "l'état doit remonter");
            assert_eq!(s.position_ms, 42_000, "la position doit remonter");
            assert_eq!(s.duration_ms, 300_000, "la durée doit remonter");
        }
    });
    assert_eq!(
        sain.tours(),
        1_000,
        "le journal ne décide d'aucun sondage : les mille tours doivent avoir eu lieu"
    );
    assert!(
        capture.texte().is_empty(),
        "mille sondages réussis doivent laisser le journal vierge, or il porte :\n{}",
        capture.texte()
    );

    // ── Témoin 2 : une VRAIE erreur de connexion est toujours dite ───────
    //
    // Un appareil éteint doit produire sa ligne complète, avec l'erreur, la
    // zone, l'appareil et le recul. Vert des deux côtés lui aussi : c'est
    // exactement ce qu'on ne doit pas perdre en bornant le volume.
    let eteint = ChromecastFactice::en_echec(ERREUR_CONNEXION);
    let mut journal = JournalSondage::default();
    let mut compteurs = Compteurs::default();
    rt.block_on(async {
        assert!(
            un_tour(&eteint, &mut journal, &mut compteurs)
                .await
                .is_none(),
            "un appareil injoignable ne rend aucun statut"
        );
    });
    let texte = capture.texte();
    assert_eq!(
        lignes(&texte, "poll_failed_backing_off"),
        1,
        "une erreur de connexion isolée doit produire une ligne détaillée et une seule"
    );
    assert!(
        texte.contains(ERREUR_CONNEXION),
        "la ligne doit porter l'erreur telle quelle — c'est elle qu'on lit :\n{texte}"
    );
    assert!(
        texte.contains(APPAREIL),
        "la ligne doit nommer l'appareil :\n{texte}"
    );
    assert!(
        texte.contains("backoff=2"),
        "la ligne doit porter le recul, inchangé :\n{texte}"
    );
    assert_eq!(
        compteurs.consecutive_errors, 1,
        "l'échec doit être compté : c'est ce compteur qui remonte et qui décide"
    );

    // ── La mesure : les 79 échecs de Dimitri, sur la branche « lecture » ──
    let repere = capture.texte().len();
    let muet = ChromecastFactice::en_echec(ERREUR_EAGAIN);
    let mut journal = JournalSondage::default();
    let mut compteurs = Compteurs::default();
    rt.block_on(async {
        for _ in 0..ECHECS_DIMITRI {
            assert!(un_tour(&muet, &mut journal, &mut compteurs).await.is_none());
        }
    });
    let texte = capture.texte();
    let texte = &texte[repere..];

    // Le nombre de TOURS ne change pas : ce passage ne touche pas la cadence.
    assert_eq!(
        muet.tours(),
        ECHECS_DIMITRI as usize,
        "les {ECHECS_DIMITRI} tours doivent tous avoir eu lieu — c'est le journal \
         qu'on borne, pas le sondage"
    );
    // AVANT : 79 lignes, une par tour.
    // APRÈS : 5 détaillées (échecs 1 à 5) + 4 récapitulatifs aux paliers 8, 16,
    // 32 et 64 = 9.
    assert_eq!(
        lignes(texte, "poll_failed_backing_off"),
        ECHECS_SONDAGE_DETAILLES as usize,
        "les {ECHECS_SONDAGE_DETAILLES} premiers échecs doivent être détaillés, \
         et eux seuls — journal :\n{texte}"
    );
    assert_eq!(
        lignes(texte, "poll_still_failing"),
        4,
        "au-delà du plafond, seuls les paliers 8/16/32/64 parlent — journal :\n{texte}"
    );
    assert_eq!(
        lignes_d_echec(texte),
        9,
        "{ECHECS_DIMITRI} échecs consécutifs en lecture ne doivent plus coûter que \
         9 lignes (c'était {ECHECS_DIMITRI}) — journal :\n{texte}"
    );

    // ── Les compteurs qui DÉCIDENT n'ont pas bougé ───────────────────────
    //
    // Le repli de fin de piste (`poll_failed_past_end`) et l'arrêt de zone
    // lisent `consecutive_errors` et `backoff_remaining`, pas le journal. Se
    // taire ne doit pas se traduire par « ne plus compter » — ce serait
    // troquer du bruit contre une panne invisible.
    assert_eq!(
        compteurs.total_errors, ECHECS_DIMITRI as u64,
        "chaque échec doit être compté, y compris ceux que le journal a tus"
    );
    assert_eq!(
        compteurs.backoff_remaining, 16,
        "le recul de la branche « lecture » plafonne à 1 << 4 = 16 ticks, inchangé"
    );
    assert_eq!(
        journal.echecs(),
        ECHECS_DIMITRI,
        "le compte du journal reste exact même quand il se tait : il est en u32, \
         là où `consecutive_errors` sature à 255 en u8"
    );

    // ── Le total n'est jamais perdu ──────────────────────────────────────
    let dernier = texte
        .lines()
        .filter(|l| l.contains("poll_still_failing"))
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
        dernier.contains(ERREUR_EAGAIN),
        "le récapitulatif doit porter l'erreur : c'est elle qu'on lit, or :\n{dernier}"
    );

    // ── Le retour : une clôture, et l'état de lecture retrouvé ───────────
    let repere = capture.texte().len();
    muet.repond_desormais();
    let statut = rt
        .block_on(un_tour(&muet, &mut journal, &mut compteurs))
        .expect("l'appareil répond de nouveau : le statut doit remonter");
    assert_eq!(
        statut.state,
        TransportState::Playing,
        "après une panne, l'état de lecture doit être rapporté correctement"
    );
    assert_eq!(statut.position_ms, 42_000, "et la position avec lui");
    assert_eq!(
        compteurs.consecutive_errors, 0,
        "un succès remet le compteur d'échecs à zéro, comme avant"
    );
    let texte = capture.texte();
    let texte = &texte[repere..];
    assert_eq!(
        lignes(texte, "poll_recovered"),
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
    // 8 h à un tour toutes les 17 s font 1 694 tours. Sans plafond, c'étaient
    // 1 694 lignes pour une SEULE zone — plus du double de ce que la branche
    // « repos » produisait dans la même fenêtre, et à comparer au quart de
    // fenêtre que l'export de diagnostic accorde à tout le module (#1974).
    let repere = capture.texte().len();
    let nuit = ChromecastFactice::en_echec(ERREUR_EAGAIN);
    let mut journal = JournalSondage::default();
    let mut compteurs = Compteurs::default();
    let tours_nuit = (8 * 3600) / SECONDES_PAR_TOUR;
    assert_eq!(tours_nuit, 1_694, "8 h à un tour toutes les 17 s");
    rt.block_on(async {
        for _ in 0..tours_nuit {
            un_tour(&nuit, &mut journal, &mut compteurs).await;
        }
    });
    let texte = capture.texte();
    let texte = &texte[repere..];
    assert_eq!(
        nuit.tours(),
        tours_nuit as usize,
        "les tours de boucle sont les mêmes qu'avant : seul le journal change"
    );
    // 5 détaillées + paliers 8/16/32/64/128/256/512/1024 = 13.
    assert_eq!(
        lignes_d_echec(texte),
        13,
        "{tours_nuit} échecs (un appareil muet 8 h) ne doivent coûter que 13 lignes"
    );
    assert!(
        (lignes_d_echec(texte) as u64) < tours_nuit / 100,
        "le journal doit être logarithmique en la durée de la panne, pas proportionnel"
    );
    assert_eq!(
        journal.echecs(),
        tours_nuit as u32,
        "et le compte réel reste exact sur toute la nuit"
    );
}
