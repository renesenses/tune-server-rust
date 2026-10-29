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
    let fiche = fiches.as_ref().and_then(|fiches| {
        fiches.iter().find(|f| match link {
            Some(id) => f.external_id.as_ref() == Some(id),
            None => super::super::radios_annuaire::flux_de_la_fiche(f, &radio.url),
        })
    });
    match fiche {
        Some(f) => stream_for_directory(state, f, Some(&radio.url)).map(|s| s.url),
        None => Ok(radio.url.clone()),
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
