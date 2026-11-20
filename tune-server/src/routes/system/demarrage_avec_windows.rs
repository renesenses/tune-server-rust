//! #8054 — « Démarrer Tune avec Windows » : `GET` / `PUT
//! /api/v1/system/autostart`.
//!
//! Fil forum 2850 (sashasha), fil VNAV : après l'installation, au premier
//! redémarrage du PC, Tune ne repartait pas et l'interface affichait
//! « serveur hors ligne ». L'installeur ne posait AUCUN démarrage automatique :
//! ni service, ni tâche planifiée, ni clé `Run`, ni raccourci dans le dossier
//! Démarrage.
//!
//! # Pourquoi une valeur `HKCU\…\Run`, et pas un service ni une tâche planifiée
//!
//! * **La session de l'utilisateur.** WASAPI (exclusif surtout) et la plupart
//!   des pilotes ASIO veulent tourner dans la session de l'utilisateur
//!   connecté. Un service vit dans la session 0 : pas de périphérique audio
//!   « par défaut » de l'utilisateur, pilotes ASIO souvent absents ou refusés.
//! * **Sans droits administrateur.** L'installeur est par utilisateur
//!   (`%LOCALAPPDATA%\Programs`, `RequestExecutionLevel user`) pour que la mise
//!   à jour intégrée remplace l'exécutable sans élévation. Un service exige
//!   l'administrateur ; `schtasks /SC ONLOGON` aussi (« Accès refusé » pour un
//!   compte standard). `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//!   s'écrit sans élévation, depuis l'installeur comme depuis le serveur.
//! * **Visible et réversible par Windows lui-même.** L'entrée apparaît dans
//!   Gestionnaire des tâches ▸ Applications de démarrage, où l'utilisateur peut
//!   la désactiver. Windows note ce choix à part, sous
//!   `…\Explorer\StartupApproved\Run` : un premier octet impair veut dire
//!   « désactivée ». [`etat`] en tient compte, sinon les Réglages diraient
//!   « activé » à tort ; activer depuis les Réglages efface ce marqueur.
//!
//! La commande lancée est `"<dossier>\tune-server.exe" --sans-console`
//! (#6253) : le serveur se détache sans fenêtre, n'ouvre PAS de navigateur
//! (`TUNE_OPEN_BROWSER` n'est pas posé), trouve son interface web à côté de
//! l'exécutable (`config::resolve_web_dir`) et sa base sous
//! `%LOCALAPPDATA%\TuneServer` — exactement ce que fait le raccourci du Bureau.
//!
//! L'installeur (`release.yml`, script NSIS) pose la même valeur, sous le
//! même nom [`NOM_DE_LA_VALEUR`], derrière une case « Démarrer Tune avec
//! Windows » cochée par défaut, et la retire à la désinstallation. Le témoin
//! `installeur_tests` garde les deux côtés alignés.
//!
//! Hors Windows, la route répond `supported: false` et refuse l'écriture :
//! Linux a son unité systemd, macOS son LaunchAgent.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::state::AppState;

/// La clé `Run` de l'utilisateur courant.
pub(crate) const CLE_RUN: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// Où Windows note « désactivée dans le Gestionnaire des tâches ».
pub(crate) const CLE_APPROBATION: &str =
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// Le nom de la valeur, le même dans l'installeur NSIS.
pub(crate) const NOM_DE_LA_VALEUR: &str = "Tune Server";

/// L'argument passé au serveur lancé à l'ouverture de session.
pub(crate) const ARGUMENT: &str = crate::lancement_sans_console::DRAPEAU;

/// La commande enregistrée sous la clé `Run` pour cet exécutable.
pub(crate) fn commande_de_demarrage(exe: &str) -> String {
    format!("\"{exe}\" {ARGUMENT}")
}

/// La donnée d'une valeur dans la sortie de `reg query <clé> /v <nom>`.
///
/// `reg` aligne ses colonnes par blocs de QUATRE espaces :
/// `    Tune Server    REG_SZ    "C:\…\tune-server.exe" --sans-console`.
/// Le nom de la valeur contient lui-même une espace : on ne découpe donc pas
/// sur les blancs, on cherche `    <nom>    <type>    `.
pub(crate) fn donnee_de_reg_query(sortie: &str, nom: &str) -> Option<(String, String)> {
    for ligne in sortie.lines() {
        let Some(reste) = ligne.trim_start().strip_prefix(nom) else {
            continue;
        };
        let Some(reste) = reste.strip_prefix("    ") else {
            continue;
        };
        let (type_, donnee) = match reste.split_once("    ") {
            Some((t, d)) => (t, d),
            // Une donnée vide : `reg` n'écrit que le type.
            None => (reste.trim_end(), ""),
        };
        if !type_.starts_with("REG_") {
            continue;
        }
        return Some((type_.to_string(), donnee.trim_end().to_string()));
    }
    None
}

/// L'entrée a-t-elle été désactivée dans le Gestionnaire des tâches ?
///
/// `StartupApproved\Run` garde une valeur `REG_BINARY` de 12 octets : un
/// premier octet PAIR (`02`, `06`) = activée, IMPAIR (`03`, `07`) = désactivée.
/// Sans valeur, Windows considère l'entrée comme activée.
pub(crate) fn desactivee_par_windows(donnee_binaire: Option<&str>) -> bool {
    let Some(hex) = donnee_binaire else {
        return false;
    };
    let Some(premier) = hex.get(0..2) else {
        return false;
    };
    u8::from_str_radix(premier, 16).is_ok_and(|o| o & 1 == 1)
}

/// L'état tel que les Réglages l'affichent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Etat {
    /// La valeur `Run` existe et vise CET exécutable.
    pub inscrit: bool,
    /// … mais l'utilisateur l'a désactivée dans le Gestionnaire des tâches.
    pub desactive_par_windows: bool,
    /// La commande trouvée sous `Run`, s'il y en a une.
    pub commande: Option<String>,
}

impl Etat {
    /// Tune repartira-t-il à la prochaine ouverture de session ?
    pub fn actif(&self) -> bool {
        self.inscrit && !self.desactive_par_windows
    }
}

/// L'état, à partir des deux sorties de `reg query` (vides si la valeur est
/// absente) et de l'exécutable courant.
///
/// Une valeur `Tune Server` qui vise un AUTRE exécutable (ancienne copie
/// portable, dossier déplacé) ne compte pas comme inscrite : elle lancerait un
/// autre serveur que celui-ci, et la réactiver la remplace.
pub(crate) fn etat(sortie_run: &str, sortie_approbation: &str, exe: &str) -> Etat {
    let commande = donnee_de_reg_query(sortie_run, NOM_DE_LA_VALEUR).map(|(_, d)| d);
    let inscrit = commande
        .as_deref()
        .is_some_and(|c| vise_cet_executable(c, exe));
    let approbation = donnee_de_reg_query(sortie_approbation, NOM_DE_LA_VALEUR).map(|(_, d)| d);
    Etat {
        inscrit,
        desactive_par_windows: desactivee_par_windows(approbation.as_deref()),
        commande,
    }
}

/// La commande lance-t-elle `exe` ? Comparaison insensible à la casse : les
/// chemins Windows le sont.
fn vise_cet_executable(commande: &str, exe: &str) -> bool {
    let commande = commande.trim();
    let chemin = match commande.strip_prefix('"') {
        Some(reste) => reste.split('"').next().unwrap_or(""),
        None => commande.split(' ').next().unwrap_or(""),
    };
    chemin.eq_ignore_ascii_case(exe)
}

/// Les arguments de `reg.exe` pour activer (deux commandes : inscrire, puis
/// effacer un éventuel « désactivée » du Gestionnaire des tâches) ou
/// désactiver (retirer la valeur et son marqueur).
pub(crate) fn commandes_reg(activer: bool, exe: &str) -> Vec<Vec<String>> {
    let s = |x: &str| x.to_string();
    let effacer_approbation = vec![
        s("delete"),
        s(CLE_APPROBATION),
        s("/v"),
        s(NOM_DE_LA_VALEUR),
        s("/f"),
    ];
    if activer {
        vec![
            vec![
                s("add"),
                s(CLE_RUN),
                s("/v"),
                s(NOM_DE_LA_VALEUR),
                s("/t"),
                s("REG_SZ"),
                s("/d"),
                commande_de_demarrage(exe),
                s("/f"),
            ],
            effacer_approbation,
        ]
    } else {
        vec![
            vec![
                s("delete"),
                s(CLE_RUN),
                s("/v"),
                s(NOM_DE_LA_VALEUR),
                s("/f"),
            ],
            effacer_approbation,
        ]
    }
}

fn charge_utile(supporte: bool, etat: Option<&Etat>) -> Value {
    json!({
        "supported": supporte,
        "enabled": etat.is_some_and(Etat::actif),
        "registered": etat.is_some_and(|e| e.inscrit),
        "disabled_by_windows": etat.is_some_and(|e| e.desactive_par_windows),
        "command": etat.and_then(|e| e.commande.clone()),
    })
}

#[cfg(target_os = "windows")]
fn reg(args: &[String]) -> Result<std::process::Output, String> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("reg")
        .args(args)
        .creation_flags(crate::lancement_sans_console::CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "windows")]
fn executable() -> Result<String, String> {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "windows")]
fn lire_etat() -> Result<Etat, String> {
    let exe = executable()?;
    let lire = |cle: &str| -> Result<String, String> {
        let sortie = reg(&[
            "query".into(),
            cle.into(),
            "/v".into(),
            NOM_DE_LA_VALEUR.into(),
        ])?;
        // Valeur absente : code 1 et sortie vide — c'est « non inscrit ».
        Ok(String::from_utf8_lossy(&sortie.stdout).into_owned())
    };
    Ok(etat(&lire(CLE_RUN)?, &lire(CLE_APPROBATION)?, &exe))
}

#[cfg(target_os = "windows")]
fn ecrire(activer: bool) -> Result<Etat, String> {
    let exe = executable()?;
    for (i, args) in commandes_reg(activer, &exe).iter().enumerate() {
        let sortie = reg(args)?;
        // Seule la PREMIÈRE commande doit réussir : le marqueur
        // `StartupApproved` est le plus souvent absent, et `reg delete` d'une
        // valeur absente échoue. Désactiver une entrée déjà absente non plus
        // n'est pas une erreur.
        if i == 0 && !sortie.status.success() && activer {
            return Err(String::from_utf8_lossy(&sortie.stderr).trim().to_string());
        }
    }
    lire_etat()
}

#[cfg(not(target_os = "windows"))]
fn lire_etat() -> Result<Etat, String> {
    Err("unsupported".into())
}

#[cfg(not(target_os = "windows"))]
fn ecrire(_activer: bool) -> Result<Etat, String> {
    Err("unsupported".into())
}

/// `GET /system/autostart`.
pub(super) async fn get_autostart() -> Response {
    if !cfg!(target_os = "windows") {
        return Json(charge_utile(false, None)).into_response();
    }
    match tokio::task::spawn_blocking(lire_etat).await {
        Ok(Ok(e)) => Json(charge_utile(true, Some(&e))).into_response(),
        Ok(Err(e)) => erreur(
            StatusCode::INTERNAL_SERVER_ERROR,
            "autostart_read_failed",
            &e,
        ),
        Err(e) => erreur(
            StatusCode::INTERNAL_SERVER_ERROR,
            "autostart_read_failed",
            &e.to_string(),
        ),
    }
}

#[derive(serde::Deserialize)]
pub(super) struct Corps {
    enabled: bool,
}

/// `PUT /system/autostart` — `{"enabled": true|false}`. Réservé à
/// l'administrateur. La commande inscrite ne vient JAMAIS du corps : c'est
/// toujours l'exécutable en cours, avec `--sans-console`.
pub(super) async fn set_autostart(
    _admin: crate::auth::RequireAdmin,
    State(_state): State<AppState>,
    Json(corps): Json<Corps>,
) -> Response {
    if !cfg!(target_os = "windows") {
        return erreur(
            StatusCode::CONFLICT,
            "autostart_unsupported",
            "Le démarrage avec Windows n'existe que sous Windows.",
        );
    }
    let activer = corps.enabled;
    match tokio::task::spawn_blocking(move || ecrire(activer)).await {
        Ok(Ok(e)) => {
            tracing::info!(enabled = e.actif(), "autostart_set");
            Json(charge_utile(true, Some(&e))).into_response()
        }
        Ok(Err(e)) => erreur(
            StatusCode::INTERNAL_SERVER_ERROR,
            "autostart_write_failed",
            &e,
        ),
        Err(e) => erreur(
            StatusCode::INTERNAL_SERVER_ERROR,
            "autostart_write_failed",
            &e.to_string(),
        ),
    }
}

fn erreur(code: StatusCode, nom: &str, message: &str) -> Response {
    (code, Json(json!({ "error": nom, "message": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Users\sasha\AppData\Local\Programs\Tune Server\tune-server.exe";

    fn sortie_run(commande: &str) -> String {
        format!("\r\n{CLE_RUN}\r\n    {NOM_DE_LA_VALEUR}    REG_SZ    {commande}\r\n\r\n")
    }

    #[test]
    fn la_commande_lance_le_serveur_sans_console_et_sans_navigateur() {
        assert_eq!(
            commande_de_demarrage(EXE),
            format!("\"{EXE}\" --sans-console")
        );
    }

    #[test]
    fn rien_sous_run_veut_dire_non_inscrit() {
        let e = etat("", "", EXE);
        assert!(!e.inscrit);
        assert!(!e.actif());
        assert_eq!(e.commande, None);
    }

    #[test]
    fn la_valeur_posee_par_l_installeur_est_lue_comme_active() {
        let e = etat(&sortie_run(&commande_de_demarrage(EXE)), "", EXE);
        assert!(e.inscrit, "{e:?}");
        assert!(e.actif());
        assert_eq!(
            e.commande.as_deref(),
            Some(commande_de_demarrage(EXE).as_str())
        );
    }

    #[test]
    fn la_casse_du_chemin_ne_compte_pas() {
        let autre_casse = commande_de_demarrage(&EXE.to_uppercase());
        assert!(etat(&sortie_run(&autre_casse), "", EXE).inscrit);
    }

    #[test]
    fn une_valeur_qui_vise_un_autre_executable_n_est_pas_la_notre() {
        let e = etat(
            &sortie_run(r#""D:\Tune-portable\tune-server.exe" --sans-console"#),
            "",
            EXE,
        );
        assert!(!e.inscrit);
        assert!(e.commande.is_some(), "la commande reste montrée");
    }

    #[test]
    fn desactivee_dans_le_gestionnaire_des_taches() {
        let approbation = format!(
            "\r\n{CLE_APPROBATION}\r\n    {NOM_DE_LA_VALEUR}    REG_BINARY    030000000AB12C3D4E5F6071\r\n"
        );
        let e = etat(&sortie_run(&commande_de_demarrage(EXE)), &approbation, EXE);
        assert!(e.inscrit);
        assert!(e.desactive_par_windows);
        assert!(
            !e.actif(),
            "Windows ne la lancera pas : les Réglages doivent le dire"
        );

        let reactivee = approbation.replace("REG_BINARY    03", "REG_BINARY    02");
        assert!(etat(&sortie_run(&commande_de_demarrage(EXE)), &reactivee, EXE).actif());
    }

    #[test]
    fn activer_inscrit_puis_efface_le_marqueur_desactivee() {
        let c = commandes_reg(true, EXE);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0][0], "add");
        assert_eq!(c[0][1], CLE_RUN);
        assert!(c[0].contains(&commande_de_demarrage(EXE)));
        assert!(c[0].contains(&"/f".to_string()));
        assert_eq!(c[1][0], "delete");
        assert_eq!(c[1][1], CLE_APPROBATION);
    }

    #[test]
    fn desactiver_retire_la_valeur() {
        let c = commandes_reg(false, EXE);
        assert_eq!(c[0][..2], ["delete".to_string(), CLE_RUN.to_string()]);
        assert!(c.iter().all(|a| a[0] == "delete"));
    }

    #[tokio::test]
    async fn hors_windows_la_route_dit_non_supporte() {
        if cfg!(target_os = "windows") {
            return;
        }
        let r = get_autostart().await;
        assert_eq!(r.status(), StatusCode::OK);
        let corps = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let v: Value = serde_json::from_slice(&corps).unwrap();
        assert_eq!(v["supported"], false);
        assert_eq!(v["enabled"], false);
    }
}

/// L'installeur Windows (NSIS, écrit dans `release.yml`) pose la même valeur
/// `Run`, derrière une case cochée par défaut, et la retire à la
/// désinstallation (#8054).
#[cfg(test)]
mod installeur_tests {
    use super::*;

    fn installeur() -> String {
        let chemin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.github/workflows/release.yml");
        std::fs::read_to_string(&chemin).expect("release.yml lisible")
    }

    /// Une instruction ACTIVE du script NSIS (une ligne `; …` ne compte pas).
    fn instruction(yml: &str, debut: &str) -> bool {
        yml.lines().any(|l| l.trim_start().starts_with(debut))
    }

    /// `HKCU\Software\…` tel que NSIS l'écrit : racine et chemin séparés.
    fn cle_nsis(cle: &str) -> String {
        format!("HKCU \"{}\"", cle.trim_start_matches(r"HKCU\"))
    }

    #[test]
    fn l_installeur_offre_la_case_cochee_par_defaut_8054() {
        let yml = installeur();
        assert!(
            instruction(&yml, "!insertmacro MUI_PAGE_COMPONENTS"),
            "la case « Démarrer Tune avec Windows » doit être visible"
        );
        assert!(
            instruction(&yml, "Section \"Demarrer Tune avec Windows\" SecDemarrage"),
            "section dédiée, cochée par défaut (ni /o ni RO)"
        );
    }

    #[test]
    fn l_installeur_pose_la_meme_valeur_run_que_les_reglages_8054() {
        let yml = installeur();
        let attendu = format!(
            "WriteRegStr {} \"{NOM_DE_LA_VALEUR}\" '\"`$INSTDIR\\tune-server.exe\" {ARGUMENT}'",
            cle_nsis(CLE_RUN)
        );
        assert!(instruction(&yml, &attendu), "attendu : {attendu}");
        // Cocher la case réactive une entrée désactivée dans le Gestionnaire
        // des tâches, comme le fait l'interrupteur des Réglages.
        assert!(instruction(
            &yml,
            &format!(
                "DeleteRegValue {} \"{NOM_DE_LA_VALEUR}\"",
                cle_nsis(CLE_APPROBATION)
            )
        ));
    }

    #[test]
    fn la_mise_a_jour_respecte_un_choix_decoche_8054() {
        // Réinstaller (mise à jour) ne doit pas réactiver ce que l'utilisateur
        // a coupé dans les Réglages : la case part décochée si Tune est déjà
        // installé SANS la valeur Run.
        let yml = installeur();
        assert!(instruction(&yml, "Function .onInit"));
        assert!(instruction(
            &yml,
            &format!(
                "ReadRegStr `$1 {} \"{NOM_DE_LA_VALEUR}\"",
                cle_nsis(CLE_RUN)
            )
        ));
        assert!(instruction(
            &yml,
            "!insertmacro UnselectSection `${SecDemarrage}"
        ));
    }

    #[test]
    fn la_desinstallation_retire_le_demarrage_automatique_8054() {
        let yml = installeur();
        let desinstallation = yml
            .split("Section \"Uninstall\"")
            .nth(1)
            .expect("section de désinstallation");
        let fin = desinstallation.find("SectionEnd").expect("SectionEnd");
        let desinstallation = &desinstallation[..fin];
        for cle in [CLE_RUN, CLE_APPROBATION] {
            let attendu = format!("DeleteRegValue {} \"{NOM_DE_LA_VALEUR}\"", cle_nsis(cle));
            assert!(
                instruction(desinstallation, &attendu),
                "attendu : {attendu}"
            );
        }
    }
}
