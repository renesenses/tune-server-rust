//! #8052 — le `GetProtocolInfo` de la façade dépend de SA zone : une zone à
//! sortie locale en « DoP » ou « natif » annonce le DSD, les autres non.
//!
//! Sabotage qui fait rougir ce témoin : remettre dans
//! `connection_manager_control` l'appel à
//! `build_renderer_connection_manager_response(&body)`, qui ignore la zone.

use super::*;

const SOAP_GET_PROTOCOL_INFO: &str = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body><u:GetProtocolInfo xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"/></s:Body>
</s:Envelope>"#;

async fn protocol_info(state: &AppState, zone_id: i64) -> String {
    let reponse = connection_manager_control(
        State(state.clone()),
        Path(zone_id),
        SOAP_GET_PROTOCOL_INFO.to_string(),
    )
    .await;
    assert_eq!(reponse.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&octets).to_string()
}

#[tokio::test]
async fn le_sink_de_la_facade_suit_la_sortie_et_le_mode_dsd_de_la_zone() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zones = ZoneRepo::with_backend(state.backend.clone());
    let locale = zones
        .create("Gustard X16", Some("local"), Some("local:Gustard X16"))
        .unwrap();
    let reseau = zones
        .create("Eversolo", Some("dlna"), Some("dlna:uuid-eversolo"))
        .unwrap();

    for mode in ["dop", "native"] {
        zones.update_dsd_mode(locale, mode).unwrap();
        let xml = protocol_info(&state, locale).await;
        assert!(
            xml.contains("http-get:*:application/x-dsd:*"),
            "[{mode}] la façade d'une sortie locale qui joue le DSD doit l'annoncer, \
             sinon le serveur qui lui prête la lecture le convertit en PCM (#8052) :\n{xml}"
        );
        assert!(xml.contains("http-get:*:audio/flac:*"), "[{mode}]");
    }

    for mode in ["auto", "pcm"] {
        zones.update_dsd_mode(locale, mode).unwrap();
        let xml = protocol_info(&state, locale).await;
        assert!(!xml.contains("dsd"), "[{mode}] pas de DSD annoncé :\n{xml}");
    }

    // Une sortie réseau prêtée relève de #8053 : pas de DSD annoncé ici.
    zones.update_dsd_mode(reseau, "dop").unwrap();
    let xml = protocol_info(&state, reseau).await;
    assert!(!xml.contains("dsd"), "sortie réseau :\n{xml}");

    // Une zone inconnue n'annonce rien de plus que le Sink d'avant.
    let xml = protocol_info(&state, 999_999).await;
    assert!(!xml.contains("dsd"), "zone inconnue :\n{xml}");
}
