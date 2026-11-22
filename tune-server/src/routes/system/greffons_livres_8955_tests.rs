//! #8955 — essais de la bascule des greffons livrés, sur une installation
//! factice dans un dossier temporaire. Aucun vrai greffon n'est chargé.

use super::*;

/// Un en-tête ELF minimal de l'architecture de la machine, pour que le
/// binaire « neuf » passe le contrôle d'en-tête d'`install_unix`.
fn elf(etiquette: &str) -> Vec<u8> {
    let machine: u16 = match std::env::consts::ARCH {
        "aarch64" => 0xB7,
        "arm" => 0x28,
        "x86" => 0x03,
        "riscv64" => 0xF3,
        _ => 0x3E,
    };
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

fn architecture_hote() -> &'static str {
    match std::env::consts::ARCH {
        a @ ("x86_64" | "aarch64" | "arm" | "x86" | "riscv64") => a,
        _ => "x86_64",
    }
}

fn controles() -> installation_sure::Controles {
    installation_sure::Controles {
        taille_attendue: None,
        place_libre: |_| Ok(u64::MAX),
        architecture: Some(architecture_hote()),
        essai: |_| Ok(()),
        cibles_web: |exe_dir| vec![exe_dir.join("web")],
        copier: installation_sure::copier_durablement,
        renommer: |de, vers| std::fs::rename(de, vers),
    }
}

fn ecrire(p: &Path, contenu: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, contenu).unwrap();
}

fn lire(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// `opt/` : l'installation rc12, avec le greffon livré `party` ancien et un
/// greffon `mien` installé par l'utilisateur ; `extraction/` : l'archive
/// rc13, avec `party` et `playlists-converter` neufs.
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
        ecrire(&opt.join("web/index.html"), "<html>rc12</html>");
        ecrire(&extraction.join("web/index.html"), "<html>rc13</html>");
        ecrire(
            &opt.join("plugins/party/manifest.json"),
            r#"{"id":"party","version":"1"}"#,
        );
        ecrire(&opt.join("plugins/party/main.wasm"), "party ancien");
        ecrire(&opt.join("plugins/mien/manifest.json"), r#"{"id":"mien"}"#);
        ecrire(&opt.join("plugins/mien/main.wasm"), "le mien");
        ecrire(
            &extraction.join("plugins/party/manifest.json"),
            r#"{"id":"party","version":"2"}"#,
        );
        ecrire(
            &extraction.join("plugins/party/main.wasm"),
            "party neuf, plus long",
        );
        ecrire(
            &extraction.join("plugins/playlists-converter/manifest.json"),
            r#"{"id":"playlists-converter"}"#,
        );
        ecrire(
            &extraction.join("plugins/playlists-converter/main.wasm"),
            "convertisseur",
        );
        std::fs::write(opt.join("tune-server"), b"#!/bin/sh\necho rc12\n").unwrap();
        std::fs::write(extraction.join("tune-server"), elf("tune-server neuf")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                opt.join("tune-server"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        Self {
            _racine: racine,
            opt,
            extraction,
        }
    }

    #[cfg(unix)]
    fn installer(&self, controles: &installation_sure::Controles) -> Result<(), String> {
        let binaire = self.opt.join("tune-server");
        super::super::install_unix(
            &binaire,
            &binaire,
            &self.extraction.join("tune-server"),
            &self.extraction,
            controles,
        )
    }

    fn noms_dans_plugins(&self) -> Vec<String> {
        let mut noms: Vec<String> = std::fs::read_dir(self.opt.join("plugins"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        noms.sort();
        noms
    }

    /// Les greffons sont ceux de la rc12, et rien ne traîne.
    fn greffons_anciens(&self) {
        assert_eq!(
            lire(&self.opt.join("plugins/party/main.wasm")),
            "party ancien"
        );
        assert_eq!(
            lire(&self.opt.join("plugins/party/manifest.json")),
            r#"{"id":"party","version":"1"}"#
        );
        assert_eq!(self.noms_dans_plugins(), ["mien", "party"]);
        assert_eq!(lire(&self.opt.join("plugins/mien/main.wasm")), "le mien");
        assert!(
            !self.opt.join(ATTENTE).exists(),
            "plugins.new traîne après un refus"
        );
    }
}

/// Le défaut de l'issue : après la mise à jour, `plugins/party` est le neuf
/// de l'archive, le greffon nouveau de l'archive est posé, celui de
/// l'utilisateur est intact, et l'ancien est gardé HORS de `plugins/`.
#[cfg(unix)]
#[test]
fn les_greffons_livres_neufs_remplacent_les_installes_8955() {
    let inst = Installation::nouvelle();

    inst.installer(&controles()).expect("installation");

    assert_eq!(
        lire(&inst.opt.join("plugins/party/main.wasm")),
        "party neuf, plus long",
        "le greffon livré n'a pas été mis à jour"
    );
    assert_eq!(
        lire(&inst.opt.join("plugins/party/manifest.json")),
        r#"{"id":"party","version":"2"}"#
    );
    assert_eq!(
        lire(&inst.opt.join("plugins/playlists-converter/main.wasm")),
        "convertisseur",
        "le greffon livré nouveau n'a pas été posé"
    );
    assert_eq!(
        lire(&inst.opt.join("plugins/mien/main.wasm")),
        "le mien",
        "le greffon de l'utilisateur a été touché"
    );
    // Rien d'autre dans `plugins/` : ni `.new` ni `.old` que le démarrage
    // lirait comme un second `party`.
    assert_eq!(
        inst.noms_dans_plugins(),
        ["mien", "party", "playlists-converter"]
    );
    assert_eq!(
        lire(&inst.opt.join("plugins.old/party/main.wasm")),
        "party ancien",
        "l'ancien n'est pas gardé"
    );
    assert!(!inst.opt.join(ATTENTE).exists(), "plugins.new traîne");
    assert_eq!(lire(&inst.opt.join("web/index.html")), "<html>rc13</html>");
}

/// `web/` échoue après la bascule des greffons : ils sont remis avec le
/// serveur, et le greffon nouveau est retiré.
#[cfg(unix)]
#[test]
fn un_echec_de_web_remet_aussi_les_greffons_8955() {
    let inst = Installation::nouvelle();
    let mut c = controles();
    c.renommer = |de, vers| {
        if de.file_name().is_some_and(|n| n == "web.new") {
            return Err(io::Error::other("refus simulé"));
        }
        std::fs::rename(de, vers)
    };

    let e = inst.installer(&c).unwrap_err();

    assert!(e.contains("restored"), "{e}");
    inst.greffons_anciens();
    assert_eq!(lire(&inst.opt.join("web/index.html")), "<html>rc12</html>");
}

/// Une copie ratée d'un greffon refuse toute la mise à jour avant de
/// toucher au binaire : rien n'a bougé.
#[cfg(unix)]
#[test]
fn une_copie_ratee_d_un_greffon_laisse_tout_comme_avant_8955() {
    let inst = Installation::nouvelle();
    let mut c = controles();
    c.copier = |de, vers| {
        if vers
            .to_string_lossy()
            .contains("plugins.new/playlists-converter")
        {
            return Err(io::Error::other("disque plein simulé"));
        }
        installation_sure::copier_durablement(de, vers)
    };

    let e = inst.installer(&c).unwrap_err();

    assert!(e.contains("plugins"), "{e}");
    inst.greffons_anciens();
    assert_eq!(
        std::fs::read(inst.opt.join("tune-server")).unwrap(),
        b"#!/bin/sh\necho rc12\n"
    );
}

/// Seuls les dossiers de l'archive qui portent un `manifest.json` sont des
/// greffons ; une cible qui est un lien symbolique est laissée.
#[cfg(unix)]
#[test]
fn seuls_les_greffons_de_l_archive_sont_retenus_8955() {
    let inst = Installation::nouvelle();
    ecrire(
        &inst.extraction.join("plugins/sans-manifeste/main.wasm"),
        "x",
    );
    ecrire(&inst.extraction.join("plugins/fichier-isole"), "x");
    let ailleurs = inst.extraction.parent().unwrap().join("party-a-moi");
    ecrire(&ailleurs.join("manifest.json"), "{}");
    std::fs::remove_dir_all(inst.opt.join("plugins/party")).unwrap();
    std::os::unix::fs::symlink(&ailleurs, inst.opt.join("plugins/party")).unwrap();

    let retenus: Vec<PathBuf> = a_installer(&inst.extraction, &inst.opt)
        .into_iter()
        .map(|g| g.cible)
        .collect();

    assert_eq!(retenus, [inst.opt.join("plugins/playlists-converter")]);
}
