//! #3205 — « mesurer avant de toucher » : le zéro fabriqué de `buffer-stats`.
//!
//! ## Ce que ce ticket fait dépendre d'un chiffre
//!
//! > Si les xruns sont à zéro sur noyau standard, le noyau RT est un coût sans
//! > gain et le Secure Boot revient.
//!
//! Le noyau `PREEMPT_RT` de Tune OS coûte aujourd'hui le Secure Boot et un COPR
//! non signé, à tous ses utilisateurs. La décision de le retirer se prend donc
//! sur un seul nombre — et ce nombre, avant ce lot, était **écrit en dur** à
//! deux endroits :
//!
//! ```text
//! tune-server/src/routes/devices.rs:1481  "total_underruns": 0,
//! tune-server/src/routes/devices.rs:1509  "total_underruns": 0,
//! ```
//!
//! `GET /api/v1/devices/buffer-stats/all` et
//! `GET /api/v1/devices/{id}/buffer-stats` répondaient donc « zéro
//! sous-alimentation » sur n'importe quelle sortie, y compris pendant une
//! coupure. Une campagne de mesure qui aurait lu ces routes aurait conclu au
//! retrait du noyau RT **sans avoir rien mesuré**, et la conclusion aurait eu
//! l'air d'un fait.
//!
//! ## Ce que ces témoins gardent
//!
//! La distinction que #3205 exige, et une seule :
//!
//! | ce que la sortie sait | ce que la route doit dire |
//! |---|---|
//! | rien (pas d'anneau) | `null` |
//! | elle a compté, et rien n'a manqué | `0` |
//! | elle a compté 7 rappels à court | `7` |
//!
//! La deuxième ligne est celle qui compte : un `0` MESURÉ est une information,
//! et il ne doit pas se confondre avec l'absence de mesure. C'est la règle que
//! `tune-core/src/poller/famine_anneau_i3318.rs` s'était déjà donnée — « un
//! zéro qui se lirait comme *mesuré, et sain* » — et que ces deux routes
//! violaient.
//!
//! ## Ce que ces témoins NE gardent pas
//!
//! Ils ne mesurent rien sur du matériel : la machine de compilation n'a ni
//! carte son ni noyau temps réel. Ils portent sur la DÉCISION de la route face
//! à ce qu'une sortie lui rend, et c'est tout ce qu'un banc sans DAC peut
//! honnêtement tenir. La mesure elle-même est décrite dans
//! `docs/mesures/3205-noyau-rt-tune-os.md` et se fait sur une machine Tune OS.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::outputs::traits::OutputRingStarvation;
use tune_core::outputs::{OutputCapabilities, OutputStatus, OutputTarget, TransportState};
use tune_server::state::AppState;

/// Une sortie d'essai dont on choisit ce que `ring_starvation()` rend.
struct SortieDEssai {
    device_id: String,
    famine: Option<OutputRingStarvation>,
}

impl SortieDEssai {
    /// Une sortie qui n'observe pas sa famine — tout renderer réseau.
    fn sans_anneau(device_id: &str) -> Self {
        Self {
            device_id: device_id.to_string(),
            famine: None,
        }
    }

    /// Une sortie qui rend l'audio elle-même et compte ses rappels à court.
    fn avec_anneau(device_id: &str, famine: OutputRingStarvation) -> Self {
        Self {
            device_id: device_id.to_string(),
            famine: Some(famine),
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
        OutputCapabilities::v1(true, true, true, true, true, false)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn ring_starvation(&self) -> Option<OutputRingStarvation> {
        self.famine
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

    async fn enregistre(&self, sortie: SortieDEssai) -> String {
        let device_id = sortie.device_id.clone();
        self.state.outputs.lock().await.register(Box::new(sortie));
        device_id
    }

    async fn lire(&self, chemin: &str) -> (StatusCode, Value) {
        let resp = self
            .app
            .clone()
            .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let statut = resp.status();
        let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    /// La ligne de cette sortie dans `GET /devices/buffer-stats/all`.
    async fn ligne_globale(&self, device_id: &str) -> Value {
        let (statut, liste) = self.lire("/api/v1/devices/buffer-stats/all").await;
        assert_eq!(statut, StatusCode::OK);
        liste
            .as_array()
            .expect("la route globale rend une liste")
            .iter()
            .find(|l| l["device_id"] == device_id)
            .unwrap_or_else(|| panic!("aucune ligne pour {device_id} dans {liste}"))
            .clone()
    }

    /// La réponse de `GET /devices/{id}/buffer-stats`.
    async fn ligne_unitaire(&self, device_id: &str) -> Value {
        let (statut, corps) = self
            .lire(&format!("/api/v1/devices/{device_id}/buffer-stats"))
            .await;
        assert_eq!(statut, StatusCode::OK, "corps : {corps}");
        corps
    }
}

/// ⭐ Le garde principal. Une sortie qui n'observe pas sa famine ne doit pas
/// faire dire à la route qu'elle en a mesuré zéro. Avant ce lot, les deux
/// routes rendaient `0` ici — et c'est ce `0` qui aurait fait retirer le noyau
/// temps réel de Tune OS.
#[tokio::test]
async fn une_sortie_sans_anneau_ne_rapporte_pas_zero_mais_rien() {
    let banc = Banc::neuf();
    let id = banc
        .enregistre(SortieDEssai::sans_anneau("essai:reseau"))
        .await;

    for (route, ligne) in [
        ("globale", banc.ligne_globale(&id).await),
        ("unitaire", banc.ligne_unitaire(&id).await),
    ] {
        assert!(
            ligne["total_underruns"].is_null(),
            "route {route} : une sortie sans anneau doit rendre null, pas {}",
            ligne["total_underruns"]
        );
        assert!(
            ligne["served_samples"].is_null(),
            "route {route} : pas de denominateur non plus"
        );
        assert!(
            ligne["total_disconnections"].is_null(),
            "route {route} : aucun compteur de deconnexion n'existe dans l'arbre"
        );
    }
}

/// ⭐⭐ La contre-épreuve qui donne son sens au ticket : une sortie qui a
/// VRAIMENT compté, et qui n'a rien perdu, rend `0`. Sans ce témoin, on
/// pourrait « corriger » le défaut en rendant `null` partout, et le chiffre que
/// #3205 réclame n'existerait toujours pas.
#[tokio::test]
async fn un_zero_mesure_se_distingue_d_une_absence_de_mesure() {
    let banc = Banc::neuf();
    let sain = banc
        .enregistre(SortieDEssai::avec_anneau(
            "essai:local-sain",
            OutputRingStarvation {
                events: 0,
                missing_samples: 0,
                served_samples: 96_000,
                stream_ms: 1_000,
            },
        ))
        .await;
    let muet = banc
        .enregistre(SortieDEssai::sans_anneau("essai:reseau-muet"))
        .await;

    let mesure = banc.ligne_globale(&sain).await;
    assert_eq!(
        mesure["total_underruns"], 0,
        "une sortie qui a compte zero doit rendre zero"
    );
    assert_eq!(
        mesure["served_samples"], 96_000,
        "le denominateur doit accompagner le compteur : « 0 sur rien » ne dit rien"
    );
    assert_eq!(mesure["stream_ms"], 1_000);

    let absence = banc.ligne_globale(&muet).await;
    assert!(absence["total_underruns"].is_null());
    assert_ne!(
        mesure["total_underruns"], absence["total_underruns"],
        "« zero mesure » et « pas mesure » ne doivent pas se lire pareil"
    );
}

/// Ce que la sortie a compté arrive tel quel, sur les deux routes. Un compteur
/// d'événements seul ne distingue pas un micro-trou d'une coupure d'une
/// seconde : `missing_samples` part avec lui.
#[tokio::test]
async fn les_compteurs_de_la_sortie_arrivent_entiers_sur_les_deux_routes() {
    let banc = Banc::neuf();
    let id = banc
        .enregistre(SortieDEssai::avec_anneau(
            "essai:local-affame",
            OutputRingStarvation {
                events: 7,
                missing_samples: 12_345,
                served_samples: 33_816_156,
                stream_ms: 352_251,
            },
        ))
        .await;

    for (route, ligne) in [
        ("globale", banc.ligne_globale(&id).await),
        ("unitaire", banc.ligne_unitaire(&id).await),
    ] {
        assert_eq!(ligne["total_underruns"], 7, "route {route}");
        assert_eq!(
            ligne["ring_starvation_missing_samples"], 12_345,
            "route {route} : sans lui, « 7 evenements » ne dit pas la gravite"
        );
        assert_eq!(ligne["served_samples"], 33_816_156, "route {route}");
        assert_eq!(ligne["stream_ms"], 352_251, "route {route}");
    }
}

/// Témoin de non-régression : les champs que le client lisait déjà sont
/// toujours là, et au même endroit. Ce lot corrige un chiffre, il ne réécrit
/// pas le contrat de la route.
#[tokio::test]
async fn les_champs_historiques_de_buffer_stats_sont_intacts() {
    let banc = Banc::neuf();
    let id = banc
        .enregistre(SortieDEssai::sans_anneau("essai:contrat"))
        .await;

    let ligne = banc.ligne_unitaire(&id).await;
    assert_eq!(ligne["device_id"], id);
    assert_eq!(ligne["device_name"], "Sortie d'essai");
    assert_eq!(ligne["buffer_s"], 2.0, "le defaut historique");
    assert_eq!(ligne["auto"], true);
    assert_eq!(ligne["manual_override"], false);
}
