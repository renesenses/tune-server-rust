//! #6334 — ce que Tidal livre vraiment, lu dans ses réponses factices.
//!
//! Un serveur sur la boucle locale joue `playbackinfopostpaywall` : pour
//! chaque `audioquality` demandée, il rend la réponse inscrite au scénario
//! (qualité rendue, manifeste BTS ou DASH) ou un 429. Aucun appel à Tidal,
//! aucun jeton réel : le service est armé d'un jeton d'essai.

use super::*;
use base64::Engine as _;
use std::collections::HashMap;

/// Ce que le faux Tidal rend pour une qualité demandée.
#[derive(Clone)]
enum Reponse {
    /// `audioQuality` rendue, et le manifeste (déjà décodé) avec son type.
    Lecture {
        audio_quality: &'static str,
        manifest_mime: &'static str,
        manifeste: String,
        bit_depth: Option<u64>,
        sample_rate: Option<u64>,
    },
    /// Le refus de débit : `fetch_playback_info` le rend en erreur.
    Quota,
}

/// Un manifeste BTS tel que Tidal le rend (JSON, avant base64).
fn bts(mime: &str, codecs: &str, url: &str) -> String {
    json!({
        "mimeType": mime,
        "codecs": codecs,
        "encryptionType": "NONE",
        "urls": [url],
    })
    .to_string()
}

fn lecture_bts(audio_quality: &'static str, mime: &str, codecs: &str, url: &str) -> Reponse {
    let (bit_depth, sample_rate) = match audio_quality {
        "LOSSLESS" => (Some(16), Some(44_100)),
        "HI_RES_LOSSLESS" => (Some(24), Some(96_000)),
        _ => (None, None),
    };
    Reponse::Lecture {
        audio_quality,
        manifest_mime: "application/vnd.tidal.bts",
        manifeste: bts(mime, codecs, url),
        bit_depth,
        sample_rate,
    }
}

const URL_AAC: &str = "https://cdn.essai.invalid/aac-320.m4a";
const URL_FLAC: &str = "https://cdn.essai.invalid/flac-16-44.flac";

/// Lance le faux Tidal sur le scénario donné (qualité demandée → réponse) et
/// rend un service pointé dessus, ainsi que le journal des qualités demandées.
async fn faux_tidal(
    scenario: HashMap<&'static str, Reponse>,
) -> (TidalService, Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    type Etat = (
        Arc<HashMap<&'static str, Reponse>>,
        Arc<std::sync::Mutex<Vec<String>>>,
    );

    async fn lecture(
        State((scenario, journal)): State<Etat>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        let demandee = q.get("audioquality").cloned().unwrap_or_default();
        journal.lock().unwrap().push(demandee.clone());
        match scenario.get(demandee.as_str()) {
            Some(Reponse::Lecture {
                audio_quality,
                manifest_mime,
                manifeste,
                bit_depth,
                sample_rate,
            }) => {
                let mut corps = json!({
                    "trackId": 29113109,
                    "assetPresentation": "FULL",
                    "audioMode": "STEREO",
                    "audioQuality": audio_quality,
                    "manifestMimeType": manifest_mime,
                    "manifest": base64::engine::general_purpose::STANDARD.encode(manifeste),
                });
                if let Some(b) = bit_depth {
                    corps["bitDepth"] = json!(b);
                }
                if let Some(s) = sample_rate {
                    corps["sampleRate"] = json!(s);
                }
                axum::Json(corps).into_response()
            }
            Some(Reponse::Quota) | None => (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(json!({"status": 429, "userMessage": "Rate limit exceeded"})),
            )
                .into_response(),
        }
    }

    let journal: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let app = axum::Router::new()
        .route("/tracks/{id}/playbackinfopostpaywall", get(lecture))
        .with_state((Arc::new(scenario), journal.clone()));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });

    (
        TidalService::pour_un_faux_tidal(format!("http://{adresse}")),
        journal,
    )
}

/// Défaut 1 : la cascade gardait le PREMIER repli dégradé. `HI_RES_LOSSLESS`
/// rend `HIGH` (mis de côté), `HI_RES` rend `LOSSLESS` (écarté : la place
/// était prise), `LOSSLESS` échoue sur un 429, `HIGH` rend `HIGH` : Tune
/// servait l'AAC alors qu'il avait reçu du FLAC. Corrigé, le FLAC est gardé
/// et la cascade s'arrête là : aucune marche plus basse ne peut mieux faire.
#[tokio::test]
async fn la_cascade_garde_le_meilleur_repli_et_non_le_premier() {
    let scenario = HashMap::from([
        (
            "HI_RES_LOSSLESS",
            lecture_bts("HIGH", "audio/mp4", "mp4a.40.2", URL_AAC),
        ),
        (
            "HI_RES",
            lecture_bts("LOSSLESS", "audio/flac", "flac", URL_FLAC),
        ),
        ("LOSSLESS", Reponse::Quota),
        (
            "HIGH",
            lecture_bts("HIGH", "audio/mp4", "mp4a.40.2", URL_AAC),
        ),
    ]);
    let (svc, journal) = faux_tidal(scenario).await;

    let flux = svc
        .get_track_url("29113109", Some("HI_RES_LOSSLESS"))
        .await
        .expect("un flux");

    assert_eq!(
        flux.url, URL_FLAC,
        "le FLAC rendu à la marche HI_RES doit l'emporter sur l'AAC de la première marche"
    );
    assert_eq!(flux.quality.codec, "FLAC");
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        ["HI_RES_LOSSLESS", "HI_RES"],
        "un LOSSLESS en main : rien à demander plus bas"
    );
}

/// Sans aucun FLAC dans la cascade, l'AAC reste servi : le correctif ne
/// fabrique pas de meilleure qualité, il garde la meilleure reçue.
#[tokio::test]
async fn sans_flac_dans_la_cascade_l_aac_reste_servi() {
    let aac = || lecture_bts("HIGH", "audio/mp4", "mp4a.40.2", URL_AAC);
    let scenario = HashMap::from([
        ("HI_RES_LOSSLESS", aac()),
        ("HI_RES", aac()),
        ("LOSSLESS", aac()),
        ("HIGH", aac()),
    ]);
    let (svc, journal) = faux_tidal(scenario).await;
    let flux = svc
        .get_track_url("29113110", Some("HI_RES_LOSSLESS"))
        .await
        .expect("un flux");
    assert_eq!(flux.url, URL_AAC);
    assert_eq!(flux.quality.codec, "AAC");
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        ["HI_RES_LOSSLESS", "HI_RES", "LOSSLESS"],
        "le HIGH déjà reçu dispense de la marche HIGH"
    );
}

/// À rang égal, rien ne change : la première réponse reste.
#[test]
fn a_rang_egal_le_premier_repli_reste() {
    let mut repli = None;
    garder_le_meilleur_repli(&mut repli, json!({"audioQuality": "HIGH", "n": 1}));
    garder_le_meilleur_repli(&mut repli, json!({"audioQuality": "HIGH", "n": 2}));
    assert_eq!(repli.unwrap()["n"], 1);
}

/// Défaut 2 : le codec était déduit de `audioQuality`, jamais lu dans le
/// manifeste. Une réponse étiquetée `LOSSLESS` dont le manifeste BTS livre de
/// l'AAC était annoncée FLAC ; c'est le manifeste qui fait foi.
#[tokio::test]
async fn le_codec_est_lu_dans_le_manifeste_bts_et_non_deduit_de_la_qualite() {
    let scenario = HashMap::from([(
        "LOSSLESS",
        lecture_bts("LOSSLESS", "audio/mp4", "mp4a.40.2", URL_AAC),
    )]);
    let (svc, _) = faux_tidal(scenario).await;
    let flux = svc
        .get_track_url("29113109", Some("LOSSLESS"))
        .await
        .expect("un flux");
    assert_eq!(
        flux.quality.codec, "AAC",
        "le manifeste BTS nomme mp4a.40.2 : c'est de l'AAC, quoi qu'en dise audioQuality"
    );
    assert_eq!(flux.mime_type, "audio/mp4");
}

/// Le MIME d'un flux BTS est celui des octets audio (`mimeType` du
/// manifeste), jamais `application/vnd.tidal.bts`, le type du manifeste.
#[tokio::test]
async fn le_mime_d_un_flux_bts_est_celui_de_l_audio() {
    let scenario = HashMap::from([
        (
            "LOSSLESS",
            lecture_bts("HIGH", "audio/mp4", "mp4a.40.2", URL_AAC),
        ),
        (
            "HIGH",
            lecture_bts("HIGH", "audio/mp4", "mp4a.40.2", URL_AAC),
        ),
    ]);
    let (svc, _) = faux_tidal(scenario).await;
    let flux = svc
        .get_track_url("29113110", Some("LOSSLESS"))
        .await
        .expect("un flux");
    assert_eq!(flux.quality.codec, "AAC");
    assert_eq!(flux.mime_type, "audio/mp4");
}

/// Le journal de la cascade nomme le codec du manifeste (BTS et DASH) ; le
/// champ `codec` qu'il lisait à la racine n'existe pas.
#[test]
fn le_codec_annonce_est_celui_du_manifeste() {
    let encode = |t: &str| base64::engine::general_purpose::STANDARD.encode(t);
    let reponse_bts = json!({
        "manifestMimeType": "application/vnd.tidal.bts",
        "manifest": encode(&bts("audio/mp4", "mp4a.40.2", URL_AAC)),
    });
    assert_eq!(
        codec_annonce_par_le_manifeste(&reponse_bts).as_deref(),
        Some("mp4a.40.2")
    );
    let mpd = r#"<MPD><Period><AdaptationSet mimeType="audio/mp4">
      <Representation codecs="flac" bandwidth="1411000">
      <BaseURL>https://cdn.essai.invalid/flac.mp4</BaseURL>
      </Representation></AdaptationSet></Period></MPD>"#;
    let reponse_dash = json!({
        "manifestMimeType": "application/dash+xml",
        "manifest": encode(mpd),
    });
    assert_eq!(
        codec_annonce_par_le_manifeste(&reponse_dash).as_deref(),
        Some("flac")
    );
}

/// Défaut 3 : un AAC était étiqueté 16 bits / 44,1 kHz, comme un FLAC CD
/// (`parse_quality_metadata` retombait sur `(44100, 16)` sans `bitDepth`).
/// Un AAC n'a pas de profondeur : 0 ; son débit est celui que nomme `HIGH`.
#[tokio::test]
async fn un_aac_n_a_pas_de_profondeur_de_bits_et_porte_son_debit() {
    let scenario = HashMap::from([(
        "HIGH",
        lecture_bts("HIGH", "audio/mp4", "mp4a.40.2", URL_AAC),
    )]);
    let (svc, _) = faux_tidal(scenario).await;
    let flux = svc
        .get_track_url("29113109", Some("HIGH"))
        .await
        .expect("un flux");
    assert_eq!(flux.quality.codec, "AAC");
    assert_eq!(
        flux.quality.bit_depth, 0,
        "un AAC n'a pas de profondeur : 16 bits est celle d'un FLAC CD"
    );
    assert_eq!(flux.quality.sample_rate, 44_100);
    assert_eq!(flux.quality.bitrate, Some(320_000));
    assert!(flux.quality.avec_perte());
}

/// Un FLAC CD garde ses 16 bits / 44,1 kHz : seul l'AAC perd sa profondeur.
#[tokio::test]
async fn un_flac_cd_garde_ses_16_bits() {
    let scenario = HashMap::from([(
        "LOSSLESS",
        lecture_bts("LOSSLESS", "audio/flac", "flac", URL_FLAC),
    )]);
    let (svc, _) = faux_tidal(scenario).await;
    let flux = svc
        .get_track_url("29113109", Some("LOSSLESS"))
        .await
        .expect("un flux");
    assert_eq!(flux.quality.codec, "FLAC");
    assert_eq!(
        (flux.quality.sample_rate, flux.quality.bit_depth),
        (44_100, 16)
    );
}

/// Un AAC livré sous une étiquette sans perte n'hérite d'aucun débit inventé.
#[test]
fn le_debit_aac_n_est_nomme_que_par_high_et_low() {
    assert_eq!(debit_aac_de_tidal("HIGH"), Some(320_000));
    assert_eq!(debit_aac_de_tidal("LOW"), Some(96_000));
    assert_eq!(debit_aac_de_tidal("LOSSLESS"), None);
}
