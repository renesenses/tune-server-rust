//! #7106 — le MPD v2 tel que TIDAL le sert RÉELLEMENT (relevé sur le .18 le
//! 26/10/2026, `trackManifests` v2, `formats=FLAC_HIRES` et `formats=FLAC`).
//!
//! Le premier banc (`tidal_dash_entites_7106_tests.rs`) supposait des URL
//! signées à la CloudFront (`Policy`, `Signature`, `Key-Pair-Id`). Le vrai MPD
//! porte deux paramètres seulement : `token=<échéance>~<base64>` et `info=…`,
//! séparés par UN `&amp;` (échappé une seule fois, pas de `&amp;amp;`), sur
//! l'hôte `sp-ad-fa.audio.tidal.com`. Le CDN réel rend 200 à l'URL
//! désechappée une fois, 403 à l'URL brute (`amp;info=`) : c'est ce que vit
//! un serveur rc8.
//!
//! Ci-dessous, la forme relevée à l'octet près pour la structure (espaces de
//! noms, `Role`, `AudioChannelConfiguration`, `id="FLAC_HIRES,96000,24"`,
//! SegmentTimeline de 65 + 1 segments) ; le chemin, le jeton et `info` sont
//! fictifs, de même longueur et du même alphabet (`~`, `-`, `==`).

use super::manifeste_v2::interpreter_manifeste_v2;
use super::*;
use base64::Engine as _;

/// Jeton fictif de la forme réelle : échéance en secondes, `~`, base64 avec
/// `-` et `==` final.
const JETON: &str = "1793000000~ZmljdGlmLTcxMDYtamV0b24tZGUtbGEtZm9ybWUtcmVlbGxlLWR1LTI2LTEwLWNkbi1zcC1hZC1mYS1hdWRpby10aWRhbC1jb20tbWVkaWF0cmFja3MtZmxhYy1oaXJlcy05NjAwMC0yNC1hbm9ueW1pc2UtcG91ci1sZS1iYW5jLWRlLXRlc3RzLXR1bmUtcmM5LQ-ZmluLQ==";
/// `info` fictif : 44 caractères alphanumériques, comme le vrai.
const INFO: &str = "U2FsdGVkX1Tune7106InfoFictifQuaranteQuatre0";
const CHEMIN: &str = "mediatracks/GisIAxInZmljdGlmNzEwNmNoZW1pbmRlbGFwaXN0ZTk2a2h6XzYyLm1wNCIbHQAAgEAgAioQFictif7106MgA";

/// Le MPD v2 réel (structure relevée), `&amp;` échappé une fois.
fn mpd_reel(base: &str) -> String {
    let requete = format!("token={JETON}&amp;info={INFO}");
    format!(
        concat!(
            "<?xml version='1.0' encoding='UTF-8'?>",
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:cenc="urn:mpeg:cenc:2013" xsi:schemaLocation="urn:mpeg:dash:schema:mpd:2011 DASH-MPD.xsd" profiles="urn:mpeg:dash:profile:isoff-main:2011" type="static" minBufferTime="PT3.968S" mediaPresentationDuration="PT4M18.816S">"#,
            r#"<Period id="0"><AdaptationSet id="0" contentType="audio" mimeType="audio/mp4" lang="und" group="main" segmentAlignment="true">"#,
            r#"<Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>"#,
            r#"<Representation id="FLAC_HIRES,96000,24" codecs="flac" bandwidth="2726352" audioSamplingRate="96000">"#,
            r#"<AudioChannelConfiguration schemeIdUri="urn:mpeg:dash:23003:3:audio_channel_configuration:2011" value="2"/>"#,
            r#"<SegmentTemplate timescale="96000" initialization="{base}/{chemin}/0.mp4?{requete}" media="{base}/{chemin}/$Number$.mp4?{requete}" startNumber="1">"#,
            r#"<SegmentTimeline><S d="380928" r="64"/><S d="86076"/></SegmentTimeline></SegmentTemplate>"#,
            r#"</Representation></AdaptationSet></Period></MPD>"#
        ),
        base = base,
        chemin = CHEMIN,
        requete = requete,
    )
}

/// Le document `trackManifests` réel qui porte ce MPD (`uriScheme=DATA`).
fn document_reel(mpd: &str) -> serde_json::Value {
    json!({"data": {"id": "68714461", "type": "trackManifests", "attributes": {
        "trackPresentation": "FULL",
        "uri": format!(
            "data:application/dash+xml;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(mpd)
        ),
        "formats": ["FLAC_HIRES"],
        "drmData": null
    }}})
}

/// Le chemin de la lecture : document v2 → forme v1 → MPD relu.
fn lecture_du_mpd_reel(base: &str) -> (serde_json::Value, DashSegmentInfo) {
    let reponse = interpreter_manifeste_v2(&document_reel(&mpd_reel(base)), "FLAC_HIRES")
        .expect("le document v2 réel est lisible");
    let mpd =
        String::from_utf8(base64_decode(reponse["manifest"].as_str().unwrap()).unwrap()).unwrap();
    let seg = parse_dash_manifest(&mpd)
        .expect("MPD réel lisible")
        .segments
        .expect("SegmentTemplate + SegmentTimeline");
    (reponse, seg)
}

fn parametres(url: &str) -> Vec<(String, String)> {
    reqwest::Url::parse(url)
        .expect("URL absolue")
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[test]
fn le_mpd_reel_part_avec_token_et_info_desechappes_7106() {
    let (reponse, seg) = lecture_du_mpd_reel("https://sp-ad-fa.audio.tidal.com");
    assert_eq!(reponse["audioQuality"], "HI_RES_LOSSLESS");
    assert_eq!(reponse["bitDepth"], 24);
    assert_eq!(reponse["sampleRate"], 96_000);
    assert_eq!(seg.segment_count, 66, "64 répétitions + 1, puis 1");
    assert_eq!(seg.start_number, 1);

    let attendus = vec![
        ("token".to_string(), JETON.to_string()),
        ("info".to_string(), INFO.to_string()),
    ];
    // Ce que voyait le .18 en rc8 : `token` et `amp;info`.
    assert_eq!(
        parametres(&seg.init_url),
        attendus,
        "init : exactement token puis info, sans `amp;` — {}",
        seg.init_url
    );
    assert_eq!(
        parametres(&seg.media_template.replace("$Number$", "1")),
        attendus,
        "segments : exactement token puis info, sans `amp;`"
    );
    assert_eq!(
        seg.init_url,
        format!("https://sp-ad-fa.audio.tidal.com/{CHEMIN}/0.mp4?token={JETON}&info={INFO}")
    );
    assert!(!seg.media_template.contains("&amp;"));
}

/// Un faux CDN qui, comme `sp-ad-fa.audio.tidal.com`, refuse (403) une
/// requête dont `token` ou `info` manque ou diffère.
async fn faux_cdn_token_info() -> String {
    use axum::extract::{Path, Query};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use std::collections::HashMap;

    async fn segment(
        Path((_, fichier)): Path<(String, String)>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        let signe = q.len() == 2
            && q.get("token").map(String::as_str) == Some(JETON)
            && q.get("info").map(String::as_str) == Some(INFO);
        if !signe {
            return (StatusCode::FORBIDDEN, "Forbidden").into_response();
        }
        let numero: u32 = fichier.trim_end_matches(".mp4").parse().unwrap_or(999);
        if numero == 0 {
            b"\0\0\0\x18ftypiso6reel-7106".to_vec().into_response()
        } else {
            vec![numero as u8].into_response()
        }
    }

    let app = axum::Router::new().route("/mediatracks/{piste}/{fichier}", get(segment));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("http://{adresse}")
}

#[tokio::test]
async fn les_66_segments_du_mpd_reel_se_telechargent_7106() {
    let base = faux_cdn_token_info().await;
    let (_, seg) = lecture_du_mpd_reel(&base);
    let chemin = TidalService::new()
        .download_dash_segments(&seg, "68714461")
        .await
        .expect("init et segments acceptés par le CDN");
    let octets = std::fs::read(&chemin).unwrap();
    let _ = std::fs::remove_file(&chemin);
    let init = b"\0\0\0\x18ftypiso6reel-7106";
    assert_eq!(&octets[..init.len()], init, "l'init ouvre le fichier");
    let attendus: Vec<u8> = (1..=66u8).collect();
    assert_eq!(
        &octets[init.len()..],
        &attendus[..],
        "les 66 segments, tous acceptés, dans l'ordre"
    );
}

#[tokio::test]
async fn contre_epreuve_l_url_brute_du_mpd_reel_est_refusee_7106() {
    // L'URL telle qu'écrite dans le MPD (`&amp;info=`), celle qu'envoie la
    // rc8 : le faux CDN la refuse comme le vrai l'a refusée le 26/10.
    let base = faux_cdn_token_info().await;
    let brute = format!("{base}/{CHEMIN}/0.mp4?token={JETON}&amp;info={INFO}");
    let statut = crate::http::client::shared()
        .get(&brute)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(statut, 403, "`amp;info` n'est pas `info`");
}
