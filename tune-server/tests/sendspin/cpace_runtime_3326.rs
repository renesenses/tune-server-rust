//! Vrai HTTP/WebSocket et CPace Python tiers ; SID normatif incluant round.
//! Ce banc exerce les helpers aiosendspin epingles, pas son client complet.
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
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
            .join("../tune-core/tests/sendspin/reference_pake.py");
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

async fn http(r: reqwest::RequestBuilder) -> Value {
    let reponse = r.send().await.unwrap();
    assert_eq!(
        reponse.status(),
        200,
        "la commande d'appairage doit etre acceptee"
    );
    reponse.json().await.unwrap()
}
async fn lire(l: &mut Lecteur, typ: &str) -> Value {
    let v = l.lire().await;
    assert_eq!(v["type"], typ, "sequence CPace sur le vrai WebSocket");
    v["payload"].clone()
}
async fn phase(s: &Serveur, id: &str, attendue: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let v = s.disponible(id).await;
            if v["phase"] == attendue {
                break v;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("phase operateur attendue")
}
async fn temps(l: &mut Lecteur) {
    l.envoyer("client/time", json!({"client_transmitted":42}))
        .await;
    assert_eq!(
        lire(l, "server/time").await["client_transmitted"],
        42,
        "le canal doit vivre sans partage CPace avant le geste ou apres annulation"
    );
}
fn base32(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut sortie = String::new();
    let mut bits = 0u32;
    let mut n = 0;
    for b in bytes {
        bits = (bits << 8) | u32::from(*b);
        n += 8;
        while n >= 5 {
            n -= 5;
            sortie.push(A[((bits >> n) & 31) as usize] as char);
        }
    }
    if n > 0 {
        sortie.push(A[((bits << (5 - n)) & 31) as usize] as char);
    }
    sortie.replace('2', "9")
}

#[tokio::test]
#[ignore = "interop HTTP/WebSocket CPace : exige SENDSPIN_REFERENCE_PYTHON epingle"]
async fn i3326_cpace_websocket_code_reprise_annulation_et_reconnexion() {
    let mut total = 0;
    for nom in ["static", "digits", "qr"] {
        for suite in Suite::toutes() {
            for reprise in [false, true] {
                if nom == "static" && reprise {
                    continue;
                }
                let s = Serveur::nouveau().await;
                let id = Identite::generer();
                let id_texte = id.id();
                let prive = *id.prive();
                let methodes = if nom == "static" {
                    json!({"static_pairing_code":{}})
                } else {
                    json!({"dynamic_pairing_code":{
                        "formats":["digits","qr_code"],"out_channels":["display"]}})
                };
                let mut l =
                    Lecteur::nouveau(&s, id, suite, &PskPair::sentinelle(), methodes.clone()).await;
                s.disponible(&id_texte).await;
                let c = reqwest::Client::new();
                let commande = if nom == "static" {
                    json!({"method":"static_pairing_code","code":"01234567"})
                } else {
                    json!({"method":"dynamic_pairing_code",
                    "format":if nom=="qr" {"qr_code"} else {"digits"}})
                };
                let fichier = s.temporaire.path().join("tune.db.sendspin/pairing.json");
                let magasin_initial = std::fs::read(&fichier).unwrap();

                // Une activation annulee garde le transport, pas le code saisi ni l'essai.
                http(c.post(s.url(&id_texte, "pair")).json(&commande)).await;
                assert_eq!(
                    lire(&mut l, "server/activate").await["activities"],
                    json!(["pairing"])
                );
                l.envoyer(
                    "client/pair-pending",
                    json!({"pairing_index":1,"message":"appuyer"}),
                )
                .await;
                let attente = phase(&s, &id_texte, "attente_geste").await;
                assert_eq!(attente["details"]["untrusted"], true);
                temps(&mut l).await;
                http(c.delete(s.url(&id_texte, "pair"))).await;
                assert_eq!(lire(&mut l, "pair/abort").await["reason"], "user_cancelled");
                assert_eq!(
                    lire(&mut l, "server/activate").await["activities"],
                    json!([])
                );
                phase(&s, &id_texte, "abandonne").await;
                assert_eq!(
                    std::fs::read(&fichier).unwrap(),
                    magasin_initial,
                    "une annulation avant code doit conserver le magasin initial"
                );
                l.envoyer(
                    "client/pair-auth",
                    json!({"pake_msg_2":"message deja en vol"}),
                )
                .await;
                temps(&mut l).await;

                http(c.post(s.url(&id_texte, "pair")).json(&commande)).await;
                lire(&mut l, "server/activate").await;
                let index = 2;
                let mut reference = Reference::nouvelle();
                let debut = reference.demander(json!({"op":"start","format":nom,
                    "suite":if suite==Suite::AesGcm {"aes"} else {"chacha"},
                    "scenario":if reprise {"wrong_code"} else {"ok"},
                    "h":hex(&l.condensat),"index":index,"round":1}));
                l.envoyer(
                    "client/pair-pending",
                    json!({"pairing_index":index,"message":"confirmer"}),
                )
                .await;
                phase(&s, &id_texte, "attente_geste").await;
                temps(&mut l).await;
                let mut init = json!({"pairing_index":index});
                if nom != "static" {
                    init["commit_B"] = json!(URL_SAFE_NO_PAD.encode(bytes(&debut, "commit")));
                }
                l.envoyer("client/pair-init", init).await;
                let nonce = if nom == "static" {
                    vec![0; 32]
                } else {
                    let v = lire(&mut l, "server/pair-init").await;
                    URL_SAFE_NO_PAD
                        .decode(v["nonce_A"].as_str().unwrap())
                        .unwrap()
                };
                let mut precedent = None;
                let mut lt = None;
                for tour in 1..=if reprise { 2 } else { 1 } {
                    if tour == 2 {
                        assert_eq!(
                            reference.demander(json!({"op":"retry","index":index,
                            "round":tour,"scenario":"ok"}))["ready"],
                            true
                        );
                    }
                    let code = reference.demander(json!({"op":"code","nonce_a":hex(&nonce)}));
                    let mut prs = bytes(&code, "code");
                    if reprise && tour == 1 {
                        prs[0] = if nom == "qr" {
                            prs[0] ^ 1
                        } else if prs[0] == b'0' {
                            b'1'
                        } else {
                            b'0'
                        };
                    }
                    if nom != "static" {
                        let etat = phase(&s, &id_texte, "code_attendu").await;
                        assert_eq!(etat["details"]["round"], tour);
                        let saisie = if nom == "qr" {
                            format!("SP:1{}", base32(&prs))
                        } else {
                            String::from_utf8(prs).unwrap()
                        };
                        http(
                            c.post(s.url(&id_texte, "pair/code"))
                                .json(&json!({"code":saisie})),
                        )
                        .await;
                    }
                    let a = lire(&mut l, "server/pair-auth").await;
                    let ya = URL_SAFE_NO_PAD
                        .decode(a["pake_msg_1"].as_str().unwrap())
                        .unwrap();
                    if let Some(ancien) = precedent.as_ref() {
                        assert_ne!(&ya, ancien, "une reprise exige un nouvel ephemere");
                    }
                    precedent = Some(ya.clone());
                    assert_eq!(
                        reference.demander(json!({"op":"share","share":hex(&ya)}))["ready"],
                        true
                    );
                    l.envoyer(
                        "client/pair-auth",
                        json!({
                        "pake_msg_2":URL_SAFE_NO_PAD.encode(bytes(&code,"share"))}),
                    )
                    .await;
                    let confirmation = lire(&mut l, "server/pair-confirm").await;
                    let tag = URL_SAFE_NO_PAD
                        .decode(confirmation["server_kc"].as_str().unwrap())
                        .unwrap();
                    let fin = reference.demander(json!({"op":"confirm","tag":hex(&tag)}));
                    if reprise && tour == 1 {
                        assert_eq!(
                            fin["verified"], false,
                            "le client tiers doit refuser le mauvais code"
                        );
                        assert_eq!(
                            std::fs::read(&fichier).unwrap(),
                            magasin_initial,
                            "un mauvais code ne doit pas modifier le magasin"
                        );
                        l.envoyer("client/pair-retry", json!({"future_extension":true}))
                            .await;
                        assert_eq!(
                            lire(&mut l, "server/pair-init").await,
                            json!({}),
                            "la reprise conserve les nonces sans les reemettre"
                        );
                        continue;
                    }
                    assert_eq!(
                        fin["verified"], true,
                        "le client tiers doit verifier le serveur"
                    );
                    let mut confirme =
                        json!({"client_kc":URL_SAFE_NO_PAD.encode(bytes(&fin,"tag"))});
                    if nom != "static" {
                        confirme["wrapped_nonce_B"] =
                            json!(URL_SAFE_NO_PAD.encode(bytes(&fin, "wrapped_nonce")));
                    }
                    l.envoyer("client/pair-confirm", confirme).await;
                    l.envoyer(
                        "client/pair-finalize",
                        json!({
                        "wrapped_psk":URL_SAFE_NO_PAD.encode(bytes(&fin,"wrapped_psk"))}),
                    )
                    .await;
                    assert_eq!(lire(&mut l, "server/pair-finalize").await, json!({}));
                    let contenu = std::fs::read_to_string(&fichier).unwrap();
                    assert!(
                        contenu.contains(&id_texte),
                        "le pair doit etre durable avant ACK CPace"
                    );
                    let cle = PskPair::pour_pair(
                        &id_texte,
                        bytes(&fin, "psk").try_into().unwrap(),
                        CategoriePsk::LongueDuree,
                    )
                    .unwrap();
                    assert_eq!(cle.identifiant(), fin["psk_id"].as_str().unwrap());
                    l.renouveler(&cle).await;
                    assert_eq!(phase(&s, &id_texte, "appaire").await["authenticated"], true);
                    lt = Some(cle);
                }
                reference.finir();
                let lt = lt.unwrap();
                l.ws.close(None).await.unwrap();
                drop(l);
                tokio::time::timeout(Duration::from_secs(3), async {
                    while reqwest::get(s.url(&id_texte, "pair"))
                        .await
                        .unwrap()
                        .status()
                        != 404
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                let mut l =
                    Lecteur::nouveau(&s, Identite::depuis_prive(prive), suite, &lt, methodes).await;
                assert_eq!(
                    s.disponible(&id_texte).await["authenticated"],
                    true,
                    "la PSK du client tiers doit reconnecter le pair en LT"
                );
                assert_eq!(
                    http(c.delete(s.url(&id_texte, "credentials"))).await["revoked"],
                    true
                );
                assert!(
                    matches!(trame(&mut l.ws).await, Message::Close(_)),
                    "la revocation doit fermer la session CPace authentifiee"
                );
                total += 1;
                println!("CPace HTTP/WebSocket {nom} {suite} reprise={reprise}: attendu");
            }
        }
    }
    assert_eq!(total, 10);
}
