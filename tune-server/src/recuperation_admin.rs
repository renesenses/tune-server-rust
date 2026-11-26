//! Retrouver l'accès administrateur depuis la machine du serveur.
//!
//! Un mot de passe administrateur perdu, authentification active, ne laissait
//! aucun chemin de retour hors Tune OS (où `--tune-os-reinitialiser-mot-de-passe`
//! ne couvre que le compte système `tune`). La preuve demandée ici est celle
//! qu'on a la main sur la machine : lancer le binaire avec les mêmes
//! `TUNE_DATA_DIR` / `TUNE_DB_PATH` que le service, donc pouvoir écrire dans sa
//! base.
//!
//! Deux façons de finir, toutes deux depuis la machine :
//!
//! * `tune-server --reinitialiser-admin` (alias `--reset-admin`) écrit un code
//!   à usage unique sur la console et dans le fichier journal local. La base ne
//!   garde que son EMPREINTE (Argon2), sa date d'émission et le compte de ses
//!   essais ratés. Le code se saisit ensuite, avec le nouveau mot de passe,
//!   dans `POST /auth/admin/reinitialiser`, accepté seulement d'un appel de
//!   cette machine (voir `crate::auth`).
//! * `tune-server --reinitialiser-admin --mot-de-passe-stdin` lit le nouveau
//!   mot de passe sur l'entrée standard et le pose directement. C'est le seul
//!   chemin là où aucun navigateur n'atteint le serveur par le bouclage :
//!   Docker (le navigateur de l'hôte arrive par le pont du conteneur), Tune OS
//!   tant que le relais du port 80 est installé (le bouclage n'y prouve rien),
//!   un serveur sans écran. Il ne donne rien de plus que le premier : qui lance
//!   la commande écrit déjà dans la base.
//!
//! Aucune des deux n'affiche jamais de mot de passe. Après un changement, les
//! sessions existantes sont révoquées (nouveau secret de signature).

use std::sync::Arc;

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

use crate::config::TuneConfig;

/// Drapeau de la sous-commande.
pub const DRAPEAU: &str = "--reinitialiser-admin";
/// Son alias anglais.
pub const DRAPEAU_EN: &str = "--reset-admin";
/// Finir dans le terminal : nouveau mot de passe lu sur l'entrée standard.
pub const DRAPEAU_STDIN: &str = "--mot-de-passe-stdin";
/// Son alias anglais.
pub const DRAPEAU_STDIN_EN: &str = "--password-stdin";

/// Réglage : empreinte Argon2 du code en cours (jamais le code en clair).
pub const CLE_EMPREINTE: &str = "admin_reinit_code_empreinte";
/// Réglage : date d'émission du code, en secondes Unix.
pub const CLE_EMIS: &str = "admin_reinit_code_emis";
/// Réglage : essais ratés sur le code en cours.
pub const CLE_ESSAIS: &str = "admin_reinit_code_essais";

/// Durée de vie du code : celle du code d'activation.
pub const DUREE_CODE: std::time::Duration = crate::auth::DUREE_CODE_ACTIVATION;
/// Essais faux tolérés avant que le code soit détruit : ceux du code
/// d'activation.
pub const ESSAIS_CODE: u32 = crate::auth::ESSAIS_CODE_ACTIVATION;

/// Longueur minimale du nouveau mot de passe : celle de l'activation.
pub const LONGUEUR_MIN_MOT_DE_PASSE: usize = 8;

/// La ligne de commande demande-t-elle la sous-commande ? Correspondance
/// exacte, jamais un préfixe, comme `--version`.
pub fn demandee<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| a == DRAPEAU || a == DRAPEAU_EN)
}

fn mode_stdin<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter()
        .any(|a| a == DRAPEAU_STDIN || a == DRAPEAU_STDIN_EN)
}

fn maintenant() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Émet un nouveau code : il remplace celui qui serait en cours, essais remis
/// à zéro. Rend le code EN CLAIR pour l'appelant, qui l'écrit sur la console ;
/// la base n'en garde que l'empreinte.
pub fn emettre_un_code(settings: &SettingsRepo) -> Result<String, String> {
    let code = crate::auth::nouveau_code_activation();
    let empreinte = crate::auth::hash_password(&code)?;
    // L'ancien code d'abord : jamais deux codes valables à la fois, même si
    // une écriture échoue à mi-chemin.
    oublier_le_code(settings);
    settings.set(CLE_ESSAIS, "0")?;
    settings.set(CLE_EMIS, &maintenant().to_string())?;
    settings.set(CLE_EMPREINTE, &empreinte)?;
    Ok(code)
}

/// Efface le code en cours, s'il y en a un.
pub fn oublier_le_code(settings: &SettingsRepo) {
    for cle in [CLE_EMPREINTE, CLE_EMIS, CLE_ESSAIS] {
        settings.delete(cle).ok();
    }
}

/// Ce que vaut un code saisi.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Juste. Le code n'est consommé qu'avec le changement de mot de passe,
    /// dans la même transaction (voir [`reinitialiser_avec_code`]).
    Juste,
    /// Faux ; il reste `restants` essais sur ce code.
    Faux { restants: u32 },
    /// Aucun code en cours, ou code expiré (alors effacé).
    AucunOuExpire,
    /// Trop d'essais faux : le code est détruit.
    Bloque,
}

/// Un seul contrôle à la fois dans ce processus : lire, comparer, compter,
/// et, si le code est juste, changer le mot de passe.
static VERROU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Vérifie `saisi` contre le code en cours, sans le consommer s'il est juste.
/// La comparaison est celle d'Argon2 (`verify_password`), en temps constant
/// sur l'empreinte ; le code saisi n'est ni journalisé ni stocké. L'appelant
/// tient [`VERROU`].
fn verifier(settings: &SettingsRepo, saisi: &str) -> Verdict {
    let lire = |cle| settings.get(cle).ok().flatten().filter(|v| !v.is_empty());
    let (Some(empreinte), Some(emis)) = (lire(CLE_EMPREINTE), lire(CLE_EMIS)) else {
        return Verdict::AucunOuExpire;
    };
    let emis: i64 = emis.parse().unwrap_or(0);
    let age = maintenant() - emis;
    // Une horloge revenue en arrière de plus d'une minute ne rallonge rien.
    if age > DUREE_CODE.as_secs() as i64 || age < -60 {
        oublier_le_code(settings);
        return Verdict::AucunOuExpire;
    }
    let essais: u32 = lire(CLE_ESSAIS).and_then(|v| v.parse().ok()).unwrap_or(0);
    if essais >= ESSAIS_CODE {
        oublier_le_code(settings);
        return Verdict::Bloque;
    }
    let saisi = crate::auth::normaliser_code(saisi);
    if crate::auth::verify_password(&saisi, &empreinte) {
        return Verdict::Juste;
    }
    let essais = essais + 1;
    if essais >= ESSAIS_CODE {
        oublier_le_code(settings);
        return Verdict::Bloque;
    }
    settings.set(CLE_ESSAIS, &essais.to_string()).ok();
    Verdict::Faux {
        restants: ESSAIS_CODE - essais,
    }
}

/// Vérifie le code et, s'il est juste, pose le nouveau mot de passe du compte
/// `id`, consomme le code et révoque les sessions, en UNE transaction
/// ([`poser_le_mot_de_passe`]). `Ok(Verdict::Juste)` n'est rendu que si tout
/// cela est écrit ; une écriture refusée rend `Err` et le code reste valable.
pub fn reinitialiser_avec_code(
    backend: &Arc<dyn DbBackend>,
    id: i64,
    saisi: &str,
    mot_de_passe: &str,
) -> Result<Verdict, String> {
    let _verrou = VERROU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let settings = SettingsRepo::with_backend(backend.clone());
    let verdict = verifier(&settings, saisi);
    if verdict == Verdict::Juste {
        poser_le_mot_de_passe(backend, id, mot_de_passe)?;
    }
    Ok(verdict)
}

/// Le compte administrateur : le premier profil administrateur, le profil 1
/// sur une installation (`default`, ou `admin` après renommage).
pub fn compte_administrateur(backend: &Arc<dyn DbBackend>) -> Option<(i64, String)> {
    backend
        .query_one(
            "SELECT id, username FROM profiles WHERE is_admin = 1 ORDER BY id LIMIT 1",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| {
            Some((
                r.first().and_then(|v| v.as_i64())?,
                r.get(1).and_then(|v| v.as_string())?,
            ))
        })
}

/// Les réglages effacés avec le changement de mot de passe : la clé d'API,
/// les connexions SSO en attente (comme `renouveler_les_secrets` de
/// l'activation) et le code de réinitialisation. Le jeton du pont (écoute à
/// distance) n'en fait pas partie.
const SQL_EFFACER: &str = "DELETE FROM settings WHERE key IN \
     ('api_key', 'admin_reinit_code_empreinte', 'admin_reinit_code_emis', \
     'admin_reinit_code_essais') \
     OR key LIKE 'mozaik_pkce_pending%' OR key LIKE 'mozaik_redirect_uri_pending%'";

fn horodatage() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// Pose le mot de passe du compte `id` et révoque les sessions, en UNE
/// transaction : mot de passe, nouveau secret de signature, clé d'API et
/// code effacés. Tout ou rien : si une écriture échoue (base occupée au-delà
/// du délai d'attente), rien n'est changé et l'erreur remonte. Le secret est
/// relu après la validation : sans révocation constatée, pas de succès.
pub fn poser_le_mot_de_passe(
    backend: &Arc<dyn DbBackend>,
    id: i64,
    mot_de_passe: &str,
) -> Result<(), String> {
    use tune_core::db::backend::ToSqlValue;
    let hachage = crate::auth::hash_password(mot_de_passe)?;
    let secret = uuid::Uuid::new_v4().to_string();
    let maintenant = horodatage();
    backend.write_tx(&mut |tx| {
        let n = tx.execute(
            "UPDATE profiles SET password_hash_v2 = ?, password_hash = NULL WHERE id = ?",
            &[&hachage as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        )?;
        if n != 1 {
            return Err(format!("compte {id} introuvable"));
        }
        tx.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES ('jwt_secret', ?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
             updated_at = excluded.updated_at",
            &[&secret as &dyn ToSqlValue, &maintenant as &dyn ToSqlValue],
        )?;
        tx.execute(SQL_EFFACER, &[])?;
        Ok(())
    })?;
    let relu = SettingsRepo::with_backend(backend.clone()).get("jwt_secret")?;
    if relu.as_deref() != Some(secret.as_str()) {
        return Err("révocation des sessions non constatée".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// La sous-commande
// ---------------------------------------------------------------------------

/// Pourquoi la base n'a pas pu être ouverte : un message à lire tel quel.
#[derive(Debug)]
pub struct BaseIntrouvable(pub String);

/// Ouvre la base EXISTANTE du serveur, sans jamais en créer une : une base
/// neuve et vide ne contiendrait aucun administrateur à reprendre, et le
/// service ne la lirait pas.
pub async fn ouvrir_la_base(config: &TuneConfig) -> Result<Arc<dyn DbBackend>, BaseIntrouvable> {
    use tune_core::db::engine::Engine;
    let moteur = config
        .database_url
        .as_deref()
        .map(Engine::from_connection_string)
        .unwrap_or(Engine::Sqlite);
    let backend: Arc<dyn DbBackend> = match moteur {
        Engine::Sqlite => {
            let chemin = std::path::Path::new(&config.db_path);
            let absolu = std::path::absolute(chemin).unwrap_or_else(|_| chemin.to_path_buf());
            if !chemin.is_file() {
                return Err(BaseIntrouvable(format!(
                    "Aucune base Tune à {chemin}. Lancez cette commande sur la machine du \
                     serveur, sous le même compte et avec les mêmes TUNE_DATA_DIR / TUNE_DB_PATH \
                     que le service (voir le guide d'installation).\n\
                     No Tune database at {chemin}. Run this command on the server machine, as \
                     the same user and with the same TUNE_DATA_DIR / TUNE_DB_PATH as the service \
                     (see the installation guide).",
                    chemin = absolu.display()
                )));
            }
            #[cfg(unix)]
            if let Some(message) = refus_root_sur_la_base_d_un_autre(chemin) {
                return Err(BaseIntrouvable(message));
            }
            let db = tune_core::db::sqlite::SqliteDb::open(&config.db_path).map_err(|e| {
                BaseIntrouvable(format!(
                    "Base Tune illisible ({}) : {e}\nCannot open the Tune database.",
                    absolu.display()
                ))
            })?;
            eprintln!("Base / database : {}", absolu.display());
            Arc::new(db)
        }
        Engine::Postgres => ouvrir_postgres(config).await?,
    };
    // Une base d'une autre application, ou jamais migrée : le dire.
    if backend
        .query_one("SELECT COUNT(*) FROM profiles", &[])
        .is_err()
    {
        return Err(BaseIntrouvable(
            "Cette base n'a pas de table des comptes : ce n'est pas la base du serveur Tune.\n\
             This database has no accounts table: it is not the Tune server database."
                .into(),
        ));
    }
    Ok(backend)
}

/// Root sur la base d'un autre compte (paquet Debian : service sous `tune`) :
/// SQLite créerait les fichiers `-wal` / `-shm` au nom de root, et le service
/// ne pourrait plus ouvrir sa base. On refuse, et on dit sous quel compte
/// relancer.
#[cfg(unix)]
fn refus_root_sur_la_base_d_un_autre(chemin: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let proprietaire = std::fs::metadata(chemin).ok()?.uid();
    (proprietaire != 0).then(|| {
        format!(
            "La base appartient au compte d'UID {proprietaire}, pas à root : relancez sous ce \
             compte (par exemple « sudo -u tune … »), sinon le service ne pourrait plus \
             ouvrir sa base.\n\
             The database belongs to UID {proprietaire}, not root: run again as that user \
             (e.g. \"sudo -u tune …\"), otherwise the service could no longer open it."
        )
    })
}

#[cfg(feature = "postgres")]
async fn ouvrir_postgres(config: &TuneConfig) -> Result<Arc<dyn DbBackend>, BaseIntrouvable> {
    let url = config.database_url.as_deref().unwrap_or_default();
    let pg = tune_core::db::postgres::PostgresDb::connect(url)
        .await
        .map_err(|e| BaseIntrouvable(format!("PostgreSQL : {e}")))?;
    Ok(Arc::new(tune_core::db::backend::PostgresBackend::new(
        pg.pool().clone(),
    )))
}

#[cfg(not(feature = "postgres"))]
async fn ouvrir_postgres(_config: &TuneConfig) -> Result<Arc<dyn DbBackend>, BaseIntrouvable> {
    Err(BaseIntrouvable(
        "TUNE_DATABASE_URL désigne PostgreSQL, que ce binaire ne sait pas ouvrir.\n\
         TUNE_DATABASE_URL points to PostgreSQL, which this binary cannot open."
            .into(),
    ))
}

/// La ligne du code, telle qu'elle va sur la console et dans le journal. Elle
/// porte [`tune_core::confidentialite::MARQUEUR_CODE_ACTIVATION`] : tous les
/// lecteurs de journaux la retirent (API, rapports de bogue, site).
pub fn ligne_du_code(code: &str) -> String {
    format!(
        "{} {code} — Tune : code de réinitialisation du mot de passe administrateur, usage \
         unique, {} min. / One-time code to reset the administrator password.",
        tune_core::confidentialite::MARQUEUR_CODE_ACTIVATION,
        DUREE_CODE.as_secs() / 60,
    )
}

/// Ajoute la ligne au fichier journal local, `tune-server.log` : un service
/// Windows sans console, un `nohup`, gardent ainsi une trace lisible. Le
/// fichier doit déjà exister (le serveur l'a créé) : la commande lancée sous
/// un autre compte ne crée jamais, à son nom, le journal du service.
fn ecrire_dans_le_journal(ligne: &str) -> Option<std::path::PathBuf> {
    use std::io::Write;
    let candidats = std::iter::once(crate::config::default_log_file_path())
        .chain(crate::config::replis_du_journal_courants());
    for chemin in candidats {
        let ouvert = std::fs::OpenOptions::new().append(true).open(&chemin);
        if let Ok(mut f) = ouvert
            && writeln!(f, "{ligne}").is_ok()
        {
            return Some(chemin);
        }
    }
    None
}

/// Lance la sous-commande ; rend le code de sortie du processus.
pub async fn executer(config: &TuneConfig) -> i32 {
    let par_stdin = mode_stdin(std::env::args().skip(1));
    let backend = match ouvrir_la_base(config).await {
        Ok(b) => b,
        Err(BaseIntrouvable(message)) => {
            eprintln!("{message}");
            return 2;
        }
    };
    let Some((id, nom)) = compte_administrateur(&backend) else {
        eprintln!(
            "Aucun compte administrateur dans cette base. / No administrator account in this database."
        );
        return 3;
    };
    let settings = SettingsRepo::with_backend(backend.clone());

    if par_stdin {
        let mot_de_passe = match lire_le_mot_de_passe() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{e}");
                return 4;
            }
        };
        let mot_de_passe = mot_de_passe.as_str();
        if mot_de_passe.chars().count() < LONGUEUR_MIN_MOT_DE_PASSE {
            eprintln!(
                "Mot de passe trop court ({LONGUEUR_MIN_MOT_DE_PASSE} caractères au moins). / \
                 Password too short (at least {LONGUEUR_MIN_MOT_DE_PASSE} characters)."
            );
            return 4;
        }
        if let Err(e) = poser_le_mot_de_passe(&backend, id, mot_de_passe) {
            eprintln!("Échec / failed : {e}");
            return 1;
        }
        eprintln!(
            "Mot de passe du compte « {nom} » remplacé ; les sessions ouvertes sont révoquées \
             (le serveur en marche ferme ses WebSocket dans les secondes qui suivent).\n\
             Password of account \"{nom}\" replaced; existing sessions are revoked (the running \
             server closes its WebSockets within seconds)."
        );
        return 0;
    }

    let code = match emettre_un_code(&settings) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Échec / failed : {e}");
            return 1;
        }
    };
    let ligne = ligne_du_code(&code);
    eprintln!("{ligne}");
    match ecrire_dans_le_journal(&ligne) {
        Some(chemin) => eprintln!("(copié dans / copied to {})", chemin.display()),
        None => eprintln!("(journal local non inscriptible / local log not writable)"),
    }
    let minutes = DUREE_CODE.as_secs() / 60;
    eprintln!(
        "\nCompte administrateur : « {nom} ». Sur CETTE machine, ouvrez \
         http://localhost:{port}/ puis « Mot de passe oublié ? », et saisissez le code et le \
         nouveau mot de passe. Valable {minutes} min, {ESSAIS_CODE} essais.\n\
         Sans navigateur sur la machine (Docker, Tune OS, serveur sans écran) : relancez avec \
         {DRAPEAU} {DRAPEAU_STDIN} et tapez le nouveau mot de passe.\n\n\
         Administrator account: \"{nom}\". On THIS machine, open http://localhost:{port}/, \
         choose \"Forgot password?\" and enter the code and a new password. Valid {minutes} \
         min, {ESSAIS_CODE} attempts.\n\
         No browser on the machine (Docker, Tune OS, headless server): run again with \
         {DRAPEAU_EN} {DRAPEAU_STDIN_EN} and type the new password.",
        port = config.port,
    );
    tracing::info!(admin = %nom, "reinitialisation_admin_code_emis");
    0
}

// ---------------------------------------------------------------------------
// Lecture du mot de passe sur l'entrée standard
// ---------------------------------------------------------------------------

/// Le nouveau mot de passe. Dans un terminal : saisie masquée, demandée deux
/// fois. Sinon (tube, `docker exec -i`) : la première ligne.
fn lire_le_mot_de_passe() -> Result<String, String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        let mut ligne = String::new();
        std::io::stdin()
            .read_line(&mut ligne)
            .map_err(|_| "Entrée standard illisible. / Cannot read standard input.".to_string())?;
        return Ok(ligne.trim_end_matches(['\r', '\n']).to_string());
    }
    let premier = lire_masque("Nouveau mot de passe / New password : ")?;
    let second = lire_masque("Confirmez / Confirm : ")?;
    if premier != second {
        return Err("Les deux saisies diffèrent. / The two entries differ.".into());
    }
    Ok(premier)
}

fn lire_masque(invite: &str) -> Result<String, String> {
    use std::io::Write;
    eprint!("{invite}");
    std::io::stderr().flush().ok();
    let echo = EchoCoupe::couper()?;
    let mut ligne = String::new();
    let lu = std::io::stdin().read_line(&mut ligne);
    drop(echo);
    eprintln!();
    lu.map_err(|_| "Entrée standard illisible. / Cannot read standard input.".to_string())?;
    Ok(ligne.trim_end_matches(['\r', '\n']).to_string())
}

const MASQUAGE_IMPOSSIBLE: &str =
    "Impossible de masquer la saisie dans ce terminal. / Cannot hide input in this terminal.";

/// L'écho du terminal coupé tant que la valeur vit ; rétabli à sa
/// destruction, erreur comprise.
#[cfg(unix)]
struct EchoCoupe(libc::termios);

#[cfg(unix)]
impl EchoCoupe {
    fn couper() -> Result<Self, String> {
        // SAFETY : `termios` est une structure C simple, remplie par
        // `tcgetattr` avant tout usage ; descripteur 0 = entrée standard.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Err(MASQUAGE_IMPOSSIBLE.into());
            }
            let ancien = t;
            t.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return Err(MASQUAGE_IMPOSSIBLE.into());
            }
            Ok(Self(ancien))
        }
    }
}

#[cfg(unix)]
impl Drop for EchoCoupe {
    fn drop(&mut self) {
        // SAFETY : réglages lus par `tcgetattr` sur le même descripteur.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.0);
        }
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(n_std_handle: u32) -> *mut core::ffi::c_void;
    fn GetConsoleMode(h_console: *mut core::ffi::c_void, lp_mode: *mut u32) -> i32;
    fn SetConsoleMode(h_console: *mut core::ffi::c_void, dw_mode: u32) -> i32;
}

#[cfg(windows)]
struct EchoCoupe(*mut core::ffi::c_void, u32);

#[cfg(windows)]
impl EchoCoupe {
    fn couper() -> Result<Self, String> {
        const STD_INPUT_HANDLE: u32 = -10i32 as u32;
        const ENABLE_ECHO_INPUT: u32 = 0x0004;
        // SAFETY : poignée de la console d'entrée du processus ; le mode est
        // lu avant d'être modifié.
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0u32;
            if h.is_null() || GetConsoleMode(h, &mut mode) == 0 {
                return Err(MASQUAGE_IMPOSSIBLE.into());
            }
            if SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT) == 0 {
                return Err(MASQUAGE_IMPOSSIBLE.into());
            }
            Ok(Self(h, mode))
        }
    }
}

#[cfg(windows)]
impl Drop for EchoCoupe {
    fn drop(&mut self) {
        // SAFETY : même poignée, mode d'origine.
        unsafe {
            SetConsoleMode(self.0, self.1);
        }
    }
}

#[cfg(not(any(unix, windows)))]
struct EchoCoupe;

#[cfg(not(any(unix, windows)))]
impl EchoCoupe {
    fn couper() -> Result<Self, String> {
        Err(MASQUAGE_IMPOSSIBLE.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn drapeaux_exacts_seulement() {
        assert!(demandee(args(&["--reinitialiser-admin"])));
        assert!(demandee(args(&["--reset-admin"])));
        assert!(!demandee(args(&["--reset-admin-now"])));
        assert!(!demandee(args(&["--reset"])));
        assert!(mode_stdin(args(&["--reset-admin", "--password-stdin"])));
        assert!(mode_stdin(args(&["--mot-de-passe-stdin"])));
        assert!(!mode_stdin(args(&["--reset-admin"])));
    }

    #[test]
    fn la_ligne_porte_le_marqueur_retire_des_journaux() {
        let ligne = ligne_du_code("ABCDEFGHJK");
        assert!(ligne.contains(tune_core::confidentialite::MARQUEUR_CODE_ACTIVATION));
        assert_eq!(
            tune_core::confidentialite::retirer_les_codes_d_activation(&format!("{ligne}\n")),
            ""
        );
    }
}
