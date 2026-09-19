use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::info;

/// Motif rendu à l'utilisateur pour un ISO SACD que Tune n'a pas su extraire.
///
/// Il quitte volontairement le vocabulaire du journal : `sacd_extract` ne dit
/// rien à qui n'a jamais entendu parler de cet outil, et c'est justement à
/// cette personne que le rapport de scan s'adresse (#2992).
pub const MOTIF_ISO_SACD_NON_EXTRAIT: &str =
    "ISO SACD : extraction impossible (sacd_extract, outil externe non fourni avec Tune)";

/// Motif rendu à l'utilisateur pour un `.iso` qui n'est pas un SACD du tout.
pub const MOTIF_ISO_SANS_ZONE_SACD: &str =
    "image ISO sans zone SACD : ce fichier n'est pas de l'audio";

/// Clé de rapport des ISO SACD dont l'extraction a échoué.
pub const CLE_RAPPORT_ISO_SACD: &str = "iso-sacd";

/// Clé de rapport des `.iso` qui ne portent pas de zone SACD.
pub const CLE_RAPPORT_ISO_DONNEES: &str = "iso";

/// Extract DSF tracks from a SACD ISO file using sacd_extract.
/// Returns the paths of the extracted DSF files in a temp directory.
pub fn extract_iso_to_dsf(iso_path: &Path) -> Result<Vec<PathBuf>, String> {
    let sacd_extract =
        find_sacd_extract().ok_or("sacd_extract not found — install it for ISO SACD support")?;

    let output_dir = iso_path.with_extension("sacd_extract");
    std::fs::create_dir_all(&output_dir).map_err(|e| format!("create dir: {e}"))?;

    let output = Command::new(&sacd_extract)
        .args([
            "-i",
            &iso_path.to_string_lossy(),
            "-s", // stereo extraction
            "-p", // DSF output
            "-o",
            &output_dir.to_string_lossy(),
        ])
        .output()
        .map_err(|e| format!("sacd_extract exec: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("sacd_extract failed: {stderr}"));
    }

    let dsf_files: Vec<PathBuf> = std::fs::read_dir(&output_dir)
        .map_err(|e| format!("read dir: {e}"))?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension().is_some_and(|ext| ext == "dsf") {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    info!(
        iso = %iso_path.display(),
        tracks = dsf_files.len(),
        "sacd_iso_extracted"
    );

    Ok(dsf_files)
}

/// Secteur logique du Master TOC d'un disque SACD, en octets.
///
/// Le Master TOC occupe le LSN 510 d'une image SACD ; un secteur fait 0x800
/// octets. La signature `SACDMTOC` s'y trouve à l'octet zéro.
pub(crate) const DECALAGE_MASTER_TOC_SACD: u64 = 0x800 * 510;

/// Signature du Master TOC d'un disque SACD.
pub(crate) const SIGNATURE_MASTER_TOC_SACD: &[u8; 8] = b"SACDMTOC";

/// Dit si ce chemin est réellement une image SACD, signature à l'appui.
///
/// Le commentaire de cette fonction annonçait le contrôle de `SACDMTOC` depuis
/// l'origine — et la fonction ne le faisait pas : elle se contentait de
/// « extension `.iso` **et** taille > 4 Mo ». Toute image de données y passait,
/// jusqu'à `ubuntu-26.04-desktop-amd64.iso` soumis à l'extraction SACD dans le
/// journal de JeromeQ (#2992).
///
/// Le coût reste celui d'une énumération : un `open` + un `seek` + huit octets
/// lus, et **seulement sur les `.iso`**, là où le code précédent payait déjà un
/// `stat` puis jusqu'à quatre créations de processus par fichier. Sur une
/// bibliothèque sans ISO, rien ne change.
pub fn is_sacd_iso(path: &Path) -> bool {
    let est_iso = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("iso"));
    if !est_iso {
        return false;
    }
    let Ok(mut fichier) = std::fs::File::open(path) else {
        return false;
    };
    if fichier
        .seek(SeekFrom::Start(DECALAGE_MASTER_TOC_SACD))
        .is_err()
    {
        return false;
    }
    let mut signature = [0u8; SIGNATURE_MASTER_TOC_SACD.len()];
    // `read_exact` échoue proprement sur une image trop courte pour porter un
    // Master TOC : pas besoin de tester la taille séparément.
    fichier.read_exact(&mut signature).is_ok() && &signature == SIGNATURE_MASTER_TOC_SACD
}

/// Motif qui interdit la LECTURE de ce chemin, ou `None` si Tune sait le rendre.
///
/// Le parcours de bibliothèque refuse déjà les `.iso` qu'il n'a pas su extraire,
/// et il les NOMME dans son rapport (#2992). La demande de LECTURE, elle, n'avait
/// aucune garde : une ligne `tracks` qui pointe encore un `.iso` — parcours
/// antérieur à ce correctif, base restaurée, ajout hors parcours — traversait
/// toute la résolution sans un mot. `AudioFormat::from_extension("iso")` rend
/// `None`, le transcodage retombe alors sur `unwrap_or(AudioFormat::Flac)`, et la
/// sortie reçoit une image disque à la place d'un flux : la zone reste muette et
/// rien à l'écran ne l'explique (#3234, JeromeQ, fil 1206).
///
/// Le chemin est un PARAMÈTRE et la décision ne lit ni la zone, ni la
/// plateforme, ni un réglage : elle est éprouvable telle quelle.
///
/// Ne coûte rien hors `.iso` : l'extension est testée avant toute ouverture, et
/// un FLAC ou un DSF ressort par le `return None` sans qu'un octet soit lu.
pub fn refus_de_lecture(path: &Path) -> Option<&'static str> {
    let est_iso = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("iso"));
    if !est_iso {
        return None;
    }
    // Les deux motifs sont ceux du rapport de parcours, mot pour mot : un
    // utilisateur qui lit « extraction impossible » dans son rapport de scan
    // doit lire la MÊME phrase quand il clique sur l'album, sans quoi il croit
    // à deux défauts distincts.
    Some(if is_sacd_iso(path) {
        MOTIF_ISO_SACD_NON_EXTRAIT
    } else {
        MOTIF_ISO_SANS_ZONE_SACD
    })
}

const SACD_EXTRACT_CANDIDATES: [&str; 4] = [
    "sacd_extract",
    "/usr/local/bin/sacd_extract",
    "/usr/bin/sacd_extract",
    "/opt/homebrew/bin/sacd_extract",
];

fn usable_probe_output(output: &std::process::Output) -> bool {
    output.status.success() || !output.stdout.is_empty() || !output.stderr.is_empty()
}

fn find_sacd_extract() -> Option<PathBuf> {
    for name in &SACD_EXTRACT_CANDIDATES {
        if let Ok(output) = Command::new(name).arg("--help").output() {
            if usable_probe_output(&output) {
                return Some(PathBuf::from(name));
            }
        }
    }
    None
}

/// Availability of the ISO extractor, using the extraction path's candidates
/// and --help criterion. This says nothing about a particular disc or file.
/// Unlike extraction, an HTTP diagnostic must finish promptly (#3234).
pub async fn sacd_extract_available() -> Result<bool, String> {
    probe_candidates(&SACD_EXTRACT_CANDIDATES, std::time::Duration::from_secs(2)).await
}

async fn probe_candidates(
    candidates: &[&str],
    budget: std::time::Duration,
) -> Result<bool, String> {
    let deadline = tokio::time::Instant::now() + budget;
    for name in candidates {
        let mut command = tokio::process::Command::new(name);
        command.arg("--help").kill_on_drop(true);
        match tokio::time::timeout_at(deadline, command.output()).await {
            Ok(Ok(output)) if usable_probe_output(&output) => return Ok(true),
            Ok(_) => {}
            Err(_) => return Err("SACD ISO extractor probe timed out".into()),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_sacd_extract_if_available() {
        // This test only passes if sacd_extract is installed
        let result = find_sacd_extract();
        if result.is_some() {
            println!("sacd_extract found at: {:?}", result.unwrap());
        } else {
            println!("sacd_extract not installed (test skipped)");
        }
    }

    #[test]
    fn is_sacd_iso_checks_extension() {
        assert!(!is_sacd_iso(Path::new("/tmp/test.flac")));
        assert!(!is_sacd_iso(Path::new("/tmp/test.dsf")));
    }

    /// Écrit une image creuse dont le Master TOC porte `signature`.
    ///
    /// Taille portée au-delà de 4 Mo à dessein : c'est le seuil du contrôle
    /// d'origine. Une fixture plus petite ferait échouer la contre-épreuve pour
    /// une autre raison que le défaut visé.
    fn image_iso(dossier: &Path, nom: &str, signature: Option<&[u8; 8]>) -> PathBuf {
        use std::io::Write;
        let chemin = dossier.join(nom);
        let mut fichier = std::fs::File::create(&chemin).unwrap();
        // Fichier creux : aucun octet réel n'est écrit avant le décalage, le
        // test ne consomme donc pas 4 Mo sur disque.
        fichier
            .seek(SeekFrom::Start(DECALAGE_MASTER_TOC_SACD))
            .unwrap();
        fichier
            .write_all(signature.map(|s| &s[..]).unwrap_or(b"........"))
            .unwrap();
        fichier.set_len(4_200_000).unwrap();
        fichier.flush().unwrap();
        chemin
    }

    /// Le contrôle annoncé par le commentaire est enfin celui qui est fait
    /// (#2992) : une image de données de plus de 4 Mo n'est pas un SACD.
    #[test]
    fn seule_la_signature_sacdmtoc_designe_un_sacd() {
        let dossier = tempfile::tempdir().unwrap();

        let sacd = image_iso(dossier.path(), "album.iso", Some(SIGNATURE_MASTER_TOC_SACD));
        assert!(
            is_sacd_iso(&sacd),
            "une image portant SACDMTOC au LSN 510 est un SACD"
        );

        // Le cas exact du journal de JeromeQ : une image d'installation Ubuntu,
        // grosse et parfaitement innocente, soumise à l'extraction SACD.
        let ubuntu = image_iso(dossier.path(), "ubuntu-26.04-desktop-amd64.iso", None);
        assert!(
            !is_sacd_iso(&ubuntu),
            "l'ancien contrôle (extension + taille > 4 Mo) prenait cette image \
             de données pour un disque SACD"
        );

        // Une image trop courte pour porter un Master TOC ne fait pas paniquer
        // la lecture : elle est simplement refusée.
        let tronquee = dossier.path().join("tronquee.iso");
        std::fs::write(&tronquee, b"pas assez long").unwrap();
        assert!(!is_sacd_iso(&tronquee));

        // L'extension reste testée sans tenir compte de la casse, comme le
        // fait le parcours de bibliothèque qui appelle cette fonction.
        let majuscules = image_iso(dossier.path(), "ALBUM.ISO", Some(SIGNATURE_MASTER_TOC_SACD));
        assert!(is_sacd_iso(&majuscules));
    }
}

#[cfg(all(test, unix))]
mod iso_status_tests_3234 {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn tool(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/usr/bin/python3\nimport sys, time, os\nassert sys.argv[1:] == ['--help']\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[tokio::test]
    async fn iso_status_accepts_the_same_help_output_as_extraction() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("success", "sys.exit(0)"),
            ("help", "print('help', file=sys.stderr); sys.exit(1)"),
        ] {
            let path = tool(dir.path(), name, body);
            assert!(
                probe_candidates(&[path.to_str().unwrap()], Duration::from_secs(1))
                    .await
                    .unwrap()
            );
            assert!(usable_probe_output(
                &Command::new(&path).arg("--help").output().unwrap()
            ));
        }
    }

    #[tokio::test]
    async fn iso_status_ignores_missing_and_silent_failed_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let failed = tool(dir.path(), "failed", "sys.exit(1)");
        let candidates = [missing.to_str().unwrap(), failed.to_str().unwrap()];
        assert!(
            !probe_candidates(&candidates, Duration::from_secs(1))
                .await
                .unwrap()
        );
        let found = tool(dir.path(), "found", "print('help')");
        assert!(
            probe_candidates(
                &[candidates[0], candidates[1], found.to_str().unwrap()],
                Duration::from_secs(1)
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn iso_status_timeout_is_unknown_and_kills_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let path = tool(
            dir.path(),
            "slow",
            &format!(
                "open({:?}, 'w').write(str(os.getpid()))\ntime.sleep(30)",
                pidfile.to_str().unwrap()
            ),
        );
        let start = std::time::Instant::now();
        let result = probe_candidates(&[path.to_str().unwrap()], Duration::from_secs(1)).await;
        assert!(
            result.is_err(),
            "a timeout must not claim the tool is absent"
        );
        assert!(start.elapsed() < Duration::from_secs(5));
        #[cfg(target_os = "linux")]
        {
            let pid = std::fs::read_to_string(&pidfile).unwrap();
            for _ in 0..30 {
                if !Path::new(&format!("/proc/{pid}")).exists() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("timed-out probe still present: {pid}");
        }
    }
}
