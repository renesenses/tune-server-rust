//! Activer l'authentification du serveur depuis l'interface (web#3195,
//! go de Bertrand du 18/11/2026).
//!
//! ## Le défaut
//!
//! Le refus de `AdminOuCetteMachine` (rc12) dit « enable authentication and
//! sign in as an administrator », mais aucune interface ne savait activer
//! l'authentification ; et la seule route qui écrivait `auth_enabled`
//! (`POST /auth/config`) acceptait, authentification coupée, l'appel de
//! N'IMPORTE QUI sur le réseau.
//!
//! ## La règle de sécurité retenue
//!
//! Authentification coupée, tout le réseau local parle à l'API, mais les
//! actions qui engagent la machine (éteindre, effacer un disque, installer un
//! greffon) restent réservées à « cette machine » (rc12). Activer
//! l'authentification avec un administrateur CHOISI PAR L'APPELANT donnerait
//! justement ces actions-là à celui qui active, et fermerait la porte aux
//! autres : c'est une prise de contrôle. Activer exige donc, dans la même
//! opération :
//!
//! 1. **la preuve d'avoir la main sur la machine** : soit l'appel vient de
//!    cette machine (même lecture que `AdminOuCetteMachine` : adresse de
//!    bouclage de la CONNEXION, jamais une requête relayée, jamais sur une
//!    appliance qui relaie le port 80), soit il porte le **code d'activation**
//!    que le serveur a écrit dans un fichier du dossier de sa base
//!    ([`FICHIER_CODE`]) — fichier que seul quelqu'un qui a accès à la
//!    machine (console, SSH, Cockpit, volume Docker) peut lire. Le code n'est
//!    JAMAIS écrit dans le journal : `/system/logs` le servirait au réseau.
//!    Il vaut 10 minutes, cinq essais, un seul usage ;
//! 2. **un administrateur avec mot de passe**, dont l'appelant donne le mot de
//!    passe (ou qu'il crée dans la foulée, `create_admin`) : sans lui, 409
//!    `admin_password_required` ;
//! 3. **une confirmation explicite** (`confirm: true`).
//!
//! La réponse ouvre la session de cet administrateur (jeton et cookie) :
//! celui qui active ne peut pas s'enfermer dehors. Le secret des jetons est
//! RENOUVELÉ à l'activation : un jeton fabriqué pendant que le serveur était
//! ouvert à tous ne vaut plus rien ensuite.
//!
//! Désactiver reste l'affaire d'un administrateur authentifié
//! (`POST /auth/config`, garde inchangée).
//!
//! ## La voie de secours
//!
//! Mot de passe perdu : déposer un fichier nommé [`FICHIER_DE_SECOURS`] dans le
//! dossier de la base, puis redémarrer Tune. Au démarrage, le serveur coupe
//! l'authentification et efface le fichier ([`appliquer_le_fichier_de_secours`]).
//! Écrire dans ce dossier, c'est déjà avoir la main sur la machine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use tune_core::db::backend::ToSqlValue;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

/// Le fichier, dans le dossier de la base, où le serveur écrit le code
/// d'activation.
pub const FICHIER_CODE: &str = "code-activation-authentification.txt";

/// Le fichier de secours : présent au démarrage, il coupe l'authentification.
pub const FICHIER_DE_SECOURS: &str = "desactiver-authentification";

const DUREE_DU_CODE: Duration = Duration::from_secs(10 * 60);
const ESSAIS_PAR_CODE: u32 = 5;
const LONGUEUR_DU_CODE: usize = 12;
/// Sans 0/O, 1/I/L : le code se recopie à la main depuis un écran.
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const MOT_DE_PASSE_MIN: usize = 8;

struct CodeEnCours {
    code: String,
    expire: Instant,
    essais_restants: u32,
}

/// Les codes en cours, par dossier de base (un processus de test ouvre
/// plusieurs serveurs). En mémoire seulement : jamais en base, donc jamais
/// dans une sauvegarde ni un export.
static CODES: LazyLock<Mutex<HashMap<PathBuf, CodeEnCours>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Le dossier de la base ouverte, ou `None` pour une base en mémoire.
pub fn dossier_de_la_base(state: &AppState) -> Option<PathBuf> {
    let chemin = state.config.db_path.trim();
    if chemin.is_empty() || chemin == ":memory:" || chemin.starts_with("file::memory:") {
        return None;
    }
    Some(tune_core::coffre::dossier_de_la_base(chemin))
}

/// L'appel vient-il de cette machine ? Même lecture que
/// `AdminOuCetteMachine`. N'échoue jamais : « non » quand on ne sait pas.
pub struct DepuisCetteMachine(pub bool);

impl FromRequestParts<AppState> for DepuisCetteMachine {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(super::appel_depuis_cette_machine(parts, state).await))
    }
}

/// Existe-t-il au moins un profil administrateur AVEC mot de passe ?
pub fn admin_avec_mot_de_passe_existe(state: &AppState) -> bool {
    state
        .backend
        .query_many(
            "SELECT is_admin, password_hash, password_hash_v2 FROM profiles",
            &[],
        )
        .unwrap_or_default()
        .iter()
        .any(|r| {
            let admin = r.first().and_then(|v| v.as_i64()).unwrap_or(0) != 0;
            admin && (non_vide(r.get(1)) || non_vide(r.get(2)))
        })
}

fn non_vide(v: Option<&tune_core::db::backend::SqlValue>) -> bool {
    v.and_then(|v| v.as_string()).is_some_and(|s| !s.is_empty())
}

fn refus(statut: StatusCode, code: &str, message: &str) -> Response {
    (
        statut,
        Json(json!({ "error": code, "code": code, "message": message })),
    )
        .into_response()
}

fn tirer_un_code() -> String {
    let mut octets = Vec::with_capacity(32);
    while octets.len() < LONGUEUR_DU_CODE {
        let u = uuid::Uuid::new_v4();
        // Les octets 6 et 8 d'un UUID v4 portent la version et la variante.
        octets.extend(
            u.as_bytes()
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != 6 && *i != 8)
                .map(|(_, b)| *b),
        );
    }
    octets
        .iter()
        .take(LONGUEUR_DU_CODE)
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

/// Ce que l'utilisateur tape : majuscules, sans espaces ni tirets.
fn normaliser(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn egal_en_temps_constant(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn ecrire_le_fichier_du_code(chemin: &Path, code: &str) -> std::io::Result<()> {
    use std::io::Write;
    let _ = std::fs::remove_file(chemin);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(chemin)?;
    writeln!(
        f,
        "Tune - code d'activation de l'authentification / authentication activation code\n\n{code}\n\nValable 10 minutes, un seul usage. Valid for 10 minutes, single use."
    )?;
    f.sync_all()
}

fn retirer_le_code(dossier: &Path) {
    CODES.lock().unwrap().remove(dossier);
    let _ = std::fs::remove_file(dossier.join(FICHIER_CODE));
}

enum VerdictDuCode {
    Bon,
    Absent,
    Faux,
}

/// Vérifie `saisi` sans consommer le code ; un code faux use un essai, le
/// dernier essai raté efface le code.
fn verifier_le_code(dossier: &Path, saisi: &str) -> VerdictDuCode {
    let mut codes = CODES.lock().unwrap();
    let Some(en_cours) = codes.get_mut(dossier) else {
        return VerdictDuCode::Absent;
    };
    if en_cours.expire <= Instant::now() || en_cours.essais_restants == 0 {
        codes.remove(dossier);
        let _ = std::fs::remove_file(dossier.join(FICHIER_CODE));
        return VerdictDuCode::Absent;
    }
    if egal_en_temps_constant(&normaliser(saisi), &en_cours.code) {
        return VerdictDuCode::Bon;
    }
    en_cours.essais_restants -= 1;
    if en_cours.essais_restants == 0 {
        codes.remove(dossier);
        let _ = std::fs::remove_file(dossier.join(FICHIER_CODE));
    }
    VerdictDuCode::Faux
}

// ---------------------------------------------------------------------------
// POST /auth/activation-code
// ---------------------------------------------------------------------------

/// Écrit un code d'activation dans le dossier de la base et rend le CHEMIN du
/// fichier, jamais le code. N'importe qui peut le demander : le lire exige
/// d'avoir la main sur la machine. Un code encore valable est gardé tel quel
/// (on ne fait pas tourner celui que le propriétaire est en train de lire).
pub async fn demander_un_code(State(state): State<AppState>) -> Response {
    if super::authentification_active(&state) {
        return refus(
            StatusCode::CONFLICT,
            "auth_already_enabled",
            "Authentication is already enabled.",
        );
    }
    let Some(dossier) = dossier_de_la_base(&state) else {
        return refus(
            StatusCode::SERVICE_UNAVAILABLE,
            "activation_code_unavailable",
            "This server has no data folder to write the activation code to.",
        );
    };
    let chemin = dossier.join(FICHIER_CODE);
    let mut codes = CODES.lock().unwrap();
    let maintenant = Instant::now();
    let encore_valable = codes
        .get(&dossier)
        .is_some_and(|c| c.expire > maintenant && c.essais_restants > 0)
        && chemin.is_file();
    if !encore_valable {
        let code = tirer_un_code();
        if let Err(e) = ecrire_le_fichier_du_code(&chemin, &code) {
            tracing::warn!(chemin = %chemin.display(), erreur = %e, "code_d_activation_non_ecrit");
            codes.remove(&dossier);
            return refus(
                StatusCode::INTERNAL_SERVER_ERROR,
                "activation_code_write_failed",
                "The activation code could not be written to the server's data folder.",
            );
        }
        codes.insert(
            dossier.clone(),
            CodeEnCours {
                code,
                expire: maintenant + DUREE_DU_CODE,
                essais_restants: ESSAIS_PAR_CODE,
            },
        );
        // Le chemin, JAMAIS le code : le journal est lisible par le réseau.
        tracing::info!(chemin = %chemin.display(), "code_d_activation_ecrit");
    }
    let restant = codes
        .get(&dossier)
        .map(|c| c.expire.saturating_duration_since(maintenant).as_secs())
        .unwrap_or(0);
    Json(json!({
        "file": chemin.to_string_lossy(),
        "expires_in": restant,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /auth/enable
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct DemandeActivation {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: bool,
    /// Créer cet administrateur (ou donner un mot de passe à un
    /// administrateur qui n'en a pas, comme `default`) dans la même opération.
    #[serde(default)]
    create_admin: bool,
    #[serde(default)]
    activation_code: Option<String>,
}

struct Profil {
    id: i64,
    admin: bool,
    hash_v1: Option<String>,
    hash_v2: Option<String>,
}

fn lire_profil(state: &AppState, username: &str) -> Option<Profil> {
    state
        .backend
        .query_one(
            "SELECT id, password_hash, password_hash_v2, is_admin FROM profiles WHERE username = ?",
            &[&username as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .map(|r| Profil {
            id: r.first().and_then(|v| v.as_i64()).unwrap_or(0),
            hash_v1: r
                .get(1)
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty()),
            hash_v2: r
                .get(2)
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty()),
            admin: r.get(3).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        })
}

fn mot_de_passe_juste(p: &Profil, saisi: &str) -> bool {
    if let Some(h) = &p.hash_v2
        && super::verify_password(saisi, h)
    {
        return true;
    }
    if let Some(h) = &p.hash_v1 {
        let calcule = format!("{:x}", Sha256::digest(saisi.as_bytes()));
        return egal_en_temps_constant(&calcule, h);
    }
    false
}

/// Active l'authentification. Voir l'en-tête du module pour la règle.
pub async fn activer(
    State(state): State<AppState>,
    ConnectInfo(pair): ConnectInfo<std::net::SocketAddr>,
    DepuisCetteMachine(locale): DepuisCetteMachine,
    headers: axum::http::HeaderMap,
    Json(demande): Json<DemandeActivation>,
) -> Response {
    if super::authentification_active(&state) {
        return refus(
            StatusCode::CONFLICT,
            "auth_already_enabled",
            "Authentication is already enabled.",
        );
    }
    if !demande.confirm {
        return refus(
            StatusCode::BAD_REQUEST,
            "confirmation_required",
            "Enabling authentication must be confirmed explicitly (confirm: true).",
        );
    }

    // 1. La preuve d'avoir la main sur la machine.
    let dossier = dossier_de_la_base(&state);
    let par_le_code = if locale {
        false
    } else {
        let saisi = demande
            .activation_code
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let (Some(saisi), Some(dossier)) = (saisi, dossier.as_deref()) else {
            return refus(
                StatusCode::FORBIDDEN,
                "activation_code_required",
                "From another device, enabling authentication requires the activation code \
                 the server writes to its data folder.",
            );
        };
        match verifier_le_code(dossier, saisi) {
            VerdictDuCode::Bon => true,
            VerdictDuCode::Absent | VerdictDuCode::Faux => {
                tracing::warn!(locale, "activation_authentification_code_refuse");
                return refus(
                    StatusCode::FORBIDDEN,
                    "activation_code_invalid",
                    "The activation code is wrong or has expired. Request a new one.",
                );
            }
        }
    };

    // 2. L'administrateur.
    let ip = crate::garde_du_relais::adresse_de_l_appelant(pair.ip(), &headers);
    if super::login_rate_limited(ip) {
        return refus(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many attempts, try again later.",
        );
    }
    let username = demande.username.trim().to_string();
    if username.is_empty() || demande.password.is_empty() {
        return refus(
            StatusCode::BAD_REQUEST,
            "credentials_required",
            "An administrator username and password are required.",
        );
    }
    let trop_court = demande.password.chars().count() < MOT_DE_PASSE_MIN;
    let profil_id = match (lire_profil(&state, &username), demande.create_admin) {
        (Some(p), false) if p.admin && (p.hash_v1.is_some() || p.hash_v2.is_some()) => {
            if !mot_de_passe_juste(&p, &demande.password) {
                super::record_login_failure(ip);
                return refus(
                    StatusCode::UNAUTHORIZED,
                    "invalid_credentials",
                    "Wrong administrator username or password.",
                );
            }
            p.id
        }
        (Some(p), false) if p.admin => {
            return refus(
                StatusCode::CONFLICT,
                "admin_password_required",
                "This administrator has no password yet: create one first.",
            );
        }
        (Some(_), false) => {
            return refus(
                StatusCode::FORBIDDEN,
                "admin_required",
                "This profile is not an administrator.",
            );
        }
        (None, false) => {
            if !admin_avec_mot_de_passe_existe(&state) {
                return refus(
                    StatusCode::CONFLICT,
                    "admin_password_required",
                    "No administrator has a password yet: create one first.",
                );
            }
            super::record_login_failure(ip);
            return refus(
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                "Wrong administrator username or password.",
            );
        }
        // Créer : un administrateur sans mot de passe (`default`) en reçoit un.
        (Some(p), true) if p.admin && p.hash_v1.is_none() && p.hash_v2.is_none() => {
            if trop_court {
                return refus(
                    StatusCode::BAD_REQUEST,
                    "password_too_short",
                    "The password must be at least 8 characters long.",
                );
            }
            let Ok(hash) = super::hash_password(&demande.password) else {
                return refus(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "password_hash_failed",
                    "The password could not be stored.",
                );
            };
            if state
                .backend
                .execute(
                    "UPDATE profiles SET password_hash_v2 = ? WHERE id = ?",
                    &[&hash as &dyn ToSqlValue, &p.id as &dyn ToSqlValue],
                )
                .is_err()
            {
                return refus(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "password_hash_failed",
                    "The password could not be stored.",
                );
            }
            p.id
        }
        (Some(_), true) => {
            return refus(
                StatusCode::CONFLICT,
                "username_taken",
                "A profile with this name already exists.",
            );
        }
        (None, true) => {
            if trop_court {
                return refus(
                    StatusCode::BAD_REQUEST,
                    "password_too_short",
                    "The password must be at least 8 characters long.",
                );
            }
            let Ok(hash) = super::hash_password(&demande.password) else {
                return refus(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "password_hash_failed",
                    "The password could not be stored.",
                );
            };
            let admin: i64 = 1;
            match state.backend.execute_returning_id(
                "INSERT INTO profiles (username, display_name, password_hash_v2, is_admin) VALUES (?, ?, ?, ?)",
                &[
                    &username as &dyn ToSqlValue,
                    &username as &dyn ToSqlValue,
                    &hash as &dyn ToSqlValue,
                    &admin as &dyn ToSqlValue,
                ],
            ) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(erreur = %e, "activation_authentification_profil_non_cree");
                    return refus(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "admin_create_failed",
                        "The administrator profile could not be created.",
                    );
                }
            }
        }
    };

    // 3. Activer — secret des jetons RENOUVELÉ : un jeton fabriqué pendant
    // que le serveur était ouvert à tous ne vaut plus rien.
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let secret = uuid::Uuid::new_v4().to_string();
    if settings.set("jwt_secret", &secret).is_err() || settings.set("auth_enabled", "true").is_err()
    {
        return refus(
            StatusCode::INTERNAL_SERVER_ERROR,
            "auth_enable_failed",
            "Authentication could not be enabled.",
        );
    }
    if let Some(d) = dossier.as_deref()
        && par_le_code
    {
        retirer_le_code(d);
    }
    super::clear_login_failures(ip);
    tracing::warn!(
        administrateur = %username,
        depuis_cette_machine = locale,
        par_le_code,
        cree = demande.create_admin,
        "authentification_activee"
    );

    // 4. La session de cet administrateur : pas de verrouillage dehors.
    let jeton = match super::sign_jwt(profil_id, "admin", &secret) {
        Ok(j) => j,
        Err(_) => {
            return refus(
                StatusCode::INTERNAL_SERVER_ERROR,
                "token_failed",
                "Authentication is enabled but the session could not be opened: sign in.",
            );
        }
    };
    let cookie = super::session_cookie(&jeton, &headers);
    let mut reponse = Json(json!({
        "auth_enabled": true,
        "token": jeton,
        "token_type": "Bearer",
        "expires_in": 86400,
        "user": { "id": profil_id, "username": username, "role": "admin" },
    }))
    .into_response();
    if let Ok(v) = cookie.parse() {
        reponse
            .headers_mut()
            .insert(axum::http::header::SET_COOKIE, v);
    }
    reponse
}

// ---------------------------------------------------------------------------
// Voie de secours
// ---------------------------------------------------------------------------

/// Au démarrage : si [`FICHIER_DE_SECOURS`] est dans le dossier de la base,
/// couper l'authentification et effacer le fichier. Rend `true` quand il a
/// agi.
pub fn appliquer_le_fichier_de_secours(state: &AppState) -> bool {
    let Some(dossier) = dossier_de_la_base(state) else {
        return false;
    };
    let fichier = dossier.join(FICHIER_DE_SECOURS);
    if !fichier.exists() {
        return false;
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let coupee = settings.set("auth_enabled", "false").is_ok();
    let efface = std::fs::remove_file(&fichier).is_ok();
    tracing::warn!(
        fichier = %fichier.display(),
        coupee,
        efface,
        "authentification_coupee_par_le_fichier_de_secours"
    );
    coupee
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_code_est_long_et_sans_caracteres_ambigus() {
        let a = tirer_un_code();
        let b = tirer_un_code();
        assert_eq!(a.len(), LONGUEUR_DU_CODE);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| ALPHABET.contains(&c)), "{a}");
        for ambigu in ['0', 'O', '1', 'I', 'L'] {
            assert!(!a.contains(ambigu));
        }
    }

    #[test]
    fn la_saisie_est_normalisee() {
        assert_eq!(normaliser(" abcd-efgh 2345 "), "ABCDEFGH2345");
    }

    #[test]
    fn comparaison() {
        assert!(egal_en_temps_constant("ABC", "ABC"));
        assert!(!egal_en_temps_constant("ABC", "ABD"));
        assert!(!egal_en_temps_constant("ABC", "ABCD"));
    }

    #[test]
    fn cinq_essais_faux_effacent_le_code() {
        let tmp = tempfile::tempdir().unwrap();
        let dossier = tmp.path().to_path_buf();
        CODES.lock().unwrap().insert(
            dossier.clone(),
            CodeEnCours {
                code: "ABCDEFGHJKMN".into(),
                expire: Instant::now() + DUREE_DU_CODE,
                essais_restants: ESSAIS_PAR_CODE,
            },
        );
        for _ in 0..ESSAIS_PAR_CODE {
            assert!(matches!(
                verifier_le_code(&dossier, "FAUX"),
                VerdictDuCode::Faux
            ));
        }
        // Contre-épreuve : le BON code ne passe plus, il a été effacé.
        assert!(matches!(
            verifier_le_code(&dossier, "ABCDEFGHJKMN"),
            VerdictDuCode::Absent
        ));
    }

    #[test]
    fn un_code_expire_ne_vaut_rien() {
        let tmp = tempfile::tempdir().unwrap();
        let dossier = tmp.path().to_path_buf();
        CODES.lock().unwrap().insert(
            dossier.clone(),
            CodeEnCours {
                code: "ABCDEFGHJKMN".into(),
                expire: Instant::now() - Duration::from_secs(1),
                essais_restants: ESSAIS_PAR_CODE,
            },
        );
        assert!(matches!(
            verifier_le_code(&dossier, "ABCDEFGHJKMN"),
            VerdictDuCode::Absent
        ));
    }
}
