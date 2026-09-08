//! #3322 — `output_capabilities.channel_layouts` : déclaré, publié, jamais rempli.
//!
//! ## Le constat
//!
//! Le moteur multicanal de `tune_core::audio::channels` — neuf dispositions
//! nommées de `Mono` à `Immersive32` — existe depuis le portage de la branche
//! Python `feat/multichannel` et **aucune route ne l'expose**. Mesure du
//! 04/09/2026 sur le .18 (v0.9.130), `GET /api/v1/zones`, 14 zones :
//! `output_capabilities.channel_layouts` est vide ou nul sur les quatorze,
//! tous types de sortie confondus.
//!
//! La cause mécanique est étroite : `OutputCapabilities::v1()` pose
//! `channel_layouts: Vec::new()` en dur, et il n'existait aucun builder pour
//! l'écrire, là où `with_linear_volume`, `with_percent_volume` et
//! `with_decibel_volume` existent pour le volume. Le champ n'avait donc
//! **aucun chemin d'écriture**.
//!
//! ## Ce que ce fichier garde
//!
//! Il passe par les ROUTES MONTÉES. Un témoin qui appellerait
//! `ChannelLayout::noms_jusqu_a` en direct resterait VERT le jour où plus
//! personne n'appelle cette fonction — c'est-à-dire exactement l'état que
//! l'issue décrit.
//!
//! 1. une sortie qui déclare ses dispositions les voit ARRIVER chez le client,
//!    sur `GET /zones` **et** sur `GET /zones/{id}` (critère 4 : les deux
//!    routes publient la même chose) ;
//! 2. une sortie qui n'en déclare aucune, et dont l'appareil est inconnu,
//!    publie `[]` — jamais une valeur inventée (critère 2) ;
//! 3. une sortie LOCALE reçoit celles que son appareil sait rendre, déduites
//!    de `max_channels` (critère 1), par l'appel de production
//!    `output_capabilities_avec` — retirer l'enrichissement le fait rougir.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::sync::Mutex;
use tower::ServiceExt;
use tune_core::outputs::{OutputCapabilities, OutputStatus, OutputTarget, TransportState};
use tune_server::state::AppState;

/// Une sortie d'essai dont on choisit les capacités publiées.
struct SortieDEssai {
    device_id: String,
    capabilities: OutputCapabilities,
    _recus: Mutex<Vec<f64>>,
}

impl SortieDEssai {
    fn neuve(device_id: &str, capabilities: OutputCapabilities) -> Self {
        Self {
            device_id: device_id.to_string(),
            capabilities,
            _recus: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl OutputTarget for SortieDEssai {
    fn name(&self) -> &str {
        "Sortie d'essai"
    }
    fn device_id(&self) -> &str {
        &self.device_id
    }
    fn output_type(&self) -> &str {
        "essai"
    }
    fn capabilities(&self) -> OutputCapabilities {
        self.capabilities.clone()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus {
            state: TransportState::Stopped,
            ..Default::default()
        })
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Banc {
    app: axum::Router,
    state: AppState,
}

impl Banc {
    fn neuf() -> Self {
        let state = AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite");
        let app = tune_server::routes::router(state.clone());
        Self { app, state }
    }

    /// Enregistre la sortie et crée la zone qui lui est liée. Rend son id.
    async fn zone_liee(&self, sortie: SortieDEssai) -> i64 {
        let device_id = sortie.device_id.clone();
        self.state.outputs.lock().await.register(Box::new(sortie));
        let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(self.state.backend.clone());
        zones
            .create("Zone d'essai", Some("essai"), Some(&device_id))
            .expect("création de la zone")
    }

    async fn lire(&self, chemin: &str) -> (StatusCode, Value) {
        let resp = self
            .app
            .clone()
            .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }
}

fn dispositions(charge: &Value) -> Vec<String> {
    charge["output_capabilities"]["channel_layouts"]
        .as_array()
        .map(|liste| {
            liste
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Ce que déclare la sortie arrive chez le client — et par les DEUX routes.
#[tokio::test]
async fn les_dispositions_declarees_atteignent_les_deux_routes() {
    let banc = Banc::neuf();
    let attendu = tune_core::audio::channels::ChannelLayout::noms_jusqu_a(8);
    assert_eq!(
        attendu,
        vec!["mono", "stereo", "surround51", "surround71"],
        "le vocabulaire du serveur, pas une plage libre 0–32"
    );
    let zone_id = banc
        .zone_liee(SortieDEssai::neuve(
            "essai:huit-canaux",
            OutputCapabilities::v1(true, true, true, true, true, false)
                .with_channel_layouts(attendu.clone()),
        ))
        .await;

    let (status, liste) = banc.lire("/api/v1/zones").await;
    assert_eq!(status, StatusCode::OK);
    let dans_la_liste = liste
        .as_array()
        .expect("un tableau de zones")
        .iter()
        .find(|z| z["id"].as_i64() == Some(zone_id))
        .expect("la zone d'essai");
    assert_eq!(
        dispositions(dans_la_liste),
        attendu,
        "GET /zones doit publier les dispositions déclarées par la sortie"
    );

    let (status, detail) = banc.lire(&format!("/api/v1/zones/{zone_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        dispositions(&detail),
        attendu,
        "GET /zones/{{id}} doit publier exactement la même chose que GET /zones"
    );
}

/// Une capacité inconnue publie `[]`, jamais une valeur inventée.
#[tokio::test]
async fn une_sortie_qui_ne_sait_pas_ne_declare_rien() {
    let banc = Banc::neuf();
    let zone_id = banc
        .zone_liee(SortieDEssai::neuve(
            // Pas de préfixe `local:` : aucun appareil énuméré ne peut le
            // renseigner, et rien ne doit être supposé pour autant.
            "dlna:renderer-inconnu",
            OutputCapabilities::v1(true, true, true, true, true, true),
        ))
        .await;

    let (_, detail) = banc.lire(&format!("/api/v1/zones/{zone_id}")).await;
    assert!(
        dispositions(&detail).is_empty(),
        "un renderer réseau négocie ses canaux dans le flux : le serveur n'en \
         sait rien et doit le dire par []"
    );
}

/// Le cœur de #3322 : une sortie LOCALE est enrichie depuis le parc énuméré.
///
/// Ce témoin appelle `output_capabilities_avec`, la fonction que
/// `GET /zones` et `GET /zones/{id}` empruntent tous les deux, avec un parc
/// donné — il ne dépend donc pas de la carte son de la machine qui l'exécute.
/// Retirer l'enrichissement de cette fonction le fait rougir.
#[tokio::test]
async fn une_sortie_locale_est_enrichie_depuis_le_parc_enumere() {
    let banc = Banc::neuf();
    // L'identifiant est bâti par `format!("local:{}", dev.name)` au moment de
    // l'enregistrement (startup.rs, background.rs) : c'est ce nom-là qui sert
    // de clé.
    banc.zone_liee(SortieDEssai::neuve(
        "local:Convertisseur 8 voies",
        OutputCapabilities::v1(true, true, true, true, true, false),
    ))
    .await;

    let parc = vec![
        ("Convertisseur 8 voies".to_string(), 8u16),
        ("Une autre carte".to_string(), 2u16),
    ];
    let capacites = tune_server::routes::zones::output_capabilities_avec(
        &banc.state,
        Some("local:Convertisseur 8 voies"),
        &parc,
    )
    .await
    .expect("la sortie est enregistrée");
    assert_eq!(
        capacites.channel_layouts,
        vec!["mono", "stereo", "surround51", "surround71"],
        "les dispositions d'une sortie locale se déduisent de max_channels"
    );

    // Le même appareil absent du parc : rien n'est supposé.
    let capacites = tune_server::routes::zones::output_capabilities_avec(
        &banc.state,
        Some("local:Convertisseur 8 voies"),
        &[],
    )
    .await
    .expect("la sortie est enregistrée");
    assert!(
        capacites.channel_layouts.is_empty(),
        "un appareil absent de l'énumération ne se devine pas"
    );
}

/// L'enrichissement ne recouvre jamais une donnée de première main.
#[tokio::test]
async fn une_declaration_de_la_sortie_prime_sur_la_deduction() {
    let banc = Banc::neuf();
    banc.zone_liee(SortieDEssai::neuve(
        "local:Carte qui sait",
        OutputCapabilities::v1(true, true, true, true, true, false)
            .with_channel_layouts(vec!["stereo".to_string()]),
    ))
    .await;

    let capacites = tune_server::routes::zones::output_capabilities_avec(
        &banc.state,
        Some("local:Carte qui sait"),
        &[("Carte qui sait".to_string(), 32u16)],
    )
    .await
    .expect("la sortie est enregistrée");
    assert_eq!(
        capacites.channel_layouts,
        vec!["stereo"],
        "ce que la sortie déclare elle-même ne doit jamais être écrasé"
    );
}
