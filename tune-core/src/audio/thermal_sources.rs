//! #6450 — les sources de température de Windows, essayées dans l'ordre, et
//! ce qu'on dit de chacune.
//!
//! Sur un Ryzen 7 3800X sous Windows 11, la seule source lue jusqu'ici (les
//! compteurs PDH « Thermal Zone Information ») ne rendait rien, et rien ne
//! disait pourquoi : chaque échec rendait une liste vide, code d'erreur
//! perdu. Aucun rapport ne pouvait trancher entre « compteur absent »,
//! « droits » et « aucune zone thermique déclarée par la carte ».
//!
//! Désormais, sans pilote tiers ni droits d'administrateur, dans l'ordre :
//!
//! | Source | Ce qu'elle lit |
//! |---|---|
//! | `pdh_high_precision_temperature` | `\Thermal Zone Information(*)\High Precision Temperature`, dixièmes de kelvin |
//! | `pdh_temperature` | `\Thermal Zone Information(*)\Temperature`, kelvins |
//! | `wmi_cimv2_thermal_zone_information` | WMI `ROOT\CIMV2`, `Win32_PerfFormattedData_Counters_ThermalZoneInformation.HighPrecisionTemperature`, dixièmes de kelvin |
//! | `wmi_root_wmi_msacpi_thermal_zone` | WMI `ROOT\WMI`, `MSAcpi_ThermalZoneTemperature.CurrentTemperature`, dixièmes de kelvin |
//!
//! La première source qui rend au moins une valeur dans le domaine d'un
//! capteur (5 à 125 °C) l'emporte. Chaque source essayée est journalisée avec
//! le motif de son échec ([`EchecDeSource`]), au démarrage puis à chaque
//! CHANGEMENT de ce constat seulement, pas à chaque relecture.
//!
//! Le paquet du processeur (Tctl/Tdie d'un Ryzen) n'est lisible sous Windows
//! qu'avec un pilote noyau tiers : aucune de ces sources ne le donne. Sur une
//! carte de bureau qui ne déclare aucune zone thermique ACPI, elles peuvent
//! toutes échouer ; le journal le dit alors source par source, et les
//! analyses tournent à la vitesse réglée (voir `thermal::largeur_selon_la_chaleur`).
//!
//! Le choix et la journalisation sont de la logique pure, éprouvée partout
//! avec des sources simulées ; seuls les appels PDH et WMI sont propres à
//! Windows.

use std::sync::Mutex;

use tracing::{info, warn};

/// Pourquoi une source n'a rendu aucune température.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EchecDeSource {
    /// La source n'existe pas sur cette machine : compteur, classe ou espace
    /// de noms absent. Le détail porte le code de l'API.
    Absente(String),
    /// Windows refuse la lecture à ce compte (droits d'administrateur).
    AccesRefuse(String),
    /// La source répond, mais sans aucune instance : la carte ne déclare
    /// aucune zone thermique.
    AucuneInstance,
    /// Des valeurs, mais toutes hors du domaine d'un capteur (5 à 125 °C) :
    /// des sentinelles, pas des mesures.
    HorsBornes { valeurs: usize },
    /// Un autre échec de l'API, avec son code.
    Erreur(String),
}

impl EchecDeSource {
    /// Le mot stable écrit dans le journal (`motif`).
    pub fn id(&self) -> &'static str {
        match self {
            EchecDeSource::Absente(_) => "source_absente",
            EchecDeSource::AccesRefuse(_) => "acces_refuse",
            EchecDeSource::AucuneInstance => "aucune_instance",
            EchecDeSource::HorsBornes { .. } => "valeurs_hors_bornes",
            EchecDeSource::Erreur(_) => "erreur",
        }
    }

    /// Le détail lisible : code de l'API, ou nombre de valeurs écartées.
    pub fn detail(&self) -> String {
        match self {
            EchecDeSource::Absente(d)
            | EchecDeSource::AccesRefuse(d)
            | EchecDeSource::Erreur(d) => d.clone(),
            EchecDeSource::AucuneInstance => "aucune zone thermique déclarée".into(),
            EchecDeSource::HorsBornes { valeurs } => {
                format!("{valeurs} valeur(s) hors de 5 à 125 °C")
            }
        }
    }
}

/// Une source de température : un nom stable pour le journal, une lecture de
/// toutes ses instances en valeurs brutes, et leur conversion en °C (`None`
/// hors du domaine d'un capteur).
pub trait SourceDeTemperature {
    fn nom(&self) -> &'static str;
    fn lire_brut(&self) -> Result<Vec<f64>, EchecDeSource>;
    fn en_celsius(&self, brut: f64) -> Option<f64>;
}

/// Ce qu'a donné une source essayée : la plus chaude de ses instances en °C,
/// ou le motif de son échec.
#[derive(Debug, Clone, PartialEq)]
pub struct Essai {
    pub source: &'static str,
    pub resultat: Result<f64, EchecDeSource>,
}

/// Essaie les sources dans l'ordre jusqu'à la première lisible. Rend sa
/// température (la plus chaude de ses instances) et la liste des essais, la
/// source retenue comprise ; les sources suivantes ne sont pas consultées.
pub fn premiere_source_lisible(sources: &[&dyn SourceDeTemperature]) -> (Option<f64>, Vec<Essai>) {
    let mut essais = Vec::with_capacity(sources.len());
    for source in sources {
        let resultat = source.lire_brut().and_then(|brutes| {
            if brutes.is_empty() {
                return Err(EchecDeSource::AucuneInstance);
            }
            brutes
                .iter()
                .filter_map(|b| source.en_celsius(*b))
                .reduce(f64::max)
                .ok_or(EchecDeSource::HorsBornes {
                    valeurs: brutes.len(),
                })
        });
        let lue = resultat.as_ref().ok().copied();
        essais.push(Essai {
            source: source.nom(),
            resultat,
        });
        if lue.is_some() {
            return (lue, essais);
        }
    }
    (None, essais)
}

/// Le constat d'une série d'essais, sans la valeur lue : il ne change pas
/// quand la température varie, seulement quand une source se met à répondre
/// ou à échouer autrement.
type Constat = Vec<(&'static str, Option<&'static str>)>;

fn constat(essais: &[Essai]) -> Constat {
    essais
        .iter()
        .map(|e| (e.source, e.resultat.as_ref().err().map(EchecDeSource::id)))
        .collect()
}

/// Le dernier constat journalisé par le serveur.
static DERNIER_CONSTAT: Mutex<Option<Constat>> = Mutex::new(None);

/// Journalise les essais si leur constat a changé depuis `dernier` ; dit s'il
/// l'a fait. Une ligne par source essayée (son nom, le motif de son échec),
/// puis la conclusion : la source retenue, ou aucune.
pub fn journaliser_si_change(essais: &[Essai], dernier: &mut Option<Constat>) -> bool {
    let nouveau = constat(essais);
    if dernier.as_ref() == Some(&nouveau) {
        return false;
    }
    *dernier = Some(nouveau);
    for essai in essais {
        match &essai.resultat {
            Ok(temp_c) => info!(
                source = essai.source,
                temp_c = *temp_c,
                "thermal_source_retenue — température lue par cette source"
            ),
            Err(e) => info!(
                source = essai.source,
                motif = e.id(),
                detail = %e.detail(),
                "thermal_source_echec — cette source ne rend aucune température"
            ),
        }
    }
    if !essais.iter().any(|e| e.resultat.is_ok()) {
        warn!(
            sources_essayees = essais.len(),
            "thermal_aucune_source — aucune source de température lisible sur cette machine : \
             les analyses de fond tournent à la vitesse réglée, sans garde thermique"
        );
    }
    true
}

/// Le chemin de production : choisir, puis journaliser au changement.
pub fn lire_et_journaliser(sources: &[&dyn SourceDeTemperature]) -> Option<f64> {
    let (temp_c, essais) = premiere_source_lisible(sources);
    let mut dernier = DERNIER_CONSTAT.lock().unwrap_or_else(|e| e.into_inner());
    journaliser_si_change(&essais, &mut dernier);
    temp_c
}

/// Lecture des zones thermiques par PDH et WMI (#5410, #6450). Aucune
/// dépendance nouvelle : le crate `windows` est déjà tiré par cpal, il ne
/// gagne que des fonctionnalités (`Win32_System_Performance`, `…_Wmi`,
/// `…_Ole`, `…_Variant`, `…_Rpc`).
///
/// ⚠️ Compilé pour `x86_64-pc-windows-msvc` (CI Windows), jamais exécuté par
/// les épreuves : ce qu'il rend sur une vraie machine se lit dans le journal
/// (`thermal_source_echec`, `thermal_source_retenue`).
#[cfg(target_os = "windows")]
pub(crate) mod windows_sources {
    use crate::audio::thermal::{celsius_depuis_dixiemes_de_kelvin, celsius_depuis_kelvins};

    /// Les zones thermiques, par la première source lisible ; journalise les
    /// essais au changement.
    pub(crate) fn temperature_celsius() -> Option<f64> {
        let pdh_precise = pdh::Compteur {
            nom: "pdh_high_precision_temperature",
            chemin: windows::core::w!("\\Thermal Zone Information(*)\\High Precision Temperature"),
            conversion: celsius_depuis_dixiemes_de_kelvin,
        };
        let pdh_kelvins = pdh::Compteur {
            nom: "pdh_temperature",
            chemin: windows::core::w!("\\Thermal Zone Information(*)\\Temperature"),
            conversion: celsius_depuis_kelvins,
        };
        let wmi_compteurs = wmi::Requete {
            nom: "wmi_cimv2_thermal_zone_information",
            espace: "ROOT\\CIMV2",
            requete: "SELECT HighPrecisionTemperature FROM \
                      Win32_PerfFormattedData_Counters_ThermalZoneInformation",
            propriete: windows::core::w!("HighPrecisionTemperature"),
            conversion: celsius_depuis_dixiemes_de_kelvin,
        };
        // Documentée comme réservée à l'administrateur, jamais mesurée :
        // essayée quand même, un refus se lit `acces_refuse` au journal.
        let wmi_acpi = wmi::Requete {
            nom: "wmi_root_wmi_msacpi_thermal_zone",
            espace: "ROOT\\WMI",
            requete: "SELECT CurrentTemperature FROM MSAcpi_ThermalZoneTemperature",
            propriete: windows::core::w!("CurrentTemperature"),
            conversion: celsius_depuis_dixiemes_de_kelvin,
        };
        super::lire_et_journaliser(&[&pdh_precise, &pdh_kelvins, &wmi_compteurs, &wmi_acpi])
    }

    mod pdh {
        use super::super::{EchecDeSource, SourceDeTemperature};
        use windows::Win32::System::Performance::{
            PDH_ACCESS_DENIED, PDH_CSTATUS_NO_COUNTER, PDH_CSTATUS_NO_INSTANCE,
            PDH_CSTATUS_NO_OBJECT, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER,
            PDH_HQUERY, PDH_MORE_DATA, PDH_NO_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
            PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
        };
        use windows::core::PCWSTR;

        /// `PDH_CSTATUS_VALID_DATA` et `PDH_CSTATUS_NEW_DATA`.
        const VALEUR_VALIDE: [u32; 2] = [0, 1];

        pub(super) struct Compteur {
            pub nom: &'static str,
            pub chemin: PCWSTR,
            pub conversion: fn(f64) -> Option<f64>,
        }

        impl SourceDeTemperature for Compteur {
            fn nom(&self) -> &'static str {
                self.nom
            }

            fn lire_brut(&self) -> Result<Vec<f64>, EchecDeSource> {
                lire(self.chemin)
            }

            fn en_celsius(&self, brut: f64) -> Option<f64> {
                (self.conversion)(brut)
            }
        }

        /// Un code PDH devenu motif.
        fn echec(etape: &str, code: u32) -> EchecDeSource {
            let detail = format!("{etape} : PDH 0x{code:08X}");
            match code {
                PDH_CSTATUS_NO_OBJECT | PDH_CSTATUS_NO_COUNTER => EchecDeSource::Absente(detail),
                PDH_CSTATUS_NO_INSTANCE | PDH_NO_DATA => EchecDeSource::AucuneInstance,
                PDH_ACCESS_DENIED => EchecDeSource::AccesRefuse(detail),
                _ => EchecDeSource::Erreur(detail),
            }
        }

        /// Les valeurs brutes de toutes les instances d'un compteur.
        fn lire(chemin: PCWSTR) -> Result<Vec<f64>, EchecDeSource> {
            let mut requete = PDH_HQUERY::default();
            // SAFETY : appels PDH documentés ; chaque sortie pointe une
            // variable locale vivante, le tampon est dimensionné par l'appel
            // de sondage et aligné sur `PDH_FMT_COUNTERVALUE_ITEM_W`, la
            // requête est fermée sur tous les chemins.
            unsafe {
                let code = PdhOpenQueryW(PCWSTR::null(), 0, &mut requete);
                if code != 0 {
                    return Err(echec("PdhOpenQueryW", code));
                }
                let valeurs = lire_dans(requete, chemin);
                let _ = PdhCloseQuery(requete);
                valeurs
            }
        }

        unsafe fn lire_dans(
            requete: PDH_HQUERY,
            chemin: PCWSTR,
        ) -> Result<Vec<f64>, EchecDeSource> {
            let mut compteur = PDH_HCOUNTER::default();
            unsafe {
                let code = PdhAddEnglishCounterW(requete, chemin, 0, &mut compteur);
                if code != 0 {
                    return Err(echec("PdhAddEnglishCounterW", code));
                }
                // Une température est une valeur instantanée : une collecte suffit.
                let code = PdhCollectQueryData(requete);
                if code != 0 {
                    return Err(echec("PdhCollectQueryData", code));
                }
                let mut octets = 0u32;
                let mut nombre = 0u32;
                let code = PdhGetFormattedCounterArrayW(
                    compteur,
                    PDH_FMT_DOUBLE,
                    &mut octets,
                    &mut nombre,
                    None,
                );
                if code != PDH_MORE_DATA {
                    return Err(echec("PdhGetFormattedCounterArrayW (taille)", code));
                }
                if octets == 0 {
                    return Err(EchecDeSource::AucuneInstance);
                }
                let taille = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
                let cases = (octets as usize).div_ceil(taille) + 1;
                let mut tampon = vec![PDH_FMT_COUNTERVALUE_ITEM_W::default(); cases];
                let code = PdhGetFormattedCounterArrayW(
                    compteur,
                    PDH_FMT_DOUBLE,
                    &mut octets,
                    &mut nombre,
                    Some(tampon.as_mut_ptr()),
                );
                if code != 0 {
                    return Err(echec("PdhGetFormattedCounterArrayW", code));
                }
                Ok(tampon
                    .iter()
                    .take(nombre as usize)
                    .filter(|i| VALEUR_VALIDE.contains(&i.FmtValue.CStatus))
                    .map(|i| i.FmtValue.Anonymous.doubleValue)
                    .filter(|v| v.is_finite())
                    .collect())
            }
        }
    }

    mod wmi {
        use super::super::{EchecDeSource, SourceDeTemperature};
        use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
        use windows::Win32::System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoSetProxyBlanket, CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
        };
        use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
        use windows::Win32::System::Variant::{VARIANT, VariantClear, VariantToDouble};
        use windows::Win32::System::Wmi::{
            IEnumWbemClassObject, IWbemClassObject, IWbemLocator, WBEM_E_ACCESS_DENIED,
            WBEM_E_INVALID_CLASS, WBEM_E_INVALID_NAMESPACE, WBEM_E_NOT_FOUND,
            WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WbemLocator,
        };
        use windows::core::{BSTR, HRESULT, PCWSTR};

        /// Au-delà, une instance qui tarde vaut un échec : la lecture ne
        /// doit pas retenir la garde.
        const DELAI_PAR_INSTANCE_MS: i32 = 5_000;
        /// `E_ACCESSDENIED`.
        const E_ACCES_REFUSE: HRESULT = HRESULT(0x8007_0005_u32 as i32);

        pub(super) struct Requete {
            pub nom: &'static str,
            pub espace: &'static str,
            pub requete: &'static str,
            pub propriete: PCWSTR,
            pub conversion: fn(f64) -> Option<f64>,
        }

        impl SourceDeTemperature for Requete {
            fn nom(&self) -> &'static str {
                self.nom
            }

            fn lire_brut(&self) -> Result<Vec<f64>, EchecDeSource> {
                let _com = match Com::initialiser() {
                    Ok(c) => c,
                    Err(e) => return Err(echec("CoInitializeEx", e.code())),
                };
                // SAFETY : COM est initialisé sur ce fil pour toute la durée
                // de l'appel (`_com` est relâché après lui).
                unsafe { lire(self) }
            }

            fn en_celsius(&self, brut: f64) -> Option<f64> {
                (self.conversion)(brut)
            }
        }

        /// COM initialisé sur le fil courant, désinitialisé à la sortie
        /// seulement si c'est cet appel qui l'a initialisé. Un fil déjà en
        /// appartement à fil unique (`RPC_E_CHANGED_MODE`) garde le sien,
        /// qui sert aussi bien.
        struct Com {
            a_desinitialiser: bool,
        }

        impl Com {
            fn initialiser() -> windows::core::Result<Self> {
                // SAFETY : appel documenté ; équilibré par `Drop`.
                let code = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if code == RPC_E_CHANGED_MODE {
                    return Ok(Com {
                        a_desinitialiser: false,
                    });
                }
                code.ok()?;
                Ok(Com {
                    a_desinitialiser: true,
                })
            }
        }

        impl Drop for Com {
            fn drop(&mut self) {
                if self.a_desinitialiser {
                    // SAFETY : équilibre le `CoInitializeEx` réussi ci-dessus.
                    unsafe { CoUninitialize() };
                }
            }
        }

        /// Un HRESULT de WMI devenu motif.
        fn echec(etape: &str, code: HRESULT) -> EchecDeSource {
            let detail = format!("{etape} : HRESULT 0x{:08X}", code.0 as u32);
            let absente = [
                WBEM_E_INVALID_CLASS,
                WBEM_E_INVALID_NAMESPACE,
                WBEM_E_NOT_FOUND,
            ];
            if absente.iter().any(|s| s.0 == code.0) {
                EchecDeSource::Absente(detail)
            } else if code.0 == WBEM_E_ACCESS_DENIED.0 || code == E_ACCES_REFUSE {
                EchecDeSource::AccesRefuse(detail)
            } else {
                EchecDeSource::Erreur(detail)
            }
        }

        unsafe fn lire(r: &Requete) -> Result<Vec<f64>, EchecDeSource> {
            unsafe {
                let localisateur: IWbemLocator =
                    CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)
                        .map_err(|e| echec("CoCreateInstance(WbemLocator)", e.code()))?;
                let services = localisateur
                    .ConnectServer(
                        &BSTR::from(r.espace),
                        &BSTR::new(),
                        &BSTR::new(),
                        &BSTR::new(),
                        0,
                        &BSTR::new(),
                        None,
                    )
                    .map_err(|e| echec("ConnectServer", e.code()))?;
                CoSetProxyBlanket(
                    &services,
                    RPC_C_AUTHN_WINNT,
                    RPC_C_AUTHZ_NONE,
                    PCWSTR::null(),
                    RPC_C_AUTHN_LEVEL_CALL,
                    RPC_C_IMP_LEVEL_IMPERSONATE,
                    None,
                    EOAC_NONE,
                )
                .map_err(|e| echec("CoSetProxyBlanket", e.code()))?;
                let enumeration: IEnumWbemClassObject = services
                    .ExecQuery(
                        &BSTR::from("WQL"),
                        &BSTR::from(r.requete),
                        WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                        None,
                    )
                    .map_err(|e| echec("ExecQuery", e.code()))?;
                let mut valeurs = Vec::new();
                loop {
                    let mut objets: [Option<IWbemClassObject>; 1] = [None];
                    let mut rendus = 0u32;
                    let code = enumeration.Next(DELAI_PAR_INSTANCE_MS, &mut objets, &mut rendus);
                    // Une classe absente ou refusée ne se dit qu'ici, la
                    // requête étant semi-synchrone.
                    if code.is_err() {
                        return Err(echec("IEnumWbemClassObject::Next", code));
                    }
                    let Some(objet) = objets[0].take().filter(|_| rendus == 1) else {
                        break;
                    };
                    let mut valeur = VARIANT::default();
                    if objet.Get(r.propriete, 0, &mut valeur, None, None).is_ok() {
                        // Une propriété NULL ne se convertit pas : ignorée.
                        if let Ok(v) = VariantToDouble(&valeur)
                            && v.is_finite()
                        {
                            valeurs.push(v);
                        }
                    }
                    let _ = VariantClear(&mut valeur);
                }
                Ok(valeurs)
            }
        }
    }
}

#[cfg(test)]
#[path = "thermal_sources_6450_tests.rs"]
mod tests_6450;
