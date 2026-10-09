//! Export et restauration GRATUITS de la configuration, zones comprises
//! (fil forum 2110, ticket 221).
//!
//! `GET /system/config/export` rendait la table `settings` à plat, et
//! `POST /system/config/import` la réécrivait : les zones n'y étaient pas, et
//! rien ne disait avant d'appliquer ce que le fichier allait changer. La
//! sauvegarde complète ([`crate::config_backup`]) reste la fonction Premium ;
//! ce module n'y touche pas, il en relit seulement les colonnes de zone
//! ([`crate::config_backup::export_zones`]) pour ne pas en tenir une seconde
//! liste.
//!
//! # Format
//!
//! ```json
//! { "format": "tune-config", "format_version": 2,
//!   "settings": { "theme": "…", … },
//!   "zones": [ { "id": 3, "name": "Salon", "output_device_id": "…", …,
//!                "settings": { "zone_{id}_crossfeed": "…" } } ] }
//! ```
//!
//! Un fichier SANS `format_version` est l'ancien export : la table `settings`
//! à plat. Il se restaure exactement comme avant.
//!
//! # Règles de la restauration
//!
//! - Une zone du fichier est rapprochée d'une zone de la machine par son
//!   **identifiant d'appareil** (`output_device_id`), jamais par son numéro ni
//!   par son nom : le numéro ne désigne pas la même zone d'une machine à
//!   l'autre, et deux appareils peuvent porter le même nom.
//! - Une zone dont l'appareil n'a aucune zone ici est CRÉÉE **hors ligne** : la
//!   découverte la remettra en ligne si l'appareil répond.
//! - Une zone de la machine absente du fichier n'est jamais touchée, et aucune
//!   zone n'est jamais supprimée.
//! - Les réglages de zone rangés dans `settings` (`zone_{id}_…`,
//!   `dac_profile_{id}`…) voyagent DANS leur zone, sous un gabarit `{id}`, et
//!   sont réécrits sous le numéro de la zone d'arrivée. Les deux réglages qui
//!   citent des zones par leur numéro (`default_zone_id`, `zone_groups`) sont
//!   traduits de la même façon.
//! - Le volume fixe n'est jamais réarmé (même règle que la sauvegarde
//!   Premium, #2395) : c'est une commande à 100 %, qui demande une
//!   confirmation que la restauration ne rencontre pas.
//! - Aucun secret ne sort : [`crate::secrets::retirer_les_secrets`] s'applique
//!   aux réglages et aux réglages de chaque zone.
//!
//! # Aperçu
//!
//! [`planifier`] calcule TOUT ce que l'import ferait, sans rien écrire ;
//! [`appliquer`] exécute ce plan-là. L'aperçu et l'import sont donc la même
//! décision, pas deux calculs qui pourraient diverger.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::{AutoplayMode, ZoneRepo, gabarit_de_cle_de_zone};
use crate::secrets::{est_secret, retirer_les_secrets};

/// Valeur du champ `format` d'un export.
pub const FORMAT: &str = "tune-config";
/// Version du format écrite par ce serveur. 1 = l'ancien export à plat.
pub const FORMAT_VERSION: u64 = 2;

/// Les champs de zone comparés et restaurés, dans l'ordre de l'aperçu.
///
/// Ni `id` (il change d'une machine à l'autre), ni `online` (un état, pas un
/// réglage), ni `fixed_volume` (jamais réarmé, voir l'en-tête).
const CHAMPS_DE_ZONE: &[&str] = &[
    "name",
    "output_type",
    "volume",
    "muted",
    "gapless_enabled",
    "group_id",
    "sync_delay_ms",
    "max_sample_rate",
    "autoplay_mode",
    "dsd_mode",
    "dlna_native_flac",
    "alac_passthrough",
    "aac_passthrough",
    "dlna_lpcm",
    "dlna_cap_16bit",
    "dlna_wav24",
    "dlna_play_delay_ms",
    "lyrics_offset_ms",
];

// ── Export ──────────────────────────────────────────────────────────

/// Une valeur de `settings` telle que l'export la rend : le JSON qu'elle
/// contient quand elle en contient, la chaîne sinon (comportement historique).
fn valeur_exportee(brut: &str) -> Value {
    serde_json::from_str::<Value>(brut).unwrap_or_else(|_| Value::String(brut.to_string()))
}

/// Les champs de zone qui ne sont pas des colonnes de `export_zones`, lus par
/// les accesseurs du dépôt (qui savent composer avec une colonne absente).
fn champs_lus_par_le_depot(repo: &ZoneRepo, id: i64, obj: &mut Map<String, Value>) {
    obj.insert(
        "autoplay_mode".into(),
        json!(repo.get_autoplay_mode(id).as_str()),
    );
    obj.insert("dsd_mode".into(), json!(repo.get_dsd_mode(id)));
    obj.insert(
        "dlna_native_flac".into(),
        json!(repo.get_dlna_native_flac(id)),
    );
    obj.insert(
        "alac_passthrough".into(),
        json!(repo.get_alac_passthrough(id)),
    );
    obj.insert(
        "aac_passthrough".into(),
        json!(repo.get_aac_passthrough(id)),
    );
    obj.insert("dlna_lpcm".into(), json!(repo.get_dlna_lpcm(id)));
    obj.insert("dlna_cap_16bit".into(), json!(repo.get_dlna_cap_16bit(id)));
    obj.insert("dlna_wav24".into(), json!(repo.get_dlna_wav24(id)));
    obj.insert(
        "dlna_play_delay_ms".into(),
        json!(repo.get_dlna_play_delay_ms(id)),
    );
    obj.insert(
        "lyrics_offset_ms".into(),
        json!(repo.get_lyrics_offset_ms(id)),
    );
}

/// Les zones VISIBLES, au format de l'export, réglages de zone compris.
///
/// Une zone masquée est une zone que l'utilisateur a supprimée : elle ne part
/// pas, sans quoi la restaurer la ferait renaître.
fn zones_exportees(
    backend: &Arc<dyn DbBackend>,
    reglages: &[(String, String)],
) -> Result<Vec<Value>, String> {
    let repo = ZoneRepo::with_backend(backend.clone());
    let visibles: HashSet<i64> = repo.list()?.into_iter().filter_map(|z| z.id).collect();

    let mut par_zone: HashMap<i64, Map<String, Value>> = HashMap::new();
    for (cle, brut) in reglages {
        if let Some((id, gabarit)) = gabarit_de_cle_de_zone(cle)
            && visibles.contains(&id)
        {
            par_zone
                .entry(id)
                .or_default()
                .insert(gabarit, valeur_exportee(brut));
        }
    }

    let mut zones = Vec::new();
    for mut zone in crate::config_backup::export_zones(backend)? {
        let Some(id) = zone.get("id").and_then(Value::as_i64) else {
            continue;
        };
        if !visibles.contains(&id) {
            continue;
        }
        let Some(obj) = zone.as_object_mut() else {
            continue;
        };
        obj.remove("online");
        // La colonne brute varie d'un moteur à l'autre ("0", 0, "random_album") :
        // on exporte le NOM du mode, lu comme le lit la lecture.
        obj.remove("autoplay_enabled");
        champs_lus_par_le_depot(&repo, id, obj);
        let mut leurs_reglages = par_zone.remove(&id).unwrap_or_default();
        // Toujours, même avec `include_secrets` : un réglage de zone n'a
        // aucune raison de porter un secret, et s'il en portait un il partirait
        // vers une autre machine sous un autre numéro.
        retirer_les_secrets(&mut leurs_reglages);
        obj.insert("settings".into(), Value::Object(leurs_reglages));
        zones.push(zone);
    }
    Ok(zones)
}

/// Construit l'export gratuit : réglages + zones, format versionné.
///
/// Les réglages de zone (`zone_{id}_…`) quittent la carte `settings` pour
/// voyager dans leur zone ; ceux d'une zone masquée ou disparue ne partent
/// pas — ils ne désigneraient rien sur la machine d'arrivée.
pub fn exporter(backend: &Arc<dyn DbBackend>, inclure_secrets: bool) -> Result<Value, String> {
    let reglages = SettingsRepo::with_backend(backend.clone()).all()?;
    let zones = zones_exportees(backend, &reglages)?;

    let mut carte = Map::new();
    for (cle, brut) in &reglages {
        if gabarit_de_cle_de_zone(cle).is_some() {
            continue;
        }
        carte.insert(cle.clone(), valeur_exportee(brut));
    }
    if !inclure_secrets {
        retirer_les_secrets(&mut carte);
    }

    Ok(json!({
        "format": FORMAT,
        "format_version": FORMAT_VERSION,
        "server_version": crate::version(),
        "exported_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "settings": Value::Object(carte),
        "zones": zones,
    }))
}

// ── Lecture du fichier ──────────────────────────────────────────────

/// Un fichier d'export lu et validé.
#[derive(Debug, Clone)]
pub struct Fichier {
    /// 1 pour l'ancien export à plat, sinon la version déclarée.
    pub format_version: u64,
    pub settings: Vec<(String, Value)>,
    pub zones: Vec<Map<String, Value>>,
}

/// Valide un corps d'import. Rien n'est écrit ; une erreur ici = `400`.
pub fn lire(corps: Map<String, Value>) -> Result<Fichier, String> {
    let Some(version) = corps.get("format_version") else {
        // Ancien format : la table `settings` à plat, sans zones.
        let mut settings = Vec::with_capacity(corps.len());
        for (cle, valeur) in corps {
            if cle.trim().is_empty() {
                return Err("empty setting key".into());
            }
            settings.push((cle, valeur));
        }
        return Ok(Fichier {
            format_version: 1,
            settings,
            zones: Vec::new(),
        });
    };
    let version = version
        .as_u64()
        .ok_or_else(|| "format_version must be a positive integer".to_string())?;
    if version > FORMAT_VERSION {
        return Err(format!(
            "unsupported format_version {version} (this server reads up to {FORMAT_VERSION})"
        ));
    }

    let mut settings = Vec::new();
    match corps.get("settings") {
        None | Some(Value::Null) => {}
        Some(Value::Object(carte)) => {
            for (cle, valeur) in carte {
                if cle.trim().is_empty() {
                    return Err("empty setting key".into());
                }
                settings.push((cle.clone(), valeur.clone()));
            }
        }
        Some(_) => return Err("'settings' must be an object".into()),
    }

    let mut zones = Vec::new();
    match corps.get("zones") {
        None | Some(Value::Null) => {}
        Some(Value::Array(liste)) => {
            for (rang, zone) in liste.iter().enumerate() {
                let Some(obj) = zone.as_object() else {
                    return Err(format!("zone #{rang} is not an object"));
                };
                let nom = obj.get("name").and_then(Value::as_str).unwrap_or("");
                if nom.trim().is_empty() {
                    return Err(format!("zone #{rang} has no name"));
                }
                if let Some(r) = obj.get("settings")
                    && !r.is_object()
                    && !r.is_null()
                {
                    return Err(format!("zone #{rang}: 'settings' must be an object"));
                }
                zones.push(obj.clone());
            }
        }
        Some(_) => return Err("'zones' must be an array".into()),
    }

    Ok(Fichier {
        format_version: version,
        settings,
        zones,
    })
}

// ── Plan ────────────────────────────────────────────────────────────

/// Ce qu'un import fait d'une entrée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Statut {
    Ajoute,
    Modifie,
    Inchange,
}

impl Statut {
    fn as_str(self) -> &'static str {
        match self {
            Statut::Ajoute => "added",
            Statut::Modifie => "modified",
            Statut::Inchange => "unchanged",
        }
    }
}

/// La zone d'arrivée d'une zone du fichier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cible {
    Existante(i64),
    /// À créer ; l'entier est son rang dans [`Plan::zones`].
    Nouvelle(usize),
}

#[derive(Debug, Clone)]
pub struct ZonePlanifiee {
    pub nom: String,
    pub output_device_id: Option<String>,
    pub statut: Statut,
    /// Créée hors ligne : son appareil n'a aucune zone sur cette machine.
    pub hors_ligne: bool,
    /// La zone d'arrivée existe mais est masquée (supprimée par
    /// l'utilisateur) : ses réglages sont posés, elle reste masquée.
    pub masquee: bool,
    /// Champs (et réglages de zone, sous leur gabarit) qui changent.
    pub changements: Vec<String>,
    cible: Cible,
    source_id: Option<i64>,
    donnees: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct ReglagePlanifie {
    pub cle: String,
    pub statut: Statut,
    valeur: Value,
}

/// Tout ce que l'import ferait, calculé sans rien écrire.
#[derive(Debug, Clone)]
pub struct Plan {
    pub format_version: u64,
    pub reglages: Vec<ReglagePlanifie>,
    pub zones: Vec<ZonePlanifiee>,
    pub avertissements: Vec<String>,
}

/// Deux valeurs de réglage sont-elles la même ? Les nombres se comparent en
/// nombres (`50` = `50.0`, `true` = `1`), le JSON en JSON (l'espacement d'un
/// objet rangé ne compte pas), le reste en texte.
fn meme_valeur(a: &Value, b: &Value) -> bool {
    fn nombre(v: &Value) -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    }
    fn deplier(v: &Value) -> Value {
        match v {
            Value::String(s) => match serde_json::from_str::<Value>(s) {
                Ok(p @ (Value::Object(_) | Value::Array(_))) => p,
                _ => v.clone(),
            },
            _ => v.clone(),
        }
    }
    if let (Some(x), Some(y)) = (nombre(a), nombre(b)) {
        return (x - y).abs() < 1e-9;
    }
    let (a, b) = (deplier(a), deplier(b));
    match (&a, &b) {
        (Value::Null, Value::Null) => true,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Bool(x), Value::String(y)) | (Value::String(y), Value::Bool(x)) => {
            y == if *x { "true" } else { "false" }
        }
        _ => a == b,
    }
}

/// La chaîne rangée en base pour une valeur du fichier (règle historique de
/// `import_config` : une chaîne telle quelle, tout le reste en JSON).
fn texte_a_ranger(valeur: &Value) -> String {
    match valeur {
        Value::String(s) => s.clone(),
        autre => autre.to_string(),
    }
}

/// Traduit un réglage qui cite des zones par leur numéro. `None` = ne pas
/// l'écrire (il ne désignerait aucune zone du fichier).
fn traduire_reglage(cle: &str, valeur: &Value, vers: &dyn Fn(i64) -> Option<i64>) -> Option<Value> {
    let id_de = |v: &Value| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    };
    match cle {
        "default_zone_id" => {
            let source = id_de(valeur)?;
            vers(source).map(|id| Value::String(id.to_string()))
        }
        "zone_groups" => {
            let groupes: Vec<Value> = match valeur {
                Value::Array(g) => g.clone(),
                Value::String(s) => serde_json::from_str(s).ok()?,
                _ => return None,
            };
            let mut sortie = Vec::with_capacity(groupes.len());
            for mut groupe in groupes {
                let ids: Vec<i64> = groupe
                    .get("zone_ids")
                    .and_then(Value::as_array)
                    .map(|l| l.iter().filter_map(id_de).filter_map(vers).collect())
                    .unwrap_or_default();
                let mut uniques: Vec<i64> = Vec::with_capacity(ids.len());
                for id in ids {
                    if !uniques.contains(&id) {
                        uniques.push(id);
                    }
                }
                // Un groupe réduit à une zone n'est plus un groupe.
                if uniques.len() < 2 {
                    continue;
                }
                let chef = groupe
                    .get("leader_id")
                    .and_then(id_de)
                    .and_then(vers)
                    .filter(|id| uniques.contains(id))
                    .unwrap_or(uniques[0]);
                groupe["zone_ids"] = json!(uniques);
                groupe["leader_id"] = json!(chef);
                sortie.push(groupe);
            }
            Some(Value::String(Value::Array(sortie).to_string()))
        }
        _ => Some(valeur.clone()),
    }
}

/// Les réglages ACTUELS d'une zone, au format de l'export (pour comparer).
fn zone_actuelle(
    backend: &Arc<dyn DbBackend>,
    reglages: &[(String, String)],
    id: i64,
    toutes: &[Value],
) -> Map<String, Value> {
    let repo = ZoneRepo::with_backend(backend.clone());
    let mut obj = toutes
        .iter()
        .find(|z| z.get("id").and_then(Value::as_i64) == Some(id))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    champs_lus_par_le_depot(&repo, id, &mut obj);
    let mut leurs = Map::new();
    for (cle, brut) in reglages {
        if let Some((zid, gabarit)) = gabarit_de_cle_de_zone(cle)
            && zid == id
        {
            leurs.insert(gabarit, valeur_exportee(brut));
        }
    }
    obj.insert("settings".into(), Value::Object(leurs));
    obj
}

/// Le volume du fichier est-il une préférence ? Pris zone ARMÉE, c'est
/// l'artefact du volume fixe (100 %), pas un réglage (même règle que
/// `config_backup::import_zones`).
fn volume_restaurable(zone: &Map<String, Value>) -> bool {
    !zone
        .get("fixed_volume")
        .is_some_and(|v| meme_valeur(v, &json!(1)))
}

/// Un gabarit de réglage de zone, une fois l'identifiant posé, doit rester une
/// clé de CETTE zone et ne rien porter de secret : un fichier ne peut pas
/// écrire n'importe quoi par ce chemin.
fn cle_de_zone_valide(gabarit: &str, id: i64) -> Option<String> {
    let cle = gabarit.replace("{id}", &id.to_string());
    match gabarit_de_cle_de_zone(&cle) {
        Some((zid, g)) if zid == id && g == gabarit && !est_secret(&cle) => Some(cle),
        _ => None,
    }
}

/// Calcule ce que l'import du fichier ferait, SANS RIEN ÉCRIRE.
pub fn planifier(backend: &Arc<dyn DbBackend>, fichier: &Fichier) -> Result<Plan, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let repo = ZoneRepo::with_backend(backend.clone());
    let reglages_actuels = settings.all()?;
    let actuels: HashMap<&str, &str> = reglages_actuels
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut avertissements = Vec::new();

    // ── Zones ──
    let mut zones: Vec<ZonePlanifiee> = Vec::new();
    if !fichier.zones.is_empty() {
        let visibles = repo.list()?;
        let ids_visibles: HashSet<i64> = visibles.iter().filter_map(|z| z.id).collect();
        let toutes = crate::config_backup::export_zones(backend)?;
        let mut appareils_vus: HashSet<String> = HashSet::new();
        let mut cibles_vues: HashSet<i64> = HashSet::new();

        for zone in &fichier.zones {
            let nom = zone
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let appareil = zone
                .get("output_device_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if let Some(a) = &appareil
                && !appareils_vus.insert(a.clone())
            {
                avertissements.push(format!(
                    "zone '{nom}': output device '{a}' appears twice in the file; skipped"
                ));
                continue;
            }

            // Rapprochement par APPAREIL. Une zone sans appareil n'a pas
            // d'identité stable : on ne la rapproche que d'une zone visible
            // sans appareil et de même nom, pour qu'un second import ne la
            // duplique pas.
            let existante = match &appareil {
                Some(a) => repo.get_by_device_id(a)?.and_then(|z| z.id),
                None => visibles
                    .iter()
                    .find(|z| z.output_device_id.is_none() && z.name == nom)
                    .and_then(|z| z.id),
            };
            if let Some(id) = existante
                && !cibles_vues.insert(id)
            {
                avertissements.push(format!(
                    "zone '{nom}' matches a zone already imported from this file; skipped"
                ));
                continue;
            }
            let source_id = zone.get("id").and_then(Value::as_i64);

            let (cible, statut, changements, masquee) = match existante {
                Some(id) => {
                    let actuelle = zone_actuelle(backend, &reglages_actuels, id, &toutes);
                    let mut changements = Vec::new();
                    for champ in CHAMPS_DE_ZONE {
                        let Some(v) = zone.get(*champ) else { continue };
                        if *champ == "volume" && !volume_restaurable(zone) {
                            continue;
                        }
                        if *champ == "output_type" && v.is_null() {
                            continue;
                        }
                        let a = actuelle.get(*champ).unwrap_or(&Value::Null);
                        if !meme_valeur(a, v) {
                            changements.push((*champ).to_string());
                        }
                    }
                    let vides = Map::new();
                    let leurs = actuelle
                        .get("settings")
                        .and_then(Value::as_object)
                        .unwrap_or(&vides);
                    if let Some(r) = zone.get("settings").and_then(Value::as_object) {
                        for (gabarit, v) in r {
                            if cle_de_zone_valide(gabarit, id).is_none() {
                                continue;
                            }
                            if !leurs.get(gabarit).is_some_and(|a| meme_valeur(a, v)) {
                                changements.push(gabarit.clone());
                            }
                        }
                    }
                    let statut = if changements.is_empty() {
                        Statut::Inchange
                    } else {
                        Statut::Modifie
                    };
                    (
                        Cible::Existante(id),
                        statut,
                        changements,
                        !ids_visibles.contains(&id),
                    )
                }
                None => (
                    Cible::Nouvelle(zones.len()),
                    Statut::Ajoute,
                    Vec::new(),
                    false,
                ),
            };

            zones.push(ZonePlanifiee {
                nom,
                output_device_id: appareil,
                statut,
                hors_ligne: matches!(cible, Cible::Nouvelle(_)),
                masquee,
                changements,
                cible,
                source_id,
                donnees: zone.clone(),
            });
        }
    }

    // Numéro d'arrivée PROVISOIRE d'une zone du fichier : celui de la zone
    // existante, ou un négatif pour une zone à créer — qui ne peut égaler
    // aucune valeur en base, ce qui est exactement vrai.
    let provisoire = |source: i64| -> Option<i64> {
        zones
            .iter()
            .find(|z| z.source_id == Some(source))
            .map(|z| match z.cible {
                Cible::Existante(id) => id,
                Cible::Nouvelle(rang) => -(rang as i64) - 1,
            })
    };

    // ── Réglages ──
    let mut reglages = Vec::new();
    let mut ignores_de_zone = 0usize;
    for (cle, brute) in &fichier.settings {
        let valeur = if fichier.format_version >= 2 {
            // Dans le nouveau format, un réglage de zone voyage dans sa zone.
            // Un `zone_7_…` resté dans `settings` désigne un numéro de la
            // machine de départ : l'écrire ici l'accrocherait à une autre zone.
            if gabarit_de_cle_de_zone(cle).is_some() {
                ignores_de_zone += 1;
                continue;
            }
            match traduire_reglage(cle, brute, &provisoire) {
                Some(v) => v,
                None => {
                    avertissements.push(format!(
                        "setting '{cle}' refers to a zone that is not in the file; skipped"
                    ));
                    continue;
                }
            }
        } else {
            brute.clone()
        };
        let statut = match actuels.get(cle.as_str()) {
            None => Statut::Ajoute,
            Some(brut) if meme_valeur(&Value::String((*brut).to_string()), &valeur) => {
                Statut::Inchange
            }
            Some(_) => Statut::Modifie,
        };
        reglages.push(ReglagePlanifie {
            cle: cle.clone(),
            statut,
            // On garde la valeur NON traduite : la traduction définitive se
            // fait à l'application, une fois les zones créées.
            valeur: brute.clone(),
        });
    }
    if ignores_de_zone > 0 {
        avertissements.push(format!(
            "{ignores_de_zone} zone setting(s) outside their zone were skipped"
        ));
    }

    Ok(Plan {
        format_version: fichier.format_version,
        reglages,
        zones,
        avertissements,
    })
}

// ── Application ─────────────────────────────────────────────────────

fn en_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_f64().map(|x| x != 0.0),
        Value::String(s) => match s.trim() {
            "1" | "true" => Some(true),
            "0" | "false" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn en_entier(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f.round() as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Écrit UN champ de zone. `Ok(false)` = valeur illisible, ignorée.
fn ecrire_champ(repo: &ZoneRepo, id: i64, champ: &str, v: &Value) -> Result<bool, String> {
    macro_rules! drapeau {
        ($m:ident) => {{
            let Some(b) = en_bool(v) else {
                return Ok(false);
            };
            repo.$m(id, b)?;
        }};
    }
    match champ {
        "name" => {
            let Some(n) = v.as_str().filter(|n| !n.trim().is_empty()) else {
                return Ok(false);
            };
            repo.update_name(id, n)?;
        }
        "output_type" => {
            let Some(t) = v.as_str() else {
                return Ok(false);
            };
            repo.update_output_type(id, t)?;
        }
        "volume" => {
            let Some(x) = v
                .as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            else {
                return Ok(false);
            };
            repo.update_volume(id, x.clamp(0.0, 100.0))?;
        }
        "muted" => drapeau!(update_muted),
        "gapless_enabled" => drapeau!(update_gapless_enabled),
        "dlna_native_flac" => drapeau!(update_dlna_native_flac),
        "alac_passthrough" => drapeau!(update_alac_passthrough),
        "aac_passthrough" => drapeau!(update_aac_passthrough),
        "dlna_lpcm" => drapeau!(update_dlna_lpcm),
        "dlna_cap_16bit" => drapeau!(update_dlna_cap_16bit),
        "dlna_wav24" => drapeau!(update_dlna_wav24),
        "group_id" => repo.update_group(id, v.as_str())?,
        "sync_delay_ms" => {
            let Some(ms) = en_entier(v).and_then(|n| i32::try_from(n).ok()) else {
                return Ok(false);
            };
            repo.update_sync_delay(id, ms)?;
        }
        "max_sample_rate" => {
            let taux = if v.is_null() {
                None
            } else {
                match en_entier(v).and_then(|n| u32::try_from(n).ok()) {
                    Some(t) => Some(t),
                    None => return Ok(false),
                }
            };
            repo.update_max_sample_rate(id, taux)?;
        }
        "autoplay_mode" => {
            let Some(mode) = v.as_str().and_then(AutoplayMode::from_str_stocke) else {
                return Ok(false);
            };
            repo.update_autoplay_mode(id, mode)?;
        }
        "dsd_mode" => {
            let Some(m) = v.as_str().filter(|m| !m.trim().is_empty()) else {
                return Ok(false);
            };
            repo.update_dsd_mode(id, m)?;
        }
        "dlna_play_delay_ms" => {
            let Some(ms) = en_entier(v).and_then(|n| u64::try_from(n).ok()) else {
                return Ok(false);
            };
            repo.update_dlna_play_delay_ms(id, ms)?;
        }
        "lyrics_offset_ms" => {
            let Some(ms) = en_entier(v).and_then(|n| i32::try_from(n).ok()) else {
                return Ok(false);
            };
            repo.update_lyrics_offset_ms(id, ms)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Ce que l'application a fait.
#[derive(Debug, Clone, Default)]
pub struct Bilan {
    pub reglages_ecrits: usize,
    /// Zones créées ou modifiées, avec leur numéro d'arrivée.
    pub zones_creees: Vec<i64>,
    pub zones_modifiees: Vec<i64>,
    pub avertissements: Vec<String>,
}

/// Exécute le plan. Les zones d'abord (il faut leur numéro pour traduire
/// `default_zone_id` et `zone_groups`), puis les réglages.
///
/// Une écriture qui échoue arrête tout et le DIT, avec ce qui était déjà fait
/// — même contrat que l'ancien `import_config`.
pub fn appliquer(backend: &Arc<dyn DbBackend>, plan: &Plan) -> Result<Bilan, String> {
    let repo = ZoneRepo::with_backend(backend.clone());
    let settings = SettingsRepo::with_backend(backend.clone());
    let mut bilan = Bilan {
        avertissements: plan.avertissements.clone(),
        ..Default::default()
    };
    let mut arrivee: HashMap<usize, i64> = HashMap::new();
    let etape = |bilan: &Bilan, e: String| {
        format!(
            "import stopped after {} settings and {} zones: {e}",
            bilan.reglages_ecrits,
            bilan.zones_creees.len() + bilan.zones_modifiees.len()
        )
    };

    for (rang, zone) in plan.zones.iter().enumerate() {
        let (id, nouvelle) = match zone.cible {
            Cible::Existante(id) => {
                arrivee.insert(rang, id);
                if zone.statut == Statut::Inchange {
                    continue;
                }
                (id, false)
            }
            Cible::Nouvelle(_) => {
                let type_sortie = zone.donnees.get("output_type").and_then(Value::as_str);
                let id = repo
                    .create(&zone.nom, type_sortie, zone.output_device_id.as_deref())
                    .map_err(|e| etape(&bilan, e))?;
                // Hors ligne tant que la découverte n'a pas vu l'appareil.
                repo.update_online(id, false)
                    .map_err(|e| etape(&bilan, e))?;
                arrivee.insert(rang, id);
                (id, true)
            }
        };

        for champ in CHAMPS_DE_ZONE {
            if !nouvelle && !zone.changements.iter().any(|c| c == champ) {
                continue;
            }
            let Some(v) = zone.donnees.get(*champ) else {
                continue;
            };
            if *champ == "volume" && !volume_restaurable(&zone.donnees) {
                continue;
            }
            if *champ == "output_type" && v.is_null() {
                continue;
            }
            if !ecrire_champ(&repo, id, champ, v).map_err(|e| etape(&bilan, e))? {
                bilan.avertissements.push(format!(
                    "zone '{}': value of '{champ}' is not readable; kept as is",
                    zone.nom
                ));
            }
        }
        if let Some(r) = zone.donnees.get("settings").and_then(Value::as_object) {
            for (gabarit, v) in r {
                if !nouvelle && !zone.changements.iter().any(|c| c == gabarit) {
                    continue;
                }
                let Some(cle) = cle_de_zone_valide(gabarit, id) else {
                    bilan.avertissements.push(format!(
                        "zone '{}': setting '{gabarit}' is not a zone setting; skipped",
                        zone.nom
                    ));
                    continue;
                };
                settings
                    .set(&cle, &texte_a_ranger(v))
                    .map_err(|e| etape(&bilan, e))?;
            }
        }
        if nouvelle {
            bilan.zones_creees.push(id);
        } else {
            bilan.zones_modifiees.push(id);
        }
    }

    let definitif = |source: i64| -> Option<i64> {
        plan.zones
            .iter()
            .enumerate()
            .find(|(_, z)| z.source_id == Some(source))
            .and_then(|(rang, _)| arrivee.get(&rang).copied())
    };

    for reglage in &plan.reglages {
        if reglage.statut == Statut::Inchange {
            continue;
        }
        let valeur = if plan.format_version >= 2 {
            match traduire_reglage(&reglage.cle, &reglage.valeur, &definitif) {
                Some(v) => v,
                None => continue,
            }
        } else {
            reglage.valeur.clone()
        };
        settings
            .set(&reglage.cle, &texte_a_ranger(&valeur))
            .map_err(|e| etape(&bilan, format!("writing '{}' failed: {e}", reglage.cle)))?;
        bilan.reglages_ecrits += 1;
    }

    Ok(bilan)
}

/// L'aperçu, tel que la route le rend et que l'écran l'affiche.
pub fn rapport(plan: &Plan) -> Value {
    let cles = |s: Statut| -> Vec<&str> {
        plan.reglages
            .iter()
            .filter(|r| r.statut == s)
            .map(|r| r.cle.as_str())
            .collect()
    };
    let zones: Vec<Value> = plan
        .zones
        .iter()
        .map(|z| {
            json!({
                "name": z.nom,
                "output_device_id": z.output_device_id,
                "status": z.statut.as_str(),
                "offline": z.hors_ligne,
                "hidden": z.masquee,
                "changes": z.changements,
            })
        })
        .collect();
    json!({
        "format_version": plan.format_version,
        "settings": {
            "added": cles(Statut::Ajoute),
            "modified": cles(Statut::Modifie),
            "unchanged": cles(Statut::Inchange),
        },
        "zones": zones,
        "warnings": plan.avertissements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meme_valeur_compare_les_nombres_en_nombres() {
        assert!(meme_valeur(&json!(50), &json!(50.0)));
        assert!(meme_valeur(&json!(true), &json!(1)));
        assert!(meme_valeur(&json!("0"), &json!(false)));
        assert!(meme_valeur(&json!("{\"a\": 1}"), &json!({"a": 1})));
        assert!(!meme_valeur(&json!("clair"), &json!("sombre")));
        assert!(!meme_valeur(&Value::Null, &json!("x")));
    }

    #[test]
    fn un_gabarit_ne_peut_viser_que_sa_zone() {
        assert_eq!(
            cle_de_zone_valide("zone_{id}_crossfeed", 12).as_deref(),
            Some("zone_12_crossfeed")
        );
        assert_eq!(
            cle_de_zone_valide("dac_profile_{id}", 4).as_deref(),
            Some("dac_profile_4")
        );
        // Pas un réglage de zone : refusé, même posé sous un gabarit.
        assert!(cle_de_zone_valide("jwt_secret", 4).is_none());
        assert!(cle_de_zone_valide("auth_enabled", 4).is_none());
        // Une clé de zone qui porterait un secret ne passe pas non plus.
        assert!(cle_de_zone_valide("zone_{id}_token", 4).is_none());
    }

    #[test]
    fn les_groupes_suivent_les_zones_et_se_defont_sous_deux() {
        let vers = |s: i64| match s {
            1 => Some(10),
            2 => Some(20),
            _ => None,
        };
        let v = traduire_reglage(
            "zone_groups",
            &json!([
                {"id": 1, "zone_ids": [1, 2], "leader_id": 2},
                {"id": 2, "zone_ids": [1, 3], "leader_id": 3}
            ]),
            &vers,
        )
        .unwrap();
        let groupes: Vec<Value> = serde_json::from_str(v.as_str().unwrap()).unwrap();
        assert_eq!(groupes.len(), 1);
        assert_eq!(groupes[0]["zone_ids"], json!([10, 20]));
        assert_eq!(groupes[0]["leader_id"], json!(20));
        assert_eq!(
            traduire_reglage("default_zone_id", &json!("2"), &vers),
            Some(json!("20"))
        );
        assert_eq!(traduire_reglage("default_zone_id", &json!(9), &vers), None);
    }
}
