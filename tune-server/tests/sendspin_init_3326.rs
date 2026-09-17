//! Echecs init publics, ordre normatif et silence une fois Noise commence.
use super::*;
use serde_json::json;
use std::time::Duration;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ferme_sans_message(ws: &mut Socket) {
    let recu = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("le serveur doit fermer apres le refus");
    match recu {
        None | Some(Ok(Message::Close(_))) | Some(Err(_)) => {}
        autre => panic!("aucun autre message ne doit suivre le refus : {autre:?}"),
    }
}

async fn refus(url: &str, entree: String, raison: &str) {
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws.send(Message::Text(entree.clone().into())).await.unwrap();
    let recu = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("server/error doit preceder la fermeture");
    let Some(Ok(Message::Text(texte))) = recu else {
        panic!("un echec init exige server/error avant fermeture : {entree}, recu {recu:?}");
    };
    let v: serde_json::Value = serde_json::from_str(&texte).unwrap();
    assert_eq!(
        v,
        json!({"type":"server/error","payload":{"reason":raison}}),
        "l'ordre enveloppe/version/suite/identite doit determiner le refus : {entree}"
    );
    ferme_sans_message(&mut ws).await;
}

#[tokio::test]
async fn i3326_init_malforme_annonce_son_refus_puis_ferme() {
    let url = point_d_acces().await;
    for entree in [
        "pas du JSON".to_owned(),
        "null".to_owned(),
        "[]".to_owned(),
        json!({}).to_string(),
        json!({"type":false,"payload":{"version":2}}).to_string(),
        json!({"type":"client/time","payload":{"version":2}}).to_string(),
        json!({"type":"client/init","payload":[]}).to_string(),
        json!({"type":"client/init","payload":{}}).to_string(),
    ] {
        refus(&url, entree, "malformed").await;
    }
    for version in [json!(null), json!(true), json!("2"), json!(1.5)] {
        refus(
            &url,
            json!({"type":"client/init","payload":{"version":version}}).to_string(),
            "malformed",
        )
        .await;
    }
    for charge in [
        json!({"version":1}),
        json!({"version":1,"suite":null}),
        json!({"version":1,"suite":42}),
        json!({"version":1,"suite":Suite::ChaChaPoly.nom()}),
        json!({"version":1,"suite":Suite::ChaChaPoly.nom(),"client_id":false}),
        json!({"version":1,"suite":Suite::ChaChaPoly.nom(),"client_id":"invalide"}),
        json!({"version":1,"suite":Suite::ChaChaPoly.nom(),"client_id":URL_SAFE_NO_PAD.encode([0;32])}),
    ] {
        refus(
            &url,
            json!({"type":"client/init","payload":charge}).to_string(),
            "malformed",
        )
        .await;
    }
}

#[tokio::test]
async fn i3326_init_version_prioritaire_sur_suite_et_identite() {
    let url = point_d_acces().await;
    for version in [
        json!(2),
        json!(-1),
        json!(4294967296u64),
        json!(i64::MIN),
        json!(u64::MAX),
    ] {
        for autres in [false, true] {
            let mut charge = json!({"version":version});
            if autres {
                charge["suite"] = json!(false);
                charge["client_id"] = json!([]);
            }
            refus(
                &url,
                json!({"type":"client/init","payload":charge}).to_string(),
                "unsupported_version",
            )
            .await;
        }
    }
}

#[tokio::test]
async fn i3326_init_suite_prioritaire_sur_identite() {
    let url = point_d_acces().await;
    for suite in ["", "25519_AESGCM_SHA512", "future_suite"] {
        refus(
            &url,
            json!({"type":"client/init","payload":{"version":1,"suite":suite}}).to_string(),
            "unsupported_suite",
        )
        .await;
    }
}

#[tokio::test]
async fn i3326_init_noise_malforme_ferme_sans_server_error() {
    let url = point_d_acces().await;
    for suite in Suite::toutes() {
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let init = json!({"type":"client/init","payload":{
            "version":1,"suite":suite.nom(),"client_id":Identite::generer().id()}});
        ws.send(Message::Text(init.to_string().into()))
            .await
            .unwrap();
        for attendu in ["server/init", "noise/handshake"] {
            let recu = tokio::time::timeout(Duration::from_secs(2), ws.next())
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&texte(recu)).unwrap();
            assert_eq!(v["type"], attendu);
        }
        ws.send(Message::Text(
            json!({"type":"noise/handshake","payload":{"data":"."}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        ferme_sans_message(&mut ws).await;
    }
}
