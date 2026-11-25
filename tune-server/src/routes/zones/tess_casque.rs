//! rc14 — le greffon TESS « Casque » (`equalizer-headphone`) tiré d'OPRA.
//!
//! - `GET /tess/headphone/search?q=hd650&limit=50` : les casques de la base
//!   OPRA (marque, modèle, variantes), cherchés par le SERVEUR
//!   ([`tune_core::audio::opra_base`]).
//! - `GET /tess/headphone/profile?id=…&level=100` : une correction, avant de
//!   l'appliquer — bandes, préampli automatique, courbe, attribution, et les
//!   réglages complets du maillon (`settings`), que l'écran TESS envoie avec
//!   la chaîne par `PUT /zones/{id}/tess` comme pour tout greffon.
//! - `GET /zones/{id}/tess/headphone` : le casque de la zone — la correction
//!   reconnue (ou des bandes libres), le niveau, le préampli, la courbe.
//! - `PUT /zones/{id}/tess/headphone` : `{ "opra_id"?, "level"?, "enabled"? }`.
//!   Le serveur lit la correction dans OPRA, calcule les bandes et les
//!   réglages ([`tune_core::audio::casque_opra::reglages_du_casque`]), puis
//!   écrit la chaîne par la MÊME voie que `PUT /zones/{id}/tess`
//!   ([`super::tess::appliquer_le_put`]) : mêmes validations, mêmes droits
//!   (le casque est Free), même règle de groupe des égaliseurs que la chaîne
//!   en vigueur, mêmes rafraîchissements de la sortie vivante.
//!
//! Le client ne fournit jamais les bandes : il désigne une correction OPRA.
//! Le maillon garde la référence (identifiant OPRA, auteur, licence, niveau)
//! à côté des bandes que le moteur joue.

use axum::extract::Query;
use axum::response::Response;
use tune_core::audio::casque_opra::{
    self, CHAMP_OPRA, NIVEAU_PLEIN, frequences_d_affichage, profil_reconnu,
};
use tune_core::audio::opra_base::{
    CorrectionOpra, DEPOT_OPRA, ErreurBaseOpra, LICENCE_OPRA, ProduitOpra,
};
use tune_core::tess::chaine::{ChaineTess, MaillonGreffon};
use tune_core::tess::migration::{ErreurMigration, FACADES_EQ, ID_EQ_CASQUE, lire_zone};

use super::tess::{
    appliquer_le_put, catalogue_du_serveur, erreur_migration, reponse_de_zone, verrou_des_put,
    zone_de,
};
use super::*;

/// Points de la courbe rendue à l'écran.
const POINTS_DE_COURBE: usize = 96;
const LIMITE_RECHERCHE: usize = 30;
const LIMITE_MAX_RECHERCHE: usize = 100;

/// La source citée avec chaque réponse (attribution CC BY-SA 4.0).
fn source() -> Value {
    json!({ "name": "OPRA", "url": DEPOT_OPRA, "license": LICENCE_OPRA })
}

fn refus_opra(e: &ErreurBaseOpra) -> Response {
    let statut = match e {
        ErreurBaseOpra::HorsLigne(_) => StatusCode::SERVICE_UNAVAILABLE,
        ErreurBaseOpra::ProfilInconnu(_) => StatusCode::NOT_FOUND,
        ErreurBaseOpra::BaseIllisible => StatusCode::BAD_GATEWAY,
        ErreurBaseOpra::ProfilNonPrisEnCharge { .. } => StatusCode::UNPROCESSABLE_ENTITY,
    };
    let mut corps = json!({ "error": e.cle(), "code": e.cle(), "message": e.to_string() });
    if let ErreurBaseOpra::ProfilNonPrisEnCharge { cle, .. } = e {
        corps["reason"] = json!(cle);
    }
    (statut, Json(corps)).into_response()
}

fn refus_corps(code: &str, message: impl Into<String>) -> Response {
    let message = message.into();
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": code, "code": code, "message": message })),
    )
        .into_response()
}

/// La courbe et le préampli de réglages du maillon.
fn courbe_et_preampli(reglages: &Value) -> Value {
    let hz = frequences_d_affichage(POINTS_DE_COURBE);
    let db = casque_opra::courbe_db(reglages, &hz);
    let arrondi = |x: f64| (x * 100.0).round() / 100.0;
    json!({
        "preamp_db": arrondi(casque_opra::preampli_auto_db(reglages)),
        "curve": {
            "frequency_hz": hz.iter().map(|f| arrondi(*f)).collect::<Vec<_>>(),
            "gain_db": db.iter().map(|g| arrondi(*g)).collect::<Vec<_>>(),
        },
    })
}

// ── Recherche ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RechercheCasque {
    #[serde(default)]
    q: String,
    limit: Option<usize>,
}

/// `GET /tess/headphone/search`
pub async fn chercher_un_casque(
    State(state): State<AppState>,
    Query(r): Query<RechercheCasque>,
) -> Response {
    let limite = r
        .limit
        .unwrap_or(LIMITE_RECHERCHE)
        .clamp(1, LIMITE_MAX_RECHERCHE);
    if r.q.trim().is_empty() {
        return Json(json!({ "query": r.q, "results": [], "total": 0, "source": source() }))
            .into_response();
    }
    match state.opra.rechercher(&r.q, limite).await {
        Ok((resultats, total)) => Json(json!({
            "query": r.q,
            "results": resultats,
            "total": total,
            "source": source(),
        }))
        .into_response(),
        Err(e) => refus_opra(&e),
    }
}

// ── Aperçu d'une correction ─────────────────────────────────────────

#[derive(Deserialize)]
pub struct ApercuCasque {
    id: String,
    level: Option<u8>,
}

fn produit_json(p: &ProduitOpra) -> Value {
    json!({ "id": p.id, "vendor": p.vendor, "name": p.name, "form": p.form })
}

fn correction_json(c: &CorrectionOpra, reglages: &Value) -> Value {
    let mut v = json!({
        "id": c.variante.id,
        "product": produit_json(&c.produit),
        "variant": c.variante,
        "bands": reglages["bands"],
        "level": reglages[CHAMP_OPRA]["level"],
        // Les réglages complets du maillon, référence comprise : un client
        // peut les poser tels quels par `PUT /zones/{id}/tess`.
        "settings": reglages,
        "source": source(),
    });
    if let (Some(o), Value::Object(extra)) = (v.as_object_mut(), courbe_et_preampli(reglages)) {
        o.extend(extra);
    }
    v
}

/// `GET /tess/headphone/profile`
pub async fn apercu_d_un_casque(
    State(state): State<AppState>,
    Query(a): Query<ApercuCasque>,
) -> Response {
    let niveau = a.level.unwrap_or(NIVEAU_PLEIN);
    if niveau > NIVEAU_PLEIN {
        return refus_corps("invalid_level", "level must be between 0 and 100");
    }
    match state.opra.correction(a.id.trim()).await {
        Ok(c) => {
            let reglages = casque_opra::reglages_du_casque(&c, niveau, None);
            Json(correction_json(&c, &reglages)).into_response()
        }
        Err(e) => refus_opra(&e),
    }
}

// ── Le casque de la zone ────────────────────────────────────────────

/// Ce que l'écran montre du maillon casque de la zone.
pub fn etat_du_casque(chaine: &ChaineTess) -> Value {
    let Some(m) = chaine.plugins.iter().find(|m| m.id == ID_EQ_CASQUE) else {
        return json!({ "present": false, "enabled": false, "profile": null, "free_bands": false, "source": source() });
    };
    let reconnu = profil_reconnu(&m.settings);
    let a_des_bandes = m
        .settings
        .get("bands")
        .and_then(Value::as_array)
        .is_some_and(|b| !b.is_empty());
    let mut v = json!({
        "present": true,
        "enabled": m.enabled,
        "profile": reconnu,
        // Des bandes sans profil reconnu : importées à la main, ou retouchées.
        "free_bands": reconnu.is_none() && a_des_bandes,
        "source": source(),
    });
    if let (Some(o), Value::Object(extra)) = (v.as_object_mut(), courbe_et_preampli(&m.settings)) {
        o.extend(extra);
    }
    v
}

/// `GET /zones/{id}/tess/headphone`
pub async fn lire_le_casque(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    if let Err(r) = zone_de(&state, id) {
        return r;
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    match lire_zone(&settings, id, true) {
        Ok(l) => Json(etat_du_casque(&l.chaine)).into_response(),
        Err(e) => erreur_migration(&e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpsCasque {
    /// La correction OPRA. Absente : celle que le maillon porte déjà
    /// (changer le niveau ou l'interrupteur sans la rechoisir).
    #[serde(default)]
    pub opra_id: Option<String>,
    /// Niveau d'effet, 0 à 100 % ; absent : celui du maillon, sinon 100.
    #[serde(default)]
    pub level: Option<u8>,
    /// L'interrupteur ; absent : allumé.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// La chaîne avec le maillon casque posé (remplacé à sa place, sinon
/// inséré avant le premier égaliseur, sinon en tête).
pub fn chaine_avec_le_casque(enregistree: &ChaineTess, maillon: MaillonGreffon) -> ChaineTess {
    let mut chaine = enregistree.clone();
    if let Some(i) = chaine.plugins.iter().position(|m| m.id == ID_EQ_CASQUE) {
        chaine.plugins[i] = maillon;
    } else {
        let i = chaine
            .plugins
            .iter()
            .position(|m| FACADES_EQ.contains(&m.id.as_str()))
            .unwrap_or(0);
        chaine.plugins.insert(i, maillon);
    }
    chaine
}

/// `PUT /zones/{id}/tess/headphone`
pub async fn regler_le_casque(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    corps: axum::body::Bytes,
) -> Response {
    let zone = match zone_de(&state, id) {
        Ok(z) => z,
        Err(r) => return r,
    };
    let corps: CorpsCasque = match serde_json::from_slice(&corps) {
        Ok(c) => c,
        Err(e) => return refus_corps("invalid_body", e.to_string()),
    };
    if corps.level.is_some_and(|n| n > NIVEAU_PLEIN) {
        return refus_corps("invalid_level", "level must be between 0 and 100");
    }
    let premium = state.license.is_premium().await;
    let catalogue = catalogue_du_serveur();
    let verrou = verrou_des_put(id);
    let _garde = verrou.lock().await;
    let settings = SettingsRepo::with_backend(state.backend.clone());

    let enregistree = lire_zone(&settings, id, true)
        .map(|l| l.chaine)
        .unwrap_or_default();
    let actuel = enregistree.plugins.iter().find(|m| m.id == ID_EQ_CASQUE);
    let reference = actuel.and_then(|m| {
        m.settings
            .get(CHAMP_OPRA)
            .and_then(|r| serde_json::from_value::<casque_opra::ReferenceOpra>(r.clone()).ok())
    });
    let Some(opra_id) = corps
        .opra_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| reference.as_ref().map(|r| r.id.clone()))
    else {
        return refus_corps(
            "opra_profile_required",
            "choose a headphone correction (opra_id) first",
        );
    };
    let niveau = corps
        .level
        .or_else(|| reference.as_ref().map(|r| r.level))
        .unwrap_or(NIVEAU_PLEIN);
    let correction = match state.opra.correction(&opra_id).await {
        Ok(c) => c,
        Err(e) => return refus_opra(&e),
    };
    let maillon = MaillonGreffon {
        id: ID_EQ_CASQUE.into(),
        enabled: corps.enabled.unwrap_or(true),
        settings: casque_opra::reglages_du_casque(&correction, niveau, actuel.map(|m| &m.settings)),
    };
    let chaine = chaine_avec_le_casque(&enregistree, maillon);
    let corps_put = match serde_json::to_vec(
        &json!({ "version": chaine.version, "plugins": chaine.plugins }),
    ) {
        Ok(c) => c,
        Err(e) => return erreur_migration(&ErreurMigration::Stockage(e.to_string())),
    };

    let cat = catalogue.clone();
    let resultat = tokio::task::spawn_blocking(move || {
        appliquer_le_put(&settings, id, &corps_put, premium, &cat)
    })
    .await;
    let (lecture, cles) = match resultat {
        Ok(Ok(x)) => x,
        Ok(Err(r)) => {
            info!(zone_id = id, status = %r.status(), "tess_casque_refuse");
            return r;
        }
        Err(e) => return erreur_migration(&ErreurMigration::Stockage(e.to_string())),
    };
    let avertissement =
        super::tess_prereglages::oublier_le_prereglage_s_il_differe(&state, id, &lecture.chaine);
    let mode = super::tess_prereglages::appliquer_selon_le_mode(&state, &zone, cles.clone()).await;
    info!(zone_id = id, opra_id = %opra_id, niveau, cles = ?cles, mode = ?mode, "tess_casque_ecrit");
    let casque = etat_du_casque(&lecture.chaine);
    let mut reponse = reponse_de_zone(&state, &zone, lecture, premium, catalogue).await;
    reponse.warnings.extend(avertissement);
    let mut v = serde_json::to_value(&reponse).unwrap_or_else(|_| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("headphone".into(), casque);
    }
    Json(v).into_response()
}

#[cfg(test)]
#[path = "tess_casque_tests.rs"]
mod tests;
