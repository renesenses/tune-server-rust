//! « Pont Roon » comme greffon natif [`TunePlugin`] — PREMIUM.
//!
//! Bertrand, 16/09/2026 : « Pont Roon, images [et import] : en plugin
//! PREMIUM ». Le moissonneur (`moissonneur-roon --archive=…`) parcourt un Core
//! Roon et range dans UNE archive l'export (`export.json`) et les octets des
//! images (`images/<clé>.jpg`) — les clés d'image de Roon ne servent qu'au Core
//! qui les a émises. Ce greffon importe cette archive :
//!
//! - `GET  /ext/pont-roon/`              → droit Premium ? + dernier rapport ;
//! - `POST /ext/pont-roon/import?apercu=true|false` (corps = l'archive, ou
//!   l'`export.json` seul) → le rapport. L'aperçu compte, n'écrit rien.
//!
//! L'écriture est celle de `tune_core::library::pont_roon_import` — la même
//! que la porte `POST /system/import/roon`, pour qu'aucune des deux ne dérive.
//! Tune ne remplace JAMAIS une image ni un crédit qu'il a déjà.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::DbBackend;
use tune_core::event_bus::TuneEvent;
use tune_core::library::pont_roon::ExportRoon;
use tune_core::library::pont_roon_import::{
    ARCHIVE_MAX_OCTETS, ImagesRoon, appliquer, est_un_export_du_pont, lire_archive,
};
use tune_core::license::{Feature, LicenseManager};
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

/// Réglage où le dernier rapport d'import (hors aperçu) est gardé.
pub const CLE_DERNIER_RAPPORT: &str = "pont_roon_dernier_rapport";
const URL_OFFRE: &str = "https://mozaiklabs.fr/pricing";

/// Ce que l'hôte fournit : la base, la licence, le cache d'illustrations.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    pub license: Arc<LicenseManager>,
    pub dossier_cache: PathBuf,
}

pub struct PontRoonPlugin {
    etat: Etat,
}

impl PontRoonPlugin {
    pub fn new(s: HostServices) -> Self {
        Self {
            etat: Etat {
                backend: s.backend,
                license: s.license,
                dossier_cache: Arc::new(s.dossier_cache),
            },
        }
    }
}

#[async_trait]
impl TunePlugin for PontRoonPlugin {
    fn name(&self) -> &str {
        "pont-roon"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "Pont Roon (Premium) : crédits, images d'artistes et pochettes récoltés sur un Core Roon"
    }
    /// Opt-in, comme Bandcamp : on l'installe quand on a un Core Roon.
    fn default_enabled(&self) -> bool {
        false
    }
    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        ctx.register_router(router(self.etat.clone()));
        Ok(())
    }
    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }
    async fn on_event(&mut self, _event: &TuneEvent) {}
}

#[derive(Clone)]
struct Etat {
    backend: Arc<dyn DbBackend>,
    license: Arc<LicenseManager>,
    dossier_cache: Arc<PathBuf>,
}

fn router(etat: Etat) -> Router<()> {
    Router::new()
        .route("/", get(etat_du_pont))
        .route(
            "/import",
            post(importer).layer(DefaultBodyLimit::max(ARCHIVE_MAX_OCTETS as usize)),
        )
        .with_state(etat)
}

/// Le refus Premium, même forme que `premium_guard` du serveur : `code` est le
/// terme stable que le client traduit.
fn refus_premium() -> Response {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(json!({
            "error": "premium_required",
            "code": Feature::PontRoon.code(),
            "feature": Feature::PontRoon.display_name(),
            "upgrade_url": URL_OFFRE,
        })),
    )
        .into_response()
}

fn dernier_rapport(backend: &Arc<dyn DbBackend>) -> Option<Value> {
    tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone())
        .get(CLE_DERNIER_RAPPORT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// `GET /` — le droit, et le dernier import. Sans Premium, l'écran dit
/// pourquoi le bouton est éteint au lieu d'échouer au clic.
async fn etat_du_pont(State(etat): State<Etat>) -> Json<Value> {
    Json(json!({
        "premium": etat.license.check_feature(Feature::PontRoon).await,
        "dernier_rapport": dernier_rapport(&etat.backend),
    }))
}

#[derive(Deserialize)]
struct ImportQuery {
    #[serde(default)]
    apercu: bool,
}

/// `POST /import` — l'archive du moissonneur, ou l'`export.json` seul.
async fn importer(
    State(etat): State<Etat>,
    Query(q): Query<ImportQuery>,
    corps: Bytes,
) -> Response {
    if !etat.license.check_feature(Feature::PontRoon).await {
        tracing::info!("pont_roon_refuse_sans_premium");
        return refus_premium();
    }
    let apercu = q.apercu;
    let backend = etat.backend.clone();
    let dossier = etat.dossier_cache.clone();
    let travail = tokio::task::spawn_blocking(move || -> Result<Value, (StatusCode, String)> {
        let (export, images) = if corps.starts_with(b"PK\x03\x04") {
            lire_archive(&corps).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?
        } else {
            let texte = std::str::from_utf8(&corps).map_err(|_| {
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "ni archive, ni JSON UTF-8".to_string(),
                )
            })?;
            if !est_un_export_du_pont(texte) {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "ce n'est pas un export du moissonneur".into(),
                ));
            }
            let export =
                ExportRoon::lire(texte).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
            (export, Default::default())
        };
        let porte = ImagesRoon {
            octets: &images,
            dossier_cache: dossier.as_path(),
        };
        let rapport = appliquer(&backend, &export, apercu, Some(&porte));
        let mut v = serde_json::to_value(&rapport).unwrap_or_default();
        v["preview"] = json!(apercu);
        v["core"] = json!(export.core);
        v["releve"] = json!(export.releve);
        v["absent_de_l_api"] = json!(export.absent_de_l_api);
        v["archive"] = json!(!images.is_empty());
        if !apercu {
            if let Err(e) =
                tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone())
                    .set(CLE_DERNIER_RAPPORT, &v.to_string())
            {
                tracing::warn!(erreur = %e, "pont_roon_rapport_non_garde");
            }
        }
        Ok(v)
    })
    .await;
    match travail {
        Ok(Ok(v)) => {
            tracing::info!(apercu, rapport = %v, "pont_roon_import");
            (StatusCode::OK, Json(v)).into_response()
        }
        Ok(Err((statut, e))) => {
            tracing::warn!(erreur = %e, "pont_roon_import_refuse");
            (
                statut,
                Json(json!({"error": "export_pont_roon_illisible", "detail": e})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("import interrompu : {e}")})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn etat_dans(cache: &std::path::Path) -> Etat {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        Etat {
            license: Arc::new(LicenseManager::new(backend.clone())),
            backend,
            dossier_cache: Arc::new(cache.to_path_buf()),
        }
    }

    async fn json_de(rep: Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(rep.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    fn archive(export: &str, images: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut t = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut t);
            let o = zip::write::SimpleFileOptions::default();
            z.start_file("export.json", o).unwrap();
            z.write_all(export.as_bytes()).unwrap();
            for (cle, octets) in images {
                z.start_file(format!("images/{cle}.jpg"), o).unwrap();
                z.write_all(octets).unwrap();
            }
            z.finish().unwrap();
        }
        t.into_inner()
    }

    /// Sans Premium : 402 et le code stable, sur l'import comme sur l'aperçu.
    #[tokio::test]
    async fn sans_premium_l_import_est_refuse_et_l_etat_le_dit() {
        let cache = tune_core::test_scratch::scratch_dir("pont_roon_refus");
        let app = router(etat_dans(cache.path()));
        for uri in ["/import?apercu=true", "/import"] {
            let rep = app
                .clone()
                .oneshot(
                    Request::post(uri)
                        .body(Body::from(r#"{"source":"roon","artistes":[]}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(rep.status(), StatusCode::PAYMENT_REQUIRED, "{uri}");
            assert_eq!(json_de(rep).await["code"], "pont_roon");
        }
        let rep = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let v = json_de(rep).await;
        assert_eq!(v["premium"], false);
        assert!(v["dernier_rapport"].is_null());
    }

    /// Avec Premium : l'aperçu n'écrit rien ; l'import pose l'image d'artiste
    /// depuis l'archive et garde le rapport, que `GET /` relit.
    #[tokio::test]
    async fn avec_premium_l_archive_pose_les_images_et_le_rapport_reste() {
        let cache = tune_core::test_scratch::scratch_dir("pont_roon_import");
        let e = etat_dans(cache.path());
        e.license.set_account_premium(true, None).await;
        e.backend
            .execute(
                "INSERT INTO artists (id, name) VALUES (1, 'Nick Drake')",
                &[],
            )
            .unwrap();
        let zip = archive(
            r#"{"source":"roon","core":"10.0.0.1:9330","artistes":[{"nom":"Nick Drake","image":"k1","albums":[]}]}"#,
            &[("k1", &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3])],
        );
        let app = router(e.clone());

        let rep = app
            .clone()
            .oneshot(
                Request::post("/import?apercu=true")
                    .body(Body::from(zip.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rep.status(), StatusCode::OK);
        let v = json_de(rep).await;
        assert_eq!(
            (
                v["images_artistes_a_poser"].as_u64(),
                v["images_artistes_posees"].as_u64()
            ),
            (Some(1), Some(0)),
            "{v}"
        );
        assert_eq!(v["archive"], true);

        let rep = app
            .clone()
            .oneshot(Request::post("/import").body(Body::from(zip)).unwrap())
            .await
            .unwrap();
        let v = json_de(rep).await;
        assert_eq!(v["images_artistes_posees"].as_u64(), Some(1), "{v}");
        let artiste = tune_core::db::artist_repo::ArtistRepo::with_backend(e.backend.clone())
            .get(1)
            .unwrap()
            .unwrap();
        assert_eq!(artiste.image_source.as_deref(), Some("roon"));

        let v = json_de(
            app.clone()
                .oneshot(Request::get("/").body(Body::empty()).unwrap())
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(v["premium"], true, "{v}");
        assert_eq!(
            v["dernier_rapport"]["images_artistes_posees"].as_u64(),
            Some(1),
            "{v}"
        );
        assert_eq!(v["dernier_rapport"]["core"], "10.0.0.1:9330");

        // Un corps qui n'est ni archive ni export : 422, rien d'écrit.
        let rep = app
            .oneshot(
                Request::post("/import")
                    .body(Body::from("Title,Artist"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rep.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
