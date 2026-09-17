//! Fil independant : snow cote pair, en-tetes definis ici selon messaging.md.
use super::*;
const LIMITE_DU_BANC: usize = 1_048_576;

fn paire(suite: Suite) -> (TransportNoise, snow::TransportState) {
    let serveur = Identite::generer();
    let client = Identite::generer();
    let cle = psk::sentinelle();
    let init = client_init(&client, suite);
    let mut p = PoigneeServeur::accueillir(&serveur, &init, &cle).unwrap();
    let prologue = [init.as_bytes(), p.server_init_texte().as_bytes()].concat();
    let mut c = Enceinte::nouvelle(&client, serveur.public(), &prologue, suite, &cle);
    c.lire_un(&p.message_un().unwrap());
    let (t, _) = p.message_deux(&c.ecrire_deux()).unwrap();
    (t, c.etat.into_transport_mode().unwrap())
}
fn chiffre(c: &mut snow::TransportState, clair: &[u8]) -> Vec<u8> {
    let mut b = vec![0; 65535];
    let n = c.write_message(clair, &mut b).unwrap();
    b.truncate(n);
    b
}

#[test]
fn i3326_fragments_emis_portent_le_type_et_les_drapeaux_du_fil() {
    for suite in Suite::toutes() {
        for taille in [0, 65518, 65519, 131033, LIMITE_DU_BANC] {
            let (mut t, mut c) = paire(suite);
            let corps: Vec<u8> = (0..taille).map(|n| (n % 251) as u8).collect();
            let messages = t.chiffrer_message(4, &corps).unwrap();
            assert_eq!(
                messages.len() == 1,
                taille <= 65518,
                "le seuil Noise ne doit pas tronquer ni fragmenter trop tot"
            );
            let nombre = messages.len();
            let mut recu = Vec::new();
            for (i, b) in messages.iter().enumerate() {
                assert!(b.len() <= 65535);
                let mut clair = vec![0; 65535];
                let n = c.read_message(b, &mut clair).unwrap();
                if nombre == 1 {
                    assert_eq!(clair[0], 4);
                    recu.extend_from_slice(&clair[1..n]);
                } else {
                    assert_eq!(clair[0], 1, "le type de fragmentation courant est 1");
                    let attendu = if i == 0 {
                        2
                    } else if i + 1 == nombre {
                        1
                    } else {
                        0
                    };
                    assert_eq!(
                        clair[1], attendu,
                        "les bits first/last doivent cadrer le message"
                    );
                    let debut = if i == 0 {
                        assert_eq!(clair[2], 4);
                        3
                    } else {
                        2
                    };
                    recu.extend_from_slice(&clair[debut..n]);
                }
            }
            assert_eq!(
                recu, corps,
                "le decoupage doit conserver tous les octets audio"
            );
        }
    }
}

#[test]
fn i3326_fragments_recus_ne_livrent_que_le_message_complet() {
    for suite in Suite::toutes() {
        let (mut t, mut c) = paire(suite);
        for clair in [vec![1, 2, 0, b'a'], vec![1, 0, 0xc3]] {
            assert!(
                t.recevoir_json(&chiffre(&mut c, &clair)).unwrap().is_none(),
                "un fragment partiel ne doit jamais etre livre"
            );
        }
        assert_eq!(
            t.recevoir_json(&chiffre(&mut c, &[1, 1, 0xa9]))
                .unwrap()
                .as_deref(),
            Some("aé"),
            "UTF-8 doit etre valide apres reassemblage, pas par fragment"
        );
        assert_eq!(
            t.recevoir_message(&chiffre(&mut c, &[4, 7, 8])).unwrap(),
            Some((4, vec![7, 8])),
            "le message suivant ne doit pas heriter du fragment precedent"
        );
        assert_eq!(
            t.recevoir_message(&chiffre(&mut c, &[1, 3, 4])).unwrap(),
            Some((4, vec![])),
            "first et last peuvent etre reunis, avec un corps vide"
        );
    }
}

#[test]
fn i3326_fragments_invalides_empoisonnent_le_recepteur() {
    let cas = [
        vec![vec![1]],
        vec![vec![1, 2]],
        vec![vec![1, 0, 7]],
        vec![vec![1, 1]],
        vec![vec![1, 6, 0]],
        vec![vec![1, 3, 1]],
        vec![vec![1, 2, 0], vec![1, 2, 0]],
        vec![vec![1, 2, 0], vec![0, b'x']],
    ];
    for suite in Suite::toutes() {
        for sequence in &cas {
            let (mut t, mut c) = paire(suite);
            for (i, clair) in sequence.iter().enumerate() {
                let r = t.recevoir_message(&chiffre(&mut c, clair));
                if i + 1 == sequence.len() {
                    assert!(
                        r.is_err(),
                        "une sequence de fragments invalide doit etre refusee : {sequence:?}"
                    );
                } else {
                    assert!(r.unwrap().is_none());
                }
            }
            assert!(
                t.recevoir_message(&chiffre(&mut c, &[0, b'x'])).is_err(),
                "une erreur ne doit pas permettre la reprise sur le meme transport"
            );
        }
    }
}

#[test]
fn i3326_fragments_bornent_le_corps_et_l_emetteur_avant_chiffrement() {
    for suite in Suite::toutes() {
        let (mut t, mut c) = paire(suite);
        assert!(
            t.chiffrer_message(4, &vec![0; LIMITE_DU_BANC + 1]).is_err(),
            "l'emetteur doit refuser plus de 1 Mio"
        );
        assert!(t.chiffrer_message(1, b"imbrique").is_err());
        // Les refus locaux ne doivent pas consommer de nonce.
        let b = t.chiffrer_message(0, b"ok").unwrap();
        let mut clair = vec![0; 65535];
        assert_eq!(c.read_message(&b[0], &mut clair).unwrap(), 3);
        let mut restant = LIMITE_DU_BANC;
        let mut premier = true;
        while restant > 0 {
            let n = restant.min(60000);
            let mut b = if premier { vec![1, 2, 4] } else { vec![1, 0] };
            b.resize(b.len() + n, 8);
            assert!(t.recevoir_message(&chiffre(&mut c, &b)).unwrap().is_none());
            restant -= n;
            premier = false;
        }
        assert!(
            t.recevoir_message(&chiffre(&mut c, &[1, 1, 9])).is_err(),
            "le reassemblage doit refuser le premier octet au-dela de 1 Mio"
        );
    }
}

#[test]
fn i3326_fragments_authentifient_avant_d_assembler() {
    for suite in Suite::toutes() {
        let (mut t, mut c) = paire(suite);
        assert!(
            t.recevoir_message(&chiffre(&mut c, &[1, 2, 0, b'a']))
                .unwrap()
                .is_none()
        );
        let mut b = chiffre(&mut c, &[1, 1, b'b']);
        b[0] ^= 1;
        assert!(
            t.recevoir_message(&b).is_err(),
            "un fragment corrompu ne doit jamais rejoindre le corps"
        );
    }
}

#[test]
fn i3326_fragments_reassembles_acceptent_exactement_un_mio() {
    for suite in Suite::toutes() {
        let (mut t, mut c) = paire(suite);
        let corps = vec![37; LIMITE_DU_BANC];
        let morceaux: Vec<_> = corps.chunks(60000).collect();
        for (i, donnees) in morceaux.iter().enumerate() {
            let dernier = i + 1 == morceaux.len();
            let mut b = vec![
                1,
                (if i == 0 { 2 } else { 0 }) | (if dernier { 1 } else { 0 }),
            ];
            if i == 0 {
                b.push(4);
            }
            b.extend_from_slice(donnees);
            let recu = t.recevoir_message(&chiffre(&mut c, &b)).unwrap();
            if dernier {
                assert_eq!(
                    recu,
                    Some((4, corps.clone())),
                    "un corps d'exactement 1 Mio doit arriver entier"
                );
            } else {
                assert!(
                    recu.is_none(),
                    "le plafond ne doit pas provoquer une livraison prematuree"
                );
            }
        }
    }
}
