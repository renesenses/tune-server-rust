//! Les corps JSON de l'API TESS et leur schéma.
//!
//! - `GET /zones/{id}/tess` → [`ReponseTessZone`] ;
//! - `PUT /zones/{id}/tess` ← [`crate::tess::ChaineTess`], → [`ReponseTessZone`]
//!   ou [`RefusTess`] (402 droit manquant, 409 chaîne invalide).
//!
//! Les routes elles-mêmes sont le lot T08. Un serveur sans TESS répond 404 sur
//! `GET /zones/{id}/tess` : c'est ce qui garde l'écran TESS caché côté web.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::VERSION_CONTRAT;
use super::chaine::{ChaineTess, CodeEtape, ErreurChaine, PlacementVolume};
use super::greffon::Catalogue;
use super::zones::{Applicabilite, TransportZone};

/// Une étape du chemin du signal d'une zone : demandé, effectif, motif (sur le
/// modèle de `crossfeed_status`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EtapeZone {
    pub code: CodeEtape,
    pub plugin_id: Option<String>,
    pub fixed: bool,
    /// Ce que la chaîne demande.
    pub requested: bool,
    /// Ce qui est réellement appliqué au son de la zone.
    pub effective: bool,
    /// Code stable du motif quand `requested != effective`.
    pub reason: Option<String>,
    /// Le motif en clair.
    pub detail: Option<String>,
}

/// La réponse de `GET /zones/{id}/tess` (et d'un `PUT` accepté).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReponseTessZone {
    pub contract_version: u32,
    pub zone_id: i64,
    pub transport: TransportZone,
    pub applicability: Applicabilite,
    pub chain: ChaineTess,
    /// Headroom réellement appliqué, en dB.
    pub headroom_db: f64,
    pub volume_placement: PlacementVolume,
    /// L'ordre réel (Q9), source et sortie comprises.
    pub signal_path: Vec<EtapeZone>,
    /// Les greffons qu'on peut ajouter (« + Add plugin »), avec leurs droits.
    pub catalog: Catalogue,
}

impl ReponseTessZone {
    /// Assemble la réponse d'une zone. Ce contrat ne connaît que
    /// l'applicabilité du transport : les motifs plus fins (lecteur sans LPCM,
    /// mode Pure…) sont ajoutés par les lots T08 et T12.
    pub fn depuis(
        zone_id: i64,
        transport: TransportZone,
        chain: ChaineTess,
        catalog: Catalogue,
    ) -> Self {
        let applicability = transport.applicabilite();
        let hors = match &applicability {
            Applicabilite::Applied => None,
            Applicabilite::OutsideTess { reason, detail } => Some((
                serde_json::to_value(reason)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                detail.clone(),
            )),
        };
        let signal_path = chain
            .ordre_des_etapes()
            .into_iter()
            .map(|e| {
                // Hors TESS, les étapes de traitement ne sont pas appliquées ;
                // source, volume, quantification et sortie restent celles du
                // transport.
                let traitement = matches!(
                    e.code,
                    CodeEtape::Headroom | CodeEtape::Plugin | CodeEtape::Upsampling
                );
                let bloque = traitement && e.active && hors.is_some();
                EtapeZone {
                    code: e.code,
                    plugin_id: e.plugin_id,
                    fixed: e.fixed,
                    requested: e.active,
                    effective: e.active && !bloque,
                    reason: hors.as_ref().filter(|_| bloque).map(|h| h.0.clone()),
                    detail: hors.as_ref().filter(|_| bloque).map(|h| h.1.clone()),
                }
            })
            .collect();
        Self {
            contract_version: VERSION_CONTRAT,
            zone_id,
            transport,
            applicability,
            headroom_db: chain.headroom_effectif_db(),
            volume_placement: chain.placement_volume(),
            chain,
            signal_path,
            catalog,
        }
    }
}

/// Le corps d'un refus 402 ou 409.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Le schéma JSON (draft 2020-12) de l'API TESS. Ses définitions racines
/// utiles : `chain` (corps du PUT), `zone_response` (GET), `refusal` (402/409),
/// `catalog`, `presets` (préréglages et mode d'application).
pub fn schema_json() -> Value {
    let chaine_ou_null = json!({ "type": ["string", "null"] });
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://mozaiklabs.fr/schemas/tess/contract-1.json",
        "title": "TESS API contract",
        "x-contract-version": VERSION_CONTRAT,
        "$defs": {
            "plugin_id": { "type": "string", "pattern": "^[a-z0-9]([a-z0-9-]*[a-z0-9])?$", "maxLength": 64 },
            "plugin_link": {
                "type": "object",
                "required": ["id", "enabled"],
                "properties": {
                    "id": { "$ref": "#/$defs/plugin_id" },
                    "enabled": { "type": "boolean" },
                    "settings": { "type": ["object", "null"] }
                },
                "additionalProperties": false
            },
            "headroom": {
                "oneOf": [
                    {
                        "type": "object", "required": ["mode"],
                        "properties": { "mode": { "const": "auto" } },
                        "additionalProperties": false
                    },
                    {
                        "type": "object", "required": ["mode", "db"],
                        "properties": {
                            "mode": { "const": "manual" },
                            "db": { "type": "number", "minimum": -24.0, "maximum": 0.0 }
                        },
                        "additionalProperties": false
                    }
                ]
            },
            "upsampling": {
                "oneOf": [
                    {
                        "type": "object", "required": ["mode"],
                        "properties": { "mode": { "const": "off" } },
                        "additionalProperties": false
                    },
                    {
                        "type": "object", "required": ["mode", "factor"],
                        "properties": {
                            "mode": { "const": "pcm" },
                            "factor": { "type": "integer", "minimum": 2, "maximum": 16 }
                        },
                        "additionalProperties": false
                    },
                    {
                        "type": "object", "required": ["mode", "rate"],
                        "properties": {
                            "mode": { "const": "dsd" },
                            "rate": { "enum": ["dsd64", "dsd128", "dsd256"] }
                        },
                        "additionalProperties": false
                    }
                ]
            },
            "quantization": {
                "type": "object", "required": ["dither"],
                "properties": { "dither": { "enum": ["tpdf", "noise_shaped"] } },
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
                "type": "object",
                "required": ["version"],
                "properties": {
                    "version": { "const": 1 },
                    "bypass_all": { "type": "boolean" },
                    "plugins": { "type": "array", "items": { "$ref": "#/$defs/plugin_link" } },
                    "fixed": { "$ref": "#/$defs/fixed_stages" }
                },
                "additionalProperties": false
            },
            "placement": {
                "type": "object",
                "required": ["channels", "position", "exclusive_group"],
                "properties": {
                    "channels": { "enum": ["any", "stereo_only", "remaps"] },
                    "position": { "enum": ["free", "last_plugin"] },
                    "exclusive_group": chaine_ou_null
                },
                "additionalProperties": false
            },
            "plugin_descriptor": {
                "type": "object",
                "required": ["id", "name", "category", "entitlement", "origin", "settings_schema", "placement"],
                "properties": {
                    "id": { "$ref": "#/$defs/plugin_id" },
                    "name": { "type": "string" },
                    "category": { "enum": ["equalisation", "spatial", "room_and_filters", "restoration_and_routing"] },
                    "entitlement": { "enum": ["free", "premium"] },
                    "origin": { "enum": ["reference", "package"] },
                    "settings_schema": { "type": "object" },
                    "placement": { "$ref": "#/$defs/placement" }
                },
                "additionalProperties": false
            },
            "catalog": {
                "type": "object",
                "required": ["plugins"],
                "properties": {
                    "plugins": { "type": "array", "items": { "$ref": "#/$defs/plugin_descriptor" } }
                },
                "additionalProperties": false
            },
            "transport": {
                "enum": ["local", "oaat", "dlna", "openhome", "bluos", "squeezebox", "chromecast",
                         "airplay", "devialet_air", "hqplayer", "diretta", "other"]
            },
            "applicability": {
                "oneOf": [
                    {
                        "type": "object", "required": ["status"],
                        "properties": { "status": { "const": "applied" } },
                        "additionalProperties": false
                    },
                    {
                        "type": "object", "required": ["status", "reason", "detail"],
                        "properties": {
                            "status": { "const": "outside_tess" },
                            "reason": { "enum": ["hqplayer_own_processing", "diretta_outside_tess", "unclassified_transport"] },
                            "detail": { "type": "string" }
                        },
                        "additionalProperties": false
                    }
                ]
            },
            "zone_step": {
                "type": "object",
                "required": ["code", "plugin_id", "fixed", "requested", "effective", "reason", "detail"],
                "properties": {
                    "code": { "enum": ["source", "headroom", "plugin", "upsampling", "volume",
                                       "quantization", "dsd_modulator", "output"] },
                    "plugin_id": chaine_ou_null,
                    "fixed": { "type": "boolean" },
                    "requested": { "type": "boolean" },
                    "effective": { "type": "boolean" },
                    "reason": chaine_ou_null,
                    "detail": chaine_ou_null
                },
                "additionalProperties": false
            },
            "zone_response": {
                "type": "object",
                "required": ["contract_version", "zone_id", "transport", "applicability", "chain",
                             "headroom_db", "volume_placement", "signal_path", "catalog"],
                "properties": {
                    "contract_version": { "const": VERSION_CONTRAT },
                    "zone_id": { "type": "integer" },
                    "transport": { "$ref": "#/$defs/transport" },
                    "applicability": { "$ref": "#/$defs/applicability" },
                    "chain": { "$ref": "#/$defs/chain" },
                    "headroom_db": { "type": "number" },
                    "volume_placement": { "enum": ["at_render", "before_dsd_modulator"] },
                    "signal_path": { "type": "array", "items": { "$ref": "#/$defs/zone_step" } },
                    "catalog": { "$ref": "#/$defs/catalog" }
                },
                "additionalProperties": false
            },
            "preset": {
                "type": "object",
                "required": ["id", "name", "chain"],
                "properties": {
                    "id": { "$ref": "#/$defs/plugin_id" },
                    "name": { "type": "string" },
                    "builtin": { "type": "boolean" },
                    "chain": { "$ref": "#/$defs/chain" }
                },
                "additionalProperties": false
            },
            "presets": {
                "type": "object",
                "required": ["active", "presets"],
                "properties": {
                    "active": { "type": ["string", "null"] },
                    "apply_mode": { "enum": ["immediately", "at_next_playback"] },
                    "presets": { "type": "array", "items": { "$ref": "#/$defs/preset" } }
                },
                "additionalProperties": false
            },
            "refusal": {
                "type": "object",
                "required": ["status", "code", "detail", "plugin_id"],
                "properties": {
                    "status": { "enum": [402, 409] },
                    "code": { "enum": ["unsupported_version", "unknown_plugin", "duplicate_plugin",
                                       "invalid_settings", "must_be_last_plugin", "mutually_exclusive",
                                       "stereo_only", "premium_required", "headroom_out_of_range",
                                       "upsampling_factor_out_of_range"] },
                    "detail": { "type": "string" },
                    "plugin_id": chaine_ou_null
                },
                "additionalProperties": false
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tess::chaine::{
        Dither, EtagesFixes, Headroom, MaillonGreffon, Quantification, TauxDsd, Upsampling,
    };
    use crate::tess::greffon::catalogue_de_reference;
    use crate::tess::zones::MotifHorsTess;
    use crate::tess::{ContexteValidation, VERSION_CHAINE};

    /// Un vérificateur de schéma JSON minimal, limité aux mots-clés que
    /// [`schema_json`] emploie (`pattern` et `maxLength` exceptés : l'id est
    /// vérifié par `DescripteurGreffon::verifier`). Il sert à prouver que ce que le serveur
    /// sérialise est conforme au schéma publié — et que le schéma discrimine.
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

    fn verifier(def: &str, v: &Value) -> Result<(), String> {
        let s = schema_json();
        conforme(&s, &s["$defs"][def], v)
    }

    fn chaine_complete() -> ChaineTess {
        ChaineTess {
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
            fixed: EtagesFixes {
                headroom: Headroom::Manual { db: -3.0 },
                upsampling: Upsampling::Dsd {
                    rate: TauxDsd::Dsd128,
                },
                quantization: Quantification {
                    dither: Dither::NoiseShaped,
                },
            },
        }
    }

    #[test]
    fn la_reponse_d_une_zone_fait_l_aller_retour_et_suit_le_schema() {
        for t in TransportZone::ALL {
            for chaine in [ChaineTess::default(), chaine_complete()] {
                let r = ReponseTessZone::depuis(12, t, chaine, catalogue_de_reference());
                let v = serde_json::to_value(&r).unwrap();
                verifier("zone_response", &v).unwrap_or_else(|e| panic!("{t:?} : {e}"));
                let relue: ReponseTessZone = serde_json::from_value(v).unwrap();
                assert_eq!(relue, r);
            }
        }
    }

    #[test]
    fn hors_tess_les_etapes_de_traitement_ne_sont_pas_effectives() {
        let r = ReponseTessZone::depuis(
            3,
            TransportZone::HqPlayer,
            chaine_complete(),
            catalogue_de_reference(),
        );
        let conv = r
            .signal_path
            .iter()
            .find(|e| e.plugin_id.as_deref() == Some("convolution"))
            .unwrap();
        assert!(conv.requested && !conv.effective);
        assert_eq!(conv.reason.as_deref(), Some("hqplayer_own_processing"));
        assert_eq!(
            conv.detail.as_deref(),
            Some(MotifHorsTess::HqplayerOwnProcessing.detail())
        );
        // Contre-épreuve : la même chaîne sur OAAT est effective, sans motif.
        let r = ReponseTessZone::depuis(
            3,
            TransportZone::Oaat,
            chaine_complete(),
            catalogue_de_reference(),
        );
        assert!(
            r.signal_path
                .iter()
                .all(|e| e.requested == e.effective && e.reason.is_none())
        );
    }

    #[test]
    fn chaines_catalogue_et_refus_suivent_le_schema() {
        verifier("chain", &serde_json::to_value(chaine_complete()).unwrap()).unwrap();
        verifier(
            "chain",
            &serde_json::to_value(ChaineTess::default()).unwrap(),
        )
        .unwrap();
        verifier("chain", &json!({ "version": 1 })).unwrap();
        verifier(
            "catalog",
            &serde_json::to_value(catalogue_de_reference()).unwrap(),
        )
        .unwrap();
        let p = crate::tess::PreReglages {
            active: Some("pure".into()),
            ..Default::default()
        };
        verifier("presets", &serde_json::to_value(&p).unwrap()).unwrap();
        let mauvais = json!({ "active": null, "presets": [], "apply_mode": "later" });
        assert!(verifier("presets", &mauvais).is_err());
        let ctx = ContexteValidation {
            channels: Some(2),
            premium: false,
        };
        let e = chaine_complete()
            .valider(&catalogue_de_reference(), ctx)
            .unwrap_err();
        let refus = RefusTess::from(&e);
        assert_eq!(refus.status, 402);
        assert_eq!(refus.plugin_id.as_deref(), Some("convolution"));
        let v = serde_json::to_value(&refus).unwrap();
        verifier("refusal", &v).unwrap();
        assert_eq!(serde_json::from_value::<RefusTess>(v).unwrap(), refus);
    }

    #[test]
    fn le_schema_refuse_les_documents_invalides() {
        // Contre-épreuve : le schéma discrimine.
        for doc in [
            json!({}),
            json!({ "version": 2 }),
            json!({ "version": 1, "volume": -18 }),
            json!({ "version": 1, "plugins": [{ "id": 3, "enabled": true }] }),
            json!({ "version": 1, "plugins": [{ "id": "crossfeed" }] }),
            json!({ "version": 1, "fixed": { "headroom": { "mode": "manual", "db": 3 } } }),
            json!({ "version": 1, "fixed": { "headroom": { "mode": "auto", "db": -3 } } }),
            json!({ "version": 1, "fixed": { "upsampling": { "mode": "pcm", "factor": 32 } } }),
            json!({ "version": 1, "fixed": { "upsampling": { "mode": "dsd", "rate": "dsd512" } } }),
        ] {
            assert!(verifier("chain", &doc).is_err(), "{doc}");
        }
        let mut r = serde_json::to_value(ReponseTessZone::depuis(
            1,
            TransportZone::Local,
            ChaineTess::default(),
            catalogue_de_reference(),
        ))
        .unwrap();
        r["transport"] = json!("sonos");
        assert!(verifier("zone_response", &r).is_err());
    }

    #[test]
    fn le_schema_couvre_tous_les_codes_de_refus() {
        let s = schema_json();
        let codes = s["$defs"]["refusal"]["properties"]["code"]["enum"]
            .as_array()
            .unwrap()
            .clone();
        let erreurs = [
            ErreurChaine::VersionNonPriseEnCharge {
                trouvee: 2,
                attendue: 1,
            },
            ErreurChaine::GreffonInconnu("x".into()),
            ErreurChaine::GreffonEnDouble("x".into()),
            ErreurChaine::ReglagesInvalides {
                id: "x".into(),
                raison: "r".into(),
            },
            ErreurChaine::PasEnDernier("x".into()),
            ErreurChaine::Exclusifs {
                premier: "a".into(),
                second: "b".into(),
                groupe: "g".into(),
            },
            ErreurChaine::StereoSeulement {
                id: "x".into(),
                canaux: 6,
            },
            ErreurChaine::PremiumRequis("x".into()),
            ErreurChaine::HeadroomHorsBornes(3.0),
            ErreurChaine::FacteurHorsBornes(1),
        ];
        assert_eq!(codes.len(), erreurs.len());
        for e in &erreurs {
            assert!(codes.contains(&json!(e.code())), "{}", e.code());
            verifier(
                "refusal",
                &serde_json::to_value(RefusTess::from(e)).unwrap(),
            )
            .unwrap();
        }
    }
}
