//! Orchestrateur et sondeur réels, fichier FLAC 24/96 local et contrôle TCP
//! HQPlayer factice. La porte remplace le délai réseau sans sleep ni retry.
use super::*;
use crate::TuneError;
use crate::streaming::traits::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

struct TidalLent {
    url: String,
    appels: Arc<AtomicUsize>,
    entree: Arc<Notify>,
    liberation: Arc<Notify>,
    refus: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl StreamingService for TidalLent {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "tidal"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Ok(AuthStatus::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::default()
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err("la ligne de file porte les métadonnées".into())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        self.appels
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entree.notify_one();
        self.liberation.notified().await;
        if self.refus.swap(false, Ordering::SeqCst) {
            return Err("refus temporaire simulé".into());
        }
        Ok(StreamUrl {
            url: self.url.clone(),
            mime_type: "audio/flac".into(),
            quality: StreamQuality {
                codec: "flac".into(),
                sample_rate: 96_000,
                bit_depth: 24,
                bitrate: None,
                channels: 2,
            },
            expires_at: None,
            headers: vec![],
        })
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
}

struct Portes {
    appels: Arc<AtomicUsize>,
    entree: Arc<Notify>,
    liberation: Arc<Notify>,
    refus: Arc<AtomicBool>,
}

async fn tidal_lent() -> (Banc, Portes) {
    let mut banc = Banc::monter().await;
    let fichier = tempfile::Builder::new().suffix(".flac").tempfile().unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/flac/ref_24_96000_stereo.flac"
        ),
        fichier.path(),
    )
    .unwrap();
    let portes = Portes {
        appels: Arc::new(AtomicUsize::new(0)),
        entree: Arc::new(Notify::new()),
        liberation: Arc::new(Notify::new()),
        refus: Arc::new(AtomicBool::new(false)),
    };
    banc.poller
        .orchestrator
        .services
        .lock()
        .await
        .register(Box::new(TidalLent {
            url: format!("file://{}", fichier.path().display()),
            appels: portes.appels.clone(),
            entree: portes.entree.clone(),
            liberation: portes.liberation.clone(),
            refus: portes.refus.clone(),
        }));
    banc._fichiers.push(fichier);
    PlayQueueRepo::with_backend(banc.db.clone())
        .set_streaming_queue(
            banc.zone_id,
            &[
                (
                    "courante".into(),
                    COURANTE.into(),
                    "artiste".into(),
                    None,
                    None,
                    DUREE_COURANTE_MS,
                    Some("tidal".into()),
                    None,
                    None,
                ),
                (
                    "suivante".into(),
                    ARMEE.into(),
                    "artiste".into(),
                    None,
                    None,
                    DUREE_SUIVANTE_MS,
                    Some("tidal".into()),
                    None,
                    None,
                ),
            ],
        )
        .unwrap();
    banc.playback.update_queue_info(banc.zone_id, 0, 2).await;
    banc.a(10_000); // bien AVANT la fenêtre des trente dernières secondes.
    (banc, portes)
}

async fn premier_tic(banc: &mut Banc, portes: &Portes) {
    tokio::time::timeout(Duration::from_secs(2), banc.tic())
        .await
        .expect("une préparation fichier lente ne doit pas bloquer le sondeur");
    tokio::time::timeout(Duration::from_secs(2), portes.entree.notified())
        .await
        .expect("la préparation Tidal doit commencer avant la fenêtre gapless");
    assert_eq!(portes.appels.load(Ordering::SeqCst), 1);
    assert!(
        banc.ajouts().is_empty(),
        "rien ne doit partir avant le fichier complet"
    );
}

#[tokio::test]
async fn le_fichier_lent_survit_au_budget_et_part_une_fois_en_queued_7247() {
    let (mut banc, portes) = tidal_lent().await;
    premier_tic(&mut banc, &portes).await;
    let fin = banc
        .poller
        .orchestrator
        .fin_preparation_fichier(banc.zone_id);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(12)).await;
    tokio::time::resume();
    portes.liberation.notify_one();
    attendre_la_fin(fin).await;
    let _sessions_jetables = fichiers_du_banc(&banc).await;
    let resolution = tokio::time::timeout(
        Duration::from_secs(10),
        banc.poller
            .orchestrator
            .resolve_queue_item_url(banc.zone_id, 1),
    )
    .await;
    assert_eq!(
        portes.appels.load(Ordering::SeqCst),
        1,
        "la résolution gapless doit réutiliser la préparation sans demander une seconde fois la piste au service"
    );
    let resolu = resolution
        .expect("la préparation doit survivre au budget de dix secondes")
        .unwrap();
    assert!(
        resolu.file_size.is_some_and(|n| n > 0),
        "HQPlayer doit recevoir un fichier complet de taille connue"
    );
    let sid = resolu.stream_id.as_ref().expect("session fichier");
    assert!(banc.poller.orchestrator.stream_session_alive(sid).await);
    banc.tic().await;
    let ajouts = banc.ajouts();
    assert_eq!(
        ajouts.len(),
        1,
        "la suivante prête est posée avant les trente dernières secondes"
    );
    assert!(ajouts[0].contains("clear=\"0\""));
    assert!(
        banc.relances().is_empty(),
        "préparer ne doit ni arrêter ni relancer la courante"
    );
    banc.tic().await;
    assert_eq!(banc.ajouts().len(), 1);
    assert_eq!(
        portes.appels.load(Ordering::SeqCst),
        1,
        "une seule résolution/téléchargement de la suivante"
    );
    // Le repli de fin naturelle partage aussi ce résultat : pas de seconde
    // résolution quand la préparation s'est achevée après la fin du morceau.
    banc.poller
        .orchestrator
        .play_from_queue(banc.zone_id, 1)
        .await
        .unwrap();
    assert_eq!(
        portes.appels.load(Ordering::SeqCst),
        1,
        "la fin naturelle doit réutiliser le fichier préparé"
    );
}

#[tokio::test]
async fn un_stop_annule_la_preparation_sans_playlist_tardive_7247() {
    let (mut banc, portes) = tidal_lent().await;
    demarrer_directement(&banc, &portes).await;
    let fin = banc
        .poller
        .orchestrator
        .fin_preparation_fichier(banc.zone_id);
    banc.poller
        .orchestrator
        .stop(banc.zone_id, Some(&banc.faux.device_id()))
        .await;
    portes.liberation.notify_one();
    attendre_la_fin(fin).await;
    let _sessions_jetables = fichiers_du_banc(&banc).await;
    assert!(
        banc.poller
            .orchestrator
            .streamer
            .sessions_state()
            .lock()
            .await
            .is_empty(),
        "la préparation invalidée doit nettoyer sa session fichier"
    );
    assert!(
        !banc
            .poller
            .orchestrator
            .fichier_suivant_en_preparation(banc.zone_id)
    );
    banc.tic().await;
    assert!(
        banc.ajouts().is_empty(),
        "Stop interdit tout armement tardif"
    );
    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Stopped
    );
}

#[tokio::test]
async fn une_file_remplacee_n_arme_jamais_l_ancienne_suivante_7247() {
    let (mut banc, portes) = tidal_lent().await;
    demarrer_directement(&banc, &portes).await;
    let fin = banc
        .poller
        .orchestrator
        .fin_preparation_fichier(banc.zone_id);
    PlayQueueRepo::with_backend(banc.db.clone())
        .set_queue(banc.zone_id, &banc.pistes[..2])
        .unwrap();
    banc.tic().await;
    portes.liberation.notify_one();
    attendre_la_fin(fin).await;
    let _sessions_jetables = fichiers_du_banc(&banc).await;
    assert!(
        banc.poller
            .orchestrator
            .streamer
            .sessions_state()
            .lock()
            .await
            .is_empty(),
        "une ligne supprimée ne doit laisser aucune session préparée"
    );
    assert!(
        !banc
            .poller
            .orchestrator
            .fichier_suivant_en_preparation(banc.zone_id)
    );
    assert!(
        banc.ajouts().is_empty(),
        "l'ancienne ligne Tidal supprimée ne doit jamais être armée"
    );
}

#[tokio::test]
async fn un_dash_deja_precharge_n_est_pas_telecharge_apres_rejet_du_pcm_7247() {
    let (banc, portes) = tidal_lent().await;
    let depot = ZoneRepo::with_backend(banc.db.clone());
    // Le téléchargement peut précéder l'activation du gapless / du contrat
    // fichier. Sa réutilisation ne doit pas dépendre de cette chronologie.
    depot.update_gapless_enabled(banc.zone_id, false).unwrap();
    let orch = banc.poller.orchestrator.clone();
    let zone_id = banc.zone_id;
    let prechargement = tokio::spawn(async move {
        orch.prefetch
            .prefetch_next(
                orch.db.clone(),
                orch.services.clone(),
                orch.playback.clone(),
                zone_id,
            )
            .await;
    });
    tokio::time::timeout(Duration::from_secs(2), portes.entree.notified())
        .await
        .unwrap();
    portes.liberation.notify_one();
    tokio::time::timeout(Duration::from_secs(10), prechargement)
        .await
        .unwrap()
        .unwrap();
    assert!(
        banc.poller
            .orchestrator
            .prefetch
            .has_prefetched("tidal", "suivante")
            .await
    );
    depot.update_gapless_enabled(banc.zone_id, true).unwrap();
    let resolu = tokio::time::timeout(
        Duration::from_secs(3),
        banc.poller
            .orchestrator
            .resolve_queue_item_url(banc.zone_id, 1),
    )
    .await;
    let _sessions_jetables = fichiers_du_banc(&banc).await;
    assert_eq!(
        portes.appels.load(Ordering::SeqCst),
        1,
        "un DASH complet déjà préchargé ne doit jamais être téléchargé à nouveau après rejet du PCM tronqué"
    );
    assert!(
        resolu
            .expect("la résolution doit réutiliser immédiatement le DASH complet")
            .unwrap()
            .file_size
            .is_some_and(|n| n > 0)
    );
}

async fn attendre_la_fin(mut fin: tokio::sync::watch::Receiver<bool>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !*fin.borrow() {
            fin.changed().await.expect("fin de la tâche signalée");
        }
    })
    .await
    .expect("la préparation invalidée doit terminer et nettoyer ses fichiers");
}

#[tokio::test]
async fn un_echec_de_preparation_ne_poisonne_pas_le_lancement_suivant_7247() {
    let (mut banc, portes) = tidal_lent().await;
    portes.refus.store(true, Ordering::SeqCst);
    demarrer_directement(&banc, &portes).await;
    let fin = banc
        .poller
        .orchestrator
        .fin_preparation_fichier(banc.zone_id);
    portes.liberation.notify_one();
    attendre_la_fin(fin).await;
    tokio::time::timeout(Duration::from_secs(2), banc.tic())
        .await
        .expect("un échec anticipé ne doit pas lancer de nouvelle requête dans le sondeur");
    assert_eq!(portes.appels.load(Ordering::SeqCst), 1);
    portes.liberation.notify_one();
    let resultat = tokio::time::timeout(
        Duration::from_secs(10),
        banc.poller.orchestrator.play_from_queue(banc.zone_id, 1),
    )
    .await
    .expect("le lancement explicite doit reprendre après un échec de préparation");
    let _sessions_jetables = fichiers_du_banc(&banc).await;
    assert!(
        resultat.as_ref().is_ok_and(|r| r.output_sent),
        "un échec de préparation ne doit jamais empoisonner une lecture ultérieure valide : {:?}",
        resultat.as_ref().map(|r| &r.error)
    );
    assert_eq!(portes.appels.load(Ordering::SeqCst), 2);
}

async fn fichiers_du_banc(banc: &Banc) -> Vec<tempfile::TempPath> {
    let registre = banc.poller.orchestrator.streamer.sessions_state();
    let sessions: Vec<_> = registre.lock().await.values().cloned().collect();
    let mut fichiers = Vec::new();
    for session in sessions {
        if let Some(path) = session.file_path.lock().await.clone()
            && std::path::Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("tune-dash-transcode-"))
        {
            fichiers.push(
                tempfile::TempPath::try_from_path(path)
                    .expect("chemin absolu du fichier produit par le résolveur"),
            );
        }
    }
    fichiers
}

async fn demarrer_directement(banc: &Banc, portes: &Portes) {
    let etat = banc.playback.get_state(banc.zone_id).await;
    banc.poller
        .orchestrator
        .maintenir_preparation_fichier(&etat)
        .await;
    tokio::time::timeout(Duration::from_secs(2), portes.entree.notified())
        .await
        .expect("la tâche doit avoir atteint le service simulé");
    assert_eq!(portes.appels.load(Ordering::SeqCst), 1);
}

async fn terminer_preparation(banc: &Banc, portes: &Portes) -> Vec<tempfile::TempPath> {
    let fin = banc
        .poller
        .orchestrator
        .fin_preparation_fichier(banc.zone_id);
    portes.liberation.notify_one();
    attendre_la_fin(fin).await;
    fichiers_du_banc(banc).await
}

#[tokio::test]
async fn une_suivante_prete_part_en_queued_sans_relancer_la_courante_7247() {
    let (mut banc, portes) = tidal_lent().await;
    demarrer_directement(&banc, &portes).await;
    let _sessions_jetables = terminer_preparation(&banc, &portes).await;
    banc.tic().await;
    let ajouts = banc.ajouts();
    assert_eq!(ajouts.len(), 1);
    assert!(
        ajouts[0].contains("queued=\"1\""),
        "la suivante doit être ajoutée en queued=1"
    );
    assert!(ajouts[0].contains("clear=\"0\""));
    assert!(banc.relances().is_empty());
}

#[tokio::test]
async fn le_fichier_prepare_survit_au_ramassage_des_sessions_inactives_7247() {
    let (banc, portes) = tidal_lent().await;
    demarrer_directement(&banc, &portes).await;
    let _sessions_jetables = terminer_preparation(&banc, &portes).await;
    let registre = banc.poller.orchestrator.streamer.sessions_state();
    let ids: Vec<_> = registre.lock().await.keys().cloned().collect();
    assert_eq!(ids.len(), 1, "une session fichier prête existe");
    banc.poller
        .orchestrator
        .streamer
        .cleanup_stale_sessions_with(Duration::ZERO, Duration::from_secs(3600))
        .await;
    assert!(
        banc.poller.orchestrator.stream_session_alive(&ids[0]).await,
        "le fichier prêt doit rester disponible malgré son inactivité avant l'enchaînement"
    );
}

#[tokio::test]
async fn une_preparation_ne_reserve_pas_le_registre_des_autres_services_7247() {
    let (banc, portes) = tidal_lent().await;
    demarrer_directement(&banc, &portes).await;
    assert!(
        banc.poller.orchestrator.services.try_lock().is_ok(),
        "la préparation d'une zone ne doit pas réserver le registre des autres services"
    );
    let _sessions_jetables = terminer_preparation(&banc, &portes).await;
}

#[tokio::test]
async fn une_preparation_en_cours_ne_retient_pas_le_sondeur_7247() {
    let (mut banc, portes) = tidal_lent().await;
    demarrer_directement(&banc, &portes).await;
    tokio::time::timeout(Duration::from_secs(2), banc.tic())
        .await
        .expect("une préparation fichier lente ne doit pas bloquer le sondeur");
    let _sessions_jetables = terminer_preparation(&banc, &portes).await;
    assert!(
        banc.ajouts().is_empty(),
        "le fichier encore en préparation ne doit pas être armé pendant ce tick"
    );
}
