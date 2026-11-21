//! #8944 (suite) — le rapport de bogue dit l'état des exécutables livrés.
//!
//! Dans le fil 2878 (rc12), le démon AirPlay 2 qui tournait datait d'avant
//! #8347 : la mise à jour intégrée ne l'avait jamais remplacé. Rien dans le
//! rapport ne le disait ; il a fallu le DÉDUIRE de la forme d'un message
//! d'erreur. `airplay-daemon` n'a pas d'option `--version`, et le lancer
//! depuis le rapport ferait tourner un processus de plus pour rien.
//!
//! Le rapport relève donc, pour `tune-server` et chaque nom de
//! [`EXECUTABLES_LIVRES`](super::update::compagnons::EXECUTABLES_LIVRES)
//! posé à côté de lui : la date de modification, la taille et une empreinte
//! SHA-256 courte (16 chiffres, comparable à celle du fichier de l'archive de release). Un
//! exécutable livré plus ancien que `tune-server` de plus d'un jour est
//! signalé : c'est exactement la signature d'un démon qui n'a pas suivi la
//! mise à jour. Pour `airplay-daemon`, le rapport dit aussi le chemin que le
//! serveur lancerait, quand ce n'est pas celui d'à côté (Docker,
//! `/usr/local/bin`, `PATH`).
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

/// Au-delà de cet écart avec `tune-server`, un exécutable livré est dit
/// « plus ancien » : une archive de release pose tous ses fichiers ensemble,
/// la mise à jour intégrée aussi (#8954).
const ECART_SIGNALE_SECS: u64 = 86_400;

/// Un fichier plus gros n'est pas haché : la date et la taille suffisent à
/// le situer. Au pire, le relevé lit 5 × 64 Mio par rapport (tune-server
/// compris), hors du fil asynchrone ; un `ffmpeg` statique plus gros que le
/// plafond n'a pas d'empreinte.
const EMPREINTE_MAX_OCTETS: u64 = 64 * 1024 * 1024;

/// Ce qu'on sait d'un exécutable sur le disque.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Releve {
    pub nom: String,
    pub chemin: PathBuf,
    /// `None` : absent (cible sans ce démon, ou retiré par l'utilisateur).
    pub fichier: Option<Fichier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fichier {
    pub taille: u64,
    pub modifie: Option<SystemTime>,
    /// Les 16 premiers caractères hexadécimaux du SHA-256. Pas 12 : douze
    /// chiffres hexadécimaux en fin de mot sont masqués comme une adresse MAC
    /// par `confidentialite::anonymiser_json` ; une suite plus longue reste.
    pub empreinte: Option<String>,
}

fn empreinte(chemin: &Path, taille: u64) -> Option<String> {
    if taille > EMPREINTE_MAX_OCTETS {
        return None;
    }
    let mut f = std::fs::File::open(chemin).ok()?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h).ok()?;
    let hex: String = h
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(hex)
}

/// Relève un fichier ordinaire (un lien est suivi : c'est ce qui sera lancé).
pub(crate) fn relever_un(nom: &str, chemin: &Path) -> Releve {
    let fichier = std::fs::metadata(chemin)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| Fichier {
            taille: m.len(),
            modifie: m.modified().ok(),
            empreinte: empreinte(chemin, m.len()),
        });
    Releve {
        nom: nom.to_string(),
        chemin: chemin.to_path_buf(),
        fichier,
    }
}

/// `tune-server` (le binaire en cours) puis chaque exécutable livré attendu
/// dans `exe_dir`, avec le suffixe de la plateforme.
pub(crate) fn relever(serveur: &Path, exe_dir: &Path, suffixe: &str) -> Vec<Releve> {
    let mut releves = vec![relever_un("tune-server", serveur)];
    for nom in super::update::compagnons::EXECUTABLES_LIVRES {
        releves.push(relever_un(nom, &exe_dir.join(format!("{nom}{suffixe}"))));
    }
    releves
}

fn date_utc(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn taille_lisible(octets: u64) -> String {
    if octets >= 1024 * 1024 {
        format!("{:.1} MiB", octets as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KiB", octets as f64 / 1024.0)
    }
}

/// L'écart, en jours entiers, dont `t` précède le serveur — `None` s'il ne
/// le précède pas de plus de [`ECART_SIGNALE_SECS`].
fn retard_en_jours(t: SystemTime, serveur: SystemTime) -> Option<u64> {
    let ecart = serveur.duration_since(t).ok()?.as_secs();
    (ecart > ECART_SIGNALE_SECS).then_some(ecart / 86_400)
}

/// La section Markdown du rapport. Le premier relevé est `tune-server`.
/// `demon_lance` : le chemin du démon AirPlay 2 que le serveur lance, s'il
/// diffère de celui d'à côté.
pub(crate) fn section_markdown(releves: &[Releve], demon_lance: Option<&Releve>) -> String {
    let serveur = releves
        .first()
        .and_then(|r| r.fichier.as_ref())
        .and_then(|f| f.modifie);
    let mut md = String::from("## Bundled executables\n");
    let ligne = |r: &Releve, nom: &str| -> String {
        match &r.fichier {
            None => format!("- {nom}: absent\n"),
            Some(f) => {
                let mut parties = vec![
                    f.modifie.map(date_utc).unwrap_or_else(|| "date ?".into()),
                    taille_lisible(f.taille),
                ];
                if let Some(e) = &f.empreinte {
                    parties.push(format!("sha256 {e}"));
                }
                let mut l = format!("- {nom}: {}", parties.join(", "));
                if nom != "tune-server"
                    && let (Some(t), Some(s)) = (f.modifie, serveur)
                    && let Some(jours) = retard_en_jours(t, s)
                {
                    l.push_str(&format!(
                        " — ⚠ older than tune-server by {jours} day(s): not updated with it?"
                    ));
                }
                l.push('\n');
                l
            }
        }
    };
    for r in releves {
        md.push_str(&ligne(r, &r.nom));
    }
    if let Some(d) = demon_lance {
        md.push_str(&format!(
            "- airplay-daemon the server would launch: {} (not the bundled one)\n",
            d.chemin.display()
        ));
        md.push_str(&ligne(d, "  that airplay-daemon"));
    }
    md.push('\n');
    md
}

/// Le pendant JSON de [`section_markdown`].
pub(crate) fn json(releves: &[Releve], demon_lance: Option<&Releve>) -> serde_json::Value {
    let un = |r: &Releve| {
        serde_json::json!({
            "name": r.nom,
            "path": r.chemin.to_string_lossy(),
            "present": r.fichier.is_some(),
            "size": r.fichier.as_ref().map(|f| f.taille),
            "modified": r.fichier.as_ref().and_then(|f| f.modifie).map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.to_rfc3339()
            }),
            "sha256_16": r.fichier.as_ref().and_then(|f| f.empreinte.clone()),
        })
    };
    serde_json::json!({
        "files": releves.iter().map(un).collect::<Vec<_>>(),
        "airplay_daemon_resolved": demon_lance.map(un),
    })
}

/// Le relevé complet, pour le rapport : binaire courant, son dossier, et le
/// démon AirPlay 2 que le serveur lancerait, s'il n'est pas celui d'à côté.
pub(crate) fn relever_l_installation() -> (Vec<Releve>, Option<Releve>) {
    let Ok(serveur) = std::env::current_exe() else {
        return (Vec::new(), None);
    };
    let Some(dir) = serveur.parent() else {
        return (Vec::new(), None);
    };
    let suffixe = std::env::consts::EXE_SUFFIX;
    let releves = relever(&serveur, dir, suffixe);
    let a_cote = dir.join(format!("airplay-daemon{suffixe}"));
    let demon_lance = tune_core::outputs::airplay2::chemin_du_demon_lance()
        .map(PathBuf::from)
        .filter(|p| {
            let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            canon(p) != canon(&a_cote)
        })
        .map(|p| relever_un("airplay-daemon", &p));
    (releves, demon_lance)
}

#[cfg(test)]
#[path = "executables_livres_8944_tests.rs"]
mod tests;
