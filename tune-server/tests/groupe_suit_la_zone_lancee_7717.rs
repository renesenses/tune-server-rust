//! #7717 — une lecture groupée Zone 2 + Chromecast Zone 5 est créée depuis un
//! PC ; une nouvelle lecture lancée sur la Zone 2 depuis un autre appareil ne
//! partait pas sur la Zone 5, qui continuait de jouer autre chose.
//!
//! Le groupe est tenu par le serveur (`zone_groups`), pas par le client : le
//! changement d'appareil n'y était pour rien. Il était mené par la zone de la
//! fiche ouverte pour le créer ; formé depuis la fiche de la Chromecast, il ne
//! suivait que la Chromecast. Désormais une lecture lancée sur une zone du
//! groupe, par n'importe quel client, en fait la meneuse.
//!
//! Le banc : la Zone 2 sur un renderer DLNA factice, la Zone 5 sur une
//! Chromecast factice, aucun son. Deux clients se distinguent par leurs
//! en-têtes (`user-agent`, `x-client-id`) ; les lancements passent par
//! `POST /zones/{id}/play`, la route qu'appelle le client web.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par son entrée `[[test]]` dans `Cargo.toml`.
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::PlayState;
use tune_server::routes::zones::relais_de_groupe_6155::lancer_le_relais_de_groupe;
use tune_server::state::AppState;

/// Un appareil qui parle au serveur.
#[derive(Clone, Copy)]
struct Client {
    agent: &'static str,
    id: &'static str,
}

const PC: Client = Client {
    agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Firefox/131.0",
    id: "client-pc",
};
const PORTABLE: Client = Client {
    agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) Safari/605.1.15",
    id: "client-portable",
};

async fn poster(
    app: &axum::Router,
    client: Client,
    chemin: &str,
    corps: Value,
) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header("content-type", "application/json")
                .header("user-agent", client.agent)
                .header("x-client-id", client.id)
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// Les titres que la sortie factice a reçus, dans l'ordre.
async fn joues_par(state: &AppState, device_id: &str) -> Vec<String> {
    let registre = state.outputs.lock().await;
    let sortie = registre.get(device_id).expect("sortie enregistree");
    let garde = sortie.lock().await;
    let mock = garde
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice");
    mock.play_titles().await
}

/// Le titre que la zone joue, d'après son état.
async fn titre_en_cours(state: &AppState, zone: i64) -> Option<(PlayState, String)> {
    let etat = state.playback.get_state(zone).await;
    etat.now_playing.map(|np| (etat.state, np.title))
}

/// Attend (au plus 10 s) que `condition` soit vraie : le relais agit dans une
/// tâche de fond.
async fn attendre<F, Fut>(mut condition: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if condition().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

struct Banc {
    app: axum::Router,
    state: AppState,
    zone2: i64,
    zone5: i64,
    _dir: tempfile::TempDir,
}

const DLNA_ZONE2: &str = "dlna-zone-2";
const CAST_ZONE5: &str = "chromecast-zone-5";

/// Zone 2 (DLNA factice), Zone 5 (Chromecast factice), trois pistes FLAC
/// réelles (« Un », « Deux », « Trois »), le relais lancé comme au démarrage.
async fn zone2_et_chromecast() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // Le regroupement relève du droit « Multiroom Sync ».
    state.license.set_account_premium(true, None).await;
    let app = tune_server::routes::router(state.clone());

    let dir = tempfile::tempdir().unwrap();
    let mut sql = String::from(
        "INSERT INTO artists (id, name) VALUES (1, 'Artiste');\
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1);",
    );
    for (id, titre) in [(1, "Un"), (2, "Deux"), (3, "Trois")] {
        let chemin = dir.path().join(format!("{id}.flac"));
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../tune-core/tests/fixtures/test.flac"
            ),
            &chemin,
        )
        .unwrap();
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                                 duration_ms, sample_rate, bit_depth, channels, source) \
             VALUES ({id}, '{titre}', 1, 1, '{}', 'flac', 300000, 44100, 16, 2, 'local');",
            chemin.to_string_lossy()
        ));
    }
    state.backend.execute_batch(&sql).unwrap();

    let zones = ZoneRepo::with_backend(state.backend.clone());
    let zone2 = zones
        .create("Zone 2", Some("dlna"), Some(DLNA_ZONE2))
        .unwrap();
    let zone5 = zones
        .create("Zone 5", Some("chromecast"), Some(CAST_ZONE5))
        .unwrap();
    {
        let mut registre = state.outputs.lock().await;
        registre.register(Box::new(
            MockOutput::new(DLNA_ZONE2, "Zone 2").with_type("dlna"),
        ));
        registre.register(Box::new(
            MockOutput::new(CAST_ZONE5, "Zone 5").with_type("chromecast"),
        ));
    }
    lancer_le_relais_de_groupe(&state);

    Banc {
        app,
        state,
        zone2,
        zone5,
        _dir: dir,
    }
}

impl Banc {
    /// Le corps exact du client web : la zone de la fiche ouverte en tête.
    async fn grouper_depuis_la_fiche_de(&self, client: Client, fiche: i64) {
        let autre = if fiche == self.zone2 {
            self.zone5
        } else {
            self.zone2
        };
        let (status, corps) = poster(
            &self.app,
            client,
            "/api/v1/zones/group",
            json!({ "leader_id": fiche, "zone_ids": [fiche, autre] }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "regroupement : {corps}");
    }

    async fn lancer(&self, client: Client, zone: i64, track_id: i64) {
        let (status, corps) = poster(
            &self.app,
            client,
            &format!("/api/v1/zones/{zone}/play"),
            json!({ "track_id": track_id }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "lecture de la piste {track_id} : {corps}"
        );
    }

    async fn la_zone_joue(&self, zone: i64, titre: &str) -> bool {
        attendre(|| async {
            titre_en_cours(&self.state, zone).await == Some((PlayState::Playing, titre.to_string()))
        })
        .await
    }

    fn meneuse_enregistree(&self) -> Option<i64> {
        let groupes: Vec<Value> = SettingsRepo::with_backend(self.state.backend.clone())
            .get("zone_groups")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        groupes.first().and_then(|g| g["leader_id"].as_i64())
    }
}

/// (a) Groupe créé sur la Zone 2 depuis le PC, puis deux lectures sur la
/// Zone 2, l'une du PC, l'autre du portable : la Chromecast suit les deux.
#[tokio::test]
async fn une_lecture_d_un_autre_client_sur_la_meneuse_part_sur_la_chromecast() {
    let banc = zone2_et_chromecast().await;
    banc.grouper_depuis_la_fiche_de(PC, banc.zone2).await;

    banc.lancer(PC, banc.zone2, 1).await;
    assert!(
        banc.la_zone_joue(banc.zone5, "Un").await,
        "la Chromecast doit suivre la lecture du PC — reçu : {:?}",
        joues_par(&banc.state, CAST_ZONE5).await
    );

    banc.lancer(PORTABLE, banc.zone2, 3).await;
    assert!(
        banc.la_zone_joue(banc.zone5, "Trois").await,
        "la Chromecast doit suivre une lecture lancée d'un AUTRE appareil — \
         état : {:?}, reçu : {:?}",
        titre_en_cours(&banc.state, banc.zone5).await,
        joues_par(&banc.state, CAST_ZONE5).await
    );
}

/// (b) Le cas du rapport : le groupe a été formé depuis la fiche de la
/// Chromecast, qui joue « Deux ». Le portable lance « Trois » sur la Zone 2 :
/// la Chromecast doit passer à « Trois », pas garder son ancien titre.
#[tokio::test]
async fn une_lecture_lancee_sur_l_autre_zone_du_groupe_part_sur_la_chromecast() {
    let banc = zone2_et_chromecast().await;
    banc.lancer(PC, banc.zone5, 2).await;
    banc.grouper_depuis_la_fiche_de(PC, banc.zone5).await;
    assert!(
        banc.la_zone_joue(banc.zone2, "Deux").await,
        "témoin : la Zone 2 rejoint la Chromecast qui mène"
    );

    banc.lancer(PORTABLE, banc.zone2, 3).await;
    assert!(
        banc.la_zone_joue(banc.zone2, "Trois").await,
        "témoin : la Zone 2 joue la piste lancée"
    );
    assert!(
        banc.la_zone_joue(banc.zone5, "Trois").await,
        "la Chromecast, groupée avec la Zone 2, doit jouer la lecture lancée \
         sur la Zone 2 — état : {:?}, reçu : {:?}. Elle garde « Deux » : le \
         groupe ne suit que la zone depuis laquelle il a été créé, le défaut \
         de #7717.",
        titre_en_cours(&banc.state, banc.zone5).await,
        joues_par(&banc.state, CAST_ZONE5).await
    );
    assert_eq!(
        banc.meneuse_enregistree(),
        Some(banc.zone2),
        "la zone du lancement mène désormais le groupe"
    );
}

/// (c) Après l'alignement, la lecture ne fait pas d'aller-retour : la
/// Chromecast qui rejoint ne reprend pas la tête, la Zone 2 n'est pas relancée.
#[tokio::test]
async fn la_zone_qui_rejoint_ne_reprend_pas_la_tete() {
    let banc = zone2_et_chromecast().await;
    banc.lancer(PC, banc.zone5, 2).await;
    banc.grouper_depuis_la_fiche_de(PC, banc.zone5).await;
    assert!(
        banc.la_zone_joue(banc.zone2, "Deux").await,
        "témoin : groupe aligné"
    );

    banc.lancer(PORTABLE, banc.zone2, 3).await;
    assert!(
        banc.la_zone_joue(banc.zone5, "Trois").await,
        "témoin : la Chromecast suit"
    );
    let zone2_avant = joues_par(&banc.state, DLNA_ZONE2).await;
    let zone5_avant = joues_par(&banc.state, CAST_ZONE5).await;

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        joues_par(&banc.state, DLNA_ZONE2).await,
        zone2_avant,
        "la Zone 2 ne doit pas être relancée par la Chromecast qui la rejoint"
    );
    assert_eq!(
        joues_par(&banc.state, CAST_ZONE5).await,
        zone5_avant,
        "la Chromecast ne doit pas être relancée en boucle"
    );
    assert_eq!(
        banc.meneuse_enregistree(),
        Some(banc.zone2),
        "la meneuse reste la zone du lancement, jamais celle qui rejoint"
    );
}
