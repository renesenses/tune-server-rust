//! Annonces Da réellement encodées/décodées, sortie AIR réelle et vrai sondeur.
//! Aucune lecture audio ; toute commande explicite de préparation vise 127.0.0.1.
use super::*;
use crate::db::{migrations::run_migrations, sqlite::SqliteDb};
use crate::outputs::devialet_air::{self as air, ecoute::Ecoute, protocole, sortie::Ports};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};

struct Banc {
    poller: PositionPoller,
    ecoute: Arc<Ecoute>,
    annonce: protocole::Annonce,
    commandes: UdpSocket,
    id: i64,
    did: String,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
    startup: Instant,
}
impl Banc {
    async fn nouveau() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../outputs/devialet_air/vecteurs_tune.json"))
                .unwrap();
        let h = v["da_annonce"][0]["paquet_hex"].as_str().unwrap();
        let octets: Vec<_> = (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
            .collect();
        let annonce = protocole::analyser_annonce(&octets).unwrap();
        let did = air::sortie::identifiant_de_sortie(&annonce.mac_texte());
        let id = ZoneRepo::with_backend(db.clone())
            .create("AIR simulé", Some(air::TYPE_DE_SORTIE), Some(&did))
            .unwrap();
        ZoneRepo::with_backend(db.clone())
            .update_volume(id, 50.0)
            .unwrap();
        let commandes = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        commandes.set_nonblocking(true).unwrap();
        let ecoute = Ecoute::nouvelle();
        let sortie = air::DevialetAirOutput::new(
            "AIR simulé".into(),
            annonce.mac_texte(),
            ecoute.clone(),
            air::ContexteAir {
                activee: Arc::new(|| true),
                indice_source: Arc::new(|| 9),
                bornes_db: Arc::new(|| air::reglages::BornesDb::PAR_DEFAUT),
                ports: Ports {
                    annonces: 0,
                    ampli: commandes.local_addr().unwrap().port(),
                    audio_local: 0,
                    adresse_locale: Ipv4Addr::LOCALHOST,
                },
                cadence: air::emetteur::Cadence::AIR,
            },
        );
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(sortie));
        let playback = Arc::new(PlaybackManager::new());
        playback.set_volume(id, 0.5).await;
        let orch = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(crate::http::streamer::AudioStreamer::new(0)),
            Arc::new(Mutex::new(crate::streaming::ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let poller = PositionPoller::new(
            orch,
            playback,
            outputs,
            db,
            Arc::new(Mutex::new(HashMap::new())),
        );
        Self {
            poller,
            ecoute,
            annonce,
            commandes,
            id,
            did,
            poll_states: HashMap::new(),
            idle: HashMap::new(),
            startup: Instant::now() - std::time::Duration::from_secs(31),
        }
    }
    fn annoncer(&mut self, db: f64, muted: bool) {
        self.annonce.volume_demi_db = (db * 2.0) as i16;
        self.annonce.sourdine = muted;
        assert!(
            self.ecoute
                .noter(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    &protocole::annonce(&self.annonce)
                )
                .is_some()
        );
    }
    fn zone(&self) -> crate::db::zone_repo::Zone {
        ZoneRepo::with_backend(self.poller.db.clone())
            .get(self.id)
            .unwrap()
            .unwrap()
    }
    async fn observer(&self) {
        self.poller
            .relever_controles_air(&self.zone(), &self.startup)
            .await;
    }
    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &self.startup)
            .await;
    }
    fn aucune_commande(&self) {
        let mut octets = [0; 2048];
        assert!(
            matches!(self.commandes.recv_from(&mut octets),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock),
            "observer AIR ne doit envoyer aucune commande à l'ampli"
        );
    }
}

#[tokio::test]
async fn le_sondeur_adopte_la_baisse_et_la_sourdine_air_meme_a_l_arret_6912() {
    let mut b = Banc::nouveau().await;
    b.annoncer(-66.0, true);
    b.tic().await;
    let s = b.poller.playback.get_state(b.id).await;
    assert!(
        (s.volume - 0.2).abs() < 1e-9 && s.muted,
        "une annonce AIR fraîche doit faire suivre baisse et sourdine même zone arrêtée"
    );
    assert!((b.zone().volume - 20.0).abs() < 1e-9 && b.zone().muted);
    b.aucune_commande();
}

#[tokio::test]
async fn les_annonces_air_sont_suivies_en_pause_et_en_lecture_6912() {
    for etat in [PlayState::Paused, PlayState::Playing] {
        let mut b = Banc::nouveau().await;
        b.poller
            .playback
            .play(
                b.id,
                crate::playback::NowPlaying {
                    duration_ms: 600_000,
                    ..Default::default()
                },
            )
            .await;
        if etat == PlayState::Paused {
            b.poller.playback.pause(b.id).await;
        }
        b.annoncer(-66.0, true);
        b.observer().await;
        let s = b.poller.playback.get_state(b.id).await;
        assert!(
            s.volume <= 0.2 + 1e-9 && s.muted,
            "l'annonce AIR doit suivre indépendamment de l'état de transport"
        );
        b.aucune_commande();
    }
}

#[tokio::test]
async fn une_hausse_air_est_observee_sans_relever_la_consigne_6912() {
    let mut b = Banc::nouveau().await;
    b.annoncer(-24.0, true);
    b.observer().await;
    let s = b.poller.playback.get_state(b.id).await;
    assert_eq!(
        s.volume, 0.5,
        "une hausse AIR ne doit jamais devenir la consigne"
    );
    assert_eq!(
        b.zone().volume,
        50.0,
        "une hausse AIR ne doit jamais remonter la base"
    );
    let o = s
        .device_volume_observation
        .expect("la hausse de l'ampli doit être publiée séparément");
    assert!((o.volume - 0.8).abs() < 1e-9);
    assert_eq!(o.volume_db, -24.0);
    assert!(s.muted);
    b.aucune_commande();
}

#[tokio::test]
async fn disparaitre_retire_l_observation_sans_prendre_la_consigne_de_repli_6912() {
    let mut b = Banc::nouveau().await;
    b.annoncer(-24.0, false);
    b.observer().await;
    b.ecoute.arreter();
    b.observer().await;
    let s = b.poller.playback.get_state(b.id).await;
    assert!(
        s.device_volume_observation.is_none(),
        "sans annonce fraîche aucun volume voulu de repli n'est un relevé"
    );
    assert_eq!(s.volume, 0.5);
    assert_eq!(b.zone().volume, 50.0);
    b.aucune_commande();
}

#[tokio::test]
async fn une_commande_en_vol_ne_se_fait_pas_remplacer_par_l_annonce_air_6912() {
    let mut b = Banc::nouveau().await;
    b.annoncer(-66.0, true);
    let file = b.poller.orchestrator.volume_coalesceur.file(&b.did, b.id);
    file.etat.lock().await.en_vol = true;
    b.observer().await;
    let s = b.poller.playback.get_state(b.id).await;
    assert_eq!(
        s.volume, 0.5,
        "une annonce ne doit pas remplacer une commande AIR en vol"
    );
    assert!(!s.muted);
    assert_eq!(b.zone().volume, 50.0);
    b.aucune_commande();
}

#[tokio::test]
async fn une_sourdine_demandee_ignore_une_ancienne_annonce_de_non_sourdine_6912() {
    let mut b = Banc::nouveau().await;
    b.annoncer(-45.0, false);
    b.poller
        .orchestrator
        .set_mute(b.id, true, Some(&b.did))
        .await
        .unwrap();
    b.commandes.set_nonblocking(false).unwrap();
    b.commandes
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut o = [0; 2048];
    for _ in 0..2 {
        let (n, _) = b.commandes.recv_from(&mut o).unwrap();
        assert_eq!(
            protocole::analyser_commande(&o[..n]).unwrap().opcode,
            protocole::opcode::SOURDINE
        );
    }
    b.commandes.set_nonblocking(true).unwrap();
    b.observer().await;
    assert!(
        b.poller.playback.get_state(b.id).await.muted && b.zone().muted,
        "une ancienne annonce AIR ne doit pas défaire la sourdine demandée dans Tune"
    );
    b.aucune_commande();
}

#[tokio::test]
async fn une_grace_de_volume_ou_de_demarrage_garde_la_consigne_air_6912() {
    let mut b = Banc::nouveau().await;
    b.annoncer(-66.0, true);
    b.poller.playback.mark_volume_changed(b.id).await;
    b.observer().await;
    assert_eq!(b.poller.playback.get_state(b.id).await.volume, 0.5);
    b.poller.playback.ecouler_la_grace_de_volume(b.id).await;
    b.startup = Instant::now();
    b.observer().await;
    assert_eq!(b.zone().volume, 50.0);
    assert!(!b.zone().muted);
    b.startup = Instant::now() - std::time::Duration::from_secs(31);
    b.observer().await;
    assert!(b.zone().volume <= 20.0 + 1e-9 && b.zone().muted);
    b.aucune_commande();
}

#[tokio::test]
async fn une_nouvelle_consigne_en_base_ne_se_fait_pas_ecraser_par_un_releve_air_6912() {
    let b = Banc::nouveau().await;
    let ancienne = b.zone();
    let repo = ZoneRepo::with_backend(b.poller.db.clone());
    repo.update_volume(b.id, 10.0).unwrap();
    assert!(
        !repo
            .adopter_controles_air_attendus(&ancienne, 30.0, true)
            .unwrap(),
        "un relevé AIR ancien ne doit pas réécrire une nouvelle consigne en base"
    );
    assert_eq!(b.zone().volume, 10.0);
    assert!(!b.zone().muted);
}

#[tokio::test]
async fn une_consigne_memoire_plus_basse_ne_se_fait_pas_remonter_par_air_6912() {
    let b = Banc::nouveau().await;
    b.poller.playback.set_volume(b.id, 0.1).await;
    assert!(
        !b.poller
            .playback
            .adopter_controles_air(b.id, 0.5, false, 0.3, true)
            .await,
        "un relevé AIR ancien ne doit pas remonter une nouvelle consigne mémoire"
    );
    assert_eq!(b.poller.playback.get_state(b.id).await.volume, 0.1);
}

#[tokio::test]
async fn le_repli_du_statut_air_n_est_pas_adopte_comme_une_annonce_6912() {
    let b = Banc::nouveau().await;
    assert_eq!(
        b.poller.releve_adoptable(b.id, &b.did, Some(0.5), 0.2, 0.5),
        None,
        "le repli get_status AIR ne doit jamais être pris pour une annonce fraîche"
    );
    assert_eq!(
        b.poller
            .releve_adoptable(b.id, "dlna-autre", Some(0.5), 0.2, 0.5),
        Some(0.2)
    );
}
