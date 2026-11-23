//! TESS T17 — préréglages de chaîne et mode d'application (maquette A2).
//!
//! Routes :
//! - `GET /tess/presets` → la liste globale et le mode d'application
//!   (définition `presets` du contrat) ;
//! - `POST /tess/presets` ← `{ "name", "zone_id" }` (la chaîne courante de la
//!   zone) ou `{ "name", "chain" }` → 201, le préréglage créé ;
//! - `PATCH /tess/presets/{preset_id}` ← `{ "name" }` → le préréglage renommé ;
//! - `DELETE /tess/presets/{preset_id}` → 204 ;
//! - `PUT /tess/apply-mode` ← `{ "apply_mode": "immediately" | "at_next_playback" }` ;
//! - `POST /zones/{id}/tess/presets/{preset_id}/apply` → la zone, le mode
//!   suivi et ce qui ne s'appliquera pas ici, avec son motif.
//!
//! **Charger un préréglage passe par la voie du `PUT /zones/{id}/tess`**
//! ([`super::tess::appliquer_le_put`]) : corps lu strictement, validation,
//! droits (402, jamais un droit Premium offert : un préréglage enregistré par
//! un compte Premium reste payant sur un compte gratuit), origine des
//! corrections et marques recalculées par le serveur, écriture croisée.
//! Le préréglage étant une chaîne complète, il REMPLACE celle de la zone.
//! Un étage permis mais sans effet sur cette sortie (stéréo seulement sur un
//! flux 5.1, zone hors TESS, greffon désinstallé…) est accepté comme par le
//! `PUT` ; la réponse le dit dans `not_applied`, avec le motif de la
//! résolution.
//!
//! **Mode d'application** (`apply_mode`, global, suivi par le `PUT` comme par
//! le chargement d'un préréglage) :
//! - `immediately` : la sortie vivante est rafraîchie tout de suite (à chaud
//!   en local ; relance du flux réseau à la position, #8422, une seule par
//!   rafale grâce à l'anti-rebond et au plancher des relances) ;
//! - `at_next_playback` (et toute valeur inconnue) : rien n'interrompt la
//!   lecture. Les anciennes clés sont écrites, donc le prochain flux réseau
//!   part avec ; sur une sortie locale, ce qui reste à rafraîchir l'est au
//!   prochain démarrage de lecture de la zone ([`spawn_application_au_demarrage`]).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tune_core::tess::chaine::{ChaineTess, ErreurChaine};
use tune_core::tess::migration::{
    CHAMP_ORIGINE, ErreurMigration, MARQUE_MONO_INERTE, MARQUE_REMPLACE, lire_zone,
};
use axum::body::Bytes;
use axum::response::Response;
use tune_core::tess::prereglages::{self, ModeApplication, PreReglage};
use tune_core::tess::resolution::MotifEtape;

use super::tess::{
    appliquer_le_put, catalogue_du_serveur, cle_prereglage_actif, erreur_migration, rafraichir,
    refus, reponse_de_zone, verrou_des_put, zone_de,
};
use super::*;

// ── Erreurs propres aux préréglages ─────────────────────────────────

/// Un refus qui porte sur le préréglage lui-même (pas sur la chaîne, dont les
/// refus gardent la forme `refusal` du contrat).
fn erreur(statut: StatusCode, code: &str, message: &str, preset_id: Option<&str>) -> Response {
    (
        statut,
        Json(json!({ "error": code, "message": message, "preset_id": preset_id })),
    )
        .into_response()
}

fn introuvable(preset_id: &str) -> Response {
    erreur(
        StatusCode::NOT_FOUND,
        "preset_not_found",
        "This preset does not exist (it may have been deleted).",
        Some(preset_id),
    )
}

fn stockage(e: String) -> Response {
    erreur(
        StatusCode::INTERNAL_SERVER_ERROR,
        "tess_presets_unreadable",
        &e,
        None,
    )
}

fn nom_invalide() -> Response {
    erreur(
        StatusCode::BAD_REQUEST,
        "invalid_name",
        &format!(
            "A preset needs a name of 1 to {} characters, on one line.",
            prereglages::NOM_MAX
        ),
        None,
    )
}

/// Une modification de la liste à la fois (lecture, changement, écriture).
fn verrou_de_la_liste() -> &'static tokio::sync::Mutex<()> {
    static VERROU: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    VERROU.get_or_init(Default::default)
}

fn settings_de(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

/// La chaîne telle qu'un préréglage la garde : sans les champs que seul le
/// serveur tient (origine D1 d'une correction, marques de lecture), qu'il
/// recalcule au chargement.
pub fn sans_champs_du_serveur(chaine: &ChaineTess) -> ChaineTess {
    let mut c = chaine.clone();
    for m in &mut c.plugins {
        if let Value::Object(r) = &mut m.settings {
            r.remove(CHAMP_ORIGINE);
            r.remove(MARQUE_MONO_INERTE);
            r.remove(MARQUE_REMPLACE);
        }
    }
    c.mettre_a_niveau()
}

// ── Liste ───────────────────────────────────────────────────────────

/// `GET /tess/presets`
pub async fn lister_prereglages(State(state): State<AppState>) -> Response {
    match prereglages::lire(&settings_de(&state)) {
        Ok(l) => Json(l).into_response(),
        Err(e) => stockage(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpsEnregistrer {
    pub name: String,
    #[serde(default)]
    pub zone_id: Option<i64>,
    #[serde(default)]
    pub chain: Option<Value>,
}

/// `POST /tess/presets` — enregistre la chaîne courante d'une zone
/// (`zone_id`), ou une chaîne fournie (`chain`, lue strictement et
/// vérifiée comme le corps d'un `PUT`, droits exceptés : ils se vérifient
/// au chargement). Enregistrée depuis une zone, celle-ci marque le
/// préréglage « ACTIVE ».
pub async fn enregistrer_prereglage(State(state): State<AppState>, corps: Bytes) -> Response {
    let corps: CorpsEnregistrer = match serde_json::from_slice(&corps) {
        Ok(c) => c,
        Err(e) => {
            return refus(&ErreurChaine::CorpsIllisible(e.to_string()));
        }
    };
    let Some(nom) = prereglages::nom_valide(&corps.name) else {
        return nom_invalide();
    };
    let settings = settings_de(&state);
    let chaine = match (corps.zone_id, corps.chain) {
        (Some(zone_id), None) => {
            if let Err(r) = zone_de(&state, zone_id) {
                return r;
            }
            // La chaîne ENREGISTRÉE, neutre vis-à-vis du droit : un compte
            // gratuit garde dans son préréglage le paramétrique qu'il voit
            // converti, et ne le retrouve qu'avec le Premium.
            match lire_zone(&settings, zone_id, true) {
                Ok(l) => l.chaine,
                Err(e) => return erreur_migration(&e),
            }
        }
        (None, Some(chain)) => {
            let c = match ChaineTess::depuis_corps(&chain.to_string()) {
                Ok(c) => c,
                Err(e) => return refus(&e),
            };
            // La forme (409) avec tous les droits : le droit, lui, se vérifie
            // au chargement sur la zone.
            if let Err(e) = c.valider(
                &catalogue_du_serveur(),
                tune_core::tess::ContexteValidation { premium: true },
            ) {
                return refus(&e);
            }
            c
        }
        _ => {
            return erreur(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                "Give either zone_id (save the zone's current chain) or chain.",
                None,
            );
        }
    };
    if chaine.version > tune_core::tess::VERSION_CHAINE {
        return refus(&ErreurChaine::VersionNonPriseEnCharge {
            trouvee: chaine.version,
            attendue: tune_core::tess::VERSION_CHAINE,
        });
    }
    let _garde = verrou_de_la_liste().lock().await;
    let mut liste = match prereglages::lire(&settings) {
        Ok(l) => l,
        Err(e) => return stockage(e),
    };
    let p = PreReglage {
        id: prereglages::id_pour_nom(&nom, &liste),
        name: nom,
        builtin: false,
        chain: sans_champs_du_serveur(&chaine),
    };
    liste.presets.push(p.clone());
    if let Err(e) = prereglages::ecrire(&settings, &liste) {
        return stockage(e);
    }
    if let Some(zone_id) = corps.zone_id {
        let _ = settings.set(&cle_prereglage_actif(zone_id), &p.id);
    }
    info!(preset_id = %p.id, zone_id = ?corps.zone_id, "tess_prereglage_enregistre");
    (StatusCode::CREATED, Json(p)).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpsRenommer {
    pub name: String,
}

/// `PATCH /tess/presets/{preset_id}` — renomme. L'identifiant ne change pas
/// (les zones qui l'ont chargé le gardent « ACTIVE »).
pub async fn renommer_prereglage(
    State(state): State<AppState>,
    Path(preset_id): Path<String>,
    corps: Bytes,
) -> Response {
    let corps: CorpsRenommer = match serde_json::from_slice(&corps) {
        Ok(c) => c,
        Err(e) => return refus(&ErreurChaine::CorpsIllisible(e.to_string())),
    };
    let Some(nom) = prereglages::nom_valide(&corps.name) else {
        return nom_invalide();
    };
    let settings = settings_de(&state);
    let _garde = verrou_de_la_liste().lock().await;
    let mut liste = match prereglages::lire(&settings) {
        Ok(l) => l,
        Err(e) => return stockage(e),
    };
    let Some(p) = liste.presets.iter_mut().find(|p| p.id == preset_id) else {
        return introuvable(&preset_id);
    };
    p.name = nom;
    let p = p.clone();
    if let Err(e) = prereglages::ecrire(&settings, &liste) {
        return stockage(e);
    }
    Json(p).into_response()
}

/// `DELETE /tess/presets/{preset_id}` — supprime (Pure compris : la croix de
/// la maquette A2). Les zones qui l'avaient chargé gardent leur chaîne et ne
/// le marquent plus « ACTIVE ».
pub async fn supprimer_prereglage(
    State(state): State<AppState>,
    Path(preset_id): Path<String>,
) -> Response {
    let settings = settings_de(&state);
    let _garde = verrou_de_la_liste().lock().await;
    let mut liste = match prereglages::lire(&settings) {
        Ok(l) => l,
        Err(e) => return stockage(e),
    };
    let avant = liste.presets.len();
    liste.presets.retain(|p| p.id != preset_id);
    if liste.presets.len() == avant {
        return introuvable(&preset_id);
    }
    if let Err(e) = prereglages::ecrire(&settings, &liste) {
        return stockage(e);
    }
    for zone in ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap_or_default()
    {
        let Some(id) = zone.id else { continue };
        let cle = cle_prereglage_actif(id);
        if settings.get(&cle).ok().flatten().as_deref() == Some(preset_id.as_str()) {
            let _ = settings.delete(&cle);
        }
    }
    info!(preset_id = %preset_id, "tess_prereglage_supprime");
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpsMode {
    pub apply_mode: String,
}

/// `PUT /tess/apply-mode` — « APPLY CHANGES » : `immediately` ou
/// `at_next_playback`. Toute autre valeur : 400 qui nomme les deux.
pub async fn regler_le_mode(State(state): State<AppState>, corps: Bytes) -> Response {
    let mode = serde_json::from_slice::<CorpsMode>(&corps)
        .ok()
        .and_then(|c| match c.apply_mode.as_str() {
            "immediately" => Some(ModeApplication::Immediately),
            "at_next_playback" => Some(ModeApplication::AtNextPlayback),
            _ => None,
        });
    let Some(mode) = mode else {
        return erreur(
            StatusCode::BAD_REQUEST,
            "invalid_apply_mode",
            "apply_mode must be \"immediately\" or \"at_next_playback\".",
            None,
        );
    };
    let settings = settings_de(&state);
    let _garde = verrou_de_la_liste().lock().await;
    let mut liste = match prereglages::lire(&settings) {
        Ok(l) => l,
        Err(e) => return stockage(e),
    };
    liste.apply_mode = mode;
    if let Err(e) = prereglages::ecrire(&settings, &liste) {
        return stockage(e);
    }
    Json(liste).into_response()
}

// ── Chargement sur une zone ─────────────────────────────────────────

/// Un étage du préréglage qui ne s'appliquera pas sur cette zone, et pourquoi.
fn non_appliques(reponse: &tune_core::tess::ReponseTessZone, p: &PreReglage) -> Vec<Value> {
    let coches: Vec<&str> = p
        .chain
        .plugins
        .iter()
        .filter(|m| m.enabled)
        .map(|m| m.id.as_str())
        .collect();
    reponse
        .signal_path
        .iter()
        .filter(|e| {
            e.plugin_id
                .as_deref()
                .is_some_and(|id| coches.contains(&id))
                && !e.effective
        })
        .map(|e| {
            json!({
                "plugin_id": e.plugin_id,
                "reason": e.reason.clone().unwrap_or(MotifEtape::Unknown),
                "detail": e.detail,
            })
        })
        .collect()
}

/// `POST /zones/{id}/tess/presets/{preset_id}/apply` — charge le préréglage
/// sur la zone par la voie du `PUT` (validation, droits, marques), puis le
/// marque « ACTIVE » pour cette zone.
pub async fn appliquer_prereglage(
    State(state): State<AppState>,
    Path((id, preset_id)): Path<(i64, String)>,
) -> Response {
    let zone = match zone_de(&state, id) {
        Ok(z) => z,
        Err(r) => return r,
    };
    let settings = settings_de(&state);
    let liste = match prereglages::lire(&settings) {
        Ok(l) => l,
        Err(e) => return stockage(e),
    };
    let Some(p) = liste.get(&preset_id).cloned() else {
        return introuvable(&preset_id);
    };
    let corps = match serde_json::to_vec(&p.chain) {
        Ok(c) => c,
        Err(e) => return refus(&ErreurChaine::CorpsIllisible(e.to_string())),
    };
    let premium = state.license.is_premium().await;
    let catalogue = catalogue_du_serveur();
    let verrou = verrou_des_put(id);
    let _garde = verrou.lock().await;
    let cat = catalogue.clone();
    let s = settings_de(&state);
    let resultat =
        tokio::task::spawn_blocking(move || appliquer_le_put(&s, id, &corps, premium, &cat)).await;
    let (lecture, cles) = match resultat {
        Ok(Ok(x)) => x,
        Ok(Err(r)) => return r,
        Err(e) => return erreur_migration(&ErreurMigration::Stockage(e.to_string())),
    };
    let _ = settings.set(&cle_prereglage_actif(id), &p.id);
    let mode = appliquer_selon_le_mode(&state, &zone, cles.clone()).await;
    info!(zone_id = id, preset_id = %p.id, cles = ?cles, mode = ?mode, "tess_prereglage_charge");
    let reponse = reponse_de_zone(&state, &zone, lecture, premium, catalogue).await;
    let non_appliques = non_appliques(&reponse, &p);
    Json(json!({
        "preset_id": p.id,
        "apply_mode": if mode.immediat() { "immediately" } else { "at_next_playback" },
        "not_applied": non_appliques,
        "zone": reponse,
    }))
    .into_response()
}

/// Après un `PUT` à la main : la zone ne marque plus « ACTIVE » un préréglage
/// qui ne décrit plus sa chaîne (comparées sans les champs du serveur).
pub(super) fn oublier_le_prereglage_s_il_differe(state: &AppState, zone_id: i64, chaine: &ChaineTess) {
    let settings = settings_de(state);
    let cle = cle_prereglage_actif(zone_id);
    let Some(actif) = settings.get(&cle).ok().flatten().filter(|a| !a.is_empty()) else {
        return;
    };
    let decrit = prereglages::lire(&settings)
        .ok()
        .and_then(|l| l.get(&actif).cloned())
        .is_some_and(|p| sans_champs_du_serveur(&p.chain) == sans_champs_du_serveur(chaine));
    if !decrit {
        let _ = settings.delete(&cle);
    }
}

// ── Mode d'application ──────────────────────────────────────────────

/// Les anciennes clés écrites par TESS qui attendent la prochaine lecture
/// d'une zone (mode `at_next_playback`).
fn en_attente() -> &'static Mutex<HashMap<i64, Vec<String>>> {
    static EN_ATTENTE: OnceLock<Mutex<HashMap<i64, Vec<String>>>> = OnceLock::new();
    EN_ATTENTE.get_or_init(Default::default)
}

/// Ce qui attend la prochaine lecture de la zone (banc et diagnostic).
pub fn cles_en_attente(zone_id: i64) -> Vec<String> {
    en_attente()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&zone_id)
        .cloned()
        .unwrap_or_default()
}

fn ajouter_en_attente(zone_id: i64, cles: Vec<String>) {
    if cles.is_empty() {
        return;
    }
    let mut table = en_attente().lock().unwrap_or_else(|e| e.into_inner());
    let liste = table.entry(zone_id).or_default();
    for c in cles {
        if !liste.contains(&c) {
            liste.push(c);
        }
    }
}

fn prendre_en_attente(zone_id: i64) -> Vec<String> {
    en_attente()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&zone_id)
        .unwrap_or_default()
}

/// Le mode d'application en vigueur. Liste illisible : `immediately`, le
/// comportement d'avant T17.
pub fn mode_en_vigueur(state: &AppState) -> ModeApplication {
    prereglages::lire(&settings_de(state))
        .map(|l| l.apply_mode)
        .unwrap_or_default()
}

/// Atteint la sortie vivante selon le mode : tout de suite (avec ce qui
/// attendait encore), ou à la prochaine lecture de la zone.
pub(super) async fn appliquer_selon_le_mode(
    state: &AppState,
    zone: &Zone,
    cles: Vec<String>,
) -> ModeApplication {
    let id = zone.id.unwrap_or_default();
    let mode = mode_en_vigueur(state);
    if mode.immediat() {
        let mut toutes = prendre_en_attente(id);
        for c in cles {
            if !toutes.contains(&c) {
                toutes.push(c);
            }
        }
        rafraichir(state, zone, &toutes).await;
    } else {
        ajouter_en_attente(id, cles);
    }
    mode
}

/// Au démarrage d'une lecture de la zone : ce qui attendait atteint la sortie
/// locale (la convolution, posée une fois, ne se relit pas seule). Un flux
/// réseau, lui, vient d'être fabriqué avec les clés déjà écrites : rien à
/// relancer (jamais de relance juste après un démarrage).
pub async fn appliquer_a_la_lecture(state: &AppState, zone_id: i64) {
    let cles = prendre_en_attente(zone_id);
    if cles.is_empty() {
        return;
    }
    let Ok(zone) = zone_de(state, zone_id) else {
        return;
    };
    let locale = zone
        .output_device_id
        .as_deref()
        .is_none_or(|d| d.starts_with("local:"));
    if locale {
        rafraichir(state, &zone, &cles).await;
    }
    info!(zone_id, cles = ?cles, locale, "tess_applique_au_demarrage");
}

/// Écoute les démarrages de lecture (`started`) pour le mode
/// `at_next_playback`. Appelée une fois au lancement (`background.rs`).
pub fn spawn_application_au_demarrage(state: &AppState) {
    let mut rx = state.playback.subscribe();
    let state = state.clone();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            let ev = match rx.recv().await {
                Ok(e) => e,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            };
            if ev.event == "started" && !cles_en_attente(ev.zone_id).is_empty() {
                appliquer_a_la_lecture(&state, ev.zone_id).await;
            }
        }
    });
}
