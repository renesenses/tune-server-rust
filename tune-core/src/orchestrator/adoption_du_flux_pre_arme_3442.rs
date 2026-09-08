//! #3442 — a l'avance gapless, la zone ADOPTE le flux pre-arme.
//!
//! ## La chronologie journalisee (05/09/2026, .18, zone 10, DMP-A8 en DLNA)
//!
//! ```text
//! 19:51:52  proxy_session_created stream_id=e32c865e… is_radio=false
//! 19:51:52  dlna_set_next device=DMP-A8 url=".../stream/e32c865e-….flac"
//! 19:51:52  gapless_next_set zone_id=10 title="Sugar Sugar" streaming=true
//! 19:52:21  gapless_position_reset_detected zone_id=10 prev_pos=216000 new_pos=0
//! 19:52:21  gapless_advance_on_position_reset zone_id=10 next_pos=7
//! 19:52:51  octets_servis_inconnus_zone_non_coupee zone_id=10
//!             peak_pos=6000 track_dur=226000 wall_secs=30 has_stream_id=false
//! …          wall_secs monte a 285, peak_pos reste fige a 6000
//! ```
//!
//! ## Ce qui manquait
//!
//! Le serveur POSSEDE l'identifiant qui lui manque : il l'a cree lui-meme
//! trente secondes plus tot en armant le flux suivant, et l'a mis dans l'URL
//! passee a `dlna_set_next`. `resolve_queue_item_url` le range sous
//! `gapless_sessions[zone]`. Mais `advance_queue_metadata` — la seule porte
//! par laquelle passe une avance gapless — publiait `stream_id: None` en dur.
//! Ce n'est pas une information perdue en route : c'est une information qui
//! n'etait pas transmise entre l'armement et l'avance.
//!
//! ## Les trois consequences, une par epreuve
//!
//! 1. le sondeur ne peut plus mesurer les octets servis : sa garde refuse de
//!    couper une zone dont elle ne peut pas prouver la panne
//!    (`fsm::ConsommationFlux::Inconnue`, #2394) et le silence dure sans fin ;
//! 2. `decisions::qui_tient_le_renderer` relit l'URI annoncee par l'appareil,
//!    y trouve `/stream/…` sans notre identifiant, et conclut « tenu par un
//!    AUTRE serveur Tune » ;
//! 3. l'armement SUIVANT commence par `cleanup_gapless_session`, qui retirait
//!    du gestionnaire de flux la session encore rangee sous la zone —
//!    c'est-a-dire, depuis la transition, celle que le renderer est EN TRAIN
//!    de lire.
//!
//! ## Les contre-epreuves
//!
//! - `rien_a_adopter_ne_change_rien` : l'enchainement par FICHIER local
//!   (`resolve_gapless_next_local_file`) n'ouvre aucune session ; l'avance
//!   doit alors se comporter au mot pres comme avant le correctif.
//! - `l_ancien_flux_est_libere_et_pas_fuit` : le correctif ne se contente pas
//!   de garder le flux adopte, il libere celui de la piste finie — sans quoi
//!   il echangerait une coupure contre une fuite de sessions et de fichiers
//!   de pre-transcodage.
//! - `le_site_de_production_ne_reperd_pas_l_identifiant` : les epreuves
//!   ci-dessus passeraient encore si quelqu'un remettait un `stream_id: None`
//!   dans une des deux branches de `advance_queue_metadata`. Celle-ci lit le
//!   texte du site lui-meme, meme idiome que `annonce_apres_sortie_guard`.

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::poller::fsm::{ConsommationFlux, consommation_flux};
use crate::streaming::registry::ServiceRegistry;

use super::PlaybackOrchestrator;

/// La chronologie de l'issue, arretee juste apres `gapless_next_set` : la
/// piste 1 joue sur son flux, la piste 2 est armee et son flux existe.
struct Chronologie {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    /// Le flux de la piste 1, celle qui joue.
    flux_courant: String,
    /// Le flux ouvert a l'avance pour la piste 2 — `e32c865e…` dans le
    /// journal. `None` quand l'enchainement se fait par fichier local.
    flux_pre_arme: Option<String>,
    /// Les emetteurs des sessions ouvertes : une session dont le canal est
    /// ferme n'est plus celle que le renderer tire. Ils vivent aussi
    /// longtemps que l'epreuve.
    _emetteurs: Vec<tokio::sync::mpsc::Sender<Vec<u8>>>,
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

/// Ouvre une vraie session dans le gestionnaire de flux et lui fait servir
/// `octets` octets — le compteur que le sondeur interroge.
async fn ouvrir_un_flux(
    orch: &PlaybackOrchestrator,
    octets: u64,
) -> (String, tokio::sync::mpsc::Sender<Vec<u8>>) {
    let (id, tx, _pret) = orch
        .streamer
        .create_session(
            StreamInfo {
                format: "flac".to_string(),
                mime_type: "audio/flac".to_string(),
                ..StreamInfo::default()
            },
            false,
            1,
        )
        .await;
    if octets > 0 {
        let sessions = orch.streamer.sessions_state();
        let guard = sessions.lock().await;
        guard[&id]
            .bytes_sent
            .store(octets, std::sync::atomic::Ordering::Relaxed);
    }
    (id, tx)
}

/// Rejoue la chronologie jusqu'a `gapless_next_set` inclus.
async fn armement_effectue(par_flux: bool) -> Chronologie {
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Zone 3442", Some("dlna"), Some("uuid:eversolo-dmp-a8"))
        .unwrap();

    let pistes = TrackRepo::with_backend(orch.db.clone());
    let mut ids = Vec::new();
    for n in 1..=2 {
        let mut piste = crate::db::models::Track::new(format!("Piste {n}"));
        piste.file_path = Some(format!("/aucun/chemin/3442/piste{n}.flac"));
        piste.track_number = n;
        piste.duration_ms = 226_000;
        ids.push(pistes.create(&piste).unwrap());
    }
    PlayQueueRepo::with_backend(orch.db.clone())
        .set_queue(zone_id, &ids)
        .unwrap();

    let mut emetteurs = Vec::new();

    // La piste 1 joue, sur SON flux.
    let (flux_courant, tx) = ouvrir_un_flux(&orch, 12_000_000).await;
    emetteurs.push(tx);
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                track_id: Some(ids[0]),
                title: "Piste 1".into(),
                duration_ms: 226_000,
                source: "local".into(),
                stream_id: Some(flux_courant.clone()),
                ..Default::default()
            },
        )
        .await;
    let _ = orch.playback.update_position(zone_id, 216_000).await;

    // L'armement de la piste 2. Par FLUX, c'est exactement ce que fait
    // `resolve_queue_item_url` : une session, rangee sous la zone. Par
    // FICHIER local (`resolve_gapless_next_local_file`), aucune session
    // n'est ouverte — le repere de la contre-epreuve.
    let flux_pre_arme = if par_flux {
        let (sid, tx) = ouvrir_un_flux(&orch, 900_000).await;
        emetteurs.push(tx);
        orch.gapless_sessions
            .lock()
            .await
            .insert(zone_id, sid.clone());
        Some(sid)
    } else {
        None
    };

    Chronologie {
        orch,
        zone_id,
        flux_courant,
        flux_pre_arme,
        _emetteurs: emetteurs,
    }
}

/// Ce que le sondeur LIT pour decider, tel qu'il le lit
/// (`poller/tick.rs`, bras `TransportState::Stopped`).
async fn consommation_vue_par_le_sondeur(
    orch: &PlaybackOrchestrator,
    zone_id: i64,
    octets_precedents: u64,
) -> ConsommationFlux {
    let stream_id = orch
        .playback
        .get_state(zone_id)
        .await
        .now_playing
        .and_then(|np| np.stream_id);
    let octets_servis = match stream_id.as_deref() {
        Some(sid) => orch.streamer_bytes_sent(sid).await,
        None => None,
    };
    consommation_flux(octets_servis, octets_precedents)
}

/// L'epreuve centrale : `has_stream_id=true` juste apres l'avance.
#[tokio::test]
async fn la_zone_adopte_le_flux_pre_arme_a_l_avance() {
    let c = armement_effectue(true).await;
    let attendu = c.flux_pre_arme.clone().unwrap();

    c.orch
        .advance_queue_metadata(c.zone_id, 1)
        .await
        .expect("l'avance gapless doit aboutir");

    let np = c
        .orch
        .playback
        .get_state(c.zone_id)
        .await
        .now_playing
        .expect("la zone joue la piste 2");
    assert_eq!(
        np.stream_id.as_deref(),
        Some(attendu.as_str()),
        "la zone doit porter le flux qu'elle a fait armer trente secondes plus tot \
         (has_stream_id=false dans le journal du 05/09)"
    );
    assert!(
        !c.orch
            .gapless_sessions
            .lock()
            .await
            .values()
            .any(|s| s == &attendu),
        "le flux adopte n'est plus « a jeter » : il ne doit plus figurer comme \
         session pre-armee de la zone"
    );
}

/// La garde retrouve de quoi juger : ce n'est plus « inconnu », donc la zone
/// muette finit par etre coupee bruyamment au lieu de se taire cinq minutes.
#[tokio::test]
async fn la_garde_mesure_de_nouveau_et_cesse_de_se_desarmer() {
    let c = armement_effectue(true).await;

    // Avant l'avance, la zone porte son propre flux : rien d'anormal.
    assert_eq!(
        consommation_vue_par_le_sondeur(&c.orch, c.zone_id, 0).await,
        ConsommationFlux::Consomme
    );

    c.orch
        .advance_queue_metadata(c.zone_id, 1)
        .await
        .expect("l'avance gapless doit aboutir");

    // Le renderer a servi 900 000 octets puis s'est fige a 6 s : le compteur
    // ne bouge plus. MESURE, et a sec — le seul etat qui autorise la coupure.
    assert_eq!(
        consommation_vue_par_le_sondeur(&c.orch, c.zone_id, 900_000).await,
        ConsommationFlux::ASec,
        "le compteur est connu et n'avance plus : la garde doit pouvoir couper \
         (au lieu de octets_servis_inconnus_zone_non_coupee, repete 285 s)"
    );
    // Et un flux qui delivre reste un flux qui delivre : la garde n'est pas
    // devenue coupeuse pour autant.
    assert_eq!(
        consommation_vue_par_le_sondeur(&c.orch, c.zone_id, 0).await,
        ConsommationFlux::Consomme
    );
}

/// Deuxieme consequence du meme oubli : l'appareil passait pour tenu par un
/// autre serveur Tune, puisque l'URI qu'il annonce porte un identifiant que
/// la zone ne connaissait plus.
#[tokio::test]
async fn le_renderer_reste_le_notre_apres_l_avance() {
    use crate::poller::decisions::{TenueDuRenderer, qui_tient_le_renderer};

    let c = armement_effectue(true).await;
    let uri =
        c.orch
            .streamer
            .get_stream_url(c.flux_pre_arme.as_deref().unwrap(), "192.168.1.18", "flac");

    c.orch
        .advance_queue_metadata(c.zone_id, 1)
        .await
        .expect("l'avance gapless doit aboutir");

    let notre = c
        .orch
        .playback
        .get_state(c.zone_id)
        .await
        .now_playing
        .and_then(|np| np.stream_id);
    assert_eq!(
        qui_tient_le_renderer(Some(&uri), notre.as_deref()),
        TenueDuRenderer::LeNotre,
        "l'URI que l'appareil annonce est celle du flux que Tune lui a donne : \
         sans l'adoption, elle passait pour celle d'un AUTRE serveur Tune"
    );
}

/// Troisieme consequence : l'armement suivant commence par
/// `cleanup_gapless_session`. Tant que la session rangee sous la zone etait
/// celle du morceau EN COURS, cet armement la retirait du gestionnaire de
/// flux — fichier de pre-transcodage compris.
///
/// Contre-epreuve du meme geste : l'ancien flux, lui, doit bien etre libere.
/// Un correctif qui garderait les deux echangerait une coupure contre une
/// fuite.
#[tokio::test]
async fn l_armement_suivant_ne_retire_plus_le_flux_qui_joue() {
    let c = armement_effectue(true).await;
    let adopte = c.flux_pre_arme.clone().unwrap();

    c.orch
        .advance_queue_metadata(c.zone_id, 1)
        .await
        .expect("l'avance gapless doit aboutir");

    // Ce que fait l'armement de la piste 3, en premier.
    c.orch.cleanup_gapless_session(c.zone_id).await;

    assert!(
        c.orch.stream_session_alive(&adopte).await,
        "l'armement suivant ne doit plus retirer le flux que le renderer est \
         en train de lire"
    );
    assert!(
        !c.orch.stream_session_alive(&c.flux_courant).await,
        "et le flux de la piste finie doit bien etre libere : pas de fuite de \
         sessions ni de fichiers de pre-transcodage"
    );
}

/// Contre-epreuve : quand il n'y a RIEN a adopter — enchainement par fichier
/// local, qui n'ouvre aucune session — l'avance ne doit rien changer a ce
/// qu'elle faisait, et surtout ne rien liberer.
#[tokio::test]
async fn rien_a_adopter_ne_change_rien() {
    let c = armement_effectue(false).await;

    c.orch
        .advance_queue_metadata(c.zone_id, 1)
        .await
        .expect("l'avance gapless doit aboutir");

    let np = c
        .orch
        .playback
        .get_state(c.zone_id)
        .await
        .now_playing
        .expect("la zone joue la piste 2");
    assert_eq!(
        np.stream_id, None,
        "aucun flux n'a ete pre-arme : il n'y a rien a porter"
    );
    assert!(
        c.orch.gapless_sessions.lock().await.is_empty(),
        "et rien n'a ete range sous la zone : l'echange ne se fait que \
         lorsqu'une adoption a bien eu lieu"
    );
    assert!(
        c.orch.stream_session_alive(&c.flux_courant).await,
        "le flux de la piste precedente n'est pas libere par ce chemin — \
         c'est le comportement d'avant le correctif, au mot pres"
    );
}

/// Garde de SITE. Les epreuves ci-dessus resteraient vertes si quelqu'un
/// remettait un `stream_id: None` dans l'une des deux branches de
/// `advance_queue_metadata` tout en gardant l'autre : la file a deux formes
/// de piste, locale et streaming, et le journal du 05/09 porte la seconde.
/// Meme idiome que `annonce_apres_sortie_guard` : on lit le texte du site.
#[test]
fn le_site_de_production_ne_reperd_pas_l_identifiant() {
    let source = include_str!("queue.rs");
    let debut = source
        .find("pub async fn advance_queue_metadata")
        .expect("advance_queue_metadata doit exister dans orchestrator/queue.rs");
    let reste = &source[debut..];
    let fin = reste[1..]
        .find("\n    pub async fn ")
        .map(|i| i + 1)
        .unwrap_or(reste.len());
    let corps = &reste[..fin];

    assert!(
        corps.contains("gapless_sessions"),
        "l'avance gapless doit prendre le flux pre-arme range sous la zone (#3442)"
    );
    assert!(
        !corps.contains("stream_id: None"),
        "aucune branche de l'avance gapless ne doit reposer un `stream_id: None` \
         en dur : c'est exactement ce qui rendait has_stream_id=false et \
         desarmait la garde (#3442, suite de #2394)"
    );
    assert_eq!(
        corps.matches("stream_id: flux_adopte").count(),
        2,
        "les DEUX branches — piste locale et piste streaming — doivent porter \
         le flux adopte"
    );
}
