//! #4677 — dire, au journal, ce que le pare-feu Windows sait de `tune-server.exe`.
//!
//! Marco Polo (fil 1886) : accès distant ET diffusion DLNA impossibles tant
//! que le pare-feu du profil PRIVÉ est actif. Le dépôt ne contient aucun
//! mécanisme de règle de pare-feu, et n'en a jamais contenu ; l'installateur
//! est par utilisateur, sans élévation (`RequestExecutionLevel user`, pour que
//! l'auto-mise-à-jour remplace l'exécutable sans droits admin), il ne PEUT donc
//! pas en poser. Et le trafic entrant filtré ne laisse, par nature, aucune
//! trace côté serveur : un journal muet ne prouve rien.
//!
//! Trois états réels mènent à trois conclusions opposées — une règle `Block`
//! (l'invite « Autoriser l'accès » refusée ou fermée), une règle `Allow` sur
//! un ANCIEN chemin, aucune règle — et aucun n'est visible aujourd'hui sans
//! faire taper une commande PowerShell au testeur. Ce module ne corrige rien :
//! il relève ces règles UNE fois au démarrage et écrit une ligne qui les
//! distingue, pour que le prochain journal exporté tranche seul.
//!
//! La LECTURE des règles ne demande pas d'élévation. Aucune écriture.
//!
//! La logique (lecture du JSON, verdict) est pure et testée partout ; seul le
//! lancement de PowerShell est propre à Windows.

use serde::{Deserialize, Serialize};

/// Une règle du pare-feu Windows qui porte sur un programme « tune-server ».
///
/// Les énumérations NetSecurity sont converties en entiers par le script
/// (`[int]`), pour ne dépendre ni de la langue du système ni de la version de
/// PowerShell : `Direction` 1 = entrant, 2 = sortant ; `Action` 2 = autoriser,
/// 4 = bloquer ; `Enabled` 1 = oui, 2 = non ; `Profile` masque de bits,
/// 0 = tous, 1 = domaine, 2 = privé, 4 = public.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RegleDePareFeu {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub program: String,
    pub direction: i64,
    pub action: i64,
    pub enabled: i64,
    pub profile: i64,
}

const ENTRANT: i64 = 1;
const AUTORISER: i64 = 2;
const BLOQUER: i64 = 4;
const ACTIVEE: i64 = 1;
const PROFIL_PRIVE: i64 = 2;

impl RegleDePareFeu {
    fn entrante_active_sur_le_profil_prive(&self) -> bool {
        self.direction == ENTRANT
            && self.enabled == ACTIVEE
            && (self.profile == 0 || self.profile & PROFIL_PRIVE != 0)
    }
}

/// Ce que les règles disent de l'exécutable qui tourne, profil privé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictPareFeu {
    /// Une règle entrante `Allow` couvre cet exécutable sur le profil privé,
    /// et aucune `Block` ne la contredit.
    Autorise,
    /// Une règle entrante `Block` couvre cet exécutable sur le profil privé.
    /// Sous Windows, `Block` l'emporte sur `Allow` : l'accès distant et le
    /// DLNA tombent dès que le pare-feu est actif.
    Bloque { regles: Vec<String> },
    /// Aucune règle pour CET exécutable, mais des règles pour un autre chemin
    /// de `tune-server.exe` : l'hypothèse du déménagement d'installation.
    RegleSurUnAutreChemin { chemins: Vec<String> },
    /// Aucune règle entrante active pour `tune-server`, sur aucun chemin.
    AucuneRegle,
}

/// Lit la sortie du script. `[]`, un objet seul ou un tableau.
pub fn lire_les_regles(json: &str) -> Result<Vec<RegleDePareFeu>, String> {
    let texte = json.trim().trim_start_matches('\u{feff}');
    if texte.is_empty() {
        return Ok(Vec::new());
    }
    let valeur: serde_json::Value = serde_json::from_str(texte).map_err(|e| e.to_string())?;
    match valeur {
        serde_json::Value::Array(_) => serde_json::from_value(valeur).map_err(|e| e.to_string()),
        serde_json::Value::Object(_) => serde_json::from_value(valeur)
            .map(|r| vec![r])
            .map_err(|e| e.to_string()),
        autre => Err(format!("sortie inattendue : {autre}")),
    }
}

/// Deux chemins Windows désignent-ils le même fichier ? Insensible à la casse
/// et aux séparateurs, variables `%NOM%` résolues par `variable`.
fn meme_chemin(regle: &str, exe: &str, variable: &dyn Fn(&str) -> Option<String>) -> bool {
    let normaliser = |s: &str| s.trim().replace('/', "\\").to_lowercase();
    normaliser(&developper(regle, variable)) == normaliser(exe)
}

/// Développe les `%NOM%` d'un chemin de règle (Windows en écrit parfois).
fn developper(chemin: &str, variable: &dyn Fn(&str) -> Option<String>) -> String {
    let mut sortie = String::new();
    let mut reste = chemin;
    while let Some(debut) = reste.find('%') {
        let (avant, apres) = reste.split_at(debut);
        sortie.push_str(avant);
        let apres = &apres[1..];
        match apres.find('%') {
            Some(fin) => {
                let nom = &apres[..fin];
                match variable(nom) {
                    Some(v) => sortie.push_str(&v),
                    None => {
                        sortie.push('%');
                        sortie.push_str(nom);
                        sortie.push('%');
                    }
                }
                reste = &apres[fin + 1..];
            }
            None => {
                sortie.push('%');
                reste = apres;
            }
        }
    }
    sortie.push_str(reste);
    sortie
}

/// Le verdict, pur.
pub fn verdict(
    regles: &[RegleDePareFeu],
    exe: &str,
    variable: &dyn Fn(&str) -> Option<String>,
) -> VerdictPareFeu {
    let pertinentes: Vec<&RegleDePareFeu> = regles
        .iter()
        .filter(|r| r.entrante_active_sur_le_profil_prive())
        .collect();
    let (les_notres, les_autres): (Vec<&RegleDePareFeu>, Vec<&RegleDePareFeu>) = pertinentes
        .into_iter()
        .partition(|r| meme_chemin(&r.program, exe, variable));
    let bloquantes: Vec<String> = les_notres
        .iter()
        .filter(|r| r.action == BLOQUER)
        .map(|r| r.display_name.clone())
        .collect();
    if !bloquantes.is_empty() {
        return VerdictPareFeu::Bloque { regles: bloquantes };
    }
    if les_notres.iter().any(|r| r.action == AUTORISER) {
        return VerdictPareFeu::Autorise;
    }
    let mut chemins: Vec<String> = les_autres
        .iter()
        .filter(|r| r.action == AUTORISER)
        .map(|r| r.program.clone())
        .collect();
    chemins.sort();
    chemins.dedup();
    if chemins.is_empty() {
        VerdictPareFeu::AucuneRegle
    } else {
        VerdictPareFeu::RegleSurUnAutreChemin { chemins }
    }
}

/// Écrit la ligne de journal qui tranche. Un seul appel par démarrage.
pub fn journaliser(verdict: &VerdictPareFeu, exe: &str, nombre_de_regles: usize) {
    use tracing::{info, warn};
    match verdict {
        VerdictPareFeu::Autorise => info!(
            program = exe,
            rules = nombre_de_regles,
            "windows_firewall_inbound_allowed_private"
        ),
        VerdictPareFeu::Bloque { regles } => warn!(
            program = exe,
            rules = ?regles,
            "windows_firewall_inbound_blocked_private — a Block rule covers tune-server.exe: \
             remote access and DLNA fail while the firewall is on. Remove it in \
             'Windows Defender Firewall > Advanced settings > Inbound rules'"
        ),
        VerdictPareFeu::RegleSurUnAutreChemin { chemins } => warn!(
            program = exe,
            other_paths = ?chemins,
            "windows_firewall_rule_for_other_path — tune-server.exe is allowed only at \
             another path; allow the running one on the private profile"
        ),
        VerdictPareFeu::AucuneRegle => warn!(
            program = exe,
            "windows_firewall_no_inbound_rule — no inbound rule for tune-server.exe: with the \
             private-profile firewall on, remote access and DLNA are filtered. Allow \
             tune-server.exe in 'Allow an app through Windows Firewall'"
        ),
    }
}

// ───────────────────────── #7371 — profil réseau actif ─────────────────────────
//
// TEAC NT-505X (fil VNAV, 26-30/10) : seul AirPlay est proposé, le renderer
// UPnP n'est jamais découvert. La règle « Tune » que pose l'installeur (rc11)
// ne couvre que le profil PRIVÉ (décision du 30/10, confirmée le 12/11 : on
// ne l'étend pas au Public). Sur un réseau classé Public, la découverte SSDP
// et le flux DLNA restent donc filtrés, et rien ne le disait : ni le journal,
// ni l'écran. Ce relevé dit le profil actif et ce que la règle en couvre.

/// Un profil du pare-feu Windows, tel que le réseau actif le porte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfilReseau {
    Domaine,
    Prive,
    Public,
}

impl ProfilReseau {
    const TOUS: [ProfilReseau; 3] = [Self::Domaine, Self::Prive, Self::Public];

    /// Le bit du profil dans le masque `Profile` d'une règle.
    fn bit(self) -> i64 {
        match self {
            Self::Domaine => 1,
            Self::Prive => PROFIL_PRIVE,
            Self::Public => 4,
        }
    }

    /// `NetworkCategory` de `Get-NetConnectionProfile`, converti en entier :
    /// 0 = Public, 1 = Privé, 2 = Domaine (authentifié).
    fn depuis_categorie(categorie: i64) -> Option<Self> {
        match categorie {
            0 => Some(Self::Public),
            1 => Some(Self::Prive),
            2 => Some(Self::Domaine),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ProfilLu {
    categorie: i64,
}

/// Lit la sortie de [`SCRIPT_PROFILS_RESEAU`] : `[]`, un objet seul ou un
/// tableau. Profils dédoublonnés, dans l'ordre Domaine, Privé, Public.
pub fn lire_les_profils(json: &str) -> Result<Vec<ProfilReseau>, String> {
    let texte = json.trim().trim_start_matches('\u{feff}');
    if texte.is_empty() {
        return Ok(Vec::new());
    }
    let valeur: serde_json::Value = serde_json::from_str(texte).map_err(|e| e.to_string())?;
    let lus: Vec<ProfilLu> = match valeur {
        serde_json::Value::Array(_) => serde_json::from_value(valeur).map_err(|e| e.to_string())?,
        serde_json::Value::Object(_) => {
            vec![serde_json::from_value(valeur).map_err(|e| e.to_string())?]
        }
        autre => return Err(format!("sortie inattendue : {autre}")),
    };
    let actifs: Vec<ProfilReseau> = lus
        .iter()
        .filter_map(|p| ProfilReseau::depuis_categorie(p.categorie))
        .collect();
    Ok(ProfilReseau::TOUS
        .into_iter()
        .filter(|p| actifs.contains(p))
        .collect())
}

/// Ce que le pare-feu fait des réseaux actifs, pour `tune-server.exe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EtatDuPareFeu {
    /// Les profils des réseaux connectés (vide : aucun réseau, ou relevé muet).
    pub profils_actifs: Vec<ProfilReseau>,
    /// Une règle entrante active `Allow` existe pour CET exécutable.
    pub regle_presente: bool,
    /// Les profils où l'entrant de Tune est autorisé (et non bloqué).
    pub profils_couverts: Vec<ProfilReseau>,
    /// Les profils actifs que la règle ne couvre pas : la découverte DLNA et
    /// l'accès distant y sont filtrés.
    pub profils_non_couverts: Vec<ProfilReseau>,
}

impl EtatDuPareFeu {
    /// Faut-il le dire à l'écran ?
    pub fn alerte(&self) -> bool {
        !self.profils_non_couverts.is_empty()
    }
}

/// L'état, pur : quels profils actifs la règle de Tune couvre-t-elle ?
pub fn etat_du_pare_feu(
    regles: &[RegleDePareFeu],
    profils_actifs: &[ProfilReseau],
    exe: &str,
    variable: &dyn Fn(&str) -> Option<String>,
) -> EtatDuPareFeu {
    let les_notres: Vec<&RegleDePareFeu> = regles
        .iter()
        .filter(|r| r.direction == ENTRANT && r.enabled == ACTIVEE)
        .filter(|r| meme_chemin(&r.program, exe, variable))
        .collect();
    let porte_sur =
        |r: &RegleDePareFeu, p: ProfilReseau| r.profile == 0 || r.profile & p.bit() != 0;
    let profils_couverts: Vec<ProfilReseau> = ProfilReseau::TOUS
        .into_iter()
        .filter(|&p| {
            let bloque = les_notres
                .iter()
                .any(|r| r.action == BLOQUER && porte_sur(r, p));
            let autorise = les_notres
                .iter()
                .any(|r| r.action == AUTORISER && porte_sur(r, p));
            autorise && !bloque
        })
        .collect();
    let profils_non_couverts = profils_actifs
        .iter()
        .copied()
        .filter(|p| !profils_couverts.contains(p))
        .collect();
    EtatDuPareFeu {
        profils_actifs: profils_actifs.to_vec(),
        regle_presente: les_notres.iter().any(|r| r.action == AUTORISER),
        profils_couverts,
        profils_non_couverts,
    }
}

/// La ligne de journal du profil réseau. Un seul appel par démarrage.
pub fn journaliser_l_etat(etat: &EtatDuPareFeu, exe: &str) {
    use tracing::{info, warn};
    if etat.alerte() {
        warn!(
            program = exe,
            active_profiles = ?etat.profils_actifs,
            rule_present = etat.regle_presente,
            covered_profiles = ?etat.profils_couverts,
            uncovered_profiles = ?etat.profils_non_couverts,
            "windows_firewall_profile_not_covered — the active network profile is not \
             covered by Tune's inbound firewall rule: DLNA discovery and remote access are \
             filtered. Set the network to Private, or allow tune-server.exe in 'Allow an app \
             through Windows Firewall' for this profile"
        );
    } else {
        info!(
            program = exe,
            active_profiles = ?etat.profils_actifs,
            rule_present = etat.regle_presente,
            covered_profiles = ?etat.profils_couverts,
            "windows_firewall_network_profile"
        );
    }
}

static ETAT_RELEVE: std::sync::RwLock<Option<EtatDuPareFeu>> = std::sync::RwLock::new(None);

/// Retient l'état relevé, que lit [`etat_releve`].
pub fn retenir_l_etat(etat: EtatDuPareFeu) {
    if let Ok(mut place) = ETAT_RELEVE.write() {
        *place = Some(etat);
    }
}

/// Le dernier état relevé au démarrage (`None` hors Windows, ou avant la fin
/// du relevé).
pub fn etat_releve() -> Option<EtatDuPareFeu> {
    ETAT_RELEVE.read().ok().and_then(|e| e.clone())
}

/// Le corps de `GET /api/v1/system/pare-feu`. Contrat additif.
pub fn charge_utile(windows: bool, etat: Option<&EtatDuPareFeu>) -> serde_json::Value {
    serde_json::json!({
        "windows": windows,
        "releve": etat,
        "alerte": etat.is_some_and(EtatDuPareFeu::alerte),
    })
}

/// `GET /api/v1/system/pare-feu` — l'écran des zones l'affiche quand le
/// profil actif n'est pas couvert (#7371).
pub async fn route_etat() -> axum::Json<serde_json::Value> {
    axum::Json(charge_utile(
        cfg!(target_os = "windows"),
        etat_releve().as_ref(),
    ))
}

/// #7371 — les profils des réseaux connectés, en entiers (indépendance de la
/// langue). Ni nom de réseau ni adresse : rien qui identifie le testeur dans
/// un journal partagé. Lecture seule.
pub const SCRIPT_PROFILS_RESEAU: &str = "$ErrorActionPreference='SilentlyContinue'; \
$p = @(Get-NetConnectionProfile | Select-Object @{n='Categorie';e={[int]$_.NetworkCategory}}); \
ConvertTo-Json -InputObject $p -Compress";

/// Le script PowerShell : toutes les règles dont le programme ressemble à
/// `tune-server`, énumérations converties en entiers, sortie JSON compacte.
/// Lecture seule.
pub const SCRIPT_POWERSHELL: &str = "$ErrorActionPreference='SilentlyContinue'; \
$r = @(Get-NetFirewallApplicationFilter | Where-Object { $_.Program -like '*tune-server*' } | \
ForEach-Object { $p = $_.Program; $_ | Get-NetFirewallRule | Select-Object DisplayName, \
@{n='Program';e={$p}}, @{n='Direction';e={[int]$_.Direction}}, @{n='Action';e={[int]$_.Action}}, \
@{n='Enabled';e={[int]$_.Enabled}}, @{n='Profile';e={[int]$_.Profile}} }); \
ConvertTo-Json -InputObject $r -Compress";

/// Lance un script PowerShell en lecture seule et rend sa sortie standard.
#[cfg(target_os = "windows")]
async fn lancer_powershell(script: &str) -> Result<String, String> {
    let mut commande = tokio::process::Command::new("powershell");
    commande
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .kill_on_drop(true);
    match tokio::time::timeout(std::time::Duration::from_secs(30), commande.output()).await {
        Ok(Ok(sortie)) => Ok(String::from_utf8_lossy(&sortie.stdout).into_owned()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timed out".into()),
    }
}

/// Lance le relevé en tâche de fond, une fois. Sans effet hors Windows.
pub fn lancer_le_releve() {
    #[cfg(target_os = "windows")]
    tokio::spawn(async {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let exe = exe.display().to_string();
        let texte = match lancer_powershell(SCRIPT_POWERSHELL).await {
            Ok(texte) => texte,
            Err(e) => {
                tracing::info!(error = %e, "windows_firewall_probe_unavailable");
                return;
            }
        };
        let regles = match lire_les_regles(&texte) {
            Ok(regles) => regles,
            Err(e) => {
                tracing::info!(error = %e, "windows_firewall_probe_unreadable");
                return;
            }
        };
        let variable = |nom: &str| std::env::var(nom).ok();
        let v = verdict(&regles, &exe, &variable);
        journaliser(&v, &exe, regles.len());
        // #7371 — le profil du réseau actif, et ce que la règle en couvre.
        let profils = match lancer_powershell(SCRIPT_PROFILS_RESEAU).await {
            Ok(texte) => lire_les_profils(&texte),
            Err(e) => Err(e),
        };
        match profils {
            Ok(profils) => {
                let etat = etat_du_pare_feu(&regles, &profils, &exe, &variable);
                journaliser_l_etat(&etat, &exe);
                retenir_l_etat(etat);
            }
            Err(e) => tracing::info!(error = %e, "windows_firewall_profile_probe_unreadable"),
        }
    });
}

#[cfg(test)]
#[path = "pare_feu_profil_reseau_7371_tests.rs"]
mod profil_reseau_7371_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Users\marco\AppData\Local\Programs\Tune Server\tune-server.exe";

    fn sans_variable(_: &str) -> Option<String> {
        None
    }

    fn regle(nom: &str, program: &str, action: i64, profile: i64) -> RegleDePareFeu {
        RegleDePareFeu {
            display_name: nom.into(),
            program: program.into(),
            direction: ENTRANT,
            action,
            enabled: ACTIVEE,
            profile,
        }
    }

    /// Ce que Windows crée quand l'invite « Autoriser l'accès » est refusée :
    /// deux règles `Block` (TCP, UDP) sur le chemin exact, profil privé.
    #[test]
    fn une_invite_refusee_se_lit_bloque_4677() {
        let json = format!(
            r#"[{{"DisplayName":"tune-server.exe","Program":{exe:?},"Direction":1,"Action":4,"Enabled":1,"Profile":2}},
               {{"DisplayName":"tune-server.exe","Program":{exe:?},"Direction":1,"Action":4,"Enabled":1,"Profile":2}}]"#,
            exe = EXE
        );
        let regles = lire_les_regles(&json).unwrap();
        assert_eq!(
            verdict(&regles, EXE, &sans_variable),
            VerdictPareFeu::Bloque {
                regles: vec!["tune-server.exe".into(), "tune-server.exe".into()]
            }
        );
    }

    /// `Block` l'emporte sur `Allow`, comme dans le pare-feu lui-même.
    #[test]
    fn block_l_emporte_sur_allow_4677() {
        let regles = [
            regle("autorise", EXE, AUTORISER, 0),
            regle("bloque", EXE, BLOQUER, 6),
        ];
        assert_eq!(
            verdict(&regles, EXE, &sans_variable),
            VerdictPareFeu::Bloque {
                regles: vec!["bloque".into()]
            }
        );
    }

    #[test]
    fn une_autorisation_privee_ou_tous_profils_se_lit_autorise_4677() {
        for profil in [0, 2, 3, 7] {
            assert_eq!(
                verdict(&[regle("ok", EXE, AUTORISER, profil)], EXE, &sans_variable),
                VerdictPareFeu::Autorise,
                "profil {profil}"
            );
        }
    }

    /// Une autorisation sur le seul profil PUBLIC ne couvre pas le réseau
    /// privé du testeur ; une règle désactivée ou sortante non plus.
    #[test]
    fn ce_qui_ne_couvre_pas_l_entrant_prive_ne_compte_pas_4677() {
        let mut desactivee = regle("off", EXE, AUTORISER, 2);
        desactivee.enabled = 2;
        let mut sortante = regle("out", EXE, AUTORISER, 2);
        sortante.direction = 2;
        let regles = [regle("public", EXE, AUTORISER, 4), desactivee, sortante];
        assert_eq!(
            verdict(&regles, EXE, &sans_variable),
            VerdictPareFeu::AucuneRegle
        );
    }

    /// L'hypothèse du déménagement : une autorisation sur l'ANCIEN dossier.
    #[test]
    fn une_autorisation_sur_un_autre_chemin_est_nommee_4677() {
        let ancien = r"D:\Tune\tune-server.exe";
        assert_eq!(
            verdict(
                &[regle("ancien", ancien, AUTORISER, 2)],
                EXE,
                &sans_variable
            ),
            VerdictPareFeu::RegleSurUnAutreChemin {
                chemins: vec![ancien.into()]
            }
        );
    }

    /// Casse, séparateurs et `%LOCALAPPDATA%` : c'est bien le même fichier.
    #[test]
    fn le_meme_fichier_ecrit_autrement_est_reconnu_4677() {
        let variable = |nom: &str| {
            (nom.eq_ignore_ascii_case("LOCALAPPDATA"))
                .then(|| r"C:\Users\marco\AppData\Local".to_string())
        };
        let regles = [regle(
            "ok",
            "%LOCALAPPDATA%/programs/TUNE SERVER/Tune-Server.exe",
            AUTORISER,
            2,
        )];
        assert_eq!(verdict(&regles, EXE, &variable), VerdictPareFeu::Autorise);
    }

    #[test]
    fn la_sortie_du_script_se_lit_sous_ses_trois_formes_4677() {
        assert_eq!(lire_les_regles("").unwrap(), vec![]);
        assert_eq!(lire_les_regles("[]").unwrap(), vec![]);
        assert_eq!(lire_les_regles("\u{feff}[]\r\n").unwrap(), vec![]);
        let seul = lire_les_regles(
            r#"{"DisplayName":"x","Program":"C:\\t\\tune-server.exe","Direction":1,"Action":2,"Enabled":1,"Profile":2}"#,
        )
        .unwrap();
        assert_eq!(seul.len(), 1);
        assert!(lire_les_regles("Get-NetFirewallRule : accès refusé").is_err());
        assert_eq!(
            verdict(&[], EXE, &sans_variable),
            VerdictPareFeu::AucuneRegle
        );
    }

    /// Le script ne fait que LIRE : aucune cmdlet d'écriture, et les
    /// énumérations sortent en entiers (indépendance de la langue).
    #[test]
    fn le_script_est_en_lecture_seule_et_rend_des_entiers_4677() {
        for interdit in [
            "New-NetFirewallRule",
            "Set-NetFirewall",
            "Remove-NetFirewall",
            "netsh",
        ] {
            assert!(!SCRIPT_POWERSHELL.contains(interdit), "{interdit}");
        }
        for champ in ["Direction", "Action", "Enabled", "Profile"] {
            assert!(
                SCRIPT_POWERSHELL.contains(&format!("@{{n='{champ}';e={{[int]$_.{champ}}}}}")),
                "{champ} doit sortir en entier"
            );
        }
    }

    /// Garde de BRANCHEMENT : le démarrage lance bien le relevé.
    #[test]
    fn le_demarrage_lance_le_releve_4677() {
        let src = include_str!("startup.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert_eq!(
            prod.matches("crate::pare_feu_windows::lancer_le_releve();")
                .count(),
            1,
            "le relevé du pare-feu doit être lancé une fois au démarrage (#4677)"
        );
    }
}
