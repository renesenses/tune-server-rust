//! #7895 — témoins : « Suivant » et « Précédent » enjambent un titre que le
//! service refuse, le disent, et ne bouclent pas sur une file entièrement
//! refusée.
//!
//! Faux service : un fournisseur d'URL (`crate::source_url`) qui refuse
//! certaines références avec le motif propre au titre, sur une zone DLNA
//! simulée (`MockOutput`). Aucun son, aucun réseau : l'URL rendue pointe sur
//! le port 9 et n'est jamais lue.

use super::enjamber_les_refus::{SensDuGeste, TitreSaute, message_titres_sautes};
use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::{PlaybackManager, RepeatMode};
use crate::source_url::{FournisseurDUrl, RefusDUrl, UrlFournie};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const APPAREIL: &str = "dlna:banc-refus-7895";
const SOURCE: &str = "circle";

#[derive(Clone)]
enum Reponse {
    Url,
    Refus,
    Arret,
}

struct FauxService {
    reponses: HashMap<String, Reponse>,
    demandes: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl FournisseurDUrl for FauxService {
    async fn url(&self, _zone_id: i64, source_id: &str) -> Result<UrlFournie, RefusDUrl> {
        self.demandes.lock().unwrap().push(source_id.to_string());
        match self
            .reponses
            .get(source_id)
            .cloned()
            .unwrap_or(Reponse::Url)
        {
            Reponse::Url => Ok(UrlFournie {
                url: format!("http://127.0.0.1:9/stream/{}", source_id.replace(':', "-")),
                media_format: Some("flac".into()),
            }),
            Reponse::Refus => Err(RefusDUrl::Piste("not_available".into())),
            Reponse::Arret => Err(RefusDUrl::ArretDeLaFile("owner_offline".into())),
        }
    }
}

struct Banc {
    orchestrator: Arc<PlaybackOrchestrator>,
    playback: Arc<PlaybackManager>,
    zone_id: i64,
    service: Arc<FauxService>,
    bus: Arc<EventBus>,
}

fn ligne(reference: &str, titre: &str) -> QueueInput {
    QueueInput::Streaming {
        source: SOURCE.into(),
        source_id: reference.into(),
        title: titre.into(),
        artist: "Miles Davis".into(),
        album: Some("Kind of Blue".into()),
        cover_url: None,
        duration_ms: 200_000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}

impl Banc {
    async fn monter(reponses: &[(&str, Reponse)]) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("dlna"), Some(APPAREIL))
            .unwrap();
        PlayQueueRepo::with_backend(db.clone())
            .append(
                zone_id,
                &[
                    ligne("7:1", "So What"),
                    ligne("7:2", "Freddie Freeloader"),
                    ligne("7:3", "Blue in Green"),
                ],
            )
            .unwrap();
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Renderer").with_type("dlna"),
        ));
        let playback = Arc::new(PlaybackManager::new());
        let bus = Arc::new(EventBus::new());
        let mut orchestrator = PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs,
            None,
        );
        orchestrator.event_bus = Some(bus.clone());
        let orchestrator = Arc::new(orchestrator);
        let service = Arc::new(FauxService {
            reponses: reponses
                .iter()
                .map(|(r, v)| (r.to_string(), v.clone()))
                .collect(),
            demandes: std::sync::Mutex::new(Vec::new()),
        });
        orchestrator.sources_url().inscrire(SOURCE, service.clone());
        playback.update_queue_info(zone_id, 0, 3).await;
        Self {
            orchestrator,
            playback,
            zone_id,
            service,
            bus,
        }
    }

    fn demandes(&self) -> Vec<String> {
        self.service.demandes.lock().unwrap().clone()
    }

    fn oublier_les_demandes(&self) {
        self.service.demandes.lock().unwrap().clear();
    }

    async fn joue(&self) -> (i64, Option<String>) {
        let etat = self.playback.get_state(self.zone_id).await;
        (
            etat.queue_position,
            etat.now_playing.and_then(|n| n.source_id),
        )
    }

    /// Le geste, borné : une boucle sans fin ferait échouer le témoin au lieu
    /// de le bloquer.
    async fn geste(&self, depart: i64, sens: SensDuGeste) -> Result<(), String> {
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            self.orchestrator
                .jouer_en_enjambant_les_refus(self.zone_id, depart, sens),
        )
        .await
        .expect("#7895 : le geste n'a pas rendu la main, il boucle sur la file")
        .map(|_| ())
    }
}

fn messages_non_fatals(
    rx: &mut tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
) -> Vec<String> {
    let mut vus = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if e.event_type == "zone.playback_error" && e.data["fatal"] == false {
            vus.push(e.data["error"].as_str().unwrap_or_default().to_string());
        }
    }
    vus
}

/// « Suivant » depuis 7:1 tombe sur 7:2, refusé : 7:3 doit jouer, et le
/// message nomme le titre sauté et la raison.
#[tokio::test]
async fn suivant_enjambe_le_titre_refuse_et_le_dit_7895() {
    let banc = Banc::monter(&[("7:2", Reponse::Refus)]).await;
    let mut rx = banc.bus.subscribe();

    let rendu = banc.geste(1, SensDuGeste::Suivant).await;

    assert!(
        rendu.is_ok(),
        "#7895 : « Suivant » s'est arrêté sur le titre refusé au lieu de \
         passer au suivant jouable : {rendu:?}"
    );
    assert_eq!(banc.demandes(), vec!["7:2", "7:3"]);
    assert_eq!(
        banc.joue().await,
        (2, Some("7:3".into())),
        "#7895 : 7:2 refusé, « Suivant » devait jouer 7:3"
    );
    let messages = messages_non_fatals(&mut rx);
    assert!(
        messages
            .iter()
            .any(|m| m.contains("Freddie Freeloader") && m.contains("refusé par sa source")),
        "#7895 : aucun message ne dit quel titre a été sauté et pourquoi : {messages:?}"
    );
}

/// « Précédent » depuis 7:3 tombe sur 7:2, refusé : 7:1 doit jouer.
#[tokio::test]
async fn precedent_enjambe_le_titre_refuse_7895() {
    let banc = Banc::monter(&[("7:2", Reponse::Refus)]).await;
    banc.playback.update_queue_info(banc.zone_id, 2, 3).await;
    let mut rx = banc.bus.subscribe();

    let rendu = banc.geste(1, SensDuGeste::Precedent).await;

    assert!(
        rendu.is_ok(),
        "#7895 : « Précédent » s'est arrêté sur le titre refusé au lieu de \
         reculer au titre jouable : {rendu:?}"
    );
    assert_eq!(banc.demandes(), vec!["7:2", "7:1"]);
    assert_eq!(banc.joue().await, (0, Some("7:1".into())));
    assert!(
        messages_non_fatals(&mut rx)
            .iter()
            .any(|m| m.contains("Freddie Freeloader")),
        "#7895 : le titre sauté par « Précédent » n'est pas nommé"
    );
}

/// File entièrement refusée, en répétition de toute la file : le geste rend
/// la main avec une erreur, chaque titre n'est demandé qu'une fois.
#[tokio::test]
async fn une_file_entierement_refusee_ne_boucle_pas_7895() {
    let banc = Banc::monter(&[
        ("7:1", Reponse::Refus),
        ("7:2", Reponse::Refus),
        ("7:3", Reponse::Refus),
    ])
    .await;
    banc.playback
        .set_repeat(banc.zone_id, RepeatMode::All)
        .await;

    let rendu = banc.geste(1, SensDuGeste::Suivant).await;

    let erreur = rendu.expect_err("rien n'est jouable : le geste devait échouer");
    assert!(
        erreur.contains("aucun titre jouable"),
        "l'erreur doit dire qu'aucun titre n'est jouable : {erreur}"
    );
    let mut demandes = banc.demandes();
    demandes.sort();
    assert_eq!(
        demandes,
        vec!["7:1", "7:2", "7:3"],
        "chaque titre est essayé une seule fois"
    );

    // Même chose en répétition d'un titre : le geste manuel l'ignore, mais ne
    // boucle pas non plus.
    banc.oublier_les_demandes();
    banc.playback
        .set_repeat(banc.zone_id, RepeatMode::One)
        .await;
    assert!(banc.geste(1, SensDuGeste::Suivant).await.is_err());
    assert_eq!(banc.demandes().len(), 3);

    // Et « Précédent » s'arrête au début de la file.
    banc.oublier_les_demandes();
    assert!(banc.geste(1, SensDuGeste::Precedent).await.is_err());
    assert_eq!(banc.demandes(), vec!["7:2", "7:1"]);
}

/// Ce qui n'est pas un refus propre au titre n'est PAS enjambé : la source
/// qui arrête la file arrête aussi le geste, sans demander la suite.
#[tokio::test]
async fn un_arret_de_la_source_n_est_pas_enjambe_7895() {
    let banc = Banc::monter(&[("7:2", Reponse::Arret)]).await;
    let mut rx = banc.bus.subscribe();

    let rendu = banc.geste(1, SensDuGeste::Suivant).await;

    assert!(rendu.is_err());
    assert_eq!(
        banc.demandes(),
        vec!["7:2"],
        "7:3 ne devait pas être demandée"
    );
    while let Ok(e) = rx.try_recv() {
        assert_ne!(e.event_type, "playback.track_skipped", "{}", e.data);
    }
}

#[test]
fn le_message_nomme_les_titres_et_la_raison_7895() {
    let t = |p: i64, titre: Option<&str>| TitreSaute {
        position: p,
        titre: titre.map(str::to_string),
        raison: "non disponible sur le service pour ce compte",
    };
    assert_eq!(message_titres_sautes(&[]), None);
    assert_eq!(
        message_titres_sautes(&[t(1, Some("So What"))]).unwrap(),
        "Titre sauté : « So What » (non disponible sur le service pour ce compte)."
    );
    let plusieurs = message_titres_sautes(&[
        t(1, Some("A")),
        t(2, None),
        t(3, Some("C")),
        t(4, Some("D")),
    ])
    .unwrap();
    assert!(plusieurs.starts_with("4 titres sautés : « A », le titre n° 3, « C » et 1 autre(s)"));
}

/// #8227 — fil 2723 (rc11) : « Suivant » sur le premier titre d'un album dont
/// le service refuse TOUS les titres suivants. Le journal :
/// `geste_enjambe_piste_refusee` positions 2 à 9, puis `piste_non_demarree
/// cause="aucun titre jouable, 8 titre(s) refusé(s) par le service (no url)"`.
/// L'auditeur lisait « ce titre n'est pas disponible », comme si un seul
/// titre manquait et que la lecture aurait dû continuer.
///
/// Contre-épreuve : retirer de `message_piste_non_demarree` la branche
/// `titres_refuses_sans_titre_jouable` fait tomber ce test.
#[tokio::test]
async fn aucun_titre_jouable_se_dit_pour_toute_la_file_8227() {
    let banc = Banc::monter(&[
        ("7:1", Reponse::Url),
        ("7:2", Reponse::Refus),
        ("7:3", Reponse::Refus),
        ("7:4", Reponse::Refus),
    ])
    .await;

    let erreur = banc
        .geste(1, SensDuGeste::Suivant)
        .await
        .expect_err("les titres suivants sont tous refusés");
    let message = PlaybackOrchestrator::message_piste_non_demarree("suivante", &erreur);

    assert!(
        !message.contains("ce titre"),
        "#8227 : trois titres refusés, le message parle d'un seul : {message}"
    );
    assert!(
        message.contains("aucun titre de la file") && message.contains("3 titre(s)"),
        "#8227 : le message doit dire que la file n'a plus de titre jouable, et combien \
         ont été refusés : {message}"
    );
    // Un refus isolé garde sa phrase.
    assert!(
        PlaybackOrchestrator::message_piste_non_demarree("suivante", "no url")
            .contains("ce titre n'est pas disponible")
    );
}
