//! TESS T08 — l'API : `GET/PUT /zones/{id}/tess` et `GET /tess/catalog`.
//!
//! - `GET /zones/{id}/tess` : la chaîne de la zone (T02, [`lire_zone`]), son
//!   état réel étape par étape (demandé, effectif, motif ; sur le modèle de
//!   `crossfeed_status`), le badge et le catalogue.
//! - `PUT /zones/{id}/tess` : corps lu STRICTEMENT (`ChaineTess::depuis_corps`,
//!   400 `invalid_body`), fusionné avec la chaîne enregistrée pour les champs
//!   absents, validé (409 chaîne invalide, exclusivité du groupe crossfeed,
//!   version ; 402 droit manquant), puis écrit par [`ecrire_chaine`] (écriture
//!   croisée vers les anciennes clés, sous [`verrou_de_zone`]) ; la sortie
//!   vivante est rafraîchie pour chaque ancienne clé modifiée.
//! - `GET /tess/catalog` : le catalogue (schémas de réglages des caisses du
//!   SDK, droits, contraintes de place, politique des groupes) et l'état de
//!   chaque greffon pour le compte. Un serveur sans TESS répond 404 : c'est la
//!   capacité TESS que l'écran attend.
//!
//! **Droits.** Le droit vient de la licence, jamais du corps :
//! - l'origine d'une correction de pièce (D1 : `measured` Premium,
//!   `estimation` Free) est réécrite par le serveur d'après
//!   `room_profile_{id}` AVANT la validation ([`origines_par_le_serveur`]) ;
//! - les marques de lecture (`replaced_in_group`, `inert_on_network`) sont
//!   tenues par le serveur à l'écriture (`ecrire_chaine`) et ne marquent que
//!   des maillons décochés : elles ne lèvent aucun droit.
//!
//! Un maillon (ou l'upsampling) identique à la chaîne enregistrée n'est pas
//! revérifié au droit : il ne lève rien (la résolution l'éteint,
//! `premium_required`), et perdre le Premium ne bloque pas l'édition du reste
//! de la chaîne. Seulement si la chaîne enregistrée n'était pas sous
//! `bypass_all` : `ChaineTess::valider` contrôle le droit sous `bypass_all`
//! comme ailleurs, donc une chaîne enregistrée hors `bypass_all` a passé ce
//! contrôle (ou vient des anciennes routes, qui ont le leur).
//!
//! Un maillon absent du catalogue (paquet tiers installé mais non chargé)
//! renvoyé inchangé est conservé tel quel : il est validé comme un greffon
//! tiers ([`descripteur_tiers`], Premium) au lieu d'un 409 `unknown_plugin`.
//!
//! **Authentification** : celle de toutes les routes de zone (couche
//! `auth_middleware` de `/api/v1`), rien de plus.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use axum::body::Bytes;
use axum::response::Response;

use tune_core::tess::TauxDsd;
use tune_core::tess::api::{RefusTess, ReponseCatalogue, ReponseTessZone};
use tune_core::tess::chaine::{
    ChaineTess, ContexteValidation, ErreurChaine, MaillonGreffon, Upsampling,
};
use tune_core::tess::greffon::{
    Canaux, Catalogue, Categorie, ContraintesPlace, DescripteurGreffon, Droit,
    GROUPE_CROSSFEED_CROSSTALK, Origine, PolitiqueGroupe, Position, catalogue_de_reference,
    id_valide,
};
use tune_core::tess::migration::{
    ErreurMigration, ID_CONVOLUTION, ID_CORRECTION_PIECE, ID_CROSSFEED, ID_EQ_CASQUE,
    ID_EQ_GRAPHIQUE, ID_EQ_PARAMETRIQUE, ID_MONO, ID_REAFFECTATION, LectureZone, Partie,
    correction_estimee, ecrire_chaine, lecture_dsd, lire_zone, origines_par_le_serveur,
};
use tune_core::tess::resolution::{
    ContexteExecution, ControleVolume, LectureDsd, SourceFlux, VolumeZone,
};
use tune_core::tess::zones::TransportZone;

use super::*;

/// L'id du greffon Declick temps réel : pas encore branché dans l'hôte (T13).
const ID_DECLICK_LIVE: &str = "declick-live";

/// Le préréglage chargé sur la zone (« ACTIVE »), posé par T17.
pub fn cle_prereglage_actif(zone_id: i64) -> String {
    format!("zone_{zone_id}_tess_active_preset")
}

// ── Catalogue ───────────────────────────────────────────────────────

const SCHEMA_EGALISEUR: &str =
    include_str!("../../../../sdk/tune-plugin-equalizer/schemas/config.json");
const SCHEMA_CROSSFEED: &str =
    include_str!("../../../../sdk/tune-plugin-crossfeed/schemas/config.json");
const SCHEMA_REAFFECTATION: &str =
    include_str!("../../../../sdk/tune-plugin-channel-remap/schemas/config.json");

/// Le schéma publié par la caisse du SDK, sans `enabled` : dans la chaîne,
/// c'est le maillon qui le porte, et ses `settings` n'en ont pas.
pub fn schema_de_maillon(brut: &str) -> Value {
    let mut v: Value = serde_json::from_str(brut).unwrap_or_else(|_| json!({ "type": "object" }));
    if let Some(o) = v.as_object_mut() {
        if let Some(p) = o.get_mut("properties").and_then(Value::as_object_mut) {
            p.remove("enabled");
        }
        if let Some(r) = o.get_mut("required").and_then(Value::as_array_mut) {
            r.retain(|k| k != "enabled");
        }
    }
    v
}

fn schema_de_reference(id: &str) -> Option<&'static str> {
    match id {
        ID_EQ_GRAPHIQUE | ID_EQ_PARAMETRIQUE | ID_EQ_CASQUE | ID_CORRECTION_PIECE => {
            Some(SCHEMA_EGALISEUR)
        }
        ID_CROSSFEED => Some(SCHEMA_CROSSFEED),
        ID_REAFFECTATION => Some(SCHEMA_REAFFECTATION),
        _ => None,
    }
}

/// Le descripteur d'un paquet natif tiers chargé, absent du catalogue de
/// référence : Premium (droit des greffons tiers), schéma fourni par le
/// paquet lui-même (l'hôte n'en vérifie que l'objet).
fn descripteur_tiers(id: &str) -> DescripteurGreffon {
    let famille = id.starts_with(tune_core::tess::greffon::PREFIXE_FAMILLE_CROSSFEED);
    DescripteurGreffon {
        id: id.to_owned(),
        name: tune_core::audio::natifs_tiers::nom_affichable(id),
        category: if famille {
            Categorie::Spatial
        } else {
            Categorie::RestorationAndRouting
        },
        entitlement: Droit::Premium,
        feature: Some(crate::native_audio::THIRD_PARTY_FEATURE.code().to_owned()),
        origin: Origine::Package,
        settings_schema: json!({ "type": "object" }),
        placement: ContraintesPlace {
            channels: if famille {
                Canaux::StereoOnly
            } else {
                Canaux::Any
            },
            position: Position::Free,
            // Le groupe vient du préfixe (`groupe_effectif`).
            exclusive_group: None,
        },
    }
}

/// Le catalogue que l'API applique : la référence (T01), les schémas de
/// réglages des caisses du SDK, les paquets tiers chargés, et le groupe
/// crossfeed / Crossfeed Pro / Crosstalk en politique `refuse` (décision du
/// 18/11 : deux membres cochés = 409). La résolution, elle, reste tolérante
/// pour une chaîne ancienne (priorité).
pub fn catalogue_tess(paquets_charges: &[String]) -> Catalogue {
    let mut c = catalogue_de_reference();
    for d in &mut c.plugins {
        if let Some(brut) = schema_de_reference(&d.id) {
            d.settings_schema = schema_de_maillon(brut);
        }
    }
    for id in paquets_charges {
        if id_valide(id) && c.get(id).is_none() {
            c.plugins.push(descripteur_tiers(id));
        }
    }
    for g in &mut c.groups {
        if g.id == GROUPE_CROSSFEED_CROSSTALK {
            g.policy = PolitiqueGroupe::Refuse;
        }
    }
    c
}

fn catalogue_du_serveur() -> Catalogue {
    catalogue_tess(&tune_core::audio::natifs_tiers::identifiants_charges())
}

/// Le greffon est-il installé, activé et branché dans l'hôte ?
pub fn greffon_disponible(settings: &SettingsRepo, id: &str) -> bool {
    use tune_core::audio::premium_plugins::enabled;
    match id {
        ID_EQ_GRAPHIQUE | ID_EQ_PARAMETRIQUE | ID_EQ_CASQUE | ID_CORRECTION_PIECE => {
            enabled(settings, "equalizer")
        }
        ID_CROSSFEED => enabled(settings, "crossfeed"),
        ID_REAFFECTATION => enabled(settings, "channel-remap"),
        ID_CONVOLUTION | ID_MONO => true,
        // Le moteur existe (T13) ; l'hôte ne le fait pas encore tourner.
        ID_DECLICK_LIVE => false,
        autre => tune_core::audio::natifs_tiers::actif(settings, autre),
    }
}

/// `GET /tess/catalog`
pub async fn catalogue_tess_route(State(state): State<AppState>) -> impl IntoResponse {
    let premium = state.license.is_premium().await;
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let disponible = |id: &str| greffon_disponible(&settings, id);
    Json(ReponseCatalogue::depuis(
        catalogue_du_serveur(),
        premium,
        &disponible,
    ))
}

// ── Contexte de la zone ─────────────────────────────────────────────

fn zone_introuvable(zone_id: i64) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "zone_not_found", "zone_id": zone_id })),
    )
        .into_response()
}

/// Le refus du contrat (`status`, `code`, `detail`, `plugin_id`), avec les
/// champs des autres refus de l'API (`premium_guard`) : `error` (= `code`),
/// `message` (= `detail`) et, sur un 402, `upgrade_url`. Un client qui
/// affiche `message` tel quel (client web, `api.ts`) dit donc pourquoi.
pub fn corps_du_refus(r: &RefusTess) -> Value {
    let mut v = serde_json::to_value(r).unwrap_or_else(|_| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("error".into(), json!(r.code));
        o.insert("message".into(), json!(r.detail));
        if r.status == 402 {
            o.insert(
                "upgrade_url".into(),
                json!(tune_core::license::URL_OFFRE_PREMIUM),
            );
        }
    }
    v
}

fn refus(e: &ErreurChaine) -> Response {
    let r = RefusTess::from(e);
    let statut = StatusCode::from_u16(r.status).unwrap_or(StatusCode::CONFLICT);
    (statut, Json(corps_du_refus(&r))).into_response()
}

/// Une clé de la zone ne se relit pas (T02 : refus explicite, jamais une
/// chaîne vide) : l'écran le dit, la lecture garde son chemin actuel.
fn erreur_migration(e: &ErreurMigration) -> Response {
    match e {
        ErreurMigration::ReglagesInvalides { id, raison } => {
            refus(&ErreurChaine::ReglagesInvalides {
                id: id.clone(),
                raison: raison.clone(),
            })
        }
        ErreurMigration::VersionPlusRecente { trouvee } => {
            refus(&ErreurChaine::VersionNonPriseEnCharge {
                trouvee: *trouvee,
                attendue: tune_core::tess::VERSION_CHAINE,
            })
        }
        ErreurMigration::CleCorrompue { cle, raison } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "tess_setting_unreadable", "key": cle, "detail": raison })),
        )
            .into_response(),
        ErreurMigration::Stockage(detail) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "storage_error", "detail": detail })),
        )
            .into_response(),
    }
}

fn taux_dsd(cadence: u32) -> TauxDsd {
    match cadence {
        c if c >= 11_289_600 => TauxDsd::Dsd256,
        c if c >= 5_644_800 => TauxDsd::Dsd128,
        _ => TauxDsd::Dsd64,
    }
}

/// Le flux qui entre dans la chaîne, quand quelque chose joue.
fn source_de(etat: &tune_core::playback::ZoneState, dsd_mode: &str) -> Option<SourceFlux> {
    if etat.state == tune_core::playback::PlayState::Stopped {
        return None;
    }
    let np = etat.now_playing.as_ref()?;
    let cadence = np.sample_rate?;
    let channels = np.channels.unwrap_or(2);
    let format = np
        .format
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(format.as_str(), "dsf" | "dff" | "dsd") {
        return Some(SourceFlux::Dsd {
            rate: taux_dsd(cadence),
            channels,
            // `auto` : la sortie décide ; prudence = hors traitement.
            played_as: lecture_dsd(dsd_mode).unwrap_or(LectureDsd::Unknown),
        });
    }
    Some(SourceFlux::Pcm {
        sample_rate_hz: cadence,
        bits: np.bit_depth.unwrap_or(16).min(64) as u8,
        channels,
    })
}

/// Le volume de la zone tel que TESS le voit, sur le modèle du chemin du
/// signal (T16) : fixe (zone à volume fixe, ou sortie qui ne sait pas le
/// régler), logiciel sur une sortie locale, réglé par l'appareil ailleurs.
/// `zones.volume` est sur 0 à 100 ; le verrou Pure (`audiophile_lock_volume`)
/// le tient à 100 % (`effective_volume`), donc 0 dB.
async fn volume_de(state: &AppState, zone: &Zone) -> VolumeZone {
    let capacites = super::output_capabilities(state, zone.output_device_id.as_deref()).await;
    if zone.fixed_volume || capacites.is_some_and(|c| !c.can_set_volume) {
        return VolumeZone {
            control: ControleVolume::Fixed,
            db: 0.0,
        };
    }
    if zone.output_type.as_deref().unwrap_or("local") != "local" {
        return VolumeZone {
            control: ControleVolume::Device,
            db: 0.0,
        };
    }
    let gain = tune_core::audio::audiophile::effective_volume(
        &state.backend,
        zone.id.unwrap_or_default(),
        zone.volume / 100.0,
    );
    let db = if gain >= 1.0 {
        0.0
    } else {
        tune_core::audio::volume_scale::linear_to_db(gain)
            .unwrap_or(-120.0)
            .max(-120.0)
    };
    VolumeZone {
        control: ControleVolume::Software,
        db,
    }
}

/// La réponse de la zone, à partir de sa chaîne lue (T02).
async fn reponse_de_zone(
    state: &AppState,
    zone: &Zone,
    lecture: LectureZone,
    premium: bool,
    catalogue: Catalogue,
) -> ReponseTessZone {
    let zone_id = zone.id.unwrap_or_default();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let etat = state.playback.get_state(zone_id).await;
    let volume = volume_de(state, zone).await;
    let disponible = |id: &str| greffon_disponible(&settings, id);
    let ctx = ContexteExecution {
        transport: TransportZone::depuis_type_de_sortie(zone.output_type.as_deref()),
        source: source_de(&etat, &lecture.dsd_mode),
        premium,
        greffon_disponible: &disponible,
        pure: lecture.pure,
        volume,
    };
    let actif = settings
        .get(&cle_prereglage_actif(zone_id))
        .ok()
        .flatten()
        .filter(|p| !p.is_empty());
    ReponseTessZone::depuis(zone_id, lecture.chaine, actif, catalogue, &ctx)
}

fn zone_de(state: &AppState, zone_id: i64) -> Result<Zone, Response> {
    match ZoneRepo::with_backend(state.backend.clone()).get(zone_id) {
        Ok(Some(z)) => Ok(z),
        Ok(None) => Err(zone_introuvable(zone_id)),
        Err(e) => Err(erreur_migration(&ErreurMigration::Stockage(e))),
    }
}

/// `GET /zones/{id}/tess`
pub async fn get_zone_tess(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let zone = match zone_de(&state, id) {
        Ok(z) => z,
        Err(r) => return r,
    };
    let premium = state.license.is_premium().await;
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let lecture = match tokio::task::spawn_blocking(move || lire_zone(&settings, id, premium)).await
    {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => return erreur_migration(&e),
        Err(e) => return erreur_migration(&ErreurMigration::Stockage(e.to_string())),
    };
    Json(reponse_de_zone(&state, &zone, lecture, premium, catalogue_du_serveur()).await)
        .into_response()
}

// ── PUT ─────────────────────────────────────────────────────────────

/// Un `PUT` à la fois par zone, de la lecture de la chaîne enregistrée à son
/// écriture (comparaison des marques et des maillons inchangés). L'écriture
/// elle-même prend en plus [`verrou_de_zone`] (dans [`ecrire_chaine`]), qui
/// l'ordonne avec la migration de T03.
fn verrou_des_put(zone_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    static VERROUS: OnceLock<std::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut table = VERROUS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    table.entry(zone_id).or_default().clone()
}

/// Un champ absent du corps garde sa valeur enregistrée (`fixed` champ par
/// champ) : un client qui ne connaît pas un champ plus récent ne l'efface
/// pas en renvoyant la chaîne (reste (1) de T01). La liste `plugins`, quand
/// elle est présente, remplace la liste enregistrée.
pub fn fusionner(corps: &Value, enregistree: &ChaineTess) -> Result<ChaineTess, ErreurChaine> {
    let illisible = |e: serde_json::Error| ErreurChaine::CorpsIllisible(e.to_string());
    let mut base = serde_json::to_value(enregistree).map_err(illisible)?;
    if let (Some(b), Some(c)) = (base.as_object_mut(), corps.as_object()) {
        for (k, v) in c {
            if let (true, Some(fixe_corps), Some(Value::Object(fixe))) =
                (k == "fixed", v.as_object(), b.get_mut(k))
            {
                for (kf, vf) in fixe_corps {
                    fixe.insert(kf.clone(), vf.clone());
                }
                continue;
            }
            b.insert(k.clone(), v.clone());
        }
    }
    serde_json::from_value(base).map_err(illisible)
}

/// Valide la chaîne : d'abord la forme (409) avec tous les droits, puis le
/// droit (402) sur ce qui CHANGE par rapport à la chaîne enregistrée. Une
/// correction de pièce calculée sans micro (D1, origine `estimation` posée
/// par le serveur) est Free.
pub fn valider_le_put(
    chaine: &ChaineTess,
    enregistree: &ChaineTess,
    catalogue: &Catalogue,
    premium: bool,
) -> Result<(), ErreurChaine> {
    let inchange = |m: &MaillonGreffon| {
        enregistree
            .plugins
            .iter()
            .any(|e| e.id == m.id && e.enabled == m.enabled && e.settings == m.settings)
    };
    // Un maillon inconnu du catalogue mais inchangé (paquet tiers installé,
    // non chargé) est gardé : validé comme un greffon tiers.
    let mut avec_inconnus = None;
    for m in &chaine.plugins {
        if catalogue.get(&m.id).is_none() && id_valide(&m.id) && inchange(m) {
            avec_inconnus
                .get_or_insert_with(|| catalogue.clone())
                .plugins
                .push(descripteur_tiers(&m.id));
        }
    }
    let catalogue = avec_inconnus.as_ref().unwrap_or(catalogue);
    chaine.valider(catalogue, ContexteValidation { premium: true })?;
    if premium {
        return Ok(());
    }
    // L'exemption « inchangé » suppose que la chaîne enregistrée a passé le
    // contrôle du droit : jamais si elle était sous `bypass_all`.
    let exemptable = !enregistree.bypass_all;
    let mut a_payer = chaine.clone();
    for m in &mut a_payer.plugins {
        if (exemptable && inchange(m)) || correction_estimee(m) {
            m.enabled = false;
        }
    }
    if exemptable && a_payer.fixed.upsampling == enregistree.fixed.upsampling {
        a_payer.fixed.upsampling = Upsampling::Off {};
    }
    a_payer.valider(catalogue, ContexteValidation { premium })
}

/// Le P2 inverse (et D1) : sans Premium, le `GET` montre l'égaliseur
/// paramétrique coché, ou la correction de pièce MESURÉE cochée
/// (`conversion_piece`), converti en graphique. Renvoyé tel quel, ce
/// graphique redevient le maillon enregistré : la chaîne enregistrée ne
/// dépend pas du droit (T02, B1), et le compte qui redevient Premium le
/// retrouve. Un graphique modifié, lui, remplace le maillon Premium.
pub fn defaire_p2(
    chaine: &mut ChaineTess,
    enregistree: &ChaineTess,
    vue_gratuite: &ChaineTess,
    conversion_piece: bool,
) {
    let Some(converti) = vue_gratuite
        .plugins
        .iter()
        .find(|m| m.id == ID_EQ_GRAPHIQUE)
    else {
        return;
    };
    let source = if conversion_piece {
        ID_CORRECTION_PIECE
    } else {
        ID_EQ_PARAMETRIQUE
    };
    let Some(premium) = enregistree
        .plugins
        .iter()
        .find(|m| m.id == source && m.enabled && !correction_estimee(m))
    else {
        return;
    };
    if chaine.plugins.iter().any(|m| m.id == source) {
        return;
    }
    if let Some(m) = chaine.plugins.iter_mut().find(|m| {
        m.id == converti.id && m.enabled == converti.enabled && m.settings == converti.settings
    }) {
        *m = premium.clone();
    }
}

/// La partie synchrone d'un `PUT` : lecture, fusion, droits, validation,
/// écriture. Rend la lecture après écriture et les anciennes clés modifiées.
fn appliquer_le_put(
    settings: &SettingsRepo,
    zone_id: i64,
    corps: &[u8],
    premium: bool,
    catalogue: &Catalogue,
) -> Result<(LectureZone, Vec<String>), Response> {
    let texte = std::str::from_utf8(corps).map_err(|e| {
        refus(&ErreurChaine::CorpsIllisible(format!(
            "body is not UTF-8: {e}"
        )))
    })?;
    // Lecture stricte du corps tel qu'envoyé : 400 sur tout champ ou valeur
    // inconnus, avant toute fusion.
    ChaineTess::depuis_corps(texte).map_err(|e| refus(&e))?;
    let brut: Value = serde_json::from_str(texte)
        .map_err(|e| refus(&ErreurChaine::CorpsIllisible(e.to_string())))?;

    // La chaîne enregistrée, neutre vis-à-vis du droit. Illisible : le PUT
    // la remplace (rien à comparer, rien d'exempté).
    let enregistree = lire_zone(settings, zone_id, true)
        .map(|l| l.chaine)
        .unwrap_or_default();
    if enregistree.version > tune_core::tess::VERSION_CHAINE {
        // Écrite par une version plus récente : jamais réécrite ici.
        return Err(refus(&ErreurChaine::VersionNonPriseEnCharge {
            trouvee: enregistree.version,
            attendue: tune_core::tess::VERSION_CHAINE,
        }));
    }
    let mut chaine = fusionner(&brut, &enregistree).map_err(|e| refus(&e))?;
    if !premium
        && let Ok(vue) = lire_zone(settings, zone_id, false)
        && vue.conversion_p2
    {
        defaire_p2(&mut chaine, &enregistree, &vue.chaine, vue.conversion_piece);
    }
    // D1 : l'origine d'une correction de pièce vient de `room_profile_{id}`,
    // jamais du corps ; `ecrire_chaine` la réécrit de même.
    let chaine = origines_par_le_serveur(settings, zone_id, &chaine);
    valider_le_put(&chaine, &enregistree, catalogue, premium).map_err(|e| refus(&e))?;
    let chaine = chaine.mettre_a_niveau();

    let bilan = ecrire_chaine(settings, zone_id, &chaine).map_err(|e| erreur_migration(&e))?;
    let lecture = lire_zone(settings, zone_id, premium).map_err(|e| erreur_migration(&e))?;
    Ok((lecture, bilan.cles_modifiees))
}

/// Chaque ancienne clé réécrite atteint la sortie vivante, comme par son
/// ancienne route (`REGLAGES_A_RAFRAICHIR`).
async fn rafraichir(state: &AppState, zone: &Zone, cles: &[String]) {
    let id = zone.id.unwrap_or_default();
    let est = |p: Partie| cles.contains(&p.cle(id));
    let natif = cles
        .iter()
        .any(|c| c.starts_with(&format!("zone_{id}_native_plugin_")));
    if est(Partie::Egaliseur) {
        let _ = state.orchestrator.apply_eq_change_portee(id).await;
    }
    if est(Partie::Crossfeed) || natif {
        let _ = state.orchestrator.refresh_zone_crossfeed_portee(id).await;
    }
    if est(Partie::Mono) {
        let _ = state.orchestrator.refresh_zone_mono_downmix(id).await;
    }
    if est(Partie::Reaffectation) {
        let _ = state.orchestrator.refresh_zone_reaffectation(id).await;
    }
    if est(Partie::Surechantillonnage) {
        state.orchestrator.refresh_zone_upsampler(id).await;
    }
    if est(Partie::Convolution) {
        rafraichir_le_convolueur(state, zone).await;
    }
}

/// La convolution d'une sortie locale : posée ou retirée à chaud, comme par
/// `POST /room-correction/ir/…` (le transcodage réseau relit la clé).
async fn rafraichir_le_convolueur(state: &AppState, zone: &Zone) {
    #[cfg(feature = "local-audio")]
    {
        let Some(device_id) = zone.output_device_id.as_deref() else {
            return;
        };
        if !device_id.starts_with("local:") {
            return;
        }
        let id = zone.id.unwrap_or_default();
        let chemin = SettingsRepo::with_backend(state.backend.clone())
            .get(&Partie::Convolution.cle(id))
            .ok()
            .flatten()
            .filter(|c| !c.trim().is_empty());
        let outputs = state.outputs.lock().await;
        if let Some(output) = outputs.get(device_id) {
            let output = output.lock().await;
            if let Some(local) = output
                .as_any()
                .downcast_ref::<tune_core::outputs::local::LocalOutput>()
            {
                match chemin {
                    Some(c) => {
                        if let Err(e) = local.set_convolver_ir(&c) {
                            warn!(zone_id = id, error = %e, "tess_convolueur_non_pose");
                        }
                    }
                    None => local.clear_convolver(),
                }
            }
        }
    }
    #[cfg(not(feature = "local-audio"))]
    let _ = (state, zone);
}

/// `PUT /zones/{id}/tess`
pub async fn put_zone_tess(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    corps: Bytes,
) -> Response {
    let zone = match zone_de(&state, id) {
        Ok(z) => z,
        Err(r) => return r,
    };
    let premium = state.license.is_premium().await;
    let catalogue = catalogue_du_serveur();
    let verrou = verrou_des_put(id);
    let _garde = verrou.lock().await;
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let cat = catalogue.clone();
    let resultat =
        tokio::task::spawn_blocking(move || appliquer_le_put(&settings, id, &corps, premium, &cat))
            .await;
    let (lecture, cles) = match resultat {
        Ok(Ok(x)) => x,
        Ok(Err(r)) => return r,
        Err(e) => return erreur_migration(&ErreurMigration::Stockage(e.to_string())),
    };
    rafraichir(&state, &zone, &cles).await;
    info!(zone_id = id, cles = ?cles, "tess_chaine_ecrite");
    Json(reponse_de_zone(&state, &zone, lecture, premium, catalogue).await).into_response()
}
