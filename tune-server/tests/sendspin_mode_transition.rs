//! Témoins du **mode de transition** Sendspin (#3326).
//!
//! ## Ce que ce fichier garde
//!
//! Le mode de transition accepte un `client/hello` **en clair** comme premier
//! message, parce qu'aucun lecteur publié ne parle encore le Sendspin chiffré
//! (mesuré le 11/09/2026 : `aiosendspin` 6.0.5 n'embarque aucun module
//! `noise/`, et ne connaît ni `client/init` ni `server/activate`).
//!
//! C'est une porte **supplémentaire**, et trois choses doivent rester vraies :
//!
//! 1. elle est **fermée par défaut** ;
//! 2. une session qui l'emprunte est **nommée** comme telle ;
//! 3. **elle n'affaiblit pas le chemin chiffré** — c'est
//!    [`avec_le_mode_de_transition_un_client_capable_de_noise_negocie_quand_meme_noise`],
//!    la contre-épreuve la plus importante de ce travail. Sans elle, ce serait
//!    une régression de sécurité déguisée en fonctionnalité.
//!
//! ## Pourquoi aucun témoin ne vide le registre
//!
//! Le registre est un état de PROCESSUS, et `cargo test` lance les témoins d'un
//! même binaire sur plusieurs fils. Chaque témoin se donne donc un `client_id`
//! unique et ne regarde que le sien : personne ne compte, personne ne vide,
//! rien ne peut se marcher dessus.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use snow::Builder;
use tokio_tungstenite::tungstenite::Message;

use tune_core::sendspin::identite::Identite;
use tune_core::sendspin::suite::Suite;
use tune_core::sendspin::{ModeTransition, psk, registre};

const MAX_NOISE: usize = 65535;

/// Monte le VRAI routeur du point d'accès, dans le mode demandé.
async fn point_d_acces(mode: ModeTransition) -> String {
    let app = axum::Router::new()
        .nest(
            "/sendspin",
            tune_server::routes::sendspin::router::<()>(mode),
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

/// Un `client/hello` en clair, tel qu'un lecteur d'avant le chiffrement l'écrit.
///
/// La forme est copiée de `ClientHelloPayload` d'`aiosendspin` 6.0.5, où
/// `client_id`, `name`, `version` et `supported_roles` sont **obligatoires**, et
/// où les capacités du lecteur voyagent sous la clé VERSIONNÉE.
fn hello_en_clair(client_id: &str, nom: &str) -> String {
    serde_json::json!({
        "type": "client/hello",
        "payload": {
            "client_id": client_id,
            "name": nom,
            "version": 1,
            "supported_roles": ["player@v1"],
            "player@v1_support": {
                "buffer_capacity": 2_000_000,
                "supported_commands": ["volume", "mute"],
                "supported_formats": [
                    {"bit_depth": 16, "channels": 2, "codec": "pcm", "sample_rate": 44100}
                ]
            }
        }
    })
    .to_string()
}

/// Ce qu'une trame reçue est, sans paniquer.
enum Recu {
    Texte(String),
    Binaire(Vec<u8>),
    Ferme,
}

async fn recevoir<S>(ws: &mut S) -> Recu
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    match tokio::time::timeout(std::time::Duration::from_secs(10), ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => Recu::Texte(t.to_string()),
        Ok(Some(Ok(Message::Binary(b)))) => Recu::Binaire(b.to_vec()),
        // Fermeture, erreur de transport ou flux tari : dans les trois cas le
        // pair n'a RIEN obtenu d'applicatif, et c'est ce qui nous intéresse.
        _ => Recu::Ferme,
    }
}

/// Un identifiant unique par témoin — le registre vit pour tout le binaire.
fn identifiant_unique(quoi: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!("{quoi}-{}", N.fetch_add(1, Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// 1. Le défaut : la porte est fermée.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn par_defaut_un_client_hello_en_clair_est_refuse() {
    let url = point_d_acces(ModeTransition::default()).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connexion");
    let id = identifiant_unique("refuse");
    ws.send(Message::Text(hello_en_clair(&id, "Cuisine").into()))
        .await
        .expect("envoi");

    match recevoir(&mut ws).await {
        Recu::Ferme => {}
        Recu::Texte(t) => panic!(
            "le mode par defaut ne doit RIEN repondre a un hello en clair, \
             recu : {t}"
        ),
        Recu::Binaire(_) => panic!("aucune trame ne doit partir sur un hello en clair refuse"),
    }

    assert!(
        !registre::pairs_vus().iter().any(|p| p.client_id == id),
        "un pair refuse ne doit pas figurer au registre : il n'a jamais ete admis"
    );
}

#[test]
fn le_mode_par_defaut_est_bien_le_refus() {
    // La garde la plus bete et la plus utile : si le jour ou quelqu'un change
    // le `#[default]`, TOUT Tune se met a accepter du clair en silence.
    assert_eq!(ModeTransition::default(), ModeTransition::ChiffrementSeul);
    assert!(!ModeTransition::default().accepte_le_clair());
}

// ---------------------------------------------------------------------------
// 2. Armé, il parle au lecteur publié.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn le_mode_de_transition_repond_un_server_hello_herite_en_clair() {
    let url = point_d_acces(ModeTransition::ClairAccepte).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connexion");
    let id = identifiant_unique("transition");
    ws.send(Message::Text(hello_en_clair(&id, "Salon").into()))
        .await
        .expect("envoi");

    let texte = match recevoir(&mut ws).await {
        Recu::Texte(t) => t,
        Recu::Binaire(_) => panic!(
            "un lecteur de transition ne sait pas dechiffrer : la reponse doit \
             etre une trame TEXTE"
        ),
        Recu::Ferme => panic!("le mode de transition arme doit repondre"),
    };
    let v: serde_json::Value = serde_json::from_str(&texte).expect("json");
    assert_eq!(v["type"], serde_json::json!("server/hello"));

    // Les CINQ champs qu'aiosendspin 6.0.5 rend obligatoires. En omettre un ne
    // produit aucune erreur sur le fil : le lecteur echoue a desserialiser en
    // silence, et sa poignee de main expire au bout de 10 s.
    for champ in [
        "server_id",
        "name",
        "version",
        "active_roles",
        "connection_reason",
    ] {
        assert!(
            v["payload"].get(champ).is_some(),
            "un lecteur publie exige {champ} : {texte}"
        );
    }
    assert_eq!(v["payload"]["version"], serde_json::json!(1));
    assert_eq!(
        v["payload"]["connection_reason"],
        serde_json::json!("discovery"),
        "l'enumeration de 6.0.5 ne connait que `discovery` et `playback`, et le \
         lecteur la lit strictement : {texte}"
    );
    assert_eq!(
        v["payload"]["active_roles"],
        serde_json::json!([]),
        "rien n'est joue et rien ne doit l'etre sur une connexion en clair non \
         authentifiee : aucun role actif"
    );

    // Et surtout : PAS de `server/activate`. Ce message n'existe nulle part
    // dans aiosendspin 6.0.5 — l'envoyer serait parler dans le vide.
    match recevoir(&mut ws).await {
        Recu::Ferme => {}
        Recu::Texte(t) => {
            assert!(
                !t.contains("server/activate"),
                "aucun `server/activate` ne doit suivre le hello herite : le \
                 lecteur publie ne connait pas ce message ({t})"
            );
        }
        Recu::Binaire(_) => panic!("aucune trame binaire sur une session en clair"),
    }
}

// ---------------------------------------------------------------------------
// 3. LA contre-épreuve : le chemin chiffré n'est pas affaibli.
// ---------------------------------------------------------------------------

/// **Le témoin le plus important de ce travail.**
///
/// Mode de transition **ARMÉ**, et un client qui propose Noise : il doit obtenir
/// Noise, entièrement, et sa session doit être enregistrée comme chiffrée.
///
/// Ce qui serait une régression de sécurité déguisée en fonctionnalité :
/// qu'avec ce mode armé, Tune réponde en clair, ou se rabatte sur le clair, ou
/// enregistre la session comme non chiffrée. Les trois sont mesurés ici.
#[tokio::test]
async fn avec_le_mode_de_transition_un_client_capable_de_noise_negocie_quand_meme_noise() {
    let url = point_d_acces(ModeTransition::ClairAccepte).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connexion");

    let moi = Identite::generer();
    let sentinelle = psk::sentinelle();
    let suite = Suite::ChaChaPoly;

    // 1. `client/init` : le client PROPOSE Noise.
    let init = serde_json::json!({
        "type": "client/init",
        "payload": {"client_id": moi.id(), "version": 1, "suite": suite.nom()}
    })
    .to_string();
    ws.send(Message::Text(init.clone().into()))
        .await
        .expect("envoi client/init");

    // 2. `server/init` — en TEXTE, comme la specification l'impose pour les
    //    trois messages de poignee de main. Ce n'est PAS la branche en clair.
    let server_init = match recevoir(&mut ws).await {
        Recu::Texte(t) => t,
        _ => panic!("le serveur doit repondre server/init a un client/init"),
    };
    let brute: serde_json::Value = serde_json::from_str(&server_init).expect("json");
    assert_eq!(
        brute["type"],
        serde_json::json!("server/init"),
        "avec le mode de transition arme, un `client/init` doit TOUJOURS mener \
         a la poignee de main Noise, jamais a la branche en clair : {server_init}"
    );
    assert!(
        brute["payload"].get("connection_reason").is_none(),
        "un `server/hello` herite en reponse a un `client/init` serait une \
         retrogradation silencieuse : {server_init}"
    );
    let server_id = brute["payload"]["server_id"]
        .as_str()
        .expect("server_id")
        .to_string();
    let serveur_public: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&server_id)
        .expect("base64url")
        .try_into()
        .expect("32 octets");

    let mut prologue = Vec::new();
    prologue.extend_from_slice(init.as_bytes());
    prologue.extend_from_slice(server_init.as_bytes());
    let mut etat = Builder::new(suite.motif_noise().parse().expect("motif"))
        .local_private_key(moi.prive())
        .expect("cle locale")
        .remote_public_key(&serveur_public)
        .expect("cle serveur")
        .prologue(&prologue)
        .expect("prologue")
        .psk(Suite::position_psk(), &sentinelle)
        .expect("psk")
        .build_responder()
        .expect("repondeur");

    // 3. message Noise 1.
    let hs1 = match recevoir(&mut ws).await {
        Recu::Texte(t) => t,
        _ => panic!("le message Noise 1 doit arriver"),
    };
    let v1: serde_json::Value = serde_json::from_str(&hs1).expect("json");
    assert_eq!(v1["type"], serde_json::json!("noise/handshake"));
    let brut = URL_SAFE_NO_PAD
        .decode(v1["payload"]["data"].as_str().expect("data"))
        .expect("base64url");
    let mut tampon = vec![0u8; MAX_NOISE];
    let n = etat
        .read_message(&brut, &mut tampon)
        .expect("lecture noise 1");
    tampon.truncate(n);

    // 4. message Noise 2.
    let mut tampon = vec![0u8; MAX_NOISE];
    let n = etat.write_message(&[], &mut tampon).expect("noise 2");
    tampon.truncate(n);
    ws.send(Message::Text(
        serde_json::json!({
            "type": "noise/handshake",
            "payload": {"data": URL_SAFE_NO_PAD.encode(&tampon)}
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("envoi noise 2");
    let mut transport = etat.into_transport_mode().expect("transport");

    // 5. `server/hello` : il doit arriver CHIFFRE, en trame BINAIRE.
    let trame = match recevoir(&mut ws).await {
        Recu::Binaire(b) => b,
        Recu::Texte(t) => panic!(
            "le mode de transition arme ne doit RIEN changer au chemin chiffre : \
             `server/hello` doit arriver chiffre en trame binaire, recu en clair : {t}"
        ),
        Recu::Ferme => panic!("la poignee de main doit aboutir"),
    };
    let mut clair = vec![0u8; MAX_NOISE];
    let n = transport
        .read_message(&trame, &mut clair)
        .expect("le server/hello doit se dechiffrer : c'est la preuve que le tuyau est Noise");
    clair.truncate(n);
    assert_eq!(clair[0], 0, "octet de type d'un corps JSON");
    let hello: serde_json::Value = serde_json::from_slice(&clair[1..]).expect("server/hello");
    assert_eq!(hello["type"], serde_json::json!("server/hello"));

    // 6. `client/hello`, chiffre.
    let mon_hello = serde_json::json!({
        "type": "client/hello",
        "payload": {
            "name": "Chiffre malgre le mode de transition",
            "supported_roles": ["player@v1"],
            "player@v1_support": {"buffer_capacity": 42}
        }
    })
    .to_string();
    let mut a_chiffrer = vec![0u8];
    a_chiffrer.extend_from_slice(mon_hello.as_bytes());
    let mut sortie = vec![0u8; MAX_NOISE];
    let n = transport
        .write_message(&a_chiffrer, &mut sortie)
        .expect("chiffrement");
    sortie.truncate(n);
    ws.send(Message::Binary(sortie.into()))
        .await
        .expect("envoi");

    // 7. `server/activate` — que la branche chiffree envoie, elle.
    let trame = match recevoir(&mut ws).await {
        Recu::Binaire(b) => b,
        _ => panic!("`server/activate` doit arriver chiffre"),
    };
    let mut clair = vec![0u8; MAX_NOISE];
    let n = transport
        .read_message(&trame, &mut clair)
        .expect("dechiffrement server/activate");
    clair.truncate(n);
    let activate: serde_json::Value = serde_json::from_slice(&clair[1..]).expect("json");
    assert_eq!(activate["type"], serde_json::json!("server/activate"));

    // 8. Et le registre doit dire CHIFFRE.
    let vus = registre::pairs_vus();
    let pair = vus
        .iter()
        .find(|p| p.client_id == moi.id())
        .expect("le pair doit figurer au registre");
    assert!(
        pair.chiffre,
        "une session menee par Noise doit etre enregistree comme chiffree, mode \
         de transition arme ou non"
    );
    assert_eq!(pair.suite.as_deref(), Some(suite.nom()));

    let decrit = registre::decrire();
    let d = decrit
        .iter()
        .find(|d| d["client_id"] == serde_json::json!(moi.id()))
        .expect("le pair doit etre decrit");
    assert_eq!(d["encrypted"], serde_json::json!(true));
    assert_eq!(d["transport"], serde_json::json!("noise"));
}

// ---------------------------------------------------------------------------
// 4. Une session en clair se NOMME.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_session_en_clair_est_nommee_comme_telle_au_registre() {
    let url = point_d_acces(ModeTransition::ClairAccepte).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connexion");
    let id = identifiant_unique("nomme");
    ws.send(Message::Text(hello_en_clair(&id, "Chambre").into()))
        .await
        .expect("envoi");
    let _ = recevoir(&mut ws).await;

    let decrit = registre::decrire();
    let d = decrit
        .iter()
        .find(|d| d["client_id"] == serde_json::json!(id))
        .expect("une session admise doit figurer au registre");
    assert_eq!(
        d["encrypted"],
        serde_json::json!(false),
        "une session en clair doit se DIRE en clair : {d}"
    );
    assert_eq!(d["transport"], serde_json::json!("clair"));
    assert_eq!(
        d["suite"],
        serde_json::json!(null),
        "annoncer un nom de suite sur une session en clair ferait croire a du \
         chiffrement : {d}"
    );
    assert_eq!(d["authenticated"], serde_json::json!(false));
    assert_eq!(d["playable"], serde_json::json!(false));
    // La matiere est quand meme recoltee, sous la cle VERSIONNEE.
    assert_eq!(
        d["player_support"]["supported_formats"][0]["codec"],
        serde_json::json!("pcm"),
        "les capacites doivent etre lues meme en clair : {d}"
    );
}

// ---------------------------------------------------------------------------
// 5. Pas de rétrogradation.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_identite_deja_vue_en_noise_ne_peut_pas_revenir_en_clair() {
    // On plante d'abord un pair CHIFFRE au registre, puis on tente de reclamer
    // son identifiant en clair. Sans cette garde, le mode de transition
    // offrirait a n'importe qui sur le reseau local le moyen d'usurper une
    // enceinte connue en recopiant simplement son identifiant.
    let id = identifiant_unique("noise-prouve");
    registre::enregistrer(registre::PairVu {
        client_id: id.clone(),
        suite: Some(Suite::ChaChaPoly.nom().to_string()),
        chiffre: true,
        nom: Some("Enceinte connue".into()),
        roles: vec!["player@v1".into()],
        player_support: None,
        hello_brut: serde_json::json!({}),
        vu_a: registre::maintenant(),
    });

    let url = point_d_acces(ModeTransition::ClairAccepte).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connexion");
    ws.send(Message::Text(hello_en_clair(&id, "Usurpateur").into()))
        .await
        .expect("envoi");

    match recevoir(&mut ws).await {
        Recu::Ferme => {}
        Recu::Texte(t) => panic!(
            "une identite deja vue en Noise ne doit pas etre admise en clair, \
             recu : {t}"
        ),
        Recu::Binaire(_) => panic!("aucune trame ne doit partir"),
    }

    // Et l'entree chiffree ne doit pas avoir ete ECRASEE par la tentative.
    let vus = registre::pairs_vus();
    let pair = vus
        .iter()
        .find(|p| p.client_id == id)
        .expect("l'entree chiffree doit survivre");
    assert!(
        pair.chiffre,
        "la tentative en clair ne doit pas degrader l'entree du registre"
    );
    assert_eq!(pair.nom.as_deref(), Some("Enceinte connue"));
}

// ---------------------------------------------------------------------------
// 6. Le montage réel ne force pas le clair.
// ---------------------------------------------------------------------------

/// Garde contre « écrit mais pas branché », côté réglage.
///
/// Les témoins ci-dessus montent le routeur en lui passant un mode à la main :
/// ils ne disent rien de ce que fait l'application réelle. Le jour où quelqu'un
/// écrirait `ModeTransition::ClairAccepte` en dur à la ligne de montage, tout
/// resterait vert et Tune ouvrirait un point d'accès en clair chez chaque
/// utilisateur.
///
/// L'aiguille est **assemblée à l'exécution** : écrite en clair dans ce
/// fichier, elle serait inoffensive ici mais la même garde posée un jour dans
/// `routes/mod.rs` se trouverait elle-même.
#[test]
fn le_point_d_acces_monte_lit_le_reglage_et_ne_force_pas_le_clair() {
    let source = include_str!("../src/routes/mod.rs");
    let utiles: Vec<&str> = source
        .lines()
        .map(str::trim_start)
        .filter(|l| !l.starts_with("//"))
        .collect();

    let lecture = ["ModeTransition", "::", "en_vigueur()"].concat();
    assert!(
        utiles.iter().any(|l| l.contains(&lecture)),
        "le montage du point d'acces doit LIRE le reglage du processus ; sans \
         cet appel, le mode affiche par /devices/sendspin et le mode applique \
         pourraient diverger"
    );

    let force = ["ModeTransition", "::", "ClairAccepte"].concat();
    assert!(
        !utiles.iter().any(|l| l.contains(&force)),
        "le mode de transition ne doit JAMAIS etre force a l'assemblage : ce \
         serait ouvrir un point d'acces en clair chez tout le monde sans que \
         personne ne l'ait demande"
    );
}
