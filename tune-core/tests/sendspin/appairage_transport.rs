//! Temoin S2-b : selection liee au pair, perte de cle et re-echange.
//! Le repondeur utilise snow directement ; l'interoperabilite tierce est separee.
use super::*;
use tune_core::sendspin::poignee::InfosPair;
use tune_core::sendspin::psk::{CategoriePsk, PskPair};

impl Enceinte {
    fn lire_charge(&mut self, texte: &str) -> serde_json::Value {
        let message: serde_json::Value = serde_json::from_str(texte).unwrap();
        let brut = URL_SAFE_NO_PAD
            .decode(message["payload"]["data"].as_str().unwrap())
            .unwrap();
        let mut tampon = vec![0; MAX_NOISE];
        let n = self.etat.read_message(&brut, &mut tampon).unwrap();
        serde_json::from_slice(&tampon[..n]).unwrap()
    }
}

fn demarrer(
    serveur: &Identite,
    client: &Identite,
    suite: Suite,
    psk: &PskPair,
    secret_client: &[u8; 32],
) -> (PoigneeServeur, Enceinte) {
    let init = client_init(client, suite);
    let mut poignee = PoigneeServeur::accueillir_avec_psk(serveur, &init, psk).unwrap();
    let prologue = [init.as_bytes(), poignee.server_init_texte().as_bytes()].concat();
    let mut enceinte =
        Enceinte::nouvelle(client, serveur.public(), &prologue, suite, secret_client);
    let charge = enceinte.lire_charge(&poignee.message_un().unwrap());
    let categorie = match psk.categorie() {
        CategoriePsk::Sentinelle => "sn",
        CategoriePsk::Appairage => "pr",
        CategoriePsk::LongueDuree => "lt",
    };
    assert_eq!(
        charge,
        serde_json::json!({
            "psk_id": psk.identifiant(), "psk_category": categorie
        }),
        "le message Noise 1 doit lier l'identifiant ET la categorie de PSK"
    );
    (poignee, enceinte)
}

fn etablir(
    serveur: &Identite,
    client: &Identite,
    suite: Suite,
    psk: &PskPair,
    secret_client: &[u8; 32],
) -> (TransportNoise, snow::TransportState, InfosPair) {
    let (poignee, mut enceinte) = demarrer(serveur, client, suite, psk, secret_client);
    let deux = enceinte.ecrire_deux();
    let (transport, infos) = poignee.message_deux(&deux).unwrap();
    assert_eq!(
        infos.condensat_poignee.as_slice(),
        enceinte.etat.get_handshake_hash(),
        "le condensat lie le prochain appairage a CETTE connexion"
    );
    (
        transport,
        enceinte.etat.into_transport_mode().unwrap(),
        infos,
    )
}

fn verifier_transport(mut serveur: TransportNoise, mut client: snow::TransportState) {
    let chiffre = serveur.chiffrer_json(r#"{"preuve":"serveur"}"#).unwrap();
    let mut clair = vec![0; MAX_NOISE];
    let n = client.read_message(&chiffre, &mut clair).unwrap();
    assert_eq!(&clair[..n], b"\0{\"preuve\":\"serveur\"}");
    let mut chiffre = vec![0; MAX_NOISE];
    let n = client
        .write_message(b"\0{\"preuve\":\"client\"}", &mut chiffre)
        .unwrap();
    assert_eq!(
        serveur.dechiffrer_json(&chiffre[..n]).unwrap(),
        r#"{"preuve":"client"}"#
    );
}

#[test]
fn i3326_les_trois_categories_sont_liees_au_message_noise_dans_les_deux_suites() {
    for suite in Suite::toutes() {
        let serveur = Identite::generer();
        let client = Identite::generer();
        let secrets = [
            PskPair::sentinelle(),
            PskPair::pour_pair(&client.id(), [17; 32], CategoriePsk::Appairage).unwrap(),
            PskPair::pour_pair(&client.id(), [29; 32], CategoriePsk::LongueDuree).unwrap(),
        ];
        for psk in secrets {
            let (a, b, infos) = etablir(&serveur, &client, suite, &psk, psk.secret());
            assert_eq!(
                infos.categorie_psk,
                psk.categorie(),
                "la categorie doit survivre a Noise"
            );
            assert!(!infos.identifiant_perdu);
            verifier_transport(a, b);
        }
    }
}

#[test]
fn i3326_une_cle_perdue_signale_le_repli_sans_authentifier_ni_effacer_le_record() {
    for suite in Suite::toutes() {
        for categorie in [CategoriePsk::Appairage, CategoriePsk::LongueDuree] {
            let serveur = Identite::generer();
            let client = Identite::generer();
            let record = PskPair::pour_pair(&client.id(), [29; 32], categorie).unwrap();
            let (a, b, infos) = etablir(&serveur, &client, suite, &record, &psk::sentinelle());
            assert!(
                infos.identifiant_perdu,
                "la perte de cle doit rester visible"
            );
            assert_eq!(
                infos.categorie_psk,
                CategoriePsk::Sentinelle,
                "une sentinelle ne devient pas un pair appaire"
            );
            assert_eq!(infos.psk_id, psk::identifiant(&psk::sentinelle()));
            assert_eq!(
                record.secret(),
                &[29; 32],
                "le signal ne remplace pas le record"
            );
            assert_eq!(record.categorie(), categorie);
            verifier_transport(a, b);
        }
    }
}

#[test]
fn i3326_une_psk_inconnue_ne_devient_pas_un_repli_sentinelle() {
    for suite in Suite::toutes() {
        let serveur = Identite::generer();
        let client = Identite::generer();
        let record = PskPair::pour_pair(&client.id(), [29; 32], CategoriePsk::LongueDuree).unwrap();
        let (poignee, mut enceinte) = demarrer(&serveur, &client, suite, &record, &[71; 32]);
        assert!(
            poignee.message_deux(&enceinte.ecrire_deux()).is_err(),
            "le repli doit VERIFIER la sentinelle, pas accepter toute erreur de PSK"
        );
    }
}

#[test]
fn i3326_le_secret_est_lie_a_la_cle_publique_et_la_sentinelle_ne_peut_pas_etre_promue() {
    let serveur = Identite::generer();
    let client = Identite::generer();
    let autre = Identite::generer();
    for categorie in [CategoriePsk::Appairage, CategoriePsk::LongueDuree] {
        assert!(
            PskPair::pour_pair(&client.id(), psk::sentinelle(), categorie).is_err(),
            "la sentinelle publique ne peut pas devenir une PSK privee"
        );
        let record = PskPair::pour_pair(&client.id(), [29; 32], categorie).unwrap();
        assert!(
            PoigneeServeur::accueillir_avec_psk(
                &serveur,
                &client_init(&autre, Suite::toutes()[0]),
                &record
            )
            .is_err(),
            "une cle pre-partagee ne doit pas etre offerte a une autre identite"
        );
        assert!(
            !format!("{record:?}").contains(&URL_SAFE_NO_PAD.encode(record.secret())),
            "Debug ne doit pas exposer le secret d'appairage"
        );
    }
}

#[test]
fn i3326_le_re_echange_lie_le_condensat_et_change_les_cles_sans_nouveaux_init() {
    for suite in Suite::toutes() {
        let serveur = Identite::generer();
        let client = Identite::generer();
        let (mut ancien_a, mut ancien_b, infos) = etablir(
            &serveur,
            &client,
            suite,
            &PskPair::sentinelle(),
            &psk::sentinelle(),
        );
        let psk = PskPair::pour_pair(&client.id(), [29; 32], CategoriePsk::LongueDuree).unwrap();
        let mut poignee = PoigneeServeur::renouveler(&serveur, &infos, &psk).unwrap();
        assert!(
            poignee.server_init_texte().is_empty(),
            "pas de nouveaux init au re-echange"
        );
        let mut enceinte = Enceinte::nouvelle(
            &client,
            serveur.public(),
            &infos.condensat_poignee,
            suite,
            psk.secret(),
        );
        let message = poignee.message_un().unwrap();
        let chiffre = ancien_a.chiffrer_json(&message).unwrap();
        let mut clair = vec![0; MAX_NOISE];
        let n = ancien_b.read_message(&chiffre, &mut clair).unwrap();
        assert_eq!(clair[0], 0);
        let charge = enceinte.lire_charge(std::str::from_utf8(&clair[1..n]).unwrap());
        assert_eq!(
            charge["psk_category"], "lt",
            "promotion explicite en longue duree"
        );
        let deux = enceinte.ecrire_deux();
        let mut chiffre = vec![0; MAX_NOISE];
        let n = ancien_b
            .write_message(&[&[0], deux.as_bytes()].concat(), &mut chiffre)
            .unwrap();
        let (nouveau_a, nouvelles_infos) = poignee
            .message_deux(&ancien_a.dechiffrer_json(&chiffre[..n]).unwrap())
            .unwrap();
        assert_eq!(
            nouvelles_infos.condensat_poignee.as_slice(),
            enceinte.etat.get_handshake_hash()
        );
        assert_ne!(nouvelles_infos.condensat_poignee, infos.condensat_poignee);
        assert_eq!(nouvelles_infos.categorie_psk, CategoriePsk::LongueDuree);
        assert!(!nouvelles_infos.identifiant_perdu);
        verifier_transport(nouveau_a, enceinte.etat.into_transport_mode().unwrap());
    }
}

#[test]
fn i3326_le_re_echange_refuse_le_repli_et_les_identites_substituees() {
    for suite in Suite::toutes() {
        let serveur = Identite::generer();
        let client = Identite::generer();
        let (_, _, infos) = etablir(
            &serveur,
            &client,
            suite,
            &PskPair::sentinelle(),
            &psk::sentinelle(),
        );
        let psk = PskPair::pour_pair(&client.id(), [29; 32], CategoriePsk::LongueDuree).unwrap();
        assert!(PoigneeServeur::renouveler(&Identite::generer(), &infos, &psk).is_err());
        let autre_psk = PskPair::pour_pair(
            &Identite::generer().id(),
            [29; 32],
            CategoriePsk::LongueDuree,
        )
        .unwrap();
        assert!(PoigneeServeur::renouveler(&serveur, &infos, &autre_psk).is_err());
        let mut poignee = PoigneeServeur::renouveler(&serveur, &infos, &psk).unwrap();
        let mut enceinte = Enceinte::nouvelle(
            &client,
            serveur.public(),
            &infos.condensat_poignee,
            suite,
            &psk::sentinelle(),
        );
        enceinte.lire_charge(&poignee.message_un().unwrap());
        assert!(
            poignee.message_deux(&enceinte.ecrire_deux()).is_err(),
            "un re-echange vers LT ne doit jamais retomber en sentinelle"
        );
    }
}

#[test]
fn i3326_le_second_message_exige_l_objet_vide_litteral() {
    for suite in Suite::toutes() {
        for charge in [
            b"".as_slice(),
            b"null",
            b"[]",
            b"{ }",
            b"{\"role\":\"player\"}",
        ] {
            let serveur = Identite::generer();
            let client = Identite::generer();
            let (poignee, mut enceinte) = demarrer(
                &serveur,
                &client,
                suite,
                &PskPair::sentinelle(),
                &psk::sentinelle(),
            );
            let mut chiffre = vec![0; MAX_NOISE];
            let n = enceinte.etat.write_message(charge, &mut chiffre).unwrap();
            let deux = serde_json::json!({"type":"noise/handshake","payload":{
                "data":URL_SAFE_NO_PAD.encode(&chiffre[..n])
            }})
            .to_string();
            assert!(
                poignee.message_deux(&deux).is_err(),
                "la charge du message 2 doit etre exactement les deux octets {{}}"
            );
        }
    }
}

#[test]
fn i3326_le_repli_ne_permet_pas_d_usurper_la_cle_statique() {
    for suite in Suite::toutes() {
        let serveur = Identite::generer();
        let client = Identite::generer();
        let imposteur = Identite::generer();
        let record = PskPair::pour_pair(&client.id(), [29; 32], CategoriePsk::LongueDuree).unwrap();
        let init = client_init(&client, suite);
        let mut poignee = PoigneeServeur::accueillir_avec_psk(&serveur, &init, &record).unwrap();
        let prologue = [init.as_bytes(), poignee.server_init_texte().as_bytes()].concat();
        let mut enceinte = Enceinte::nouvelle(
            &imposteur,
            serveur.public(),
            &prologue,
            suite,
            &psk::sentinelle(),
        );
        assert!(
            enceinte
                .essayer_lire_un(&poignee.message_un().unwrap())
                .is_err(),
            "posseder la sentinelle ne donne pas la cle privee de l'enceinte"
        );
    }
}

#[test]
fn i3326_une_identite_x25519_de_faible_ordre_est_refusee_avant_noise() {
    let serveur = Identite::generer();
    let mut points = vec![[0; 32]];
    let mut un = [0; 32];
    un[0] = 1;
    points.push(un);
    // p-1, p et p+1 : formes canoniques et non canoniques de petits points.
    for premier in [0xec, 0xed, 0xee] {
        let mut p = [0xff; 32];
        p[0] = premier;
        p[31] = 0x7f;
        points.push(p);
    }
    for point in points {
        for bit_ignore in [0, 0x80] {
            let mut point = point;
            point[31] |= bit_ignore;
            let id = URL_SAFE_NO_PAD.encode(point);
            assert!(
                tune_core::sendspin::identite::cle_publique_du_pair(&id).is_err(),
                "une cle X25519 donnant un DH nul ne prouve pas une identite"
            );
            assert!(
                PskPair::pour_pair(&id, [29; 32], CategoriePsk::LongueDuree).is_err(),
                "aucun record d'appairage pour une identite de faible ordre"
            );
            let init = serde_json::json!({"type":"client/init","payload":{
                "client_id":id, "version":1, "suite":Suite::toutes()[0].nom()
            }})
            .to_string();
            assert!(
                PoigneeServeur::accueillir(&serveur, &init, &psk::sentinelle()).is_err(),
                "refus avant d'emettre un message Noise vers une cle sans identite"
            );
        }
    }
}
