//! #5141 — Sur macOS, la mise à jour remplace le paquet `Tune Server.app`
//! ENTIER, et plus seulement le binaire et `web/`.
//!
//! ## La cause
//!
//! Jusqu'à la v0.9.165, `update_install` téléchargeait le `.tar.gz` et
//! remplaçait `Contents/Resources/tune-server` et `Contents/Resources/web/` à
//! l'intérieur du paquet. Deux conséquences, mesurées sur un Mac Studio le
//! 26/09/2026 :
//!
//! - tout correctif qui vit dans le PAQUET (`Info.plist` de #4949, droits,
//!   lanceur) n'atteint jamais un Mac déjà installé ;
//! - le sceau du paquet est CASSÉ : `codesign --verify --deep --strict` rend
//!   « a sealed resource is missing or invalid », `file added:
//!   …/Resources/tune-server.old`, puis chaque fichier de `web/assets/`.
//!
//! ## Le mécanisme
//!
//! L'actif retenu est le **DMG** de la release, et non une nouvelle archive :
//!
//! - il existe pour TOUTES les releases déjà publiées, 0.9.165 comprise ;
//! - il figure dans le `SHA256SUMS` signé minisign (même vérification que le
//!   `.tar.gz`) ;
//! - il est notarisé ET agrafé : `spctl` le juge hors ligne. La release ne
//!   publie jamais un DMG non agrafé (`release.yml` le supprime).
//!
//! Le remplacement : DMG monté en lecture seule, sans Finder
//! (`hdiutil attach -nobrowse -readonly -noautoopen`), copie par `ditto` dans
//! un atelier À CÔTÉ du paquet (même volume), vérification de la COPIE
//! (`codesign --verify --deep --strict`, équipe du développeur, version,
//! binaire présent), puis deux renommages. En cas d'échec, l'ancien paquet
//! reste en place, intact.
//!
//! Ce module ne décide pas QUAND : c'est `update.rs` (mise à jour, et
//! réparation au démarrage).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use tracing::{error, info, warn};
use tune_core::updater::{ReleaseAsset, ReleaseInfo};

/// Réglage : ce que la dernière opération sur le paquet a fait, en JSON. Rendu
/// par `GET /system/update/status` pour vérification sur le terrain.
pub(crate) const CLE_ETAT_PAQUET: &str = "macos_paquet_etat";
/// Réglage : la version pour laquelle une réparation au démarrage a déjà été
/// TENTÉE. Une seule tentative par version, pour qu'un échec ne boucle pas.
pub(crate) const CLE_REPARATION_TENTEE: &str = "macos_paquet_reparation_tentee";

/// La clé sans laquelle macOS refuse le réseau local (#4949).
const CLE_RESEAU_LOCAL: &str = "NSLocalNetworkUsageDescription";

pub(crate) const MOTIF_SANS_RESEAU_LOCAL: &str = "info_plist_sans_reseau_local";
pub(crate) const MOTIF_SIGNATURE_INVALIDE: &str = "signature_du_paquet_invalide";
pub(crate) const MOTIF_VERSION_DIFFERENTE: &str = "version_du_paquet_differente";
pub(crate) const MOTIF_MISE_A_JOUR: &str = "mise_a_jour_du_paquet";
pub(crate) const MOTIF_HORS_PAQUET: &str = "hors_paquet_app";
pub(crate) const MOTIF_DMG_ABSENT: &str = "dmg_absent_de_la_release";

/// Le paquet `.app` qui contient cet exécutable, s'il y en a un.
///
/// Le DMG range le serveur dans `X.app/Contents/Resources/tune-server` ; un
/// exécutable dans `Contents/MacOS/` est aussi reconnu. Tout le reste
/// (`.tar.gz`, Homebrew, Linux) n'est pas un paquet.
pub(crate) fn paquet_de_l_executable(exe: &Path) -> Option<PathBuf> {
    let dossier = exe.parent()?;
    let nom = dossier.file_name()?.to_str()?;
    if nom != "Resources" && nom != "MacOS" {
        return None;
    }
    let contents = dossier.parent()?;
    if contents.file_name()?.to_str()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    if !app.extension()?.to_str()?.eq_ignore_ascii_case("app") {
        return None;
    }
    Some(app.to_path_buf())
}

/// Le DMG du SERVEUR pour cette architecture. Le moissonneur et les autres
/// binaires publiés sur la même release sont écartés par le préfixe, comme
/// pour l'archive (`PREFIXE_ARCHIVE_SERVEUR`).
pub(crate) fn actif_dmg<'a>(release: &'a ReleaseInfo, arch: &str) -> Option<&'a ReleaseAsset> {
    release.assets.iter().find(|a| {
        let nom = a.name.to_lowercase();
        if !nom.starts_with("tune-server") || !nom.ends_with(".dmg") || !nom.contains("macos") {
            return false;
        }
        match arch {
            "aarch64" => nom.contains("aarch64") || nom.contains("arm64"),
            "x86_64" => nom.contains("x86_64") || nom.contains("amd64"),
            _ => false,
        }
    })
}

/// Ce que le paquet dit de lui-même.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InfoPaquet {
    pub version: Option<String>,
    pub reseau_local: bool,
}

/// Lit un `Info.plist` au format XML (celui qu'écrit `release.yml`, ou la
/// sortie de `plutil -convert xml1`).
pub(crate) fn lire_info_plist_xml(xml: &str) -> InfoPaquet {
    fn chaine_apres_cle(xml: &str, cle: &str) -> Option<String> {
        let marque = format!("<key>{cle}</key>");
        let apres = &xml[xml.find(&marque)? + marque.len()..];
        let apres = apres.trim_start();
        let contenu = apres.strip_prefix("<string>")?;
        let fin = contenu.find("</string>")?;
        Some(contenu[..fin].trim().to_string())
    }
    InfoPaquet {
        version: chaine_apres_cle(xml, "CFBundleShortVersionString"),
        reseau_local: xml.contains(&format!("<key>{CLE_RESEAU_LOCAL}</key>")),
    }
}

fn version_normalisee(v: &str) -> &str {
    v.trim().trim_start_matches('v')
}

/// Pourquoi ce paquet ne correspond pas au binaire qui y tourne. Vide : le
/// paquet est cohérent, il n'y a rien à réparer.
pub(crate) fn motifs_de_reparation(
    info: &InfoPaquet,
    signature_valide: bool,
    version_binaire: &str,
) -> Vec<&'static str> {
    let mut motifs = Vec::new();
    if !info.reseau_local {
        motifs.push(MOTIF_SANS_RESEAU_LOCAL);
    }
    if !signature_valide {
        motifs.push(MOTIF_SIGNATURE_INVALIDE);
    }
    if info.version.as_deref().map(version_normalisee) != Some(version_normalisee(version_binaire))
    {
        motifs.push(MOTIF_VERSION_DIFFERENTE);
    }
    motifs
}

/// Ce que le démarrage fait du paquet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecisionDemarrage {
    /// Paquet cohérent avec son binaire.
    RienAFaire,
    /// Paquet incohérent : on le remplace par celui de la version du binaire.
    Reparer(Vec<&'static str>),
    /// Incohérent, mais une tentative a déjà eu lieu pour CETTE version. On
    /// n'insiste pas : un échec ne doit pas tourner en boucle de relances.
    DejaTentee(Vec<&'static str>),
}

pub(crate) fn decision_au_demarrage(
    motifs: Vec<&'static str>,
    tentee_pour: Option<&str>,
    version_binaire: &str,
) -> DecisionDemarrage {
    if motifs.is_empty() {
        return DecisionDemarrage::RienAFaire;
    }
    if tentee_pour.map(version_normalisee) == Some(version_normalisee(version_binaire)) {
        return DecisionDemarrage::DejaTentee(motifs);
    }
    DecisionDemarrage::Reparer(motifs)
}

/// La voie d'une mise à jour sur macOS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoieMiseAJour {
    /// Le paquet `.app` complet, depuis le DMG.
    Paquet { motif: &'static str },
    /// L'archive : binaire et `web/`, comme avant.
    BinaireSeul { motif: &'static str },
}

/// Une installation dans un paquet est TOUJOURS mise à jour par le paquet
/// complet quand le DMG est publié. Le motif retenu est le plus parlant : un
/// paquet sans la clé du réseau local se RÉPARE (#4949), même quand seule la
/// version du binaire change.
pub(crate) fn voie_de_mise_a_jour(
    dans_un_paquet: bool,
    dmg_publie: bool,
    motifs: &[&'static str],
) -> VoieMiseAJour {
    if !dans_un_paquet {
        return VoieMiseAJour::BinaireSeul {
            motif: MOTIF_HORS_PAQUET,
        };
    }
    if !dmg_publie {
        return VoieMiseAJour::BinaireSeul {
            motif: MOTIF_DMG_ABSENT,
        };
    }
    VoieMiseAJour::Paquet {
        motif: motifs.first().copied().unwrap_or(MOTIF_MISE_A_JOUR),
    }
}

/// Comment relancer le serveur une fois le paquet remplacé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Relance {
    /// Par le lanceur du paquet (`open`), pour que macOS rattache le nouveau
    /// processus au NOUVEAU paquet — son `Info.plist`, sa signature.
    Lanceur,
    /// `exec` sur place, comme avant : le processus a été lancé autrement que
    /// par le lanceur, et son environnement doit être conservé tel quel.
    ExecSurPlace,
}

/// Variables `TUNE_*` que le lanceur du paquet pose lui-même : leur présence
/// ne dit pas qu'on a lancé le serveur autrement.
const VARIABLES_DU_LANCEUR: &[&str] = &[
    "TUNE_PORT",
    "TUNE_WEB_DIR",
    "TUNE_OPEN_BROWSER",
    "TUNE_RELANCE_APRES_MAJ",
];

/// Le lanceur ne sait relancer que ce qu'il lance : port 8888, `web/` du
/// paquet, rien d'autre. Un processus lancé par launchd (étiquette propre dans
/// `XPC_SERVICE_NAME`) ou avec d'autres réglages `TUNE_*` (une base de test
/// dans `TUNE_DB_PATH`, un autre port…) serait relancé SANS eux, sur la vraie
/// base : celui-là garde l'`exec` sur place.
pub(crate) fn mode_de_relance(env: &[(String, String)]) -> Relance {
    for (cle, valeur) in env {
        if cle == "XPC_SERVICE_NAME" {
            let v = valeur.trim();
            if !v.is_empty() && v != "0" && !v.starts_with("application.") {
                return Relance::ExecSurPlace;
            }
        }
        if cle == "TUNE_PORT" && valeur.trim() != "8888" {
            return Relance::ExecSurPlace;
        }
        if cle.starts_with("TUNE_") && !VARIABLES_DU_LANCEUR.contains(&cle.as_str()) {
            return Relance::ExecSurPlace;
        }
    }
    Relance::Lanceur
}

/// Le script détaché qui attend la fin de CE processus, puis rouvre le paquet
/// par son lanceur. `TUNE_RELANCE_APRES_MAJ=1` dit au lanceur de ne pas ouvrir
/// un onglet de plus (#1236) ; un lanceur plus ancien l'ignore.
pub(crate) fn script_de_relance() -> &'static str {
    "i=0; while kill -0 \"$1\" 2>/dev/null && [ \"$i\" -lt 120 ]; do sleep 0.5; i=$((i+1)); done; \
     exec /usr/bin/open -n --env TUNE_RELANCE_APRES_MAJ=1 \"$2\""
}

/// Lance le script de relance, détaché. `Ok` : le processus courant peut
/// sortir, le lanceur prendra le relais.
pub(crate) fn armer_la_relance_par_le_lanceur(app: &Path, pid: u32) -> Result<(), String> {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(script_de_relance())
        .arg("tune-relance")
        .arg(pid.to_string())
        .arg(app)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("lancement du script de relance : {e}"))
}

// ───────────────────── Ce qui touche au système ─────────────────────

fn sortie(cmd: &mut Command) -> Result<(bool, String), String> {
    let out = cmd
        .output()
        .map_err(|e| format!("{:?} : {e}", cmd.get_program()))?;
    let mut texte = String::from_utf8_lossy(&out.stdout).into_owned();
    texte.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), texte.trim().to_string()))
}

/// Lit l'`Info.plist` du paquet, quel que soit son format (XML ou binaire).
pub(crate) fn info_du_paquet(app: &Path) -> Result<InfoPaquet, String> {
    let plist = app.join("Contents/Info.plist");
    let (ok, xml) = sortie(
        Command::new("/usr/bin/plutil")
            .args(["-convert", "xml1", "-o", "-"])
            .arg(&plist),
    )?;
    if !ok {
        return Err(format!("plutil {} : {xml}", plist.display()));
    }
    Ok(lire_info_plist_xml(&xml))
}

/// `codesign --verify --deep --strict` : le sceau du paquet ENTIER.
pub(crate) fn verifier_signature(app: &Path) -> Result<(), String> {
    let (ok, texte) = sortie(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app),
    )?;
    if ok {
        Ok(())
    } else {
        Err(texte.lines().take(3).collect::<Vec<_>>().join(" | "))
    }
}

/// L'équipe du développeur (`TeamIdentifier`) qui a signé ce chemin.
pub(crate) fn equipe_du_developpeur(chemin: &Path) -> Option<String> {
    let (_, texte) = sortie(Command::new("/usr/bin/codesign").arg("-dv").arg(chemin)).ok()?;
    texte
        .lines()
        .find_map(|l| l.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != "not set")
        .map(str::to_string)
}

/// Gatekeeper sur le DMG : notarisé Developer ID. Le DMG est agrafé, le
/// jugement se fait hors ligne.
pub(crate) fn verifier_notarisation_du_dmg(dmg: &Path) -> Result<String, String> {
    let (ok, texte) = sortie(
        Command::new("/usr/sbin/spctl")
            .args([
                "-a",
                "-vv",
                "-t",
                "open",
                "--context",
                "context:primary-signature",
            ])
            .arg(dmg),
    )?;
    let source = texte
        .lines()
        .find_map(|l| l.strip_prefix("source="))
        .unwrap_or("")
        .to_string();
    if ok {
        Ok(source)
    } else {
        Err(texte.lines().take(3).collect::<Vec<_>>().join(" | "))
    }
}

/// Ce que le nouveau paquet doit prouver avant d'être posé.
#[derive(Debug, Clone)]
pub(crate) struct Attentes {
    /// Version exigée dans `CFBundleShortVersionString`.
    pub version: String,
    /// Équipe du développeur exigée ; `None` sur un binaire non signé
    /// (construction locale), où il n'y a rien à comparer.
    pub equipe: Option<String>,
    /// Octets que le nouveau binaire doit contenir (garde PostgreSQL).
    pub marqueur_binaire: Option<&'static [u8]>,
}

fn atelier_pour(cible: &Path) -> Result<PathBuf, String> {
    let parent = cible
        .parent()
        .ok_or_else(|| format!("{} n'a pas de dossier parent", cible.display()))?;
    Ok(parent.join(format!(".tune-maj-paquet-{}", std::process::id())))
}

/// Copie `source` à côté de `cible`, vérifie la COPIE, puis la met à la place
/// de `cible` par deux renommages. Toute erreur laisse `cible` intacte.
///
/// Rend le compte rendu des vérifications, pour l'état de la mise à jour.
pub(crate) fn installer_paquet_verifie(
    source: &Path,
    cible: &Path,
    attentes: &Attentes,
) -> Result<Value, String> {
    let atelier = atelier_pour(cible)?;
    if atelier.exists() {
        std::fs::remove_dir_all(&atelier)
            .map_err(|e| format!("nettoyage de {} : {e}", atelier.display()))?;
    }
    std::fs::create_dir_all(&atelier)
        .map_err(|e| format!("création de {} : {e}", atelier.display()))?;
    let nom = cible
        .file_name()
        .ok_or_else(|| format!("{} n'a pas de nom", cible.display()))?;
    let neuf = atelier.join(nom);

    let resultat = preparer_et_verifier(source, &neuf, attentes);
    let verification = match resultat {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&atelier);
            return Err(e);
        }
    };

    // Deux renommages sur le même volume : à chaque instant, `cible` est
    // l'ancien paquet complet ou le nouveau complet.
    let ancien = atelier.join("ancien.app");
    let avait_un_ancien = cible.exists();
    if avait_un_ancien {
        if let Err(e) = std::fs::rename(cible, &ancien) {
            let _ = std::fs::remove_dir_all(&atelier);
            return Err(format!("mise de côté de l'ancien paquet : {e}"));
        }
    }
    if let Err(e) = std::fs::rename(&neuf, cible) {
        if avait_un_ancien {
            if let Err(e2) = std::fs::rename(&ancien, cible) {
                // Le seul état où l'ancien n'est plus à sa place : on le
                // laisse dans l'atelier, et on le dit.
                error!(
                    ancien = %ancien.display(),
                    error = %e2,
                    "macos_paquet_retour_arriere_impossible"
                );
                return Err(format!(
                    "pose du nouveau paquet : {e} ; retour arrière impossible ({e2}), l'ancien est dans {}",
                    ancien.display()
                ));
            }
        }
        let _ = std::fs::remove_dir_all(&atelier);
        return Err(format!("pose du nouveau paquet : {e}"));
    }
    // L'ancien paquet peut contenir l'image du processus qui tourne : macOS
    // garde l'inode ouvert, la suppression est sans effet sur lui.
    let _ = std::fs::remove_dir_all(&atelier);
    info!(cible = %cible.display(), "macos_paquet_remplace");
    Ok(verification)
}

fn preparer_et_verifier(source: &Path, neuf: &Path, attentes: &Attentes) -> Result<Value, String> {
    // `ditto` garde signatures, attributs étendus et liens : `cp -R` ne les
    // garantit pas.
    let (ok, texte) = sortie(Command::new("/usr/bin/ditto").arg(source).arg(neuf))?;
    if !ok {
        return Err(format!("copie du nouveau paquet : {texte}"));
    }
    verifier_signature(neuf).map_err(|e| format!("signature du nouveau paquet refusée : {e}"))?;
    let equipe = equipe_du_developpeur(neuf);
    if let Some(attendue) = &attentes.equipe {
        if equipe.as_deref() != Some(attendue.as_str()) {
            return Err(format!(
                "nouveau paquet signé par l'équipe {:?}, attendue {attendue}",
                equipe
            ));
        }
    }
    let info = info_du_paquet(neuf)?;
    if info.version.as_deref().map(version_normalisee)
        != Some(version_normalisee(&attentes.version))
    {
        return Err(format!(
            "le nouveau paquet porte la version {:?}, attendue {}",
            info.version, attentes.version
        ));
    }
    let binaire = neuf.join("Contents/Resources/tune-server");
    if !binaire.is_file() {
        return Err("le nouveau paquet ne contient pas Contents/Resources/tune-server".into());
    }
    if let Some(marqueur) = attentes.marqueur_binaire {
        if !super::update::file_contains_bytes(&binaire, marqueur) {
            return Err(
                "le binaire du nouveau paquet n'a pas la prise en charge PostgreSQL".into(),
            );
        }
    }
    Ok(json!({
        "codesign": "ok",
        "equipe": equipe,
        "version": info.version,
        "reseau_local": info.reseau_local,
    }))
}

/// Monte le DMG en lecture seule, sans Finder, et en installe le paquet.
pub(crate) fn remplacer_depuis_dmg(
    dmg: &Path,
    cible: &Path,
    attentes: &Attentes,
) -> Result<Value, String> {
    let notarisation = verifier_notarisation_du_dmg(dmg)
        .map_err(|e| format!("DMG refusé par Gatekeeper : {e}"))?;

    let montage = std::env::temp_dir().join(format!("tune-maj-dmg-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&montage);
    let (ok, texte) = sortie(
        Command::new("/usr/bin/hdiutil")
            .args([
                "attach",
                "-nobrowse",
                "-readonly",
                "-noautoopen",
                "-mountpoint",
            ])
            .arg(&montage)
            .arg(dmg),
    )?;
    if !ok {
        let _ = std::fs::remove_dir(&montage);
        return Err(format!("montage du DMG : {texte}"));
    }

    let resultat = (|| {
        let source = std::fs::read_dir(&montage)
            .map_err(|e| format!("lecture du DMG monté : {e}"))?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("app")))
            .ok_or_else(|| "aucun paquet .app dans le DMG".to_string())?;
        installer_paquet_verifie(&source, cible, attentes)
    })();

    let (detache, texte) = sortie(Command::new("/usr/bin/hdiutil").arg("detach").arg(&montage))
        .unwrap_or((false, String::new()));
    if !detache {
        warn!(sortie = %texte, "macos_dmg_detach_retry_force");
        let _ = sortie(
            Command::new("/usr/bin/hdiutil")
                .args(["detach", "-force"])
                .arg(&montage),
        );
    }
    let _ = std::fs::remove_dir(&montage);

    let mut verification = resultat?;
    verification["notarisation_dmg"] = json!(notarisation);
    Ok(verification)
}

/// L'état rendu par `/system/update/status` sous `macos_paquet`.
pub(crate) fn etat_du_paquet(
    bundle_remplace: bool,
    motif: &str,
    motifs: &[&str],
    origine: &str,
    version: &str,
    erreur: Option<&str>,
    verification: Option<&Value>,
) -> Value {
    json!({
        "bundle_remplace": bundle_remplace,
        "motif": motif,
        "motifs": motifs,
        "origine": origine,
        "version": version,
        "erreur": erreur,
        "verification": verification,
        "horodatage": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(noms: &[&str]) -> ReleaseInfo {
        ReleaseInfo {
            tag_name: "v0.9.166".into(),
            version: "0.9.166".into(),
            name: String::new(),
            body: String::new(),
            published_at: String::new(),
            html_url: String::new(),
            assets: noms
                .iter()
                .map(|n| ReleaseAsset {
                    name: (*n).into(),
                    browser_download_url: format!("https://example.invalid/{n}"),
                    size: 1,
                    content_type: String::new(),
                })
                .collect(),
        }
    }

    const ACTIFS_0_9_165: &[&str] = &[
        "SHA256SUMS",
        "SHA256SUMS.minisig",
        "moissonneur-roon-v0.9.165-macos-arm64.tar.gz",
        "tune-server-v0.9.165-linux-x86_64.tar.gz",
        "tune-server-v0.9.165-macos-aarch64.dmg",
        "tune-server-v0.9.165-macos-aarch64.tar.gz",
        "tune-server-v0.9.165-macos-x86_64.dmg",
        "tune-server-v0.9.165-macos-x86_64.tar.gz",
        "tune-server-v0.9.165-windows-x86_64-setup.exe",
    ];

    #[test]
    fn le_dmg_retenu_est_celui_du_serveur_et_de_l_architecture() {
        let r = release(ACTIFS_0_9_165);
        assert_eq!(
            actif_dmg(&r, "aarch64").map(|a| a.name.as_str()),
            Some("tune-server-v0.9.165-macos-aarch64.dmg")
        );
        assert_eq!(
            actif_dmg(&r, "x86_64").map(|a| a.name.as_str()),
            Some("tune-server-v0.9.165-macos-x86_64.dmg")
        );
        // Pas de DMG publié (notarisation refusée) : rien.
        let sans = release(&["tune-server-v0.9.165-macos-aarch64.tar.gz"]);
        assert!(actif_dmg(&sans, "aarch64").is_none());
        // Un DMG d'un autre binaire n'est jamais le paquet du serveur.
        let autre = release(&["moissonneur-roon-v0.9.165-macos-arm64.dmg"]);
        assert!(actif_dmg(&autre, "aarch64").is_none());
    }

    #[test]
    fn le_paquet_se_deduit_du_chemin_de_l_executable() {
        assert_eq!(
            paquet_de_l_executable(Path::new(
                "/Applications/Tune Server.app/Contents/Resources/tune-server"
            )),
            Some(PathBuf::from("/Applications/Tune Server.app"))
        );
        assert_eq!(
            paquet_de_l_executable(Path::new(
                "/Users/x/Apps/Tune.app/Contents/MacOS/tune-server"
            )),
            Some(PathBuf::from("/Users/x/Apps/Tune.app"))
        );
        // Archive, Homebrew, Linux : pas de paquet.
        assert_eq!(
            paquet_de_l_executable(Path::new("/opt/tune/tune-server")),
            None
        );
        assert_eq!(
            paquet_de_l_executable(Path::new(
                "/opt/homebrew/Cellar/tune-server/0.9.165/bin/tune-server"
            )),
            None
        );
        assert_eq!(
            paquet_de_l_executable(Path::new("/x/Resources/tune-server")),
            None,
            "un dossier Resources hors d'un Contents n'est pas un paquet"
        );
        assert_eq!(
            paquet_de_l_executable(Path::new("/x/Tune/Contents/Resources/tune-server")),
            None,
            "un Contents hors d'un .app n'est pas un paquet"
        );
    }

    /// L'`Info.plist` du 17/09 (avant #4949), tel qu'il est sur le Mac Studio.
    const PLIST_17_09: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Tune Server</string>
  <key>CFBundleShortVersionString</key><string>0.9.150</string>
  <key>CFBundleExecutable</key><string>Tune Server</string>
</dict>
</plist>"#;

    /// Celui de la 0.9.165, sortie de `plutil -convert xml1` (clé et valeur
    /// sur deux lignes).
    const PLIST_0_9_165: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>CFBundleShortVersionString</key>\n\t<string>0.9.165</string>\n\t<key>NSLocalNetworkUsageDescription</key>\n\t<string>Tune trouve…</string>\n</dict>\n</plist>\n";

    #[test]
    fn l_info_plist_dit_sa_version_et_la_cle_du_reseau_local() {
        assert_eq!(
            lire_info_plist_xml(PLIST_17_09),
            InfoPaquet {
                version: Some("0.9.150".into()),
                reseau_local: false
            }
        );
        assert_eq!(
            lire_info_plist_xml(PLIST_0_9_165),
            InfoPaquet {
                version: Some("0.9.165".into()),
                reseau_local: true
            }
        );
    }

    #[test]
    fn un_paquet_ancien_sous_un_binaire_neuf_se_repare() {
        let ancien = lire_info_plist_xml(PLIST_17_09);
        // Le cas du Mac Studio : Info.plist du 17/09, sceau cassé par
        // l'ancien programme de mise à jour, binaire 0.9.166.
        let motifs = motifs_de_reparation(&ancien, false, "0.9.166");
        assert_eq!(
            motifs,
            vec![
                MOTIF_SANS_RESEAU_LOCAL,
                MOTIF_SIGNATURE_INVALIDE,
                MOTIF_VERSION_DIFFERENTE
            ]
        );
        assert_eq!(
            decision_au_demarrage(motifs.clone(), None, "0.9.166"),
            DecisionDemarrage::Reparer(motifs.clone())
        );
        // Une tentative pour une AUTRE version ne compte pas.
        assert_eq!(
            decision_au_demarrage(motifs.clone(), Some("0.9.165"), "0.9.166"),
            DecisionDemarrage::Reparer(motifs.clone())
        );
        // Une seule tentative par version : pas de boucle.
        assert_eq!(
            decision_au_demarrage(motifs.clone(), Some("0.9.166"), "v0.9.166"),
            DecisionDemarrage::DejaTentee(motifs)
        );
    }

    #[test]
    fn un_paquet_coherent_ne_se_touche_pas() {
        let neuf = lire_info_plist_xml(PLIST_0_9_165);
        let motifs = motifs_de_reparation(&neuf, true, "0.9.165");
        assert!(motifs.is_empty());
        assert_eq!(
            decision_au_demarrage(motifs, None, "0.9.165"),
            DecisionDemarrage::RienAFaire
        );
        // Chaque écart, seul, suffit.
        assert_eq!(
            motifs_de_reparation(&neuf, false, "0.9.165"),
            vec![MOTIF_SIGNATURE_INVALIDE]
        );
        assert_eq!(
            motifs_de_reparation(&neuf, true, "0.9.166"),
            vec![MOTIF_VERSION_DIFFERENTE]
        );
    }

    #[test]
    fn la_mise_a_jour_passe_par_le_paquet_des_qu_elle_le_peut() {
        // Paquet sans la clé : réparé, même si seule la version change.
        assert_eq!(
            voie_de_mise_a_jour(
                true,
                true,
                &[MOTIF_SANS_RESEAU_LOCAL, MOTIF_SIGNATURE_INVALIDE]
            ),
            VoieMiseAJour::Paquet {
                motif: MOTIF_SANS_RESEAU_LOCAL
            }
        );
        // Paquet sain : remplacé quand même, la mise à jour EST le paquet.
        assert_eq!(
            voie_de_mise_a_jour(true, true, &[]),
            VoieMiseAJour::Paquet {
                motif: MOTIF_MISE_A_JOUR
            }
        );
        assert_eq!(
            voie_de_mise_a_jour(true, false, &[MOTIF_SANS_RESEAU_LOCAL]),
            VoieMiseAJour::BinaireSeul {
                motif: MOTIF_DMG_ABSENT
            }
        );
        assert_eq!(
            voie_de_mise_a_jour(false, true, &[]),
            VoieMiseAJour::BinaireSeul {
                motif: MOTIF_HORS_PAQUET
            }
        );
    }

    fn env(paires: &[(&str, &str)]) -> Vec<(String, String)> {
        paires
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn seul_un_processus_lance_par_le_lanceur_est_relance_par_lui() {
        // Ce que pose le lanceur AppleScript (et ce qu'a le Tune du Mac Studio).
        assert_eq!(
            mode_de_relance(&env(&[
                ("XPC_SERVICE_NAME", "0"),
                ("TUNE_PORT", "8888"),
                (
                    "TUNE_WEB_DIR",
                    "/Applications/Tune Server.app/Contents/Resources/web"
                ),
                ("HOME", "/Users/x"),
            ])),
            Relance::Lanceur
        );
        // Une base de test : la relancer par le lanceur la ferait tourner sur
        // la VRAIE base. Jamais.
        assert_eq!(
            mode_de_relance(&env(&[("TUNE_DB_PATH", "/tmp/t/tune.db")])),
            Relance::ExecSurPlace
        );
        assert_eq!(
            mode_de_relance(&env(&[("TUNE_PORT", "8896")])),
            Relance::ExecSurPlace
        );
        // Un service launchd : launchd le relance, pas nous.
        assert_eq!(
            mode_de_relance(&env(&[("XPC_SERVICE_NAME", "fr.mozaiklabs.tune")])),
            Relance::ExecSurPlace
        );
        assert_eq!(
            mode_de_relance(&env(&[(
                "XPC_SERVICE_NAME",
                "application.fr.mozaiklabs.tune-server.123.456"
            )])),
            Relance::Lanceur
        );
    }

    #[test]
    fn le_script_de_relance_attend_le_processus_puis_ouvre_le_paquet() {
        let s = script_de_relance();
        let attente = s
            .find("kill -0")
            .expect("le script n'attend plus la fin du processus");
        let ouverture = s
            .find("/usr/bin/open -n")
            .expect("le script n'ouvre plus le paquet");
        assert!(
            attente < ouverture,
            "le paquet doit s'ouvrir APRÈS la sortie de l'ancien"
        );
        assert!(s.contains("TUNE_RELANCE_APRES_MAJ=1"));
        assert!(
            s.contains("\"$2\""),
            "le chemin du paquet doit rester entre guillemets"
        );
    }

    /// Le témoin sur de VRAIS paquets (#5141). Ignoré par défaut : il lui faut
    /// un Mac, un DMG notarisé et une COPIE d'une app installée.
    ///
    /// ```sh
    /// TUNE_5141_DMG=~/Downloads/tune-server-v0.9.165-macos-aarch64.dmg \
    /// TUNE_5141_ANCIEN=/tmp/x/Tune\ Server.app TUNE_5141_VERSION=0.9.165 \
    /// TUNE_5141_EQUIPE=VV3696M7PL \
    /// cargo test -p tune-server paquet_macos::tests::sur_de_vrais_paquets -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn sur_de_vrais_paquets() {
        let dmg = PathBuf::from(std::env::var("TUNE_5141_DMG").expect("TUNE_5141_DMG"));
        let ancien = PathBuf::from(std::env::var("TUNE_5141_ANCIEN").expect("TUNE_5141_ANCIEN"));
        let version = std::env::var("TUNE_5141_VERSION").expect("TUNE_5141_VERSION");
        let equipe = std::env::var("TUNE_5141_EQUIPE").ok();
        assert!(
            !ancien.starts_with("/Applications"),
            "le témoin ne travaille que sur une COPIE"
        );

        // L'état de départ, par la logique du démarrage.
        let info = info_du_paquet(&ancien).unwrap();
        let sig = verifier_signature(&ancien);
        let motifs = motifs_de_reparation(&info, sig.is_ok(), &version);
        println!("AVANT : info={info:?} signature={sig:?} motifs={motifs:?}");
        let DecisionDemarrage::Reparer(motifs) = decision_au_demarrage(motifs, None, &version)
        else {
            panic!("l'ancien paquet devrait être à réparer");
        };
        assert!(motifs.contains(&MOTIF_SANS_RESEAU_LOCAL));

        let attentes = Attentes {
            version: version.clone(),
            equipe,
            marqueur_binaire: None,
        };
        let verification = remplacer_depuis_dmg(&dmg, &ancien, &attentes).expect("remplacement");
        println!("VÉRIFICATION : {verification}");

        let apres = info_du_paquet(&ancien).unwrap();
        println!("APRÈS : info={apres:?}");
        assert!(
            apres.reseau_local,
            "la clé NSLocalNetworkUsageDescription manque encore"
        );
        assert_eq!(apres.version.as_deref(), Some(version.as_str()));
        verifier_signature(&ancien).expect("codesign --verify --deep --strict après remplacement");
        assert!(
            motifs_de_reparation(&apres, true, &version).is_empty(),
            "au démarrage suivant, plus rien à réparer"
        );
    }

    /// Contre-épreuve : un nouveau paquet au sceau cassé est REFUSÉ, et
    /// l'ancien reste octet pour octet ce qu'il était.
    ///
    /// `TUNE_5141_CASSE` : copie du paquet du DMG avec un octet modifié.
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn un_paquet_au_sceau_casse_est_refuse() {
        let casse = PathBuf::from(std::env::var("TUNE_5141_CASSE").expect("TUNE_5141_CASSE"));
        let ancien = PathBuf::from(std::env::var("TUNE_5141_ANCIEN").expect("TUNE_5141_ANCIEN"));
        let version = std::env::var("TUNE_5141_VERSION").expect("TUNE_5141_VERSION");
        assert!(!ancien.starts_with("/Applications"));
        let empreinte = |p: &Path| {
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg("cd \"$1\" && find . -type f -print0 | sort -z | xargs -0 shasum | shasum")
                .arg("x")
                .arg(p)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let avant = empreinte(&ancien);
        let attentes = Attentes {
            version,
            equipe: None,
            marqueur_binaire: None,
        };
        let refus = installer_paquet_verifie(&casse, &ancien, &attentes)
            .expect_err("un paquet au sceau cassé doit être refusé");
        println!("REFUS : {refus}");
        assert!(
            refus.contains("signature du nouveau paquet refusée"),
            "{refus}"
        );
        assert_eq!(empreinte(&ancien), avant, "l'ancien paquet a été modifié");
        assert!(
            !atelier_pour(&ancien).unwrap().exists(),
            "l'atelier doit être nettoyé après un refus"
        );
    }
}
