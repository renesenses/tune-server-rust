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
//! ## Le mode de transition (11/09/2026)
//!
//! Ce point d'accès a désormais **deux entrées**, et l'aiguillage se fait sur le
//! TYPE du premier message reçu, comme dans l'implémentation de référence :
//!
//! | Premier message | Ce qui se passe |
//! |---|---|
//! | `client/init` | poignée de main Noise — **toujours**, mode ou pas |
//! | `client/hello` | admis **en clair** si et seulement si le mode de transition est armé |
//! | autre chose | fermeture, sans message applicatif |
//!
//! Trois points sur lesquels ce fichier ne transige pas :
//!
//! 1. **Ce n'est pas un repli.** Il n'existe aucun chemin où un échec de la
//!    branche chiffrée fait retomber en clair. Un pair qui envoie `client/init`
//!    obtient Noise ou rien. Le témoin
//!    `avec_le_mode_de_transition_un_client_capable_de_noise_negocie_quand_meme_noise`
//!    garde exactement ce point.
//! 2. **Le défaut est le refus** ([`ModeTransition::ChiffrementSeul`]).
//! 3. **Une session en clair se nomme.** `warn!` au journal, `encrypted: false`
//!    et `transport: "clair"` au registre, mode publié par
//!    `GET /devices/sendspin`.
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

use tune_core::sendspin::transition::{self, ModeTransition};
use tune_core::sendspin::{
    ErreurSendspin, PoigneeServeur, VERSION_PROTOCOLE, identite_du_serveur, messages, psk, registre,
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
///
/// Le mode est un **argument**, pas une lecture d'environnement enfouie ici.
/// C'est la forme qu'a l'implémentation de référence (`allow_unencrypted` est
/// un paramètre du constructeur de son serveur), et c'est ce qui permet à un
/// témoin de mesurer les deux modes sans toucher à l'environnement du
/// processus — `std::env::set_var` dans un test casse la suite `--workspace`.
pub fn router<S>(mode: ModeTransition) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new().route(
        "/",
        get(move |ws: WebSocketUpgrade| async move { point_d_acces(ws, mode).await }),
    )
}

async fn point_d_acces(ws: WebSocketUpgrade, mode: ModeTransition) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = conduire(socket, mode).await {
            // La specification n'a AUCUN message d'erreur applicatif : la seule
            // reaction admise est de fermer sans rien dire au pair. Le motif
            // reste donc chez nous, dans le journal.
            warn!(error = %e, mode = mode.nom(), "sendspin_poignee_echouee");
        }
    })
}

/// Aiguille sur le TYPE du premier message, et rien d'autre.
///
/// Mesuré le 11/09/2026 dans `aiosendspin` (git, `_establish_transport`) :
/// c'est exactement cette forme. Le point important est que la décision se
/// prend sur ce que le pair **demande**, jamais sur un échec : il n'existe
/// aucune arête qui mène de « Noise a raté » à « tant pis, en clair ».
async fn conduire(mut socket: WebSocket, mode: ModeTransition) -> Result<(), ErreurSendspin> {
    let premier = lire_texte(&mut socket, "premier message").await?;
    match messages::type_du_message(&premier).as_deref() {
        Some(messages::TYPE_CLIENT_INIT) => conduire_chiffre(socket, premier).await,
        Some(messages::TYPE_CLIENT_HELLO) => conduire_en_clair(socket, premier, mode).await,
        autre => Err(ErreurSendspin::MessageIllisible(format!(
            "premier message : {} attendu ou {} (mode de transition), recu {}",
            messages::TYPE_CLIENT_INIT,
            messages::TYPE_CLIENT_HELLO,
            autre.unwrap_or("un message sans type lisible")
        ))),
    }
}

/// Mène la séquence complète de S2-a, **chiffrée**.
///
/// `client/init` → `server/init` → `noise/handshake` ×2 → `server/hello` →
/// `client/hello` → `server/activate`.
///
/// Rien ici n'a changé avec le mode de transition, et c'est délibéré : la porte
/// supplémentaire ne doit pas assouplir d'un pouce celle qui existait.
async fn conduire_chiffre(
    mut socket: WebSocket,
    client_init_texte: String,
) -> Result<(), ErreurSendspin> {
    // 1. `client/init`, en clair. Le texte est garde TEL QUEL : il entre dans
    //    le prologue Noise octet pour octet.
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
        suite: Some(infos.suite.to_string()),
        // Noise a mené jusqu'au bout : le `client_id` est une clé publique
        // PROUVÉE, et les trames sont chiffrées.
        chiffre: true,
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

/// Le **mode de transition** : un `client/hello` en clair comme premier message.
///
/// ## Ce que cette branche fait, et pourquoi exactement cette forme
///
/// Tout ce qui suit est **mesuré** le 11/09/2026, pas déduit : sur le paquet
/// publié `aiosendspin` 6.0.5 (celui dont dépend le lecteur de référence
/// `sendspin` 7.5.0) et sur le serveur de référence au dépôt git.
///
/// 1. Le pair n'a pas mené de poignée de main : son `client_id` et sa `version`
///    voyagent **dans le hello**. Le lecteur publié les rend obligatoires.
/// 2. La réponse est un `server/hello` **hérité** — cinq champs, tous
///    obligatoires — envoyé en trame **TEXTE**, pas en binaire chiffré.
/// 3. **Aucun `server/activate` ne suit** : ni `client/init` ni `server/activate`
///    n'apparaissent dans 6.0.5. Le hello hérité porte à lui seul
///    `active_roles` et `connection_reason`. Le serveur de référence écrit la
///    même chose : « the legacy hello replaces server/hello plus activate ».
///
/// ## Ce que cette branche refuse
///
/// - Tout, si le mode n'est pas armé — et c'est le défaut.
/// - Une **rétrogradation** : un `client_id` déjà vu en Noise ne revient pas en
///   clair (le serveur de référence fait le même refus contre son magasin
///   d'appairage).
/// - Une version de protocole autre que 1.
async fn conduire_en_clair(
    mut socket: WebSocket,
    hello_texte: String,
    mode: ModeTransition,
) -> Result<(), ErreurSendspin> {
    if !mode.accepte_le_clair() {
        // Le refus est l'etat NORMAL, pas une panne : il se journalise en
        // disant quoi armer, sinon un testeur passe une heure a chercher
        // pourquoi son enceinte ne repond pas.
        warn!(
            reglage = transition::VARIABLE_ENVIRONNEMENT,
            "sendspin_client_hello_en_clair_refuse"
        );
        return Err(ErreurSendspin::ClairRefuse);
    }

    let brute: messages::EnveloppeBrute = serde_json::from_str(&hello_texte)
        .map_err(|e| ErreurSendspin::MessageIllisible(format!("client/hello en clair : {e}")))?;
    let hello_brut = brute.payload.clone();
    let client_hello: messages::ClientHello =
        serde_json::from_value(brute.payload).map_err(|e| {
            ErreurSendspin::MessageIllisible(format!("charge client/hello en clair : {e}"))
        })?;

    // La version voyage ICI faute de `client/init`. Absente, on ne la reproche
    // pas — le serveur de reference se contente de noter l'ecart ; presente et
    // fausse, on ferme, comme lui.
    if let Some(version) = client_hello.version
        && version != VERSION_PROTOCOLE
    {
        return Err(ErreurSendspin::VersionInconnue(version));
    }

    let Some(client_id) = client_hello.client_id.clone() else {
        return Err(ErreurSendspin::IdentifiantInvalide(
            "client/hello en clair sans client_id : aucune poignee de main n'a \
             pu en fournir un"
                .into(),
        ));
    };

    // PROTECTION CONTRE LA RETROGRADATION. Un pair qui a deja prouve qu'il sait
    // parler Noise ne redescend pas en clair : sinon n'importe qui sur le
    // reseau local usurperait une enceinte connue en recopiant son identifiant.
    if registre::deja_vu_chiffre(&client_id) {
        warn!(%client_id, "sendspin_retrogradation_refusee");
        return Err(ErreurSendspin::RetrogradationRefusee(client_id));
    }

    // `warn!`, pas `info!` : une session en clair est un ECART, meme voulu. Le
    // serveur de reference journalise au meme niveau (« Accepting unencrypted
    // legacy connection »).
    warn!(
        %client_id,
        nom = ?client_hello.name,
        roles = ?client_hello.supported_roles,
        reglage = transition::VARIABLE_ENVIRONNEMENT,
        "sendspin_connexion_en_clair_acceptee"
    );
    info!(
        %client_id,
        sait_jouer = client_hello.sait_jouer(),
        player_support = %client_hello
            .support_du_lecteur()
            .map(std::string::ToString::to_string)
            .unwrap_or_else(|| "absent".into()),
        hello = %hello_brut,
        "sendspin_client_hello_en_clair_recu"
    );

    registre::enregistrer(registre::PairVu {
        client_id: client_id.clone(),
        // Aucune suite : il n'y a rien de chiffre. Annoncer un nom de suite ici
        // ferait croire au contraire.
        suite: None,
        chiffre: false,
        nom: client_hello.name.clone(),
        roles: client_hello.supported_roles.clone(),
        player_support: client_hello.support_du_lecteur().cloned(),
        hello_brut,
        vu_a: registre::maintenant(),
    });

    let herite = messages::Enveloppe::nouvelle(
        messages::TYPE_SERVER_HELLO,
        messages::ServerHelloHerite {
            server_id: identite_du_serveur().id(),
            name: format!("Tune ({})", tune_core::discovery::system_hostname()),
            version: VERSION_PROTOCOLE,
            // Aucun role actif : S2-a ne joue rien, et une connexion en clair
            // ne doit de toute facon jamais porter d'audio tant que S2-b n'a
            // pas apporte l'authentification du pair.
            active_roles: Vec::new(),
            connection_reason: messages::RAISON_CONNEXION_DECOUVERTE.to_string(),
        },
    );
    let texte = serde_json::to_string(&herite)
        .map_err(|e| ErreurSendspin::MessageIllisible(format!("server/hello herite : {e}")))?;
    envoyer_texte(&mut socket, texte).await?;
    info!(%client_id, "sendspin_server_hello_herite_envoye");

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
