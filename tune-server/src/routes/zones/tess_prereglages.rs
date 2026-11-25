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
//! - `immediately` : la sortie vivante est rafraîchie tout de suite, par les
//!   chemins des anciennes routes : à chaud en local ; sur une zone réseau
//!   qui joue, SEUL l'égaliseur relance le flux à la position (#8422, une
//!   relance par rafale grâce à l'anti-rebond et au plancher) ; le crossfeed
//!   réseau prend effet à la piste suivante ; mono, réaffectation,
//!   suréchantillonnage et convolution n'agissent qu'en local (le prochain
//!   flux réseau les porte) ;
//! - `at_next_playback` (et toute valeur inconnue) : rien n'interrompt la
//!   lecture. Les anciennes clés sont écrites, donc le prochain flux réseau
//!   part avec ; sur une sortie locale, ce qui reste à rafraîchir l'est au
//!   prochain démarrage de lecture de la zone ([`spawn_application_au_demarrage`]).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::body::Bytes;
use axum::response::Response;
use tune_core::tess::chaine::{ChaineTess, ErreurChaine};
use tune_core::tess::migration::{
    CHAMP_ORIGINE, ErreurMigration, MARQUE_MONO_INERTE, MARQUE_REMPLACE, lire_zone,
};
use tune_core::tess::prereglages::{self, ModeApplication, PreReglage};
use tune_core::tess::resolution::MotifEtape;

use super::tess::{
    appliquer_le_put, catalogue_du_serveur, cle_mode_pure, cle_prereglage_actif, erreur_migration,
    rafraichir, refus, reponse_de_zone, verrou_des_put, zone_de,
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
pub fn verrou_de_la_liste() -> &'static tokio::sync::Mutex<()> {
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
    // Ordre des verrous : liste, puis zone. Le verrou de la zone est pris
    // AVANT de lire sa chaîne et tenu jusqu'à la pose du marqueur : un `PUT`
    // intercalé passe avant (sa chaîne est enregistrée) ou après (il
    // décide alors du marqueur), jamais entre les deux.
    let _liste = verrou_de_la_liste().lock().await;
    let verrou_zone = corps.zone_id.map(verrou_des_put);
    let _zone = match &verrou_zone {
        Some(v) => Some(v.lock().await),
        None => None,
    };
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
            // Revue #9520 v2 — un préréglage posé par son corps est une
            // NOUVELLE édition : les bornes de gain publiées valent pour lui
            // (sinon il les contournerait au chargement, où ses façades sont
            // « connues »).
            if let Err(e) = tune_core::tess::egaliseurs_en_serie::verifier_les_gains(&c, &[]) {
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
    // rc14 — égaliseurs combinables : une façade écartée par l'exclusivité
    // du 24/11 (marque `replaced_in_group`) est recochée.
    let mut chaine = chaine;
    let premium = state.license.is_premium().await;
    tune_core::tess::migration::normaliser_les_egaliseurs(&mut chaine, premium);
    let mut liste = match prereglages::lire(&settings) {
        Ok(l) => l,
        Err(e) => return stockage(e),
    };
    if liste.presets.len() >= prereglages::PREREGLAGES_MAX {
        return erreur(
            StatusCode::CONFLICT,
            "too_many_presets",
            &format!(
                "This server keeps at most {} presets. Delete one first.",
                prereglages::PREREGLAGES_MAX
            ),
            None,
        );
    }
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
    // Le préréglage est créé : un échec du marqueur ne le défait pas (un 500
    // pousserait à recréer un doublon `…-2`) ; il est dit en avertissement.
    let mut avertissements = Vec::new();
    if let Some(zone_id) = corps.zone_id
        && let Some(a) = poser_le_marqueur(&settings, zone_id, &p.id)
    {
        avertissements.push(a);
    }
    info!(preset_id = %p.id, zone_id = ?corps.zone_id, "tess_prereglage_enregistre");
    let mut v = serde_json::to_value(&p).unwrap_or_else(|_| json!({}));
    if !avertissements.is_empty()
        && let Some(o) = v.as_object_mut()
    {
        o.insert("warnings".into(), json!(avertissements));
    }
    (StatusCode::CREATED, Json(v)).into_response()
}

/// Pose le marqueur « ACTIVE » de la zone. En cas d'échec : journalisé,
/// l'ancien marqueur est effacé au mieux (aucun marqueur plutôt qu'un
/// marqueur faux) et un avertissement est rendu, jamais une erreur.
fn poser_le_marqueur(settings: &SettingsRepo, zone_id: i64, preset_id: &str) -> Option<String> {
    let cle = cle_prereglage_actif(zone_id);
    let e = settings.set(&cle, preset_id).err()?;
    warn!(zone_id, preset_id, erreur = %e, "tess_marqueur_actif_non_pose");
    effacer_le_marqueur(settings, zone_id);
    Some(AVERTISSEMENT_MARQUEUR.to_owned())
}

/// Efface le marqueur au mieux : suppression, ou à défaut valeur vide (lue
/// comme « aucun préréglage actif »). Rend `false` si les deux échouent.
fn effacer_le_marqueur(settings: &SettingsRepo, zone_id: i64) -> bool {
    let cle = cle_prereglage_actif(zone_id);
    match settings.delete(&cle) {
        Ok(_) => true,
        Err(e) => {
            warn!(zone_id, erreur = %e, "tess_marqueur_actif_non_efface");
            settings.set(&cle, "").is_ok()
        }
    }
}

/// L'avertissement rendu quand le marqueur « ACTIVE » n'a pas pu être écrit.
pub const AVERTISSEMENT_MARQUEUR: &str = "active_preset_not_stored";

/// rc14 — le mode PURE n'a pas été basculé par le préréglage parce que le
/// verrou de volume de la zone est armé : la bascule ferait sauter l'appareil
/// à 100 % (ou l'en ferait redescendre sans recomposer le trim), ce que
/// l'interrupteur PURE ne fait qu'après une confirmation explicite.
pub const AVERTISSEMENT_PURE_A_CONFIRMER: &str = "pure_mode_needs_confirmation";
/// rc14 — le réglage PURE de la zone n'a pas pu être écrit.
pub const AVERTISSEMENT_PURE_NON_ECRIT: &str = "pure_mode_not_stored";

enum BasculePure {
    Rien,
    Ecrite { cle: String, pure: bool },
    Avertissement(String),
}

/// La chaîne traite-t-elle quelque chose (greffon coché ou suréchantillonnage
/// demandé, hors « Bypass all ») ?
fn chaine_traite(c: &ChaineTess) -> bool {
    !c.bypass_all && (c.plugins.iter().any(|m| m.enabled) || c.fixed.upsampling.demande())
}

/// rc14 (recette .18) — charger « Pure » vidait la chaîne mais laissait le
/// mode PURE de la zone éteint ([`cle_mode_pure`], hors chaîne). Le
/// préréglage intégré l'allume désormais ; un préréglage qui traite l'éteint
/// (sinon PURE contournerait ce qu'on vient de charger). Un préréglage
/// utilisateur sans traitement n'y touche pas. `lock_volume` est gardé, et
/// rien ne bascule quand le verrou de volume est armé : c'est
/// l'interrupteur PURE, avec sa confirmation, qui le fait.
async fn accorder_le_mode_pure(state: &AppState, zone_id: i64, p: &PreReglage) -> BasculePure {
    use tune_core::audio::audiophile;
    let voulu = if p.builtin && p.id == prereglages::ID_PREREGLAGE_PURE {
        true
    } else if chaine_traite(&p.chain) {
        false
    } else {
        return BasculePure::Rien;
    };
    if audiophile::zone_enabled(&state.backend, zone_id) == voulu {
        return BasculePure::Rien;
    }
    // Comme l'interrupteur PURE de la zone : seule l'ENTRÉE en PURE
    // avec le verrou de volume armé (saut à 100 %) demande une confirmation.
    // La sortie est libre ; elle recompose la consigne (`set_volume_sortie_de_pure`).
    let verrou = audiophile::volume_lock_enabled(&state.backend, zone_id);
    if voulu && verrou {
        warn!(zone_id, "tess_prereglage_pure_a_confirmer");
        return BasculePure::Avertissement(AVERTISSEMENT_PURE_A_CONFIRMER.to_owned());
    }
    let sortie_du_plein_volume = !voulu && verrou;
    let settings = settings_de(state);
    // La clé est tenue par `tess.rs`, dont `rafraichir` l'applique
    // à la sortie vivante selon le mode, comme le reste du préréglage.
    let cle = cle_mode_pure(zone_id);
    let mut objet = settings
        .get(&cle)
        .ok()
        .flatten()
        .and_then(|b| serde_json::from_str::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    objet.insert("enabled".into(), Value::Bool(voulu));
    match settings.set(&cle, &Value::Object(objet).to_string()) {
        Ok(()) => {
            if sortie_du_plein_volume {
                // #6213 — après l'écriture, pour que le verrou ne morde plus :
                // l'appareil revient à la consigne de la zone, trim compris.
                let appareil = zone_de(state, zone_id)
                    .ok()
                    .and_then(|z| z.output_device_id);
                let consigne = state.playback.get_state(zone_id).await.volume;
                if let Err(e) = state
                    .orchestrator
                    .set_volume_sortie_de_pure(zone_id, consigne, appareil.as_deref())
                    .await
                {
                    warn!(zone_id, erreur = %e, "tess_prereglage_sortie_pure_non_recomposee");
                }
            }
            BasculePure::Ecrite { cle, pure: voulu }
        }
        Err(e) => {
            warn!(zone_id, erreur = %e, "tess_prereglage_pure_non_ecrit");
            BasculePure::Avertissement(AVERTISSEMENT_PURE_NON_ECRIT.to_owned())
        }
    }
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
        // Une zone dont le marqueur ne s'efface pas n'arrête pas les autres.
        if settings.get(&cle).ok().flatten().as_deref() == Some(preset_id.as_str()) {
            effacer_le_marqueur(&settings, id);
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
    drop(_garde);
    // Passer en « Immediately » : ce qui attendait la prochaine lecture
    // atteint la sortie vivante maintenant, et la file se vide.
    if mode.immediat() {
        appliquer_toute_l_attente(&state).await;
    }
    Json(liste).into_response()
}

/// Sert tout ce qui attend une prochaine lecture, zone par zone (sous le
/// verrou des `PUT` de chaque zone), et vide la file.
async fn appliquer_toute_l_attente(state: &AppState) {
    let zones: Vec<i64> = en_attente()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .copied()
        .collect();
    for zone_id in zones {
        // Toutes les sorties, réseau compris, par le même `rafraichir` que le
        // `PUT` en mode immédiat : sur une zone réseau qui joue, l'égaliseur
        // relance le flux à la position ; le crossfeed réseau attend la piste
        // suivante ; le reste n'agit qu'en local.
        servir_l_attente(state, zone_id, true).await;
    }
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
    // Le verrou de la liste est tenu jusqu'à la pose du marqueur : une
    // suppression concurrente passe avant (404) ou après (elle efface alors
    // le marqueur), jamais entre les deux. Ordre des verrous : liste, puis
    // zone (comme à l'enregistrement).
    let _liste = verrou_de_la_liste().lock().await;
    let liste = match prereglages::lire(&settings) {
        Ok(l) => l,
        Err(e) => return stockage(e),
    };
    let Some(p) = liste.get(&preset_id).cloned() else {
        return introuvable(&preset_id);
    };
    // rc14 — égaliseurs combinables : un préréglage enregistré sous
    // l'exclusivité du 24/11 retrouve les façades qu'elle avait décochées.
    let premium = state.license.is_premium().await;
    let mut chaine_du_prereglage = p.chain.clone();
    tune_core::tess::migration::normaliser_les_egaliseurs(&mut chaine_du_prereglage, premium);
    let corps = match serde_json::to_vec(&chaine_du_prereglage) {
        Ok(c) => c,
        Err(e) => return refus(&ErreurChaine::CorpsIllisible(e.to_string())),
    };
    let catalogue = catalogue_du_serveur();
    let verrou = verrou_des_put(id);
    let _garde = verrou.lock().await;
    // Le marqueur est effacé AVANT d'écrire la chaîne : si l'écriture du
    // nouveau échoue ensuite, il n'en reste aucun (jamais un faux). S'il ne
    // peut même pas être effacé, rien n'est modifié : refus propre. L'ancien
    // marqueur et la chaîne enregistrée sont notés pour le remettre si le
    // `PUT` refuse (402, 409, 400…) sans toucher à la chaîne.
    let cle_actif = cle_prereglage_actif(id);
    let ancien_marqueur = settings
        .get(&cle_actif)
        .ok()
        .flatten()
        .filter(|m| !m.is_empty());
    let chaine_avant = settings
        .get(&tune_core::tess::cle_chaine_zone(id))
        .ok()
        .flatten();
    if !effacer_le_marqueur(&settings, id) {
        return erreur(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_error",
            "The active preset of the zone could not be cleared; nothing was changed. Try again.",
            Some(&p.id),
        );
    }
    let cat = catalogue.clone();
    let s = settings_de(&state);
    let resultat =
        tokio::task::spawn_blocking(move || appliquer_le_put(&s, id, &corps, premium, &cat)).await;
    let resultat = match resultat {
        Ok(r) => r,
        Err(e) => Err(erreur_migration(&ErreurMigration::Stockage(e.to_string()))),
    };
    let (mut lecture, mut cles) = match resultat {
        Ok(x) => x,
        Err(r) => {
            // Refusé : si la chaîne enregistrée n'a pas bougé, l'ancien
            // marqueur la décrit toujours et revient. (Si elle a bougé, aucun
            // marqueur plutôt qu'un faux.)
            let chaine_apres = settings
                .get(&tune_core::tess::cle_chaine_zone(id))
                .ok()
                .flatten();
            if chaine_apres == chaine_avant
                && let Some(m) = &ancien_marqueur
                && let Err(e) = settings.set(&cle_actif, m)
            {
                warn!(zone_id = id, erreur = %e, "tess_marqueur_actif_non_remis");
            }
            return r;
        }
    };
    // La chaîne est écrite : quoi qu'il arrive au marqueur, elle atteint la
    // sortie selon le mode, et la réponse est un succès.
    let mut avertissements: Vec<String> = poser_le_marqueur(&settings, id, &p.id)
        .into_iter()
        .collect();
    // rc14 (recette .18) — « Pure » allume le mode PURE de la zone ; un
    // préréglage qui traite l'éteint. Même voie que le reste du préréglage :
    // la clé rejoint `cles` (tout de suite, ou à la prochaine lecture).
    match accorder_le_mode_pure(&state, id, &p).await {
        BasculePure::Rien => {}
        BasculePure::Ecrite { cle, pure } => {
            cles.push(cle);
            lecture.pure = pure;
        }
        BasculePure::Avertissement(a) => avertissements.push(a),
    }
    drop(_liste);
    let mode = appliquer_selon_le_mode(&state, &zone, cles.clone()).await;
    info!(zone_id = id, preset_id = %p.id, cles = ?cles, mode = ?mode, "tess_prereglage_charge");
    let reponse = reponse_de_zone(&state, &zone, lecture, premium, catalogue).await;
    let non_appliques = non_appliques(&reponse, &p);
    Json(json!({
        "preset_id": p.id,
        "apply_mode": if mode.immediat() { "immediately" } else { "at_next_playback" },
        "not_applied": non_appliques,
        "warnings": avertissements,
        "zone": reponse,
    }))
    .into_response()
}

/// Après un `PUT` à la main : la zone ne marque plus « ACTIVE » un préréglage
/// qui ne décrit plus sa chaîne (comparées sans les champs du serveur).
pub(crate) fn oublier_le_prereglage_s_il_differe(
    state: &AppState,
    zone_id: i64,
    chaine: &ChaineTess,
) -> Option<String> {
    let settings = settings_de(state);
    let cle = cle_prereglage_actif(zone_id);
    let actif = settings
        .get(&cle)
        .ok()
        .flatten()
        .filter(|a| !a.is_empty())?;
    // La chaîne ENREGISTRÉE (sans les droits) : sur un compte gratuit, la vue
    // convertie (P2) ne doit pas faire croire que la chaîne a changé.
    let enregistree = lire_zone(&settings, zone_id, true)
        .map(|l| l.chaine)
        .unwrap_or_else(|_| chaine.clone());
    let decrit = prereglages::lire(&settings)
        .ok()
        .and_then(|l| l.get(&actif).cloned())
        .is_some_and(|p| {
            // rc14 — comparé tel qu'il a été chargé (égaliseurs normalisés).
            [false, true].into_iter().any(|premium| {
                let mut c = p.chain.clone();
                tune_core::tess::migration::normaliser_les_egaliseurs(&mut c, premium);
                sans_champs_du_serveur(&c) == sans_champs_du_serveur(&enregistree)
            })
        });
    if !decrit && !effacer_le_marqueur(&settings, zone_id) {
        return Some(AVERTISSEMENT_MARQUEUR.to_owned());
    }
    None
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
    servir_l_attente(state, zone_id, false).await;
}

/// Combien de rafraîchissements de la sortie vivante la file d'attente a
/// DEMANDÉS pour la zone (banc et diagnostic). Compte des demandes, pas des
/// relances : ce qu'une demande produit sur la sortie dépend de l'étage
/// (voir l'en-tête du module).
pub fn rafraichissements_demandes(zone_id: i64) -> usize {
    rafraichis()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&zone_id)
        .copied()
        .unwrap_or(0)
}

fn rafraichis() -> &'static Mutex<HashMap<i64, usize>> {
    static R: OnceLock<Mutex<HashMap<i64, usize>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Sert la file d'une zone. `toutes_sorties` : rafraîchit aussi une sortie
/// réseau (passage en immédiat) ; sinon, au démarrage d'une lecture, seule
/// une sortie locale l'est (le flux réseau vient d'être fabriqué).
async fn servir_l_attente(state: &AppState, zone_id: i64, toutes_sorties: bool) {
    // Sous le verrou des `PUT` de la zone : un `PUT` (ou un chargement) en
    // cours écrit ses clés et décide de leur sort avant que la file soit lue.
    let verrou = verrou_des_put(zone_id);
    let _garde = verrou.lock().await;
    let cles = prendre_en_attente(zone_id);
    if cles.is_empty() {
        return;
    }
    let Ok(zone) = zone_de(state, zone_id) else {
        return;
    };
    let locale = sortie_locale(&zone);
    if locale || toutes_sorties {
        *rafraichis()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(zone_id)
            .or_default() += 1;
        rafraichir(state, &zone, &cles).await;
    }
    info!(zone_id, cles = ?cles, locale, "tess_applique_au_demarrage");
}

/// La zone sort-elle sur une sortie locale ? Lu dans `output_type` (absent =
/// locale), comme le volume de TESS : une zone réseau sans appareil connu
/// n'est pas locale.
pub fn sortie_locale(zone: &Zone) -> bool {
    zone.output_type.as_deref().unwrap_or("local") == "local"
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
