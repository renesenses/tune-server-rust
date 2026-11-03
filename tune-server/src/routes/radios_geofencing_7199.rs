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

/// Garde le choix explicite autorisé, sinon le principal autorisé, puis le
/// meilleur débit (sans perte d'abord). L'ordre publié départage les ex æquo.
/// Les alias v1 restent utiles au rapprochement, mais ne contournent jamais
/// les restrictions de la liste détaillée v2.
pub(super) fn select_stream(
    fiche: &StationAnnuaire,
    country: Option<&str>,
    preferred: Option<&str>,
) -> Option<FluxAnnuaire> {
    let country = country.and_then(country_code);
    let streams = if !fiche.streams_declared && fiche.flux.is_empty() {
        fiche.flux_proposes()
    } else {
        fiche.flux.clone()
    };
    streams
        .into_iter()
        .rev()
        .filter(|f| {
            f.restricted_to_countries.is_empty()
                || country.as_deref().is_some_and(|c| {
                    f.restricted_to_countries
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(c))
                })
        })
        .max_by_key(|f| {
            (
                preferred
                    .is_some_and(|p| super::flux_normalise(p) == super::flux_normalise(&f.url)),
                f.primary,
                f.codec.as_deref().is_some_and(|c| {
                    c.eq_ignore_ascii_case("flac") || c.eq_ignore_ascii_case("alac")
                }),
                f.bitrate.unwrap_or(0),
                f.sample_rate.unwrap_or(0),
            )
        })
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
        Some(id) => f.external_id.as_ref() == Some(id),
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
    // La première fiche d'un identifiant l'emporte, comme dans `fiche_suivie`.
    let mut par_id: std::collections::HashMap<&str, &StationAnnuaire> = Default::default();
    for f in fiches.iter() {
        if let Some(id) = f.external_id.as_deref() {
            par_id.entry(id).or_insert(f);
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
