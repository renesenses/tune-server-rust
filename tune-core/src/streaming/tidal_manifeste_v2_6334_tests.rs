//! #6334 — la lecture par `trackManifests` v2 et son repli v1, sur des
//! réponses ENREGISTRÉES (forme du document JSON:API de TIDAL, SDK officiel
//! `TrackManifestsAttributes`). Un faux TIDAL sur la boucle locale sert les
//! deux points d'accès ; aucun appel réel, jeton d'essai seulement.

use super::manifeste_v2::{VOIE_V2, interpreter_manifeste_v2};
use super::*;
use base64::Engine as _;
use std::collections::HashMap;

const URL_FLAC_V2: &str = "https://cdn.essai.invalid/v2/flac-16-44.mp4";
const URL_HIRES_V2: &str = "https://cdn.essai.invalid/v2/flac-24-96.mp4";
const URL_AAC_V2: &str = "https://cdn.essai.invalid/v2/aac-320.mp4";
const URL_AAC_V1: &str = "https://cdn.essai.invalid/v1/aac-320.m4a";
const URL_FLAC_V1: &str = "https://cdn.essai.invalid/v1/flac-16-44.flac";

fn mpd(codec: &str, cadence: u32, url: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT3M41S">
 <Period id="0"><AdaptationSet id="0" contentType="audio" mimeType="audio/mp4" segmentAlignment="true">
  <Representation id="FLAC,{cadence}" codecs="{codec}" bandwidth="1411200" audioSamplingRate="{cadence}">
   <BaseURL>{url}</BaseURL>
  </Representation>
 </AdaptationSet></Period>
</MPD>"#
    )
}

/// Le document `trackManifests` tel que TIDAL le rend (uriScheme=DATA).
fn document_v2(formats: &[&str], codec: &str, cadence: u32, url: &str) -> serde_json::Value {
    let uri = format!(
        "data:application/dash+xml;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(mpd(codec, cadence, url))
    );
    json!({
        "data": {
            "id": "61681502",
            "type": "trackManifests",
            "attributes": {
                "trackPresentation": "FULL",
                "uri": uri,
                "hash": "essai",
                "formats": formats,
                "albumAudioNormalizationData": {"replayGain": -8.1, "peakAmplitude": 0.99},
                "trackAudioNormalizationData": {"replayGain": -7.9, "peakAmplitude": 0.98}
            }
        },
        "links": {"self": "/trackManifests/61681502"}
    })
}

/// La réponse v1 `playbackinfopostpaywall` du défaut : HIGH, AAC, BTS.
fn v1(audio_quality: &str, mime: &str, codecs: &str, url: &str) -> serde_json::Value {
    let bts = json!({"mimeType": mime, "codecs": codecs, "encryptionType": "NONE", "urls": [url]});
    let mut corps = json!({
        "trackId": 61681502,
        "assetPresentation": "FULL",
        "audioMode": "STEREO",
        "audioQuality": audio_quality,
        "manifestMimeType": "application/vnd.tidal.bts",
        "manifest": base64::engine::general_purpose::STANDARD.encode(bts.to_string()),
    });
    if audio_quality == "LOSSLESS" {
        corps["bitDepth"] = json!(16);
        corps["sampleRate"] = json!(44_100);
    }
    corps
}

#[derive(Default)]
struct Journal {
    /// Les `formats` demandés à la v2, dans l'ordre.
    v2: Vec<String>,
    /// Les `audioquality` demandées à la v1, dans l'ordre.
    v1: Vec<String>,
    /// Paramètres et en-têtes de la dernière requête v2.
    requete_v2: HashMap<String, String>,
}

type Etat = (
    Arc<HashMap<String, (u16, serde_json::Value)>>,
    Arc<HashMap<String, serde_json::Value>>,
    Arc<std::sync::Mutex<Journal>>,
);

/// Un faux TIDAL : `v2` (format → statut, corps), `v1` (qualité → corps ;
/// absente : 429).
async fn faux_tidal(
    v2: HashMap<String, (u16, serde_json::Value)>,
    v1: HashMap<String, serde_json::Value>,
) -> (TidalService, Arc<std::sync::Mutex<Journal>>) {
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;

    async fn manifeste(
        State((v2, _, journal)): State<Etat>,
        entetes: HeaderMap,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        let format = q.get("formats").cloned().unwrap_or_default();
        {
            let mut j = journal.lock().unwrap();
            j.v2.push(format.clone());
            j.requete_v2 = q.clone();
            for nom in ["authorization", "accept"] {
                if let Some(v) = entetes.get(nom).and_then(|v| v.to_str().ok()) {
                    j.requete_v2.insert(nom.into(), v.into());
                }
            }
        }
        match v2.get(&format) {
            Some((statut, corps)) => (
                StatusCode::from_u16(*statut).unwrap(),
                axum::Json(corps.clone()),
            )
                .into_response(),
            None => (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"errors": [{"code": "NOT_FOUND"}]})),
            )
                .into_response(),
        }
    }

    async fn lecture(
        State((_, v1, journal)): State<Etat>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        let qualite = q.get("audioquality").cloned().unwrap_or_default();
        journal.lock().unwrap().v1.push(qualite.clone());
        match v1.get(&qualite) {
            Some(corps) => axum::Json(corps.clone()).into_response(),
            None => (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(json!({"status": 429})),
            )
                .into_response(),
        }
    }

    let journal: Arc<std::sync::Mutex<Journal>> = Arc::default();
    let app = axum::Router::new()
        .route("/trackManifests/{id}", get(manifeste))
        .route("/tracks/{id}/playbackinfopostpaywall", get(lecture))
        .with_state((Arc::new(v2), Arc::new(v1), journal.clone()));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (
        TidalService::pour_un_faux_tidal(format!("http://{adresse}")),
        journal,
    )
}

fn v1_du_defaut() -> HashMap<String, serde_json::Value> {
    ["HI_RES_LOSSLESS", "HI_RES", "LOSSLESS", "HIGH"]
        .into_iter()
        .map(|q| {
            (
                q.to_string(),
                v1("HIGH", "audio/mp4", "mp4a.40.2", URL_AAC_V1),
            )
        })
        .collect()
}

/// LE cas des cinq fils : la v1 rend HIGH (AAC) à toutes les marches, la v2
/// rend le FLAC 16/44,1. Tune lit la v2, annonce FLAC 16/44,1, et ne demande
/// rien à la v1.
#[tokio::test]
async fn la_v2_rend_le_flac_que_la_v1_refusait_6334() {
    let v2 = HashMap::from([(
        "FLAC".to_string(),
        (200, document_v2(&["FLAC"], "flac", 44_100, URL_FLAC_V2)),
    )]);
    let (svc, journal) = faux_tidal(v2, v1_du_defaut()).await;
    let flux = svc
        .get_track_url("61681502", Some("LOSSLESS"))
        .await
        .expect("un flux");
    assert_eq!(
        flux.quality.codec, "FLAC",
        "#6334 : la v2 rend du FLAC, Tune ne doit plus servir l'AAC de la v1"
    );
    assert_eq!(flux.url, URL_FLAC_V2);
    assert_eq!(
        (flux.quality.sample_rate, flux.quality.bit_depth),
        (44_100, 16)
    );
    let j = journal.lock().unwrap();
    assert_eq!(j.v2, ["FLAC"]);
    assert!(j.v1.is_empty(), "la v1 n'est pas appelée : {:?}", j.v1);
}

/// La requête v2 : MPEG-DASH, URI `data:`, lecture, JSON:API, le jeton de la
/// session (celui du code appareil / PKCE, inchangé).
#[tokio::test]
async fn la_requete_v2_est_celle_du_lecteur_6334() {
    let v2 = HashMap::from([(
        "FLAC".to_string(),
        (200, document_v2(&["FLAC"], "flac", 44_100, URL_FLAC_V2)),
    )]);
    let (svc, journal) = faux_tidal(v2, v1_du_defaut()).await;
    svc.get_track_url("61681502", Some("LOSSLESS"))
        .await
        .unwrap();
    let j = journal.lock().unwrap();
    let r = &j.requete_v2;
    assert_eq!(r["manifestType"], "MPEG_DASH");
    assert_eq!(r["uriScheme"], "DATA");
    assert_eq!(r["usage"], "PLAYBACK");
    assert_eq!(r["adaptive"], "false");
    assert_eq!(r["accept"], "application/vnd.api+json");
    assert_eq!(r["authorization"], "Bearer jeton-essai");
}

/// Hi-res : `FLAC_HIRES` d'abord ; 24 bits et la cadence du MPD.
#[tokio::test]
async fn le_hires_vient_de_flac_hires_6334() {
    let v2 = HashMap::from([(
        "FLAC_HIRES".to_string(),
        (
            200,
            document_v2(&["FLAC_HIRES"], "flac", 96_000, URL_HIRES_V2),
        ),
    )]);
    let (svc, journal) = faux_tidal(v2, v1_du_defaut()).await;
    let flux = svc
        .get_track_url("225405487", Some("HI_RES_LOSSLESS"))
        .await
        .unwrap();
    assert_eq!(flux.quality.codec, "FLAC");
    assert_eq!(
        (flux.quality.sample_rate, flux.quality.bit_depth),
        (96_000, 24)
    );
    assert_eq!(journal.lock().unwrap().v2, ["FLAC_HIRES"]);
}

/// Un titre CD demandé en « max » : `FLAC_HIRES` refusé, `FLAC` servi.
#[tokio::test]
async fn un_titre_cd_demande_en_max_tombe_sur_flac_6334() {
    let v2 = HashMap::from([(
        "FLAC".to_string(),
        (200, document_v2(&["FLAC"], "flac", 44_100, URL_FLAC_V2)),
    )]);
    let (svc, journal) = faux_tidal(v2, v1_du_defaut()).await;
    let flux = svc
        .get_track_url("476388871", Some("HI_RES_LOSSLESS"))
        .await
        .unwrap();
    assert_eq!(flux.quality.codec, "FLAC");
    assert_eq!(flux.quality.bit_depth, 16);
    let j = journal.lock().unwrap();
    assert_eq!(j.v2, ["FLAC_HIRES", "FLAC"]);
    assert!(j.v1.is_empty());
}

/// La v2 échoue (500) : la cascade v1 d'avant sert, inchangée.
#[tokio::test]
async fn la_v2_en_echec_laisse_la_v1_servir_6334() {
    let v2 = HashMap::from([(
        "FLAC".to_string(),
        (500, json!({"errors": [{"code": "INTERNAL"}]})),
    )]);
    let v1 = HashMap::from([(
        "LOSSLESS".to_string(),
        v1("LOSSLESS", "audio/flac", "flac", URL_FLAC_V1),
    )]);
    let (svc, journal) = faux_tidal(v2, v1).await;
    let flux = svc
        .get_track_url("61681502", Some("LOSSLESS"))
        .await
        .expect("le repli v1");
    assert_eq!(flux.url, URL_FLAC_V1);
    assert_eq!(flux.quality.codec, "FLAC");
    assert_eq!(journal.lock().unwrap().v1, ["LOSSLESS"]);
}

/// Un AAC reçu de la v2 sur une demande FLAC est annoncé AAC (0 bit, pas
/// d'étiquette CD) ; la v1 est tentée derrière, et l'AAC est servi si elle ne
/// fait pas mieux.
#[tokio::test]
async fn un_aac_de_la_v2_est_annonce_aac_6334() {
    let v2 = HashMap::from([(
        "FLAC".to_string(),
        (
            200,
            document_v2(&["AACLC"], "mp4a.40.2", 44_100, URL_AAC_V2),
        ),
    )]);
    let (svc, journal) = faux_tidal(v2, v1_du_defaut()).await;
    let flux = svc
        .get_track_url("61681502", Some("LOSSLESS"))
        .await
        .unwrap();
    assert_eq!(
        flux.quality.codec, "AAC",
        "#6334 : un AAC n'est jamais étiqueté CD"
    );
    assert_eq!(flux.quality.bit_depth, 0);
    assert!(flux.quality.avec_perte());
    assert_eq!(
        journal.lock().unwrap().v1,
        ["LOSSLESS"],
        "la v1 a sa chance"
    );
}

/// La v2 rend AAC, la v1 FLAC : le FLAC l'emporte.
#[tokio::test]
async fn un_flac_de_la_v1_bat_un_aac_de_la_v2_6334() {
    let v2 = HashMap::from([(
        "FLAC".to_string(),
        (
            200,
            document_v2(&["AACLC"], "mp4a.40.2", 44_100, URL_AAC_V2),
        ),
    )]);
    let v1 = HashMap::from([(
        "LOSSLESS".to_string(),
        v1("LOSSLESS", "audio/flac", "flac", URL_FLAC_V1),
    )]);
    let (svc, _) = faux_tidal(v2, v1).await;
    let flux = svc
        .get_track_url("61681502", Some("LOSSLESS"))
        .await
        .unwrap();
    assert_eq!(flux.url, URL_FLAC_V1);
    assert_eq!(flux.quality.codec, "FLAC");
}

/// Un extrait de 30 s (`PREVIEW`) n'est pas une lecture : la v1 sert.
#[tokio::test]
async fn un_extrait_v2_n_est_pas_servi_6334() {
    let mut doc = document_v2(&["FLAC"], "flac", 44_100, URL_FLAC_V2);
    doc["data"]["attributes"]["trackPresentation"] = json!("PREVIEW");
    doc["data"]["attributes"]["previewReason"] = json!("FULL_REQUIRES_SUBSCRIPTION");
    let v2 = HashMap::from([("FLAC".to_string(), (200, doc))]);
    let (svc, journal) = faux_tidal(v2, v1_du_defaut()).await;
    let flux = svc
        .get_track_url("61681502", Some("LOSSLESS"))
        .await
        .unwrap();
    assert_eq!(flux.url, URL_AAC_V1);
    assert!(!journal.lock().unwrap().v1.is_empty());
}

#[test]
fn un_flux_chiffre_ou_une_uri_https_est_refuse_6334() {
    let mut doc = document_v2(&["FLAC"], "flac", 44_100, URL_FLAC_V2);
    doc["data"]["attributes"]["drmData"] = json!({"drmSystem": "WIDEVINE"});
    assert!(interpreter_manifeste_v2(&doc, "FLAC").is_err());
    let mut doc = document_v2(&["FLAC"], "flac", 44_100, URL_FLAC_V2);
    doc["data"]["attributes"]["uri"] = json!("https://cdn.essai.invalid/m.mpd");
    assert!(interpreter_manifeste_v2(&doc, "FLAC").is_err());
    assert!(interpreter_manifeste_v2(&json!({"errors": []}), "FLAC").is_err());
}

#[test]
fn la_reponse_v2_porte_sa_voie_6334() {
    let doc = document_v2(&["FLAC"], "flac", 44_100, URL_FLAC_V2);
    let r = interpreter_manifeste_v2(&doc, "FLAC").unwrap();
    assert_eq!(r["voie"], VOIE_V2);
    assert_eq!(r["audioQuality"], "LOSSLESS");
    assert_eq!(r["manifestMimeType"], "application/dash+xml");
}
