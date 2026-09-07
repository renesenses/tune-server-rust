//! #3318 — « coupures et arrêt pendant la lecture » (Yacine, Linux, un seul
//! cœur, DAC USB DENAFRIPS en ALSA).
//!
//! ## Le trou que ce fichier garde
//!
//! Son rapport porte les deux traces que Tune savait déjà écrire :
//!
//! ```text
//! WARN local_audio_slow_read bytes=65536 wait_ms=38594 total_bytes_read=6621166
//! WARN local_audio_slow_read bytes=65536 wait_ms=44853 total_bytes_read=10743808
//! INFO local_output_ended_naturally_advancing zone_id=20 wall_elapsed=290 peak_pos=215525
//! ```
//!
//! Une piste de 210,5 s qui met 290 s à finir, et 83,5 s d'attente de lecture
//! qui en rendent compte. Ces lignes disent que le FIL DE LECTURE a attendu.
//! Elles ne disent pas si l'auditeur a entendu quelque chose : entre ce fil et
//! le DAC il y a un anneau de 2 s (`ring_cap = taux × canaux × 2`,
//! `outputs/local.rs`), et tant qu'il tient, une attente ne s'entend pas.
//!
//! L'instant où il ne tient plus n'était écrit nulle part. `RingStarvation`
//! (`tune-output-api`) compte bien les rappels du pilote servis à court, mais
//! ses compteurs ne se lisent que sur demande, dans le rapport de diagnostic,
//! sans date, et ils repartent de zéro à la piste suivante. C'est exactement ce
//! que montre le sien : `0 événement(s) […] sur 0 servis (0 ms de flux)` — un
//! relevé pris quand plus rien ne jouait.
//!
//! ## Ce qui est tenu ici
//!
//! 1. `SuiviFamine` — la comptabilité pure : deux lignes par incident, quelle
//!    qu'en soit la durée, et un flux neuf ne passe pas pour une famine.
//! 2. Le BRANCHEMENT : un vrai `tick()` de sondeur, sur une vraie zone locale
//!    en lecture, doit faire remonter le silence jusqu'à la métrique de zone
//!    — celle que `/zones/{id}/network-health` et `sync-status` publient.
//!    Sans le relevé pris dans `get_status_with_signal_path_bounded`, ou sans
//!    sa recopie dans `ZonePollerMetrics`, ces tests tombent.
//!
//! Le témoin vert est du même moule : une sortie SANS anneau — tout renderer
//! réseau — ne doit rien faire remonter du tout, et surtout pas un zéro qui se
//! lirait comme « mesuré, et sain ».

use super::decisions::{FamineAnneau, SuiviFamine};
use super::{IdlePollBackoff, OutputStatus, PositionPoller, TransportState, ZonePollState};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::{OutputRingStarvation, OutputTarget};
use crate::playback::{NowPlaying, PlaybackManager};
use crate::poller::PollerMetricsMap;
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

/// La cadence relevée chez Yacine : `served_samples = 33 816 156` pour
/// `stream_ms = 352 251`, soit 96 000 échantillons entrelacés par seconde —
/// 48 kHz en stéréo, ce que son DENAFRIPS recevait.
const CADENCE: u64 = 96_000;

/// Un relevé d'anneau cohérent : `served` échantillons servis à la cadence
/// ci-dessus, dont `manquants` remplacés par des zéros sur `evenements`
/// rappels.
fn releve(served: u64, evenements: u64, manquants: u64) -> OutputRingStarvation {
    OutputRingStarvation {
        events: evenements,
        missing_samples: manquants,
        served_samples: served,
        stream_ms: served * 1000 / CADENCE,
    }
}

// ─────────────────────────── la comptabilité pure ───────────────────────────

/// Le premier relevé ne dit RIEN. Des compteurs déjà hauts au moment où on
/// commence à regarder n'appartiennent pas au tick qui les découvre.
#[test]
fn le_premier_releve_sert_de_repere_et_ne_parle_pas() {
    let mut suivi = SuiviFamine::default();
    assert!(suivi.observer(releve(96_000, 12, 4_800)).is_none());
}

/// Une seconde d'anneau vide : la ligne d'ouverture part, et elle porte le
/// silence de ce tick-là.
#[test]
fn l_anneau_qui_se_vide_ouvre_un_episode() {
    let mut suivi = SuiviFamine::default();
    suivi.observer(releve(96_000, 0, 0));
    let Some(FamineAnneau::Debut(ep)) = suivi.observer(releve(192_000, 40, 19_200)) else {
        panic!("un anneau qui se vide doit ouvrir un épisode");
    };
    assert_eq!(ep.rappels_a_court, 40);
    // 19 200 échantillons à 96 000/s = 200 ms de zéros.
    assert_eq!(ep.silence_ms, 200);
    assert_eq!(ep.flux_ms, 2_000);
}

/// LE POINT DU SUIVI : chez Yacine l'anneau reste vide des dizaines de
/// secondes d'affilée. Au tick du sondeur, cela ferait une ligne par seconde.
/// Une seule doit sortir tant que l'épisode dure — puis une seule à sa
/// fermeture, avec le CUMUL.
#[test]
fn une_famine_qui_dure_n_ecrit_pas_une_ligne_par_seconde() {
    let mut suivi = SuiviFamine::default();
    suivi.observer(releve(96_000, 0, 0));
    assert!(matches!(
        suivi.observer(releve(192_000, 40, 19_200)),
        Some(FamineAnneau::Debut(_))
    ));
    for n in 2..40u64 {
        let r = releve(96_000 * (n + 1), 40 * n, 19_200 * n);
        assert!(
            suivi.observer(r).is_none(),
            "l'épisode dure : le tick {n} ne doit rien écrire de plus"
        );
    }
    // 40 s plus tard le producteur rattrape : les compteurs cessent de bouger.
    let dernier = releve(96_000 * 41, 40 * 39, 19_200 * 39);
    let Some(FamineAnneau::Fin(ep)) = suivi.observer(dernier) else {
        panic!("la réalimentation doit fermer l'épisode");
    };
    assert_eq!(ep.rappels_a_court, 40 * 39);
    assert_eq!(ep.echantillons_manquants, 19_200 * 39);
    // 39 tranches de 200 ms de zéros.
    assert_eq!(ep.silence_ms, 7_800);
    // L'épisode s'étale du relevé qui précède son ouverture au relevé qui le
    // ferme : 40 s d'audio joué à l'horloge du pilote.
    assert_eq!(ep.duree_ms, 40_000);
}

/// TÉMOIN VERT : une lecture saine — le pilote servi en entier à chaque
/// rappel — ne doit jamais rien produire, si longtemps qu'elle dure.
#[test]
fn une_lecture_saine_ne_dit_jamais_rien() {
    let mut suivi = SuiviFamine::default();
    for n in 0..600u64 {
        assert!(
            suivi.observer(releve(96_000 * (n + 1), 0, 0)).is_none(),
            "dix minutes de lecture saine ne doivent pas produire une ligne"
        );
    }
}

/// La piste suivante remet les compteurs à zéro (`begin_stream`). Ce recul
/// n'est pas une famine — et il ne doit pas non plus faire perdre le bilan
/// d'un épisode encore ouvert, sans quoi une piste jouée à court de bout en
/// bout n'aurait jamais le sien.
#[test]
fn un_flux_neuf_se_recale_mais_ferme_l_episode_ouvert() {
    let mut suivi = SuiviFamine::default();
    suivi.observer(releve(96_000, 0, 0));
    suivi.observer(releve(192_000, 40, 19_200));
    // Piste suivante : compteurs neufs, très en dessous des précédents.
    let Some(FamineAnneau::Fin(ep)) = suivi.observer(releve(48_000, 0, 0)) else {
        panic!("l'épisode ouvert doit se fermer sur le dernier relevé de la piste finie");
    };
    assert_eq!(ep.silence_ms, 200);
    // Et le flux neuf, lui, repart d'un repère muet.
    assert!(suivi.observer(releve(144_000, 0, 0)).is_none());
}

/// Un flux qui n'a encore rien servi n'a pas de cadence : on ne peut pas
/// convertir des échantillons en millisecondes, et on ne l'invente pas.
#[test]
fn sans_echantillon_servi_aucune_duree_n_est_inventee() {
    assert_eq!(super::decisions::silence_ms(1_024, releve(0, 3, 1_024)), 0);
}

// ──────────────────────────── le branchement réel ────────────────────────────

const APPAREIL: &str = "local:DENAFRIPS USB Audio V3.14, USB Audio";

/// La sortie locale du banc : elle joue, et elle rend le relevé d'anneau que
/// le test lui a posé — ou rien du tout, comme un renderer réseau.
struct SortieLocale {
    anneau: Arc<std::sync::Mutex<Option<OutputRingStarvation>>>,
}

#[async_trait::async_trait]
impl OutputTarget for SortieLocale {
    fn name(&self) -> &str {
        "DENAFRIPS USB Audio V3.14, USB Audio"
    }
    fn device_id(&self) -> &str {
        APPAREIL
    }
    fn output_type(&self) -> &str {
        "local"
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
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus {
            state: TransportState::Playing,
            // Deux secondes, et plus rien : la position doit rester SOUS
            // l'horloge murale du banc, sinon `decisions::stale_start_position`
            // écarte l'échantillon et le tick sort avant la métrique.
            position_ms: 2_000,
            duration_ms: 210_525,
            ..Default::default()
        })
    }
    fn ring_starvation(&self) -> Option<OutputRingStarvation> {
        *self.anneau.lock().unwrap()
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Banc {
    poller: PositionPoller,
    metriques: PollerMetricsMap,
    anneau: Arc<std::sync::Mutex<Option<OutputRingStarvation>>>,
    zone_id: i64,
    poll_states: HashMap<i64, ZonePollState>,
    idle_backoff: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    async fn monter(anneau_initial: Option<OutputRingStarvation>) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("local"), Some(APPAREIL))
            .unwrap();
        let anneau = Arc::new(std::sync::Mutex::new(anneau_initial));
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(SortieLocale {
            anneau: anneau.clone(),
        }));
        let playback = Arc::new(PlaybackManager::new());
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let metriques: PollerMetricsMap = Arc::new(Mutex::new(HashMap::new()));
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs,
            db.clone(),
            metriques.clone(),
        );
        playback
            .play(
                zone_id,
                NowPlaying {
                    title: "Melody".into(),
                    source: "local".into(),
                    duration_ms: 210_525,
                    ..Default::default()
                },
            )
            .await;
        Self {
            poller,
            metriques,
            anneau,
            zone_id,
            poll_states: HashMap::new(),
            idle_backoff: HashMap::new(),
        }
    }

    /// Poser un relevé d'anneau, puis faire tourner UN tick de sondeur —
    /// l'état de sondage est conservé d'un tick au suivant, comme dans la
    /// boucle réelle.
    async fn tick_avec(&mut self, anneau: Option<OutputRingStarvation>) {
        *self.anneau.lock().unwrap() = anneau;
        self.poller
            .tick(
                &mut self.poll_states,
                &mut self.idle_backoff,
                &Instant::now(),
            )
            .await;
    }

    async fn metrique(&self) -> super::ZonePollerMetrics {
        self.metriques
            .lock()
            .await
            .get(&self.zone_id)
            .cloned()
            .unwrap_or_default()
    }
}

/// LE FAIT DE BASE : le silence envoyé au DAC doit ATTEINDRE la métrique de
/// zone. Sans le relevé pris dans `get_status_with_signal_path_bounded`, ou
/// sans sa recopie dans `ZonePollerMetrics`, ce test tombe — et le testeur
/// reste, comme Yacine, avec un journal muet sur la seule chose qu'il entend.
#[tokio::test]
async fn le_silence_envoye_au_dac_atteint_la_metrique_de_zone() {
    let mut banc = Banc::monter(Some(releve(96_000, 0, 0))).await;
    banc.tick_avec(Some(releve(96_000, 0, 0))).await;
    assert_eq!(
        banc.metrique().await.famine_anneau_silence_ms,
        0,
        "une première seconde saine ne doit rien accuser"
    );

    // Une seconde d'anneau vide : 19 200 échantillons de zéros = 200 ms.
    banc.tick_avec(Some(releve(192_000, 40, 19_200))).await;
    let m = banc.metrique().await;
    assert_eq!(m.famine_anneau_evenements, 40);
    assert_eq!(
        m.famine_anneau_silence_ms, 200,
        "200 ms de zéros sont partis vers le DAC : la métrique doit le dire"
    );

    // Puis la lecture repart : le cumul du flux en cours ne recule pas.
    banc.tick_avec(Some(releve(288_000, 40, 19_200))).await;
    assert_eq!(banc.metrique().await.famine_anneau_silence_ms, 200);
}

/// TÉMOIN VERT n° 1 : la même zone, le même tick, une lecture saine — la
/// métrique reste à zéro. C'est la garde contre un constat qui crierait au
/// loup à chaque piste.
#[tokio::test]
async fn une_lecture_saine_laisse_la_metrique_a_zero() {
    let mut banc = Banc::monter(Some(releve(96_000, 0, 0))).await;
    for n in 1..=5u64 {
        banc.tick_avec(Some(releve(96_000 * n, 0, 0))).await;
    }
    let m = banc.metrique().await;
    assert_eq!(m.famine_anneau_evenements, 0);
    assert_eq!(m.famine_anneau_silence_ms, 0);
}

/// TÉMOIN VERT n° 2 : une sortie qui ne tient pas d'anneau — tout renderer
/// réseau, où c'est l'appareil qui tamponne — ne doit rien faire remonter.
/// Le sondeur doit continuer de tourner exactement comme avant.
#[tokio::test]
async fn une_sortie_sans_anneau_ne_remonte_rien() {
    let mut banc = Banc::monter(None).await;
    banc.tick_avec(None).await;
    banc.tick_avec(None).await;
    let m = banc.metrique().await;
    assert!(
        m.total_polls > 0,
        "le sondeur doit avoir tourné : sans cela le test ne prouve rien"
    );
    assert_eq!(m.famine_anneau_evenements, 0);
    assert_eq!(m.famine_anneau_silence_ms, 0);
}
