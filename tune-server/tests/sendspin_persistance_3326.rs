//! Included by sendspin_point_d_acces_s2a (autotests=false).
use super::*;
use std::path::Path;
use tune_core::sendspin::magasin::{ErreurMagasin, MagasinAppairage, MethodeAppairage};
use tune_core::sendspin::psk::{CategoriePsk, PskPair};
use tune_server::routes::sendspin::{ContexteSendspin, router};

async fn servir(
    dossier: &Path,
    mode: tune_core::sendspin::ModeTransition,
) -> (String, tokio::task::JoinHandle<()>) {
    let contexte = ContexteSendspin::nouveau(dossier.to_owned());
    let app = axum::Router::new().nest("/sendspin", router::<()>(mode, contexte));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let tache = tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (format!("ws://{adresse}/sendspin"), tache)
}

async fn arreter(tache: tokio::task::JoinHandle<()>, dossier: &Path) {
    tache.abort(); // cette fixture seulement, jamais un processus externe
    let _ = tache.await;
    // Une tache WebSocket peut encore finir son Close. Attendre la liberation
    // effective du magasin avant de simuler un nouveau serveur.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match MagasinAppairage::ouvrir(dossier) {
                Ok(m) => {
                    drop(m);
                    break;
                }
                Err(ErreurMagasin::Occupe) => tokio::task::yield_now().await,
                Err(e) => panic!("magasin apres arret : {e}"),
            }
        }
    })
    .await
    .expect("l'ancien serveur doit liberer son magasin");
}

#[tokio::test]
async fn i3326_le_point_d_acces_recharge_l_identite_et_la_psk_longue_duree() {
    for suite in Suite::toutes() {
        let t = tempfile::tempdir().unwrap();
        let dossier = t.path().join("sendspin");
        let client = Identite::generer();
        let psk = PskPair::pour_pair(&client.id(), [29; 32], CategoriePsk::LongueDuree).unwrap();
        let mut magasin = MagasinAppairage::ouvrir(&dossier).unwrap();
        let attendu = magasin.identite().id();
        magasin
            .conserver(&client.id(), &psk, MethodeAppairage::Psk)
            .unwrap();
        drop(magasin);

        for _ in 0..2 {
            let (url, tache) = servir(
                &dossier,
                tune_core::sendspin::ModeTransition::ChiffrementSeul,
            )
            .await;
            let (id, activate) =
                conversation_avec_cle(&url, &client, suite, "Pair durable", psk.secret(), &psk)
                    .await;
            assert_eq!(
                id, attendu,
                "le vrai point d'acces doit garder son identite apres redemarrage"
            );
            let activate: serde_json::Value = serde_json::from_str(&activate).unwrap();
            assert_eq!(
                activate["payload"]["activities"],
                serde_json::json!([]),
                "S2-b ne joue pas encore"
            );
            let pair = tune_core::sendspin::registre::decrire()
                .into_iter()
                .find(|p| p["client_id"] == client.id())
                .unwrap();
            assert_eq!(
                pair["authenticated"], true,
                "LT verifiee doit etre distinguee de la sentinelle"
            );
            assert_eq!(pair["psk_category"], "lt");
            assert_eq!(pair["credential_mismatch"], false);
            arreter(tache, &dossier).await;
        }
    }
}

#[tokio::test]
async fn i3326_un_client_ayant_perdu_sa_cle_reste_non_appaire_et_le_record_survit() {
    let t = tempfile::tempdir().unwrap();
    let dossier = t.path().join("sendspin");
    let client = Identite::generer();
    let psk = PskPair::pour_pair(&client.id(), [29; 32], CategoriePsk::LongueDuree).unwrap();
    let mut magasin = MagasinAppairage::ouvrir(&dossier).unwrap();
    magasin
        .conserver(&client.id(), &psk, MethodeAppairage::Psk)
        .unwrap();
    drop(magasin);
    let (url, tache) = servir(
        &dossier,
        tune_core::sendspin::ModeTransition::ChiffrementSeul,
    )
    .await;
    conversation_avec_cle(
        &url,
        &client,
        Suite::ChaChaPoly,
        "Cle perdue",
        &psk::sentinelle(),
        &psk,
    )
    .await;
    let pair = tune_core::sendspin::registre::decrire()
        .into_iter()
        .find(|p| p["client_id"] == client.id())
        .unwrap();
    assert_eq!(
        pair["authenticated"], false,
        "un repli ne doit pas devenir une authentification LT"
    );
    assert_eq!(pair["credential_mismatch"], true);
    arreter(tache, &dossier).await;
    let magasin = MagasinAppairage::ouvrir(&dossier).unwrap();
    assert_eq!(
        magasin
            .cle_du_pair(&client.id())
            .unwrap()
            .unwrap()
            .identifiant(),
        psk.identifiant(),
        "le signal ne supprime pas le record"
    );
}

#[tokio::test]
async fn i3326_un_pair_appaire_jamais_vu_dans_ce_processus_ne_peut_pas_revenir_en_clair() {
    let t = tempfile::tempdir().unwrap();
    let dossier = t.path().join("sendspin");
    let client = Identite::generer();
    let psk = PskPair::pour_pair(&client.id(), [29; 32], CategoriePsk::LongueDuree).unwrap();
    let mut magasin = MagasinAppairage::ouvrir(&dossier).unwrap();
    magasin
        .conserver(&client.id(), &psk, MethodeAppairage::Psk)
        .unwrap();
    drop(magasin);
    assert!(!tune_core::sendspin::registre::deja_vu_chiffre(
        &client.id()
    ));
    let (url, tache) = servir(&dossier, tune_core::sendspin::ModeTransition::ClairAccepte).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws.send(Message::Text(
        serde_json::json!({"type":"client/hello","payload":{
            "client_id":client.id(),"version":1,"name":"Usurpation","supported_roles":[]
        }})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let recu = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await
        .unwrap();
    assert!(
        !matches!(
            recu,
            Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_)))
        ),
        "un record persiste doit interdire le clair avant toute observation dans ce processus"
    );
    drop(ws);
    arreter(tache, &dossier).await;
}

#[tokio::test]
async fn i3326_un_magasin_illisible_renvoie_503_sans_nouvelle_identite() {
    let t = tempfile::tempdir().unwrap();
    let dossier = t.path().join("sendspin");
    drop(MagasinAppairage::ouvrir(&dossier).unwrap());
    let fichier = dossier.join("pairing.json");
    std::fs::write(&fichier, b"{").unwrap();
    let (url, tache) = servir(
        &dossier,
        tune_core::sendspin::ModeTransition::ChiffrementSeul,
    )
    .await;
    match tokio_tungstenite::connect_async(url).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(reponse)) => {
            assert_eq!(reponse.status(), 503)
        }
        autre => panic!("503 attendu avant upgrade, recu {autre:?}"),
    }
    assert_eq!(std::fs::read(&fichier).unwrap(), b"{");
    tache.abort();
    let _ = tache.await;
}

#[tokio::test]
async fn i3326_l_api_et_le_websocket_du_routeur_complet_partagent_le_magasin() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    let t = tempfile::tempdir().unwrap();
    let base = t.path().join("tune.db").to_str().unwrap().to_owned();
    let dossier = t.path().join("tune.db.sendspin");
    let client = Identite::generer();
    let psk = PskPair::pour_pair(&client.id(), [29; 32], CategoriePsk::LongueDuree).unwrap();
    let mut magasin = MagasinAppairage::ouvrir(&dossier).unwrap();
    let id = magasin.identite().id();
    let prive = *magasin.identite().prive();
    magasin
        .conserver(&client.id(), &psk, MethodeAppairage::Psk)
        .unwrap();
    drop(magasin);
    let config = tune_server::config::TuneConfig {
        db_path: base,
        ..Default::default()
    };
    let etat = tune_server::state::AppState::new(":memory:", 0, config).unwrap();
    let app = tune_server::routes::router(etat);
    let reponse = app
        .clone()
        .oneshot(
            Request::get("/api/v1/devices/sendspin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        reponse.status(),
        StatusCode::OK,
        "l'extension du magasin doit etre montee sur la vraie API"
    );
    let corps = axum::body::to_bytes(reponse.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&corps).unwrap();
    assert_eq!(v["server_id"], id);
    assert_eq!(v["pairing"]["available"], true);
    assert_eq!(v["pairing"]["paired_clients"][0]["client_id"], client.id());
    assert_eq!(v["playback_supported"], false);
    let texte = std::str::from_utf8(&corps).unwrap();
    for secret in [psk.secret(), &prive] {
        assert!(!texte.contains(&tune_core::sendspin::identite::b64url(secret)));
        assert!(!texte.contains(&serde_json::to_string(secret).unwrap()));
    }
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let tache = tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    let (id_ws, _) = conversation_avec_cle(
        &format!("ws://{adresse}/sendspin"),
        &client,
        Suite::ChaChaPoly,
        "Routeur complet",
        psk.secret(),
        &psk,
    )
    .await;
    assert_eq!(
        id_ws, id,
        "HTTP et WebSocket doivent utiliser la meme identite persistante"
    );
    arreter(tache, &dossier).await;
}
