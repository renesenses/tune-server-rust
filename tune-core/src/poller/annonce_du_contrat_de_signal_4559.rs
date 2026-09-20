//! #4559 — le contrat de signal publié par la sortie doit être ANNONCÉ.
//!
//! Jean Valjean, v0.9.158, Windows 11, sortie locale `local:Haut-parleurs`,
//! fil forum 1857 : « Je suis obligé de baisser le volume puis de le remettre
//! à 100 pour avoir signal sans perte et Bit-Perfect en vert. »
//!
//! # Le fait mesuré
//!
//! Son journal montre le bras exclusif ouvert et le contrat publié —
//! `windows_exclusive_signal_contract backend="WASAPI" bit_perfect=true
//! dop=false volume_units=1000 reasons=[]` — pendant que son panneau affiche
//! « WASAPI (shared — Windows mixer) » et deux étapes orange. Les deux ne se
//! contredisent pas : le libellé « shared » ne décrit pas le mode d'ouverture,
//! il décrit l'ABSENCE de contrat dans l'état de zone
//! (`routes/zones/signal_path.rs`, `exclusif_observe = runtime_signal_path
//! .is_some()`). Le panneau qu'il regarde a donc été bâti AVANT la publication.
//!
//! La chronologie, instruction par instruction :
//!
//! 1. `PlaybackManager::play` efface `output_signal_path` et
//!    `transformations_reelles` de l'état de zone, et émet `playback.started` ;
//! 2. le client relit la zone dans la foulée (`syncZoneState` sur
//!    `playback.started`, `App.svelte`) — donc SANS contrat : « shared » ;
//! 3. `bras_wasapi::jouer_via_wasapi` ouvre le périphérique et publie le
//!    contrat `output_ms` plus tard : **236 ms et 2 219 ms** sur les deux
//!    enchaînements de son export (`playback_timing`) ;
//! 4. … et personne ne le dit. Les deux seules charges utiles qui portent
//!    `signal_path` sont des RÉPONSES (`GET /zones`, réponse du `play`) plus
//!    l'instantané WebSocket de la CONNEXION. Aucun évènement poussé ne le
//!    porte.
//!
//! Le panneau reste donc faux jusqu'au prochain geste qui fait relire la zone.
//! Un aller-retour de volume en est un ; une bascule PURE aussi — et c'est
//! exactement le symptôme jumeau de #4347, sur la même machine, chez le même
//! testeur : là-bas, `orchestrator/dsp.rs` émet bien un `zone.updated`, ce qui
//! explique que PURE « actualise » un panneau que rien d'autre n'actualise.
//!
//! # Ce que ces témoins tiennent
//!
//! Le vrai `tick`, une zone locale, une sortie factice qui publie son contrat
//! avec un tour de retard — comme le DAC qui s'ouvre. L'invariant : *quand la
//! sortie change d'avis, le client l'apprend, une fois et une seule*.
//!
//! # Ce qu'ils ne voient pas
//!
//! Ni le rendu de l'écran, ni le délai réel d'ouverture d'un périphérique
//! WASAPI, ni le cas d'un client qui aurait perdu son WebSocket. Ils ne
//! disent rien non plus du libellé lui-même (#4172), qui reste juste : c'est
//! sa condition de déclenchement qui arrivait trop tôt.

use super::*;

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::outputs::traits::{
    OutputDspState, OutputSampleTransport, OutputSignalPathStatus, OutputVolumeState,
};
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

const APPAREIL: &str = "local:temoin-4559";

/// Le contrat que le bras WASAPI exclusif de Jean Valjean publie :
/// `bit_perfect=true`, `volume_units=1000` (donc volume à l'unité),
/// `reasons=[]`.
fn contrat_bit_perfect() -> OutputSignalPathStatus {
    OutputSignalPathStatus {
        bit_perfect: true,
        sample_transport: OutputSampleTransport::NativeInteger,
        dsp: OutputDspState::Inactive,
        volume: OutputVolumeState::Unity,
        reasons: Vec::new(),
    }
}

/// Le même flux, volume logiciel appliqué : ce que voit l'auditeur qui BAISSE
/// le volume. Sert à prouver qu'un second changement s'annonce lui aussi.
fn contrat_volume_applique() -> OutputSignalPathStatus {
    OutputSignalPathStatus {
        bit_perfect: false,
        sample_transport: OutputSampleTransport::NativeInteger,
        dsp: OutputDspState::Inactive,
        volume: OutputVolumeState::Applied,
        reasons: vec![crate::outputs::traits::OutputSignalReason::SoftwareVolume],
    }
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    zone_id: i64,
    recu: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Haut-parleurs", Some("local"), Some(APPAREIL))
            .unwrap();
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Haut-parleurs").with_type("local"),
        ));
        let playback = Arc::new(PlaybackManager::new());
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let bus = Arc::new(EventBus::new());
        let recu = bus.subscribe();
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(bus.clone());
        Self {
            poller,
            playback,
            outputs,
            zone_id,
            recu,
            poll_states: HashMap::new(),
            idle: HashMap::new(),
        }
    }

    async fn jouer(&mut self, track_id: i64, titre: &str) {
        self.playback
            .play(
                self.zone_id,
                NowPlaying {
                    track_id: Some(track_id),
                    title: titre.into(),
                    source: "local".into(),
                    duration_ms: 300_000,
                    ..Default::default()
                },
            )
            .await;
        let generation = self.playback.get_state(self.zone_id).await.track_generation;
        self.poll_states
            .entry(self.zone_id)
            .or_insert_with(|| ZonePollState::new(generation));
    }

    /// Ce que la SORTIE rend au sondeur ce tour-ci.
    async fn sortie(&self, position_ms: u64, contrat: Option<OutputSignalPathStatus>) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(TransportState::Playing).await;
        mock.set_duration(300_000);
        mock.set_position(position_ms);
        mock.set_signal_path_status(contrat);
    }

    fn dater_le_debut(&mut self, il_y_a: Duration) {
        let ps = self.poll_states.get_mut(&self.zone_id).unwrap();
        ps.track_started_at = Some(Instant::now() - il_y_a);
    }

    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    /// Combien de `zone.updated` pour CETTE zone depuis le dernier comptage.
    fn annonces(&mut self) -> usize {
        let mut n = 0;
        while let Ok(ev) = self.recu.try_recv() {
            if ev.event_type == "zone.updated"
                && ev.data.get("zone_id").and_then(|v| v.as_i64()) == Some(self.zone_id)
            {
                n += 1;
            }
        }
        n
    }

    /// Ce que `GET /zones` servirait : le contrat que le panneau lit.
    async fn contrat_servi(&self) -> Option<OutputSignalPathStatus> {
        self.playback
            .get_state(self.zone_id)
            .await
            .output_signal_path
    }
}

/// Le témoin du ticket : le contrat arrive APRÈS que le client a relu la zone.
///
/// Rouge avant correctif : l'état de zone se corrige (le sondeur le recopie
/// déjà), mais AUCUN `zone.updated` ne part — le client garde son panneau
/// « WASAPI (shared — Windows mixer) » jusqu'à ce que l'auditeur touche au
/// volume.
#[tokio::test]
async fn le_contrat_publie_a_l_ouverture_du_peripherique_est_annonce() {
    let mut banc = Banc::monter().await;
    banc.jouer(1, "L'EAU QUI DORT").await;
    banc.dater_le_debut(Duration::from_secs(20));

    // Tour 1 — le périphérique n'est pas encore ouvert : rien à publier. C'est
    // l'instant où le client a relu la zone, et il n'a rien à réapprendre.
    banc.sortie(1_000, None).await;
    banc.tic().await;
    assert_eq!(
        banc.annonces(),
        0,
        "rien n'a changé depuis ce que le client tient : ce tour doit être muet"
    );

    // Tour 2 — le bras exclusif a ouvert le DAC et publié son contrat.
    banc.sortie(2_000, Some(contrat_bit_perfect())).await;
    banc.tic().await;
    assert_eq!(
        banc.contrat_servi().await,
        Some(contrat_bit_perfect()),
        "le banc lui-même est faux : le sondeur doit reporter le contrat"
    );
    assert_eq!(
        banc.annonces(),
        1,
        "le contrat est arrivé et personne ne l'a dit : le panneau de Jean \
         Valjean reste sur « WASAPI (shared — Windows mixer) » jusqu'à ce \
         qu'un aller-retour du volume fasse relire la zone (#4559)."
    );

    // Tour 3 — même contrat republié à chaque tampon : le client n'a rien à
    // relire, et surtout pas toutes les secondes.
    banc.sortie(3_000, Some(contrat_bit_perfect())).await;
    banc.tic().await;
    assert_eq!(
        banc.annonces(),
        0,
        "un contrat inchangé ne doit pas faire refetch le client à chaque tick"
    );
}

/// Le versant symétrique : quand le contrat CHANGE en cours de piste — c'est
/// ce que produit un coup de volume —, l'annonce repart.
#[tokio::test]
async fn un_changement_de_contrat_en_cours_de_piste_est_annonce_aussi() {
    let mut banc = Banc::monter().await;
    banc.jouer(1, "L'EAU QUI DORT").await;
    banc.dater_le_debut(Duration::from_secs(20));

    banc.sortie(1_000, Some(contrat_bit_perfect())).await;
    banc.tic().await;
    assert_eq!(banc.annonces(), 1, "première publication");

    banc.sortie(2_000, Some(contrat_volume_applique())).await;
    banc.tic().await;
    assert_eq!(
        banc.annonces(),
        1,
        "le volume logiciel entre dans le chemin : l'étape « Volume logiciel » \
         et le verdict changent, le client doit relire (#4559)."
    );
}

/// La piste SUIVANTE repart de zéro : `play` efface le contrat côté zone, le
/// client relit une zone sans contrat, et la republication doit s'annoncer de
/// nouveau — même si c'est le MÊME contrat qu'à la piste précédente.
///
/// C'est le cas de son enchaînement automatique : `auto_next` →
/// `wasapi_exclusive_initialized` → `windows_exclusive_signal_contract`.
#[tokio::test]
async fn chaque_nouvelle_piste_reannonce_son_contrat() {
    let mut banc = Banc::monter().await;
    banc.jouer(1, "Piste A").await;
    banc.dater_le_debut(Duration::from_secs(20));
    banc.sortie(1_000, Some(contrat_bit_perfect())).await;
    banc.tic().await;
    assert_eq!(banc.annonces(), 1, "piste A");

    // Enchaînement : `play` remet l'état de zone à nu, la sortie referme puis
    // rouvre son périphérique.
    banc.jouer(2, "Piste B").await;
    banc.sortie(0, None).await;
    banc.tic().await;
    banc.annonces();
    banc.dater_le_debut(Duration::from_secs(20));

    banc.sortie(1_000, Some(contrat_bit_perfect())).await;
    banc.tic().await;
    assert_eq!(
        banc.annonces(),
        1,
        "le contrat de la piste B est identique à celui de la piste A, mais le \
         client, lui, a relu une zone SANS contrat entre les deux : sans cette \
         annonce son panneau reste faux pour toute la piste (#4559)."
    );
}

// ───────────────────────── garde d'implantation ─────────────────────────
//
// Les témoins ci-dessus montent le vrai `tick`. Ils ne disent pas OÙ l'appel
// vit : déplacé dans la branche « zone au repos », il resterait vert ici et ne
// garderait plus rien pour une zone en lecture.

/// ⚠️ `include_str!` rend le fichier ENTIER. On coupe à ce module pour que les
/// motifs cherchés ne puissent pas se trouver eux-mêmes dans les assertions
/// ci-dessous (#2082).
fn code_de_production() -> &'static str {
    static PRODUCTION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PRODUCTION.get_or_init(|| {
        const TOUT: &str = include_str!("../poller.rs");
        const BORNE: &str = "mod annonce_du_contrat_de_signal_4559";
        let fin = TOUT
            .find(BORNE)
            .unwrap_or_else(|| panic!("ce module a été renommé : la découpe ne protège plus rien"));
        format!("{}{}", include_str!("../poller/tick.rs"), &TOUT[..fin])
    })
}

fn position(motif: &str) -> usize {
    code_de_production().find(motif).unwrap_or_else(|| {
        panic!(
            "motif introuvable dans le sondeur : « {motif} ».\n\
             Le code a été remanié ; ce garde-fou ne garde plus rien tant \
             qu'il n'a pas suivi. Voir #4559."
        )
    })
}

/// L'annonce doit vivre dans la branche « zone EN LECTURE » — précisément
/// dans le bras `Ok` du sondage, là où le contrat est relevé. Le sondage de la
/// branche « zone au repos » relève le même champ et n'a rien à annoncer :
/// aucun panneau n'est affiché pour une zone arrêtée.
#[test]
fn l_annonce_vit_dans_le_bras_ok_du_sondage_de_lecture() {
    let sondage_de_lecture = position("let (status, famine_anneau) = {");
    let releve = position(".contrat_de_signal_a_change(");
    let emission = position("\"contrat_de_signal_publie_annonce\"");
    let bras_erreur = position("ps.consecutive_errors = ps.consecutive_errors.saturating_add(1);");
    assert!(
        sondage_de_lecture < releve && releve < emission && emission < bras_erreur,
        "l'annonce du contrat a quitté le bras `Ok` du sondage de lecture : \
         elle ne garde plus rien pour la zone que Jean Valjean écoute (#4559)."
    );
}

/// … et la remise à zéro par piste doit rester attachée au changement de
/// génération, sans quoi une piste sur deux garderait un panneau faux.
#[test]
fn la_reference_repart_a_chaque_changement_de_piste() {
    let changement = position("// Detect track change: if the generation changed");
    let transition = position("ps.transition(fsm::Transition::NouvellePiste);");
    let remise = position("ps.reprendre_le_contrat_a_zero();");
    let fin_du_bloc = position("if ps.backoff_remaining > 0 {");
    assert!(
        changement < transition && transition < remise && remise < fin_du_bloc,
        "la référence d'annonce n'est plus remise à zéro au changement de \
         piste : un contrat republié à l'identique d'une piste à l'autre ne \
         serait jamais annoncé, et le panneau resterait « shared » (#4559)."
    );
}
