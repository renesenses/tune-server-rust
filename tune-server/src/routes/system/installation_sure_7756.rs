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

use std::io;
use std::path::Path;

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
    /// Remplace `web/` (`exe_dir`, `tmp_dir`) ; en production
    /// `update_web_dir`.
    pub mettre_a_jour_web: fn(&Path, &Path) -> Result<(), String>,
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
            mettre_a_jour_web: super::update_web_dir,
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
    let mut enfant = std::process::Command::new(chemin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
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

        /// L'ancien binaire est sous son nom, inchangé, en 0755, et s'exécute.
        fn ancien_intact(&self) {
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
            mettre_a_jour_web: |_, _| Ok(()),
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

    /// Un échec après la bascule (`web/`) remet l'ancien binaire en place.
    #[test]
    fn un_echec_de_web_remet_l_ancien_binaire() {
        let inst = Installation::nouvelle();
        let neuf = inst.extrait(&faux_elf(machine_hote(), 4096));
        let web_en_echec = Controles {
            mettre_a_jour_web: |_, _| Err("stage new web/: Permission denied".into()),
            ..controles(Some(4096))
        };
        let err = inst
            .installer(&neuf, &web_en_echec)
            .expect_err("l'échec de web/ n'est pas remonté");
        assert!(
            err.contains("previous binary restored"),
            "l'ancien binaire n'a pas été remis après l'échec de web/ : {err}"
        );
        inst.ancien_intact();
        assert!(!inst.binaire.with_extension("old").exists());
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
