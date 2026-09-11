//! `ws://<hote>:<port>/sendspin` — le point d'accès Sendspin de Tune (#3326, S2-a).
//!
//! C'est le mode **client-initié** : l'enceinte compose vers nous. La
//! spécification exige que le serveur supporte les deux modes de découverte, et
//! la phase 1 n'avait livré que le parcours ; l'annonce
//! `_sendspin-server._tcp.local.` qui mène ici est posée par
//! `tune_core::discovery::mdns::MdnsScanner::register_sendspin_server`.
//!
//! ## Pourquoi cette route n'est pas derrière l'authentification de Tune
//!
//! Toutes les autres routes WebSocket du serveur passent par `WsAuthorized`.
//! Celle-ci ne le peut pas : une enceinte Sendspin ne connaît ni nos jetons ni
//! nos en-têtes, et la spécification lui fait ouvrir la conversation par un
//! `client/init` en clair. L'authentification du pair est **le travail de la
//! couche Noise**, pas celui d'un extracteur axum.
//!
//! Il faut donc être net sur ce que S2-a garantit : la PSK employée est la
//! **Sentinelle**, une constante publiée. Le canal est chiffré et intègre ;
//! **le pair n'est pas authentifié**. N'importe qui sur le réseau local peut
//! mener cette poignée de main à bien. C'est acceptable ici parce que rien
//! n'est offert derrière : aucun son, aucune commande, aucune donnée de
//! bibliothèque — la connexion s'arrête juste après `server/activate`. Le jour
//! où S2-c y branchera de l'audio, S2-b devra avoir apporté les PSK `lt`/`pr`.
//!
//! ## Ce qui est délibérément absent
//!
//! Pas d'`OutputTarget`, pas d'enregistrement de sortie, pas de zone. Deux
//! décisions de produit ne sont pas tranchées (ce que vaut « pause » côté
//! Sendspin, et le fait que l'identité d'un appareil n'existe pas avant la
//! connexion), et elles appartiennent à Bertrand.

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use tracing::{debug, info, warn};

use tune_core::sendspin::{
    ErreurSendspin, PoigneeServeur, identite_du_serveur, messages, psk, registre,
};

/// Délai maximal d'attente d'un message du pair.
///
/// La spécification abandonne une connexion provisoire restée sans
/// `server/activate` au bout de 30 s. Nous nous tenons sous cette borne : une
/// enceinte qui se tait ne doit pas immobiliser une tâche indéfiniment.
const DELAI_MESSAGE: std::time::Duration = std::time::Duration::from_secs(10);

/// Le routeur du point d'accès.
///
/// Générique sur l'état : ce parcours n'en lit aucun — l'identité du serveur
/// vit dans `tune_core::sendspin`, et rien ici ne touche à la base ni aux
/// zones. Le rendre générique n'est pas de la coquetterie : c'est ce qui
/// permet au témoin de monter la VRAIE route, sans fabriquer un `AppState`
/// complet dont la séquence ne dépend pas.
pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/", get(point_d_acces))
}

async fn point_d_acces(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = conduire(socket).await {
            // La specification n'a AUCUN message d'erreur applicatif : la seule
            // reaction admise est de fermer sans rien dire au pair. Le motif
            // reste donc chez nous, dans le journal.
            warn!(error = %e, "sendspin_poignee_echouee");
        }
    })
}

/// Mène la séquence complète de S2-a.
///
/// `client/init` → `server/init` → `noise/handshake` ×2 → `server/hello` →
/// `client/hello` → `server/activate`.
async fn conduire(mut socket: WebSocket) -> Result<(), ErreurSendspin> {
    // 1. `client/init`, en clair. Le texte est garde TEL QUEL : il entre dans
    //    le prologue Noise octet pour octet.
    let client_init_texte = lire_texte(&mut socket, "client/init").await?;
    debug!(
        taille = client_init_texte.len(),
        "sendspin_client_init_recu"
    );

    // 2. La poignee de main est batie avant que quoi que ce soit ne parte : un
    //    pair que nous n'admettons pas ne doit pas meme voir notre server/init.
    let mut poignee = PoigneeServeur::accueillir(
        identite_du_serveur(),
        &client_init_texte,
        &psk::sentinelle(),
    )?;
    let client_id = poignee.client_id().to_string();
    let suite = poignee.suite();
    info!(%client_id, %suite, "sendspin_client_init_admis");

    // 3. `server/init` puis le message Noise 1, dos a dos : la specification
    //    n'attend aucun message du client entre les deux.
    envoyer_texte(&mut socket, poignee.server_init_texte().to_string()).await?;
    let message_un = poignee.message_un()?;
    envoyer_texte(&mut socket, message_un).await?;

    // 4. Le message Noise 2 ferme la poignee de main et ouvre le tuyau.
    let message_deux = lire_texte(&mut socket, "noise/handshake").await?;
    let (mut transport, infos) = poignee.message_deux(&message_deux)?;
    info!(
        client_id = %infos.client_id,
        suite = %infos.suite,
        psk_id = %infos.psk_id,
        "sendspin_poignee_etablie"
    );

    // 5. `server/hello` — premiere parole chiffree.
    let hello = messages::Enveloppe::nouvelle(
        messages::TYPE_SERVER_HELLO,
        messages::ServerHello {
            name: format!("Tune ({})", tune_core::discovery::system_hostname()),
            languages: None,
        },
    );
    let texte = serde_json::to_string(&hello)
        .map_err(|e| ErreurSendspin::MessageIllisible(format!("server/hello : {e}")))?;
    let trame = transport.chiffrer_json(&texte)?;
    envoyer_binaire(&mut socket, trame).await?;

    // 6. `client/hello` — LA PORTE DE SORTIE DE S2-a. On y apprend qui parle.
    let recu = lire_binaire(&mut socket, "client/hello").await?;
    let texte = transport.dechiffrer_json(&recu)?;
    let brute: messages::EnveloppeBrute = serde_json::from_str(&texte)
        .map_err(|e| ErreurSendspin::MessageIllisible(format!("client/hello : {e}")))?;
    if brute.type_message != messages::TYPE_CLIENT_HELLO {
        return Err(ErreurSendspin::MessageIllisible(format!(
            "{} attendu, {} recu",
            messages::TYPE_CLIENT_HELLO,
            brute.type_message
        )));
    }
    let hello_brut = brute.payload.clone();
    let client_hello: messages::ClientHello = serde_json::from_value(brute.payload)
        .map_err(|e| ErreurSendspin::MessageIllisible(format!("charge client/hello : {e}")))?;

    // Journalise le hello ENTIER : c'est la matiere que S2-a existe pour
    // recolter, sur une specification qui bouge encore. Un resume nous ferait
    // perdre le champ que nous ne savons pas encore nommer.
    info!(
        client_id = %infos.client_id,
        nom = ?client_hello.name,
        roles = ?client_hello.supported_roles,
        sait_jouer = client_hello.sait_jouer(),
        player_support = %client_hello
            .support_du_lecteur()
            .map(std::string::ToString::to_string)
            .unwrap_or_else(|| "absent".into()),
        hello = %hello_brut,
        "sendspin_client_hello_recu"
    );

    registre::enregistrer(registre::PairVu {
        client_id: infos.client_id.clone(),
        suite: infos.suite.to_string(),
        nom: client_hello.name.clone(),
        roles: client_hello.supported_roles.clone(),
        player_support: client_hello.support_du_lecteur().cloned(),
        hello_brut,
        vu_a: registre::maintenant(),
    });

    // 7. `server/activate` avec une liste d'activites VIDE. Ni lecture ni
    //    appairage : S2-a ne revendique rien. Ce message est neanmoins du : sans
    //    lui, l'enceinte abandonne la connexion provisoire au bout de 30 s.
    let activate = messages::Enveloppe::nouvelle(
        messages::TYPE_SERVER_ACTIVATE,
        messages::ServerActivate {
            activities: Vec::new(),
            active_roles: None,
        },
    );
    let texte = serde_json::to_string(&activate)
        .map_err(|e| ErreurSendspin::MessageIllisible(format!("server/activate : {e}")))?;
    let trame = transport.chiffrer_json(&texte)?;
    envoyer_binaire(&mut socket, trame).await?;
    info!(client_id = %infos.client_id, "sendspin_server_activate_envoye");

    // 8. Fin de S2-a. Rien a jouer, donc on rend la main proprement plutot que
    //    de tenir une connexion qui ne servirait a rien.
    let _ = socket.send(Message::Close(None)).await;
    Ok(())
}

async fn lire_texte(socket: &mut WebSocket, quoi: &str) -> Result<String, ErreurSendspin> {
    match tokio::time::timeout(DELAI_MESSAGE, socket.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => Ok(t.to_string()),
        Ok(Some(Ok(autre))) => Err(ErreurSendspin::MessageIllisible(format!(
            "{quoi} attendu en trame TEXTE, {} recue",
            nom_de_trame(&autre)
        ))),
        Ok(Some(Err(e))) => Err(ErreurSendspin::MessageIllisible(format!("{quoi} : {e}"))),
        Ok(None) => Err(ErreurSendspin::MessageIllisible(format!(
            "connexion fermee avant {quoi}"
        ))),
        Err(_) => Err(ErreurSendspin::MessageIllisible(format!(
            "aucun {quoi} en {} s",
            DELAI_MESSAGE.as_secs()
        ))),
    }
}

async fn lire_binaire(socket: &mut WebSocket, quoi: &str) -> Result<Vec<u8>, ErreurSendspin> {
    match tokio::time::timeout(DELAI_MESSAGE, socket.recv()).await {
        Ok(Some(Ok(Message::Binary(b)))) => Ok(b.to_vec()),
        Ok(Some(Ok(autre))) => Err(ErreurSendspin::MessageIllisible(format!(
            "{quoi} attendu en trame BINAIRE, {} recue",
            nom_de_trame(&autre)
        ))),
        Ok(Some(Err(e))) => Err(ErreurSendspin::MessageIllisible(format!("{quoi} : {e}"))),
        Ok(None) => Err(ErreurSendspin::MessageIllisible(format!(
            "connexion fermee avant {quoi}"
        ))),
        Err(_) => Err(ErreurSendspin::MessageIllisible(format!(
            "aucun {quoi} en {} s",
            DELAI_MESSAGE.as_secs()
        ))),
    }
}

/// Nomme le genre de trame reçue.
///
/// Sans ça, « trame inattendue » ne dit pas si le pair a envoyé du binaire trop
/// tôt ou s'il a simplement raccroché — deux causes très différentes.
fn nom_de_trame(message: &Message) -> &'static str {
    match message {
        Message::Text(_) => "texte",
        Message::Binary(_) => "binaire",
        Message::Ping(_) => "ping",
        Message::Pong(_) => "pong",
        Message::Close(_) => "fermeture",
    }
}

async fn envoyer_texte(socket: &mut WebSocket, texte: String) -> Result<(), ErreurSendspin> {
    socket
        .send(Message::Text(texte.into()))
        .await
        .map_err(|e| ErreurSendspin::MessageIllisible(format!("envoi texte : {e}")))
}

async fn envoyer_binaire(socket: &mut WebSocket, trame: Vec<u8>) -> Result<(), ErreurSendspin> {
    socket
        .send(Message::Binary(trame.into()))
        .await
        .map_err(|e| ErreurSendspin::MessageIllisible(format!("envoi binaire : {e}")))
}
