//! Choix du flux selon le pays déclaré, sans déduire un pays de la langue.

use super::{FluxAnnuaire, StationAnnuaire};
use crate::state::AppState;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use tune_core::db::{
    radio_repo::{RadioRepo, RadioStation},
    settings_repo::SettingsRepo,
};

pub const COUNTRY_SETTING: &str = "radios_country";

pub fn country_code(raw: &str) -> Option<String> {
    let raw = raw.trim();
    (raw.len() == 2
        && raw.bytes().all(|b| b.is_ascii_alphabetic())
        && !raw.eq_ignore_ascii_case("zz"))
    .then(|| raw.to_ascii_uppercase())
}

/// Une restriction mal formée ne devient jamais une autorisation mondiale.
pub(super) fn restrictions(value: Option<&Value>) -> Vec<String> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|v| {
                v.as_str()
                    .and_then(country_code)
                    .unwrap_or_else(|| "invalid".into())
            })
            .collect(),
        Some(_) => vec!["invalid".into()],
    }
}

pub fn normalize_setting(
    values: &mut serde_json::Map<String, Value>,
) -> Result<(), crate::error::AppError> {
    let Some(value) = values.get(COUNTRY_SETTING) else {
        return Ok(());
    };
    let normalized = match value {
        Value::Null => String::new(),
        Value::String(s) if s.trim().is_empty() || s.trim().eq_ignore_ascii_case("auto") => {
            String::new()
        }
        Value::String(s) => country_code(s).ok_or_else(|| {
            crate::error::AppError::bad_request(
                "Pays des radios : indiquez un code à deux lettres (FR, GB…) ou laissez vide.",
            )
        })?,
        _ => {
            return Err(crate::error::AppError::bad_request(
                "Pays des radios : un code à deux lettres est attendu.",
            ));
        }
    };
    values.insert(COUNTRY_SETTING.into(), Value::String(normalized));
    Ok(())
}

fn listener_country(state: &AppState) -> Option<String> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(COUNTRY_SETTING)
        .ok()
        .flatten()
        .as_deref()
        .and_then(country_code)
}

/// #7515 — le rang d'un flux, du meilleur au moins bon : sans perte d'abord,
/// puis le débit ; à qualité égale le principal de la fiche, puis la
/// fréquence d'échantillonnage, que l'annuaire omet souvent (FIP : le principal
/// sans fréquence et son alias à 48 kHz sont le même 192 kbit/s).
pub(crate) fn rang(f: &FluxAnnuaire) -> (bool, i32, bool, i32) {
    (
        f.codec
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case("flac") || c.eq_ignore_ascii_case("alac")),
        f.bitrate.unwrap_or(0),
        f.primary,
        f.sample_rate.unwrap_or(0),
    )
}

/// #7515 — ce qu'une restriction de pays permet, commun au choix du flux
/// ([`select_stream`]) et à ce que l'annuaire en annonce
/// ([`marquer_le_flux_par_defaut`]) : `Some(true)` pour un flux libre ou ouvert
/// au pays d'écoute, `Some(false)` pour un flux réservé à d'autres pays ou
/// marqué `invalid`, `None` pour un flux restreint quand ce pays n'est pas
/// renseigné.
pub(crate) fn permis_dans_le_pays<S: AsRef<str>>(
    pays: &[S],
    country: Option<&str>,
) -> Option<bool> {
    if pays.is_empty() {
        return Some(true);
    }
    match country {
        Some(c) => Some(pays.iter().any(|p| p.as_ref().eq_ignore_ascii_case(c))),
        None if pays.iter().any(|p| p.as_ref() == "invalid") => Some(false),
        None => None,
    }
}

/// Garde le choix explicite autorisé, sinon le meilleur flux autorisé selon
/// [`rang`]. #7515 : le principal ne fait plus que départager deux flux de
/// même qualité ; l'ordre publié départage le reste.
/// Les alias v1 restent utiles au rapprochement, mais ne contournent jamais
/// les restrictions de la liste détaillée v2.
pub(super) fn select_stream(
    fiche: &StationAnnuaire,
    country: Option<&str>,
    preferred: Option<&str>,
) -> Option<FluxAnnuaire> {
    let choisi = |f: &FluxAnnuaire| {
        preferred.is_some_and(|p| super::flux_normalise(p) == super::flux_normalise(&f.url))
    };
    flux_autorises(fiche, country, preferred)
        .into_iter()
        .rev()
        .max_by_key(|f| (choisi(f), rang(f)))
}

/// #8110 — les flux de la fiche que la lecture peut essayer après `url`,
/// quand le diffuseur refuse celui-ci : les autres flux autorisés pour le pays
/// d'écoute, du meilleur au moins bon ([`rang`]), sans `url` lui-même. Vide
/// quand `url` n'est le flux d'aucune fiche, ou que la fiche n'a pas d'autre
/// flux autorisé.
pub(crate) fn replis_autorises(state: &AppState, url: &str) -> Vec<String> {
    let Ok(fiches) = state.annuaire_radios.read() else {
        return Vec::new();
    };
    let Some(fiche) = fiches
        .iter()
        .find(|f| super::super::radios_annuaire::flux_de_la_fiche(f, url))
    else {
        return Vec::new();
    };
    let country = listener_country(state);
    let joue = super::flux_normalise(url);
    let mut flux: Vec<FluxAnnuaire> = flux_autorises(fiche, country.as_deref(), Some(url))
        .into_iter()
        .filter(|f| super::flux_normalise(&f.url) != joue)
        .collect();
    // Tri stable : l'ordre de la fiche départage les ex æquo, comme
    // `flux_proposes`.
    flux.sort_by_key(|f| std::cmp::Reverse(rang(f)));
    flux.into_iter().map(|f| f.url).collect()
}

/// Les flux de la fiche que le pays d'écoute permet, dans l'ordre de la fiche
/// — le filtre de [`select_stream`], partagé avec [`replis_autorises`].
fn flux_autorises(
    fiche: &StationAnnuaire,
    country: Option<&str>,
    preferred: Option<&str>,
) -> Vec<FluxAnnuaire> {
    let country = country.and_then(country_code);
    let streams = if !fiche.streams_declared && fiche.flux.is_empty() {
        fiche.flux_proposes()
    } else {
        fiche.flux.clone()
    };
    let choisi = |f: &FluxAnnuaire| {
        preferred.is_some_and(|p| super::flux_normalise(p) == super::flux_normalise(&f.url))
    };
    // #7199 : sans flux libre, l'adresse enregistrée n'est pas un choix de
    // l'auditeur mais le repli posé à l'ajout (« c'est sa lecture qui sera
    // refusée ») ; pays inconnu, on refuse et on demande le pays. Avec un
    // flux libre, l'ajout aurait enregistré celui-ci : une adresse restreinte
    // ne peut venir que d'un choix explicite (#7177), que #7658 respecte.
    let flux_libre = streams.iter().any(|f| f.restricted_to_countries.is_empty());
    streams
        .into_iter()
        .filter(|f| {
            match permis_dans_le_pays(&f.restricted_to_countries, country.as_deref()) {
                Some(permis) => permis,
                // #7658/#7660 : pays d'écoute non renseigné. Le flux que
                // l'auditeur a choisi lui-même reste le sien : on ne le
                // remplace pas en silence par le seul flux sans
                // restriction (BBC : « nonuk » HE-AAC 96 kbit/s au lieu
                // du 320 kbit/s choisi). S'il est vraiment bloqué chez
                // lui, la lecture échoue et il en choisit un autre.
                None => flux_libre && choisi(f),
            }
        })
        .collect()
}

/// #7515 — le flux que la lecture prendra sans choix de l'auditeur, et ce que
/// le serveur sait de chaque flux pour le pays d'écoute, posés sur les flux
/// (`streams`) de chaque fiche de `corps` (le corps de `GET /radios/directory`) :
/// - `par_defaut` : c'est le flux joué si l'auditeur n'en choisit aucun ;
///   un flux réservé à d'autres pays ne l'est jamais ;
/// - `autorise` : `true` ou `false` selon le pays d'écoute, `null` pour un
///   flux restreint quand ce pays n'est pas renseigné.
///
/// L'écran peut ainsi présélectionner un flux jouable plutôt que le principal
/// de la fiche (BBC Radio 6 Music : le principal est refusé hors de sa zone).
pub(crate) fn marquer_le_flux_par_defaut(state: &AppState, corps: &mut Value) {
    let Ok(fiches) = state.annuaire_radios.read() else {
        return;
    };
    let country = listener_country(state);
    let Some(items) = corps.get_mut("items").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        let fiche = item
            .get("external_id")
            .and_then(Value::as_str)
            .and_then(|id| fiches.iter().find(|f| f.porte_la_cle(id)));
        let Some(fiche) = fiche else {
            continue;
        };
        let defaut = select_stream(fiche, country.as_deref(), None).map(|f| f.url);
        let Some(streams) = item.get_mut("streams").and_then(Value::as_array_mut) else {
            continue;
        };
        for s in streams {
            let Some(o) = s.as_object_mut() else {
                continue;
            };
            let url = o.get("url").and_then(Value::as_str).unwrap_or_default();
            let par_defaut = defaut
                .as_deref()
                .is_some_and(|d| super::flux_normalise(d) == super::flux_normalise(url));
            let pays: Vec<&str> = o
                .get("restricted_to_countries")
                .and_then(Value::as_array)
                .map(|p| p.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let autorise = json!(permis_dans_le_pays(&pays, country.as_deref()));
            o.insert("par_defaut".into(), json!(par_defaut));
            o.insert("autorise".into(), autorise);
        }
    }
}

#[derive(Debug)]
pub(crate) struct CountryUnavailable {
    country: Option<String>,
}

impl CountryUnavailable {
    pub(crate) fn message(&self) -> String {
        match self.country.as_deref() {
        Some(c) => format!("Aucun flux de cette radio n’est autorisé dans votre pays ({c})."),
        None => "Aucun flux sans restriction n’est disponible pour cette radio. Votre pays d’écoute n’est pas renseigné.".into(),
        }
    }
}

impl IntoResponse for CountryUnavailable {
    fn into_response(self) -> Response {
        (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({
            "output_sent": false, "error": self.message(), "error_code": "radio_country_unavailable", "country": self.country,
        }))).into_response()
    }
}

pub(crate) fn stream_for_directory(
    state: &AppState,
    fiche: &StationAnnuaire,
    preferred: Option<&str>,
) -> Result<FluxAnnuaire, CountryUnavailable> {
    let country = listener_country(state);
    select_stream(fiche, country.as_deref(), preferred).ok_or(CountryUnavailable { country })
}

/// Une radio enregistrée suit son lien stable, ou son URL exacte si elle
/// n'a pas encore de lien. Aucun rapprochement par nom pour changer un flux.
pub(super) fn stream_for_saved(
    state: &AppState,
    radio: &RadioStation,
) -> Result<String, CountryUnavailable> {
    let links = RadioRepo::with_backend(state.backend.clone())
        .liens_annuaire()
        .unwrap_or_default();
    let link = radio.id.and_then(|id| links.get(&id));
    let fiches = state.annuaire_radios.read().ok();
    let fiche = fiches
        .as_ref()
        .and_then(|fiches| fiche_suivie(fiches, link, &radio.url));
    match fiche {
        Some(f) => stream_for_directory(state, f, Some(&radio.url)).map(|s| s.url),
        None => Ok(radio.url.clone()),
    }
}

/// La fiche que suit la lecture d'une station enregistrée : par son lien
/// stable, sinon par son adresse exacte.
fn fiche_suivie<'a>(
    fiches: &'a [StationAnnuaire],
    link: Option<&String>,
    url: &str,
) -> Option<&'a StationAnnuaire> {
    fiches.iter().find(|f| match link {
        // #7500 — ou l'ancienne clé `mozaiklabs:ID` d'une fiche numérotée depuis.
        Some(id) => f.porte_la_cle(id),
        None => super::super::radios_annuaire::flux_de_la_fiche(f, url),
    })
}

/// #7721 — le débit affiché d'une station enregistrée est celui du flux que
/// [`stream_for_saved`] jouera, pas celui de l'adresse enregistrée.
///
/// Une station peut garder l'adresse d'un flux réservé à d'autres pays (flux
/// choisi dans la fiche, ou enregistré avant que l'annuaire publie ses
/// restrictions). La lecture l'écarte et prend un autre flux, souvent moins
/// haut ; l'écran annonçait pourtant toujours le débit enregistré : « 320 »
/// pour un flux reçu à 96. Quand le flux joué n'est pas celui enregistré,
/// `bitrate` et `codec` deviennent les siens, l'ancien débit reste sous
/// `bitrate_enregistre`, et `flux_ecoute` dit lequel et pourquoi. Rien n'est
/// écrit en base.
///
/// `corps` : une station sérialisée, ou un tableau de stations.
pub(crate) fn annoncer_le_debit_ecoute(state: &AppState, corps: &mut Value) {
    let Ok(fiches) = state.annuaire_radios.read() else {
        return;
    };
    if fiches.is_empty() {
        return;
    }
    let links = RadioRepo::with_backend(state.backend.clone())
        .liens_annuaire()
        .unwrap_or_default();
    let country = listener_country(state);
    // Une liste de « Mes radios » peut compter des centaines de stations :
    // les fiches liées se retrouvent par leur identifiant en une table, faite
    // une fois par appel, plutôt qu'en parcourant l'annuaire pour chacune.
    // La première fiche d'un identifiant l'emporte, comme dans `fiche_suivie`,
    // qui reconnaît aussi l'ancienne clé `mozaiklabs:ID` (#7500).
    let mut par_id: std::collections::HashMap<&str, &StationAnnuaire> = Default::default();
    for f in fiches.iter() {
        for cle in [f.external_id.as_deref(), f.cle_locale.as_deref()]
            .into_iter()
            .flatten()
        {
            par_id.entry(cle).or_insert(f);
        }
    }
    let stations: Vec<&mut Value> = match corps {
        Value::Array(stations) => stations.iter_mut().collect(),
        station => vec![station],
    };
    for station in stations {
        let Some(o) = station.as_object_mut() else {
            continue;
        };
        let Some(url) = o
            .get("stream_url")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let link = o
            .get("id")
            .and_then(Value::as_i64)
            .and_then(|id| links.get(&id));
        // Même règle que `fiche_suivie` : le lien, sinon l'adresse exacte.
        let fiche = match link {
            Some(id) => par_id.get(id.as_str()).copied(),
            None => fiche_suivie(&fiches, None, &url),
        };
        let Some(fiche) = fiche else {
            continue;
        };
        let Some(flux) = select_stream(fiche, country.as_deref(), Some(&url)) else {
            continue;
        };
        if super::flux_normalise(&flux.url) == super::flux_normalise(&url) {
            continue;
        }
        let enregistre = fiche
            .flux_proposes()
            .into_iter()
            .find(|f| super::flux_normalise(&f.url) == super::flux_normalise(&url));
        let raison = match enregistre {
            // L'adresse enregistrée est de la fiche mais pas pour ce pays.
            Some(f) if !f.restricted_to_countries.is_empty() => "restreint_au_pays",
            // La fiche ne publie plus cette adresse : la lecture suit le lien.
            _ => "hors_fiche",
        };
        let codec = flux.codec.as_deref().map(str::to_ascii_lowercase);
        let ancien = o.get("bitrate").cloned().unwrap_or(Value::Null);
        o.insert("bitrate_enregistre".into(), ancien);
        o.insert("bitrate".into(), json!(flux.bitrate));
        o.insert("codec".into(), json!(codec));
        o.insert(
            "flux_ecoute".into(),
            json!({
                "url": flux.url,
                "bitrate": flux.bitrate,
                "codec": codec,
                "raison": raison,
                "pays": country,
            }),
        );
    }
}

/// Les commandes génériques de zone portent une URL, pas un identifiant de
/// station. Elles appliquent la même sélection à la fiche de cette URL.
pub(crate) fn stream_for_url(state: &AppState, url: &str) -> Result<String, CountryUnavailable> {
    let fiches = state.annuaire_radios.read().ok();
    let fiche = fiches.as_ref().and_then(|fiches| {
        fiches
            .iter()
            .find(|f| super::super::radios_annuaire::flux_de_la_fiche(f, url))
    });
    match fiche {
        Some(f) => stream_for_directory(state, f, Some(url)).map(|s| s.url),
        None => Ok(url.to_string()),
    }
}

#[cfg(test)]
#[path = "radios_geofencing_7199_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "radios_debit_ecoute_7721_tests.rs"]
mod debit_ecoute_tests;

#[cfg(test)]
#[path = "radios_repli_8110_tests.rs"]
mod repli_8110_tests;
