//! Interop des API publiques natives contre CPace 0.1.0 et les helpers aiosendspin.
//! Pas une preuve du SDK complet : sa revision epinglee omet encore round du SID.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use tune_core::sendspin::Suite;
use tune_core::sendspin::pake::{
    CodeAppairage, ContextePake, ErreurAppairage, FormatCode, LiaisonDynamique, PakeServeur,
};

struct Reference {
    enfant: Child,
    entree: ChildStdin,
    sortie: Receiver<String>,
    lecture: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Reference {
    fn drop(&mut self) {
        if self.enfant.try_wait().ok().flatten().is_none() {
            let _ = self.enfant.kill(); // uniquement notre sous-processus de fixture
        }
        let _ = self.enfant.wait();
        if let Some(t) = self.lecture.take() {
            let _ = t.join();
        }
    }
}
impl Reference {
    fn nouvelle() -> Self {
        let python =
            std::env::var_os("SENDSPIN_REFERENCE_PYTHON").expect("venv CPace/aiosendspin epingle");
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/sendspin/reference_pake.py");
        let mut enfant = Command::new(python)
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let entree = enfant.stdin.take().unwrap();
        let stdout = enfant.stdout.take().unwrap();
        let (tx, sortie) = mpsc::channel();
        let lecture = Some(std::thread::spawn(move || {
            for ligne in BufReader::new(stdout).lines() {
                match ligne {
                    Ok(ligne) => {
                        if tx.send(ligne).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }));
        Self {
            enfant,
            entree,
            sortie,
            lecture,
        }
    }
    fn demander(&mut self, v: Value) -> Value {
        writeln!(self.entree, "{v}").unwrap();
        self.entree.flush().unwrap();
        let ligne = self
            .sortie
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("reponse bornee du client CPace");
        serde_json::from_str(&ligne).unwrap()
    }
    fn finir(&mut self) {
        assert_eq!(self.demander(json!({"op":"end"}))["ended"], true);
        assert!(
            self.enfant.wait().unwrap().success(),
            "le client de reference doit terminer sans erreur"
        );
    }
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}
fn bytes(v: &Value, key: &str) -> Vec<u8> {
    let s = v[key].as_str().unwrap();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
#[ignore = "interop CPace : exige SENDSPIN_REFERENCE_PYTHON avec les revisions epinglees"]
fn i3326_pake_reference_vivante_sur_les_api_publiques() {
    let mut total = 0;
    for (nom, format, tour) in [
        ("static", FormatCode::Statique, 1),
        ("digits", FormatCode::Dynamique, 2),
        ("qr", FormatCode::Qr, 3),
    ] {
        for suite in Suite::toutes() {
            for scenario in [
                "ok",
                "wrong_code",
                "wrong_tag",
                "wrong_wrap",
                "wrong_commit",
                "wrong_binding",
                "wrong_nonce_wrap",
            ] {
                if format == FormatCode::Statique
                    && ["wrong_commit", "wrong_binding", "wrong_nonce_wrap"].contains(&scenario)
                {
                    continue;
                }
                let mut reference = Reference::nouvelle();
                let h = [37u8; 32];
                let debut=reference.demander(json!({"op":"start","format":nom,"suite":if suite==Suite::AesGcm {"aes"} else {"chacha"},"scenario":scenario,"h":hex(&h),"index":3,"round":tour}));
                let client_id = debut["client_id"].as_str().unwrap();
                let mut commit: [u8; 32] = bytes(&debut, "commit").try_into().unwrap();
                if scenario == "wrong_commit" {
                    commit[0] ^= 1;
                }
                let liaison =
                    (format != FormatCode::Statique).then(|| LiaisonDynamique::nouvelle(commit));
                let nonce_a = liaison.as_ref().map(|l| *l.nonce_a()).unwrap_or([0; 32]);
                let code = reference.demander(json!({"op":"code","nonce_a":hex(&nonce_a)}));
                let mut prs = bytes(&code, "code");
                if scenario == "wrong_code" {
                    prs[0] = if format == FormatCode::Qr {
                        prs[0] ^ 1
                    } else if prs[0] == b'0' {
                        b'1'
                    } else {
                        b'0'
                    };
                }
                let s = PakeServeur::demarrer(
                    CodeAppairage::nouveau(format, &prs).unwrap(),
                    ContextePake::nouveau(h, 3, tour).unwrap(),
                    suite,
                    liaison,
                )
                .unwrap();
                assert_eq!(
                    reference.demander(json!({"op":"share","share":hex(s.partage())}))["ready"],
                    true
                );
                let c = s.recevoir_partage(&bytes(&code, "share")).unwrap();
                let fin = reference.demander(json!({"op":"confirm","tag":hex(&c.tag_serveur())}));
                let nonce = (format != FormatCode::Statique && scenario != "wrong_code")
                    .then(|| bytes(&fin, "wrapped_nonce"));
                let confirme = c.confirmer(&bytes(&fin, "tag"), nonce.as_deref());
                match scenario {
                    "wrong_code" | "wrong_tag" => assert!(
                        matches!(confirme, Err(ErreurAppairage::CodeIncorrect)),
                        "la reference ne doit pas faire accepter un code ou tag incorrect"
                    ),
                    "wrong_commit" | "wrong_binding" | "wrong_nonce_wrap" => assert!(
                        matches!(confirme, Err(ErreurAppairage::Protocole(_))),
                        "une confirmation seule ne suffit pas sans liaison dynamique"
                    ),
                    _ => {
                        let psk = confirme
                            .expect("confirmation du client tiers")
                            .recevoir_psk(client_id, &bytes(&fin, "wrapped_psk"));
                        if scenario == "wrong_wrap" {
                            assert!(psk.is_err(), "une PSK chiffree alteree doit etre refusee");
                        } else {
                            assert_eq!(
                                psk.unwrap().identifiant(),
                                fin["psk_id"],
                                "la PSK de la reference doit etre retrouvee"
                            );
                        }
                    }
                }
                reference.finir();
                total += 1;
                println!("CPace public {nom} {suite} {scenario}: comportement attendu");
            }
        }
    }
    assert_eq!(total, 36);
}
