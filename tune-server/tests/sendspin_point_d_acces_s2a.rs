//! Témoin du point d'accès Sendspin de Tune (#3326, brique S2-a).
//!
//! ## Ce qu'il mesure
//!
//! La séquence complète, sur une **vraie** connexion WebSocket, contre le
//! **vrai** routeur : `client/init` → `server/init` → `noise/handshake` ×2 →
//! `server/hello` → `client/hello` → `server/activate`. Rien n'est simulé : le
//! serveur écoute sur une socket TCP, le client compose dessus.
//!
//! ## Pourquoi ce fichier existe en plus de `tune-core/tests/sendspin_poignee_s2a.rs`
//!
//! Celui de `tune-core` éprouve la machine à états. Celui-ci éprouve le
//! **branchement** : qu'une enceinte qui compose vers `ws://…/sendspin` soit
//! réellement conduite jusqu'au bout. « Écrit mais pas branché » est le défaut
//! que ce dépôt paye le plus souvent ; une poignée de main que personne
//! n'invoque ne prouve rien.
//!
//! ## Ce qu'il ne prouve toujours pas
//!
//! L'interopérabilité. Le côté client est écrit ici. La porte de sortie de
//! S2-a — un `client/hello` venu d'une implémentation tierce — se franchit
//! ailleurs, et ce fichier ne s'y substitue pas.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use snow::{Builder, HandshakeState};
use tokio_tungstenite::tungstenite::Message;

use tune_core::sendspin::identite::Identite;
use tune_core::sendspin::psk;
use tune_core::sendspin::suite::Suite;

const MAX_NOISE: usize = 65535;

/// Monte le VRAI routeur du point d'accès sur une socket éphémère.
///
/// **Mode de transition FERMÉ**, délibérément : tous les témoins de ce fichier
/// éprouvent le chemin chiffré, et ils doivent le faire dans la configuration
/// par défaut de Tune. Un chemin chiffré qui n'aurait été mesuré qu'avec la
/// porte du clair ouverte ne prouverait pas grand-chose.
async fn point_d_acces() -> String {
    let app = axum::Router::new()
        .nest(
            "/sendspin",
            tune_server::routes::sendspin::router::<()>(
                tune_core::sendspin::ModeTransition::ChiffrementSeul,
            ),
        )
        .with_state(());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("socket ephemere");
    let adresse = ecoute.local_addr().expect("adresse");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("ws://{adresse}/sendspin")
}

/// Le côté enceinte, monté sur `snow`, indépendant du code testé.
struct Enceinte {
    etat: HandshakeState,
}

impl Enceinte {
    fn nouvelle(
        moi: &Identite,
        serveur_public: &[u8; 32],
        prologue: &[u8],
        suite: Suite,
        psk: &[u8; 32],
    ) -> Self {
        let params = suite.motif_noise().parse().expect("motif");
        Self {
            etat: Builder::new(params)
                .local_private_key(moi.prive())
                .expect("cle locale")
                .remote_public_key(serveur_public)
                .expect("cle serveur")
                .prologue(prologue)
                .expect("prologue")
                .psk(Suite::position_psk(), psk)
                .expect("psk")
                .build_responder()
                .expect("repondeur"),
        }
    }
}

fn texte(message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>) -> String {
    match message.expect("connexion fermee").expect("erreur ws") {
        Message::Text(t) => t.to_string(),
        autre => panic!("trame TEXTE attendue, recu : {autre:?}"),
    }
}

fn binaire(message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>) -> Vec<u8> {
    match message.expect("connexion fermee").expect("erreur ws") {
        Message::Binary(b) => b.to_vec(),
        autre => panic!("trame BINAIRE attendue, recu : {autre:?}"),
    }
}

/// Joue la séquence entière côté enceinte et rend le `server/activate` reçu.
async fn conversation_complete(suite: Suite, nom: &str) -> (String, String) {
    let url = point_d_acces().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connexion au point d'acces");

    let moi = Identite::generer();
    let sentinelle = psk::sentinelle();

    // 1. `client/init` — l'enceinte ouvre et CHOISIT la suite.
    let init = serde_json::json!({
        "type": "client/init",
        "payload": {"client_id": moi.id(), "version": 1, "suite": suite.nom()}
    })
    .to_string();
    ws.send(Message::Text(init.clone().into()))
        .await
        .expect("envoi client/init");

    // 2. `server/init` — on garde le texte TEL QUEL pour le prologue.
    let server_init = texte(ws.next().await);
    let brute: serde_json::Value = serde_json::from_str(&server_init).expect("server/init");
    assert_eq!(brute["type"], "server/init");
    let server_id = brute["payload"]["server_id"]
        .as_str()
        .expect("server_id")
        .to_string();
    assert_eq!(
        server_id.len(),
        43,
        "un server_id est une cle publique X25519 en base64url"
    );
    let serveur_public: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&server_id)
        .expect("base64url")
        .try_into()
        .expect("32 octets");

    let mut prologue = Vec::new();
    prologue.extend_from_slice(init.as_bytes());
    prologue.extend_from_slice(server_init.as_bytes());
    let mut enceinte = Enceinte::nouvelle(&moi, &serveur_public, &prologue, suite, &sentinelle);

    // 3. message Noise 1, qui porte le `psk_id`.
    let hs1 = texte(ws.next().await);
    let v1: serde_json::Value = serde_json::from_str(&hs1).expect("noise/handshake 1");
    assert_eq!(v1["type"], "noise/handshake");
    let brut = URL_SAFE_NO_PAD
        .decode(v1["payload"]["data"].as_str().expect("data"))
        .expect("base64url");
    let mut tampon = vec![0u8; MAX_NOISE];
    let n = enceinte
        .etat
        .read_message(&brut, &mut tampon)
        .expect("lecture noise 1");
    tampon.truncate(n);
    let charge: serde_json::Value = serde_json::from_slice(&tampon).expect("charge noise 1");
    assert_eq!(
        charge["psk_id"].as_str().expect("psk_id"),
        psk::identifiant(&sentinelle),
        "S2-a annonce la Sentinelle, et rien d'autre"
    );

    // 4. message Noise 2 : la poignee de main se ferme.
    let mut tampon = vec![0u8; MAX_NOISE];
    let n = enceinte
        .etat
        .write_message(&[], &mut tampon)
        .expect("ecriture noise 2");
    tampon.truncate(n);
    let hs2 = serde_json::json!({
        "type": "noise/handshake",
        "payload": {"data": URL_SAFE_NO_PAD.encode(&tampon)}
    })
    .to_string();
    ws.send(Message::Text(hs2.into()))
        .await
        .expect("envoi noise 2");
    let mut transport = enceinte
        .etat
        .into_transport_mode()
        .expect("bascule en transport");

    // 5. `server/hello`, chiffre, en trame BINAIRE.
    let trame = binaire(ws.next().await);
    let mut clair = vec![0u8; MAX_NOISE];
    let n = transport
        .read_message(&trame, &mut clair)
        .expect("dechiffrement server/hello");
    clair.truncate(n);
    assert_eq!(clair[0], 0, "l'octet de type d'un corps JSON vaut 0");
    let hello: serde_json::Value = serde_json::from_slice(&clair[1..]).expect("server/hello");
    assert_eq!(hello["type"], "server/hello");
    assert!(
        hello["payload"]["name"]
            .as_str()
            .is_some_and(|n| n.starts_with("Tune")),
        "le serveur doit se nommer : {hello}"
    );

    // 6. `client/hello` — ce que S2-a existe pour recolter.
    let mon_hello = serde_json::json!({
        "type": "client/hello",
        "payload": {
            "name": nom,
            "supported_roles": ["player@v1", "metadata@v1"],
            // Cle VERSIONNEE, telle qu'un vrai lecteur l'envoie (mesure le
            // 09/09/2026 contre aiosendspin git HEAD). Ecrire `player_support`
            // ici ferait passer le temoin alors que le fil reel echouerait.
            "player@v1_support": {
                "buffer_capacity": 2000000,
                "supported_commands": ["volume", "mute"],
                "supported_formats": [
                    {"bit_depth": 24, "channels": 2, "codec": "flac", "sample_rate": 48000}
                ]
            },
            "un_champ_du_futur": true
        }
    })
    .to_string();
    let mut a_chiffrer = vec![0u8];
    a_chiffrer.extend_from_slice(mon_hello.as_bytes());
    let mut sortie = vec![0u8; MAX_NOISE];
    let n = transport
        .write_message(&a_chiffrer, &mut sortie)
        .expect("chiffrement client/hello");
    sortie.truncate(n);
    ws.send(Message::Binary(sortie.into()))
        .await
        .expect("envoi client/hello");

    // 7. `server/activate`.
    let trame = binaire(ws.next().await);
    let mut clair = vec![0u8; MAX_NOISE];
    let n = transport
        .read_message(&trame, &mut clair)
        .expect("dechiffrement server/activate");
    clair.truncate(n);
    let activate: serde_json::Value = serde_json::from_slice(&clair[1..]).expect("server/activate");

    (moi.id(), activate.to_string())
}

#[tokio::test]
async fn la_sequence_complete_aboutit_sur_une_vraie_connexion() {
    let (client_id, activate) = conversation_complete(Suite::ChaChaPoly, "Cuisine").await;
    let v: serde_json::Value = serde_json::from_str(&activate).expect("json");
    assert_eq!(v["type"], "server/activate");
    assert_eq!(
        v["payload"]["activities"],
        serde_json::json!([]),
        "S2-a ne revendique NI lecture NI appairage : la liste doit etre vide, \
         sinon l'enceinte croirait qu'on va jouer"
    );

    // Le pair doit avoir ete retenu, avec ce qu'il a dit de lui.
    let vus = tune_core::sendspin::registre::pairs_vus();
    let pair = vus
        .iter()
        .find(|p| p.client_id == client_id)
        .expect("le pair qui vient de parler doit figurer au registre");
    assert_eq!(pair.nom.as_deref(), Some("Cuisine"));
    assert!(
        pair.roles.iter().any(|r| r == "player@v1"),
        "les roles annonces doivent etre retenus : {:?}",
        pair.roles
    );
    let support = pair
        .player_support
        .as_ref()
        .expect("les capacites de lecture sont la matiere que S2-a recolte");
    assert_eq!(
        support["supported_formats"][0]["codec"],
        serde_json::json!("flac"),
        "les capacites doivent etre lues sous la cle VERSIONNEE `player@v1_support` : {support}"
    );
    assert!(
        pair.hello_brut.get("un_champ_du_futur").is_some(),
        "un champ que nous ne savons pas nommer doit etre CONSERVE : {:?}",
        pair.hello_brut
    );
}

#[tokio::test]
async fn les_deux_suites_aboutissent_sur_une_vraie_connexion() {
    // Un serveur Sendspin doit supporter les DEUX suites. Le prouver sur le
    // vrai point d'acces, pas seulement dans la machine a etats.
    for suite in Suite::toutes() {
        let (_, activate) = conversation_complete(suite, "Salon").await;
        assert!(
            activate.contains("server/activate"),
            "la suite {suite} doit aboutir jusqu'a server/activate"
        );
    }
}

#[tokio::test]
async fn un_client_init_illisible_fait_fermer_sans_reponse_applicative() {
    // La specification n'a AUCUN message d'erreur applicatif : la seule
    // reaction admise a un echec de poignee de main est de fermer le
    // WebSocket. Un serveur qui repondrait « erreur » serait hors protocole.
    let url = point_d_acces().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connexion");
    ws.send(Message::Text("ceci n'est pas du JSON".into()))
        .await
        .expect("envoi");

    match ws.next().await {
        None => {}
        Some(Ok(Message::Close(_))) => {}
        Some(Ok(autre)) => panic!(
            "aucun message applicatif ne doit partir apres un echec de poignee \
             de main, recu : {autre:?}"
        ),
        Some(Err(_)) => {}
    }
}

/// Garde contre « écrit mais pas branché ».
///
/// Les témoins ci-dessus montent le routeur du point d'accès à la main. Ça ne
/// dit rien de l'application réelle : le jour où la ligne `nest` disparaît de
/// `routes/mod.rs`, ils resteraient tous verts et plus aucune enceinte ne
/// pourrait joindre Tune. Ce test relit donc la source de l'assemblage.
#[test]
fn le_point_d_acces_est_monte_a_la_racine_de_l_application() {
    let source = include_str!("../src/routes/mod.rs");

    // Ligne par ligne, en ecartant les lignes COMMENTEES. Un simple `contains`
    // sur la source entiere retrouvait la sous-chaine dans
    // `// .nest("/sendspin", ...)` : la contre-epreuve a montre la garde verte
    // alors que le point d'acces etait demonte.
    //
    // Depuis le mode de transition, le montage prend un argument et rustfmt le
    // coupe sur plusieurs lignes : la garde cherche donc l'APPEL, pas la ligne
    // entiere, mais toujours sur une ligne non commentee.
    let est_monte = |bloc: &str| {
        bloc.lines().any(|l| {
            let l = l.trim_start();
            !l.starts_with("//") && l.contains(MONTAGE_ATTENDU)
        })
    };

    assert!(
        est_monte(source),
        "le point d'acces Sendspin n'est plus monte dans routes/mod.rs \
         (une ligne commentee ne compte pas) : la poignee de main existerait \
         sans que personne ne puisse l'invoquer"
    );

    // Et a la RACINE, pas sous `/api/v1` : le TXT mDNS annonce `/sendspin`, et
    // une enceinte ne connait pas nos prefixes.
    let sous_api = source
        .split(r#".nest("/api/v1", api)"#)
        .nth(1)
        .expect("l'assemblage doit toujours nester /api/v1");
    assert!(
        est_monte(sous_api),
        "le point d'acces doit etre monte sur le routeur RACINE, apres \
         .nest(\"/api/v1\", api)"
    );
}

/// Le montage attendu, ecrit une seule fois.
const MONTAGE_ATTENDU: &str = "sendspin::router(";

/// L'annonce mDNS est l'autre moitié de la découverte, et elle a le même
/// défaut possible : exister sans être appelée.
#[test]
fn l_annonce_du_service_serveur_est_reellement_appelee() {
    let source = include_str!("../src/discovery_setup.rs");
    assert!(
        source.contains("register_sendspin_server(port)"),
        "la specification impose au serveur de supporter les DEUX modes de \
         decouverte ; sans cet appel, seul le parcours de la phase 1 subsiste"
    );
}

/// Banc de preuve — le VRAI point d'accès, servi sur un port fixe, pour qu'une
/// implémentation **tierce** vienne s'y connecter.
///
/// C'est la porte de sortie de S2-a. Les témoins ci-dessus prouvent que notre
/// serveur est d'accord avec un client que nous avons écrit ; ils ne prouvent
/// pas l'interopérabilité. Seule une implémentation que nous n'avons pas
/// écrite peut la prouver, et ce banc existe pour qu'on puisse la refaire
/// plutôt que de croire un rapport sur parole.
///
/// Marqué `#[ignore]` : il demande un lecteur en face, donc il n'a rien à
/// faire dans la porte de CI, où il resterait suspendu.
///
/// ```sh
/// # 1. le serveur
/// cargo test -p tune-server --test sendspin_point_d_acces_s2a \
///     -- --ignored --nocapture banc_de_preuve_lecteur_reel
///
/// # 2. le lecteur, dans un autre terminal (implementation de reference
/// #    Python de l'Open Home Foundation, `pip install sendspin`)
/// sendspin daemon --url ws://127.0.0.1:8927/sendspin --name "Preuve S2-a"
/// ```
#[tokio::test]
#[ignore = "banc de preuve : demande un vrai lecteur Sendspin en face"]
async fn banc_de_preuve_lecteur_reel() {
    // Le journal du serveur EST la preuve : sans abonne `tracing`, le parcours
    // se deroulerait en silence et une absence de trace ne dirait rien.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    // Le banc arme le mode de transition : le lecteur de reference PUBLIE ne
    // parle que le clair (aiosendspin 6.0.5 n'a aucun module `noise/`). Sans
    // ca, ce banc ne peut recevoir que d'un lecteur bati sur le depot git.
    let mode = match std::env::var("SENDSPIN_BANC_CHIFFRE_SEUL").as_deref() {
        Ok("1") => tune_core::sendspin::ModeTransition::ChiffrementSeul,
        _ => tune_core::sendspin::ModeTransition::ClairAccepte,
    };
    println!("mode de transition du banc : {}", mode.nom());
    let app = axum::Router::new()
        .nest(
            "/sendspin",
            tune_server::routes::sendspin::router::<()>(mode),
        )
        .with_state(());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:8927")
        .await
        .expect("le port 8927 doit etre libre");
    println!("banc de preuve : ws://127.0.0.1:8927/sendspin");
    println!(
        "server_id = {}",
        tune_core::sendspin::identite_du_serveur().id()
    );
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });

    let attente = std::time::Duration::from_secs(
        std::env::var("SENDSPIN_BANC_SECONDES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90),
    );
    println!("en attente d'un lecteur pendant {} s...", attente.as_secs());
    let debut = std::time::Instant::now();
    while debut.elapsed() < attente {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if !tune_core::sendspin::registre::pairs_vus().is_empty() {
            break;
        }
    }

    let vus = tune_core::sendspin::registre::pairs_vus();
    for pair in &vus {
        println!("--- client/hello REEL ---");
        println!("client_id       = {}", pair.client_id);
        println!("suite           = {:?}", pair.suite);
        println!(
            "transport       = {}",
            if pair.chiffre { "noise" } else { "CLAIR" }
        );
        println!("name            = {:?}", pair.nom);
        println!("supported_roles = {:?}", pair.roles);
        println!(
            "player_support  = {}",
            serde_json::to_string_pretty(&pair.player_support).unwrap_or_default()
        );
        println!(
            "hello (entier)  = {}",
            serde_json::to_string_pretty(&pair.hello_brut).unwrap_or_default()
        );
    }
    assert!(
        !vus.is_empty(),
        "aucun lecteur ne s'est presente : la porte de sortie de S2-a n'est pas franchie"
    );
}
