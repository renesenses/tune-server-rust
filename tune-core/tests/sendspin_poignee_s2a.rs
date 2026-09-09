//! Témoin de la poignée de main Sendspin, côté serveur (#3326, brique S2-a).
//!
//! ## Ce que ce fichier prouve — et ce qu'il ne prouve pas
//!
//! Il prouve que notre serveur mène la séquence `client/init` → `server/init` →
//! `noise/handshake` ×2 jusqu'au transport chiffré, **dans les deux suites que
//! la spécification impose au serveur**, et qu'il refuse ce qu'il doit refuser.
//!
//! Il **ne prouve pas** l'interopérabilité. Un serveur qui ne réussit la
//! poignée de main que contre un client écrit par la même main n'a rien établi :
//! les deux côtés peuvent partager la même erreur de lecture de la
//! spécification et se comprendre parfaitement. C'est pourquoi la porte de
//! sortie de S2-a est ailleurs — la trace d'un `client/hello` venu d'une
//! implémentation tierce — et pourquoi ce fichier ne s'en réclame pas.
//!
//! ## Pourquoi le répondeur est réécrit ici
//!
//! Le côté client est monté dans ce fichier, directement sur `snow`, et
//! n'emprunte RIEN à `tune_core::sendspin::poignee`. Un témoin qui appellerait
//! un répondeur fourni par le code testé se contenterait de vérifier que le
//! code est d'accord avec lui-même. En particulier, le prologue est reconstruit
//! ici à partir des deux textes tels qu'ils ont circulé : si le serveur changeait
//! sa façon de le composer, ce fichier rougirait.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use snow::{Builder, HandshakeState};

use tune_core::sendspin::identite::Identite;
use tune_core::sendspin::messages::{
    ChargeMessageUn, EnveloppeBrute, NoiseHandshake, TYPE_NOISE_HANDSHAKE,
};
use tune_core::sendspin::poignee::PoigneeServeur;
use tune_core::sendspin::psk;
use tune_core::sendspin::suite::Suite;
use tune_core::sendspin::transport::{MAX_CLAIR, TransportNoise};

const MAX_NOISE: usize = 65535;

/// Le `client/init` tel qu'une enceinte l'écrirait.
fn client_init(enceinte: &Identite, suite: Suite) -> String {
    serde_json::json!({
        "type": "client/init",
        "payload": {
            "client_id": enceinte.id(),
            "version": 1,
            "suite": suite.nom(),
        }
    })
    .to_string()
}

/// Le côté enceinte, monté sur `snow` sans passer par notre code.
struct Enceinte {
    etat: HandshakeState,
}

impl Enceinte {
    fn nouvelle(
        enceinte: &Identite,
        serveur_public: &[u8; 32],
        prologue: &[u8],
        suite: Suite,
        psk: &[u8; 32],
    ) -> Self {
        let params = suite.motif_noise().parse().expect("motif noise");
        let etat = Builder::new(params)
            .local_private_key(enceinte.prive())
            .expect("cle locale")
            .remote_public_key(serveur_public)
            .expect("cle serveur")
            .prologue(prologue)
            .expect("prologue")
            .psk(Suite::position_psk(), psk)
            .expect("psk")
            .build_responder()
            .expect("repondeur");
        Self { etat }
    }

    /// Lit le message 1 et rend le `psk_id` que le serveur y a glissé.
    fn lire_un(&mut self, texte: &str) -> String {
        self.essayer_lire_un(texte)
            .expect("lecture du message noise 1")
    }

    /// Idem, mais faillible : le prologue entre dans le hachage dès le premier
    /// message, donc c'est ICI qu'un écart se voit.
    fn essayer_lire_un(&mut self, texte: &str) -> Result<String, snow::Error> {
        let brute: EnveloppeBrute = serde_json::from_str(texte).expect("enveloppe");
        assert_eq!(brute.type_message, TYPE_NOISE_HANDSHAKE);
        let hs: NoiseHandshake = serde_json::from_value(brute.payload).expect("charge");
        let brut = URL_SAFE_NO_PAD.decode(&hs.data).expect("base64url");
        let mut tampon = vec![0u8; MAX_NOISE];
        let n = self.etat.read_message(&brut, &mut tampon)?;
        tampon.truncate(n);
        let charge: ChargeMessageUn = serde_json::from_slice(&tampon).expect("psk_id");
        Ok(charge.psk_id)
    }

    fn ecrire_deux(&mut self) -> String {
        let mut tampon = vec![0u8; MAX_NOISE];
        let n = self
            .etat
            .write_message(&[], &mut tampon)
            .expect("ecriture du message noise 2");
        tampon.truncate(n);
        serde_json::json!({
            "type": "noise/handshake",
            "payload": { "data": URL_SAFE_NO_PAD.encode(&tampon) }
        })
        .to_string()
    }
}

/// Mène la séquence complète et rend les deux bouts du tuyau.
fn poignee_complete(suite: Suite) -> (TransportNoise, String, String) {
    let serveur = Identite::generer();
    let enceinte = Identite::generer();
    let sentinelle = psk::sentinelle();

    let init = client_init(&enceinte, suite);
    let mut poignee =
        PoigneeServeur::accueillir(&serveur, &init, &sentinelle).expect("client/init admis");

    // Le prologue est reconstruit ICI, a partir des deux textes tels qu'ils
    // circulent — le `client/init` envoye et le `server/init` rendu par le
    // serveur.
    let mut prologue = Vec::new();
    prologue.extend_from_slice(init.as_bytes());
    prologue.extend_from_slice(poignee.server_init_texte().as_bytes());

    let mut cote_enceinte =
        Enceinte::nouvelle(&enceinte, serveur.public(), &prologue, suite, &sentinelle);

    let message_un = poignee.message_un().expect("message noise 1");
    let psk_id_recu = cote_enceinte.lire_un(&message_un);
    let message_deux = cote_enceinte.ecrire_deux();
    let (transport, infos) = poignee
        .message_deux(&message_deux)
        .expect("bascule en transport");

    assert_eq!(
        infos.client_id,
        enceinte.id(),
        "le serveur doit connaitre l'identite de l'enceinte, qui EST sa cle publique"
    );
    assert_eq!(infos.suite, suite);
    (transport, psk_id_recu, infos.psk_id)
}

#[test]
fn la_poignee_reussit_dans_les_deux_suites_imposees_au_serveur() {
    // La specification est asymetrique : le client choisit UNE suite, le
    // serveur doit savoir les DEUX. Si l'une des deux tombe, ce test la nomme.
    for suite in Suite::toutes() {
        let (mut transport, psk_id_recu, psk_id_annonce) = poignee_complete(suite);
        assert_eq!(
            psk_id_recu, psk_id_annonce,
            "le psk_id lu par l'enceinte doit etre celui que le serveur a glisse \
             dans le message 1 ({suite})"
        );
        assert_eq!(
            psk_id_annonce,
            psk::identifiant(&psk::sentinelle()),
            "S2-a n'emploie QUE la Sentinelle : une autre PSK ici voudrait dire \
             que l'appairage (S2-b) a fuite dans cette brique ({suite})"
        );
        // Le tuyau doit reellement chiffrer : un aller simple suffit a le dire,
        // et on verifie que l'octet de type survit.
        let trame = transport
            .chiffrer_json(r#"{"type":"server/hello","payload":{"name":"Tune"}}"#)
            .expect("chiffrement");
        assert!(
            !trame.windows(4).any(|f| f == b"Tune"),
            "la trame doit etre CHIFFREE : « Tune » ne doit pas y apparaitre en clair ({suite})"
        );
    }
}

#[test]
fn un_prologue_reconstruit_fait_echouer_la_poignee() {
    // Le piege d'interoperabilite numero un. Le prologue est fait des OCTETS
    // EXACTS des deux messages en clair ; re-serialiser le `server/init` (ne
    // serait-ce qu'avec une espace de plus) donne un prologue different, et
    // Noise refuse — au message 2, sans jamais nommer la cause.
    let suite = Suite::ChaChaPoly;
    let serveur = Identite::generer();
    let enceinte = Identite::generer();
    let sentinelle = psk::sentinelle();

    let init = client_init(&enceinte, suite);
    let mut poignee =
        PoigneeServeur::accueillir(&serveur, &init, &sentinelle).expect("client/init admis");

    // Une espace de plus : JSON equivalent, octets differents.
    let server_init_reserialise = format!("{} ", poignee.server_init_texte());
    assert_ne!(
        server_init_reserialise.as_bytes(),
        poignee.server_init_texte().as_bytes(),
        "le sabotage doit reellement changer les octets, sinon ce test ne prouve rien"
    );

    let mut prologue = Vec::new();
    prologue.extend_from_slice(init.as_bytes());
    prologue.extend_from_slice(server_init_reserialise.as_bytes());

    let mut cote_enceinte =
        Enceinte::nouvelle(&enceinte, serveur.public(), &prologue, suite, &sentinelle);

    let message_un = poignee.message_un().expect("message noise 1");

    // MESURE, et non supposition : l'ecart est fatal DES le message 1. Le
    // prologue entre dans le hachage `h` avant que la charge utile du premier
    // message ne soit chiffree, donc l'enceinte echoue en la dechiffrant et
    // n'atteint jamais le message 2. On note l'endroit exact, parce que c'est
    // celui qu'on lira dans un journal le jour ou un vrai lecteur refusera
    // notre poignee de main.
    let erreur = cote_enceinte
        .essayer_lire_un(&message_un)
        .expect_err("un prologue different DOIT faire echouer la poignee");
    assert!(
        matches!(erreur, snow::Error::Decrypt),
        "l'echec doit etre un refus de dechiffrement Noise, pas autre chose : {erreur:?}"
    );
}

#[test]
fn une_psk_differente_fait_echouer_la_poignee() {
    let suite = Suite::AesGcm;
    let serveur = Identite::generer();
    let enceinte = Identite::generer();
    let sentinelle = psk::sentinelle();
    let mut autre = sentinelle;
    autre[0] ^= 0xff;

    let init = client_init(&enceinte, suite);
    let mut poignee =
        PoigneeServeur::accueillir(&serveur, &init, &sentinelle).expect("client/init admis");

    let mut prologue = Vec::new();
    prologue.extend_from_slice(init.as_bytes());
    prologue.extend_from_slice(poignee.server_init_texte().as_bytes());

    let mut cote_enceinte =
        Enceinte::nouvelle(&enceinte, serveur.public(), &prologue, suite, &autre);

    let message_un = poignee.message_un().expect("message noise 1");
    let _ = cote_enceinte.lire_un(&message_un);
    let message_deux = cote_enceinte.ecrire_deux();

    assert!(
        poignee.message_deux(&message_deux).is_err(),
        "une PSK differente doit faire echouer la poignee : sans ca, le jour ou \
         S2-b apportera les PSK d'appairage, n'importe qui entrerait"
    );
}

#[test]
fn un_client_init_de_version_inconnue_est_refuse_avant_toute_reponse() {
    let serveur = Identite::generer();
    let enceinte = Identite::generer();
    let init = serde_json::json!({
        "type": "client/init",
        "payload": {"client_id": enceinte.id(), "version": 2, "suite": Suite::ChaChaPoly.nom()}
    })
    .to_string();
    let erreur = PoigneeServeur::accueillir(&serveur, &init, &psk::sentinelle())
        .expect_err("une version inconnue doit etre refusee");
    assert!(
        erreur.to_string().contains('2'),
        "le refus doit NOMMER la version recue : {erreur}"
    );
}

#[test]
fn une_suite_hors_specification_est_refusee() {
    let serveur = Identite::generer();
    let enceinte = Identite::generer();
    let init = serde_json::json!({
        "type": "client/init",
        "payload": {"client_id": enceinte.id(), "version": 1, "suite": "25519_AESGCM_SHA512"}
    })
    .to_string();
    let erreur = PoigneeServeur::accueillir(&serveur, &init, &psk::sentinelle())
        .expect_err("une suite hors specification doit etre refusee");
    assert!(
        erreur.to_string().contains("25519_AESGCM_SHA512"),
        "le refus doit nommer la suite : {erreur}"
    );
}

#[test]
fn un_message_hors_sequence_est_refuse_en_nommant_ce_qui_etait_attendu() {
    let serveur = Identite::generer();
    let enceinte = Identite::generer();
    // Un `server/init` la ou un `client/init` est attendu.
    let init = serde_json::json!({
        "type": "server/init",
        "payload": {"server_id": enceinte.id(), "version": 1}
    })
    .to_string();
    let erreur = PoigneeServeur::accueillir(&serveur, &init, &psk::sentinelle())
        .expect_err("un type hors sequence doit etre refuse");
    assert!(
        erreur.to_string().contains("client/init"),
        "le refus doit dire ce qui etait ATTENDU, pas seulement que ca a rate : {erreur}"
    );
}

#[test]
fn le_transport_refuse_ce_qui_depasse_une_trame_au_lieu_de_le_tronquer() {
    // La fragmentation est le sujet de S2-c. Tant qu'elle n'est pas ecrite, une
    // charge trop grande doit etre REFUSEE : tronquer en silence produirait un
    // flux corrompu que rien ne nommerait.
    let (mut transport, _, _) = poignee_complete(Suite::ChaChaPoly);
    let trop = vec![0u8; MAX_CLAIR + 1];
    let erreur = transport
        .chiffrer(&trop)
        .expect_err("au-dela d'une trame, le refus doit etre explicite");
    assert!(
        erreur.to_string().contains("S2-c"),
        "le refus doit dire OU la fragmentation sera traitee : {erreur}"
    );
    // La borne elle-meme reste franchissable.
    assert!(
        transport.chiffrer(&vec![7u8; MAX_CLAIR]).is_ok(),
        "exactement une trame doit passer"
    );
}
