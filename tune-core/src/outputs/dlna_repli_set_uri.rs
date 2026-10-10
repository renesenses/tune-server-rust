//! Repli conservateur quand un renderer REFUSE `SetAVTransportURI`.
//!
//! Un refus applicatif `501 Action Failed`, `714 Illegal MIME-type` ou
//! `716 Resource not found` ne dit pas forcément « je ne sais pas lire ce
//! flux » : il dit souvent « je n'aime pas la façon dont tu me l'annonces ».
//! Le cas d'école est la pile `upmpdcli` (libupnp) : avec son contrôle
//! `checkcontentformat` actif, elle compare le MIME du `protocolInfo` de la
//! DIDL à sa propre liste, et tout écart remonte en `501 Action Failed`, le
//! code générique de libupnp pour une action qui a échoué. Le même flux,
//! annoncé dans l'orthographe que le renderer publie dans son
//! `GetProtocolInfo` → `Sink`, passe.
//!
//! La reprise 714 existait déjà dans `play_media` (orthographe exacte du Sink,
//! puis étiquette PCM). Ce module y ajoute :
//!
//! - le classement de la faute (`501`, `714`, `716`, autre) ;
//! - la MÉMOIRE par appareil (UDN) et par MIME source du profil qui a fini par
//!   passer, pour ne pas repayer les refus à chaque piste ni sur le gapless ;
//! - l'extraction du `protocolInfo` émis, pour que chaque essai se lise au
//!   journal avec ce qui a été envoyé et ce qui a été répondu.
//!
//! Aucune boucle : deux replis au plus par pose d'URI, décidés par
//! `play_media`.
//!
//! La mémoire survit au redémarrage : elle est rangée par UDN dans
//! `settings[dlna_compat_set_uri:<udn>]`, relue à la découverte de
//! l'appareil, oubliée sur un refus du profil appris ou un changement de
//! version logicielle, et réinitialisable depuis la zone.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Ce que le renderer a répondu à un `SetAVTransportURI` refusé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FauteSetUri {
    /// `714 Illegal MIME-type` : le MIME annoncé n'est pas dans son Sink.
    MimeIllegal714,
    /// `501 Action Failed` : échec générique — upmpdcli/libupnp y range le
    /// refus de son contrôle de format.
    ActionEchouee501,
    /// `716 Resource not found` : certains renderers y rangent une ressource
    /// dont ils refusent le format annoncé.
    RessourceIntrouvable716,
    /// Toute autre faute : pas de repli de profil, l'erreur remonte telle
    /// quelle (701, 402…, qui ont leur propre sens).
    Autre,
}

impl FauteSetUri {
    pub(crate) fn code(self) -> &'static str {
        match self {
            FauteSetUri::MimeIllegal714 => "714",
            FauteSetUri::ActionEchouee501 => "501",
            FauteSetUri::RessourceIntrouvable716 => "716",
            FauteSetUri::Autre => "autre",
        }
    }
}

/// Classe la réponse d'un `SetAVTransportURI` refusé. Lit le `errorCode`
/// UPnP ; à défaut, la description `Illegal MIME` (forme historique de la
/// reprise 714).
pub(crate) fn classer_faute_set_uri(reponse: &str) -> FauteSetUri {
    let code = super::dlna::extract_tag(reponse, "errorCode")
        .map(|c| c.trim().to_string())
        .unwrap_or_default();
    match code.as_str() {
        "714" => FauteSetUri::MimeIllegal714,
        "501" => FauteSetUri::ActionEchouee501,
        "716" => FauteSetUri::RessourceIntrouvable716,
        _ if reponse.to_ascii_lowercase().contains("illegal mime") => FauteSetUri::MimeIllegal714,
        _ => FauteSetUri::Autre,
    }
}

/// Le profil d'annonce qui a fini par passer sur un appareil pour un MIME
/// source donné.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilSetUri {
    /// MIME à annoncer dans la DIDL à la place du MIME source.
    pub mime_annonce: String,
    /// Niveau de DIDL minimal à employer (0 complet, 1 minimal, 2 vide).
    pub niveau_didl_min: u8,
    /// Le MIME annoncé est un alias du format servi (mêmes octets) : il est
    /// aussi servi en `Content-Type` (#4958). Faux pour l'étiquette PCM.
    pub servi_sous_ce_mime: bool,
}

/// Un profil appris, avec sa date : le diagnostic dit DEPUIS QUAND un appareil
/// est servi en mode conservateur.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProfilDate {
    #[serde(flatten)]
    profil: ProfilSetUri,
    #[serde(default)]
    appris_le: Option<String>,
}

/// Ce que Tune sait d'un appareil (UDN).
#[derive(Debug, Default)]
struct Appareil {
    /// La base a déjà été lue pour cet UDN dans ce processus.
    charge: bool,
    /// Version logicielle publiée par la description UPnP, quand elle existe.
    /// Un profil appris sous une version est oublié sous une autre.
    firmware: Option<String>,
    /// MIME source (minuscules) → profil appris.
    profils: HashMap<String, ProfilDate>,
}

/// La forme rangée en base, sous `settings[dlna_compat_set_uri:<udn>]`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Enregistrement {
    #[serde(default)]
    firmware: Option<String>,
    #[serde(default)]
    profils: BTreeMap<String, ProfilDate>,
}

/// Mémoire process : UDN → profils appris, doublée d'une copie en base.
///
/// Process et non champ de la sortie : une sortie DLNA peut être recréée
/// (redécouverte, changement de zone) sans que l'appareil ait changé de pile
/// UPnP. L'UDN, lui, ne bouge pas. La copie en base fait survivre la mémoire
/// à un redémarrage de Tune : sans elle, chaque démarrage repayait les refus
/// sur la première piste de chaque format.
static APPAREILS: LazyLock<Mutex<HashMap<String, Appareil>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// La base où ranger les profils. `None` tant que le serveur ne l'a pas
/// installée (outils, bancs) : la mémoire reste alors process seulement.
static MAGASIN: RwLock<Option<Arc<dyn DbBackend>>> = RwLock::new(None);

/// Sérialise les écritures : chacune relit l'état courant SOUS ce verrou, donc
/// la dernière écrite est toujours la plus récente, quel que soit l'ordre dans
/// lequel les tâches d'écriture démarrent.
static ECRITURE: Mutex<()> = Mutex::new(());

/// Préfixe des clés de `settings` qui portent les profils appris.
pub const PREFIXE_CLE_REGLAGE: &str = "dlna_compat_set_uri:";

fn cle_reglage(udn: &str) -> String {
    format!("{PREFIXE_CLE_REGLAGE}{udn}")
}

fn cle_mime(mime_source: &str) -> String {
    mime_source.trim().to_ascii_lowercase()
}

/// Branche la mémoire sur la base : appelé une fois au démarrage du serveur.
pub fn installer_persistance(db: Arc<dyn DbBackend>) {
    if let Ok(mut m) = MAGASIN.write() {
        *m = Some(db);
    }
}

fn magasin() -> Option<Arc<dyn DbBackend>> {
    MAGASIN.read().ok().and_then(|m| m.clone())
}

fn lire_enregistrement(db: &Arc<dyn DbBackend>, udn: &str) -> Option<Enregistrement> {
    let brut = SettingsRepo::with_backend(db.clone())
        .get(&cle_reglage(udn))
        .map_err(|e| warn!(device_id = %udn, error = %e, "dlna_compat_set_uri_lecture_echouee"))
        .ok()
        .flatten()?;
    serde_json::from_str(&brut)
        .map_err(|e| warn!(device_id = %udn, error = %e, "dlna_compat_set_uri_illisible"))
        .ok()
}

/// L'entrée de l'appareil, relue en base la première fois. La mémoire gagne
/// sur la base : elle porte ce qui a été appris depuis le démarrage.
fn appareil<'a>(m: &'a mut HashMap<String, Appareil>, udn: &str) -> &'a mut Appareil {
    let a = m.entry(udn.to_string()).or_default();
    if !a.charge {
        a.charge = true;
        if let Some(db) = magasin()
            && let Some(e) = lire_enregistrement(&db, udn)
        {
            if a.firmware.is_none() {
                a.firmware = e.firmware;
            }
            for (mime, profil) in e.profils {
                a.profils.entry(mime).or_insert(profil);
            }
        }
    }
    a
}

/// Écrit l'état COURANT de l'appareil en base (ou efface la clé s'il n'a plus
/// aucun profil). Synchrone.
fn ecrire_maintenant(db: &Arc<dyn DbBackend>, udn: &str) -> Result<(), String> {
    let _ordre = ECRITURE.lock().unwrap_or_else(|e| e.into_inner());
    let instantane = APPAREILS.lock().ok().and_then(|m| {
        m.get(udn).map(|a| Enregistrement {
            firmware: a.firmware.clone(),
            profils: a.profils.clone().into_iter().collect(),
        })
    });
    let repo = SettingsRepo::with_backend(db.clone());
    match instantane {
        Some(e) if !e.profils.is_empty() => {
            let json = serde_json::to_string(&e).map_err(|e| e.to_string())?;
            repo.set(&cle_reglage(udn), &json)
        }
        _ => repo.delete(&cle_reglage(udn)),
    }
}

/// Recopie l'état de l'appareil en base, HORS de la pose d'URI : l'écrivain
/// SQLite peut être tenu par un scan, et une lecture ne doit pas l'attendre.
fn persister(udn: &str) {
    let Some(db) = magasin() else {
        return;
    };
    let udn = udn.to_string();
    let tache = move || {
        if let Err(e) = ecrire_maintenant(&db, &udn) {
            warn!(device_id = %udn, error = %e, "dlna_compat_set_uri_ecriture_echouee");
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            h.spawn_blocking(tache);
        }
        Err(_) => tache(),
    }
}

pub(crate) fn profil_memorise(udn: &str, mime_source: &str) -> Option<ProfilSetUri> {
    let mut m = APPAREILS.lock().ok()?;
    appareil(&mut m, udn)
        .profils
        .get(&cle_mime(mime_source))
        .map(|p| p.profil.clone())
}

pub(crate) fn memoriser_profil(udn: &str, mime_source: &str, profil: ProfilSetUri) {
    let change = match APPAREILS.lock() {
        Ok(mut m) => {
            let a = appareil(&mut m, udn);
            let cle = cle_mime(mime_source);
            if a.profils.get(&cle).map(|p| &p.profil) == Some(&profil) {
                false
            } else {
                let appris_le = Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
                a.profils.insert(cle, ProfilDate { profil, appris_le });
                true
            }
        }
        Err(_) => false,
    };
    if change {
        persister(udn);
    }
}

pub(crate) fn oublier_profil(udn: &str, mime_source: &str) {
    let retire = APPAREILS
        .lock()
        .ok()
        .map(|mut m| {
            appareil(&mut m, udn)
                .profils
                .remove(&cle_mime(mime_source))
                .is_some()
        })
        .unwrap_or(false);
    if retire {
        persister(udn);
    }
}

/// À la découverte d'un appareil : relit ses profils en base, et les OUBLIE
/// si sa version logicielle a changé depuis qu'ils ont été appris — une mise
/// à jour peut avoir corrigé le refus, ou en avoir créé un autre.
///
/// Sans version publiée (`None`), rien n'est oublié : beaucoup de
/// descriptions UPnP n'en portent pas, et le refus d'un profil appris suffit
/// alors à le faire oublier.
pub fn charger_pour_appareil(udn: &str, firmware: Option<&str>) {
    let firmware = firmware.map(str::trim).filter(|f| !f.is_empty());
    let mut a_persister = false;
    if let Ok(mut m) = APPAREILS.lock() {
        let a = appareil(&mut m, udn);
        if let (Some(ancien), Some(courant)) = (a.firmware.as_deref(), firmware)
            && ancien != courant
            && !a.profils.is_empty()
        {
            info!(
                device_id = %udn,
                firmware_ancien = %ancien,
                firmware_courant = %courant,
                profils = a.profils.len(),
                "dlna_compat_set_uri_oubliee_firmware_change"
            );
            a.profils.clear();
            a_persister = true;
        }
        if let Some(courant) = firmware
            && a.firmware.as_deref() != Some(courant)
        {
            a.firmware = Some(courant.to_string());
            a_persister |= !a.profils.is_empty();
        }
        if !a.profils.is_empty() {
            info!(
                device_id = %udn,
                profils = a.profils.len(),
                "dlna_compat_set_uri_rechargee"
            );
        }
    }
    if a_persister {
        persister(udn);
    }
}

/// Un profil appris, tel que le diagnostic de la zone l'expose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfilAppris {
    pub mime_source: String,
    #[serde(flatten)]
    pub profil: ProfilSetUri,
    pub appris_le: Option<String>,
}

/// La compatibilité apprise pour un appareil (diagnostic de la zone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompatibiliteAppareil {
    pub device_id: String,
    pub firmware: Option<String>,
    pub profils: Vec<ProfilAppris>,
}

/// Ce que Tune a appris pour cet appareil, relu en base au besoin.
pub fn compatibilite_de(udn: &str) -> CompatibiliteAppareil {
    let mut sortie = CompatibiliteAppareil {
        device_id: udn.to_string(),
        firmware: None,
        profils: Vec::new(),
    };
    if let Ok(mut m) = APPAREILS.lock() {
        let a = appareil(&mut m, udn);
        sortie.firmware = a.firmware.clone();
        sortie.profils = a
            .profils
            .iter()
            .map(|(mime, p)| ProfilAppris {
                mime_source: mime.clone(),
                profil: p.profil.clone(),
                appris_le: p.appris_le.clone(),
            })
            .collect();
        sortie
            .profils
            .sort_by(|x, y| x.mime_source.cmp(&y.mime_source));
    }
    sortie
}

/// « Réinitialiser la compatibilité » : oublie tous les profils de
/// l'appareil, en mémoire ET en base, avant de rendre la main. Rend le nombre
/// de profils oubliés. Synchrone : à appeler hors de l'exécuteur.
pub fn reinitialiser_compatibilite(udn: &str) -> Result<usize, String> {
    let oublies = match APPAREILS.lock() {
        Ok(mut m) => {
            let a = appareil(&mut m, udn);
            let n = a.profils.len();
            a.profils.clear();
            n
        }
        Err(e) => return Err(e.to_string()),
    };
    if let Some(db) = magasin() {
        ecrire_maintenant(&db, udn)?;
    }
    info!(device_id = %udn, profils = oublies, "dlna_compat_set_uri_reinitialisee");
    Ok(oublies)
}

/// Bancs : simule un redémarrage de Tune pour CET appareil — la mémoire
/// process est vidée, la base reste.
#[cfg(test)]
pub(crate) fn oublier_en_memoire_seulement(udn: &str) {
    if let Ok(mut m) = APPAREILS.lock() {
        m.remove(udn);
    }
}

#[cfg(test)]
pub(crate) fn base_de_test() -> Arc<dyn DbBackend> {
    static BASE: std::sync::OnceLock<Arc<dyn DbBackend>> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().expect("base mémoire");
        db.init_schema().expect("schéma");
        crate::db::migrations::run_migrations(&db).expect("migrations");
        let db: Arc<dyn DbBackend> = Arc::new(db);
        installer_persistance(db.clone());
        db
    })
    .clone()
}

/// Bancs : la valeur rangée en base pour cet appareil, telle quelle.
#[cfg(test)]
pub(crate) fn valeur_en_base(udn: &str) -> Option<String> {
    SettingsRepo::with_backend(base_de_test())
        .get(&cle_reglage(udn))
        .ok()
        .flatten()
}

/// Le `protocolInfo` de la première ressource d'une DIDL, échappée (telle
/// qu'elle part dans `CurrentURIMetaData`) ou non. `None` pour une DIDL vide
/// ou sans `protocolInfo`. Sert au journal : chaque essai dit ce qu'il a
/// envoyé.
pub(crate) fn protocol_info_du_didl(metadata: &str) -> Option<String> {
    for (ouverture, fermeture) in [("protocolInfo=&quot;", "&quot;"), ("protocolInfo=\"", "\"")] {
        if let Some(debut) = metadata.find(ouverture) {
            let reste = &metadata[debut + ouverture.len()..];
            let fin = reste.find(fermeture)?;
            return Some(reste[..fin].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn faute(code: u16, description: &str) -> String {
        format!(
            "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>{code}</errorCode><errorDescription>{description}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
        )
    }

    #[test]
    fn classe_les_trois_fautes_de_format_et_laisse_les_autres() {
        assert_eq!(
            classer_faute_set_uri(&faute(501, "Action Failed")),
            FauteSetUri::ActionEchouee501
        );
        assert_eq!(
            classer_faute_set_uri(&faute(714, "Illegal MIME-type")),
            FauteSetUri::MimeIllegal714
        );
        assert_eq!(
            classer_faute_set_uri(&faute(716, "Resource not found")),
            FauteSetUri::RessourceIntrouvable716
        );
        // 701 « Transition not available » a sa propre reprise ; 402 dit
        // « arguments invalides » : ni l'un ni l'autre n'appelle un repli.
        assert_eq!(
            classer_faute_set_uri(&faute(701, "Transition not available")),
            FauteSetUri::Autre
        );
        assert_eq!(
            classer_faute_set_uri(&faute(402, "Invalid Args")),
            FauteSetUri::Autre
        );
        // Forme historique sans code : la description suffit pour le 714.
        assert_eq!(
            classer_faute_set_uri("<UPnPError>Illegal MIME-Type</UPnPError>"),
            FauteSetUri::MimeIllegal714
        );
    }

    #[test]
    fn la_memoire_est_par_appareil_et_par_mime_source() {
        let udn = "uuid:test-memoire-repli-set-uri";
        let profil = ProfilSetUri {
            mime_annonce: "audio/x-flac".into(),
            niveau_didl_min: 1,
            servi_sous_ce_mime: true,
        };
        memoriser_profil(udn, "audio/FLAC", profil.clone());
        assert_eq!(profil_memorise(udn, "audio/flac"), Some(profil));
        assert_eq!(profil_memorise(udn, "audio/mpeg"), None);
        assert_eq!(profil_memorise("uuid:autre-appareil", "audio/flac"), None);
        oublier_profil(udn, "audio/flac");
        assert_eq!(profil_memorise(udn, "audio/flac"), None);
    }

    #[test]
    fn lit_le_protocol_info_echappe_ou_non() {
        let echappe = "&lt;res protocolInfo=&quot;http-get:*:audio/flac:DLNA.ORG_OP=01&quot; duration=&quot;0:01:00.000&quot;&gt;";
        assert_eq!(
            protocol_info_du_didl(echappe).as_deref(),
            Some("http-get:*:audio/flac:DLNA.ORG_OP=01")
        );
        let brut = r#"<res protocolInfo="http-get:*:audio/wav:*">"#;
        assert_eq!(
            protocol_info_du_didl(brut).as_deref(),
            Some("http-get:*:audio/wav:*")
        );
        assert_eq!(protocol_info_du_didl(""), None);
    }
}
