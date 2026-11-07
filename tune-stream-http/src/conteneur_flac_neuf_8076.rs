//! #8076 — la requête de l'Eversolo DMP-A8 (`Lavf/58.45.100`,
//! `Range: bytes=0-`, `Icy-MetaData: 1`), faite DEUX fois de suite comme au
//! .18, sur un FLAC servi sous son en-tête neuf (#4800) : `206` avec
//! `Content-Range` et `Content-Length` exacts, aucun bloc ICY glissé dans les
//! trames, les deux réponses identiques, et le corps décodé rend exactement
//! les échantillons du fichier source.
use super::*;
use axum::extract::{Path, State};
use futures_util::StreamExt;
use std::sync::Arc;
use tune_core::http::streamer::SharedSessions;

const FLAC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tune-core/tests/fixtures/test.flac"
);

async fn demander(
    sessions: &SharedSessions,
    id: &str,
    range: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut h = HeaderMap::new();
    h.insert("User-Agent", "Lavf/58.45.100".parse().unwrap());
    h.insert("Icy-MetaData", "1".parse().unwrap());
    if let Some(r) = range {
        h.insert("Range", r.parse().unwrap());
    }
    let rep = handle_stream(Path(format!("{id}.flac")), State(sessions.clone()), h).await;
    let statut = rep.status();
    let entetes = rep.headers().clone();
    let mut flux = rep.into_body().into_data_stream();
    let mut corps = Vec::new();
    while let Some(m) = flux.next().await {
        corps.extend_from_slice(&m.unwrap());
    }
    (statut, entetes, corps)
}

fn entete(h: &HeaderMap, nom: &str) -> String {
    h.get(nom).unwrap().to_str().unwrap().to_string()
}

fn trames(chemin: &std::path::Path) -> (Vec<i32>, u64) {
    let d =
        tune_core::audio::decode::decode_to_pcm(&chemin.to_string_lossy(), None, None, 0.0, 0.0)
            .unwrap();
    let n = d.samples_i32.len() as u64 / u64::from(d.channels);
    (d.samples_i32, n)
}

#[tokio::test]
async fn la_double_requete_lavf_recoit_les_trames_exactes_du_fichier_8076() {
    let bac = tune_core::test_scratch::scratch_dir("tune-8076-http");
    let source = bac.join("piste.flac");
    let mut octets = std::fs::read(FLAC).unwrap();
    octets[26..42].fill(0); // `Lavf` + MD5 nul (#4350)
    std::fs::write(&source, &octets).unwrap();
    let carte = tune_core::audio::flac_vendeur::conteneur_neuf_pour_passthrough(&source)
        .expect("conteneur neuf");
    let total = carte.total;

    let id = "c8076".to_string();
    let info = StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        file_size: Some(total),
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new(id.clone(), info, false, 8));
    *session.file_path.lock().await = Some(source.to_string_lossy().into_owned());
    *session.faststart.lock().unwrap() = Some(carte.clone());
    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.clone(), session)].into_iter().collect(),
    ));

    let (s1, h1, c1) = demander(&sessions, &id, Some("bytes=0-")).await;
    let (s2, h2, c2) = demander(&sessions, &id, Some("bytes=0-")).await;
    for (s, h) in [(s1, &h1), (s2, &h2)] {
        assert_eq!(s, StatusCode::PARTIAL_CONTENT);
        assert_eq!(entete(h, "Content-Length"), total.to_string());
        assert_eq!(
            entete(h, "Content-Range"),
            format!("bytes 0-{}/{total}", total - 1)
        );
        assert_eq!(entete(h, "Accept-Ranges"), "bytes");
        assert!(h.get("icy-metaint").is_none(), "pas d'ICY dans un fichier");
    }
    assert_eq!(c1, c2, "la seconde requête rend les mêmes octets");
    let debut = usize::try_from(carte.body_src_start).unwrap();
    assert_eq!(c1.len() as u64, total);
    assert_eq!(&c1[..carte.header.len()], &carte.header[..]);
    assert_eq!(
        &c1[carte.header.len()..],
        &octets[debut..],
        "trames à l'octet près"
    );

    // Une tranche au milieu, comme un renderer qui reprend.
    let (s3, h3, c3) = demander(&sessions, &id, Some("bytes=1000-1999")).await;
    assert_eq!(s3, StatusCode::PARTIAL_CONTENT);
    assert_eq!(entete(&h3, "Content-Length"), "1000");
    assert_eq!(c3, c1[1000..2000]);

    // Décodé : les échantillons du fichier source, ni plus ni moins.
    let servi = bac.join("servi.flac");
    std::fs::write(&servi, &c1).unwrap();
    let mot = u64::from_be_bytes(octets[18..26].try_into().unwrap());
    let total_streaminfo = mot & ((1 << 36) - 1);
    let (pcm_source, n_source) = trames(&source);
    let (pcm_servi, n_servi) = trames(&servi);
    assert_eq!(n_source, total_streaminfo);
    assert_eq!(
        n_servi, total_streaminfo,
        "aucun échantillon perdu ni ajouté"
    );
    assert!(pcm_servi == pcm_source, "échantillons identiques");
}
