//! Routes HTTP et vrai watcher, horloge Tokio contrôlée, sortie factice.
//! Aucune piste de service ni appareil réel : les URL sont remises au mock.
use super::*;
use axum::http::Request;
use serde_json::json;
use std::sync::atomic::{AtomicI64, Ordering};
use tower::ServiceExt;
use tune_core::playback::PlayState;

struct Banc {
    state: AppState,
    app: Router,
    zone: i64,
    device: String,
    _dir: tempfile::TempDir,
}

impl Banc {
    async fn new() -> Self {
        // Les sessions UPnP sont globales au processus : ne jamais partager
        // les ids avec les autres bases en mémoire des tests unitaires.
        static ZONE: AtomicI64 = AtomicI64::new(4_324_000);
        let zone = ZONE.fetch_add(1, Ordering::Relaxed);
        let device = format!("mock-4324-{zone}");
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("renderer.db");
        let config = crate::config::TuneConfig {
            db_path: db.to_str().unwrap().to_owned(),
            ..Default::default()
        };
        let state = AppState::new(db.to_str().unwrap(), 0, config).unwrap();
        state
            .backend
            .execute(
                "INSERT INTO zones (id,name,output_type,output_device_id) VALUES (?1,?2,?3,?4)",
                &[&zone, &"Renderer 4324", &"mock", &device],
            )
            .unwrap();
        SettingsRepo::with_backend(state.backend.clone())
            .set(&format!("zone_{zone}_upnp_renderer"), "true")
            .unwrap();
        state
            .outputs
            .lock()
            .await
            .register(Box::new(tune_core::outputs::mock::MockOutput::new(
                &device,
                "Sortie de banc",
            )));
        let app = crate::routes::router(state.clone());
        Self {
            state,
            app,
            zone,
            device,
            _dir: dir,
        }
    }

    async fn soap(&self, action: &str, args: &str) {
        let body = format!(
            r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID>{args}</u:{action}></s:Body></s:Envelope>"#
        );
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post(format!("/upnp/renderer/{}/AVTransport/control", self.zone))
                    .header("content-type", "text/xml")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "SOAP {action}: {}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(
            !String::from_utf8_lossy(&bytes).contains("<errorCode>"),
            "SOAP {action}: {}",
            String::from_utf8_lossy(&bytes)
        );
    }

    fn uri(name: &str) -> String {
        format!("http://127.0.0.1:9/{name}.mp3")
    }

    async fn current(&self, name: &str) {
        self.soap(
            "SetAVTransportURI",
            &format!(
                "<CurrentURI>{}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>",
                Self::uri(name)
            ),
        )
        .await;
    }

    async fn next(&self, name: &str) {
        self.soap(
            "SetNextAVTransportURI",
            &format!(
                "<NextURI>{}</NextURI><NextURIMetaData></NextURIMetaData>",
                Self::uri(name)
            ),
        )
        .await;
        tokio::task::yield_now().await; // inscrire le timer du vrai watcher
    }

    async fn play(&self) {
        self.soap("Play", "<Speed>1</Speed>").await;
    }

    async fn native(&self, source: &str, name: &str) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/zones/{}/play", self.zone))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "source":source, "source_id":Self::uri(name), "title":name,
                            "duration_ms":60000
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "lecture Tune: {}",
            String::from_utf8_lossy(&bytes)
        );
    }

    async fn stopped(&self) {
        // Même état que l'arrêt du poller après fin ou échec. L'orchestrateur
        // arrête effectivement la sortie et conserve le now-playing.
        self.state
            .orchestrator
            .stop(self.zone, Some(&self.device))
            .await;
    }

    async fn tick(&self) {
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        // Le watcher et l'orchestrateur n'attendent aucune sortie réelle.
        for _ in 0..40 {
            tokio::task::yield_now().await;
        }
    }

    async fn calls(&self) -> usize {
        let outputs = self.state.outputs.lock().await;
        let output = outputs.get(&self.device).unwrap();
        let output = output.lock().await;
        output
            .as_any()
            .downcast_ref::<tune_core::outputs::mock::MockOutput>()
            .unwrap()
            .play_call_count()
            .await
    }

    fn pending(&self) -> Option<String> {
        sessions()
            .lock()
            .unwrap()
            .get(&self.zone)
            .unwrap()
            .next
            .as_ref()
            .map(|n| n.uri.clone())
    }

    async fn assert_uri(&self, source: &str, name: &str) {
        let np = self
            .state
            .playback
            .get_state(self.zone)
            .await
            .now_playing
            .unwrap();
        assert_eq!(np.source, source);
        assert_eq!(np.source_id.as_deref(), Some(Self::uri(name).as_str()));
    }
}

#[tokio::test(start_paused = true)]
async fn reprise_tune_puis_arret_ne_lance_pas_l_ancienne_suivante() {
    let b = Banc::new().await;
    b.current("avant").await;
    b.play().await;
    b.next("perimee").await;
    b.tick().await;
    b.native("podcast", "tune").await;
    b.stopped().await;
    b.tick().await;
    assert_eq!(
        b.calls().await,
        2,
        "#4324 : le watcher a injecté une ancienne suivante UPnP après l'arrêt Tune"
    );
    b.assert_uri("podcast", "tune").await;
    assert!(
        b.pending().is_none(),
        "la suivante périmée doit être désarmée"
    );
}

#[tokio::test(start_paused = true)]
async fn reprise_et_arret_entre_deux_ticks_sont_detectes() {
    let b = Banc::new().await;
    b.current("avant").await;
    b.play().await;
    b.next("perimee").await;
    b.native("podcast", "tune").await;
    b.stopped().await;
    b.tick().await;
    assert_eq!(b.calls().await, 2);
    assert!(
        b.pending().is_none(),
        "une reprise rapide ne doit pas laisser un watcher armé"
    );
}

#[tokio::test(start_paused = true)]
async fn meme_uri_rejouee_par_tune_n_est_plus_la_session_renderer() {
    let b = Banc::new().await;
    b.current("identique").await;
    b.play().await;
    b.next("perimee").await;
    b.tick().await;
    b.stopped().await;
    b.native("upnp", "identique").await;
    b.stopped().await;
    b.tick().await;
    assert_eq!(
        b.calls().await,
        2,
        "la comparaison d'URI seule accepte une autre lecture du même titre"
    );
    assert!(b.pending().is_none());
}

#[tokio::test(start_paused = true)]
async fn fin_de_la_bonne_session_enchaine_exactement_une_fois() {
    let b = Banc::new().await;
    b.current("avant").await;
    b.play().await;
    b.next("apres").await;
    b.tick().await;
    b.stopped().await;
    b.tick().await;
    assert_eq!(
        b.calls().await,
        2,
        "l'enchaînement UPnP valide doit rester fonctionnel"
    );
    b.assert_uri("upnp", "apres").await;
    b.tick().await;
    b.tick().await;
    assert_eq!(b.calls().await, 2);
}

#[tokio::test(start_paused = true)]
async fn suivante_avant_play_attend_la_commande_du_renderer() {
    let b = Banc::new().await;
    b.native("podcast", "tune").await;
    b.current("avant").await;
    b.next("apres").await;
    b.tick().await;
    b.stopped().await;
    b.tick().await;
    assert_eq!(
        b.calls().await,
        1,
        "SetNext seul ne s'approprie pas une lecture Tune"
    );
    assert_eq!(b.pending(), Some(Banc::uri("apres")));
    b.play().await;
    b.tick().await;
    b.stopped().await;
    b.tick().await;
    assert_eq!(b.calls().await, 3);
    b.assert_uri("upnp", "apres").await;
}

#[tokio::test(start_paused = true)]
async fn pause_reprise_conserve_l_enchainement_sans_rejouer_la_piste() {
    let b = Banc::new().await;
    b.current("avant").await;
    b.play().await;
    b.next("apres").await;
    b.tick().await;
    b.soap("Pause", "").await;
    b.tick().await;
    assert_eq!(
        b.state.playback.get_state(b.zone).await.state,
        PlayState::Paused
    );
    b.play().await;
    assert_eq!(
        b.calls().await,
        1,
        "Play en pause doit reprendre, pas rejouer"
    );
    b.stopped().await;
    b.tick().await;
    assert_eq!(b.calls().await, 2);
    b.assert_uri("upnp", "apres").await;
}

#[tokio::test(start_paused = true)]
async fn stop_et_nouveau_contexte_ne_ressuscitent_pas_l_ancienne_suivante() {
    let b = Banc::new().await;
    b.current("ancien").await;
    b.play().await;
    b.next("perimee").await;
    b.tick().await;
    b.soap("Stop", "").await;
    b.current("nouveau").await;
    b.next("nouvelle-suite").await;
    b.tick().await;
    assert_eq!(
        b.calls().await,
        1,
        "une ancienne observation Playing ne vaut pas pour le nouveau SetURI"
    );
    b.play().await;
    b.tick().await;
    b.stopped().await;
    b.tick().await;
    assert_eq!(b.calls().await, 3);
    b.assert_uri("upnp", "nouvelle-suite").await;
}

#[tokio::test(start_paused = true)]
async fn dernier_set_next_gagne_et_le_watcher_se_rearme() {
    let b = Banc::new().await;
    b.current("a").await;
    b.play().await;
    b.next("b").await;
    b.next("c").await;
    b.tick().await;
    b.stopped().await;
    b.tick().await;
    b.assert_uri("upnp", "c").await;
    b.tick().await; // sortie du watcher sans suivante
    b.next("d").await;
    b.stopped().await;
    b.tick().await;
    b.assert_uri("upnp", "d").await;
    assert_eq!(b.calls().await, 3);
}

#[tokio::test(start_paused = true)]
async fn set_next_tardif_apres_reprise_tune_ne_rearme_pas_le_renderer() {
    let b = Banc::new().await;
    b.current("avant").await;
    b.play().await;
    b.native("podcast", "tune").await;
    b.next("perimee").await;
    b.stopped().await;
    b.tick().await;
    assert_eq!(
        b.calls().await,
        2,
        "#4324 : un SetNext tardif a repris une lecture appartenant a Tune"
    );
    b.assert_uri("podcast", "tune").await;
    assert!(b.pending().is_none());
}
