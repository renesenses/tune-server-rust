//! #8944 — essais de la bascule des exécutables livrés, sur une installation
//! factice dans un dossier temporaire : aucun vrai binaire n'est remplacé,
//! aucun processus n'est arrêté. Les « exécutables » sont de faux ELF de
//! l'architecture de la machine (le contrôle d'en-tête les accepte), jamais
//! exécutés.

use super::*;

/// Un en-tête ELF minimal (64 bits, petit-boutiste) pour `machine`, suivi
/// d'une étiquette lisible qui dit sa version.
fn faux_elf(machine: u16, etiquette: &str) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    b[..4].copy_from_slice(b"\x7fELF");
    b[4] = 2;
    b[5] = 1;
    b[6] = 1;
    b[16] = 2;
    b[18..20].copy_from_slice(&machine.to_le_bytes());
    b.extend_from_slice(etiquette.as_bytes());
    b
}

fn machine_hote() -> u16 {
    match std::env::consts::ARCH {
        "aarch64" => 0xB7,
        "arm" => 0x28,
        "x86" => 0x03,
        "riscv64" => 0xF3,
        _ => 0x3E,
    }
}

fn architecture_hote() -> &'static str {
    match std::env::consts::ARCH {
        a @ ("x86_64" | "aarch64" | "arm" | "x86" | "riscv64") => a,
        _ => "x86_64",
    }
}

fn elf(etiquette: &str) -> Vec<u8> {
    faux_elf(machine_hote(), etiquette)
}

fn controles() -> Controles {
    Controles {
        taille_attendue: None,
        place_libre: |_| Ok(u64::MAX),
        architecture: Some(architecture_hote()),
        essai: |_| Ok(()),
        cibles_web: |exe_dir| vec![exe_dir.join("web")],
        copier: installation_sure::copier_durablement,
        renommer: |de, vers| std::fs::rename(de, vers),
    }
}

#[cfg(unix)]
mod installation_unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const ANCIEN_SERVEUR: &[u8] = b"#!/bin/sh\necho tune-server 1.0.0-rc12\n";

    /// `opt/` : le dossier d'installation (tune-server, airplay-daemon,
    /// librespot, ffmpeg de la rc11, web/) ; `extraction/` : l'archive rc13.
    struct Installation {
        _racine: tempfile::TempDir,
        opt: PathBuf,
        extraction: PathBuf,
    }

    impl Installation {
        fn nouvelle() -> Self {
            let racine = tempfile::tempdir().unwrap();
            let opt = racine.path().join("opt");
            let extraction = racine.path().join("extraction");
            std::fs::create_dir_all(opt.join("web")).unwrap();
            std::fs::create_dir_all(extraction.join("web")).unwrap();
            ecrire_x(&opt.join("tune-server"), ANCIEN_SERVEUR);
            for nom in ["airplay-daemon", "librespot", "ffmpeg"] {
                ecrire_x(&opt.join(nom), &elf(&format!("{nom} ancien")));
            }
            std::fs::write(opt.join("web/index.html"), "<html>rc12</html>").unwrap();
            std::fs::write(extraction.join("web/index.html"), "<html>rc13</html>").unwrap();
            std::fs::write(extraction.join("tune-server"), elf("tune-server neuf")).unwrap();
            Self {
                _racine: racine,
                opt,
                extraction,
            }
        }

        /// Pose `nom` dans l'archive extraite, en 0644 comme peut le laisser
        /// une extraction : l'installé doit sortir en 0755 quand même.
        fn archive(&self, nom: &str, contenu: &[u8]) {
            let p = self.extraction.join(nom);
            std::fs::write(&p, contenu).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        fn installer(&self, controles: &Controles) -> Result<(), String> {
            let binaire = self.opt.join("tune-server");
            super::super::super::install_unix(
                &binaire,
                &binaire,
                &self.extraction.join("tune-server"),
                &self.extraction,
                controles,
            )
        }

        fn lire(&self, nom: &str) -> Vec<u8> {
            std::fs::read(self.opt.join(nom)).unwrap_or_default()
        }

        fn mode(&self, nom: &str) -> u32 {
            std::fs::metadata(self.opt.join(nom))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        }

        fn restes(&self) -> Vec<String> {
            let mut r: Vec<String> = std::fs::read_dir(&self.opt)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".new"))
                .collect();
            r.sort();
            r
        }

        /// Tout est comme avant : serveur, démons, web/, aucun `.new`.
        fn tout_est_l_ancien(&self) {
            assert_eq!(self.lire("tune-server"), ANCIEN_SERVEUR, "serveur remplacé");
            for nom in ["airplay-daemon", "librespot", "ffmpeg"] {
                assert_eq!(
                    self.lire(nom),
                    elf(&format!("{nom} ancien")),
                    "{nom} n'est plus l'ancien"
                );
                assert_eq!(self.mode(nom), 0o755, "{nom} n'est plus exécutable");
            }
            assert_eq!(
                std::fs::read_to_string(self.opt.join("web/index.html")).unwrap(),
                "<html>rc12</html>"
            );
            assert_eq!(self.restes(), Vec::<String>::new(), "des .new traînent");
        }
    }

    fn ecrire_x(p: &Path, contenu: &[u8]) {
        std::fs::write(p, contenu).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Le cas du fil 2878 : l'archive porte un `airplay-daemon` neuf ; après
    /// `install_unix`, l'installé est le neuf, exécutable, l'ancien en `.old`.
    #[test]
    fn le_demon_airplay_neuf_de_l_archive_remplace_l_installe_8944() {
        let inst = Installation::nouvelle();
        inst.archive("airplay-daemon", &elf("airplay-daemon neuf"));
        inst.archive("librespot", &elf("librespot neuf"));

        inst.installer(&controles()).expect("installation");

        assert_eq!(inst.lire("tune-server"), elf("tune-server neuf"));
        assert_eq!(inst.lire("airplay-daemon"), elf("airplay-daemon neuf"));
        assert_eq!(inst.lire("librespot"), elf("librespot neuf"));
        assert_eq!(inst.mode("airplay-daemon"), 0o755);
        assert_eq!(inst.mode("librespot"), 0o755);
        assert_eq!(
            inst.lire("airplay-daemon.old"),
            elf("airplay-daemon ancien")
        );
        assert_eq!(inst.lire("librespot.old"), elf("librespot ancien"));
        // ffmpeg absent de l'archive : l'installé reste.
        assert_eq!(inst.lire("ffmpeg"), elf("ffmpeg ancien"));
        assert!(!inst.opt.join("ffmpeg.old").exists());
        assert_eq!(inst.restes(), Vec::<String>::new());
    }

    /// Un exécutable livré absent de l'installation (premier `fpcalc`) est
    /// posé ; un fichier hors de la liste fixe ne l'est jamais.
    #[test]
    fn seuls_les_noms_de_la_liste_fixe_sont_installes_8944() {
        let inst = Installation::nouvelle();
        inst.archive("fpcalc", &elf("fpcalc neuf"));
        inst.archive("intrus", &elf("intrus"));
        inst.archive("airplay-daemon.sh", b"#!/bin/sh\n");

        inst.installer(&controles()).expect("installation");

        assert_eq!(inst.lire("fpcalc"), elf("fpcalc neuf"));
        assert_eq!(inst.mode("fpcalc"), 0o755);
        assert!(!inst.opt.join("fpcalc.old").exists());
        assert!(
            !inst.opt.join("intrus").exists(),
            "fichier hors liste installé"
        );
        assert!(!inst.opt.join("airplay-daemon.sh").exists());
    }

    /// Un démon de la mauvaise architecture est refusé AVANT toute bascule :
    /// rien ne change, ni serveur, ni démons, ni web/.
    #[test]
    fn un_demon_invalide_laisse_toute_l_installation_comme_avant_8944() {
        let inst = Installation::nouvelle();
        inst.archive("airplay-daemon", &elf("airplay-daemon neuf"));
        let autre = if machine_hote() == 0x3E { 0xB7 } else { 0x3E };
        inst.archive("librespot", &faux_elf(autre, "librespot autre arch"));

        let e = inst.installer(&controles()).unwrap_err();

        assert!(e.contains("librespot"), "{e}");
        assert!(
            e.contains("previous binary, bundled executables and web/ kept"),
            "{e}"
        );
        inst.tout_est_l_ancien();
    }

    /// Un démon vide dans l'archive (copie coupée) est refusé de même.
    #[test]
    fn un_demon_vide_est_refuse_8944() {
        let inst = Installation::nouvelle();
        inst.archive("airplay-daemon", b"");

        let e = inst.installer(&controles()).unwrap_err();

        assert!(e.contains("empty"), "{e}");
        inst.tout_est_l_ancien();
    }

    /// La bascule de `librespot` échoue après celle du serveur et
    /// d'`airplay-daemon` : tout est remis, serveur compris.
    #[test]
    fn un_echec_de_bascule_remet_serveur_et_demons_8944() {
        let inst = Installation::nouvelle();
        inst.archive("airplay-daemon", &elf("airplay-daemon neuf"));
        inst.archive("librespot", &elf("librespot neuf"));
        let mut c = controles();
        c.renommer = |de, vers| {
            if vers.file_name().is_some_and(|n| n == "librespot") {
                return Err(io::Error::other("refus simulé"));
            }
            std::fs::rename(de, vers)
        };

        let e = inst.installer(&c).unwrap_err();

        assert!(
            e.contains("previous binary and bundled executables restored"),
            "{e}"
        );
        inst.tout_est_l_ancien();
    }

    /// `web/` échoue après la bascule des démons : les démons sont remis
    /// avec le serveur.
    #[test]
    fn un_echec_de_web_remet_aussi_les_demons_8944() {
        let inst = Installation::nouvelle();
        inst.archive("airplay-daemon", &elf("airplay-daemon neuf"));
        let mut c = controles();
        c.renommer = |de, vers| {
            if de.file_name().is_some_and(|n| n == "web.new") {
                return Err(io::Error::other("refus simulé"));
            }
            std::fs::rename(de, vers)
        };

        let e = inst.installer(&c).unwrap_err();

        assert!(e.contains("bundled executables"), "{e}");
        inst.tout_est_l_ancien();
    }

    /// Un outil remplacé à la main par un lien (ffmpeg du système) reste un
    /// lien ; un lien dans l'archive n'est jamais suivi.
    #[test]
    fn les_liens_ne_sont_ni_remplaces_ni_suivis_8944() {
        let inst = Installation::nouvelle();
        let systeme = inst.extraction.parent().unwrap().join("ffmpeg-systeme");
        ecrire_x(&systeme, &elf("ffmpeg systeme"));
        std::fs::remove_file(inst.opt.join("ffmpeg")).unwrap();
        std::os::unix::fs::symlink(&systeme, inst.opt.join("ffmpeg")).unwrap();
        inst.archive("ffmpeg", &elf("ffmpeg neuf"));
        std::os::unix::fs::symlink(&systeme, inst.extraction.join("fpcalc")).unwrap();

        inst.installer(&controles()).expect("installation");

        assert!(
            std::fs::symlink_metadata(inst.opt.join("ffmpeg"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&systeme).unwrap(), elf("ffmpeg systeme"));
        assert!(!inst.opt.join("fpcalc").exists(), "lien de l'archive suivi");
    }

    /// Un démon en cours garde son fichier : renommer sous lui ne le touche
    /// pas. Le « démon » est un `sleep` lancé depuis une COPIE locale du
    /// `sleep` du système, posée dans le dossier temporaire sous le nom
    /// `airplay-daemon` ; il est arrêté par son propre `Child` à la fin.
    #[test]
    fn un_demon_en_cours_ne_bloque_pas_la_bascule_8944() {
        let inst = Installation::nouvelle();
        let Some(sleep) = ["/bin/sleep", "/usr/bin/sleep"]
            .iter()
            .map(Path::new)
            .find(|p| p.is_file())
        else {
            return;
        };
        let installe = inst.opt.join("airplay-daemon");
        std::fs::copy(sleep, &installe).unwrap();
        std::fs::set_permissions(&installe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut demon = loop {
            match std::process::Command::new(&installe).arg("30").spawn() {
                Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                autre => break autre.unwrap(),
            }
        };
        inst.archive("airplay-daemon", &elf("airplay-daemon neuf"));

        let resultat = inst.installer(&controles());
        let encore_vivant = demon.try_wait().unwrap().is_none();
        demon.kill().ok();
        demon.wait().ok();

        resultat.expect("installation");
        assert!(encore_vivant, "la bascule a touché au démon en cours");
        assert_eq!(inst.lire("airplay-daemon"), elf("airplay-daemon neuf"));
    }
    /// Relecture de #8954 — la cible n'est jamais absente pendant la
    /// bascule : au moment où le neuf prend son nom, l'ancien y est encore
    /// (lien dur vers `.old`, puis `rename` qui l'écrase d'un coup).
    #[test]
    fn la_cible_n_est_jamais_absente_pendant_la_bascule_8944() {
        let inst = Installation::nouvelle();
        inst.archive("airplay-daemon", &elf("airplay-daemon neuf"));
        inst.archive("librespot", &elf("librespot neuf"));
        let mut c = controles();
        c.renommer = |de, vers| {
            let pose = de.extension().is_some_and(|e| e == "new");
            let livre = vers
                .file_name()
                .is_some_and(|n| n == "airplay-daemon" || n == "librespot");
            if pose && livre && std::fs::symlink_metadata(vers).is_err() {
                return Err(io::Error::other("cible absente pendant la bascule"));
            }
            std::fs::rename(de, vers)
        };

        inst.installer(&c).expect("installation");

        assert_eq!(inst.lire("airplay-daemon"), elf("airplay-daemon neuf"));
        assert_eq!(
            inst.lire("airplay-daemon.old"),
            elf("airplay-daemon ancien")
        );
        assert_eq!(inst.lire("librespot"), elf("librespot neuf"));
    }

    /// Le marqueur `.no-bundled-<nom>` garde un exécutable : ni créé quand il
    /// manque, ni remplacé quand l'utilisateur a mis le sien.
    #[test]
    fn le_marqueur_garde_l_executable_de_l_utilisateur_8944() {
        let inst = Installation::nouvelle();
        ecrire_x(&inst.opt.join("ffmpeg"), &elf("ffmpeg de l'utilisateur"));
        std::fs::write(inst.opt.join(".no-bundled-ffmpeg"), "").unwrap();
        std::fs::remove_file(inst.opt.join("airplay-daemon")).unwrap();
        std::fs::write(inst.opt.join(".no-bundled-airplay-daemon"), "").unwrap();
        inst.archive("ffmpeg", &elf("ffmpeg neuf"));
        inst.archive("airplay-daemon", &elf("airplay-daemon neuf"));
        inst.archive("librespot", &elf("librespot neuf"));

        inst.installer(&controles()).expect("installation");

        assert_eq!(inst.lire("ffmpeg"), elf("ffmpeg de l'utilisateur"));
        assert!(
            !inst.opt.join("airplay-daemon").exists(),
            "retrait volontaire défait"
        );
        assert_eq!(inst.lire("librespot"), elf("librespot neuf"));
    }

    /// Un `<nom>.old` qui est un dossier est écarté sous un nom unique, avec
    /// son contenu, au lieu de faire échouer la mise à jour.
    #[test]
    fn une_sauvegarde_dossier_est_ecartee_sans_echec_8944() {
        let inst = Installation::nouvelle();
        std::fs::create_dir(inst.opt.join("airplay-daemon.old")).unwrap();
        std::fs::write(inst.opt.join("airplay-daemon.old/note"), "a garder").unwrap();
        inst.archive("airplay-daemon", &elf("airplay-daemon neuf"));

        inst.installer(&controles()).expect("installation");

        assert_eq!(inst.lire("airplay-daemon"), elf("airplay-daemon neuf"));
        assert_eq!(
            inst.lire("airplay-daemon.old"),
            elf("airplay-daemon ancien")
        );
        let ecarte = std::fs::read_dir(&inst.opt)
            .unwrap()
            .flatten()
            .find(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with("airplay-daemon.old.") && e.path().is_dir()
            })
            .expect("le dossier .old a disparu");
        assert_eq!(
            std::fs::read_to_string(ecarte.path().join("note")).unwrap(),
            "a garder"
        );
    }

    fn dater(p: &Path, il_y_a: std::time::Duration) {
        let t = std::time::SystemTime::now() - il_y_a;
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    const HEURE: std::time::Duration = std::time::Duration::from_secs(3600);

    /// Coupure après la bascule du binaire : le `.new` valide d'un démon
    /// prend la place de la cible plus ancienne au démarrage suivant.
    #[test]
    fn le_demarrage_finit_une_bascule_coupee_8944() {
        let inst = Installation::nouvelle();
        dater(&inst.opt.join("airplay-daemon"), HEURE);
        std::fs::write(
            inst.opt.join("airplay-daemon.new"),
            elf("airplay-daemon neuf"),
        )
        .unwrap();
        std::fs::remove_file(inst.opt.join("librespot")).unwrap();
        std::fs::write(inst.opt.join("librespot.new"), elf("librespot neuf")).unwrap();

        let finies = finir_les_bascules_interrompues(&inst.opt.join("tune-server"), &controles());

        assert_eq!(finies.len(), 2, "{finies:?}");
        assert_eq!(inst.lire("airplay-daemon"), elf("airplay-daemon neuf"));
        assert_eq!(inst.mode("airplay-daemon"), 0o755);
        assert_eq!(inst.lire("librespot"), elf("librespot neuf"));
        assert_eq!(inst.restes(), Vec::<String>::new());
    }

    /// Coupure AVANT la bascule du binaire (`tune-server.new` encore là) :
    /// les démons restent ceux du serveur qui tourne, les `.new` partent.
    #[test]
    fn sans_bascule_du_binaire_les_demons_ne_basculent_pas_8944() {
        let inst = Installation::nouvelle();
        std::fs::write(inst.opt.join("tune-server.new"), elf("tune-server neuf")).unwrap();
        dater(&inst.opt.join("airplay-daemon"), HEURE);
        std::fs::write(
            inst.opt.join("airplay-daemon.new"),
            elf("airplay-daemon neuf"),
        )
        .unwrap();

        let finies = finir_les_bascules_interrompues(&inst.opt.join("tune-server"), &controles());

        assert!(finies.is_empty());
        assert_eq!(inst.lire("airplay-daemon"), elf("airplay-daemon ancien"));
        assert!(!inst.opt.join("airplay-daemon.new").exists());
    }

    /// Un `.new` invalide (autre architecture) ou plus ancien que la cible
    /// n'est jamais posé ; il est retiré.
    #[test]
    fn un_reste_invalide_ou_perime_n_est_pas_pose_8944() {
        let inst = Installation::nouvelle();
        let autre = if machine_hote() == 0x3E { 0xB7 } else { 0x3E };
        dater(&inst.opt.join("airplay-daemon"), HEURE);
        std::fs::write(
            inst.opt.join("airplay-daemon.new"),
            faux_elf(autre, "airplay-daemon autre arch"),
        )
        .unwrap();
        let perime = inst.opt.join("librespot.new");
        std::fs::write(&perime, elf("librespot perime")).unwrap();
        dater(&perime, HEURE);

        let finies = finir_les_bascules_interrompues(&inst.opt.join("tune-server"), &controles());

        assert!(finies.is_empty(), "{finies:?}");
        assert_eq!(inst.lire("airplay-daemon"), elf("airplay-daemon ancien"));
        assert_eq!(inst.lire("librespot"), elf("librespot ancien"));
        assert_eq!(inst.restes(), Vec::<String>::new());
    }

    /// Au premier démarrage de la version neuve, les sauvegardes des
    /// exécutables livrés partent ; ni `tune-server.old`, ni un dossier, ni
    /// un fichier étranger ne sont touchés.
    #[test]
    fn les_sauvegardes_partent_apres_le_demarrage_de_la_version_neuve_8944() {
        let inst = Installation::nouvelle();
        for f in [
            "airplay-daemon.old",
            "librespot.old.12-34",
            "tune-server.old",
            "ffmpeg.older",
        ] {
            std::fs::write(inst.opt.join(f), "x").unwrap();
        }
        std::fs::create_dir(inst.opt.join("fpcalc.old")).unwrap();

        let retirees = retirer_les_sauvegardes(&inst.opt, "");

        assert_eq!(retirees.len(), 2, "{retirees:?}");
        assert!(!inst.opt.join("airplay-daemon.old").exists());
        assert!(!inst.opt.join("librespot.old.12-34").exists());
        assert!(inst.opt.join("tune-server.old").exists());
        assert!(inst.opt.join("ffmpeg.older").exists());
        assert!(inst.opt.join("fpcalc.old").is_dir());
    }
}

/// La liste fixe suit l'archive : chaque exécutable que `release.yml` met
/// à côté de `tune-server` (archive Unix et zip Windows) y figure. Un
/// exécutable ajouté à l'archive sans l'être ici resterait figé chez les
/// utilisateurs mis à jour par le bouton — le défaut de #8944.
#[test]
fn la_liste_fixe_couvre_les_executables_de_l_archive_8944() {
    let workflow = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../.github/workflows/release.yml"
    ))
    .expect("release.yml");

    // Archive Unix : les variables de la ligne `tar czf`, et ce qu'on y range.
    let tar = workflow
        .lines()
        .find(|l| l.contains("tar czf") && l.contains("tune-server"))
        .expect("ligne tar de l'archive Unix");
    let mut unix: Vec<String> = Vec::new();
    for var in tar.split_whitespace().filter_map(|t| t.strip_prefix('$')) {
        let affectation = format!("{var}=\"");
        for l in workflow.lines().map(str::trim) {
            if let Some(reste) = l.strip_prefix(&affectation) {
                for mot in reste.trim_end_matches('"').split_whitespace() {
                    if !mot.contains('.') && !mot.starts_with('$') {
                        unix.push(mot.to_string());
                    }
                }
            }
        }
    }
    unix.sort();
    unix.dedup();
    let mut attendu: Vec<String> = EXECUTABLES_LIVRES.iter().map(|s| s.to_string()).collect();
    attendu.sort();
    assert_eq!(unix, attendu, "archive Unix ≠ EXECUTABLES_LIVRES");

    // Zip Windows : chaque `.exe` copié dans `package\`.
    let mut windows: Vec<String> = workflow
        .lines()
        .filter(|l| l.contains("copy ") && l.contains(".exe") && l.contains("package\\"))
        .filter_map(|l| {
            let source = l
                .split('"')
                .nth(1)
                .or_else(|| l.split_whitespace().nth(1))?;
            let nom = source.rsplit(['\\', '/']).next()?;
            nom.strip_suffix(".exe").map(str::to_string)
        })
        .filter(|n| n != "tune-server")
        .collect();
    windows.sort();
    windows.dedup();
    assert!(
        !windows.is_empty(),
        "aucun .exe livré trouvé dans le zip Windows"
    );
    for nom in &windows {
        assert!(
            EXECUTABLES_LIVRES.contains(&nom.as_str()),
            "{nom}.exe est dans le zip Windows mais pas dans EXECUTABLES_LIVRES"
        );
    }
}
