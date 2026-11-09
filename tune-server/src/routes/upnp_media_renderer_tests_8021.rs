//! #8021 — une lecture envoyée sur une zone d'un groupe par un point de
//! contrôle UPnP externe, ou par un greffon, ne partait que sur cette zone.
//!
//! Depuis #7717, une lecture lancée depuis Tune sur n'importe quelle zone d'un
//! groupe en fait la meneuse, et le relais fait suivre les autres. Les deux
//! chemins d'ici n'appelaient pas cette règle : la meneuse restait l'autre
//! zone, qui gardait ce qu'elle jouait. L'alignement des suiveuses sur la
//! meneuse est déjà prouvé de bout en bout par
//! `tests/groupe_suit_la_zone_lancee_7717.rs` ; ces témoins prouvent que les
//! deux chemins font changer la meneuse, et la rendent sur un refus.
//!
//! Le banc : deux zones sur des sorties factices (`MockOutput`), aucun son.
use super::*;
use axum::http::Request;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicI64, Ordering};
use tower::ServiceExt;

const FLUX: &str = "http://127.0.0.1:9/stream/8021.flac";
const REFUS: &str = "Le renderer a refusé SetAVTransportURI";

struct Banc {
    state: AppState,
    app: Router,
    /// La zone qui mène le groupe au départ.
    meneuse: i64,
    /// L'autre zone du groupe, celle qui reçoit la lecture.
    zone: i64,
    device: String,
    _dir: tempfile::TempDir,
}

impl Banc {
    async fn new() -> Self {
        // `sessions()` est une statique de processus : une plage d'ids à nous.
        static ZONE: AtomicI64 = AtomicI64::new(8_021_000);
        let meneuse = ZONE.fetch_add(2, Ordering::Relaxed);
        let zone = meneuse + 1;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("renderer.db");
        let config = crate::config::TuneConfig {
            db_path: db.to_str().unwrap().to_owned(),
            ..Default::default()
        };
        let state = AppState::new(db.to_str().unwrap(), 0, config).unwrap();
        let device = format!("mock-8021-{zone}");
        for (id, nom, sortie) in [
            (meneuse, "Zone A", format!("mock-8021-{meneuse}")),
            (zone, "Zone B", device.clone()),
        ] {
            state
                .backend
                .execute(
                    "INSERT INTO zones (id,name,output_type,output_device_id) VALUES (?1,?2,?3,?4)",
                    &[&id, &nom, &"mock", &sortie],
                )
                .unwrap();
            state.outputs.lock().await.register(Box::new(
                tune_core::outputs::mock::MockOutput::new(&sortie, nom).with_type("dlna"),
            ));
        }
        let settings = SettingsRepo::with_backend(state.backend.clone());
        settings
            .set(&format!("zone_{zone}_upnp_renderer"), "true")
            .unwrap();
        settings
            .set(
                "zone_groups",
                &json!([{ "id": 1, "leader_id": meneuse, "zone_ids": [meneuse, zone] }])
                    .to_string(),
            )
            .unwrap();
        let app = crate::routes::router(state.clone());
        Self {
            state,
            app,
            meneuse,
            zone,
            device,
            _dir: dir,
        }
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

    async fn soap(&self, action: &str, args: &str) -> StatusCode {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID>{args}</u:{action}></s:Body></s:Envelope>"#
        );
        self.app
            .clone()
            .oneshot(
                Request::post(format!("/upnp/renderer/{}/AVTransport/control", self.zone))
                    .header("content-type", "text/xml; charset=\"utf-8\"")
                    .header(
                        "soapaction",
                        format!("\"urn:schemas-upnp-org:service:AVTransport:1#{action}\""),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    /// `SetAVTransportURI` puis `Play`, comme un point de contrôle.
    async fn envoyer_un_titre(&self) -> StatusCode {
        let poser = self
            .soap(
                "SetAVTransportURI",
                &format!(
                    "<CurrentURI>{FLUX}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>"
                ),
            )
            .await;
        assert_eq!(poser, StatusCode::OK, "SetAVTransportURI");
        self.soap("Play", "<Speed>1</Speed>").await
    }

    async fn faire_refuser_la_lecture(&self) {
        let outputs = self.state.outputs.lock().await;
        let output = outputs.get(&self.device).unwrap();
        let output = output.lock().await;
        output
            .as_any()
            .downcast_ref::<tune_core::outputs::mock::MockOutput>()
            .unwrap()
            .refuser_la_lecture(Some(REFUS));
    }
}

#[tokio::test]
async fn un_play_upnp_sur_une_zone_du_groupe_en_fait_la_meneuse_8021() {
    let b = Banc::new().await;
    assert_eq!(b.meneuse_enregistree(), Some(b.meneuse), "témoin de départ");

    assert_eq!(b.envoyer_un_titre().await, StatusCode::OK, "Play");
    assert_eq!(
        b.meneuse_enregistree(),
        Some(b.zone),
        "le titre envoyé par un point de contrôle UPnP sur la zone B doit \
         faire de B la meneuse, pour que le relais fasse suivre la zone A — \
         sinon seule B le joue (#8021)"
    );
}

#[tokio::test]
async fn un_play_upnp_refuse_laisse_la_meneuse_d_avant_8021() {
    let b = Banc::new().await;
    b.faire_refuser_la_lecture().await;

    let statut = b.envoyer_un_titre().await;
    assert_ne!(statut, StatusCode::OK, "témoin : la sortie refuse le Play");
    assert_eq!(
        b.meneuse_enregistree(),
        Some(b.meneuse),
        "une lecture refusée ne change pas la zone qui mène le groupe"
    );
}

/// Les lectures lancées par un greffon : l'hôte n'existe qu'avec
/// `plugins-wasm` (lot de tests « plugin-catalog » de la CI).
#[cfg(feature = "plugins-wasm")]
mod greffon {
    use super::*;
    use tune_plugin_runtime_wasm::HostContext;

    /// Une lecture lancée par un greffon, par la vraie fonction hôte.
    async fn lecture_d_un_greffon(b: &Banc) -> Result<Value, String> {
        let hote = crate::plugins_host::AppStateHost::from_state(&b.state);
        let zone = b.zone;
        tokio::task::spawn_blocking(move || {
            hote.play(
                zone,
                json!({ "source": "upnp", "source_id": FLUX, "title": "Titre du greffon" }),
            )
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn une_lecture_de_greffon_sur_une_zone_du_groupe_en_fait_la_meneuse_8021() {
        let b = Banc::new().await;
        let rendu = lecture_d_un_greffon(&b).await.expect("lecture du greffon");
        assert_eq!(
            rendu["output_sent"], true,
            "témoin : la lecture part — {rendu}"
        );
        assert_eq!(
            b.meneuse_enregistree(),
            Some(b.zone),
            "une lecture lancée par un greffon sur la zone B doit faire de B la \
             meneuse du groupe (#8021)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn une_lecture_de_greffon_refusee_laisse_la_meneuse_d_avant_8021() {
        let b = Banc::new().await;
        b.faire_refuser_la_lecture().await;
        let rendu = lecture_d_un_greffon(&b).await;
        assert!(
            !rendu.as_ref().is_ok_and(|r| r["output_sent"] == true),
            "témoin : la sortie refuse — {rendu:?}"
        );
        assert_eq!(
            b.meneuse_enregistree(),
            Some(b.meneuse),
            "une lecture refusée ne change pas la zone qui mène le groupe"
        );
    }
}
