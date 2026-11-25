//! La page locale et ses appels, sur 127.0.0.1 seulement.
//!
//! Gardes, parce qu'un serveur local est joignable par N'IMPORTE QUELLE page
//! ouverte dans le navigateur :
//!
//! - un jeton aléatoire dans le chemin (`/s/<jeton>/…`), exigé partout ;
//! - l'en-tête `Host` doit être exactement `127.0.0.1:<port>` (rebinding DNS) ;
//! - un `Origin`, s'il est présent, doit être la page elle-même ;
//! - les appels sont des POST `application/json` ;
//! - toute adresse de Tune est revérifiée « réseau local » côté binaire
//!   ([`crate::adresse`]), si bien que l'outil ne peut relayer vers Internet.
//!
//! Les jetons de session du Tune restent en mémoire, jamais sur disque.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::adresse::{base, lire_saisie, relire_base, resoudre_localement, PORT_TUNE};
use crate::decouverte;
use crate::tune::{self, ClientTune, Demande, Echec};

pub const PAGE: &str = include_str!("page.html");

pub struct Etat {
    pub jeton: String,
    pub port: u16,
    pub jetons_tune: Mutex<HashMap<String, String>>,
    pub derniere_activite: Mutex<Instant>,
    pub quitter: AtomicBool,
    /// Remplaçable sous test : la découverte réseau n'y a pas sa place.
    pub chercher: fn(Duration) -> Vec<decouverte::Trouve>,
}

impl Etat {
    pub fn new(jeton: String, port: u16) -> Self {
        Self {
            jeton,
            port,
            jetons_tune: Mutex::new(HashMap::new()),
            derniere_activite: Mutex::new(Instant::now()),
            quitter: AtomicBool::new(false),
            chercher: decouverte::chercher,
        }
    }

    pub fn chemin_page(&self) -> String {
        format!("/s/{}/", self.jeton)
    }

    pub fn inactif_depuis(&self) -> Duration {
        self.derniere_activite.lock().unwrap().elapsed()
    }
}

pub fn nouveau_jeton() -> String {
    let mut octets = [0u8; 24];
    getrandom::getrandom(&mut octets).expect("aléa du système indisponible");
    octets.iter().map(|o| format!("{o:02x}")).collect()
}

/// Ce que reçoit la couche HTTP. Séparé de `tiny_http` pour être testé.
pub struct Requete<'a> {
    pub methode: &'a str,
    pub chemin: &'a str,
    pub hote: Option<&'a str>,
    pub origine: Option<&'a str>,
    pub type_contenu: Option<&'a str>,
    pub corps: &'a str,
}

pub struct Reponse {
    pub code: u16,
    pub type_contenu: &'static str,
    pub corps: String,
}

fn rep_json(code: u16, v: Value) -> Reponse {
    Reponse {
        code,
        type_contenu: "application/json; charset=utf-8",
        corps: v.to_string(),
    }
}

fn refus(code: u16) -> Reponse {
    Reponse {
        code,
        type_contenu: "text/plain; charset=utf-8",
        corps: "Refusé / Forbidden".into(),
    }
}

fn erreur(e: Echec) -> Reponse {
    rep_json(200, json!({"ok": false, "erreur": e}))
}

fn erreur_code(code: &'static str) -> Reponse {
    erreur(Echec { code, detail: None })
}

pub fn traiter(etat: &Etat, r: &Requete) -> Reponse {
    let attendu = format!("127.0.0.1:{}", etat.port);
    if r.hote != Some(attendu.as_str()) {
        return refus(421);
    }
    if let Some(o) = r.origine {
        if o != format!("http://{attendu}") {
            return refus(403);
        }
    }
    let prefixe = etat.chemin_page();
    let Some(reste) = r
        .chemin
        .split('?')
        .next()
        .and_then(|c| c.strip_prefix(prefixe.as_str()))
    else {
        return refus(404);
    };
    *etat.derniere_activite.lock().unwrap() = Instant::now();
    if reste.is_empty() {
        return if r.methode == "GET" {
            Reponse {
                code: 200,
                type_contenu: "text/html; charset=utf-8",
                corps: PAGE.to_string(),
            }
        } else {
            refus(405)
        };
    }
    let Some(action) = reste.strip_prefix("api/") else {
        return refus(404);
    };
    if r.methode != "POST"
        || !r
            .type_contenu
            .is_some_and(|t| t.starts_with("application/json"))
    {
        return refus(405);
    }
    let corps: Value = serde_json::from_str(r.corps).unwrap_or(Value::Null);
    match action {
        "presence" => rep_json(200, json!({"ok": true})),
        "quitter" => {
            etat.quitter.store(true, Ordering::SeqCst);
            rep_json(200, json!({"ok": true}))
        }
        "decouvrir" => {
            let trouves = (etat.chercher)(Duration::from_secs(4));
            rep_json(200, json!({"ok": true, "trouves": trouves}))
        }
        "diagnostiquer" => diagnostiquer(&corps),
        "debloquer" => debloquer(etat, &corps),
        "suivre" => suivre(etat, &corps),
        _ => refus(404),
    }
}

fn diagnostiquer(corps: &Value) -> Reponse {
    let saisie = corps["adresse"].as_str().unwrap_or("");
    // Une base déjà découverte (`http://ip:port`) : un seul essai.
    if saisie.starts_with("http://") {
        if let Ok(b) = relire_base(saisie) {
            return match tune::diagnostiquer(&ClientTune::new(&b)) {
                Ok(d) => rep_json(200, json!({"ok": true, "diagnostic": d})),
                Err(e) => erreur(e),
            };
        }
    }
    let s = match lire_saisie(saisie) {
        Ok(s) => s,
        Err(code) => return erreur_code(code),
    };
    // Sans port : 8888 (le serveur), puis 80 (le relais de Tune OS).
    let ports: Vec<u16> = match s.port {
        Some(p) => vec![p],
        None => vec![PORT_TUNE, 80],
    };
    let mut derniere = Echec {
        code: "injoignable",
        detail: None,
    };
    for port in ports {
        let ip = match resoudre_localement(&s.hote, port) {
            Ok(ip) => ip,
            Err(code) => return erreur_code(code),
        };
        match tune::diagnostiquer(&ClientTune::new(&base(ip, port))) {
            Ok(d) => return rep_json(200, json!({"ok": true, "diagnostic": d})),
            Err(e) => derniere = e,
        }
    }
    erreur(derniere)
}

#[derive(Deserialize)]
struct CorpsDebloquer {
    base: String,
    #[serde(default)]
    identifiant: String,
    mot_de_passe: String,
    #[serde(default)]
    nouveau: bool,
    #[serde(default)]
    force: bool,
}

fn debloquer(etat: &Etat, corps: &Value) -> Reponse {
    let Ok(c) = serde_json::from_value::<CorpsDebloquer>(corps.clone()) else {
        return erreur_code("demande_invalide");
    };
    let b = match relire_base(&c.base) {
        Ok(b) => b,
        Err(code) => return erreur_code(code),
    };
    let demande = Demande {
        identifiant: c.identifiant,
        mot_de_passe: c.mot_de_passe,
        nouveau: c.nouveau,
        force: c.force,
    };
    match tune::debloquer(&ClientTune::new(&b), &demande) {
        Ok(res) => {
            etat.jetons_tune
                .lock()
                .unwrap()
                .insert(b, res.jeton.clone());
            rep_json(200, json!({"ok": true, "resultat": res}))
        }
        Err(e) => erreur(e),
    }
}

fn suivre(etat: &Etat, corps: &Value) -> Reponse {
    let b = match relire_base(corps["base"].as_str().unwrap_or("")) {
        Ok(b) => b,
        Err(code) => return erreur_code(code),
    };
    let jeton = etat.jetons_tune.lock().unwrap().get(&b).cloned();
    let s = tune::suivre(&ClientTune::new(&b), jeton.as_deref());
    rep_json(200, json!({"ok": true, "suivi": s}))
}
