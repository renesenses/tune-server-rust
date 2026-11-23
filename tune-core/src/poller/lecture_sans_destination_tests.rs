use super::{DELAI_SILENCE_ETABLI, PositionPoller, TAILLE_MAXIMALE_D_UNE_SONDE, decisions};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlayState, PlaybackManager, ZoneState};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    orchestrator: Arc<PlaybackOrchestrator>,
    db: Arc<dyn crate::db::backend::DbBackend>,
    recu: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    tmp: tempfile::TempDir,
}

impl Banc {
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let playback = Arc::new(PlaybackManager::new());
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
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
            orchestrator.clone(),
            playback.clone(),
            outputs,
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(bus);
        Self {
            poller,
            playback,
            orchestrator,
            db,
            recu,
            tmp: tempfile::TempDir::new().unwrap(),
        }
    }

    /// Une zone en lecture, sans AUCUN périphérique de sortie, dont le flux
    /// a servi `octets`. C'est la scène du ticket.
    async fn zone_en_lecture(&self, nom: &str, output_type: &str, octets: u64) -> i64 {
        let zone_id = ZoneRepo::with_backend(self.db.clone())
            .create(nom, Some(output_type), None)
            .unwrap();
        let fichier = self.tmp.path().join(format!("{zone_id}.flac"));
        std::fs::write(&fichier, b"fake audio").unwrap();
        let sid = self
            .orchestrator
            .streamer
            .create_file_session(
                crate::http::streamer::StreamInfo {
                    format: "flac".into(),
                    mime_type: "audio/flac".into(),
                    ..Default::default()
                },
                fichier.to_string_lossy().into_owned(),
                false,
            )
            .await;
        if octets > 0 {
            let sessions = self.orchestrator.streamer.sessions_state();
            let sessions = sessions.lock().await;
            sessions
                .get(&sid)
                .expect("la session vient d'être créée")
                .bytes_sent
                .store(octets, std::sync::atomic::Ordering::Relaxed);
        }
        self.playback
            .play(
                zone_id,
                NowPlaying {
                    title: "Never Make It on Time".into(),
                    stream_id: Some(sid),
                    source: "local".into(),
                    duration_ms: 240_000,
                    ..Default::default()
                },
            )
            .await;
        zone_id
    }

    /// L'instantané que le poller reçoit, vieilli de `age`. Vieillir l'état
    /// plutôt que dormir : le seuil est de douze secondes.
    async fn instantane(&self, zone_id: i64, age: Duration) -> ZoneState {
        let mut zs = self.playback.get_state(zone_id).await;
        zs.last_play_started_at = Some(
            Instant::now()
                .checked_sub(age)
                .expect("machine démarrée depuis moins que l'âge simulé"),
        );
        zs
    }

    /// L'erreur remontée au client pour cette zone, s'il y en a une.
    fn erreur(&mut self, zone_id: i64) -> Option<(String, bool)> {
        let mut trouvee = None;
        while let Ok(ev) = self.recu.try_recv() {
            if ev.event_type == "zone.playback_error"
                && ev.data.get("zone_id").and_then(|v| v.as_i64()) == Some(zone_id)
            {
                trouvee = Some((
                    ev.data
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    ev.data.get("fatal").and_then(|v| v.as_bool()) == Some(true),
                ));
            }
        }
        trouvee
    }
}

/// ⛔ LA RÉGRESSION À NE PAS COMMETTRE.
///
/// Une zone navigateur n'a pas de périphérique de sortie et joue pourtant
/// vraiment : son onglet tire le flux. Une garde « pas de périphérique donc
/// pas de lecture » la couperait — c'est exactement ce que `70401f2d` a
/// fait à l'annonce Last.fm, et qu'il a fallu réparer par #2657. Ici la
/// zone joue depuis cinq minutes : rien ne doit lui arriver.
#[tokio::test]
async fn la_zone_navigateur_qui_joue_vraiment_nest_jamais_touchee() {
    let mut banc = Banc::monter().await;
    let zone_id = banc
        .zone_en_lecture("Cet ordinateur", "browser", 64 * 1024)
        .await;
    let zs = banc.instantane(zone_id, Duration::from_secs(300)).await;

    assert!(
        !banc.poller.abandonner_lecture_sans_destination(&zs).await,
        "l'onglet tire le flux : cette zone JOUE, on n'y touche pas"
    );
    assert_eq!(
        banc.playback.get_state(zone_id).await.state,
        PlayState::Playing,
        "la zone navigateur doit rester en lecture"
    );
    assert_eq!(
        banc.erreur(zone_id),
        None,
        "aucune erreur ne doit être montrée à qui écoute réellement"
    );
}

/// Le démarrage d'une zone navigateur n'est pas un silence : l'onglet met
/// une seconde ou deux à tirer ses premiers octets. Pendant la grâce, on ne
/// conclut rien.
#[tokio::test]
async fn le_demarrage_dun_onglet_lent_est_laisse_tranquille() {
    let mut banc = Banc::monter().await;
    let zone_id = banc.zone_en_lecture("Cet ordinateur", "browser", 0).await;
    let zs = banc
        .instantane(zone_id, DELAI_SILENCE_ETABLI - Duration::from_secs(1))
        .await;

    assert!(
        !banc.poller.abandonner_lecture_sans_destination(&zs).await,
        "avant l'échéance, un onglet peut encore démarrer"
    );
    assert_eq!(
        banc.playback.get_state(zone_id).await.state,
        PlayState::Playing
    );
    assert_eq!(banc.erreur(zone_id), None);
}

/// Zone navigateur, personne au bout du fil, l'échéance est passée : Tune
/// arrête de prétendre. Le message dit le geste utile — ouvrir un onglet.
#[tokio::test]
async fn la_zone_navigateur_sans_onglet_cesse_detre_annoncee() {
    let mut banc = Banc::monter().await;
    let zone_id = banc.zone_en_lecture("Cet ordinateur", "browser", 0).await;
    let zs = banc.instantane(zone_id, Duration::from_secs(30)).await;

    assert!(
        banc.poller.abandonner_lecture_sans_destination(&zs).await,
        "douze secondes sans un octet : ce n'est plus un démarrage"
    );
    assert_eq!(
        banc.playback.get_state(zone_id).await.state,
        PlayState::Stopped,
        "la zone ne doit plus être annoncée « en lecture »"
    );
    let (message, fatal) = banc
        .erreur(zone_id)
        .expect("l'utilisateur doit être prévenu");
    assert!(
        message.starts_with("zone_browser_unattended:"),
        "le message doit désigner l'onglet manquant, pas un périphérique : {message}"
    );
    assert!(fatal, "rien ne se rétablira tout seul");
}

/// #6066 — par la vraie porte : l'iPhone (AVPlayer) a sondé le flux, deux
/// octets, puis l'a refusé. Rien ne joue ; la zone ne doit pas rester
/// annoncée « en lecture » pour autant.
#[tokio::test]
async fn la_zone_navigateur_seulement_sondee_cesse_detre_annoncee_6066() {
    let mut banc = Banc::monter().await;
    let zone_id = banc.zone_en_lecture("Ce téléphone", "browser", 2).await;
    let zs = banc.instantane(zone_id, Duration::from_secs(30)).await;

    assert!(
        banc.poller.abandonner_lecture_sans_destination(&zs).await,
        "#6066 : deux octets de sonde ont tenu la lecture pour reçue"
    );
    assert_eq!(
        banc.playback.get_state(zone_id).await.state,
        PlayState::Stopped
    );
    let (message, _) = banc
        .erreur(zone_id)
        .expect("l'utilisateur doit être prévenu");
    assert!(message.starts_with("zone_browser_unattended:"), "{message}");
}

/// #2588 — l'abandon ne doit pas emporter l'explication du silence.
///
/// L'arrêt fait retomber `output_reach` à `"ok"`, et le bandeau « aucun
/// onglet ne reçoit le son » disparaissait donc à l'instant même où le
/// poller le rendait vrai. Sans marque laissée derrière, l'utilisateur
/// voit la lecture cesser et n'apprend jamais pourquoi.
#[tokio::test]
async fn labandon_laisse_derriere_lui_de_quoi_expliquer_le_silence() {
    let banc = Banc::monter().await;
    let zone_id = banc.zone_en_lecture("Cet ordinateur", "browser", 0).await;
    let zs = banc.instantane(zone_id, Duration::from_secs(30)).await;
    assert!(banc.poller.abandonner_lecture_sans_destination(&zs).await);
    let etat = banc.playback.get_state(zone_id).await;
    assert_eq!(etat.state, PlayState::Stopped);
    assert!(
        etat.browser_unattended_at.is_some(),
        "la raison du silence doit survivre à l'arrêt qui la produit"
    );
}
/// Le constat est celui d'un ONGLET absent : une zone DLNA sans
/// périphérique produit le même silence pour une autre raison, et ne doit
/// pas hériter d'un message qui parle d'onglets (#2588).
#[tokio::test]
async fn labandon_dune_zone_sans_peripherique_naccuse_aucun_onglet() {
    let banc = Banc::monter().await;
    let zone_id = banc.zone_en_lecture("Salon", "dlna", 0).await;
    let zs = banc.instantane(zone_id, Duration::from_secs(30)).await;
    assert!(banc.poller.abandonner_lecture_sans_destination(&zs).await);
    assert!(
        banc.playback
            .get_state(zone_id)
            .await
            .browser_unattended_at
            .is_none(),
        "aucun onglet n'est en cause ici"
    );
}
/// La scène du ticket : zone 987, aucun périphérique, aucun onglet. Le
/// message reprend la sentinelle que le client sait déjà traduire.
#[tokio::test]
async fn la_zone_sans_peripherique_ni_onglet_ne_ment_plus() {
    let mut banc = Banc::monter().await;
    let zone_id = banc.zone_en_lecture("Salon", "dlna", 0).await;
    let zs = banc.instantane(zone_id, Duration::from_secs(30)).await;

    assert!(banc.poller.abandonner_lecture_sans_destination(&zs).await);
    assert_eq!(
        banc.playback.get_state(zone_id).await.state,
        PlayState::Stopped,
        "`output_sent=false` ne doit plus produire un état « en lecture »"
    );
    let (message, fatal) = banc
        .erreur(zone_id)
        .expect("l'utilisateur doit être prévenu");
    assert!(
        message.starts_with("zone_no_output_device:") && message.contains("Salon"),
        "le message doit nommer la zone et le geste : {message}"
    );
    assert!(fatal);
}

/// Le verdict, sans I/O. `None` n'est jamais une preuve : ni une date de
/// démarrage absente (`last_play_started_at` est `#[serde(skip)]`, il vaut
/// `None` après une restauration d'état), ni un flux inconnu du streamer.
#[test]
fn le_doute_profite_toujours_a_la_lecture() {
    use decisions::lecture_sans_destination_abandonnee as abandon;
    let vieux = DELAI_SILENCE_ETABLI + Duration::from_secs(1);

    assert!(
        abandon(Some(vieux), Some(0)),
        "vieux et muet : on abandonne"
    );
    assert!(
        abandon(Some(DELAI_SILENCE_ETABLI), Some(0)),
        "pile à l'échéance : on abandonne"
    );
    assert!(
        !abandon(
            Some(DELAI_SILENCE_ETABLI - Duration::from_millis(1)),
            Some(0)
        ),
        "une milliseconde avant l'échéance : on attend"
    );
    assert!(
        !abandon(None, Some(0)),
        "démarrage non daté : on ne conclut rien"
    );
    assert!(
        !abandon(Some(vieux), None),
        "flux inconnu du streamer : ce n'est pas une preuve de silence"
    );
    assert!(
        !abandon(Some(vieux), Some(TAILLE_MAXIMALE_D_UNE_SONDE + 1)),
        "au-delà d'une sonde, quelqu'un consomme le flux"
    );
}

/// #6066 — le cas du terrain : AVPlayer sonde `bytes=0-1` (deux octets), puis
/// refuse le flux. Rien ne joue, et la zone restait annoncée « en lecture »
/// sans fin, faute d'un zéro exact.
#[test]
fn une_sonde_de_plage_ne_fait_pas_une_destination_6066() {
    use decisions::lecture_sans_destination_abandonnee as abandon;
    let vieux = DELAI_SILENCE_ETABLI + Duration::from_secs(1);
    for sonde in [1, 2, 4096, TAILLE_MAXIMALE_D_UNE_SONDE] {
        assert!(
            abandon(Some(vieux), Some(sonde)),
            "#6066 : {sonde} octets tirés par une sonde ont suffi à tenir la lecture \
             pour reçue, alors que rien ne joue"
        );
    }
}

/// Diagnostic rc14 (#8963) : l'abandon d'une zone navigateur dit si le pont
/// a relayé quelque chose, et quel flux il a demandé en dernier. C'est ce qui
/// départage « le téléphone ne dit plus rien », « il parle sans demander le
/// flux » et « il demande un autre flux ».
#[tokio::test]
async fn labandon_dit_ce_que_le_pont_a_relaye_8963() {
    #[derive(Clone, Default)]
    struct Journal(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Journal {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
        type Writer = Journal;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Journal::default();
    let _garde = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(journal.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish(),
    );
    crate::cloud::activite_du_relais::noter_requete_relayee();
    crate::cloud::activite_du_relais::noter_demande_de_flux_relayee("feedbeef0011.wav");

    let banc = Banc::monter().await;
    let zone_id = banc.zone_en_lecture("Ce téléphone", "browser", 0).await;
    let zs = banc.instantane(zone_id, Duration::from_secs(30)).await;
    assert!(banc.poller.abandonner_lecture_sans_destination(&zs).await);

    let texte = String::from_utf8_lossy(&journal.0.lock().unwrap()).into_owned();
    let ligne = texte
        .lines()
        .find(|l| l.contains("lecture_sans_destination_abandonnee"))
        .unwrap_or_else(|| panic!("aucune ligne d'abandon : {texte}"));
    for attendu in [
        "depuis_requete_relayee_ms=Some(",
        "depuis_demande_de_flux_relayee_ms=Some(",
        "octets_servis=",
        "flux=",
        // Suite (#8963) : l'adresse remise au pont, la zone relue, le verdict.
        "adresse_remise_au_pont_ms=",
        "derniere_zone_relue_par_le_pont=",
        "websockets_par_le_pont=",
        "silence=",
    ] {
        assert!(ligne.contains(attendu), "« {attendu} » absent : {ligne}");
    }
    // Un test voisin peut noter une autre demande entre-temps : seul le
    // format court est garanti.
    assert!(
        !ligne.contains("feedbeef0011"),
        "l'identifiant entier du flux est recopié : {ligne}"
    );
}

/// Suite (#8963) : l'adresse du flux abandonné est partie vers le pont (une
/// relecture de la zone, relayée), et le téléphone ne l'a pas tirée. La
/// ligne d'abandon le dit en un mot, `silence=adresse_remise_flux_non_demande`,
/// avec la zone que le pont a relue : le défaut est alors côté application,
/// et non dans le chemin du serveur.
#[tokio::test]
async fn labandon_dit_si_l_adresse_du_flux_est_partie_vers_le_pont_8963() {
    #[derive(Clone, Default)]
    struct Journal(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Journal {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
        type Writer = Journal;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Journal::default();
    let _garde = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(journal.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish(),
    );

    let banc = Banc::monter().await;
    let zone_id = banc.zone_en_lecture("Ce téléphone", "browser", 0).await;
    let zs = banc.instantane(zone_id, Duration::from_secs(30)).await;
    let sid = zs
        .now_playing
        .as_ref()
        .and_then(|np| np.stream_id.clone())
        .expect("le banc pose un flux");
    // Le téléphone relit sa zone par le pont ; la réponse porte l'adresse.
    crate::cloud::activite_du_relais::noter_requete_relayee();
    let corps = format!(
        r#"{{"id":{zone_id},"stream_url":"http://10.0.0.2:8888/stream/{sid}.flac","stream_url_remote":"https://bridge.mozaiklabs.fr/stream/relay/srv/{sid}.flac"}}"#
    );
    crate::cloud::activite_du_relais::noter_reponse_relayee(
        "GET",
        &format!("/api/v1/zones/{zone_id}"),
        corps.as_bytes(),
    );
    assert!(banc.poller.abandonner_lecture_sans_destination(&zs).await);

    let texte = String::from_utf8_lossy(&journal.0.lock().unwrap()).into_owned();
    let ligne = texte
        .lines()
        .find(|l| l.contains("lecture_sans_destination_abandonnee"))
        .unwrap_or_else(|| panic!("aucune ligne d'abandon : {texte}"));
    assert!(
        ligne.contains("adresse_remise_flux_non_demande"),
        "le verdict ne dit pas que l'adresse est partie vers le pont : {ligne}"
    );
    assert!(
        ligne.contains("adresse_remise_au_pont_ms=Some("),
        "l'âge de l'adresse remise manque : {ligne}"
    );
}
