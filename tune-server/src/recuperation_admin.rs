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
    /// Juste : le code est consommé.
    Juste,
    /// Faux ; il reste `restants` essais sur ce code.
    Faux { restants: u32 },
    /// Aucun code en cours, ou code expiré (alors effacé).
    AucunOuExpire,
    /// Trop d'essais faux : le code est détruit.
    Bloque,
}

/// Un seul contrôle à la fois dans ce processus : lire, comparer, compter.
static VERROU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Vérifie `saisi` contre le code en cours. La comparaison est celle
/// d'Argon2 (`verify_password`), en temps constant sur l'empreinte ; le code
/// saisi n'est ni journalisé ni stocké.
pub fn verifier_et_consommer(settings: &SettingsRepo, saisi: &str) -> Verdict {
    let _verrou = VERROU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        oublier_le_code(settings);
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

/// Pose le mot de passe du compte `id` et révoque les sessions : nouveau
/// secret de signature, clé d'API effacée (même logique que l'activation).
pub fn poser_le_mot_de_passe(
    backend: &Arc<dyn DbBackend>,
    id: i64,
    mot_de_passe: &str,
) -> Result<(), String> {
    use tune_core::db::backend::ToSqlValue;
    let hachage = crate::auth::hash_password(mot_de_passe)?;
    backend.execute(
        "UPDATE profiles SET password_hash_v2 = ?, password_hash = NULL WHERE id = ?",
        &[&hachage as &dyn ToSqlValue, &id as &dyn ToSqlValue],
    )?;
    let settings = SettingsRepo::with_backend(backend.clone());
    crate::auth::renouveler_les_secrets(&settings);
    oublier_le_code(&settings);
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
            "La base appartient au compte d'UID {proprietaire}, pas à root : relancez sous ce              compte (par exemple « sudo -u tune … »), sinon le service ne pourrait plus ouvrir              sa base.
The database belongs to UID {proprietaire}, not root: run again as that              user (e.g. \"sudo -u tune …\"), otherwise the service could no longer open it."
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
        let mut mot_de_passe = String::new();
        if std::io::stdin().read_line(&mut mot_de_passe).is_err() {
            eprintln!("Entrée standard illisible. / Cannot read standard input.");
            return 4;
        }
        let mot_de_passe = mot_de_passe.trim_end_matches(['\r', '\n']);
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
            "Mot de passe du compte « {nom} » remplacé ; les sessions ouvertes sont révoquées.\n\
             Password of account \"{nom}\" replaced; existing sessions are revoked."
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
