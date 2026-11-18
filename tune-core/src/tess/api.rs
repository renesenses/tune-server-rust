//! Les corps JSON de l'API TESS et leur schéma.
//!
//! - `GET /zones/{id}/tess` → [`ReponseTessZone`] ;
//! - `PUT /zones/{id}/tess` ← corps lu par [`ChaineTess::depuis_corps`]
//!   (strict), → [`ReponseTessZone`] ou [`RefusTess`] (400 corps illisible,
//!   402 droit manquant, 409 chaîne invalide).
//!
//! Les routes elles-mêmes sont le lot T08. Un serveur sans TESS répond 404 sur
//! `GET /zones/{id}/tess` : c'est ce qui garde l'écran TESS caché côté web.
//!
//! **Règle de compatibilité** : un client IGNORE les champs qu'il ne connaît
//! pas et lit toute valeur d'énumération inconnue comme `unknown`. Les types
//! Rust de réponse le font déjà ; [`schema_json_lecture`] est le schéma qu'un
//! client doit appliquer. [`schema_json`] est le schéma exact de ce que CE
//! serveur émet et accepte : il sert aux tests du serveur et au corps strict
//! du `PUT`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::VERSION_CONTRAT;
use super::chaine::{ChaineTess, ErreurChaine};
use super::greffon::Catalogue;
use super::resolution::{Badge, ContexteExecution, EtapeResolue, EtatVolume};
use super::zones::{Applicabilite, TransportZone};

/// La réponse de `GET /zones/{id}/tess` (et d'un `PUT` accepté).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReponseTessZone {
    pub contract_version: u32,
    pub zone_id: i64,
    pub transport: TransportZone,
    pub applicability: Applicabilite,
    pub chain: ChaineTess,
    /// Le préréglage chargé sur CETTE zone (« ACTIVE »), s'il y en a un.
    pub active_preset: Option<String>,
    /// Headroom réellement appliqué, en dB (0 hors TESS ou sans traitement).
    pub headroom_db: f64,
    pub volume: EtatVolume,
    pub badge: Badge,
    /// L'ordre réel (Q9), source et sortie comprises, demandé et effectif.
    pub signal_path: Vec<EtapeResolue>,
    /// Les greffons qu'on peut ajouter (« + Add plugin »), avec leurs droits.
    pub catalog: Catalogue,
}

impl ReponseTessZone {
    pub fn depuis(
        zone_id: i64,
        chain: ChaineTess,
        active_preset: Option<String>,
        catalog: Catalogue,
        ctx: &ContexteExecution<'_>,
    ) -> Self {
        let r = chain.resoudre(&catalog, ctx);
        Self {
            contract_version: VERSION_CONTRAT,
            zone_id,
            transport: ctx.transport,
            applicability: ctx.transport.applicabilite(),
            chain,
            active_preset,
            headroom_db: r.headroom_db,
            volume: r.volume,
            badge: r.badge,
            signal_path: r.etapes,
            catalog,
        }
    }
}

/// Le corps d'un refus 400, 402 ou 409.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusTess {
    pub status: u16,
    pub code: String,
    pub detail: String,
    pub plugin_id: Option<String>,
}

impl From<&ErreurChaine> for RefusTess {
    fn from(e: &ErreurChaine) -> Self {
        Self {
            status: e.statut_http(),
            code: e.code().to_owned(),
            detail: e.to_string(),
            plugin_id: e.greffon().map(str::to_owned),
        }
    }
}

fn enumere(valeurs: &[&str]) -> Value {
    json!({ "enum": valeurs })
}

/// Le schéma JSON (draft 2020-12) EXACT de ce que ce serveur émet et accepte.
/// Définitions utiles : `chain` (corps du PUT), `zone_response` (GET),
/// `refusal`, `catalog`, `presets`.
pub fn schema_json() -> Value {
    let texte_ou_null = json!({ "type": ["string", "null"] });
    let entier_ou_null = json!({ "type": ["integer", "null"] });
    let objet_ou_null = json!({ "type": ["object", "null"] });
    let filtre = json!({ "type": "string" });
    let taux_dsd = enumere(&["dsd64", "dsd128", "dsd256"]);
    let codes_refus: Vec<&str> = ErreurChaine::exemples()
        .iter()
        .map(ErreurChaine::code)
        .collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://mozaiklabs.fr/schemas/tess/contract-1.json",
        "title": "TESS API contract",
        "x-contract-version": VERSION_CONTRAT,
        "$defs": {
            "id": { "type": "string", "pattern": "^[a-z0-9]([a-z0-9-]*[a-z0-9])?$", "maxLength": 64 },
            "plugin_link": {
                "type": "object", "required": ["id", "enabled"],
                "properties": {
                    "id": { "$ref": "#/$defs/id" },
                    "enabled": { "type": "boolean" },
                    "settings": objet_ou_null
                },
                "additionalProperties": false
            },
            "headroom": { "oneOf": [
                { "type": "object", "required": ["mode"],
                  "properties": { "mode": { "const": "auto" } }, "additionalProperties": false },
                { "type": "object", "required": ["mode", "db"],
                  "properties": { "mode": { "const": "manual" },
                                  "db": { "type": "number", "minimum": -24.0, "maximum": 0.0 } },
                  "additionalProperties": false }
            ] },
            "upsampling": { "oneOf": [
                { "type": "object", "required": ["mode"],
                  "properties": { "mode": { "const": "off" } }, "additionalProperties": false },
                { "type": "object", "required": ["mode"],
                  "properties": { "mode": { "const": "pcm" }, "filter": filtre,
                                  "max_ratio": { "enum": [2, 4, 8, 16, null] },
                                  "max_rate_hz": { "type": ["integer", "null"], "minimum": 1, "maximum": 768000 } },
                  "additionalProperties": false },
                { "type": "object", "required": ["mode", "rate"],
                  "properties": { "mode": { "const": "dsd" }, "rate": taux_dsd, "filter": filtre },
                  "additionalProperties": false }
            ] },
            "quantization": {
                "type": "object",
                "properties": { "dither": enumere(&["tpdf", "noise_shaped", "off"]) },
                "additionalProperties": false
            },
            "fixed_stages": {
                "type": "object",
                "properties": {
                    "headroom": { "$ref": "#/$defs/headroom" },
                    "upsampling": { "$ref": "#/$defs/upsampling" },
                    "quantization": { "$ref": "#/$defs/quantization" }
                },
                "additionalProperties": false
            },
            "chain": {
                "type": "object", "required": ["version"],
                "properties": {
                    "version": { "type": "integer", "minimum": 1, "maximum": super::VERSION_CHAINE },
                    "bypass_all": { "type": "boolean" },
                    "plugins": { "type": "array", "items": { "$ref": "#/$defs/plugin_link" } },
                    "fixed": { "$ref": "#/$defs/fixed_stages" }
                },
                "additionalProperties": false
            },
            "exclusive_group": { "oneOf": [
                { "type": "null" },
                { "type": "object", "required": ["id", "priority"],
                  "properties": { "id": { "type": "string" }, "priority": { "type": "integer", "minimum": 0, "maximum": 255 } },
                  "additionalProperties": false }
            ] },
            "placement": {
                "type": "object", "required": ["channels", "position", "exclusive_group"],
                "properties": {
                    "channels": enumere(&["any", "stereo_only", "remaps"]),
                    "position": enumere(&["free", "last_plugin"]),
                    "exclusive_group": { "$ref": "#/$defs/exclusive_group" }
                },
                "additionalProperties": false
            },
            "plugin_descriptor": {
                "type": "object",
                "required": ["id", "name", "category", "entitlement", "feature", "origin", "settings_schema", "placement"],
                "properties": {
                    "id": { "$ref": "#/$defs/id" },
                    "name": { "type": "string" },
                    "category": enumere(&["equalisation", "spatial", "room_and_filters", "restoration_and_routing"]),
                    "entitlement": enumere(&["free", "premium"]),
                    "feature": texte_ou_null,
                    "origin": enumere(&["reference", "package"]),
                    "settings_schema": { "type": "object" },
                    "placement": { "$ref": "#/$defs/placement" }
                },
                "additionalProperties": false
            },
            "catalog": {
                "type": "object", "required": ["plugins"],
                "properties": {
                    "plugins": { "type": "array", "items": { "$ref": "#/$defs/plugin_descriptor" } },
                    "groups": { "type": "array", "items": {
                        "type": "object", "required": ["id", "policy"],
                        "properties": { "id": { "type": "string" }, "policy": enumere(&["priority", "refuse"]) },
                        "additionalProperties": false
                    } }
                },
                "additionalProperties": false
            },
            "transport": enumere(&["local", "oaat", "dlna", "openhome", "bluos", "squeezebox", "chromecast",
                                   "airplay", "devialet_air", "browser", "aes67", "sendspin", "hqplayer",
                                   "diretta", "other"]),
            "applicability": { "oneOf": [
                { "type": "object", "required": ["status"],
                  "properties": { "status": { "const": "applied" } }, "additionalProperties": false },
                { "type": "object", "required": ["status", "reason", "detail"],
                  "properties": { "status": { "const": "outside_tess" },
                                  "reason": enumere(&["hqplayer_own_processing", "diretta_outside_tess", "unclassified_transport"]),
                                  "detail": { "type": "string" } },
                  "additionalProperties": false }
            ] },
            "step": {
                "type": "object",
                "required": ["code", "plugin_id", "fixed", "requested", "effective", "reason", "detail",
                             "params", "latency_frames"],
                "properties": {
                    "code": enumere(&["source", "headroom", "plugin", "upsampling", "volume",
                                      "quantization", "dsd_modulator", "output"]),
                    "plugin_id": texte_ou_null,
                    "fixed": { "type": "boolean" },
                    "requested": { "type": "boolean" },
                    "effective": { "type": "boolean" },
                    "reason": { "enum": ["outside_tess", "pure_mode", "bypass_all", "dsd_source_native",
                                         "chain_version_unsupported", "unknown_value", "unknown_plugin",
                                         "plugin_unavailable", "premium_required", "stereo_only",
                                         "replaced_in_group", "nothing_to_protect", "volume_not_software",
                                         "nothing_to_quantize", null] },
                    "detail": texte_ou_null,
                    "params": objet_ou_null,
                    "latency_frames": entier_ou_null
                },
                "additionalProperties": false
            },
            "volume": {
                "type": "object", "required": ["control", "placement", "db"],
                "properties": {
                    "control": enumere(&["software", "device", "fixed"]),
                    "placement": { "enum": ["at_render", "before_dsd_modulator", null] },
                    "db": { "type": ["number", "null"] }
                },
                "additionalProperties": false
            },
            "zone_response": {
                "type": "object",
                "required": ["contract_version", "zone_id", "transport", "applicability", "chain",
                             "active_preset", "headroom_db", "volume", "badge", "signal_path", "catalog"],
                "properties": {
                    "contract_version": { "const": VERSION_CONTRAT },
                    "zone_id": { "type": "integer" },
                    "transport": { "$ref": "#/$defs/transport" },
                    "applicability": { "$ref": "#/$defs/applicability" },
                    "chain": { "$ref": "#/$defs/chain" },
                    "active_preset": texte_ou_null,
                    "headroom_db": { "type": "number" },
                    "volume": { "$ref": "#/$defs/volume" },
                    "badge": enumere(&["bit_perfect", "processed_by_tess", "outside_tess"]),
                    "signal_path": { "type": "array", "items": { "$ref": "#/$defs/step" } },
                    "catalog": { "$ref": "#/$defs/catalog" }
                },
                "additionalProperties": false
            },
            "preset": {
                "type": "object", "required": ["id", "name", "chain"],
                "properties": {
                    "id": { "$ref": "#/$defs/id" },
                    "name": { "type": "string" },
                    "builtin": { "type": "boolean" },
                    "chain": { "$ref": "#/$defs/chain" }
                },
                "additionalProperties": false
            },
            "presets": {
                "type": "object", "required": ["presets"],
                "properties": {
                    "apply_mode": enumere(&["immediately", "at_next_playback"]),
                    "presets": { "type": "array", "items": { "$ref": "#/$defs/preset" } }
                },
                "additionalProperties": false
            },
            "refusal": {
                "type": "object", "required": ["status", "code", "detail", "plugin_id"],
                "properties": {
                    "status": { "enum": [400, 402, 409] },
                    "code": { "enum": codes_refus },
                    "detail": { "type": "string" },
                    "plugin_id": texte_ou_null
                },
                "additionalProperties": false
            }
        }
    })
}

/// Ouvre un schéma pour la LECTURE par un client : champs inconnus permis,
/// valeurs d'énumération inconnues permises, variantes inconnues permises,
/// bornes levées. Seul `contract_version` reste fixé : il ne monte que quand un
/// champ change de SENS, et un client doit alors le savoir.
fn ouvrir(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut sortie = Map::new();
            for (k, x) in o {
                match k.as_str() {
                    "additionalProperties" if *x == Value::Bool(false) => {}
                    // Une liste fermée devient « une chaîne, un nombre ou null »,
                    // valeurs connues gardées pour la documentation.
                    "enum" => {
                        sortie.insert("x-known-values".into(), x.clone());
                    }
                    "oneOf" => {
                        let mut variantes: Vec<Value> =
                            x.as_array().into_iter().flatten().map(ouvrir).collect();
                        variantes.push(json!({ "type": "object" }));
                        sortie.insert("anyOf".into(), Value::Array(variantes));
                    }
                    // Les bornes servent à l'écriture ; une version plus récente
                    // peut les élargir (version de chaîne, plafonds).
                    "minimum" | "maximum" => {}
                    _ => {
                        sortie.insert(k.clone(), ouvrir(x));
                    }
                }
            }
            Value::Object(sortie)
        }
        Value::Array(a) => Value::Array(a.iter().map(ouvrir).collect()),
        autre => autre.clone(),
    }
}

/// Le schéma qu'un CLIENT applique aux réponses : celui de [`schema_json`],
/// ouvert (champs, valeurs et variantes inconnus permis). Un serveur plus
/// récent qui ajoute un champ facultatif ou une valeur reste lisible.
pub fn schema_json_lecture() -> Value {
    ouvrir(&schema_json())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tess::chaine::{MaillonGreffon, TauxDsd, Upsampling};
    use crate::tess::greffon::catalogue_de_reference;
    use crate::tess::prereglages::PreReglages;
    use crate::tess::resolution::{ControleVolume, SourceFlux, VolumeZone};
    use crate::tess::{ContexteValidation, VERSION_CHAINE};

    /// Un vérificateur de schéma JSON minimal, limité aux mots-clés que
    /// [`schema_json`] emploie (`pattern` et `maxLength` exceptés : l'id est
    /// vérifié par `id_valide`).
    fn conforme(racine: &Value, schema: &Value, v: &Value) -> Result<(), String> {
        if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
            let nom = r.strip_prefix("#/$defs/").ok_or("ref")?;
            return conforme(racine, &racine["$defs"][nom], v);
        }
        if let Some(t) = schema.get("type") {
            let types: Vec<&str> = match t {
                Value::String(s) => vec![s.as_str()],
                Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                _ => return Err("type".into()),
            };
            let ok = types.iter().any(|t| match *t {
                "object" => v.is_object(),
                "array" => v.is_array(),
                "string" => v.is_string(),
                "boolean" => v.is_boolean(),
                "null" => v.is_null(),
                "number" => v.is_number(),
                "integer" => v.is_i64() || v.is_u64(),
                _ => false,
            });
            if !ok {
                return Err(format!("{v} n'est pas de type {t}"));
            }
        }
        if let Some(c) = schema.get("const")
            && c != v
        {
            return Err(format!("{v} ≠ {c}"));
        }
        if let Some(e) = schema.get("enum").and_then(Value::as_array)
            && !e.contains(v)
        {
            return Err(format!("{v} hors de {e:?}"));
        }
        if let Some(n) = v.as_f64() {
            if schema
                .get("minimum")
                .and_then(Value::as_f64)
                .is_some_and(|m| n < m)
            {
                return Err(format!("{n} sous le minimum"));
            }
            if schema
                .get("maximum")
                .and_then(Value::as_f64)
                .is_some_and(|m| n > m)
            {
                return Err(format!("{n} au-dessus du maximum"));
            }
        }
        if let Some(variantes) = schema.get("oneOf").and_then(Value::as_array) {
            let n = variantes
                .iter()
                .filter(|s| conforme(racine, s, v).is_ok())
                .count();
            if n != 1 {
                return Err(format!("{v} : {n} variantes conformes au lieu d'une"));
            }
        }
        if let Some(variantes) = schema.get("anyOf").and_then(Value::as_array)
            && !variantes.iter().any(|s| conforme(racine, s, v).is_ok())
        {
            return Err(format!("{v} : aucune variante conforme"));
        }
        if let Some(o) = v.as_object() {
            if let Some(req) = schema.get("required").and_then(Value::as_array) {
                for k in req.iter().filter_map(Value::as_str) {
                    if !o.contains_key(k) {
                        return Err(format!("champ {k} manquant"));
                    }
                }
            }
            let props = schema.get("properties").and_then(Value::as_object);
            for (k, val) in o {
                match props.and_then(|p| p.get(k)) {
                    Some(s) => conforme(racine, s, val).map_err(|e| format!("{k}: {e}"))?,
                    None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                        return Err(format!("champ inconnu {k}"));
                    }
                    None => {}
                }
            }
        }
        if let (Some(a), Some(items)) = (v.as_array(), schema.get("items")) {
            for (i, x) in a.iter().enumerate() {
                conforme(racine, items, x).map_err(|e| format!("[{i}]: {e}"))?;
            }
        }
        Ok(())
    }

    fn strict(def: &str, v: &Value) -> Result<(), String> {
        let s = schema_json();
        conforme(&s, &s["$defs"][def], v)
    }

    fn lecture(def: &str, v: &Value) -> Result<(), String> {
        let s = schema_json_lecture();
        conforme(&s, &s["$defs"][def], v)
    }

    fn tout(_: &str) -> bool {
        true
    }

    fn ctx(transport: TransportZone) -> ContexteExecution<'static> {
        ContexteExecution {
            transport,
            source: Some(SourceFlux::Pcm {
                sample_rate_hz: 96_000,
                bits: 24,
                channels: 2,
            }),
            premium: true,
            greffon_disponible: &tout,
            pure: false,
            volume: VolumeZone {
                control: ControleVolume::Software,
                db: -18.0,
            },
        }
    }

    fn chaine_complete() -> ChaineTess {
        let mut c = ChaineTess {
            version: VERSION_CHAINE,
            bypass_all: false,
            plugins: vec![
                MaillonGreffon {
                    id: "convolution".into(),
                    enabled: true,
                    settings: json!({ "filter_set": "living-room.zip" }),
                },
                MaillonGreffon {
                    id: "equalizer-graphic".into(),
                    enabled: true,
                    settings: Value::Null,
                },
                MaillonGreffon {
                    id: "crossfeed".into(),
                    enabled: false,
                    settings: Value::Null,
                },
            ],
            ..ChaineTess::default()
        };
        c.fixed.upsampling = Upsampling::Dsd {
            rate: TauxDsd::Dsd128,
            filter: "polyvalent".into(),
        };
        c
    }

    fn reponse(t: TransportZone, c: ChaineTess) -> ReponseTessZone {
        ReponseTessZone::depuis(
            12,
            c,
            Some("evening-listening".into()),
            catalogue_de_reference(),
            &ctx(t),
        )
    }

    #[test]
    fn la_reponse_d_une_zone_fait_l_aller_retour_et_suit_le_schema() {
        for t in TransportZone::ALL {
            for c in [ChaineTess::default(), chaine_complete()] {
                let r = reponse(t, c);
                let v = serde_json::to_value(&r).unwrap();
                strict("zone_response", &v).unwrap_or_else(|e| panic!("{t:?} : {e}"));
                lecture("zone_response", &v).unwrap_or_else(|e| panic!("{t:?} : {e}"));
                assert_eq!(serde_json::from_value::<ReponseTessZone>(v).unwrap(), r);
            }
        }
    }

    #[test]
    fn un_client_v1_lit_une_reponse_plus_recente() {
        let mut v = serde_json::to_value(reponse(TransportZone::Oaat, chaine_complete())).unwrap();
        v["latency_total_frames"] = json!(512);
        v["transport"] = json!("snapcast");
        v["badge"] = json!("processed_by_tess_and_room");
        v["volume"]["placement"] = json!("in_dac");
        v["signal_path"][1]["code"] = json!("limiter");
        v["signal_path"][1]["thermal"] = json!(true);
        v["applicability"] = json!({ "status": "partial", "since": 2 });
        v["chain"]["fixed"]["upsampling"] = json!({ "mode": "oversample_x3", "taps": 1024 });
        v["chain"]["version"] = json!(VERSION_CHAINE + 1);
        v["catalog"]["plugins"][0]["category"] = json!("dynamics");
        // Le schéma de lecture et les types Rust l'acceptent…
        lecture("zone_response", &v).unwrap();
        let r: ReponseTessZone = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(r.transport, TransportZone::Other);
        assert_eq!(r.badge, Badge::Unknown);
        assert_eq!(r.applicability, Applicabilite::Unknown);
        assert_eq!(r.chain.fixed.upsampling, Upsampling::Unknown);
        // … et le schéma strict du serveur ne l'aurait pas émis (contre-épreuve).
        assert!(strict("zone_response", &v).is_err());
    }

    #[test]
    fn hors_tess_la_reponse_n_annonce_ni_headroom_ni_ordre_dsd() {
        let r = reponse(TransportZone::HqPlayer, chaine_complete());
        assert_eq!(r.headroom_db, 0.0);
        assert_eq!(r.volume.placement, None);
        assert_eq!(r.badge, Badge::OutsideTess);
        // Contre-épreuve sur OAAT.
        let r = reponse(TransportZone::Oaat, chaine_complete());
        assert_eq!(r.headroom_db, -3.0);
        assert!(r.volume.placement.is_some());
    }

    #[test]
    fn chaines_catalogue_presets_et_refus_suivent_le_schema() {
        strict("chain", &serde_json::to_value(chaine_complete()).unwrap()).unwrap();
        strict("chain", &json!({ "version": 1 })).unwrap();
        strict(
            "catalog",
            &serde_json::to_value(catalogue_de_reference()).unwrap(),
        )
        .unwrap();
        strict(
            "presets",
            &serde_json::to_value(PreReglages::default()).unwrap(),
        )
        .unwrap();
        let e = chaine_complete()
            .valider(
                &catalogue_de_reference(),
                ContexteValidation { premium: false },
            )
            .unwrap_err();
        let refus = RefusTess::from(&e);
        assert_eq!(refus.status, 402);
        assert_eq!(refus.plugin_id.as_deref(), Some("convolution"));
        let v = serde_json::to_value(&refus).unwrap();
        strict("refusal", &v).unwrap();
        assert_eq!(serde_json::from_value::<RefusTess>(v).unwrap(), refus);
        // Corps illisible : 400 invalid_body.
        let e = ChaineTess::depuis_corps("{").unwrap_err();
        let refus = RefusTess::from(&e);
        assert_eq!((refus.status, refus.code.as_str()), (400, "invalid_body"));
        strict("refusal", &serde_json::to_value(&refus).unwrap()).unwrap();
    }

    #[test]
    fn le_schema_strict_refuse_les_corps_invalides() {
        for doc in [
            json!({}),
            json!({ "version": 0 }),
            json!({ "version": 1, "volume": -18 }),
            json!({ "version": 1, "plugins": [{ "id": 3, "enabled": true }] }),
            json!({ "version": 1, "plugins": [{ "id": "crossfeed" }] }),
            json!({ "version": 1, "fixed": { "headroom": { "mode": "manual", "db": 3 } } }),
            json!({ "version": 1, "fixed": { "headroom": { "mode": "auto", "db": -3 } } }),
            json!({ "version": 1, "fixed": { "upsampling": { "mode": "pcm", "max_ratio": 3 } } }),
            json!({ "version": 1, "fixed": { "upsampling": { "mode": "pcm", "factor": 2 } } }),
            json!({ "version": 1, "fixed": { "upsampling": { "mode": "dsd", "rate": "dsd512" } } }),
        ] {
            assert!(strict("chain", &doc).is_err(), "{doc}");
        }
    }

    #[test]
    fn le_schema_couvre_chaque_code_de_refus() {
        // `exemples()` couvre chaque variante (test de `chaine`) ; le schéma en
        // est tiré, et chaque refus construit s'y conforme.
        for e in ErreurChaine::exemples() {
            strict(
                "refusal",
                &serde_json::to_value(RefusTess::from(&e)).unwrap(),
            )
            .unwrap_or_else(|x| panic!("{} : {x}", e.code()));
        }
        // Contre-épreuve : un code hors contrat est refusé.
        let faux = json!({ "status": 409, "code": "nope", "detail": "", "plugin_id": null });
        assert!(strict("refusal", &faux).is_err());
    }
}
