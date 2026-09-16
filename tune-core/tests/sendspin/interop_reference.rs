//! Interop explicite contre les objets Noise et modeles de l'implementation tierce.
//! Ce temoin ne pretend pas exercer le SDK complet ni un lecteur materiel.
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use tune_core::sendspin::psk::{CategoriePsk, PskPair};

struct Reference {
    enfant: Child,
    entree: ChildStdin,
    sortie: BufReader<ChildStdout>,
}
impl Drop for Reference {
    fn drop(&mut self) {
        let _ = self.enfant.kill();
        let _ = self.enfant.wait();
    }
}
impl Reference {
    fn nouvelle() -> Self {
        let python = std::env::var_os("SENDSPIN_REFERENCE_PYTHON")
            .expect("definir SENDSPIN_REFERENCE_PYTHON vers le venv aiosendspin epingle");
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/sendspin/reference_noise.py");
        let mut enfant = Command::new(python)
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("demarrage reference tierce");
        let entree = enfant.stdin.take().unwrap();
        let sortie = BufReader::new(enfant.stdout.take().unwrap());
        Self {
            enfant,
            entree,
            sortie,
        }
    }
    fn demander(&mut self, demande: serde_json::Value) -> serde_json::Value {
        writeln!(self.entree, "{demande}").unwrap();
        self.entree.flush().unwrap();
        let mut ligne = String::new();
        assert_ne!(
            self.sortie.read_line(&mut ligne).unwrap(),
            0,
            "la reference tierce a termine avant de repondre"
        );
        serde_json::from_str(&ligne).unwrap()
    }
}

#[test]
#[ignore = "interop tierce : exige un venv aiosendspin epingle sur Shrek"]
fn i3326_reference_aiosendspin_categories_perte_de_cle_et_promotion_lt() {
    for suite in Suite::toutes() {
        for categorie in [
            CategoriePsk::Sentinelle,
            CategoriePsk::Appairage,
            CategoriePsk::LongueDuree,
        ] {
            // Le cas 'perdu' est la perte du record client, jamais une PSK aleatoire.
            for perdu in [false, true] {
                if perdu && categorie == CategoriePsk::Sentinelle {
                    continue;
                }
                let mut reference = Reference::nouvelle();
                let client_id = reference.demander(serde_json::json!({"op":"identity"}))["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let serveur = Identite::generer();
                let psk = match categorie {
                    CategoriePsk::Sentinelle => PskPair::sentinelle(),
                    _ => PskPair::pour_pair(&client_id, [29; 32], categorie).unwrap(),
                };
                let init = serde_json::json!({"type":"client/init","payload":{
                    "client_id":client_id, "version":1, "suite":suite.nom()
                }})
                .to_string();
                let mut poignee =
                    PoigneeServeur::accueillir_avec_psk(&serveur, &init, &psk).unwrap();
                let message = poignee.message_un().unwrap();
                let reponse = reference.demander(serde_json::json!({
                    "op":"start", "client_init":init, "server_init":poignee.server_init_texte(),
                    "server_id":serveur.id(), "suite":suite.nom(), "message":message,
                    "category":categorie, "advertised_psk":URL_SAFE_NO_PAD.encode(psk.secret()),
                    "used_psk":URL_SAFE_NO_PAD.encode(if perdu {psk::sentinelle()} else {*psk.secret()})
                }));
                let (mut transport, infos) = poignee
                    .message_deux(reponse["message"].as_str().unwrap())
                    .expect("la reponse de la reference tierce doit etablir Noise");
                assert_eq!(
                    URL_SAFE_NO_PAD.encode(infos.condensat_poignee),
                    reponse["hash"]
                );
                assert_eq!(infos.identifiant_perdu, perdu);
                assert_eq!(
                    infos.categorie_psk,
                    if perdu {
                        CategoriePsk::Sentinelle
                    } else {
                        categorie
                    }
                );
                let chiffre = transport.chiffrer_json(r#"{"preuve":"Tune"}"#).unwrap();
                let reponse = reference.demander(serde_json::json!({
                    "op":"transport", "message":URL_SAFE_NO_PAD.encode(chiffre)
                }));
                let chiffre = URL_SAFE_NO_PAD
                    .decode(reponse["message"].as_str().unwrap())
                    .unwrap();
                assert_eq!(
                    transport.dechiffrer_json(&chiffre).unwrap(),
                    r#"{"preuve":"aiosendspin"}"#
                );

                let nouvelle =
                    PskPair::pour_pair(&client_id, [83; 32], CategoriePsk::LongueDuree).unwrap();
                let mut poignee = PoigneeServeur::renouveler(&serveur, &infos, &nouvelle).unwrap();
                let un = transport
                    .chiffrer_json(&poignee.message_un().unwrap())
                    .unwrap();
                let reponse = reference.demander(serde_json::json!({
                    "op":"renew", "server_id":serveur.id(), "suite":suite.nom(),
                    "message":URL_SAFE_NO_PAD.encode(un), "category":"lt",
                    "advertised_psk":URL_SAFE_NO_PAD.encode(nouvelle.secret()),
                    "used_psk":URL_SAFE_NO_PAD.encode(nouvelle.secret())
                }));
                let deux = URL_SAFE_NO_PAD
                    .decode(reponse["message"].as_str().unwrap())
                    .unwrap();
                let (mut nouveau, apres) = poignee
                    .message_deux(&transport.dechiffrer_json(&deux).unwrap())
                    .unwrap();
                assert_eq!(
                    URL_SAFE_NO_PAD.encode(apres.condensat_poignee),
                    reponse["hash"]
                );
                assert_eq!(apres.categorie_psk, CategoriePsk::LongueDuree);
                assert!(!apres.identifiant_perdu);
                let chiffre = nouveau.chiffrer_json(r#"{"preuve":"Tune"}"#).unwrap();
                let reponse = reference.demander(serde_json::json!({
                    "op":"transport", "message":URL_SAFE_NO_PAD.encode(chiffre)
                }));
                assert_eq!(
                    nouveau
                        .dechiffrer_json(
                            &URL_SAFE_NO_PAD
                                .decode(reponse["message"].as_str().unwrap())
                                .unwrap()
                        )
                        .unwrap(),
                    r#"{"preuve":"aiosendspin"}"#
                );
                println!(
                    "aiosendspin {suite} {categorie:?} perte={perdu} : transport et re-echange LT reussis"
                );
            }
        }
    }
}
