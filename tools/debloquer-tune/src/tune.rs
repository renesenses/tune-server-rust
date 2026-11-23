//! Le dialogue avec le serveur Tune, et la logique d'état.
//!
//! Routes de la rc12, vérifiées dans `v1.0.0-rc12:tune-server/src/` :
//!
//! - `GET  /api/v1/system/version` (publique) → `{version, engine}` ;
//! - `GET  /api/v1/auth/config` (publique) → `{auth_enabled, …}` ;
//! - `POST /api/v1/auth/login` (publique) → `{token, role, user_id}`. Tant que
//!   l'authentification est coupée, le profil `default` SANS mot de passe se
//!   revendique : le mot de passe fourni est enregistré (argon2) ;
//! - `POST /api/v1/auth/config {"auth_enabled":true}` : ouverte quand
//!   l'authentification est coupée, réservée à l'administrateur sinon ;
//! - `POST /api/v1/system/update/install[?force=true]` : garde
//!   `AdminOuCetteMachine`, qui sous Tune OS (relais du port 80) exige un
//!   administrateur authentifié ;
//! - `GET  /api/v1/system/update/status` : authentifiée quand l'auth est active.
//!
//! Rien ici ne force une version rc13 ou plus récente (voir [`crate::version`]).

use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::version::{famille, Famille};

const AGENT_UTILISATEUR: &str = concat!("debloquer-tune/", env!("CARGO_PKG_VERSION"));
pub const LONGUEUR_MINIMALE: usize = 8;

pub struct ClientTune {
    base: String,
    court: ureq::Agent,
    long: ureq::Agent,
}

/// Réponse HTTP lue : code et corps JSON (`Null` si le corps n'en est pas).
type Lu = Result<(u16, Value), String>;

fn agent(delai: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(4))
        .timeout(delai)
        .user_agent(AGENT_UTILISATEUR)
        // Jamais de redirection : un 30x vers ailleurs emporterait le jeton.
        .redirects(0)
        .build()
}

fn lire(r: Result<ureq::Response, ureq::Error>) -> Lu {
    match r {
        Ok(rep) => {
            let code = rep.status();
            Ok((code, rep.into_json::<Value>().unwrap_or(Value::Null)))
        }
        Err(ureq::Error::Status(code, rep)) => {
            Ok((code, rep.into_json::<Value>().unwrap_or(Value::Null)))
        }
        Err(ureq::Error::Transport(t)) => Err(t.kind().to_string()),
    }
}

impl ClientTune {
    /// `base` : `http://ip:port`, déjà vérifiée locale par [`crate::adresse`].
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            court: agent(Duration::from_secs(10)),
            // L'installation interroge GitHub avant de répondre.
            long: agent(Duration::from_secs(90)),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, chemin: &str) -> String {
        format!("{}/api/v1{}", self.base, chemin)
    }

    fn get(&self, chemin: &str, jeton: Option<&str>) -> Lu {
        let mut r = self.court.get(&self.url(chemin));
        if let Some(j) = jeton {
            r = r.set("Authorization", &format!("Bearer {j}"));
        }
        lire(r.call())
    }

    fn post(&self, agent: &ureq::Agent, chemin: &str, corps: Value, jeton: Option<&str>) -> Lu {
        let mut r = agent.post(&self.url(chemin));
        if let Some(j) = jeton {
            r = r.set("Authorization", &format!("Bearer {j}"));
        }
        lire(r.send_json(corps))
    }
}

// ---------------------------------------------------------------------------
// Diagnostic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Etat {
    /// rc12 ou avant, authentification coupée : choisir un mot de passe.
    ADebloquer,
    /// rc12 ou avant, authentification déjà active : saisir le mot de passe.
    AuthActive,
    /// rc13 ou plus récent : renvoyer au fil épinglé, ne rien faire.
    Recent,
    /// Version illisible : ne rien faire.
    VersionInconnue,
}

#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub base: String,
    pub version: String,
    pub auth_active: bool,
    pub etat: Etat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Echec {
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn echec(code: &'static str) -> Echec {
    Echec { code, detail: None }
}

fn echec_detail(code: &'static str, detail: impl Into<String>) -> Echec {
    Echec {
        code,
        detail: Some(detail.into()),
    }
}

pub fn diagnostiquer(c: &ClientTune) -> Result<Diagnostic, Echec> {
    let (code, corps) = c
        .get("/system/version", None)
        .map_err(|e| echec_detail("injoignable", e))?;
    let version = match (code, corps["version"].as_str()) {
        (200, Some(v)) if !v.is_empty() => v.to_string(),
        _ => return Err(echec("pas_un_tune")),
    };
    let (code, corps) = c
        .get("/auth/config", None)
        .map_err(|e| echec_detail("injoignable", e))?;
    let auth_active = match (code, corps["auth_enabled"].as_bool()) {
        (200, Some(b)) => b,
        _ => return Err(echec("pas_un_tune")),
    };
    let etat = match famille(&version) {
        Famille::Rc13OuPlus => Etat::Recent,
        Famille::Inconnue => Etat::VersionInconnue,
        Famille::AvantRc13 if auth_active => Etat::AuthActive,
        Famille::AvantRc13 => Etat::ADebloquer,
    };
    Ok(Diagnostic {
        base: c.base().to_string(),
        version,
        auth_active,
        etat,
    })
}

// ---------------------------------------------------------------------------
// Déblocage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Demande {
    pub identifiant: String,
    pub mot_de_passe: String,
    /// La page a demandé un NOUVEAU mot de passe (cas « à débloquer »).
    pub nouveau: bool,
    /// Clic explicite de l'utilisateur après un report (lecture, analyse).
    pub force: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Resultat {
    /// Étapes franchies, dans l'ordre (`connexion`, `authentification_activee`,
    /// `mise_a_jour_demandee`).
    pub etapes: Vec<&'static str>,
    /// Issue : `mise_a_jour_lancee`, `deja_a_jour`, `lecture_en_cours`,
    /// `analyse_en_cours`, `deja_en_cours`.
    pub issue: &'static str,
    pub version_depart: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_cible: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zones: Option<Value>,
    #[serde(skip)]
    pub jeton: String,
}

fn connexion(c: &ClientTune, identifiant: &str, mdp: &str, nouveau: bool) -> Result<String, Echec> {
    let (code, corps) = c
        .post(
            &c.court,
            "/auth/login",
            json!({"username": identifiant, "password": mdp}),
            None,
        )
        .map_err(|e| echec_detail("injoignable", e))?;
    match code {
        200 => {}
        // Cas « à débloquer » : `default` a déjà un mot de passe (ou n'existe
        // plus). On demandera le mot de passe existant.
        401 if nouveau => return Err(echec("mot_de_passe_existant_requis")),
        401 => return Err(echec("mot_de_passe_incorrect")),
        429 => return Err(echec("trop_de_tentatives")),
        _ => return Err(echec_detail("connexion_refusee", format!("HTTP {code}"))),
    }
    let Some(jeton) = corps["token"].as_str().filter(|t| !t.is_empty()) else {
        return Err(echec("connexion_refusee"));
    };
    // Activer l'authentification avec un compte qui n'est pas administrateur
    // fermerait la porte à tout le monde : on s'arrête AVANT.
    if corps["role"].as_str() != Some("admin") {
        return Err(echec("pas_administrateur"));
    }
    Ok(jeton.to_string())
}

fn activer_authentification(c: &ClientTune, jeton: &str) -> Result<(), Echec> {
    let (code, corps) = c
        .post(
            &c.court,
            "/auth/config",
            json!({"auth_enabled": true}),
            Some(jeton),
        )
        .map_err(|e| echec_detail("injoignable", e))?;
    if code != 200 || corps["auth_enabled"].as_bool() != Some(true) {
        return Err(echec_detail("activation_refusee", format!("HTTP {code}")));
    }
    // Relecture : on ne croit pas la réponse seule.
    let (code, corps) = c
        .get("/auth/config", None)
        .map_err(|e| echec_detail("injoignable", e))?;
    if code != 200 || corps["auth_enabled"].as_bool() != Some(true) {
        return Err(echec("activation_non_confirmee"));
    }
    Ok(())
}

pub fn debloquer(c: &ClientTune, d: &Demande) -> Result<Resultat, Echec> {
    // La page n'est pas crue sur parole : état relu ici, juste avant d'agir.
    let diag = diagnostiquer(c)?;
    let mut etapes = Vec::new();
    match diag.etat {
        Etat::Recent => Err(echec("version_recente")),
        Etat::VersionInconnue => Err(echec("version_inconnue")),
        Etat::ADebloquer => {
            if d.nouveau && d.mot_de_passe.chars().count() < LONGUEUR_MINIMALE {
                return Err(echec("mot_de_passe_trop_court"));
            }
            if d.mot_de_passe.is_empty() {
                return Err(echec("mot_de_passe_vide"));
            }
            // Seul `default` (profil 1, administrateur) se revendique.
            let jeton = connexion(c, "default", &d.mot_de_passe, d.nouveau)?;
            etapes.push("connexion");
            activer_authentification(c, &jeton)?;
            etapes.push("authentification_activee");
            installer(c, &jeton, d.force, diag.version, etapes)
        }
        Etat::AuthActive => {
            if d.mot_de_passe.is_empty() {
                return Err(echec("mot_de_passe_vide"));
            }
            let identifiant = if d.identifiant.trim().is_empty() {
                "default"
            } else {
                d.identifiant.trim()
            };
            let jeton = connexion(c, identifiant, &d.mot_de_passe, false)?;
            etapes.push("connexion");
            installer(c, &jeton, d.force, diag.version, etapes)
        }
    }
}

fn installer(
    c: &ClientTune,
    jeton: &str,
    force: bool,
    version_depart: String,
    mut etapes: Vec<&'static str>,
) -> Result<Resultat, Echec> {
    let chemin = if force {
        "/system/update/install?force=true"
    } else {
        "/system/update/install"
    };
    let (code, corps) = c
        .post(&c.long, chemin, json!({}), Some(jeton))
        .map_err(|e| echec_detail("injoignable", e))?;
    let statut = corps["status"].as_str().unwrap_or("");
    let raison = corps["reason"].as_str().unwrap_or("");
    let message = corps["message"].as_str().unwrap_or("").to_string();
    let issue = match (code, statut, raison) {
        (200 | 202, "downloading" | "homebrew_upgrade_started", _) => "mise_a_jour_lancee",
        (200, "up_to_date", _) => "deja_a_jour",
        (409, "already_in_progress", _) => "deja_en_cours",
        (409, _, "playback_in_progress") if !force => "lecture_en_cours",
        (409, _, "scan_in_progress") if !force => "analyse_en_cours",
        (401 | 403, _, _) => return Err(echec_detail("installation_refusee", message)),
        _ => {
            let detail = if message.is_empty() {
                format!("HTTP {code} {statut}")
            } else {
                message
            };
            return Err(echec_detail("installation_echouee", detail));
        }
    };
    etapes.push("mise_a_jour_demandee");
    Ok(Resultat {
        etapes,
        issue,
        version_depart,
        version_cible: corps["version"].as_str().map(str::to_string),
        zones: corps.get("zones").cloned(),
        jeton: jeton.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Suivi
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Suivi {
    /// Le serveur répond-il ? Pendant le redémarrage, non.
    pub joignable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub en_cours: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echec: Option<String>,
}

pub fn suivre(c: &ClientTune, jeton: Option<&str>) -> Suivi {
    let version = match c.get("/system/version", None) {
        Ok((200, corps)) => corps["version"].as_str().map(str::to_string),
        _ => {
            return Suivi {
                joignable: false,
                version: None,
                phase: None,
                en_cours: true,
                echec: None,
            };
        }
    };
    let (phase, en_cours, echec) = match c.get("/system/update/status", jeton) {
        Ok((200, corps)) => {
            let phase = corps["phase"].as_str().map(str::to_string);
            let echec = phase
                .as_deref()
                .filter(|p| p.starts_with("failed"))
                .map(|p| corps["message"].as_str().unwrap_or(p).to_string());
            (
                phase,
                corps["update_in_progress"].as_bool().unwrap_or(false),
                echec,
            )
        }
        // Statut illisible (jeton refusé après un redémarrage, par exemple) :
        // la version publique suffit à conclure.
        _ => (None, false, None),
    };
    Suivi {
        joignable: true,
        version,
        phase,
        en_cours,
        echec,
    }
}
