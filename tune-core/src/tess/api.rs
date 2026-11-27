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
use super::chaine::{ChaineTess, ErreurChaine, Upsampling};
use super::greffon::Catalogue;
use super::modulation_dsd::{CapacitesDsd, MotifDsdRefuse};
use super::reseau_upnp::{
    AcheminementUpnp, FaitsUpnp, SurechantillonnageUpnp, acheminer_avec, appliquer, famille_upnp,
};
use super::resolution::{
    Badge, CodeEtape, ContexteExecution, EtapeResolue, EtatVolume, MotifEtape, PlacementVolume,
    Resolution,
};
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
    /// rc14 (Rémi) — le préréglage dont la chaîne de la zone est PARTIE (le
    /// dernier chargé, enregistré ou écrasé depuis cette zone), même après
    /// une retouche. Absent s'il n'y en a pas ou s'il a été supprimé.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_preset: Option<String>,
    /// rc14 (Rémi) — la chaîne ne décrit plus `base_preset` : l'écran propose
    /// « Enregistrer » (écraser) et « Recharger ». Absent quand faux.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preset_modified: bool,
    /// Headroom réellement appliqué, en dB (0 hors TESS ou sans traitement).
    pub headroom_db: f64,
    pub volume: EtatVolume,
    pub badge: Badge,
    /// L'ordre réel (Q9), source et sortie comprises, demandé et effectif.
    pub signal_path: Vec<EtapeResolue>,
    /// Les greffons qu'on peut ajouter (« + Add plugin »), avec leurs droits.
    pub catalog: Catalogue,
    /// T12b — famille UPnP seulement : la voie par laquelle la chaîne atteint
    /// le lecteur (relais progressif LPCM, transcodage, ou aucune) et son
    /// motif. Absent ailleurs ; un client qui ne le connaît pas l'ignore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<AcheminementUpnp>,
    /// Avertissements d'un `PUT` accepté (T17 : `active_preset_not_stored`
    /// quand le marqueur « ACTIVE » n'a pas pu être tenu). Absent si vide.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// rc14 — ce que la sortie de la zone fait de chaque cible DSD
    /// (`tess::modulation_dsd::CapacitesDsd`) : acceptée ou non, en natif ou
    /// en DoP, avec son motif. Posé par le serveur ; absent d'un serveur
    /// antérieur, qu'un client lit comme « inconnu ».
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsd_capabilities: Option<CapacitesDsd>,
}

impl ReponseTessZone {
    pub fn depuis(
        zone_id: i64,
        chain: ChaineTess,
        active_preset: Option<String>,
        catalog: Catalogue,
        ctx: &ContexteExecution<'_>,
    ) -> Self {
        Self::depuis_avec_le_lecteur(zone_id, chain, active_preset, catalog, ctx, None)
    }

    /// T12b — comme [`Self::depuis`], et, sur une zone de la famille UPnP
    /// dont la lecture a établi les faits du lecteur, la voie de la chaîne :
    /// un étage que la voie ne porte pas est publié éteint, avec son motif.
    pub fn depuis_avec_le_lecteur(
        zone_id: i64,
        chain: ChaineTess,
        active_preset: Option<String>,
        catalog: Catalogue,
        ctx: &ContexteExecution<'_>,
        lecteur: Option<&FaitsUpnp>,
    ) -> Self {
        Self::depuis_avec_le_lecteur_et_surech(
            zone_id,
            chain,
            active_preset,
            catalog,
            ctx,
            lecteur,
            None,
        )
    }

    /// rc14 — comme [`Self::depuis_avec_le_lecteur`], avec la décision de la
    /// lecture sur le suréchantillonnage PCM vers ce lecteur
    /// (`delivery.upsampling`, et l'étape `upsampling` qui publie ses
    /// cadences ou son refus).
    pub fn depuis_avec_le_lecteur_et_surech(
        zone_id: i64,
        chain: ChaineTess,
        active_preset: Option<String>,
        catalog: Catalogue,
        ctx: &ContexteExecution<'_>,
        lecteur: Option<&FaitsUpnp>,
        surech: Option<&SurechantillonnageUpnp>,
    ) -> Self {
        let mut r = chain.resoudre(&catalog, ctx);
        let delivery = lecteur
            .filter(|_| famille_upnp(ctx.transport) && ctx.transport.applicabilite().appliquee())
            .map(|f| {
                let a = acheminer_avec(&r, f, surech);
                appliquer(&mut r, &a);
                a
            });
        Self {
            contract_version: VERSION_CONTRAT,
            zone_id,
            transport: ctx.transport,
            applicability: ctx.transport.applicabilite(),
            chain,
            active_preset,
            base_preset: None,
            preset_modified: false,
            headroom_db: r.headroom_db,
            volume: r.volume,
            badge: r.badge,
            signal_path: r.etapes,
            catalog,
            delivery,
            warnings: Vec::new(),
            dsd_capabilities: None,
        }
    }

    /// rc14 — publie les capacités DSD de la sortie et les reporte sur le
    /// chemin résolu : vers le DSD, une cible que la sortie locale ne prend
    /// pas éteint l'upsampling et le modulateur (`dsd_not_supported_by_output`,
    /// `detail` = motif précis) ; une cible acceptée dit son transport. Les
    /// sorties réseau ne sont pas touchées (`not_applied_on_transport`).
    pub fn avec_capacites_dsd(mut self, capacites: CapacitesDsd) -> Self {
        let cible = match &self.chain.fixed.upsampling {
            Upsampling::Dsd { rate, .. } => capacites.cible(*rate).cloned(),
            _ => None,
        };
        if let Some(cible) = cible.filter(|_| self.transport == TransportZone::Local) {
            let f = &cible.famille_44k1;
            let mut modifie = false;
            for e in self.signal_path.iter_mut().filter(|e| {
                e.effective && matches!(e.code, CodeEtape::Upsampling | CodeEtape::DsdModulator)
            }) {
                if f.accepted {
                    if e.code == CodeEtape::DsdModulator {
                        let mut p = match e.params.take() {
                            Some(Value::Object(m)) => m,
                            _ => Map::new(),
                        };
                        p.insert("order".into(), json!(capacites.modulator_order));
                        p.insert("rate_hz".into(), json!(f.rate_hz));
                        p.insert("transport".into(), json!(f.transport));
                        p.insert("dop_rate_hz".into(), json!(f.dop_rate_hz));
                        p.insert(
                            "zero_dbfs_modulation".into(),
                            json!(super::modulation_dsd::MODULATION_A_0_DBFS),
                        );
                        // Décision de Bertrand du 26/11 : la marge propre du
                        // modulateur, publiée à part du headroom de la chaîne.
                        p.insert(
                            "modulator_margin_db".into(),
                            json!(super::modulation_dsd::MARGE_DU_MODULATEUR_DB),
                        );
                        e.params = Some(Value::Object(p));
                    }
                } else {
                    e.effective = false;
                    e.reason = Some(MotifEtape::DsdNotSupportedByOutput);
                    e.detail = f.reason.map(|m| m.as_str().to_owned());
                    modifie = true;
                }
            }
            if modifie {
                if self.volume.placement == Some(PlacementVolume::BeforeDsdModulator) {
                    self.volume.placement = Some(PlacementVolume::AtRender);
                }
                let mut r = Resolution {
                    etapes: std::mem::take(&mut self.signal_path),
                    headroom_db: self.headroom_db,
                    volume: self.volume,
                    badge: self.badge,
                };
                r.recalculer_les_etapes_finales();
                self.signal_path = r.etapes;
                self.headroom_db = r.headroom_db;
                self.badge = r.badge;
            }
        }
        self.dsd_capabilities = Some(capacites);
        self
    }
}

/// Ce qu'un greffon du catalogue vaut pour CE serveur et CE compte
/// (`GET /tess/catalog`, lot T08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtatGreffonCatalogue {
    pub id: String,
    /// Le compte a le droit de le cocher (Free, ou Premium actif).
    pub entitled: bool,
    /// Installé, activé et branché dans l'hôte.
    pub available: bool,
    /// Pourquoi il ne traiterait pas s'il était coché, dans l'ordre de la
    /// résolution : `plugin_not_installed` ou `plugin_unavailable` d'abord,
    /// puis `premium_required` ; `None` : utilisable.
    pub reason: Option<MotifEtape>,
    /// Avec `plugin_not_installed` seulement : le paquet à installer
    /// (`POST /plugins/{install_package}/install`), qui rend le greffon
    /// disponible sans redémarrage. Absent sinon ; un client ancien l'ignore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_package: Option<String>,
}

/// La réponse de `GET /tess/catalog` : le catalogue (schémas de réglages,
/// droits, contraintes de place, politique des groupes) et son état pour le
/// compte. Un serveur sans TESS répond 404 : c'est la capacité TESS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReponseCatalogue {
    pub contract_version: u32,
    /// Le compte est Premium.
    pub premium: bool,
    pub catalog: Catalogue,
    /// Un état par greffon du catalogue, dans le même ordre.
    pub plugins: Vec<EtatGreffonCatalogue>,
}

impl ReponseCatalogue {
    pub fn depuis(catalog: Catalogue, premium: bool, disponible: &dyn Fn(&str) -> bool) -> Self {
        Self::depuis_avec_installation(catalog, premium, disponible, &|_| None)
    }

    /// Comme [`Self::depuis`] ; `a_installer(id)` rend le paquet facultatif
    /// qui manque à un greffon indisponible (égaliseur, réaffectation des
    /// canaux sur une base neuve) : le motif est alors `plugin_not_installed`
    /// avec `install_package`, au lieu de `plugin_unavailable`.
    pub fn depuis_avec_installation(
        catalog: Catalogue,
        premium: bool,
        disponible: &dyn Fn(&str) -> bool,
        a_installer: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let plugins = catalog
            .plugins
            .iter()
            .map(|d| {
                let entitled = premium || !d.demande_le_premium();
                let available = disponible(&d.id);
                let install_package = if available { None } else { a_installer(&d.id) };
                let reason = if install_package.is_some() {
                    Some(MotifEtape::PluginNotInstalled)
                } else if !available {
                    Some(MotifEtape::PluginUnavailable)
                } else if !entitled {
                    Some(MotifEtape::PremiumRequired)
                } else {
                    None
                };
                EtatGreffonCatalogue {
                    id: d.id.clone(),
                    entitled,
                    available,
                    reason,
                    install_package,
                }
            })
            .collect();
        Self {
            contract_version: VERSION_CONTRAT,
            premium,
            catalog,
            plugins,
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
    // T12b — `delivery` est défini hors du grand littéral : `json!` y atteint
    // sa limite de récursion.
    let delivery = json!({
        "type": "object",
        "required": ["route", "reason", "detail", "renderer_lpcm", "lpcm_bits"],
        "properties": {
            "route": enumere(&["untouched", "progressive_lpcm", "file_transcode"]),
            "reason": { "enum": ["nothing_to_process", "renderer_without_lpcm",
                                 "progressive_off", "service_not_streamable",
                                 "dsd_converted_to_pcm", "upsampling_file_transcode", null] },
            "detail": texte_ou_null.clone(),
            "renderer_lpcm": enumere(&["accepted", "refused", "unknown"]),
            "lpcm_bits": { "type": "integer" },
            // rc14 — le suréchantillonnage PCM vers ce lecteur.
            "upsampling": {
                "type": "object",
                "required": ["active", "offered_rates_hz"],
                "properties": {
                    "active": { "type": "boolean" },
                    "reason": { "type": "string" },
                    "from_hz": { "type": "integer" },
                    "to_hz": { "type": "integer" },
                    "filter": { "type": "string" },
                    "headroom_db": { "type": "number" },
                    "offered_rates_hz": { "type": "array", "items": { "type": "integer" } }
                },
                "additionalProperties": false
            }
        },
        "additionalProperties": false
    });
    let motifs_etape = json!({ "enum": ["outside_tess", "pure_mode", "bypass_all",
        "dsd_source_native", "chain_version_unsupported", "unknown_value", "unknown_plugin",
        "plugin_unavailable", "premium_required", "stereo_only", "replaced_in_group",
        "nothing_to_protect", "volume_not_software", "nothing_to_quantize", "not_in_service",
        "renderer_without_lpcm", "not_applied_on_transport", "plugin_not_installed",
        "no_filter_selected", "dsd_not_supported_by_output", "neutral_settings", null] });
    let mut schema = json!({
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
                    "position": enumere(&["free", "last_plugin", "before_upsampling"]),
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
            "catalog_response": {
                "type": "object", "required": ["contract_version", "premium", "catalog", "plugins"],
                "properties": {
                    "contract_version": { "const": VERSION_CONTRAT },
                    "premium": { "type": "boolean" },
                    "catalog": { "$ref": "#/$defs/catalog" },
                    "plugins": { "type": "array", "items": {
                        "type": "object", "required": ["id", "entitled", "available", "reason"],
                        "properties": {
                            "id": { "$ref": "#/$defs/id" },
                            "entitled": { "type": "boolean" },
                            "available": { "type": "boolean" },
                            "reason": { "enum": ["plugin_not_installed", "plugin_unavailable",
                                                 "premium_required", null] },
                            "install_package": { "$ref": "#/$defs/id" }
                        },
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
                    "reason": motifs_etape,
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
                    "badge": enumere(&["bit_perfect", "processed_by_tess", "outside_tess",
                                       "altered_outside_tess"]),
                    "signal_path": { "type": "array", "items": { "$ref": "#/$defs/step" } },
                    "catalog": { "$ref": "#/$defs/catalog" },
                    // Facultatif (T17) : avertissements d'un `PUT` accepté.
                    "warnings": { "type": "array", "items": { "type": "string" } }
                },
                "additionalProperties": false
            },
            "preset": {
                "type": "object", "required": ["id", "name", "chain"],
                "properties": {
                    "id": { "$ref": "#/$defs/id" },
                    "name": { "type": "string" },
                    "builtin": { "type": "boolean" },
                    "chain": { "$ref": "#/$defs/chain" },
                    // Facultatif (T17) : seulement dans la réponse d'un
                    // `POST /tess/presets` dont le marqueur « ACTIVE » n'a
                    // pas pu être posé (`active_preset_not_stored`).
                    "warnings": { "type": "array", "items": { "type": "string" } }
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
                    "plugin_id": texte_ou_null,
                    // Facultatifs (T08) : la forme des autres refus de l'API
                    // (`premium_guard`) pour les clients qui lisent `error`
                    // et affichent `message` ; `error` = `code`, `message` =
                    // `detail`, `upgrade_url` sur un 402 seulement.
                    "error": { "enum": codes_refus },
                    "message": { "type": "string" },
                    "upgrade_url": { "type": "string" },
                    // rc14 — `dsd_not_supported_by_output` seulement : le
                    // motif précis et les capacités de la sortie.
                    "reason": { "type": "string" },
                    "dsd_capabilities": { "type": "object" }
                },
                "additionalProperties": false
            }
        }
    });
    // Facultatifs (rc14, Rémi) : le préréglage d'origine et sa retouche.
    // Posés hors de `json!` : la macro est à sa limite de récursion.
    schema["$defs"]["zone_response"]["properties"]["base_preset"] = json!({ "type": "string" });
    schema["$defs"]["zone_response"]["properties"]["preset_modified"] =
        json!({ "type": "boolean" });
    schema["$defs"]["delivery"] = delivery;
    schema["$defs"]["zone_response"]["properties"]["delivery"] =
        json!({ "$ref": "#/$defs/delivery" });
    schema["$defs"]["dsd_capabilities"] = schema_capacites_dsd();
    schema["$defs"]["zone_response"]["properties"]["dsd_capabilities"] =
        json!({ "$ref": "#/$defs/dsd_capabilities" });
    schema
}

/// rc14 — le schéma de `dsd_capabilities` (voir [`CapacitesDsd`]).
fn schema_capacites_dsd() -> Value {
    let mut motifs: Vec<Value> = MotifDsdRefuse::TOUS
        .iter()
        .map(|m| json!(m.as_str()))
        .collect();
    motifs.push(Value::Null);
    let famille = json!({
        "type": "object",
        "required": ["rate_hz", "accepted", "transport", "dop_rate_hz", "reason", "native_reason"],
        "properties": {
            "rate_hz": { "type": "integer" },
            "accepted": { "type": "boolean" },
            "transport": { "enum": ["native", "dop", null] },
            "dop_rate_hz": { "type": ["integer", "null"] },
            "reason": { "enum": motifs },
            "native_reason": { "type": ["string", "null"] }
        },
        "additionalProperties": false
    });
    let mut cible = famille.clone();
    cible["required"]
        .as_array_mut()
        .expect("liste")
        .extend([json!("rate"), json!("family_48k")]);
    cible["properties"]["rate"] = json!({ "enum": ["dsd64", "dsd128", "dsd256"] });
    cible["properties"]["family_48k"] = famille;
    json!({
        "type": "object",
        "required": ["dsd_mode", "output_local", "modulator_order", "reference_level",
                     "volume_placement", "machine_measured", "status", "targets"],
        "properties": {
            "dsd_mode": { "type": "string" },
            "output_local": { "type": "boolean" },
            "modulator_order": { "type": "integer" },
            "reference_level": { "type": "string" },
            "volume_placement": { "const": "before_dsd_modulator" },
            "machine_measured": { "type": "boolean" },
            "status": { "enum": ["known", "unknown"] },
            "targets": { "type": "array", "items": cible }
        },
        "additionalProperties": false
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

/// Un vérificateur de schéma JSON minimal, limité aux mots-clés que
/// [`schema_json`] emploie (sert aux tests du serveur, T08) (`pattern` et `maxLength` exceptés : l'id est
/// vérifié par `id_valide`).
pub fn conforme(racine: &Value, schema: &Value, v: &Value) -> Result<(), String> {
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

/// `v` est-il conforme à la définition `def` du schéma EXACT ([`schema_json`]) ?
pub fn conforme_au_schema(def: &str, v: &Value) -> Result<(), String> {
    let s = schema_json();
    if s["$defs"].get(def).is_none() {
        return Err(format!("définition {def} absente du schéma"));
    }
    conforme(&s, &s["$defs"][def], v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tess::chaine::{MaillonGreffon, TauxDsd, Upsampling};
    use crate::tess::greffon::catalogue_de_reference;
    use crate::tess::prereglages::PreReglages;
    use crate::tess::resolution::{ControleVolume, SourceFlux, VolumeZone};
    use crate::tess::{ContexteValidation, VERSION_CHAINE};

    fn strict(def: &str, v: &Value) -> Result<(), String> {
        conforme_au_schema(def, v)
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
            replaygain_actif: false,
            voies_de_sortie: None,
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

    /// T12b — `delivery` n'apparaît que sur la famille UPnP, suit le schéma
    /// strict et celui de lecture, et fait l'aller-retour ; l'étage casque
    /// éteint faute de LPCM porte `renderer_without_lpcm`.
    #[test]
    fn la_voie_upnp_est_publiee_sur_la_seule_famille_upnp() {
        use crate::tess::reseau_upnp::{FaitsUpnp, LpcmDuLecteur, OrigineDuFlux, VoieUpnp};
        let casque_seul = ChaineTess {
            plugins: vec![MaillonGreffon {
                id: "crossfeed".into(),
                enabled: true,
                settings: Value::Null,
            }],
            ..ChaineTess::default()
        };
        let faits = FaitsUpnp {
            lpcm: LpcmDuLecteur::Refused,
            lpcm_bits: 16,
            progressif_arme: false,
            origine: OrigineDuFlux::Library,
            replaygain_actif: false,
            service_decodable_au_fil: true,
            format_impose_le_transcodage: false,
        };
        for t in TransportZone::ALL {
            for c in [
                ChaineTess::default(),
                chaine_complete(),
                casque_seul.clone(),
            ] {
                let r = ReponseTessZone::depuis_avec_le_lecteur(
                    12,
                    c.clone(),
                    None,
                    catalogue_de_reference(),
                    &ctx(t),
                    Some(&faits),
                );
                assert_eq!(r.delivery.is_some(), super::famille_upnp(t), "{t:?}");
                let v = serde_json::to_value(&r).unwrap();
                strict("zone_response", &v).unwrap_or_else(|e| panic!("{t:?} : {e}"));
                lecture("zone_response", &v).unwrap_or_else(|e| panic!("{t:?} : {e}"));
                assert_eq!(serde_json::from_value::<ReponseTessZone>(v).unwrap(), r);
            }
        }
        let r = ReponseTessZone::depuis_avec_le_lecteur(
            12,
            casque_seul.clone(),
            None,
            catalogue_de_reference(),
            &ctx(TransportZone::Dlna),
            Some(&faits),
        );
        assert_eq!(r.delivery.as_ref().unwrap().route, VoieUpnp::Untouched);
        let cf = r
            .signal_path
            .iter()
            .find(|e| e.plugin_id.as_deref() == Some("crossfeed"))
            .unwrap();
        assert_eq!(cf.reason, Some(MotifEtape::RendererWithoutLpcm));
        // Contre-épreuve : sans faits du lecteur, rien ne change (`depuis`).
        let sans = reponse(TransportZone::Dlna, casque_seul);
        assert!(sans.delivery.is_none());
        assert_eq!(sans.badge, Badge::ProcessedByTess);
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
        assert_eq!(r.headroom_db, crate::tess::chaine::HEADROOM_AUTO_DB);
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
    #[test]
    fn la_reponse_du_catalogue_suit_le_schema_et_l_ordre_des_motifs() {
        let indisponible = |id: &str| id != "declick-live";
        for premium in [false, true] {
            let r = ReponseCatalogue::depuis(catalogue_de_reference(), premium, &indisponible);
            let v = serde_json::to_value(&r).unwrap();
            strict("catalog_response", &v).unwrap();
            lecture("catalog_response", &v).unwrap();
            assert_eq!(serde_json::from_value::<ReponseCatalogue>(v).unwrap(), r);
            assert_eq!(r.plugins.len(), r.catalog.plugins.len());
            let etat = |id: &str| r.plugins.iter().find(|p| p.id == id).unwrap().clone();
            // Indisponible l'emporte sur le droit, comme à la résolution.
            assert_eq!(
                etat("declick-live").reason,
                Some(MotifEtape::PluginUnavailable)
            );
            assert!(etat("crossfeed").entitled && etat("crossfeed").reason.is_none());
            let conv = etat("convolution");
            assert_eq!(conv.entitled, premium);
            assert_eq!(
                conv.reason,
                (!premium).then_some(MotifEtape::PremiumRequired)
            );
        }
        // rc14 — un paquet facultatif à installer : motif distinct et paquet
        // nommé ; ce qui n'a rien à installer reste `plugin_unavailable`.
        let a_installer = |id: &str| (id == "equalizer-graphic").then(|| "equalizer".to_owned());
        let r = ReponseCatalogue::depuis_avec_installation(
            catalogue_de_reference(),
            true,
            &|id: &str| id != "declick-live" && id != "equalizer-graphic",
            &a_installer,
        );
        let v = serde_json::to_value(&r).unwrap();
        strict("catalog_response", &v).unwrap();
        lecture("catalog_response", &v).unwrap();
        assert_eq!(serde_json::from_value::<ReponseCatalogue>(v).unwrap(), r);
        let etat = |id: &str| r.plugins.iter().find(|p| p.id == id).unwrap().clone();
        assert_eq!(
            etat("equalizer-graphic").reason,
            Some(MotifEtape::PluginNotInstalled)
        );
        assert_eq!(
            etat("equalizer-graphic").install_package.as_deref(),
            Some("equalizer")
        );
        assert_eq!(
            etat("declick-live").reason,
            Some(MotifEtape::PluginUnavailable)
        );
        assert_eq!(etat("declick-live").install_package, None);
        // Un greffon disponible n'a jamais rien à installer.
        let r = ReponseCatalogue::depuis_avec_installation(
            catalogue_de_reference(),
            true,
            &tout,
            &a_installer,
        );
        assert!(r.plugins.iter().all(|p| p.install_package.is_none()));
        // Contre-épreuve : un champ de plus n'est pas émis par ce serveur.
        let mut v = serde_json::to_value(ReponseCatalogue::depuis(
            catalogue_de_reference(),
            true,
            &tout,
        ))
        .unwrap();
        v["plugins"][0]["granted"] = json!(true);
        assert!(strict("catalog_response", &v).is_err());
        assert!(conforme_au_schema("nope", &v).is_err());
    }
}
