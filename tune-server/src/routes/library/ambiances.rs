//! Ambiances enregistrées — les recherches acoustiques que l'utilisateur veut
//! retrouver.
//!
//! La recherche acoustique (`/library/search/acoustic`) trouve des titres à
//! partir d'une formulation libre (« warm intimate late-night jazz »). Trouver
//! LA bonne formulation demande quelques essais, et rien ne la gardait : au
//! rechargement de la page, elle était perdue. Seules les huit ambiances
//! fournies survivaient, et elles sont codées dans le client.
//!
//! Le stockage suit le modèle des presets d'égaliseur (`eq_presets`) : un
//! tableau JSON dans `settings`, donc **aucune migration de schéma**. Il est en
//! revanche rangé **par profil** (`ambiances:{profile_id}`) : une ambiance est
//! un objet personnel, pas un réglage de la maison. Et côté serveur, pas dans
//! le navigateur — un `localStorage` ne suit ni le profil ni l'appareil (leçon
//! du tri d'albums, #1134).
//!
//! Le nom et la requête sont deux champs distincts, comme dans les presets du
//! client : la tour texte CLAP est entraînée en anglais, une requête anglaise
//! donne de bien meilleurs résultats qu'une française. Séparer les deux laisse
//! l'utilisateur nommer « Jazz feutré » ce qu'il interroge en anglais — et nous
//! laisse la porte ouverte à une traduction automatique plus tard.

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

/// Garde-fous de saisie. Généreux pour l'usage réel, assez bas pour qu'un
/// client fautif ne remplisse pas la table `settings`.
const MAX_NAME: usize = 80;
const MAX_QUERY: usize = 300;
const MAX_AMBIANCES: usize = 200;

fn key(profile_id: i64) -> String {
    format!("ambiances:{profile_id}")
}

fn load(state: &AppState, profile_id: i64) -> Vec<Value> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(&key(profile_id))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(state: &AppState, profile_id: i64, items: &[Value]) -> Result<(), AppError> {
    SettingsRepo::with_backend(state.backend.clone())
        .set(&key(profile_id), &serde_json::to_string(items)?)
        .map_err(|e| AppError::internal(format!("ambiances non enregistrées : {e}")))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Nettoie et valide un champ texte. Renvoie l'erreur que l'interface peut
/// afficher telle quelle.
fn clean(value: &str, max: usize, field: &str) -> Result<String, AppError> {
    let v = value.trim();
    if v.is_empty() {
        return Err(AppError::bad_request(format!(
            "{field} ne peut pas être vide"
        )));
    }
    if v.chars().count() > max {
        return Err(AppError::bad_request(format!(
            "{field} est limité à {max} caractères"
        )));
    }
    Ok(v.to_string())
}

/// Deux ambiances du même nom ne se distinguent pas dans une liste : on refuse,
/// plutôt que de laisser l'utilisateur en supprimer une au hasard plus tard.
fn name_taken(items: &[Value], name: &str, except_id: Option<&str>) -> bool {
    items.iter().any(|a| {
        a["id"].as_str() != except_id
            && a["name"]
                .as_str()
                .is_some_and(|n| n.trim().eq_ignore_ascii_case(name))
    })
}

/// GET /library/ambiances
pub(super) async fn list_ambiances(
    State(state): State<AppState>,
    profile: ActiveProfile,
) -> Json<Value> {
    Json(json!({ "ambiances": load(&state, profile.0) }))
}

#[derive(Deserialize)]
pub(super) struct CreateBody {
    name: String,
    query: String,
}

/// POST /library/ambiances
pub(super) async fn create_ambiance(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<CreateBody>,
) -> Result<Json<Value>, AppError> {
    let name = clean(&body.name, MAX_NAME, "Le nom")?;
    let query = clean(&body.query, MAX_QUERY, "La recherche")?;

    let mut items = load(&state, profile.0);
    if items.len() >= MAX_AMBIANCES {
        return Err(AppError::bad_request(format!(
            "limite de {MAX_AMBIANCES} ambiances atteinte"
        )));
    }
    if name_taken(&items, &name, None) {
        return Err(AppError::bad_request(format!(
            "une ambiance nommée « {name} » existe déjà"
        )));
    }

    let ambiance = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "name": name,
        "query": query,
        "created_at": now_secs(),
    });
    items.push(ambiance.clone());
    save(&state, profile.0, &items)?;
    Ok(Json(ambiance))
}

#[derive(Deserialize)]
pub(super) struct UpdateBody {
    name: Option<String>,
    query: Option<String>,
}

/// PATCH /library/ambiances/{id} — renommer et/ou réécrire la recherche.
pub(super) async fn update_ambiance(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<Value>, AppError> {
    let mut items = load(&state, profile.0);
    let idx = items
        .iter()
        .position(|a| a["id"].as_str() == Some(id.as_str()))
        .ok_or_else(|| AppError::not_found("ambiance introuvable"))?;

    if let Some(raw) = body.name.as_deref() {
        let name = clean(raw, MAX_NAME, "Le nom")?;
        if name_taken(&items, &name, Some(id.as_str())) {
            return Err(AppError::bad_request(format!(
                "une ambiance nommée « {name} » existe déjà"
            )));
        }
        items[idx]["name"] = json!(name);
    }
    if let Some(raw) = body.query.as_deref() {
        items[idx]["query"] = json!(clean(raw, MAX_QUERY, "La recherche")?);
    }

    let updated = items[idx].clone();
    save(&state, profile.0, &items)?;
    Ok(Json(updated))
}

/// DELETE /library/ambiances/{id}
pub(super) async fn delete_ambiance(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let mut items = load(&state, profile.0);
    let before = items.len();
    items.retain(|a| a["id"].as_str() != Some(id.as_str()));
    if items.len() == before {
        return Err(AppError::not_found("ambiance introuvable"));
    }
    save(&state, profile.0, &items)?;
    Ok(Json(json!({ "deleted": id })))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `AppError` n'implémente pas Debug : on compare via `ok()` plutôt que
    // `unwrap()`, qui exigerait ce Debug sur le type d'erreur.
    #[test]
    fn clean_refuse_le_vide_et_les_blancs() {
        assert!(clean("   ", MAX_NAME, "Le nom").is_err());
        assert_eq!(
            clean("  Jazz feutré ", MAX_NAME, "Le nom").ok().as_deref(),
            Some("Jazz feutré")
        );
    }

    #[test]
    fn clean_compte_en_caracteres_pas_en_octets() {
        // 80 accents = 160 octets : la limite porte sur ce que l'utilisateur
        // voit, pas sur l'encodage.
        let accents = "é".repeat(MAX_NAME);
        assert!(clean(&accents, MAX_NAME, "Le nom").is_ok());
        assert!(clean(&"é".repeat(MAX_NAME + 1), MAX_NAME, "Le nom").is_err());
    }

    #[test]
    fn nom_deja_pris_ignore_la_casse_et_les_blancs() {
        let items = vec![json!({ "id": "a", "name": " Jazz Feutré " })];
        assert!(name_taken(&items, "jazz feutré", None));
        // …sauf pour l'ambiance qu'on est en train de renommer elle-même.
        assert!(!name_taken(&items, "jazz feutré", Some("a")));
    }

    #[test]
    fn la_cle_est_rangee_par_profil() {
        assert_eq!(key(1), "ambiances:1");
        assert_ne!(key(1), key(2));
    }
}
