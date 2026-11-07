//! #7756 — l'installation Unix ne remplace plus l'ancien binaire par un
//! fichier qu'elle n'a pas vérifié.
//!
//! Sur Tune OS, après la mise à jour rc9 → rc10, `/opt/tune/tune-server`
//! faisait 0 octet (`.old` intact, 207 Go libres) et systemd bouclait sur
//! « Exec format error ». `install_unix` copiait le neuf par `fs::copy`
//! (aucun `fsync`), libérait le nom en parquant l'ancien en `.old`, puis
//! renommait le `.new` dont les données n'étaient peut-être pas encore sur le
//! disque : après une coupure, ext4 peut laisser un fichier vide sous le nom
//! d'installation. Rien ne relisait le fichier avant de basculer.
//!
//! Ce module porte ce qui manquait, dans l'ordre où `install_unix` l'emploie :
//! la place disque vérifiée avant toute écriture, l'écriture durable du
//! `.new`, sa validation (taille, en-tête ELF, architecture, `--version`)
//! AVANT de toucher à l'ancien binaire, et la synchronisation du dossier
//! après les renommages.
//!
//! `web/` avait le même défaut (constat du 03/11) : `index.html`, `sw.js`,
//! `manifest.json`, `version.json`… laissés à 0 octet, et le `web/` de la rc10
//! posé à côté du binaire rc9 remis en place. La copie (`fs::copy`) n'était
//! pas forcée sur le disque, rien ne la relisait avant la bascule, l'ancien
//! `web/` était supprimé aussitôt, et `web/` n'était copié qu'APRÈS la bascule
//! du binaire. Désormais :
//!
//! 1. chaque `web/` neuf est préparé en `<cible>.new`, à côté de sa cible
//!    (même système de fichiers), fichier par fichier forcé sur le disque,
//!    puis vérifié ([`verifier_le_web`]) — AVANT de toucher au binaire ;
//! 2. la bascule renomme le binaire, puis chaque dossier entier ; l'ancien
//!    `web/` est gardé en `<cible>.old`, à côté de `tune-server.old` ;
//! 3. un échec pendant la bascule remet le binaire ET `web/` ensemble.
//!
//! Retour manuel à la version précédente (installation dans `/opt/tune`,
//! service arrêté) :
//!
//! ```sh
//! cd /opt/tune
//! cp -a tune-server.old tune-server
//! rm -rf web.rejete && mv web web.rejete && mv web.old web
//! ```

use std::io;
use std::path::{Path, PathBuf};

use tracing::warn;

/// Marge gardée en plus de ce qu'on écrit : un disque rempli à l'octet près
/// laisse le serveur (base, journaux) sans place au redémarrage.
pub(super) const MARGE_DE_PLACE: u64 = 64 * 1024 * 1024;

/// Délai laissé au neuf pour répondre à `--version`.
const DELAI_ESSAI_VERSION: std::time::Duration = std::time::Duration::from_secs(30);

/// Ce que `install_unix` vérifie avant de basculer. Les mesures sont des
/// fonctions pour que les tests simulent un disque plein ou se passent
/// d'exécuter un faux binaire.
#[derive(Clone, Copy)]
pub(super) struct Controles {
    /// Taille du binaire annoncée par l'archive ; `None` si inconnue.
    pub taille_attendue: Option<u64>,
    /// Place libre (octets) sur le système de fichiers d'un dossier.
    pub place_libre: fn(&Path) -> io::Result<u64>,
    /// Architecture attendue (`std::env::consts::ARCH`) ; `None` : pas de
    /// contrôle ELF (macOS, dont les binaires ne sont pas des ELF).
    pub architecture: Option<&'static str>,
    /// Lance le binaire posé et vérifie qu'il répond.
    pub essai: fn(&Path) -> Result<(), String>,
    /// Les dossiers `web/` à remplacer, le servi en premier ; en production
    /// `cibles_web`.
    pub cibles_web: fn(&Path) -> Vec<PathBuf>,
    /// Copie un fichier de `web/` et le force sur le disque ; en production
    /// [`copier_durablement`]. Les tests y simulent une coupure.
    pub copier: fn(&Path, &Path) -> io::Result<()>,
    /// Renomme pendant la bascule (binaire et `web/`) ; en production
    /// `fs::rename`. Le retour en arrière n'y passe pas.
    pub renommer: fn(&Path, &Path) -> io::Result<()>,
}

impl Controles {
    /// Les contrôles de production, pour la machine qui tourne.
    pub(super) fn de_cette_machine(taille_attendue: Option<u64>) -> Self {
        let linux = cfg!(target_os = "linux");
        Self {
            taille_attendue,
            place_libre,
            architecture: linux.then_some(std::env::consts::ARCH),
            essai: if linux { essayer_version } else { sans_essai },
            cibles_web: super::cibles_web,
            copier: copier_durablement,
            renommer: |de, vers| std::fs::rename(de, vers),
        }
    }
}

fn sans_essai(_: &Path) -> Result<(), String> {
    Ok(())
}

/// Place disponible pour un utilisateur non privilégié, en octets.
#[cfg(unix)]
pub(super) fn place_libre(dir: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` est une chaîne C valide et `st` une zone de la bonne taille.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(not(unix))]
pub(super) fn place_libre(_dir: &Path) -> io::Result<u64> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "statvfs"))
}

fn mo(octets: u64) -> u64 {
    octets.div_ceil(1024 * 1024)
}

/// Refuse si `dir` n'a pas `besoin` octets libres. Une mesure impossible
/// (système de fichiers exotique) n'arrête pas la mise à jour : la
/// validation du binaire écrit reste le dernier rempart.
pub(super) fn verifier_la_place(
    dir: &Path,
    besoin: u64,
    mesure: fn(&Path) -> io::Result<u64>,
) -> Result<(), String> {
    match mesure(dir) {
        Ok(libre) if libre < besoin => Err(format!(
            "Not enough disk space in {}: {} MB needed, {} MB free",
            dir.display(),
            mo(besoin),
            libre / (1024 * 1024)
        )),
        Ok(_) => Ok(()),
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "update_disk_space_unknown");
            Ok(())
        }
    }
}

/// Ce que l'archive va écrire, lu dans ses en-têtes avant l'extraction.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ContenuArchive {
    /// Taille du binaire `name` à la racine de l'archive.
    pub binaire: Option<u64>,
    /// Somme des tailles des fichiers.
    pub totale: u64,
}

/// Lit les tailles annoncées par l'archive, sans rien écrire.
pub(super) fn contenu_de_l_archive(
    data: &[u8],
    is_zip: bool,
    name: &str,
) -> Result<ContenuArchive, String> {
    let a_la_racine = |p: &Path| {
        let mut parts = p
            .components()
            .filter(|c| matches!(c, std::path::Component::Normal(_)));
        parts.next().is_some_and(|c| c.as_os_str() == name) && parts.next().is_none()
    };
    let mut contenu = ContenuArchive::default();
    if is_zip {
        let mut archive =
            zip::ZipArchive::new(io::Cursor::new(data)).map_err(|e| format!("zip open: {e}"))?;
        for i in 0..archive.len() {
            let f = archive
                .by_index_raw(i)
                .map_err(|e| format!("zip entry {i}: {e}"))?;
            if f.is_dir() {
                continue;
            }
            contenu.totale = contenu.totale.saturating_add(f.size());
            if f.enclosed_name().is_some_and(|p| a_la_racine(&p)) {
                contenu.binaire = Some(f.size());
            }
        }
    } else {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(data));
        for entry in archive.entries().map_err(|e| format!("tar read: {e}"))? {
            let entry = entry.map_err(|e| format!("tar entry: {e}"))?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let taille = entry.size();
            contenu.totale = contenu.totale.saturating_add(taille);
            if entry.path().is_ok_and(|p| a_la_racine(&p)) {
                contenu.binaire = Some(taille);
            }
        }
    }
    Ok(contenu)
}

/// Copie `src` vers `dst` et force les données sur le disque avant de
/// rendre la main. `fs::copy` laissait tout dans le cache : renommé ensuite,
/// le fichier pouvait se retrouver vide après une coupure (#7756).
pub(super) fn ecrire_durablement(src: &Path, dst: &Path) -> Result<u64, String> {
    let ecrire = || -> io::Result<u64> {
        let mut entree = std::fs::File::open(src)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o755);
        }
        let mut sortie = options.open(dst)?;
        let copies = io::copy(&mut entree, &mut sortie)?;
        sortie.sync_all()?;
        Ok(copies)
    };
    let resultat = ecrire().map_err(|e| format!("write new binary {}: {e}", dst.display()));
    if resultat.is_err() {
        std::fs::remove_file(dst).ok();
    }
    resultat
}

/// Code `e_machine` ELF de l'architecture Rust, quand on le connaît.
fn code_elf(arch: &str) -> Option<u16> {
    match arch {
        "x86_64" => Some(0x3E),
        "aarch64" => Some(0xB7),
        "arm" => Some(0x28),
        "x86" => Some(0x03),
        "riscv64" => Some(0xF3),
        _ => None,
    }
}

fn arch_de_code(code: u16) -> String {
    match code {
        0x3E => "x86_64".into(),
        0xB7 => "aarch64".into(),
        0x28 => "arm".into(),
        0x03 => "x86".into(),
        0xF3 => "riscv64".into(),
        c => format!("ELF machine 0x{c:X}"),
    }
}

/// Vérifie le binaire posé en `.new` AVANT tout renommage. Chaque refus dit
/// sa cause : c'est elle qui finit dans la phase `failed: …`.
pub(super) fn valider_binaire(chemin: &Path, controles: &Controles) -> Result<(), String> {
    use std::io::Read;
    let taille = std::fs::metadata(chemin)
        .map_err(|e| format!("invalid binary: cannot read {}: {e}", chemin.display()))?
        .len();
    if taille == 0 {
        return Err("invalid binary: file is empty (0 bytes)".into());
    }
    if let Some(attendue) = controles.taille_attendue
        && taille != attendue
    {
        return Err(format!(
            "invalid binary: {taille} bytes written, {attendue} expected (truncated write?)"
        ));
    }
    if let Some(arch) = controles.architecture {
        let mut entete = [0u8; 20];
        let lus = std::fs::File::open(chemin)
            .and_then(|mut f| {
                let mut n = 0;
                while n < entete.len() {
                    match f.read(&mut entete[n..])? {
                        0 => break,
                        k => n += k,
                    }
                }
                Ok(n)
            })
            .map_err(|e| format!("invalid binary: cannot read header: {e}"))?;
        if lus < entete.len() || &entete[..4] != b"\x7fELF" {
            return Err("invalid binary: not an ELF executable".into());
        }
        let machine = match entete[5] {
            2 => u16::from_be_bytes([entete[18], entete[19]]),
            _ => u16::from_le_bytes([entete[18], entete[19]]),
        };
        if let Some(attendu) = code_elf(arch)
            && machine != attendu
        {
            return Err(format!(
                "invalid binary: built for {} but this machine is {arch}",
                arch_de_code(machine)
            ));
        }
    }
    (controles.essai)(chemin)
}

/// Lance `<binaire> --version` : le serveur y répond et sort avant tout
/// effet de bord (bootstrap). Un binaire qui ne s'exécute pas, ou ne
/// répond pas dans le délai, est refusé.
fn essayer_version(chemin: &Path) -> Result<(), String> {
    use std::io::Read;
    let mut enfant = lancer_quand_libre(chemin)
        .map_err(|e| format!("invalid binary: cannot run `--version`: {e}"))?;
    let debut = std::time::Instant::now();
    let statut = loop {
        match enfant.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if debut.elapsed() < DELAI_ESSAI_VERSION => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Ok(None) => {
                enfant.kill().ok();
                enfant.wait().ok();
                return Err("invalid binary: `--version` did not answer in time".into());
            }
            Err(e) => return Err(format!("invalid binary: `--version`: {e}")),
        }
    };
    let mut sortie = String::new();
    if let Some(mut out) = enfant.stdout.take() {
        out.read_to_string(&mut sortie).ok();
    }
    if !statut.success() || !sortie.starts_with("tune-server") {
        return Err(format!(
            "invalid binary: `--version` failed ({statut}, output {:?})",
            sortie.trim()
        ));
    }
    Ok(())
}

/// Délai pendant lequel un `--version` refusé par `ETXTBSY` est relancé.
const DELAI_FICHIER_OCCUPE: std::time::Duration = std::time::Duration::from_secs(5);

/// Lance `<binaire> --version`, en relançant tant que le système répond
/// `ETXTBSY` (« Text file busy »).
///
/// Un `fork` d'un autre fil du serveur (ou d'un test voisin) pendant
/// `ecrire_durablement` hérite du descripteur ouvert en écriture sur le
/// `.new`, jusqu'à son propre `exec`. Pendant cette fenêtre, `exec` du `.new`
/// échoue par `ETXTBSY` : le binaire est bon, il n'est simplement pas encore
/// libre. Sans relance, un binaire valide était refusé (CI de #7755,
/// « Text file busy (os error 26) — previous binary kept »).
fn lancer_quand_libre(chemin: &Path) -> io::Result<std::process::Child> {
    let debut = std::time::Instant::now();
    loop {
        let essai = std::process::Command::new(chemin)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        match essai {
            Err(e)
                if e.kind() == io::ErrorKind::ExecutableFileBusy
                    && debut.elapsed() < DELAI_FICHIER_OCCUPE =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            autre => return autre,
        }
    }
}

/// Force sur le disque les entrées du dossier (les renommages). Sans lui, la
/// bascule elle-même peut ne pas survivre à une coupure.
pub(super) fn synchroniser_dossier(dir: &Path) {
    if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        warn!(dir = %dir.display(), error = %e, "update_dir_sync_failed");
    }
}

/// Taille cumulée des fichiers d'un dossier (0 s'il n'existe pas).
pub(super) fn taille_du_dossier(dir: &Path) -> u64 {
    let Ok(entrees) = std::fs::read_dir(dir) else {
        return 0;
    };
    entrees
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => taille_du_dossier(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// `<cible>.<suffixe>`, à côté de `cible` : `web` → `web.new`.
pub(super) fn a_cote(cible: &Path, suffixe: &str) -> PathBuf {
    let mut nom = cible.file_name().unwrap_or_default().to_os_string();
    nom.push(".");
    nom.push(suffixe);
    cible.with_file_name(nom)
}

/// Copie un fichier, conserve ses droits et force ses données sur le disque.
pub(super) fn copier_durablement(src: &Path, dst: &Path) -> io::Result<()> {
    let mut entree = std::fs::File::open(src)?;
    let droits = entree.metadata()?.permissions();
    let mut sortie = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    io::copy(&mut entree, &mut sortie)?;
    sortie.set_permissions(droits)?;
    sortie.sync_all()
}

/// Copie récursive de `src` vers `dst` (créé), chaque fichier par `copier`,
/// chaque dossier synchronisé après son contenu.
fn copier_le_dossier(
    src: &Path,
    dst: &Path,
    copier: fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    std::fs::create_dir(dst)?;
    for entree in std::fs::read_dir(src)? {
        let entree = entree?;
        let vers = dst.join(entree.file_name());
        if entree.path().is_dir() {
            copier_le_dossier(&entree.path(), &vers, copier)?;
        } else {
            copier(&entree.path(), &vers)?;
        }
    }
    std::fs::File::open(dst)?.sync_all()
}

/// Fichiers de la racine de `web/` sans lesquels l'interface ne s'affiche
/// pas : vides, ils sont refusés même si l'archive les donne vides.
const FICHIERS_WEB_ESSENTIELS: [&str; 4] = ["index.html", "sw.js", "manifest.json", "version.json"];

/// Vérifie le `web/` préparé contre sa source, avant toute bascule :
/// `index.html` présent, chaque fichier présent à la taille de la source, et
/// aucun fichier vide quand la source ne l'est pas (ni un essentiel vide).
pub(super) fn verifier_le_web(source: &Path, prepare: &Path) -> Result<(), String> {
    if !prepare.join("index.html").is_file() {
        return Err(format!(
            "invalid web: index.html missing in {}",
            prepare.display()
        ));
    }
    fn parcourir(source: &Path, prepare: &Path, relatif: &Path) -> Result<(), String> {
        let entrees = std::fs::read_dir(source.join(relatif))
            .map_err(|e| format!("invalid web: cannot read {}: {e}", relatif.display()))?;
        for entree in entrees {
            let entree = entree.map_err(|e| format!("invalid web: {e}"))?;
            let rel = relatif.join(entree.file_name());
            if entree.path().is_dir() {
                parcourir(source, prepare, &rel)?;
                continue;
            }
            let attendue = std::fs::metadata(entree.path())
                .map_err(|e| format!("invalid web: cannot read source {}: {e}", rel.display()))?
                .len();
            let ecrite = std::fs::metadata(prepare.join(&rel))
                .map_err(|_| format!("invalid web: {} missing", rel.display()))?
                .len();
            let essentiel = relatif.as_os_str().is_empty()
                && FICHIERS_WEB_ESSENTIELS
                    .iter()
                    .any(|f| entree.file_name() == *f);
            if ecrite == 0 && (attendue > 0 || essentiel) {
                return Err(format!("invalid web: {} is empty (0 bytes)", rel.display()));
            }
            if ecrite != attendue {
                return Err(format!(
                    "invalid web: {}: {ecrite} bytes written, {attendue} expected",
                    rel.display()
                ));
            }
        }
        Ok(())
    }
    parcourir(source, prepare, Path::new(""))
}

/// Prépare `<cible>.new` pour chaque cible, copie durable puis vérifiée,
/// sans toucher aux cibles. Au premier échec, toutes les copies préparées
/// sont retirées. Rend les cibles préparées (aucune si l'archive n'a pas de
/// `web/`).
pub(super) fn preparer_les_web(
    source: &Path,
    cibles: &[PathBuf],
    copier: fn(&Path, &Path) -> io::Result<()>,
) -> Result<Vec<PathBuf>, String> {
    if !source.is_dir() {
        tracing::info!("no web/ directory in archive, skipping web update");
        return Ok(Vec::new());
    }
    let mut preparees: Vec<PathBuf> = Vec::new();
    for cible in cibles {
        let prepare = a_cote(cible, "new");
        let resultat = (|| {
            if prepare.exists() {
                std::fs::remove_dir_all(&prepare)
                    .map_err(|e| format!("clear staged web {}: {e}", prepare.display()))?;
            }
            copier_le_dossier(source, &prepare, copier)
                .map_err(|e| format!("stage new web/ {}: {e}", prepare.display()))?;
            verifier_le_web(source, &prepare)
        })();
        if let Err(e) = resultat {
            std::fs::remove_dir_all(&prepare).ok();
            abandonner_les_web(&preparees);
            return Err(e);
        }
        if let Some(parent) = prepare.parent() {
            synchroniser_dossier(parent);
        }
        preparees.push(cible.clone());
    }
    Ok(preparees)
}

/// Retire les `<cible>.new` préparés et non basculés.
pub(super) fn abandonner_les_web(cibles: &[PathBuf]) {
    for cible in cibles {
        std::fs::remove_dir_all(a_cote(cible, "new")).ok();
    }
}

/// Une cible `web/` basculée : de quoi la remettre.
pub(super) struct WebBascule {
    pub cible: PathBuf,
    /// Un ancien `web/` a été mis de côté en `<cible>.old`.
    pub ancien_garde: bool,
}

/// Bascule une cible préparée : l'ancien `web/` part en `<cible>.old` (le
/// précédent `.old` est retiré), le `.new` prend son nom, le dossier parent
/// est synchronisé. Un échec laisse la cible comme avant.
pub(super) fn basculer_un_web(
    cible: &Path,
    renommer: fn(&Path, &Path) -> io::Result<()>,
) -> Result<WebBascule, String> {
    let prepare = a_cote(cible, "new");
    let ancien = a_cote(cible, "old");
    if ancien.exists() {
        std::fs::remove_dir_all(&ancien)
            .map_err(|e| format!("clear web backup {}: {e}", ancien.display()))?;
    }
    let ancien_garde = cible.exists();
    if ancien_garde {
        renommer(cible, &ancien).map_err(|e| format!("park old web/ {}: {e}", cible.display()))?;
    }
    if let Err(e) = renommer(&prepare, cible) {
        if ancien_garde {
            std::fs::rename(&ancien, cible).ok();
        }
        return Err(format!("swap new web/ into {}: {e}", cible.display()));
    }
    if let Some(parent) = cible.parent() {
        synchroniser_dossier(parent);
    }
    Ok(WebBascule {
        cible: cible.to_path_buf(),
        ancien_garde,
    })
}

/// Remet l'ancien `web/` d'une cible basculée ; le neuf est retiré.
pub(super) fn remettre_un_web(b: &WebBascule) -> Result<(), String> {
    let rejete = a_cote(&b.cible, "new");
    if rejete.exists() {
        std::fs::remove_dir_all(&rejete).ok();
    }
    std::fs::rename(&b.cible, &rejete).map_err(|e| format!("{e}"))?;
    if b.ancien_garde {
        std::fs::rename(a_cote(&b.cible, "old"), &b.cible).map_err(|e| format!("{e}"))?;
    }
    std::fs::remove_dir_all(&rejete).ok();
    if let Some(parent) = b.cible.parent() {
        synchroniser_dossier(parent);
    }
    Ok(())
}

/// #7756 — une installation factice : l'ancien binaire est un script
/// exécutable qui répond comme le serveur ; on prouve qu'après chaque refus
/// il est toujours à sa place, intact, et qu'il s'exécute.
#[cfg(all(test, unix))]
mod tests {
    use super::super::install_unix;
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    const ANCIEN: &str = "#!/bin/sh\necho tune-server 1.0.0-rc9\n";
    const ANCIEN_INDEX: &str = "<html>rc9</html>";

    /// Le `web/` neuf de l'archive : (chemin relatif, contenu).
    const WEB_NEUF: [(&str, &str); 5] = [
        ("index.html", "<html>rc10</html>"),
        ("sw.js", "self.addEventListener('fetch', () => {});"),
        ("manifest.json", "{\"name\":\"Tune\"}"),
        ("version.json", "{\"version\":\"1.0.0-rc10\"}"),
        (
            "assets/app.js",
            "console.log('tune rc10, un fichier un peu plus long');",
        ),
    ];

    struct Installation {
        _racine: tempfile::TempDir,
        binaire: PathBuf,
        extraction: PathBuf,
    }

    impl Installation {
        fn nouvelle() -> Self {
            let racine = tempfile::tempdir().unwrap();
            let opt = racine.path().join("opt");
            let extraction = racine.path().join("extraction");
            std::fs::create_dir_all(&opt).unwrap();
            std::fs::create_dir_all(&extraction).unwrap();
            let binaire = opt.join("tune-server");
            std::fs::write(&binaire, ANCIEN).unwrap();
            std::fs::set_permissions(&binaire, std::fs::Permissions::from_mode(0o755)).unwrap();
            for web in ["web", "repli"] {
                std::fs::create_dir_all(opt.join(web).join("assets")).unwrap();
                std::fs::write(opt.join(web).join("index.html"), ANCIEN_INDEX).unwrap();
                std::fs::write(opt.join(web).join("assets/old.js"), "rc9").unwrap();
            }
            let web_neuf = extraction.join("web");
            std::fs::create_dir_all(web_neuf.join("assets")).unwrap();
            for (nom, contenu) in WEB_NEUF {
                std::fs::write(web_neuf.join(nom), contenu).unwrap();
            }
            Self {
                _racine: racine,
                binaire,
                extraction,
            }
        }

        /// Pose le « binaire » extrait de l'archive.
        fn extrait(&self, contenu: &[u8]) -> PathBuf {
            let neuf = self.extraction.join("tune-server");
            std::fs::write(&neuf, contenu).unwrap();
            neuf
        }

        fn installer(&self, neuf: &Path, controles: &Controles) -> Result<(), String> {
            install_unix(
                &self.binaire,
                &self.binaire,
                neuf,
                &self.extraction,
                controles,
            )
        }

        fn opt(&self) -> &Path {
            self.binaire.parent().unwrap()
        }

        /// Un `web/` (`web` ou `repli`) est l'ancien, entier, sans `.new`
        /// qui traîne.
        fn ancien_web_intact(&self, nom: &str) {
            let web = self.opt().join(nom);
            assert_eq!(
                std::fs::read_to_string(web.join("index.html")).unwrap_or_default(),
                ANCIEN_INDEX,
                "{nom}/ n'est plus l'ancien : binaire et web/ ne sont plus de la même version"
            );
            assert!(
                web.join("assets/old.js").exists(),
                "{nom}/ ancien incomplet"
            );
            assert!(!web.join("sw.js").exists(), "{nom}/ mêle ancien et neuf");
            assert!(
                !self.opt().join(format!("{nom}.new")).exists(),
                "la copie préparée {nom}.new est restée sur le disque"
            );
        }

        /// L'ancien binaire est sous son nom, inchangé, en 0755, et s'exécute ;
        /// l'ancien `web/` est en place.
        fn ancien_intact(&self) {
            self.ancien_web_intact("web");
            assert_eq!(
                std::fs::read_to_string(&self.binaire).unwrap(),
                ANCIEN,
                "l'ancien binaire a été remplacé ou tronqué"
            );
            let mode = std::fs::metadata(&self.binaire)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o755,
                "l'ancien binaire n'est plus exécutable"
            );
            let sortie = std::process::Command::new(&self.binaire)
                .output()
                .expect("l'ancien binaire ne s'exécute plus");
            assert_eq!(
                String::from_utf8_lossy(&sortie.stdout),
                "tune-server 1.0.0-rc9\n"
            );
            assert!(
                !self.binaire.with_extension("new").exists(),
                "le .new refusé est resté sur le disque"
            );
        }
    }

    /// Un en-tête ELF minimal (64 bits, petit-boutiste) pour `machine`,
    /// complété à `taille` octets.
    fn faux_elf(machine: u16, taille: usize) -> Vec<u8> {
        let mut b = vec![0u8; taille.max(64)];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[6] = 1;
        b[16] = 2;
        b[18..20].copy_from_slice(&machine.to_le_bytes());
        b
    }

    fn machine_hote() -> u16 {
        code_elf(std::env::consts::ARCH).unwrap_or(0x3E)
    }

    fn controles(taille_attendue: Option<u64>) -> Controles {
        Controles {
            taille_attendue,
            place_libre: |_| Ok(u64::MAX),
            architecture: Some(match code_elf(std::env::consts::ARCH) {
                Some(_) => std::env::consts::ARCH,
                None => "x86_64",
            }),
            essai: |_| Ok(()),
            cibles_web: |exe_dir| vec![exe_dir.join("web")],
            copier: copier_durablement,
            renommer: |de, vers| std::fs::rename(de, vers),
        }
    }

    #[test]
    fn un_binaire_valide_remplace_l_ancien_qui_part_en_old() {
        let inst = Installation::nouvelle();
        let elf = faux_elf(machine_hote(), 4096);
        let neuf = inst.extrait(&elf);
        inst.installer(&neuf, &controles(Some(4096)))
            .expect("installation d'un binaire valide");
        assert_eq!(std::fs::read(&inst.binaire).unwrap(), elf);
        assert_eq!(
            std::fs::read_to_string(inst.binaire.with_extension("old")).unwrap(),
            ANCIEN
        );
        assert!(!inst.binaire.with_extension("new").exists());
        // `web/` neuf en place, entier ; l'ancien gardé en `web.old`, à côté
        // de `tune-server.old`.
        let web = inst.opt().join("web");
        for (nom, contenu) in WEB_NEUF {
            assert_eq!(
                std::fs::read_to_string(web.join(nom)).unwrap(),
                contenu,
                "{nom}"
            );
        }
        assert!(
            !web.join("assets/old.js").exists(),
            "web/ mêle ancien et neuf"
        );
        assert_eq!(
            std::fs::read_to_string(inst.opt().join("web.old/index.html")).unwrap(),
            ANCIEN_INDEX,
            "l'ancien web/ n'est pas gardé en web.old pour le retour en arrière"
        );
        assert!(!inst.opt().join("web.new").exists());
    }

    /// Le cas du testeur : un binaire de 0 octet.
    #[test]
    fn un_binaire_vide_est_refuse_et_l_ancien_reste_en_place() {
        let inst = Installation::nouvelle();
        let neuf = inst.extrait(b"");
        let err = inst
            .installer(&neuf, &controles(None))
            .expect_err("un binaire de 0 octet a remplacé l'ancien");
        assert!(
            err.contains("file is empty (0 bytes)"),
            "cause absente : {err}"
        );
        assert!(err.contains("previous binary kept"), "{err}");
        inst.ancien_intact();
        assert!(!inst.binaire.with_extension("old").exists());
    }

    /// Une écriture interrompue (disque plein pendant l'extraction) : le
    /// fichier fait moins que ce qu'annonce l'archive.
    #[test]
    fn un_binaire_tronque_est_refuse_et_l_ancien_reste_en_place() {
        let inst = Installation::nouvelle();
        let entier = faux_elf(machine_hote(), 8192);
        let neuf = inst.extrait(&entier[..3000]);
        let err = inst
            .installer(&neuf, &controles(Some(8192)))
            .expect_err("un binaire tronqué a remplacé l'ancien");
        assert!(
            err.contains("3000 bytes written, 8192 expected"),
            "la troncature n'est pas nommée : {err}"
        );
        inst.ancien_intact();
    }

    /// Disque plein avant de commencer : refus sans rien écrire.
    #[test]
    fn sans_place_rien_n_est_ecrit_et_la_cause_est_dite() {
        let inst = Installation::nouvelle();
        let neuf = inst.extrait(&faux_elf(machine_hote(), 4096));
        let plein = Controles {
            place_libre: |_| Ok(10 * 1024 * 1024),
            ..controles(Some(4096))
        };
        let err = inst
            .installer(&neuf, &plein)
            .expect_err("installation lancée sans la place nécessaire");
        assert!(err.starts_with("Not enough disk space in "), "{err}");
        assert!(err.contains("MB needed, 10 MB free"), "{err}");
        inst.ancien_intact();
        assert!(!inst.binaire.with_extension("old").exists());
    }

    #[test]
    fn un_elf_d_une_autre_architecture_est_refuse() {
        let inst = Installation::nouvelle();
        let autre = if machine_hote() == 0xB7 { 0x3E } else { 0xB7 };
        let neuf = inst.extrait(&faux_elf(autre, 4096));
        let err = inst
            .installer(&neuf, &controles(Some(4096)))
            .expect_err("un binaire d'une autre architecture a remplacé l'ancien");
        assert!(err.contains("built for"), "{err}");
        assert!(err.contains("but this machine is"), "{err}");
        inst.ancien_intact();
    }

    #[test]
    fn un_fichier_qui_n_est_pas_un_elf_est_refuse() {
        let inst = Installation::nouvelle();
        let neuf = inst.extrait(&[b'x'; 4096]);
        let err = inst
            .installer(&neuf, &controles(None))
            .expect_err("un fichier non exécutable a remplacé l'ancien binaire");
        assert!(err.contains("not an ELF executable"), "{err}");
        inst.ancien_intact();
    }

    /// L'essai `--version` réel : un binaire qui échoue est refusé, un
    /// binaire qui répond comme le serveur est accepté.
    #[test]
    fn l_essai_version_refuse_un_binaire_qui_ne_repond_pas() {
        let inst = Installation::nouvelle();
        let reel = Controles {
            architecture: None,
            essai: essayer_version,
            ..controles(None)
        };
        let neuf = inst.extrait(b"#!/bin/sh\nexit 3\n");
        let err = inst
            .installer(&neuf, &reel)
            .expect_err("un binaire qui ne répond pas à --version a remplacé l'ancien");
        assert!(err.contains("`--version` failed"), "{err}");
        inst.ancien_intact();

        let neuf = inst.extrait(b"#!/bin/sh\necho tune-server 1.0.0-rc10\n");
        inst.installer(&neuf, &reel).expect("binaire qui répond");
    }

    /// Un binaire valide encore ouvert en écriture ailleurs (le `fork` d'un
    /// autre fil hérite du descripteur de `ecrire_durablement`) n'est pas
    /// refusé : l'essai attend qu'il soit libre. Le descripteur tenu ici
    /// rejoue, sans dépendre du hasard, le « Text file busy » de la CI de
    /// #7755.
    #[test]
    fn l_essai_version_attend_un_binaire_encore_ouvert_en_ecriture() {
        let inst = Installation::nouvelle();
        let neuf = inst.extrait(b"#!/bin/sh\necho tune-server 1.0.0-rc10\n");
        std::fs::set_permissions(&neuf, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ecrivain = std::fs::OpenOptions::new().write(true).open(&neuf).unwrap();
        let liberation = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(ecrivain);
        });
        let resultat = essayer_version(&neuf);
        liberation.join().unwrap();
        resultat.unwrap_or_else(|e| {
            panic!("un binaire valide, encore ouvert en écriture un instant, a été refusé : {e}")
        });
    }

    /// Le constat du 03/11 : un fichier de `web/` écrit vide (données
    /// perdues). La copie préparée est refusée avant de toucher à quoi que
    /// ce soit : ancien binaire et ancien `web/` restent en place.
    #[test]
    fn un_fichier_web_ecrit_vide_est_refuse_et_tout_reste_en_place() {
        let inst = Installation::nouvelle();
        let neuf = inst.extrait(&faux_elf(machine_hote(), 4096));
        let perd_les_donnees = Controles {
            copier: |de, vers| {
                if de.ends_with("manifest.json") {
                    std::fs::File::create(vers).map(|_| ())
                } else {
                    copier_durablement(de, vers)
                }
            },
            ..controles(Some(4096))
        };
        let err = inst
            .installer(&neuf, &perd_les_donnees)
            .expect_err("un web/ avec un fichier vide a été mis en place");
        assert!(
            err.contains("invalid web: manifest.json is empty (0 bytes)"),
            "le fichier vide n'est pas nommé : {err}"
        );
        assert!(err.contains("previous binary and web/ kept"), "{err}");
        inst.ancien_intact();
        assert!(!inst.binaire.with_extension("old").exists());
        assert!(!inst.opt().join("web.old").exists());
    }

    /// Une archive qui donne elle-même un fichier essentiel vide, ou pas
    /// d'`index.html` : refus, rien n'est touché.
    #[test]
    fn un_web_sans_index_ou_avec_un_essentiel_vide_est_refuse() {
        let inst = Installation::nouvelle();
        let neuf = inst.extrait(&faux_elf(machine_hote(), 4096));
        let source = inst.extraction.join("web");
        std::fs::write(source.join("sw.js"), "").unwrap();
        let err = inst
            .installer(&neuf, &controles(Some(4096)))
            .expect_err("un web/ avec sw.js vide a été mis en place");
        assert!(
            err.contains("invalid web: sw.js is empty (0 bytes)"),
            "{err}"
        );
        inst.ancien_intact();

        std::fs::write(source.join("sw.js"), "self").unwrap();
        std::fs::remove_file(source.join("index.html")).unwrap();
        let err = inst
            .installer(&neuf, &controles(Some(4096)))
            .expect_err("un web/ sans index.html a été mis en place");
        assert!(err.contains("invalid web: index.html missing"), "{err}");
        inst.ancien_intact();
    }

    /// Une coupure pendant la copie de `web/` : la copie s'arrête au milieu
    /// d'un fichier. Le binaire n'a pas encore été touché ; ni l'ancien
    /// binaire ni l'ancien `web/` ne bougent, et aucun `.new` ne traîne.
    #[test]
    fn une_coupure_pendant_la_copie_de_web_laisse_l_installation_d_avant() {
        let inst = Installation::nouvelle();
        let neuf = inst.extrait(&faux_elf(machine_hote(), 4096));
        let coupure = Controles {
            copier: |de, vers| {
                if de.ends_with("assets/app.js") {
                    let moitie = &std::fs::read(de)?[..10];
                    std::fs::write(vers, moitie)?;
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "coupure"));
                }
                copier_durablement(de, vers)
            },
            ..controles(Some(4096))
        };
        let err = inst
            .installer(&neuf, &coupure)
            .expect_err("la coupure pendant la copie de web/ n'est pas remontée");
        assert!(err.contains("stage new web/"), "{err}");
        assert!(err.contains("coupure"), "{err}");
        assert!(err.contains("previous binary and web/ kept"), "{err}");
        inst.ancien_intact();
        assert!(!inst.binaire.with_extension("old").exists());
    }

    /// Un échec pendant la bascule, après celle du binaire et du `web/`
    /// servi (ici le renommage du `web/` de repli) : binaire ET `web/` sont
    /// remis ensemble, jamais un ancien binaire avec un `web/` neuf.
    #[test]
    fn un_echec_de_bascule_remet_le_binaire_et_web_ensemble() {
        let inst = Installation::nouvelle();
        let neuf = inst.extrait(&faux_elf(machine_hote(), 4096));
        let repli_en_echec = Controles {
            cibles_web: |exe_dir| vec![exe_dir.join("web"), exe_dir.join("repli")],
            renommer: |de, vers| {
                if de.ends_with("repli.new") {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Permission denied",
                    ));
                }
                std::fs::rename(de, vers)
            },
            ..controles(Some(4096))
        };
        let err = inst
            .installer(&neuf, &repli_en_echec)
            .expect_err("l'échec de bascule de web/ n'est pas remonté");
        assert!(err.contains("swap new web/ into"), "{err}");
        assert!(
            err.contains("previous binary and web/ restored"),
            "binaire et web/ n'ont pas été remis ensemble : {err}"
        );
        inst.ancien_intact();
        inst.ancien_web_intact("repli");
        assert!(!inst.binaire.with_extension("old").exists());
        assert!(!inst.opt().join("web.old").exists());
        assert!(!inst.opt().join("repli.old").exists());
    }

    #[test]
    fn l_archive_annonce_la_taille_du_binaire_et_le_total() {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (nom, taille) in [("tune-server", 5000usize), ("web/index.html", 300)] {
            let mut h = tar::Header::new_gnu();
            h.set_size(taille as u64);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, nom, &vec![0u8; taille][..])
                .unwrap();
        }
        let data = tar.into_inner().unwrap().finish().unwrap();
        assert_eq!(
            contenu_de_l_archive(&data, false, "tune-server").unwrap(),
            ContenuArchive {
                binaire: Some(5000),
                totale: 5300
            }
        );
    }

    #[test]
    fn la_place_manquante_est_refusee_avec_les_chiffres() {
        let err = verifier_la_place(Path::new("/opt/tune"), 300 * 1024 * 1024, |_| {
            Ok(100 * 1024 * 1024)
        })
        .unwrap_err();
        assert_eq!(
            err,
            "Not enough disk space in /opt/tune: 300 MB needed, 100 MB free"
        );
        assert!(verifier_la_place(Path::new("/x"), 1, |_| Err(io::Error::other("?"))).is_ok());
    }
}
