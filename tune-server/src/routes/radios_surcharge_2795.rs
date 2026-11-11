//! web#2795 — la SURCHARGE LOCALE d'une station de l'annuaire.
//!
//! Depuis web#2443, une station de « Mes radios » liée à sa fiche de
//! l'annuaire (`radio_stations.external_id`) n'était plus modifiable : sa
//! fiche est tenue en central, et le relevé de l'annuaire (#7043) lui
//! reprenait son logo et l'adresse de son flux. Décision de Bertrand du 11/11 :
//! « Modifier » revient, et change CHEZ SOI le nom affiché, le logo et l'URL du
//! flux. La fiche de l'annuaire n'est jamais modifiée ; le relevé ne touche
//! plus une station surchargée ; « Revenir à la fiche de l'annuaire » efface
//! la surcharge.
//!
//! La surcharge est une date (`radio_stations.surcharge_locale`, SQLite 144 /
//! PG 110) : NULL, la station suit sa fiche.

use super::*;

/// Le logo tel qu'il compte : une adresse vide vaut « pas de logo ».
fn logo_compte(st: &RadioStation) -> Option<&str> {
    st.logo_url
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
}

/// La modification `avant` → `apres` change-t-elle ce que la fiche fournit :
/// le nom affiché, l'adresse du flux (à la normalisation près) ou le logo ?
/// Le genre, le pays, le favori… ne surchargent rien : le relevé n'y touche
/// pas.
pub(crate) fn touche_la_fiche(avant: &RadioStation, apres: &RadioStation) -> bool {
    avant.name.trim() != apres.name.trim()
        || flux_normalise(&avant.url) != flux_normalise(&apres.url)
        || logo_compte(avant) != logo_compte(apres)
}

/// Après une modification enregistrée : une station LIÉE à sa fiche dont le
/// nom, le flux ou le logo a changé devient surchargée. Une station saisie à
/// la main n'a pas de fiche à surcharger : rien n'est posé.
pub(crate) fn noter_la_surcharge(repo: &RadioRepo, avant: &RadioStation, apres: &RadioStation) {
    let Some(id) = apres.id else {
        return;
    };
    if !touche_la_fiche(avant, apres) {
        return;
    }
    if repo.lien_annuaire(id).ok().flatten().is_none() {
        return;
    }
    if let Err(e) = repo.surcharger_localement(id, Some(&maintenant_rfc3339())) {
        tracing::warn!(id, erreur = %e, "radio_surcharge_locale_non_posee");
    }
}

/// Joint `surcharge_locale` (booléen) à chaque station de `corps` (une
/// station sérialisée ou un tableau) : le repère discret de l'écran, et la
/// condition du bouton « Revenir à la fiche de l'annuaire ».
pub(crate) fn marquer_les_surcharges(state: &AppState, corps: &mut Value) {
    let surchargees = RadioRepo::with_backend(state.backend.clone())
        .surcharges_locales()
        .unwrap_or_default();
    let stations: Vec<&mut Value> = match corps {
        Value::Array(stations) => stations.iter_mut().collect(),
        station => vec![station],
    };
    for st in stations {
        let Some(o) = st.as_object_mut() else {
            continue;
        };
        let Some(id) = o.get("id").and_then(Value::as_i64) else {
            continue;
        };
        o.insert(
            "surcharge_locale".into(),
            Value::Bool(surchargees.contains(&id)),
        );
    }
}

/// La station reprend ce que sa fiche fournit : nom, logo et flux. Une
/// adresse que la fiche publie (flux préféré, #7177) est gardée.
pub(crate) fn reprendre_la_fiche(fiche: &StationAnnuaire, station: &mut RadioStation) {
    station.name = fiche.name.clone();
    station.logo_url = fiche.logo_url.clone();
    if !fiche.stream_url.trim().is_empty() && !flux_de_la_fiche(fiche, &station.url) {
        station.url = fiche.stream_url.clone();
        if let Some(c) = fiche.codec.as_deref() {
            station.codec = Some(c.to_ascii_lowercase());
        }
        if fiche.bitrate.is_some() {
            station.bitrate = fiche.bitrate;
        }
    }
}

/// `POST /radios/directory/revert/{radio_id}` — « Revenir à la fiche de
/// l'annuaire » : la surcharge locale est effacée, la station reprend le nom,
/// le logo et le flux de sa fiche. 404 `radio_inconnue` ; 409
/// `radio_sans_fiche_liee` pour une station sans fiche (saisie à la main, ou
/// fiche absente du relevé en main) : rien ne change.
pub(crate) async fn revenir_a_la_fiche(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(radio_id): Path<i64>,
) -> Response {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let Some(mut station) = repo.get(radio_id).ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "radio_inconnue"})),
        )
            .into_response();
    };
    let lien = repo.lien_annuaire(radio_id).ok().flatten();
    let Some(fiche) = lien.as_deref().and_then(|ext| fiche_par_id(&state, ext)) else {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "radio_sans_fiche_liee"})),
        )
            .into_response();
    };
    reprendre_la_fiche(&fiche, &mut station);
    if let Err(e) = repo.update(&station) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Err(e) = repo.surcharger_localement(radio_id, None) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    state.event_bus.emit(
        "library.radios_changed",
        json!({"action": "updated", "id": radio_id}),
    );
    let mut sortie = json!(station);
    super::super::radios_libelles::enrichir(&mut sortie, &crate::i18n::lang_from_header(&headers));
    if let Some(o) = sortie.as_object_mut() {
        o.insert(
            "annuaire".into(),
            rattachement(&fiche, Lien::Id, &station.url),
        );
        o.insert("surcharge_locale".into(), Value::Bool(false));
    }
    super::super::radios::geofencing::annoncer_le_debit_ecoute(&state, &mut sortie);
    Json(sortie).into_response()
}

#[cfg(test)]
#[path = "radios_surcharge_2795_tests.rs"]
mod tests;
