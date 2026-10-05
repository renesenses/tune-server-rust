//! #5770 — « Précédent » deux fois, hors aléatoire, par la ROUTE.
//!
//! FabienM (rc2, fil 2143 point 1) : « un clic sur le bouton précédent rejoue
//! la piste, très bien, mais un 2e clic devrait jouer la piste précédente ».
//! Le correctif de #1929 (1e5eec06) n'était éprouvé que par la fonction pure
//! [`super::precedent_doit_relancer`] : rien ne vérifiait que le handler pose
//! bien la marque du redémarrage, ni que la position relue au second appui ne
//! la défait pas. Ces tests passent par `POST /api/v1/zones/{id}/previous`,
//! le chemin qu'emprunte le bouton de la barre de transport du client web
//! (`skipPrevious` → `previousAndSync` → `api.previous`, aucune décision
//! locale côté client).
//!
//! File A, B, C ; lecture sur C à 10 s, sur une sortie enregistrée.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tower::ServiceExt;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::NowPlaying;

use super::{DERNIER_REDEMARRAGE, FENETRE_DOUBLE_PRECEDENT};

/// `DERNIER_REDEMARRAGE` est global et indexé par zone : chaque test prend
/// un identifiant de zone à lui, loin de ceux qu'une base neuve numérote.
static ZONE: AtomicI64 = AtomicI64::new(5_770_000);

struct Banc {
    app: axum::Router,
    state: crate::state::AppState,
    zone_id: i64,
    appareil: String,
}

async fn banc(type_de_sortie: &str) -> Banc {
    let zone_id = ZONE.fetch_add(1, Ordering::Relaxed);
    let appareil = format!("sortie-5770-{zone_id}");
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute(
            "INSERT INTO zones (id,name,output_type,output_device_id) VALUES (?1,?2,?3,?4)",
            &[&zone_id, &"Salon", &type_de_sortie, &appareil],
        )
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(&appareil, "Salon").with_type(type_de_sortie),
    ));
    let items: Vec<QueueInput> = ["a", "b", "c"]
        .iter()
        .map(|id| QueueInput::Streaming {
            source: "qobuz".into(),
            source_id: (*id).into(),
            title: (*id).to_string(),
            artist: "Fabien".into(),
            album: None,
            duration_ms: 197_000,
            cover_url: None,
            track_number: None,
            disc_number: None,
            album_ref: None,
        })
        .collect();
    PlayQueueRepo::with_backend(state.backend.clone())
        .append(zone_id, &items)
        .unwrap();
    state
        .playback
        .play(
            zone_id,
            NowPlaying {
                title: "c".into(),
                source: "qobuz".into(),
                source_id: Some("c".into()),
                duration_ms: 197_000,
                ..Default::default()
            },
        )
        .await;
    state.playback.update_queue_info(zone_id, 2, 3).await;
    // Dix secondes dans la piste, telles que le sondeur les a relevées.
    state.playback.update_position(zone_id, 10_000).await;
    let app = crate::routes::router(state.clone());
    Banc {
        app,
        state,
        zone_id,
        appareil,
    }
}

impl Banc {
    async fn precedent(&self) -> Value {
        let requete = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/zones/{}/previous", self.zone_id))
            .body(Body::empty())
            .unwrap();
        let reponse = self.app.clone().oneshot(requete).await.unwrap();
        let statut = reponse.status();
        let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);
        assert_eq!(statut, StatusCode::OK, "POST /previous : {v}");
        v
    }

    async fn seeks_recus(&self) -> Vec<u64> {
        let registre = self.state.outputs.lock().await;
        let arc = registre.get(&self.appareil).expect("sortie enregistrée");
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .expect("MockOutput")
            .seek_calls()
    }

    /// Vieillit la marque du dernier redémarrage, comme si `age` s'était
    /// écoulé depuis le premier appui.
    fn vieillir_la_marque(&self, age: Duration) {
        let mut m = DERNIER_REDEMARRAGE.lock().unwrap();
        let t = m.get_mut(&self.zone_id).expect("marque posée au 1er appui");
        *t = Instant::now()
            .checked_sub(age)
            .expect("horloge assez avancée");
    }
}

async fn deux_appuis_rapproches_reculent(type_de_sortie: &str) {
    let b = banc(type_de_sortie).await;

    let v = b.precedent().await;
    assert_eq!(v["status"], "restarted", "1er appui à 10 s : relance : {v}");
    assert_eq!(b.seeks_recus().await, vec![0], "la sortie a reçu Seek(0)");
    assert_eq!(
        b.state.playback.get_state(b.zone_id).await.position_ms,
        0,
        "après la relance, la position publique est 0, pas la position d'avant"
    );

    let v = b.precedent().await;
    assert_eq!(v["status"], "playing", "2e appui sous 6 s : recule : {v}");
    assert_eq!(v["queue_position"], 1, "B, la piste d'avant C : {v}");
    assert_eq!(b.seeks_recus().await, vec![0], "aucun second Seek(0)");
}

#[tokio::test]
async fn deux_appuis_rapproches_reculent_sur_une_sortie_locale() {
    deux_appuis_rapproches_reculent("mock").await;
}

#[tokio::test]
async fn deux_appuis_rapproches_reculent_sur_une_sortie_dlna() {
    // La zone 6 de Fabien (Devialet, DLNA) : la sortie réseau est celle dont
    // la position reste en retard après un saut.
    deux_appuis_rapproches_reculent("dlna").await;
}

#[tokio::test]
async fn une_position_perimee_au_second_appui_ne_defait_pas_la_marque() {
    // L'hypothèse « position périmée » : entre les deux appuis, le renderer
    // rapporte encore la position d'avant la relance (tampon DLNA). Le second
    // appui doit reculer quand même.
    let b = banc("dlna").await;
    assert_eq!(b.precedent().await["status"], "restarted");
    b.state.playback.update_position(b.zone_id, 10_400).await;
    assert!(b.state.playback.get_state(b.zone_id).await.position_ms > 3_000);

    let v = b.precedent().await;
    assert_eq!(v["status"], "playing", "{v}");
    assert_eq!(v["queue_position"], 1, "{v}");
}

#[tokio::test]
async fn un_second_appui_hors_fenetre_relance_de_nouveau() {
    // La règle, pas un défaut : passé six secondes d'écoute, un appui isolé
    // relance la piste (convention de tous les lecteurs).
    let b = banc("mock").await;
    assert_eq!(b.precedent().await["status"], "restarted");
    b.vieillir_la_marque(FENETRE_DOUBLE_PRECEDENT + Duration::from_millis(500));
    // Le sondeur a relevé la vraie position depuis : 6,5 s dans la piste.
    b.state.playback.update_position(b.zone_id, 6_500).await;

    let v = b.precedent().await;
    assert_eq!(v["status"], "restarted", "{v}");
    assert_eq!(b.seeks_recus().await, vec![0, 0]);
}

#[tokio::test]
async fn un_troisieme_appui_rapproche_relance_de_nouveau() {
    // La marque est consommée par le recul : sur B, 10 s plus loin, le
    // troisième appui relance B au lieu de remonter à A.
    let b = banc("mock").await;
    assert_eq!(b.precedent().await["status"], "restarted");
    assert_eq!(b.precedent().await["status"], "playing");
    b.state.playback.update_queue_info(b.zone_id, 1, 3).await;
    b.state.playback.update_position(b.zone_id, 10_000).await;

    let v = b.precedent().await;
    assert_eq!(v["status"], "restarted", "{v}");
}
