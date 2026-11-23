//! Logique d'état contre un FAUX serveur Tune, qui imite les réponses de la
//! rc12 (lues dans `v1.0.0-rc12:tune-server/src/auth.rs` et
//! `routes/system/update.rs`), de la rc13, et d'une authentification déjà
//! active.
//!
//! Le faux serveur rejoue aussi le REFUS : sous Tune OS, `update/install` sans
//! authentification rend 403 `port80_relay`. La contre-épreuve
//! `le_faux_tune_rc12_refuse_bien_la_mise_a_jour_sans_authentification`
//! montre qu'il bloque comme le vrai : sans elle, un faux serveur trop
//! complaisant ferait passer n'importe quel outil.

use std::sync::{Arc, Mutex};
use std::thread;

use debloquer_tune::tune::{self, ClientTune, Demande, Etat};
use debloquer_tune::web;
use serde_json::{json, Value};

#[derive(Default)]
struct Modele {
    version: String,
    auth_enabled: bool,
    /// Mot de passe de `default` (`None` = aucun hash, revendicable).
    mdp_default: Option<String>,
    /// Rôle rendu par la connexion (le profil 1 est administrateur).
    role: String,
    lecture: bool,
    analyse: bool,
    phase: Option<String>,
    jetons: Vec<String>,
    /// Chaque appel reçu : (méthode, chemin, Authorization).
    journal: Vec<(String, String, Option<String>)>,
}

struct FauxTune {
    base: String,
    m: Arc<Mutex<Modele>>,
    serveur: Arc<tiny_http::Server>,
}

impl Drop for FauxTune {
    fn drop(&mut self) {
        self.serveur.unblock();
    }
}

impl FauxTune {
    fn demarrer(version: &str, auth_enabled: bool, mdp_default: Option<&str>) -> Self {
        let serveur = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = serveur.server_addr().to_ip().unwrap().port();
        let m = Arc::new(Mutex::new(Modele {
            version: version.into(),
            auth_enabled,
            mdp_default: mdp_default.map(str::to_string),
            role: "admin".into(),
            ..Default::default()
        }));
        let (s, mm) = (Arc::clone(&serveur), Arc::clone(&m));
        thread::spawn(move || {
            for mut req in s.incoming_requests() {
                let mut corps = String::new();
                let _ = req.as_reader().read_to_string(&mut corps);
                let auth = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .map(|h| h.value.as_str().to_string());
                let (code, rep) = repondre(&mm, req.method().as_str(), req.url(), auth, &corps);
                let r = tiny_http::Response::from_string(rep.to_string())
                    .with_status_code(code)
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                    );
                let _ = req.respond(r);
            }
        });
        FauxTune {
            base: format!("http://127.0.0.1:{port}"),
            m,
            serveur,
        }
    }

    fn client(&self) -> ClientTune {
        ClientTune::new(&self.base)
    }

    fn ecritures(&self) -> Vec<String> {
        self.m
            .lock()
            .unwrap()
            .journal
            .iter()
            .filter(|(me, _, _)| me == "POST")
            .map(|(_, c, _)| c.clone())
            .collect()
    }
}

fn repondre(
    m: &Mutex<Modele>,
    methode: &str,
    url: &str,
    auth: Option<String>,
    corps: &str,
) -> (u16, Value) {
    let mut m = m.lock().unwrap();
    m.journal.push((methode.into(), url.into(), auth.clone()));
    let corps: Value = serde_json::from_str(corps).unwrap_or(Value::Null);
    let jeton_valide = auth
        .as_deref()
        .and_then(|a| a.strip_prefix("Bearer "))
        .is_some_and(|j| m.jetons.iter().any(|x| x == j));
    let admin = jeton_valide && m.role == "admin";
    let (chemin, requete) = url.split_once('?').unwrap_or((url, ""));
    match (methode, chemin) {
        ("GET", "/api/v1/system/version") => (200, json!({"version": m.version, "engine": "rust"})),
        ("GET", "/api/v1/auth/config") => (
            200,
            json!({"auth_enabled": m.auth_enabled, "has_jwt_secret": true, "has_api_key": false}),
        ),
        ("POST", "/api/v1/auth/login") => {
            let (u, p) = (
                corps["username"].as_str().unwrap_or(""),
                corps["password"].as_str().unwrap_or(""),
            );
            if u != "default" {
                return (401, json!({"error": "invalid credentials"}));
            }
            let ok = match &m.mdp_default {
                Some(h) => h == p,
                // rc12 : revendicable tant que l'auth est coupée ; le mot de
                // passe fourni devient le hash.
                None => !m.auth_enabled,
            };
            if !ok {
                return (401, json!({"error": "invalid credentials"}));
            }
            if m.mdp_default.is_none() {
                m.mdp_default = Some(p.to_string());
            }
            let j = format!("jeton-{}", m.jetons.len() + 1);
            m.jetons.push(j.clone());
            (
                200,
                json!({"token": j, "token_type": "Bearer", "username": u, "user_id": 1, "role": m.role}),
            )
        }
        ("POST", "/api/v1/auth/config") => {
            if m.auth_enabled && !admin {
                return (401, json!({"error": "authentication required"}));
            }
            if let Some(b) = corps["auth_enabled"].as_bool() {
                m.auth_enabled = b;
            }
            (200, json!({"auth_enabled": m.auth_enabled}))
        }
        ("POST", "/api/v1/system/update/install") => {
            // AdminOuCetteMachine sous Tune OS (relais du port 80).
            if !m.auth_enabled {
                return (
                    403,
                    json!({"error": "auth_required", "reason": "port80_relay",
                    "message": "This appliance relays port 80 to the server from 127.0.0.1, so a local call cannot be told apart from the network: enable authentication and sign in as an administrator (or rebuild the Tune OS image)."}),
                );
            }
            if !admin {
                return (401, json!({"error": "authentication required"}));
            }
            let force = requete.contains("force=true");
            if !force && m.analyse {
                return (
                    409,
                    json!({"status": "blocked", "reason": "scan_in_progress", "force_available": true}),
                );
            }
            if !force && m.lecture {
                return (
                    409,
                    json!({"status": "blocked", "reason": "playback_in_progress", "zones": [{"id": 1, "name": "Salon"}], "force_available": true}),
                );
            }
            m.phase = Some("downloading".into());
            (
                200,
                json!({"status": "downloading", "version": "1.0.0-rc13.1"}),
            )
        }
        ("GET", "/api/v1/system/update/status") => {
            if m.auth_enabled && !jeton_valide {
                return (401, json!({"error": "authentication required"}));
            }
            (
                200,
                json!({"current_version": m.version, "phase": m.phase, "update_in_progress": m.phase.is_some()}),
            )
        }
        _ => (404, json!({"error": "not found"})),
    }
}

fn demande(mdp: &str, nouveau: bool, force: bool) -> Demande {
    Demande {
        identifiant: "default".into(),
        mot_de_passe: mdp.into(),
        nouveau,
        force,
    }
}

// ---------------------------------------------------------------------------

#[test]
fn le_faux_tune_rc12_refuse_bien_la_mise_a_jour_sans_authentification() {
    // Contre-épreuve : l'appel direct, celui que fait l'interface de la rc12,
    // est refusé comme sur la machine d'eric.
    let f = FauxTune::demarrer("1.0.0-rc12", false, None);
    let r = ureq::post(&format!("{}/api/v1/system/update/install", f.base)).send_json(json!({}));
    match r {
        Err(ureq::Error::Status(403, rep)) => {
            let v: Value = rep.into_json().unwrap();
            assert_eq!(v["reason"], "port80_relay");
        }
        autre => panic!("le faux Tune devait refuser en 403 : {autre:?}"),
    }
}

#[test]
fn rc12_auth_coupee_le_parcours_complet_debloque_et_lance_la_mise_a_jour() {
    let f = FauxTune::demarrer("1.0.0-rc12", false, None);
    let c = f.client();
    let d = tune::diagnostiquer(&c).unwrap();
    assert_eq!(d.etat, Etat::ADebloquer);
    assert_eq!(d.version, "1.0.0-rc12");

    let r = tune::debloquer(&c, &demande("un-bon-mot-de-passe", true, false)).unwrap();
    assert_eq!(
        r.etapes,
        [
            "connexion",
            "authentification_activee",
            "mise_a_jour_demandee"
        ]
    );
    assert_eq!(r.issue, "mise_a_jour_lancee");
    assert_eq!(r.version_cible.as_deref(), Some("1.0.0-rc13.1"));

    let m = f.m.lock().unwrap();
    assert!(m.auth_enabled, "l'authentification doit être active");
    assert_eq!(
        m.mdp_default.as_deref(),
        Some("un-bon-mot-de-passe"),
        "le mot de passe choisi est celui de default"
    );
    let install = m
        .journal
        .iter()
        .find(|(_, c, _)| c.starts_with("/api/v1/system/update/install"))
        .unwrap();
    assert_eq!(
        install.1, "/api/v1/system/update/install",
        "jamais de force sans clic"
    );
    assert_eq!(install.2.as_deref(), Some("Bearer jeton-1"));
    // Ordre : connexion, activation, installation.
    let posts: Vec<&str> = m
        .journal
        .iter()
        .filter(|(me, _, _)| me == "POST")
        .map(|(_, c, _)| c.as_str())
        .collect();
    assert_eq!(
        posts,
        [
            "/api/v1/auth/login",
            "/api/v1/auth/config",
            "/api/v1/system/update/install"
        ]
    );
}

#[test]
fn rc12_mot_de_passe_trop_court_rien_n_est_ecrit() {
    let f = FauxTune::demarrer("1.0.0-rc12", false, None);
    let e = tune::debloquer(&f.client(), &demande("court", true, false)).unwrap_err();
    assert_eq!(e.code, "mot_de_passe_trop_court");
    assert!(
        f.ecritures().is_empty(),
        "aucun appel en écriture : {:?}",
        f.ecritures()
    );
    assert!(!f.m.lock().unwrap().auth_enabled);
}

#[test]
fn rc12_mot_de_passe_court_refuse_meme_sans_le_drapeau_nouveau() {
    // La page n'est pas crue sur parole : `nouveau: false` envoyé sur un
    // `default` revendicable poserait sinon un mot de passe de 3 caractères.
    let f = FauxTune::demarrer("1.0.0-rc12", false, None);
    let e = tune::debloquer(&f.client(), &demande("abc", false, false)).unwrap_err();
    assert_eq!(e.code, "mot_de_passe_trop_court");
    assert!(
        f.ecritures().is_empty(),
        "aucun appel en écriture : {:?}",
        f.ecritures()
    );
    assert!(f.m.lock().unwrap().mdp_default.is_none());
}

#[test]
fn rc12_default_a_deja_un_mot_de_passe_on_le_demande_puis_on_reprend() {
    let f = FauxTune::demarrer("1.0.0-rc12", false, Some("ancien-secret"));
    let c = f.client();
    let e = tune::debloquer(&c, &demande("nouveau-secret", true, false)).unwrap_err();
    assert_eq!(e.code, "mot_de_passe_existant_requis");
    assert!(
        !f.m.lock().unwrap().auth_enabled,
        "rien n'est activé sur un échec de connexion"
    );

    let r = tune::debloquer(&c, &demande("ancien-secret", false, false)).unwrap();
    assert_eq!(r.issue, "mise_a_jour_lancee");
    let m = f.m.lock().unwrap();
    assert!(m.auth_enabled);
    assert_eq!(
        m.mdp_default.as_deref(),
        Some("ancien-secret"),
        "le mot de passe existant est conservé"
    );
}

#[test]
fn rc12_auth_deja_active_connexion_puis_mise_a_jour_sans_toucher_a_la_config() {
    let f = FauxTune::demarrer("1.0.0-rc12", true, Some("secret-existant"));
    let c = f.client();
    assert_eq!(tune::diagnostiquer(&c).unwrap().etat, Etat::AuthActive);

    let e = tune::debloquer(&c, &demande("mauvais", false, false)).unwrap_err();
    assert_eq!(e.code, "mot_de_passe_incorrect");
    assert_eq!(
        f.ecritures(),
        ["/api/v1/auth/login"],
        "pas d'installation après un refus"
    );

    let r = tune::debloquer(&c, &demande("secret-existant", false, false)).unwrap();
    assert_eq!(r.etapes, ["connexion", "mise_a_jour_demandee"]);
    assert!(
        !f.ecritures().iter().any(|p| p == "/api/v1/auth/config"),
        "la config n'est pas touchée"
    );
}

#[test]
fn rc13_et_plus_rien_n_est_force() {
    for v in ["1.0.0-rc13", "1.0.0-rc13.1", "1.0.0-rc14", "1.0.0"] {
        // Même un serveur ouvert et revendicable : l'outil ne s'en sert pas.
        let f = FauxTune::demarrer(v, false, None);
        let c = f.client();
        assert_eq!(tune::diagnostiquer(&c).unwrap().etat, Etat::Recent, "{v}");
        let e = tune::debloquer(&c, &demande("un-bon-mot-de-passe", true, false)).unwrap_err();
        assert_eq!(e.code, "version_recente", "{v}");
        assert!(
            f.ecritures().is_empty(),
            "{v} : aucun appel en écriture, vu {:?}",
            f.ecritures()
        );
    }
}

#[test]
fn version_illisible_rien_n_est_fait() {
    let f = FauxTune::demarrer("inconnue", false, None);
    assert_eq!(
        tune::diagnostiquer(&f.client()).unwrap().etat,
        Etat::VersionInconnue
    );
    assert_eq!(
        tune::debloquer(&f.client(), &demande("un-bon-mot-de-passe", true, false))
            .unwrap_err()
            .code,
        "version_inconnue"
    );
    assert!(f.ecritures().is_empty());
}

#[test]
fn compte_non_administrateur_on_s_arrete_avant_d_activer() {
    let f = FauxTune::demarrer("1.0.0-rc12", false, None);
    f.m.lock().unwrap().role = "user".into();
    let e = tune::debloquer(&f.client(), &demande("un-bon-mot-de-passe", true, false)).unwrap_err();
    assert_eq!(e.code, "pas_administrateur");
    assert!(
        !f.m.lock().unwrap().auth_enabled,
        "activer avec un non-admin fermerait la porte à tous"
    );
}

#[test]
fn lecture_en_cours_il_faut_un_clic_pour_forcer() {
    let f = FauxTune::demarrer("1.0.0-rc12", false, None);
    f.m.lock().unwrap().lecture = true;
    let c = f.client();
    let r = tune::debloquer(&c, &demande("un-bon-mot-de-passe", true, false)).unwrap();
    assert_eq!(r.issue, "lecture_en_cours");
    assert!(r.zones.is_some());
    assert!(
        f.m.lock().unwrap().phase.is_none(),
        "rien n'est lancé sans le clic"
    );

    // Le clic : la page relance avec force. L'auth est active, on se reconnecte.
    let r = tune::debloquer(&c, &demande("un-bon-mot-de-passe", false, true)).unwrap();
    assert_eq!(r.issue, "mise_a_jour_lancee");
    let m = f.m.lock().unwrap();
    assert!(m
        .journal
        .iter()
        .any(|(_, c, _)| c == "/api/v1/system/update/install?force=true"));
}

#[test]
fn analyse_en_cours_demande_aussi_un_clic() {
    let f = FauxTune::demarrer("1.0.0-rc12", false, None);
    f.m.lock().unwrap().analyse = true;
    let r = tune::debloquer(&f.client(), &demande("un-bon-mot-de-passe", true, false)).unwrap();
    assert_eq!(r.issue, "analyse_en_cours");
    assert!(f.m.lock().unwrap().phase.is_none());
}

#[test]
fn suivi_phase_puis_serveur_injoignable() {
    let f = FauxTune::demarrer("1.0.0-rc12", false, None);
    let c = f.client();
    let r = tune::debloquer(&c, &demande("un-bon-mot-de-passe", true, false)).unwrap();
    let s = tune::suivre(&c, Some(&r.jeton));
    assert!(s.joignable && s.en_cours);
    assert_eq!(s.phase.as_deref(), Some("downloading"));
    // Redémarrage : plus personne au bout.
    let base = f.base.clone();
    drop(f);
    thread::sleep(std::time::Duration::from_millis(200));
    let s = tune::suivre(&ClientTune::new(&base), Some(&r.jeton));
    assert!(!s.joignable);
}

// ---------------------------------------------------------------------------
// La page locale et ses gardes.
// ---------------------------------------------------------------------------

fn requete<'a>(
    etat: &web::Etat,
    methode: &'a str,
    chemin: &'a str,
    hote: &'a str,
    corps: &'a str,
) -> web::Reponse {
    let _ = etat;
    web::traiter(
        etat,
        &web::Requete {
            methode,
            chemin,
            hote: Some(hote),
            origine: None,
            type_contenu: Some("application/json"),
            corps,
        },
    )
}

#[test]
fn la_page_locale_exige_jeton_hote_et_post_json() {
    let mut etat = web::Etat::new("abc123".into(), 4321);
    etat.chercher = |_| Vec::new();
    let hote = "127.0.0.1:4321";
    assert_eq!(requete(&etat, "GET", "/s/abc123/", hote, "").code, 200);
    assert!(requete(&etat, "GET", "/s/abc123/", hote, "")
        .corps
        .contains("Débloquer Tune"));
    // Mauvais jeton, pas de jeton.
    assert_eq!(requete(&etat, "GET", "/s/zzz/", hote, "").code, 404);
    assert_eq!(requete(&etat, "GET", "/", hote, "").code, 404);
    // Rebinding DNS : un autre nom d'hôte.
    assert_eq!(
        requete(&etat, "GET", "/s/abc123/", "attaquant.exemple:4321", "").code,
        421
    );
    // Une autre origine.
    let r = web::traiter(
        &etat,
        &web::Requete {
            methode: "POST",
            chemin: "/s/abc123/api/presence",
            hote: Some(hote),
            origine: Some("http://attaquant.exemple"),
            type_contenu: Some("application/json"),
            corps: "{}",
        },
    );
    assert_eq!(r.code, 403);
    // GET sur une action, ou POST sans JSON (formulaire d'une autre page).
    assert_eq!(
        requete(&etat, "GET", "/s/abc123/api/presence", hote, "").code,
        405
    );
    let r = web::traiter(
        &etat,
        &web::Requete {
            methode: "POST",
            chemin: "/s/abc123/api/presence",
            hote: Some(hote),
            origine: None,
            type_contenu: Some("text/plain"),
            corps: "{}",
        },
    );
    assert_eq!(r.code, 405);
    assert_eq!(
        requete(&etat, "POST", "/s/abc123/api/presence", hote, "{}").code,
        200
    );
}

#[test]
fn la_page_refuse_une_adresse_hors_reseau_local() {
    let etat = web::Etat::new("abc".into(), 1);
    let r = requete(
        &etat,
        "POST",
        "/s/abc/api/diagnostiquer",
        "127.0.0.1:1",
        r#"{"adresse":"8.8.8.8"}"#,
    );
    assert!(r.corps.contains("adresse_hors_reseau_local"), "{}", r.corps);
    let r = requete(
        &etat,
        "POST",
        "/s/abc/api/debloquer",
        "127.0.0.1:1",
        r#"{"base":"http://8.8.8.8:8888","mot_de_passe":"xxxxxxxx","nouveau":true}"#,
    );
    assert!(r.corps.contains("adresse_hors_reseau_local"), "{}", r.corps);
}

#[test]
fn de_bout_en_bout_par_la_page_locale() {
    let f = FauxTune::demarrer("1.0.0-rc12", false, None);
    let etat = web::Etat::new("j".into(), 9);
    let h = "127.0.0.1:9";
    let corps = json!({"adresse": f.base}).to_string();
    let r: Value =
        serde_json::from_str(&requete(&etat, "POST", "/s/j/api/diagnostiquer", h, &corps).corps)
            .unwrap();
    assert_eq!(r["diagnostic"]["etat"], "a_debloquer");
    let corps = json!({"base": f.base, "identifiant": "default", "mot_de_passe": "un-bon-mot-de-passe", "nouveau": true}).to_string();
    let r: Value =
        serde_json::from_str(&requete(&etat, "POST", "/s/j/api/debloquer", h, &corps).corps)
            .unwrap();
    assert_eq!(r["resultat"]["issue"], "mise_a_jour_lancee", "{r}");
    assert!(
        r["resultat"].get("jeton").is_none(),
        "le jeton ne part jamais vers la page"
    );
    let corps = json!({"base": f.base}).to_string();
    let r: Value =
        serde_json::from_str(&requete(&etat, "POST", "/s/j/api/suivre", h, &corps).corps).unwrap();
    assert_eq!(
        r["suivi"]["phase"], "downloading",
        "le suivi passe le jeton gardé en mémoire : {r}"
    );
}
