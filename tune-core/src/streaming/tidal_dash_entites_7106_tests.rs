//! #7106 — depuis la rc8 (#6334, lecture par `trackManifests` v2), « all 49
//! DASH segments failed » sur toute lecture TIDAL.
//!
//! Le MPD de la v2 porte des URL signées à la manière de CloudFront :
//! `…/0.mp4?Policy=…&amp;Signature=…&amp;Key-Pair-Id=…`, le `&` écrit `&amp;`
//! comme le veut le XML. Le lecteur du MPD gardait la valeur BRUTE des
//! attributs : l'URL partait au CDN avec `amp;Signature=`, sans signature,
//! et l'init comme chacun des segments étaient refusés (403).
//!
//! Le MPD ci-dessous a la forme d'un MPD v2 réel (une `SegmentTemplate`,
//! `timescale="44100"`, `startNumber="1"`, une `SegmentTimeline` de 49
//! segments) ; les valeurs signées sont fictives. Un faux CDN sur la boucle
//! locale refuse, comme le vrai, une requête dont la signature manque.

use super::manifeste_v2::interpreter_manifeste_v2;
use super::*;
use base64::Engine as _;

const POLICY: &str = "eyJTdGF0ZW1lbnQiOiBbeyJSZXNvdXJjZSI6ZXNzYWk";
const SIGNATURE: &str = "vX~upEssai-Signature_7106";
const KEY_PAIR: &str = "KESSAI7106";
const CHEMIN: &str = "mediatracks/GisIAxInOWY0MTk5YzU0ZDJjZDhjNmQ3ZmYwNWFlMWU5ZjMzNGVf";

/// Le MPD v2 tel que TIDAL l'écrit : `&amp;` dans les attributs.
fn mpd_v2(base: &str) -> String {
    let requete = format!("Policy={POLICY}&amp;Signature={SIGNATURE}&amp;Key-Pair-Id={KEY_PAIR}");
    format!(
        r#"<?xml version='1.0' encoding='UTF-8'?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="urn:mpeg:dash:profile:isoff-main:2011" type="static" minBufferTime="PT3.993S" mediaPresentationDuration="PT3M18.248S">
  <Period id="0">
    <AdaptationSet id="0" contentType="audio" mimeType="audio/mp4" segmentAlignment="true">
      <Representation id="0" codecs="flac" bandwidth="891264" audioSamplingRate="44100">
        <SegmentTemplate timescale="44100" initialization="{base}/{CHEMIN}/0.mp4?{requete}" media="{base}/{CHEMIN}/$Number$.mp4?{requete}" startNumber="1">
          <SegmentTimeline>
            <S d="176128" r="47"/>
            <S d="137860"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#
    )
}

/// Le document `trackManifests` qui porte ce MPD en URI `data:`.
fn document_v2(mpd: &str) -> serde_json::Value {
    json!({"data": {"id": "7106", "type": "trackManifests", "attributes": {
        "trackPresentation": "FULL",
        "uri": format!(
            "data:application/dash+xml;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(mpd)
        ),
        "formats": ["FLAC"],
        "drmData": null
    }}})
}

/// Le MPD v2, passé par la même voie que la lecture : document v2 ramené à
/// la forme v1, puis manifeste relu.
fn segments_du_v2(base: &str) -> DashSegmentInfo {
    let reponse = interpreter_manifeste_v2(&document_v2(&mpd_v2(base)), "FLAC")
        .expect("le document v2 est lisible");
    let mpd =
        String::from_utf8(base64_decode(reponse["manifest"].as_str().unwrap()).unwrap()).unwrap();
    parse_dash_manifest(&mpd)
        .expect("MPD lisible")
        .segments
        .expect("SegmentTemplate + SegmentTimeline")
}

#[test]
fn les_url_signees_du_mpd_v2_partent_desechappees_7106() {
    let seg = segments_du_v2("https://sp-ad-cf.audio.tidal.com");
    let requete = format!("?Policy={POLICY}&Signature={SIGNATURE}&Key-Pair-Id={KEY_PAIR}");
    assert_eq!(
        seg.segment_count, 49,
        "48 + 1 segments de la SegmentTimeline"
    );
    assert_eq!(seg.start_number, 1);
    assert!(
        !seg.init_url.contains("&amp;") && !seg.media_template.contains("&amp;"),
        "aucune entité XML dans les URL envoyées au CDN : {} / {}",
        seg.init_url,
        seg.media_template
    );
    assert!(
        seg.init_url.ends_with(&format!("/0.mp4{requete}")),
        "{}",
        seg.init_url
    );
    assert!(
        seg.media_template
            .ends_with(&format!("/$Number$.mp4{requete}")),
        "{}",
        seg.media_template
    );
}

#[test]
fn une_base_url_signee_garde_toute_sa_requete_7106() {
    // quick-xml rend `&amp;` d'un texte comme un évènement séparé : la
    // BaseURL ne gardait que le morceau après le dernier `&`.
    let mpd = format!(
        r#"<MPD><Period><AdaptationSet mimeType="audio/mp4"><Representation codecs="flac">
<BaseURL>https://sp-ad-cf.audio.tidal.com/{CHEMIN}/0.flac?Policy={POLICY}&amp;Signature={SIGNATURE}&amp;Key-Pair-Id={KEY_PAIR}</BaseURL>
</Representation></AdaptationSet></Period></MPD>"#
    );
    let dash = parse_dash_manifest(&mpd).expect("MPD lisible");
    assert_eq!(
        dash.url,
        format!(
            "https://sp-ad-cf.audio.tidal.com/{CHEMIN}/0.flac?Policy={POLICY}&Signature={SIGNATURE}&Key-Pair-Id={KEY_PAIR}"
        )
    );
}

#[test]
fn une_url_sans_entite_est_inchangee_7106() {
    // Les manifestes v1 (jeton dans le chemin) ne changent pas.
    let mpd = r#"<MPD><Period><AdaptationSet><Representation codecs="flac">
<SegmentTemplate initialization="https://sp-pr-fa.audio.tidal.com/mediatracks/CAEa/0.mp4" media="https://sp-pr-fa.audio.tidal.com/mediatracks/CAEa/$Number$.mp4" startNumber="1">
<SegmentTimeline><S d="176128" r="2"/></SegmentTimeline></SegmentTemplate>
</Representation></AdaptationSet></Period></MPD>"#;
    let seg = parse_dash_manifest(mpd).unwrap().segments.unwrap();
    assert_eq!(
        seg.init_url,
        "https://sp-pr-fa.audio.tidal.com/mediatracks/CAEa/0.mp4"
    );
    assert_eq!(
        seg.media_template,
        "https://sp-pr-fa.audio.tidal.com/mediatracks/CAEa/$Number$.mp4"
    );
    assert_eq!(seg.segment_count, 3);
}

/// Un faux CDN qui, comme CloudFront, refuse (403) une requête dont
/// `Policy`, `Signature` ou `Key-Pair-Id` manque ou diffère.
async fn faux_cdn() -> String {
    use axum::extract::{Path, Query};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use std::collections::HashMap;

    async fn segment(
        Path((_, fichier)): Path<(String, String)>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        let signe = q.get("Policy").map(String::as_str) == Some(POLICY)
            && q.get("Signature").map(String::as_str) == Some(SIGNATURE)
            && q.get("Key-Pair-Id").map(String::as_str) == Some(KEY_PAIR);
        if !signe {
            return (
                StatusCode::FORBIDDEN,
                "<?xml version=\"1.0\"?><Error><Code>MissingKey</Code></Error>",
            )
                .into_response();
        }
        // Un octet par segment, son numéro : l'ordre se vérifie à l'arrivée.
        let numero: u32 = fichier.trim_end_matches(".mp4").parse().unwrap_or(999);
        if numero == 0 {
            b"\0\0\0\x18ftypiso6essai-init".to_vec().into_response()
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
async fn les_49_segments_du_mpd_v2_se_telechargent_7106() {
    let base = faux_cdn().await;
    let seg = segments_du_v2(&base);
    let svc = TidalService::new();
    let chemin = svc
        .download_dash_segments(&seg, "7106")
        .await
        .expect("init et segments acceptés par le CDN");
    let octets = std::fs::read(&chemin).unwrap();
    let _ = std::fs::remove_file(&chemin);
    let init = b"\0\0\0\x18ftypiso6essai-init";
    assert_eq!(&octets[..init.len()], init, "l'init ouvre le fichier");
    let attendus: Vec<u8> = (1..=49u8).collect();
    assert_eq!(
        &octets[init.len()..],
        &attendus[..],
        "49 segments, dans l'ordre"
    );
}

#[tokio::test]
async fn contre_epreuve_l_url_brute_est_refusee_par_le_cdn_7106() {
    // Ce que la rc8 envoyait : la valeur brute de l'attribut, `&amp;` compris.
    // Le faux CDN la refuse, comme le vrai : le test précédent mesure bien la
    // signature, pas un serveur qui accepte tout.
    let base = faux_cdn().await;
    let brute = format!(
        "{base}/{CHEMIN}/1.mp4?Policy={POLICY}&amp;Signature={SIGNATURE}&amp;Key-Pair-Id={KEY_PAIR}"
    );
    let statut = crate::http::client::shared()
        .get(&brute)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(statut, 403, "l'URL non désechappée perd sa signature");

    // Et un init refusé est une erreur dite comme telle, pas un corps d'erreur
    // écrit en tête du fichier fMP4 suivi de « all N segments failed ».
    let mut seg = segments_du_v2(&base);
    seg.init_url = brute.replace("/1.mp4", "/0.mp4");
    let erreur = TidalService::new()
        .download_dash_segments(&seg, "7106")
        .await
        .expect_err("init refusé");
    assert!(erreur.contains("dash init download: HTTP 403"), "{erreur}");
}
