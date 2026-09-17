//! #4173 — la fin de piste prononcée à l'HORLOGE adopte l'enchaînement du
//! renderer au lieu de le relancer.
//!
//! ## La chronologie journalisée (Villerio, Eversolo DMP-A6 1.6.01, 14/09)
//!
//! ```text
//! 19:16:47.846 gapless_arm_trace zone_id=11 output="dlna" armed=true reason="in_arming_window"
//!              reported_duration_ms=237000 queue_duration_ms=237651 position_ms=207000
//! 19:16:47.866 dlna_set_next device=DMP-A6 url=".../stream/3f3eb421-….wav"
//! 19:16:50.374 stream_request stream_id="3f3eb421-…" range="bytes=0-" agent="Lavf/58.45.100"
//! 19:17:23.857 position_past_end_advancing zone_id=11 position_ms=237000 track_dur=237651
//!              wall_secs=243 past_end_ticks=3 dlna_frozen_end=true
//! 19:17:23.857 auto_next zone_id=11 next_pos=1
//! 19:17:23.857 stream_session_removed stream_id="3f3eb421-…"   <-- le flux TIRÉ, jeté
//! 19:17:23.859 file_session_created stream_id=70d9254f-…       <-- le même titre, à neuf
//! 19:17:23.932 dlna_set_uri_ok device=DMP-A6 url=".../70d9254f-….wav"
//! 19:17:23.952 dlna_play     device=DMP-A6
//! 19:17:25.296 service_fichier_termine stream_id=3f3eb421-… octets=15925248 elapsed_ms=34160
//! ```
//!
//! Le renderer tenait le flux armé depuis 34 s quand Tune l'a coupé sous lui
//! pour repartir en `SetAVTransportURI` + `Play` : 7,8 s de blanc.
//!
//! ## Le banc
//!
//! Le vrai `tick`, le vrai orchestrateur, le vrai gestionnaire de flux, un
//! renderer factice (`MockOutput` en `dlna`) qui acquitte le `SetNext` et
//! dont on pose ce qu'il rapporte (position gelée à la durée, URI courante)
//! et ce qu'il a fait (les octets tirés sur le flux armé, écrits sur la
//! session comme le ferait `tune-stream-http`). L'horloge de fin de piste
//! est injectée, jamais dormie.
//!
//! ## Les épreuves
//!
//! - `l_uri_courante_nomme_le_flux_arme_on_adopte` : le renderer dit jouer
//!   le flux armé → zéro `Play`, la session armée conservée et adoptée ;
//! - `le_flux_arme_est_tire_et_l_uri_est_muette_on_adopte` : pas d'URI, mais
//!   les octets → adoption, puis confirmée dès que la position repart ;
//! - `rien_de_tire_le_repli_relance` : le cas symétrique, le renderer n'a
//!   pas touché au flux armé → le repli d'avant, `Play` de la piste 2 ;
//! - `l_uri_courante_nomme_encore_la_piste_finie_le_repli_relance` : le
//!   renderer dit LUI-MÊME qu'il joue encore la piste finie → repli ;
//! - `adoption_sans_signe_de_vie_le_repli_relance_la_piste_adoptee` : le
//!   flux a été tiré mais le renderer reste gelé → au bout du délai, `Play`
//!   de la piste ADOPTÉE, pas de la suivante ;
//! - `pause_pendant_la_surveillance_ne_relance_rien` : l'utilisateur met en
//!   pause pendant la fenêtre → la surveillance est levée, aucun `Play`.

use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const APPAREIL: &str = "dlna:uuid:3E151150-D9C0-11F0-A7C6-800A805C2689";
/// « Speak to Me/Breathe » : la durée de la file, et celle que le renderer
/// annonce (il arrondit à la seconde).
const DUREE_FILE_MS: i64 = 237_651;
const DUREE_RENDERER_MS: u64 = 237_000;
/// La position sur laquelle le DMP-A6 se gèle : exactement sa durée.
const POSITION_GELEE_MS: u64 = 237_000;
/// L'horloge de Tune au moment de la fin prononcée (`wall_secs=243`).
const HORLOGE_A_LA_FIN_SECS: u64 = 243;
/// Ce que le renderer avait tiré du flux armé quand Tune l'a coupé.
const OCTETS_TIRES: u64 = 15_925_248;

const FINIE: &str = "Speak to Me/Breathe";
const ARMEE: &str = "On the Run";
const SUIVANTE: &str = "Time";

/// Un WAV minuscule mais réel : `resolve_stream` ouvre le fichier.
fn ecrire_wav(chemin: &std::path::Path) {
    use std::io::Write;
    let octets_data: u32 = 44_100 * 4 / 5;
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
    orchestrator: Arc<PlaybackOrchestrator>,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    zone_id: i64,
    /// Le flux de la piste qui joue (piste 1), comme `play()` l'aurait posé.
    flux_finie: String,
    _fichiers: Vec<tempfile::NamedTempFile>,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    /// Zone DLNA en lecture de `FINIE`, file `[FINIE, ARMEE, SUIVANTE]`.
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);

        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("A6", Some("dlna"), Some(APPAREIL))
            .unwrap();

        let depot = TrackRepo::with_backend(db.clone());
        let mut fichiers = Vec::new();
        let mut pistes = Vec::new();
        for (n, (titre, duree)) in [
            (FINIE, DUREE_FILE_MS),
            (ARMEE, 212_000),
            (SUIVANTE, 200_000),
        ]
        .iter()
        .enumerate()
        {
            let f = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
            ecrire_wav(f.path());
            let mut piste = crate::db::models::Track::new((*titre).to_string());
            piste.file_path = Some(f.path().to_str().unwrap().to_string());
            piste.format = Some("wav".into());
            piste.sample_rate = Some(44_100);
            piste.bit_depth = Some(16);
            piste.channels = 2;
            piste.track_number = n as i32 + 1;
            piste.duration_ms = *duree;
            pistes.push(depot.create(&piste).unwrap());
            fichiers.push(f);
        }
        PlayQueueRepo::with_backend(db.clone())
            .set_queue(zone_id, &pistes)
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "DMP-A6").with_type("dlna"),
        ));
        let playback = Arc::new(PlaybackManager::new());
        let streamer = Arc::new(AudioStreamer::new(0));
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            streamer.clone(),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let poller = PositionPoller::new(
            orchestrator.clone(),
            playback.clone(),
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(Arc::new(EventBus::new()));

        // La piste 1 joue sur SON flux, comme après un `play()` réel : c'est
        // lui que le renderer nomme quand il n'a pas enchaîné.
        let flux_finie = streamer
            .create_file_session(
                StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    ..Default::default()
                },
                fichiers[0].path().to_string_lossy().into_owned(),
                false,
            )
            .await;
        playback
            .play(
                zone_id,
                NowPlaying {
                    track_id: Some(pistes[0]),
                    title: FINIE.into(),
                    source: "local".into(),
                    duration_ms: DUREE_FILE_MS,
                    stream_id: Some(flux_finie.clone()),
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone_id, 0, 3).await;
        // La piste 1 joue depuis quatre minutes, comme dans le journal : la
        // relance de repli ne doit pas passer pour un double envoi de la
        // commande de lecture (`RETAP_DEDUP_WINDOW`).
        playback
            .dater_le_demarrage(zone_id, Duration::from_secs(HORLOGE_A_LA_FIN_SECS))
            .await;

        let generation = playback.get_state(zone_id).await.track_generation;
        let mut poll_states = HashMap::new();
        poll_states.insert(zone_id, ZonePollState::new(generation));

        Self {
            poller,
            orchestrator,
            playback,
            outputs,
            zone_id,
            flux_finie,
            _fichiers: fichiers,
            poll_states,
            idle: HashMap::new(),
        }
    }

    /// Les titres partis au renderer par `SetNextAVTransportURI`.
    async fn armees(&self) -> Vec<String> {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_next_titles().await
    }

    /// L'URL armée chez le renderer.
    async fn url_armee(&self) -> Option<String> {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.next_uri().await
    }

    /// Ce que le renderer rapporte jouer (`TrackURI`).
    async fn le_renderer_rapporte(&self, uri: Option<String>) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_current_uri(uri).await;
    }

    /// Le renderer passe en pause (geste de l'utilisateur relayé).
    async fn le_renderer_en_pause(&self) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(crate::outputs::traits::TransportState::Paused)
            .await;
    }

    /// Les `Play` complets envoyés au renderer. Un enchaînement adopté n'en
    /// produit AUCUN : dès qu'il y en a un, il y a eu un blanc.
    async fn play_complets(&self) -> Vec<String> {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.play_titles().await
    }

    /// Le renderer rapporte `position_ms` en lecture, avec sa durée
    /// arrondie ; l'horloge de Tune est datée à `horloge_secs` du début de
    /// la piste. Injection, pas attente.
    async fn renderer_a(&mut self, position_ms: u64, horloge_secs: u64) {
        {
            let reg = self.outputs.lock().await;
            let arc = reg.get(APPAREIL).unwrap();
            let sortie = arc.lock().await;
            let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
            mock.set_state(crate::outputs::traits::TransportState::Playing)
                .await;
            mock.set_duration(DUREE_RENDERER_MS);
            mock.set_position(position_ms);
        }
        let ps = self.poll_states.get_mut(&self.zone_id).unwrap();
        ps.track_started_at = Some(Instant::now() - Duration::from_secs(horloge_secs));
        if position_ms > ps.peak_position_ms {
            ps.peak_position_ms = position_ms;
        }
        ps.last_position_ms = position_ms;
    }

    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    /// 19:16:47 — la fenêtre d'armement : `SetNext` acquitté par le
    /// renderer, flux armé rangé sous la zone. Rend l'identifiant du flux
    /// armé et l'URL passée au renderer.
    async fn armer(&mut self) -> (String, String) {
        self.renderer_a(207_000, 210).await;
        self.tic().await;
        assert_eq!(
            self.armees().await,
            vec![ARMEE.to_string()],
            "la fenêtre d'armement doit avoir envoyé la piste suivante au renderer"
        );
        let url = self.url_armee().await.unwrap();
        let flux = self
            .orchestrator
            .flux_pre_arme(self.zone_id)
            .await
            .expect("l'armement par flux range une session sous la zone");
        assert!(
            url.contains(&flux),
            "l'URL armée doit porter l'identifiant du flux : {url} / {flux}"
        );
        (flux, url)
    }

    /// 19:16:50 — le renderer TIRE le flux armé : les octets servis, écrits
    /// sur la session comme `tune-stream-http` le fait.
    async fn le_renderer_tire(&self, flux: &str, octets: u64) {
        let sessions = self.orchestrator.streamer.sessions_state();
        let sessions = sessions.lock().await;
        sessions
            .get(flux)
            .expect("le flux armé existe")
            .bytes_sent
            .store(octets, std::sync::atomic::Ordering::Relaxed);
    }

    /// 19:17:16 → 19:17:23 — la fin nominale est passée, le renderer gèle sa
    /// position à sa durée et dit toujours `Playing`.
    ///
    /// #4382 — le nombre de sondages vient de la SIGNATURE, pas d'une
    /// constante unique : gelé à la durée avec `SetNext` accepté, un seul
    /// sondage conclut (`seuil_ticks_de_fin`). Ce banc en est la preuve de
    /// bout en bout : ce qui demandait trois sondages — et trois secondes de
    /// silence chez Villerio — en demande un.
    async fn la_fin_a_l_horloge(&mut self) {
        for _ in 0..crate::poller::decisions::seuil_ticks_de_fin(true, true) {
            self.renderer_a(POSITION_GELEE_MS, HORLOGE_A_LA_FIN_SECS)
                .await;
            self.tic().await;
        }
    }

    async fn ecran(&self) -> (i64, String, Option<String>) {
        let etat = self.playback.get_state(self.zone_id).await;
        let np = etat.now_playing.unwrap_or_default();
        (etat.queue_position, np.title, np.stream_id)
    }

    fn surveillance(&self) -> Option<AdoptionHorloge> {
        self.poll_states
            .get(&self.zone_id)
            .and_then(|ps| ps.adoption_horloge.clone())
    }
}

/// Ce qu'une adoption doit laisser derrière elle : zéro `Play`, l'écran sur
/// la piste armée, le flux armé conservé ET adopté par la zone.
async fn verifier_l_adoption(banc: &Banc, flux: &str, preuve: decisions::EnchainementArme) {
    assert_eq!(
        banc.play_complets().await,
        Vec::<String>::new(),
        "aucun `SetAVTransportURI` + `Play` ne doit partir : le renderer joue déjà le flux armé"
    );
    let (position, titre, stream_id) = banc.ecran().await;
    assert_eq!(position, 1, "l'écran doit pointer la piste armée");
    assert_eq!(titre, ARMEE, "l'écran doit nommer la piste armée");
    assert_eq!(
        stream_id.as_deref(),
        Some(flux),
        "la zone doit avoir ADOPTÉ le flux armé (#3442), pas une session neuve"
    );
    assert!(
        banc.orchestrator.stream_session_alive(flux).await,
        "la session pré-armée doit être conservée : c'est elle que le renderer tire"
    );
    let surveillance = banc
        .surveillance()
        .expect("une adoption à l'horloge est surveillée jusqu'au signe de vie");
    assert_eq!(surveillance.preuve, preuve);
    assert_eq!(surveillance.flux, flux);
    assert_eq!(surveillance.position_figee_ms, POSITION_GELEE_MS);
}

/// Ce qu'un repli doit laisser derrière lui : le `Play` de la piste 2, le
/// comportement d'avant au mot près.
async fn verifier_le_repli(banc: &Banc, flux: &str) {
    assert_eq!(
        banc.play_complets().await,
        vec![ARMEE.to_string()],
        "sans enchaînement attesté, le repli doit relancer la piste suivante"
    );
    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));
    assert!(
        !banc.orchestrator.stream_session_alive(flux).await,
        "le repli ouvre une session neuve et libère le flux armé, comme avant"
    );
    assert!(
        banc.surveillance().is_none(),
        "un repli n'a rien adopté : rien à surveiller"
    );
}

/// **Le fait de base, preuve la plus sûre.** Le renderer rapporte comme URI
/// courante celle du flux armé : il a enchaîné. La fin à l'horloge doit
/// l'adopter — zéro `Play`, session conservée.
#[tokio::test]
async fn l_uri_courante_nomme_le_flux_arme_on_adopte() {
    let mut banc = Banc::monter().await;
    let (flux, url) = banc.armer().await;
    banc.le_renderer_tire(&flux, OCTETS_TIRES).await;
    banc.le_renderer_rapporte(Some(url)).await;

    banc.la_fin_a_l_horloge().await;

    verifier_l_adoption(&banc, &flux, decisions::EnchainementArme::Certain).await;
}

/// **La chronologie du journal.** Le renderer ne rapporte pas d'URI, mais le
/// serveur de flux a vu 15,9 Mo partir sur le flux armé : l'enchaînement est
/// probable, on adopte. Puis la position repart : l'adoption est confirmée,
/// toujours sans le moindre `Play`.
#[tokio::test]
async fn le_flux_arme_est_tire_et_l_uri_est_muette_on_adopte() {
    let mut banc = Banc::monter().await;
    let (flux, _) = banc.armer().await;
    banc.le_renderer_tire(&flux, OCTETS_TIRES).await;
    banc.le_renderer_rapporte(None).await;

    banc.la_fin_a_l_horloge().await;

    verifier_l_adoption(&banc, &flux, decisions::EnchainementArme::Probable).await;

    // Le signe de vie : la position repart sur la piste adoptée.
    banc.renderer_a(3_000, 3).await;
    banc.tic().await;
    assert!(
        banc.surveillance().is_none(),
        "la position qui repart confirme l'adoption : la surveillance cesse"
    );
    assert_eq!(banc.play_complets().await, Vec::<String>::new());
    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));
}

/// **Le cas symétrique.** Le renderer a acquitté le `SetNext` mais n'a pas
/// tiré un octet du flux armé, et ne dit rien de son URI : rien n'atteste
/// l'enchaînement, le repli d'avant relance la piste 2.
#[tokio::test]
async fn rien_de_tire_le_repli_relance() {
    let mut banc = Banc::monter().await;
    let (flux, _) = banc.armer().await;
    banc.le_renderer_rapporte(None).await;

    banc.la_fin_a_l_horloge().await;

    verifier_le_repli(&banc, &flux).await;
}

/// Le renderer dit LUI-MÊME qu'il joue encore la piste finie (25/08 :
/// « SetNext acquitté jamais honoré ») — même s'il a préchargé le flux armé.
/// On le croit : repli.
#[tokio::test]
async fn l_uri_courante_nomme_encore_la_piste_finie_le_repli_relance() {
    let mut banc = Banc::monter().await;
    let (flux, _) = banc.armer().await;
    banc.le_renderer_tire(&flux, OCTETS_TIRES).await;
    let uri_finie = format!("http://192.168.1.196:8888/stream/{}.wav", banc.flux_finie);
    banc.le_renderer_rapporte(Some(uri_finie)).await;

    banc.la_fin_a_l_horloge().await;

    verifier_le_repli(&banc, &flux).await;
}

/// L'adoption probable ne tient pas sans signe de vie : le renderer a tiré
/// le flux mais reste gelé. Passé le délai, la piste ADOPTÉE est relancée —
/// pas la suivante : la file ne saute aucun titre.
#[tokio::test]
async fn adoption_sans_signe_de_vie_le_repli_relance_la_piste_adoptee() {
    let mut banc = Banc::monter().await;
    let (flux, _) = banc.armer().await;
    banc.le_renderer_tire(&flux, OCTETS_TIRES).await;
    banc.le_renderer_rapporte(None).await;

    banc.la_fin_a_l_horloge().await;
    verifier_l_adoption(&banc, &flux, decisions::EnchainementArme::Probable).await;

    // Le renderer reste gelé, un tick dans le délai : on attend encore.
    banc.renderer_a(POSITION_GELEE_MS, 1).await;
    banc.tic().await;
    assert!(
        banc.surveillance().is_some(),
        "dans le délai, on attend encore"
    );
    assert_eq!(banc.play_complets().await, Vec::<String>::new());

    // Le délai est écoulé (horloge injectée), le renderer toujours gelé.
    banc.poll_states
        .get_mut(&banc.zone_id)
        .unwrap()
        .adoption_horloge
        .as_mut()
        .unwrap()
        .depuis = Instant::now() - Duration::from_secs(ADOPTION_HORLOGE_DELAI_SECS + 1);
    banc.renderer_a(POSITION_GELEE_MS, ADOPTION_HORLOGE_DELAI_SECS + 1)
        .await;
    banc.tic().await;

    assert_eq!(
        banc.play_complets().await,
        vec![ARMEE.to_string()],
        "sans signe de vie, le repli relance la piste ADOPTÉE"
    );
    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (1, ARMEE),
        "la file ne doit pas avoir sauté un titre"
    );
    assert!(
        !banc.poll_states.contains_key(&banc.zone_id),
        "la relance repart d'un état de sondage neuf"
    );
}

/// L'utilisateur met en pause pendant la fenêtre : la surveillance est
/// levée et rien n'est relancé contre lui.
#[tokio::test]
async fn pause_pendant_la_surveillance_ne_relance_rien() {
    let mut banc = Banc::monter().await;
    let (flux, _) = banc.armer().await;
    banc.le_renderer_tire(&flux, OCTETS_TIRES).await;
    banc.le_renderer_rapporte(None).await;
    banc.la_fin_a_l_horloge().await;
    verifier_l_adoption(&banc, &flux, decisions::EnchainementArme::Probable).await;

    banc.playback.pause(banc.zone_id).await;
    banc.poll_states
        .get_mut(&banc.zone_id)
        .unwrap()
        .adoption_horloge
        .as_mut()
        .unwrap()
        .depuis = Instant::now() - Duration::from_secs(ADOPTION_HORLOGE_DELAI_SECS + 1);
    banc.le_renderer_en_pause().await;
    banc.tic().await;

    assert!(
        banc.surveillance().is_none(),
        "la pause lève la surveillance"
    );
    assert_eq!(
        banc.play_complets().await,
        Vec::<String>::new(),
        "rien ne doit être relancé contre une pause de l'utilisateur"
    );
}

/// Les verdicts purs, un par ligne du tableau de
/// `decisions::enchainement_sur_le_flux_arme`.
#[test]
fn les_verdicts_purs() {
    use decisions::{EnchainementArme, enchainement_sur_le_flux_arme};
    let arme = "3f3eb421";
    let uri_armee = "http://192.168.1.196:8888/stream/3f3eb421.wav";
    let uri_finie = "http://192.168.1.196:8888/stream/aaaa1111.wav";
    assert_eq!(
        enchainement_sur_le_flux_arme(true, true, Some(arme), Some(uri_armee), Some(0)),
        EnchainementArme::Certain,
        "l'URI courante qui porte le flux armé vaut preuve, octets ou pas"
    );
    assert_eq!(
        enchainement_sur_le_flux_arme(true, true, Some(arme), None, Some(15_925_248)),
        EnchainementArme::Probable
    );
    assert_eq!(
        enchainement_sur_le_flux_arme(true, true, Some(arme), Some(""), Some(15_925_248)),
        EnchainementArme::Probable,
        "une URI vide ne dit rien : les octets parlent"
    );
    assert_eq!(
        enchainement_sur_le_flux_arme(true, true, Some(arme), None, Some(0)),
        EnchainementArme::Aucun
    );
    assert_eq!(
        enchainement_sur_le_flux_arme(true, true, Some(arme), None, None),
        EnchainementArme::Aucun,
        "une session disparue n'atteste rien"
    );
    assert_eq!(
        enchainement_sur_le_flux_arme(true, true, Some(arme), Some(uri_finie), Some(15_925_248)),
        EnchainementArme::Aucun,
        "le renderer qui nomme encore la piste finie est cru sur parole"
    );
    assert_eq!(
        enchainement_sur_le_flux_arme(false, true, Some(arme), Some(uri_armee), Some(1)),
        EnchainementArme::Aucun,
        "hors DLNA, rien ne change"
    );
    assert_eq!(
        enchainement_sur_le_flux_arme(true, false, Some(arme), Some(uri_armee), Some(1)),
        EnchainementArme::Aucun,
        "sans SetNext acquitté, rien à adopter"
    );
    assert_eq!(
        enchainement_sur_le_flux_arme(true, true, None, Some(uri_armee), Some(1)),
        EnchainementArme::Aucun,
        "sans flux armé, rien à adopter"
    );
}

/// La surveillance pure : signe de vie, attente, délai écoulé.
#[test]
fn la_surveillance_pure() {
    use decisions::{SuiteAdoption, suite_de_l_adoption};
    let flux = "3f3eb421";
    assert_eq!(
        suite_de_l_adoption(237_000, 237_000, None, flux, 3, 8),
        SuiteAdoption::EnAttente
    );
    assert_eq!(
        suite_de_l_adoption(237_400, 237_000, None, flux, 3, 8),
        SuiteAdoption::EnAttente,
        "un tremblement de quelques centaines de ms n'est pas un signe de vie"
    );
    assert_eq!(
        suite_de_l_adoption(2_000, 237_000, None, flux, 1, 8),
        SuiteAdoption::Confirmee,
        "la position qui repart confirme"
    );
    assert_eq!(
        suite_de_l_adoption(
            237_000,
            237_000,
            Some("http://h/stream/3f3eb421.wav"),
            flux,
            1,
            8
        ),
        SuiteAdoption::Confirmee,
        "l'URI qui nomme le flux adopté confirme, position gelée ou pas"
    );
    assert_eq!(
        suite_de_l_adoption(237_000, 237_000, None, flux, 8, 8),
        SuiteAdoption::Infirmee
    );
}
