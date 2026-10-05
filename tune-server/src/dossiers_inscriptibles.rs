//! Les dossiers de données sont-ils inscriptibles ? Sinon, le dire et sortir.
//!
//! L'image `renesenses/tune` tourne sous le compte `tune` (uid/gid 1000). Un
//! dossier de l'hôte monté sur `/data` appartient le plus souvent à `root`
//! (Synology Container Manager, unRAID, CasaOS le créent ainsi) : la base ne
//! s'ouvrait pas, `AppState::new(...).expect(...)` paniquait, le conteneur
//! redémarrait, paniquait encore. Container Manager n'affichait que « arrêté
//! de manière inattendue », et rien ne désignait le dossier ni le remède.
//!
//! Ce contrôle passe AVANT l'ouverture de la base. Quand un dossier refuse
//! l'écriture, il rend un seul rapport — uid/gid du processus, propriétaire du
//! dossier, commande exacte — puis le serveur sort en `78` (`EX_CONFIG` de
//! `sysexits.h`) : une erreur de configuration, pas un plantage. Quand tout va
//! bien, il ne laisse aucune trace : le fichier sonde est retiré.
//!
//! La logique est pure autant que possible (chemins, rapport) ; seule la sonde
//! touche le disque.

use std::path::{Path, PathBuf};

/// Code de sortie d'une erreur de configuration (`EX_CONFIG`, `sysexits.h`).
pub const EX_CONFIG: i32 = 78;

/// Un dossier que le serveur doit pouvoir écrire, et pourquoi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DossierRequis {
    /// D'où vient l'exigence : `TUNE_DATA_DIR` ou `TUNE_DB_PATH`.
    pub origine: &'static str,
    pub dossier: PathBuf,
}

/// Un dossier qui a refusé l'écriture.
#[derive(Debug)]
pub struct Refus {
    pub requis: DossierRequis,
    pub erreur: std::io::Error,
    /// `(uid, gid)` du dossier, ou de son plus proche parent existant.
    pub proprietaire: Option<(u32, u32)>,
}

/// Les dossiers à sonder, sans doublon.
///
/// - `TUNE_DATA_DIR` quand il est posé et non vide (relatif : sous `cwd`) ;
/// - le dossier de la base SQLite (`db_path`, relatif : sous `cwd`), sauf
///   quand la base est PostgreSQL — aucun fichier n'est alors ouvert.
pub fn dossiers_requis(
    tune_data_dir: Option<&str>,
    db_path: &str,
    base_postgres: bool,
    cwd: &Path,
) -> Vec<DossierRequis> {
    let absolu = |p: &Path| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    let mut requis = Vec::new();
    if let Some(d) = tune_data_dir.filter(|d| !d.trim().is_empty()) {
        requis.push(DossierRequis {
            origine: "TUNE_DATA_DIR",
            dossier: absolu(Path::new(d)),
        });
    }
    if !base_postgres && !db_path.trim().is_empty() {
        let base = absolu(Path::new(db_path));
        let dossier = match base.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => cwd.to_path_buf(),
        };
        if !requis.iter().any(|r| r.dossier == dossier) {
            requis.push(DossierRequis {
                origine: "TUNE_DB_PATH",
                dossier,
            });
        }
    }
    requis
}

/// Sonde l'écriture : crée le dossier au besoin (comme le ferait le serveur),
/// y écrit un fichier au nom propre à ce processus, puis le retire.
pub fn sonder(dossier: &Path) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dossier)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let sonde = dossier.join(format!(
        ".tune-sonde-ecriture-{}-{nanos}",
        std::process::id()
    ));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sonde)?;
    let ecrit = f.write_all(b"tune").and_then(|()| f.sync_all());
    drop(f);
    let retire = std::fs::remove_file(&sonde);
    ecrit?;
    retire
}

/// Le propriétaire `(uid, gid)` du dossier, ou de son plus proche parent
/// existant quand il n'existe pas encore.
pub fn proprietaire(dossier: &Path) -> Option<(u32, u32)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        dossier
            .ancestors()
            .find_map(|p| std::fs::metadata(p).ok())
            .map(|m| (m.uid(), m.gid()))
    }
    #[cfg(not(unix))]
    {
        let _ = dossier;
        None
    }
}

/// Sonde chaque dossier requis ; rend ceux qui refusent l'écriture.
pub fn verifier(requis: &[DossierRequis]) -> Vec<Refus> {
    requis
        .iter()
        .filter_map(|r| {
            sonder(&r.dossier).err().map(|erreur| Refus {
                requis: r.clone(),
                proprietaire: proprietaire(&r.dossier),
                erreur,
            })
        })
        .collect()
}

/// uid/gid EFFECTIFS du processus — ceux que le noyau confronte aux droits.
pub fn identite_du_processus() -> Option<(u32, u32)> {
    #[cfg(unix)]
    {
        // `geteuid`/`getegid` ne peuvent pas échouer.
        Some(unsafe { (libc::geteuid() as u32, libc::getegid() as u32) })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn systeme_en_lecture_seule(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EROFS)
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

/// Le rapport, tel qu'il part sur stderr et dans le journal : UN seul texte,
/// tous les dossiers fautifs, et pour chacun la commande qui répare.
///
/// `conteneur` : le processus tourne dans un conteneur (`/.dockerenv`) — le
/// remède se joue alors sur l'HÔTE, dont on ne connaît pas le chemin.
pub fn rapport(refus: &[Refus], processus: Option<(u32, u32)>, conteneur: bool) -> String {
    let (uid, gid) = processus.unwrap_or((1000, 1000));
    let mut t = String::new();
    t.push_str("FATAL: Tune cannot write to its data folder, so it cannot open its database.\n");
    match processus {
        Some((u, g)) => t.push_str(&format!("  This process runs as uid={u} gid={g}.\n")),
        None => t.push_str("  This process cannot write there.\n"),
    }
    for r in refus {
        let d = r.requis.dossier.display();
        t.push_str(&format!(
            "  - {d} (from {}): {}\n",
            r.requis.origine, r.erreur
        ));
        match r.proprietaire {
            Some((ou, og)) => {
                t.push_str(&format!("    owned by uid={ou} gid={og}.\n"));
            }
            None => t.push_str("    owner unknown.\n"),
        }
        if systeme_en_lecture_seule(&r.erreur) {
            t.push_str("    It is mounted READ-ONLY: mount it read-write (drop `:ro`).\n");
        } else if processus.is_some()
            && r.proprietaire.map(|(ou, _)| ou) == processus.map(|(u, _)| u)
        {
            // Le compte est déjà propriétaire : c'est le mode, pas le chown.
            t.push_str(&format!(
                "    It already belongs to this account but is not writable: chmod u+rwx {d}\n"
            ));
        }
    }
    t.push_str("How to fix:\n");
    if conteneur {
        let cible = refus
            .first()
            .map(|r| r.requis.dossier.display().to_string())
            .unwrap_or_else(|| "/data".into());
        t.push_str(&format!(
            "  1. On the HOST, give the folder mounted on {cible} to uid {uid}:\n\
             \x20       sudo chown -R {uid}:{gid} <host folder mounted on {cible}>\n\
             \x20  2. Or use a named volume instead of a host folder:\n\
             \x20       docker run -v tune-data:{cible} ...\n\
             \x20  3. Or let the image adopt the folder's owner (as linuxserver images do):\n\
             \x20       docker run -e PUID=<owner uid> -e PGID=<owner gid> ...\n"
        ));
    } else {
        for r in refus {
            t.push_str(&format!(
                "  sudo chown -R {uid}:{gid} {}\n",
                r.requis.dossier.display()
            ));
        }
        t.push_str("  or point TUNE_DATA_DIR / TUNE_DB_PATH to a folder this account can write.\n");
    }
    t.push_str(&format!(
        "Tune stops now (exit code {EX_CONFIG}, configuration error)."
    ));
    t
}

/// Le contrôle du démarrage : rend la main quand tout est inscriptible, sinon
/// écrit le rapport sur stderr et dans le journal, puis sort en `EX_CONFIG`.
pub fn verifier_ou_sortir(config: &crate::config::TuneConfig) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let base_postgres = config
        .database_url
        .as_deref()
        .map(tune_core::db::engine::Engine::from_connection_string)
        == Some(tune_core::db::engine::Engine::Postgres);
    let requis = dossiers_requis(
        std::env::var("TUNE_DATA_DIR").ok().as_deref(),
        &config.db_path,
        base_postgres,
        &cwd,
    );
    let refus = verifier(&requis);
    if refus.is_empty() {
        return;
    }
    let texte = rapport(
        &refus,
        identite_du_processus(),
        Path::new("/.dockerenv").exists(),
    );
    eprintln!("{texte}");
    tracing::error!(
        dossiers = ?refus.iter().map(|r| r.requis.dossier.display().to_string()).collect::<Vec<_>>(),
        "dossier_de_donnees_non_inscriptible\n{texte}"
    );
    std::process::exit(EX_CONFIG);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requis_de(v: &[DossierRequis]) -> Vec<(&'static str, PathBuf)> {
        v.iter().map(|r| (r.origine, r.dossier.clone())).collect()
    }

    #[test]
    fn image_docker_sonde_data() {
        // Dockerfile.dist : TUNE_DB_PATH=/data/tune.db, TUNE_DATA_DIR absent.
        let r = dossiers_requis(None, "/data/tune.db", false, Path::new("/app"));
        assert_eq!(
            requis_de(&r),
            vec![("TUNE_DB_PATH", PathBuf::from("/data"))]
        );
    }

    #[test]
    fn data_dir_et_dossier_de_la_base_sans_doublon() {
        let r = dossiers_requis(Some("/data"), "/data/tune.db", false, Path::new("/"));
        assert_eq!(
            requis_de(&r),
            vec![("TUNE_DATA_DIR", PathBuf::from("/data"))]
        );
        let r = dossiers_requis(
            Some("/var/lib/tune"),
            "/srv/db/tune.db",
            false,
            Path::new("/"),
        );
        assert_eq!(
            requis_de(&r),
            vec![
                ("TUNE_DATA_DIR", PathBuf::from("/var/lib/tune")),
                ("TUNE_DB_PATH", PathBuf::from("/srv/db")),
            ]
        );
    }

    #[test]
    fn chemins_relatifs_sous_le_repertoire_courant() {
        let r = dossiers_requis(Some("  "), "tune.db", false, Path::new("/opt/tune"));
        assert_eq!(
            requis_de(&r),
            vec![("TUNE_DB_PATH", PathBuf::from("/opt/tune"))]
        );
        let r = dossiers_requis(Some("donnees"), "sous/tune.db", false, Path::new("/opt"));
        assert_eq!(
            requis_de(&r),
            vec![
                ("TUNE_DATA_DIR", PathBuf::from("/opt/donnees")),
                ("TUNE_DB_PATH", PathBuf::from("/opt/sous")),
            ]
        );
    }

    #[test]
    fn postgres_ne_sonde_pas_le_dossier_de_la_base() {
        assert!(dossiers_requis(None, "/data/tune.db", true, Path::new("/")).is_empty());
    }

    #[test]
    fn dossier_inscriptible_passe_sans_laisser_de_trace() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("a/b");
        sonder(&d).expect("dossier inscriptible");
        assert!(
            d.is_dir(),
            "le dossier manquant est créé, comme par le serveur"
        );
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0, "sonde retirée");
        let requis = dossiers_requis(None, d.join("tune.db").to_str().unwrap(), false, t.path());
        assert!(verifier(&requis).is_empty());
    }

    /// Le cas de l'image : `/data` monté depuis un dossier de l'hôte qui
    /// n'appartient pas au compte. Sous `root`, les droits ne s'appliquent
    /// pas : le test se retire plutôt que de rendre un faux rouge.
    #[cfg(unix)]
    #[test]
    fn dossier_non_inscriptible_est_rapporte_avec_proprietaire_et_remede() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("ignoré sous root : les droits ne s'y appliquent pas");
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o555)).unwrap();

        let requis = dossiers_requis(
            None,
            data.join("tune.db").to_str().unwrap(),
            false,
            t.path(),
        );
        let refus = verifier(&requis);
        // Rendre le dossier supprimable avant toute assertion.
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(refus.len(), 1, "{refus:?}");
        assert_eq!(refus[0].erreur.kind(), std::io::ErrorKind::PermissionDenied);
        let moi = identite_du_processus().unwrap();
        assert_eq!(refus[0].proprietaire, Some(moi), "propriétaire relevé");

        let texte = rapport(&refus, Some(moi), false);
        assert!(
            texte.contains(&format!("chmod u+rwx {}", data.display())),
            "{texte}"
        );

        let texte = rapport(&refus, Some((1000, 1000)), true);
        assert!(texte.contains("uid=1000 gid=1000"), "{texte}");
        assert!(
            texte.contains(&format!("owned by uid={} gid={}", moi.0, moi.1)),
            "{texte}"
        );
        assert!(texte.contains(&data.display().to_string()), "{texte}");
        assert!(
            texte.contains("sudo chown -R 1000:1000 <host folder mounted on"),
            "{texte}"
        );
        assert!(texte.contains("-v tune-data:"), "{texte}");
        assert!(texte.contains("PUID="), "{texte}");
        assert!(texte.contains("exit code 78"), "{texte}");
        assert!(
            !texte.contains("READ-ONLY"),
            "EACCES n'est pas EROFS : {texte}"
        );
    }

    #[test]
    fn rapport_hors_conteneur_donne_le_chemin_exact() {
        let refus = vec![Refus {
            requis: DossierRequis {
                origine: "TUNE_DATA_DIR",
                dossier: PathBuf::from("/var/lib/tune"),
            },
            erreur: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            proprietaire: Some((0, 0)),
        }];
        let texte = rapport(&refus, Some((998, 997)), false);
        assert!(
            texte.contains("sudo chown -R 998:997 /var/lib/tune\n"),
            "{texte}"
        );
        assert!(texte.contains("owned by uid=0 gid=0"), "{texte}");
        assert!(!texte.contains("<host folder"), "{texte}");
        assert!(
            !texte.contains("chmod"),
            "root propriétaire : chown, pas chmod : {texte}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn systeme_en_lecture_seule_est_nomme() {
        let refus = vec![Refus {
            requis: DossierRequis {
                origine: "TUNE_DB_PATH",
                dossier: PathBuf::from("/data"),
            },
            erreur: std::io::Error::from_raw_os_error(libc::EROFS),
            proprietaire: Some((1000, 1000)),
        }];
        assert!(rapport(&refus, Some((1000, 1000)), true).contains("READ-ONLY"));
    }

    /// Le contrôle est bien branché, et AVANT l'ouverture de la base : un
    /// module écrit mais jamais appelé ne protégerait personne.
    #[test]
    fn branche_avant_l_ouverture_de_la_base() {
        let src = include_str!("bootstrap.rs");
        let controle = src
            .find("crate::dossiers_inscriptibles::verifier_ou_sortir(&config)")
            .expect("contrôle absent de bootstrap.rs");
        let base = src
            .find("AppState::new(&config.db_path")
            .expect("ouverture de la base introuvable");
        assert!(controle < base, "le contrôle doit précéder AppState::new");
    }
}
