use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const APPAREIL: &str = "dlna:diretta-renderer-88782b77cab17717";
/// La piste en cours. Longue, pour que la fenêtre des 30 s soit un vrai
/// intervalle et non toute la piste.
const DUREE_COURANTE_MS: i64 = 300_000;
/// Toutes les SUIVANTES ont la même durée, exprès : après la transition le
/// renderer annonce cette durée quelle que soit celle des trois qu'il joue,
/// donc la durée rapportée ne peut pas trahir la réponse qu'on cherche.
const DUREE_SUIVANTE_MS: i64 = 200_000;

/// Les quatre titres du banc, tous DISTINCTS et sur quatre pistes
/// distinctes : les gardes anti-doublon de la lecture coalescent une
/// relecture du même identifiant de piste, et un test qui rejouerait deux
/// fois la même piste serait vert sans rien prouver.
const COURANTE: &str = "Voir un ami pleurer";
const ARMEE: &str = "Script Switch Trigger";
const SUITE: &str = "Hold Me";
const INSEREE: &str = "Freddie Freeloader";

/// Un WAV minuscule mais réel : `resolve_stream` ouvre le fichier, un
/// chemin qui ne mène à rien ne s'armerait pas.
fn ecrire_wav(chemin: &std::path::Path) {
    use std::io::Write;
    let octets_data: u32 = 44_100 * 4 / 5; // 200 ms, 16 bits stéréo
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + octets_data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&44_100u32.to_le_bytes());
    v.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&octets_data.to_le_bytes());
    v.extend(std::iter::repeat_n(0u8, octets_data as usize));
    let mut f = std::fs::File::create(chemin).unwrap();
    f.write_all(&v).unwrap();
    f.flush().unwrap();
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    db: Arc<dyn crate::db::backend::DbBackend>,
    outputs: Arc<Mutex<OutputRegistry>>,
    zone_id: i64,
    pistes: Vec<i64>,
    _fichiers: Vec<tempfile::NamedTempFile>,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    /// Une zone DLNA en lecture de `COURANTE`, file `[COURANTE, ARMEE,
    /// SUITE]`. `INSEREE` existe en bibliothèque mais pas encore en file :
    /// c'est elle que le geste ajoutera.
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);

        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("dlna"), Some(APPAREIL))
            .unwrap();

        let depot = TrackRepo::with_backend(db.clone());
        let mut fichiers = Vec::new();
        let mut pistes = Vec::new();
        for (n, titre) in [COURANTE, ARMEE, SUITE, INSEREE].iter().enumerate() {
            let f = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
            ecrire_wav(f.path());
            let mut piste = crate::db::models::Track::new((*titre).to_string());
            piste.file_path = Some(f.path().to_str().unwrap().to_string());
            piste.format = Some("wav".into());
            piste.sample_rate = Some(44_100);
            piste.bit_depth = Some(16);
            piste.channels = 2;
            piste.track_number = n as i32 + 1;
            piste.duration_ms = if n == 0 {
                DUREE_COURANTE_MS
            } else {
                DUREE_SUIVANTE_MS
            };
            pistes.push(depot.create(&piste).unwrap());
            fichiers.push(f);
        }
        PlayQueueRepo::with_backend(db.clone())
            .set_queue(zone_id, &pistes[..3])
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Diretta Renderer").with_type("dlna"),
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
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(Arc::new(EventBus::new()));

        playback
            .play(
                zone_id,
                NowPlaying {
                    track_id: Some(pistes[0]),
                    title: COURANTE.into(),
                    source: "local".into(),
                    duration_ms: DUREE_COURANTE_MS,
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone_id, 0, 3).await;

        let generation = playback.get_state(zone_id).await.track_generation;
        let mut poll_states = HashMap::new();
        poll_states.insert(zone_id, ZonePollState::new(generation));

        Self {
            poller,
            playback,
            db,
            outputs,
            zone_id,
            pistes,
            _fichiers: fichiers,
            poll_states,
            idle: HashMap::new(),
        }
    }

    /// Porter la lecture à `position_ms` SANS attendre : la position du
    /// renderer et l'horloge de fin de piste du poller sont deux champs, on
    /// les écrit. C'est l'injection réclamée — pas un `sleep` déguisé.
    async fn a(&mut self, position_ms: u64) {
        {
            let reg = self.outputs.lock().await;
            let arc = reg.get(APPAREIL).unwrap();
            let sortie = arc.lock().await;
            let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
            mock.set_state(crate::outputs::traits::TransportState::Playing)
                .await;
            mock.set_duration(DUREE_COURANTE_MS as u64);
            mock.set_position(position_ms);
        }
        let ps = self.poll_states.get_mut(&self.zone_id).unwrap();
        // Le mur d'horloge de `played_enough` (30 s) et celui de
        // `stale_start_position` : on les franchit en datant le début de la
        // piste, pas en dormant.
        ps.track_started_at = Some(Instant::now() - Duration::from_secs(320));
        ps.peak_position_ms = position_ms;
        ps.last_position_ms = position_ms;
    }

    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    /// Le renderer enchaîne : il passe sur l'URI qu'on lui a armée et
    /// repart de zéro. C'est la chute de position du journal
    /// (`gapless_position_reset_detected prev_pos=… new_pos=0`).
    async fn le_renderer_enchaine(&mut self) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .simulate_gapless_transition(DUREE_SUIVANTE_MS as u64)
            .await;
    }

    /// « Lire ensuite » : exactement ce que fait la route — `insert_at` à la
    /// position demandée, puis `update_queue_info` avec le nouveau total.
    async fn lire_ensuite(&self, indice_piste: usize, position: i64) {
        let depot = PlayQueueRepo::with_backend(self.db.clone());
        depot
            .insert_at(
                self.zone_id,
                &[QueueInput::Local {
                    track_id: self.pistes[indice_piste],
                }],
                Some(position),
            )
            .unwrap();
        let total = depot.count_all(self.zone_id).unwrap();
        let courante = self.playback.get_state(self.zone_id).await.queue_position;
        self.playback
            .update_queue_info(self.zone_id, courante, total)
            .await;
    }

    /// « + File » : le même ajout, mais en FIN de file. Rien ne bouge avant
    /// la piste armée.
    async fn ajouter_en_fin(&self, indice_piste: usize) {
        let depot = PlayQueueRepo::with_backend(self.db.clone());
        depot
            .append(
                self.zone_id,
                &[QueueInput::Local {
                    track_id: self.pistes[indice_piste],
                }],
            )
            .unwrap();
        let total = depot.count_all(self.zone_id).unwrap();
        let courante = self.playback.get_state(self.zone_id).await.queue_position;
        self.playback
            .update_queue_info(self.zone_id, courante, total)
            .await;
    }

    /// Ce qui est PARTI au renderer par `SetNextAVTransportURI`, dans
    /// l'ordre. La mesure du ticket.
    async fn armees(&self) -> Vec<String> {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .set_next_titles()
            .await
    }

    /// Ce que le renderer JOUE réellement, nommé par le titre qu'on lui a
    /// donné avec l'URI.
    async fn joue_par_le_renderer(&self) -> Option<String> {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .current_title()
            .await
    }

    /// Les `Play` complets envoyés au renderer. Un enchaînement sans blanc
    /// n'en produit AUCUN : dès qu'il y en a un, il y a eu un arrêt et une
    /// relance, c'est-à-dire un blanc.
    async fn play_complets(&self) -> Vec<String> {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .play_titles()
            .await
    }

    /// Ce que l'écran affiche : la position dans la file, et le titre.
    async fn ecran(&self) -> (i64, String) {
        let etat = self.playback.get_state(self.zone_id).await;
        (
            etat.queue_position,
            etat.now_playing.map(|np| np.title).unwrap_or_default(),
        )
    }

    /// La file telle qu'elle est rendue à l'écran, dans l'ordre.
    async fn file_affichee(&self) -> Vec<String> {
        PlayQueueRepo::with_backend(self.db.clone())
            .get_ordered(self.zone_id)
            .unwrap()
            .into_iter()
            .map(|e| e.title.unwrap_or_default())
            .collect()
    }
}

/// **LE FAIT DE BASE.** « Lire ensuite » à quinze secondes de la fin — la
/// chronologie exacte du journal du 31/08.
///
/// Ce qui doit partir au renderer, dans l'ordre : la piste armée d'abord
/// (personne n'avait rien demandé), puis **l'insérée**, parce que le geste
/// de l'utilisateur est explicite et postérieur. Et à la transition, la
/// piste que le renderer joue doit être celle que l'écran nomme.
///
/// Sans le correctif, la seconde ligne n'existe pas : le renderer garde la
/// piste armée, l'écran part sur l'insérée, et les deux se contredisent.
#[tokio::test]
async fn le_geste_de_l_utilisateur_atteint_le_renderer_et_l_ecran_dit_vrai() {
    let mut banc = Banc::monter().await;

    // 14:10:46 — l'armement, quinze secondes avant l'insertion.
    banc.a(275_000).await;
    banc.tic().await;
    assert_eq!(
        banc.armees().await,
        vec![ARMEE.to_string()],
        "la fenêtre d'armement doit avoir envoyé la piste suivante au renderer"
    );

    // 14:11:01 — « Lire ensuite ». L'insérée prend la position 1, la piste
    // armée glisse en 2 : même ligne, autre position.
    banc.lire_ensuite(3, 1).await;
    assert_eq!(
        banc.file_affichee().await,
        vec![
            COURANTE.to_string(),
            INSEREE.to_string(),
            ARMEE.to_string(),
            SUITE.to_string()
        ],
        "la file doit porter l'insérée juste après la piste en cours"
    );

    // Le tick suivant, toujours dans la fenêtre.
    banc.a(276_000).await;
    banc.tic().await;
    assert_eq!(
        banc.armees().await,
        vec![ARMEE.to_string(), INSEREE.to_string()],
        "LE FAIT DE BASE : après le geste, c'est l'INSÉRÉE qui doit partir              au renderer, et après la piste déjà armée — pas à sa place, pas              jamais"
    );

    // 14:11:16 — le renderer enchaîne.
    banc.le_renderer_enchaine().await;
    banc.tic().await;

    let (position, titre) = banc.ecran().await;
    assert_eq!(
        banc.joue_par_le_renderer().await.as_deref(),
        Some(INSEREE),
        "le renderer doit jouer l'insérée : c'est la dernière URI qu'on lui              a donnée"
    );
    assert_eq!(
        titre, INSEREE,
        "l'écran doit nommer ce qui joue, pas un index de file"
    );
    assert_eq!(position, 1, "et pointer la ligne de file correspondante");
}

/// **LA COURSE, dans sa forme la plus serrée** : l'insertion tombe ENTRE
/// deux sondages. Aucun tick n'a pu ré-armer, le renderer joue donc encore
/// la piste armée — et c'est ELLE que l'écran doit nommer.
///
/// C'est le cas qui produisait la coupure du 01/09 : avancer sur l'index+1
/// nommait l'insérée, dont la durée devenait la limite du compteur de fin
/// de piste ; à 346 s d'un flux qu'on croyait long de 340 s, Tune coupait
/// l'audio réel (`dlna_frozen_end=true`). L'insertion garde sa ligne dans
/// la file ; elle perd son tour. C'est le seul prix de la course.
#[tokio::test]
async fn insertion_entre_deux_sondages_l_ecran_suit_ce_qui_joue() {
    let mut banc = Banc::monter().await;
    banc.a(275_000).await;
    banc.tic().await;
    assert_eq!(banc.armees().await, vec![ARMEE.to_string()]);

    // Le geste, puis la transition, sans un seul tick entre les deux.
    banc.lire_ensuite(3, 1).await;
    banc.le_renderer_enchaine().await;
    banc.tic().await;

    let (position, titre) = banc.ecran().await;
    assert_eq!(
        banc.joue_par_le_renderer().await.as_deref(),
        Some(ARMEE),
        "aucun tick n'a pu ré-armer : le renderer joue toujours la piste armée"
    );
    assert_eq!(
        titre, ARMEE,
        "l'écran doit dire la piste ARMÉE — la nommer autrement, c'est ce              qui faisait adopter la durée de l'insérée et couper l'audio réel"
    );
    assert_eq!(
        position, 2,
        "la piste armée a glissé en position 2 ; c'est là que l'écran doit              pointer, pas sur l'index+1 courant"
    );
    assert_eq!(
        banc.armees().await,
        vec![ARMEE.to_string()],
        "rien de neuf n'est parti au renderer : il n'y avait pas de tick pour              le faire"
    );
}

/// **TÉMOIN VERT — l'enchaînement sans blanc reste intact.** Personne ne
/// touche à la file : dix sondages dans la fenêtre ne doivent produire
/// qu'UN seul `SetNextAVTransportURI`, et la transition ne doit produire
/// AUCUN `Play` complet.
///
/// C'est la garde contre le faux correctif : désarmer le gapless dès qu'on
/// touche à la file supprimerait le défaut en supprimant la fonctionnalité.
#[tokio::test]
async fn sans_geste_l_enchainement_sans_blanc_est_intact() {
    let mut banc = Banc::monter().await;
    for ms in 0..10u64 {
        banc.a(275_000 + ms * 1_000).await;
        banc.tic().await;
    }
    assert_eq!(
        banc.armees().await,
        vec![ARMEE.to_string()],
        "un seul armement par piste : re-préparer à chaque tick, c'est              re-résoudre et re-télécharger la suivante une fois par seconde"
    );

    banc.le_renderer_enchaine().await;
    banc.tic().await;

    let (position, titre) = banc.ecran().await;
    assert_eq!(
        titre, ARMEE,
        "l'enchaînement normal doit avancer d'une piste"
    );
    assert_eq!(position, 1);
    assert_eq!(
        banc.joue_par_le_renderer().await.as_deref(),
        Some(ARMEE),
        "et le renderer joue bien cette piste-là"
    );
    assert!(
        banc.play_complets().await.is_empty(),
        "aucun `Play` complet : un enchaînement qui repasse par un arrêt et              une relance, c'est précisément le blanc qu'on ne veut pas payer"
    );
}

/// **TÉMOIN VERT — un ajout en FIN de file ne coûte rien.** La question 2
/// posée au testeur, tranchée par la mesure : « + File » pendant la fenêtre
/// ne déplace rien avant la piste armée, donc ne doit RIEN désarmer.
#[tokio::test]
async fn un_ajout_en_fin_de_file_ne_desarme_rien() {
    let mut banc = Banc::monter().await;
    banc.a(275_000).await;
    banc.tic().await;
    assert_eq!(banc.armees().await, vec![ARMEE.to_string()]);

    banc.ajouter_en_fin(3).await;
    banc.a(276_000).await;
    banc.tic().await;
    assert_eq!(
        banc.armees().await,
        vec![ARMEE.to_string()],
        "la file a changé mais la piste armée est toujours la suivante :              rien ne doit repartir au renderer"
    );

    banc.le_renderer_enchaine().await;
    banc.tic().await;
    assert_eq!(banc.ecran().await, (1, ARMEE.to_string()));
    assert!(
        banc.play_complets().await.is_empty(),
        "l'enchaînement sans blanc doit survivre à un ajout en fin de file"
    );
}

/// **TÉMOIN VERT — hors de la fenêtre, rien ne change.** À mi-morceau, rien
/// n'est armé : « Lire ensuite » se comporte exactement comme avant, et
/// c'est l'insérée qui sera armée le moment venu.
///
/// C'est la question 1 posée au testeur, celle qui tranche : le défaut est
/// bien circonscrit aux trente dernières secondes.
#[tokio::test]
async fn hors_de_la_fenetre_lire_ensuite_se_comporte_comme_avant() {
    let mut banc = Banc::monter().await;

    // Mi-morceau : la fenêtre s'ouvre à 270 s, on est loin.
    banc.a(150_000).await;
    banc.tic().await;
    assert!(
        banc.armees().await.is_empty(),
        "hors fenêtre, rien ne doit être armé"
    );

    banc.lire_ensuite(3, 1).await;
    banc.a(151_000).await;
    banc.tic().await;
    assert!(
        banc.armees().await.is_empty(),
        "un geste hors fenêtre ne doit rien envoyer au renderer : il n'y a              rien à corriger"
    );

    // La fenêtre s'ouvre enfin.
    banc.a(275_000).await;
    banc.tic().await;
    assert_eq!(
        banc.armees().await,
        vec![INSEREE.to_string()],
        "et c'est l'insérée qui est armée, une seule fois, au moment normal"
    );

    banc.le_renderer_enchaine().await;
    banc.tic().await;
    assert_eq!(banc.ecran().await, (1, INSEREE.to_string()));
    assert_eq!(banc.joue_par_le_renderer().await.as_deref(), Some(INSEREE));
}

/// Le prédicat seul, sur ses quatre cas. Il décide de désarmer un
/// enchaînement déjà accepté par le renderer : son sens de défaut doit être
/// écrit noir sur blanc.
#[test]
fn le_predicat_ne_conclut_rien_sans_les_deux_identifiants() {
    // La ligne armée est toujours la suivante : on ne touche à rien.
    assert!(!decisions::gapless_arm_outdated(Some(7), Some(7)));
    // Une AUTRE ligne occupe la place : le geste a bougé la file.
    assert!(decisions::gapless_arm_outdated(Some(7), Some(9)));
    // Rien n'a été armé : il n'y a rien à désarmer.
    assert!(!decisions::gapless_arm_outdated(None, Some(9)));
    // La file ne rend plus rien à cette position : on ne désarme pas sur une
    // absence — un « périmé » de trop fait payer un blanc à qui n'a rien
    // demandé.
    assert!(!decisions::gapless_arm_outdated(Some(7), None));
}
