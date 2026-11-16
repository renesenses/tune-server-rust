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

    /// `Name` de `Get-NetFirewallProfile` : identifiants fixes, non traduits.
    fn depuis_nom(nom: &str) -> Option<Self> {
        match nom.trim().to_ascii_lowercase().as_str() {
            "domain" => Some(Self::Domaine),
            "private" => Some(Self::Prive),
            "public" => Some(Self::Public),
            _ => None,
        }
    }
}

/// Une interface connectée, telle que la relève [`SCRIPT_PROFILS_RESEAU`].
///
/// Un champ absent se lit « inconnu », et l'interface compte alors comme
/// portant le réseau local (comportement d'avant la relecture du 12/11).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InterfaceLue {
    /// `NetworkCategory` en entier, voir [`ProfilReseau::depuis_categorie`].
    pub categorie: i64,
    /// `IPv4Connectivity` en entier : 0 déconnecté, 1 sans trafic,
    /// 2 sous-réseau, 3 réseau local, 4 Internet.
    #[serde(default)]
    pub ipv4: Option<i64>,
    /// L'interface porte une route IPv4 par défaut (`0.0.0.0/0`).
    #[serde(default)]
    pub route_par_defaut: Option<bool>,
    /// Carte physique (`Get-NetAdapter -Physical`) : ni vEthernet
    /// Hyper-V/WSL/Docker, ni tunnel VPN logiciel.
    #[serde(default)]
    pub physique: Option<bool>,
}

const CONNECTIVITE_RESEAU_LOCAL: i64 = 3;

impl InterfaceLue {
    fn profil(&self) -> Option<ProfilReseau> {
        ProfilReseau::depuis_categorie(self.categorie)
    }

    /// Joint-elle le réseau local (ou ne sait-on pas) ?
    fn atteint_le_reseau_local(&self) -> bool {
        self.route_par_defaut == Some(true)
            || self.ipv4.is_none_or(|n| n >= CONNECTIVITE_RESEAU_LOCAL)
    }
}

/// Un profil du pare-feu Windows (`Get-NetFirewallProfile -PolicyStore
/// ActiveStore`, la politique effective, GPO comprises).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ProfilDuPareFeuLu {
    /// `Domain`, `Private` ou `Public`.
    pub nom: String,
    /// `Enabled` en entier (GpoBoolean) : 0 = coupé, 1 = actif,
    /// 2 = non configuré (valeur par défaut : actif).
    pub actif: i64,
}

/// La sortie de [`SCRIPT_PROFILS_RESEAU`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleveReseau {
    pub reseaux: Vec<InterfaceLue>,
    pub pare_feu: Vec<ProfilDuPareFeuLu>,
    /// Pare-feu tiers inscrits au Centre de sécurité Windows.
    pub pare_feu_tiers: i64,
}

impl ReleveReseau {
    /// Le pare-feu Windows filtre-t-il ce profil ? Relevé muet : on le
    /// suppose actif (c'est le réglage par défaut de Windows).
    fn pare_feu_actif(&self, profil: ProfilReseau) -> bool {
        self.pare_feu
            .iter()
            .filter(|p| ProfilReseau::depuis_nom(&p.nom) == Some(profil))
            .all(|p| p.actif != 0)
    }

    /// Les interfaces qui portent le réseau local réel, et les autres.
    ///
    /// 1. les cartes physiques qui joignent le réseau local ;
    /// 2. à défaut, les interfaces qui portent la route par défaut (carte
    ///    physique liée à un commutateur Hyper-V externe, par exemple) ;
    /// 3. à défaut, toute interface qui joint le réseau local.
    ///
    /// Les vEthernet Hyper-V/WSL/Docker et les VPN, presque toujours
    /// « Public », tombent ainsi du côté des autres.
    fn partager(&self) -> (Vec<&InterfaceLue>, Vec<&InterfaceLue>) {
        let paliers: [&dyn Fn(&InterfaceLue) -> bool; 3] = [
            &|i| i.physique != Some(false) && i.atteint_le_reseau_local(),
            &|i| i.route_par_defaut == Some(true),
            &|i| i.atteint_le_reseau_local(),
        ];
        for palier in paliers {
            let (porteuses, autres): (Vec<&InterfaceLue>, Vec<&InterfaceLue>) =
                self.reseaux.iter().partition(|i| palier(i));
            if !porteuses.is_empty() {
                return (porteuses, autres);
            }
        }
        (Vec::new(), self.reseaux.iter().collect())
    }
}

/// Les profils d'une liste d'interfaces, dédoublonnés, ordre Domaine, Privé,
/// Public.
fn profils_de(interfaces: &[&InterfaceLue]) -> Vec<ProfilReseau> {
    let lus: Vec<ProfilReseau> = interfaces.iter().filter_map(|i| i.profil()).collect();
    ProfilReseau::TOUS
        .into_iter()
        .filter(|p| lus.contains(p))
        .collect()
}

/// Une liste PowerShell : `null`, un objet seul ou un tableau.
fn liste<T: serde::de::DeserializeOwned>(
    valeur: Option<&serde_json::Value>,
) -> Result<Vec<T>, String> {
    match valeur {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(v @ serde_json::Value::Array(_)) => {
            serde_json::from_value(v.clone()).map_err(|e| e.to_string())
        }
        Some(v @ serde_json::Value::Object(_)) => serde_json::from_value(v.clone())
            .map(|x| vec![x])
            .map_err(|e| e.to_string()),
        Some(autre) => Err(format!("sortie inattendue : {autre}")),
    }
}

/// Lit la sortie de [`SCRIPT_PROFILS_RESEAU`] : un objet `{ Reseaux, PareFeu,
/// PareFeuTiers }`. Un tableau seul se lit comme la liste des réseaux.
pub fn lire_le_releve_reseau(json: &str) -> Result<ReleveReseau, String> {
    let texte = json.trim().trim_start_matches('\u{feff}');
    if texte.is_empty() {
        return Ok(ReleveReseau::default());
    }
    let valeur: serde_json::Value = serde_json::from_str(texte).map_err(|e| e.to_string())?;
    match &valeur {
        serde_json::Value::Array(_) => Ok(ReleveReseau {
            reseaux: liste(Some(&valeur))?,
            ..ReleveReseau::default()
        }),
        serde_json::Value::Object(o) => Ok(ReleveReseau {
            reseaux: liste(o.get("Reseaux"))?,
            pare_feu: liste(o.get("PareFeu"))?,
            pare_feu_tiers: o.get("PareFeuTiers").and_then(|n| n.as_i64()).unwrap_or(0),
        }),
        autre => Err(format!("sortie inattendue : {autre}")),
    }
}

/// Ce que le pare-feu fait des réseaux actifs, pour `tune-server.exe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EtatDuPareFeu {
    /// Les profils des réseaux qui portent le réseau local (vide : aucun
    /// réseau, ou relevé muet).
    pub profils_actifs: Vec<ProfilReseau>,
    /// Une règle entrante active `Allow` existe pour CET exécutable.
    pub regle_presente: bool,
    /// Les profils où l'entrant de Tune est autorisé (et non bloqué).
    pub profils_couverts: Vec<ProfilReseau>,
    /// Les profils actifs, pare-feu Windows actif, que la règle ne couvre pas.
    pub profils_non_couverts: Vec<ProfilReseau>,
    /// Les profils des autres interfaces (vEthernet, VPN…), pour mémoire :
    /// ils ne décident pas de l'alerte.
    pub profils_autres_reseaux: Vec<ProfilReseau>,
    /// Les profils où le pare-feu Windows est coupé.
    pub profils_pare_feu_coupe: Vec<ProfilReseau>,
    /// Un pare-feu tiers est inscrit : on ne sait pas ce qu'il filtre.
    pub pare_feu_tiers: bool,
}

impl EtatDuPareFeu {
    /// Faut-il le dire à l'écran ? Seulement si TOUS les réseaux locaux sont
    /// filtrés par le pare-feu Windows : un seul réseau non couvert parmi
    /// d'autres ne dit pas où est le renderer, et un pare-feu tiers ne se
    /// lit pas.
    pub fn alerte(&self) -> bool {
        !self.pare_feu_tiers
            && !self.profils_non_couverts.is_empty()
            && self
                .profils_actifs
                .iter()
                .all(|p| self.profils_non_couverts.contains(p))
    }

    /// Un réseau connecté (local ou non) est-il hors de la règle, sans que
    /// cela suffise à l'alerte ?
    fn un_reseau_non_couvert(&self) -> Vec<ProfilReseau> {
        let mut profils: Vec<ProfilReseau> = self
            .profils_non_couverts
            .iter()
            .chain(self.profils_autres_reseaux.iter())
            .copied()
            .filter(|p| !self.profils_couverts.contains(p))
            .collect();
        profils.sort_by_key(|p| p.bit());
        profils.dedup();
        profils
    }
}

/// L'état, pur : quels réseaux locaux la règle de Tune couvre-t-elle ?
pub fn etat_du_pare_feu(
    regles: &[RegleDePareFeu],
    releve: &ReleveReseau,
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
    let (porteuses, autres) = releve.partager();
    let profils_actifs = profils_de(&porteuses);
    let profils_non_couverts = profils_actifs
        .iter()
        .copied()
        .filter(|&p| releve.pare_feu_actif(p) && !profils_couverts.contains(&p))
        .collect();
    let profils_autres_reseaux = profils_de(&autres)
        .into_iter()
        .filter(|p| !profils_actifs.contains(p))
        .collect();
    let profils_pare_feu_coupe = ProfilReseau::TOUS
        .into_iter()
        .filter(|&p| !releve.pare_feu_actif(p))
        .collect();
    EtatDuPareFeu {
        profils_actifs,
        regle_presente: les_notres.iter().any(|r| r.action == AUTORISER),
        profils_couverts,
        profils_non_couverts,
        profils_autres_reseaux,
        profils_pare_feu_coupe,
        pare_feu_tiers: releve.pare_feu_tiers > 0,
    }
}

/// La ligne de journal du profil réseau : au démarrage, puis à chaque
/// changement relevé par la route.
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
        return;
    }
    let ailleurs = etat.un_reseau_non_couvert();
    info!(
        program = exe,
        active_profiles = ?etat.profils_actifs,
        other_profiles = ?etat.profils_autres_reseaux,
        rule_present = etat.regle_presente,
        covered_profiles = ?etat.profils_couverts,
        firewall_off_profiles = ?etat.profils_pare_feu_coupe,
        third_party_firewall = etat.pare_feu_tiers,
        some_network_not_covered = ?ailleurs,
        "windows_firewall_network_profile"
    );
}

/// Durée de vie du relevé que sert `GET /system/pare-feu` : l'écran suit le
/// passage du réseau en Privé sans redémarrage, sans relancer PowerShell à
/// chaque appel.
pub const DUREE_DU_CACHE: std::time::Duration = std::time::Duration::from_secs(30);

/// Le dernier relevé et son instant.
#[derive(Debug, Default)]
pub struct CacheDuReleve {
    dernier: Option<(std::time::Instant, Option<EtatDuPareFeu>)>,
}

/// Rend l'état en cache s'il a moins de `duree`, sinon relève de nouveau.
/// `relever` reçoit l'état précédent (pour ne journaliser qu'un changement) ;
/// un relevé muet (`None`) garde l'état précédent. Le verrou est tenu pendant
/// le relevé : deux appels simultanés ne lancent qu'un PowerShell.
pub async fn etat_avec_cache<F, Fut>(
    cache: &tokio::sync::Mutex<CacheDuReleve>,
    maintenant: std::time::Instant,
    duree: std::time::Duration,
    relever: F,
) -> Option<EtatDuPareFeu>
where
    F: FnOnce(Option<EtatDuPareFeu>) -> Fut,
    Fut: std::future::Future<Output = Option<EtatDuPareFeu>>,
{
    let mut cache = cache.lock().await;
    let precedent = match &cache.dernier {
        Some((quand, etat)) if maintenant.saturating_duration_since(*quand) < duree => {
            return etat.clone();
        }
        Some((_, etat)) => etat.clone(),
        None => None,
    };
    let etat = relever(precedent.clone()).await.or(precedent);
    cache.dernier = Some((maintenant, etat.clone()));
    etat
}

#[cfg(target_os = "windows")]
static CACHE: std::sync::LazyLock<tokio::sync::Mutex<CacheDuReleve>> =
    std::sync::LazyLock::new(Default::default);

/// Le corps de `GET /api/v1/system/pare-feu`. Contrat additif.
pub fn charge_utile(windows: bool, etat: Option<&EtatDuPareFeu>) -> serde_json::Value {
    serde_json::json!({
        "windows": windows,
        "releve": etat,
        "alerte": etat.is_some_and(EtatDuPareFeu::alerte),
    })
}

/// `GET /api/v1/system/pare-feu` — l'écran des zones l'affiche quand le
/// profil actif n'est pas couvert (#7371). Relevé de nouveau au-delà de
/// [`DUREE_DU_CACHE`].
pub async fn route_etat() -> axum::Json<serde_json::Value> {
    #[cfg(target_os = "windows")]
    let etat = etat_avec_cache(
        &CACHE,
        std::time::Instant::now(),
        DUREE_DU_CACHE,
        |precedent| async move {
            let exe = std::env::current_exe().ok()?.display().to_string();
            let regles = relever_les_regles().await.ok()?;
            let etat = relever_l_etat(&regles, &exe).await.ok()?;
            if precedent.as_ref() != Some(&etat) {
                journaliser_l_etat(&etat, &exe);
            }
            Some(etat)
        },
    )
    .await;
    #[cfg(not(target_os = "windows"))]
    let etat: Option<EtatDuPareFeu> = None;
    axum::Json(charge_utile(cfg!(target_os = "windows"), etat.as_ref()))
}

/// #7371 — pour chaque interface connectée : catégorie, connectivité IPv4,
/// route par défaut, carte physique ; l'état effectif des trois profils du
/// pare-feu Windows ; le nombre de pare-feu tiers inscrits. Tout en entiers ou
/// booléens : ni nom de réseau, ni adresse, ni nom d'interface, rien qui
/// identifie le testeur dans un journal partagé. Lecture seule.
pub const SCRIPT_PROFILS_RESEAU: &str = "$ErrorActionPreference='SilentlyContinue'; \
$d = @(Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' | ForEach-Object { [int]$_.InterfaceIndex }); \
$f = @(Get-NetAdapter -Physical | ForEach-Object { [int]$_.InterfaceIndex }); \
$n = @(Get-NetConnectionProfile | ForEach-Object { [pscustomobject]@{ Categorie=[int]$_.NetworkCategory; \
Ipv4=[int]$_.IPv4Connectivity; RouteParDefaut=($d -contains [int]$_.InterfaceIndex); \
Physique=($f -contains [int]$_.InterfaceIndex) } }); \
$w = @(Get-NetFirewallProfile -PolicyStore ActiveStore | ForEach-Object { [pscustomobject]@{ \
Nom=[string]$_.Name; Actif=[int]$_.Enabled } }); \
$t = @(Get-CimInstance -Namespace root/SecurityCenter2 -ClassName FirewallProduct).Count; \
ConvertTo-Json -InputObject ([pscustomobject]@{ Reseaux=$n; PareFeu=$w; PareFeuTiers=$t }) -Compress -Depth 4";

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
    match tokio::time::timeout(std::time::Duration::from_secs(20), commande.output()).await {
        Ok(Ok(sortie)) => Ok(String::from_utf8_lossy(&sortie.stdout).into_owned()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timed out".into()),
    }
}

#[cfg(target_os = "windows")]
async fn relever_les_regles() -> Result<Vec<RegleDePareFeu>, String> {
    lire_les_regles(&lancer_powershell(SCRIPT_POWERSHELL).await?)
}

#[cfg(target_os = "windows")]
async fn relever_l_etat(regles: &[RegleDePareFeu], exe: &str) -> Result<EtatDuPareFeu, String> {
    let releve = lire_le_releve_reseau(&lancer_powershell(SCRIPT_PROFILS_RESEAU).await?)?;
    let variable = |nom: &str| std::env::var(nom).ok();
    Ok(etat_du_pare_feu(regles, &releve, exe, &variable))
}

/// Lance le relevé en tâche de fond, une fois. Sans effet hors Windows.
pub fn lancer_le_releve() {
    #[cfg(target_os = "windows")]
    tokio::spawn(async {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let exe = exe.display().to_string();
        let regles = match relever_les_regles().await {
            Ok(regles) => regles,
            Err(e) => {
                tracing::info!(error = %e, "windows_firewall_probe_unavailable");
                return;
            }
        };
        let variable = |nom: &str| std::env::var(nom).ok();
        let v = verdict(&regles, &exe, &variable);
        journaliser(&v, &exe, regles.len());
        // #7371 — le profil du réseau actif, et ce que la règle en couvre ;
        // le résultat amorce le cache de la route.
        let (regles, exe) = (&regles, &exe);
        etat_avec_cache(
            &CACHE,
            std::time::Instant::now(),
            DUREE_DU_CACHE,
            move |_| async move {
                match relever_l_etat(regles, exe).await {
                    Ok(etat) => {
                        journaliser_l_etat(&etat, exe);
                        Some(etat)
                    }
                    Err(e) => {
                        tracing::info!(error = %e, "windows_firewall_profile_probe_unreadable");
                        None
                    }
                }
            },
        )
        .await;
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
