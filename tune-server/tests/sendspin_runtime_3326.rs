//! Parcours du vrai routeur HTTP + WebSocket. Client Noise independant via snow.
use super::*;
use serde_json::{Value, json};
use std::time::Duration;
use tune_core::sendspin::psk::{CategoriePsk, PskPair};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
struct Serveur {
    temporaire: tempfile::TempDir,
    adresse: std::net::SocketAddr,
    tache: tokio::task::JoinHandle<()>,
}
impl Drop for Serveur {
    fn drop(&mut self) {
        self.tache.abort();
    }
}
impl Serveur {
    async fn nouveau() -> Self {
        let temporaire = tempfile::tempdir().unwrap();
        let config = tune_server::config::TuneConfig {
            db_path: temporaire
                .path()
                .join("tune.db")
                .to_str()
                .unwrap()
                .to_owned(),
            ..Default::default()
        };
        let etat = tune_server::state::AppState::new(":memory:", 0, config).unwrap();
        let app = tune_server::routes::router(etat);
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecoute.local_addr().unwrap();
        let tache = tokio::spawn(async move {
            axum::serve(ecoute, app).await.unwrap();
        });
        Self {
            temporaire,
            adresse,
            tache,
        }
    }
    fn url(&self, id: &str, suffixe: &str) -> String {
        format!(
            "http://{}/api/v1/devices/sendspin/{id}/{suffixe}",
            self.adresse
        )
    }
    async fn disponible(&self, id: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let r = reqwest::get(self.url(id, "pair")).await.unwrap();
                if r.status().is_success() {
                    break r.json::<Value>().await.unwrap();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("inscription du WebSocket dans l'API")
    }
}

struct Lecteur {
    ws: Socket,
    identite: Identite,
    serveur_public: [u8; 32],
    suite: Suite,
    condensat: Vec<u8>,
    transport: snow::TransportState,
    methodes: Value,
}
async fn trame(ws: &mut Socket) -> Message {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match ws
                .next()
                .await
                .expect("WebSocket ferme")
                .expect("trame WebSocket")
            {
                Message::Ping(_) | Message::Pong(_) => continue,
                m => return m,
            }
        }
    })
    .await
    .expect("delai du protocole")
}

impl Lecteur {
    async fn nouveau(
        s: &Serveur,
        identite: Identite,
        suite: Suite,
        cle: &PskPair,
        methodes: Value,
    ) -> Self {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/sendspin", s.adresse))
            .await
            .unwrap();
        ws.send(Message::Ping(vec![1, 2, 3].into())).await.unwrap();
        let init = json!({"type":"client/init","payload":{"client_id":identite.id(),"version":1,"suite":suite.nom()}}).to_string();
        ws.send(Message::Text(init.clone().into())).await.unwrap();
        let Message::Text(reponse) = trame(&mut ws).await else {
            panic!("server/init texte");
        };
        let v: Value = serde_json::from_str(&reponse).unwrap();
        assert_eq!(v["type"], "server/init");
        let serveur_public = URL_SAFE_NO_PAD
            .decode(v["payload"]["server_id"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let prologue = [init.as_bytes(), reponse.as_bytes()].concat();
        let mut noise =
            Enceinte::nouvelle(&identite, &serveur_public, &prologue, suite, cle.secret()).etat;
        let Message::Text(premier) = trame(&mut ws).await else {
            panic!("Noise initial texte");
        };
        let (second, condensat) =
            Self::repondre(&mut noise, &serde_json::from_str(&premier).unwrap(), cle);
        ws.send(Message::Text(second.to_string().into()))
            .await
            .unwrap();
        let transport = noise.into_transport_mode().unwrap();
        let mut l = Self {
            ws,
            identite,
            serveur_public,
            suite,
            condensat,
            transport,
            methodes,
        };
        l.saluer().await;
        l
    }
    fn repondre(noise: &mut HandshakeState, premier: &Value, cle: &PskPair) -> (Value, Vec<u8>) {
        assert_eq!(premier["type"], "noise/handshake");
        let donnees = URL_SAFE_NO_PAD
            .decode(premier["payload"]["data"].as_str().unwrap())
            .unwrap();
        let mut tampon = vec![0; MAX_NOISE];
        let n = noise.read_message(&donnees, &mut tampon).unwrap();
        let charge: Value = serde_json::from_slice(&tampon[..n]).unwrap();
        assert_eq!(charge["psk_id"], cle.identifiant());
        assert_eq!(
            charge["psk_category"],
            serde_json::to_value(cle.categorie()).unwrap()
        );
        let n = noise.write_message(b"{}", &mut tampon).unwrap();
        let second = json!({"type":"noise/handshake","payload":{"data":URL_SAFE_NO_PAD.encode(&tampon[..n])}});
        (second, noise.get_handshake_hash().to_vec())
    }
    async fn envoyer(&mut self, typ: &str, charge: Value) {
        let texte = json!({"type":typ,"payload":charge}).to_string();
        let brut = [&[0][..], texte.as_bytes()].concat();
        let mut b = vec![0; MAX_NOISE];
        let n = self.transport.write_message(&brut, &mut b).unwrap();
        b.truncate(n);
        self.ws.send(Message::Binary(b.into())).await.unwrap();
    }
    async fn lire(&mut self) -> Value {
        let Message::Binary(b) = trame(&mut self.ws).await else {
            panic!("message chiffre attendu");
        };
        let mut clair = vec![0; MAX_NOISE];
        let n = self.transport.read_message(&b, &mut clair).unwrap();
        assert_eq!(clair[0], 0);
        serde_json::from_slice(&clair[1..n]).unwrap()
    }
    async fn saluer(&mut self) {
        assert_eq!(self.lire().await["type"], "server/hello");
        self.envoyer(
            "client/hello",
            json!({"name":"Lecteur runtime 3326","supported_roles":[],
            "supported_pair_methods":self.methodes}),
        )
        .await;
        let v = self.lire().await;
        assert_eq!(v["type"], "server/activate");
        assert_eq!(v["payload"]["activities"], json!([]));
    }
    async fn renouveler(&mut self, cle: &PskPair) {
        let premier = self.lire().await;
        self.ws
            .send(Message::Ping(vec![4, 5, 6].into()))
            .await
            .unwrap();
        let mut noise = Enceinte::nouvelle(
            &self.identite,
            &self.serveur_public,
            &self.condensat,
            self.suite,
            cle.secret(),
        )
        .etat;
        let (second, condensat) = Self::repondre(&mut noise, &premier, cle);
        // Un message deja en vol sur l'ancienne cle reste admissible.
        self.envoyer("client/time", json!({"client_transmitted":17}))
            .await;
        self.envoyer("noise/handshake", second["payload"].clone())
            .await;
        self.transport = noise.into_transport_mode().unwrap();
        self.condensat = condensat;
        self.saluer().await;
    }
}
// Encodeur de fixture RFC4648 (aucun appel au decodeur de production).
fn jeton(id: &Identite, secret: &[u8; 32]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut resultat = String::from("SP:0");
    let mut bits = 0u32;
    let mut nombre = 0;
    for b in id.public().iter().chain(secret) {
        bits = (bits << 8) | u32::from(*b);
        nombre += 8;
        while nombre >= 5 {
            nombre -= 5;
            resultat.push(ALPHABET[((bits >> nombre) & 31) as usize] as char);
        }
    }
    if nombre > 0 {
        resultat.push(ALPHABET[((bits << (5 - nombre)) & 31) as usize] as char);
    }
    resultat
}

#[tokio::test]
async fn i3326_runtime_psk_persiste_renouvelle_reconnecte_et_revoque() {
    for suite in Suite::toutes() {
        let s = Serveur::nouveau().await;
        let id = Identite::generer();
        let id_texte = id.id();
        let prive = *id.prive();
        let pr = PskPair::pour_pair(&id_texte, [23; 32], CategoriePsk::Appairage).unwrap();
        let lt = PskPair::pour_pair(&id_texte, [41; 32], CategoriePsk::LongueDuree).unwrap();
        let token = jeton(&id, pr.secret());
        let mut l = Lecteur::nouveau(
            &s,
            id,
            suite,
            &PskPair::sentinelle(),
            json!({"pairing_psk":{}}),
        )
        .await;
        assert_eq!(s.disponible(&id_texte).await["authenticated"], false);
        let url = s.url(&id_texte, "pair");
        let debut = tokio::spawn(async move {
            reqwest::Client::new()
                .post(url)
                .json(&json!({"method":"pairing_psk","token":token}))
                .send()
                .await
                .unwrap()
        });
        l.renouveler(&pr).await;
        let active = l.lire().await;
        assert_eq!(active["payload"]["activities"], json!(["pairing"]));
        let index = 1; // Premiere activation pairing depuis la nouvelle poignee PR.
        assert_eq!(debut.await.unwrap().status(), 200);
        l.envoyer("client/pair-init", json!({"pairing_index":index}))
            .await;
        l.envoyer(
            "client/pair-finalize",
            json!({"long_term_psk":URL_SAFE_NO_PAD.encode(lt.secret())}),
        )
        .await;
        assert_eq!(l.lire().await["type"], "server/pair-finalize");
        // Le fichier est durable AVANT l'acquittement, alors que le re-echange LT attend encore le client.
        let fichier = s.temporaire.path().join("tune.db.sendspin/pairing.json");
        let contenu = std::fs::read_to_string(fichier).unwrap();
        assert!(
            contenu.contains(&id_texte),
            "le pair doit etre persiste avant l'acquittement"
        );
        l.renouveler(&lt).await;
        assert_eq!(s.disponible(&id_texte).await["authenticated"], true);
        l.ws.close(None).await.unwrap();
        drop(l);
        tokio::time::timeout(Duration::from_secs(3), async {
            while reqwest::get(s.url(&id_texte, "pair"))
                .await
                .unwrap()
                .status()
                != 404
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut l = Lecteur::nouveau(
            &s,
            Identite::depuis_prive(prive),
            suite,
            &lt,
            json!({"pairing_psk":{}}),
        )
        .await;
        assert_eq!(s.disponible(&id_texte).await["authenticated"], true);
        let r = reqwest::Client::new()
            .delete(s.url(&id_texte, "credentials"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.json::<Value>().await.unwrap()["revoked"], true);
        assert!(
            matches!(trame(&mut l.ws).await, Message::Close(_)),
            "la revocation ferme la session active"
        );
    }
}

#[tokio::test]
async fn i3326_runtime_api_refuse_les_entrees_invalides_sans_affecter_le_pair() {
    let s = Serveur::nouveau().await;
    let id = Identite::generer();
    let texte = id.id();
    let mut l = Lecteur::nouveau(
        &s,
        id,
        Suite::ChaChaPoly,
        &PskPair::sentinelle(),
        json!({"static_pairing_code":{}}),
    )
    .await;
    s.disponible(&texte).await;
    let c = reqwest::Client::new();
    for charge in [
        json!({"method":"inconnu"}),
        json!({"method":"static_pairing_code","code":"123"}),
        json!({"method":"pairing_psk","token":jeton(&Identite::generer(),&[11;32])}),
    ] {
        assert_eq!(
            c.post(s.url(&texte, "pair"))
                .json(&charge)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    let trop_gros = json!({"code":"1".repeat(17*1024)});
    assert_eq!(
        c.post(s.url(&texte, "pair/code"))
            .json(&trop_gros)
            .send()
            .await
            .unwrap()
            .status(),
        413
    );
    l.envoyer("client/time", json!({"client_transmitted":22}))
        .await;
    assert_eq!(l.lire().await["payload"]["client_transmitted"], 22);
    l.ws.close(None).await.unwrap();
}

#[tokio::test]
async fn i3326_runtime_commandes_exigent_un_administrateur_quand_auth_active() {
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;
    use tune_core::db::settings_repo::SettingsRepo;
    let temporaire = tempfile::tempdir().unwrap();
    let config = tune_server::config::TuneConfig {
        db_path: temporaire
            .path()
            .join("auth.db")
            .to_str()
            .unwrap()
            .to_owned(),
        ..Default::default()
    };
    let etat = tune_server::state::AppState::new(":memory:", 0, config).unwrap();
    let settings = SettingsRepo::with_backend(etat.backend.clone());
    settings.set("auth_enabled", "true").unwrap();
    settings
        .set("jwt_secret", "fixture-sendspin-3326-aucun-secret-reel")
        .unwrap();
    let app = tune_server::routes::router(etat);
    let id = Identite::generer().id();
    for (methode, suffixe, charge, admin_statut) in [
        (Method::GET, "pair", json!({}), 404),
        (
            Method::POST,
            "pair",
            json!({"method":"static_pairing_code"}),
            404,
        ),
        (Method::DELETE, "pair", json!({}), 404),
        (Method::POST, "pair/code", json!({"code":"12345678"}), 404),
        (Method::DELETE, "credentials", json!({}), 200),
    ] {
        for (role, attendu) in [
            (None, 401),
            (Some("user"), 403),
            (Some("admin"), admin_statut),
        ] {
            let mut requete = Request::builder()
                .method(methode.clone())
                .uri(format!("/api/v1/devices/sendspin/{id}/{suffixe}"))
                .header("content-type", "application/json");
            if let Some(role) = role {
                let jwt =
                    tune_server::auth::sign_jwt(1, role, "fixture-sendspin-3326-aucun-secret-reel")
                        .unwrap();
                requete = requete.header("authorization", format!("Bearer {jwt}"));
            }
            let reponse = app
                .clone()
                .oneshot(requete.body(Body::from(charge.to_string())).unwrap())
                .await
                .unwrap();
            assert_eq!(
                reponse.status().as_u16(),
                attendu,
                "{methode} {suffixe}, role={role:?}"
            );
        }
    }
}

#[tokio::test]
async fn i3326_runtime_revocation_interrompt_un_reechange_sans_reponse() {
    let s = Serveur::nouveau().await;
    let id = Identite::generer();
    let texte = id.id();
    let token = jeton(&id, &[23; 32]);
    let mut l = Lecteur::nouveau(
        &s,
        id,
        Suite::ChaChaPoly,
        &PskPair::sentinelle(),
        json!({"pairing_psk":{}}),
    )
    .await;
    s.disponible(&texte).await;
    let url = s.url(&texte, "pair");
    let debut = tokio::spawn(async move {
        reqwest::Client::new()
            .post(url)
            .json(&json!({"method":"pairing_psk","token":token}))
            .send()
            .await
            .unwrap()
    });
    assert_eq!(l.lire().await["type"], "noise/handshake");
    // Le pair ne repond plus : la revocation ne doit pas attendre le delai Noise.
    let r = reqwest::Client::new()
        .delete(s.url(&texte, "credentials"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let fermeture = tokio::time::timeout(Duration::from_secs(2), l.ws.next())
        .await
        .expect("la revocation doit interrompre le re-echange sans attendre le pair");
    assert!(!matches!(
        fermeture,
        Some(Ok(Message::Text(_) | Message::Binary(_)))
    ));
    assert_eq!(debut.await.unwrap().status(), 503);
}

#[tokio::test]
async fn i3326_runtime_ecriture_impossible_ne_confirme_pas_l_appairage() {
    let s = Serveur::nouveau().await;
    let id = Identite::generer();
    let texte = id.id();
    let pr = PskPair::pour_pair(&texte, [23; 32], CategoriePsk::Appairage).unwrap();
    let token = jeton(&id, pr.secret());
    let mut l = Lecteur::nouveau(
        &s,
        id,
        Suite::ChaChaPoly,
        &PskPair::sentinelle(),
        json!({"pairing_psk":{}}),
    )
    .await;
    s.disponible(&texte).await;
    let url = s.url(&texte, "pair");
    let debut = tokio::spawn(async move {
        reqwest::Client::new()
            .post(url)
            .json(&json!({"method":"pairing_psk","token":token}))
            .send()
            .await
            .unwrap()
    });
    l.renouveler(&pr).await;
    assert_eq!(l.lire().await["payload"]["activities"], json!(["pairing"]));
    assert_eq!(debut.await.unwrap().status(), 200);
    l.envoyer("client/pair-init", json!({"pairing_index":1}))
        .await;
    // Fixture uniquement : rename ne peut remplacer un repertoire par un fichier.
    let fichier = s.temporaire.path().join("tune.db.sendspin/pairing.json");
    let sauvegarde = fichier.with_extension("json.before");
    std::fs::rename(&fichier, &sauvegarde).unwrap();
    std::fs::create_dir(&fichier).unwrap();
    l.envoyer(
        "client/pair-finalize",
        json!({"long_term_psk":URL_SAFE_NO_PAD.encode([41;32])}),
    )
    .await;
    let fermeture = tokio::time::timeout(Duration::from_secs(3), l.ws.next())
        .await
        .unwrap();
    assert!(
        !matches!(fermeture, Some(Ok(Message::Text(_) | Message::Binary(_)))),
        "une ecriture impossible ne doit emettre ni acquittement ni re-echange LT"
    );
    let document: Value = serde_json::from_slice(&std::fs::read(sauvegarde).unwrap()).unwrap();
    assert!(
        document["pairs"].get(&texte).is_none(),
        "le fichier precedent reste non appaire"
    );
}

#[tokio::test]
async fn i3326_runtime_transport_corrompu_ferme_sans_server_error() {
    for suite in Suite::toutes() {
        for clair in [false, true] {
            let s = Serveur::nouveau().await;
            let mut l = Lecteur::nouveau(
                &s,
                Identite::generer(),
                suite,
                &PskPair::sentinelle(),
                json!({}),
            )
            .await;
            let message = if clair {
                Message::Text(
                    json!({"type":"client/time","payload":{"client_transmitted":1}})
                        .to_string()
                        .into(),
                )
            } else {
                Message::Binary(vec![0; 48].into())
            };
            l.ws.send(message).await.unwrap();
            let recu = tokio::time::timeout(Duration::from_secs(2), l.ws.next())
                .await
                .expect("une trame invalide doit fermer le transport");
            match recu {
                None | Some(Ok(Message::Close(_))) | Some(Err(_)) => {}
                autre => panic!("un echec de transport doit rester silencieux : {autre:?}"),
            }
        }
    }
}

#[path = "sendspin/cpace_runtime_3326.rs"]
mod cpace;
