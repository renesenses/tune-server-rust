//! Greffons natifs TIERS : paquets signés installés sous un identifiant qui
//! n'est aucun des quatre emplacements de [`super::premium_plugins`], et dont
//! le code n'est pas dans ce dépôt.
//!
//! Règles de l'hôte :
//!
//! - un greffon natif tiers exige TOUJOURS le Premium. Le droit gratuit de
//!   l'égaliseur n'appartient qu'à l'emplacement `equalizer` ; le champ
//!   `entitlement` d'un manifeste tiers est informatif, il n'ouvre rien ;
//! - seuls les greffons de type DSP sont branchés sur la chaîne de lecture ;
//! - un étage tiers est STÉRÉO : il rejoint l'étage casque de la chaîne
//!   ([`super::crossfeed::CrossfeedProcessor`]), après le crossfeed intégré,
//!   là où la sortie locale, les bras streaming et le relais réseau
//!   appliquent déjà cet étage. Le mode PURE le désarme comme le reste ;
//! - drapeaux d'installation : les mêmes clés que les quatre emplacements,
//!   `plugin_{id}_installed` et `plugin_{id}_enabled`, sans fenêtre legacy ni
//!   migration ;
//! - réglage d'une zone : une ligne de réglages par zone et par greffon,
//!   [`cle_de_zone`], qui porte le JSON de réglages du greffon tel quel.
//!   L'étage n'est construit que si ce JSON porte `"enabled": true`, la
//!   convention des greffons de référence ;
//! - profils nommés : une liste JSON dans UNE ligne de réglages par greffon,
//!   [`cle_des_profils`], sur le modèle des préréglages du crossfeed. Aucune
//!   migration de schéma.
use crate::db::settings_repo::SettingsRepo;
use serde_json::Value;

/// Taille maximale, en octets, du JSON de réglages d'un étage tiers (zone ou
/// profil). Un réglage audio tient en quelques centaines d'octets.
pub const REGLAGE_MAX_OCTETS: usize = 16 * 1024;

/// La ligne de réglages qui porte le réglage de la zone pour ce greffon.
pub fn cle_de_zone(zone_id: i64, id: &str) -> String {
    format!("zone_{zone_id}_native_plugin_{id}")
}

/// La ligne de réglages qui porte les profils nommés de ce greffon.
pub fn cle_des_profils(id: &str) -> String {
    format!("native_plugin_{id}_profiles")
}

/// Un identifiant que l'hôte peut accepter pour un greffon natif tiers :
/// valide pour le SDK et distinct des quatre emplacements intégrés. Les
/// collisions avec d'autres familles de greffons (compilés, WASM) sont
/// jugées par le serveur, qui les connaît.
pub fn identifiant_admissible(id: &str) -> bool {
    tune_plugin_sdk::manifest::valid_id(id) && !super::premium_plugins::contains(id)
}

/// L'utilisateur a installé ET n'a pas désactivé ce greffon tiers.
pub fn demande(settings: &SettingsRepo, id: &str) -> bool {
    identifiant_admissible(id)
        && settings
            .get(&format!("plugin_{id}_installed"))
            .is_ok_and(|v| v.as_deref() == Some("true"))
        && settings
            .get(&format!("plugin_{id}_enabled"))
            .is_ok_and(|v| v.as_deref() != Some("false"))
}

/// Le fournisseur natif DSP chargé pour ce greffon tiers, s'il y en a un et
/// qu'il n'a pas échoué au démarrage.
pub fn fournisseur_dsp(id: &str) -> Option<std::sync::Arc<tune_plugin_native::Library>> {
    if !identifiant_admissible(id) || tune_plugin_native::failure(id).is_some() {
        return None;
    }
    tune_plugin_native::provider(id)
        .filter(|library| library.manifest.kind == tune_plugin_sdk::manifest::PluginKind::Dsp)
}

/// Demandé par l'utilisateur ET chargé : prêt à traiter du son.
pub fn actif(settings: &SettingsRepo, id: &str) -> bool {
    demande(settings, id) && fournisseur_dsp(id).is_some()
}

/// Les greffons natifs tiers DSP chargés, triés.
pub fn identifiants_charges() -> Vec<String> {
    tune_plugin_native::provider_ids()
        .into_iter()
        .filter(|id| fournisseur_dsp(id).is_some())
        .collect()
}

/// Le réglage de la zone pour ce greffon, s'il est lisible.
pub fn reglage_de_zone(settings: &SettingsRepo, zone_id: i64, id: &str) -> Option<Value> {
    settings
        .get(&cle_de_zone(zone_id, id))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(Value::is_object)
}

/// Les étages tiers que la zone demande, dans l'ordre des identifiants :
/// `(identifiant, réglages)`. Le droit Premium n'est PAS jugé ici, ni le mode
/// PURE : c'est l'orchestrateur qui les juge, comme pour le crossfeed.
pub fn etages_configures(settings: &SettingsRepo, zone_id: i64) -> Vec<(String, Value)> {
    identifiants_charges()
        .into_iter()
        .filter(|id| demande(settings, id))
        .filter_map(|id| {
            let reglage = reglage_de_zone(settings, zone_id, &id)?;
            (reglage.get("enabled").and_then(Value::as_bool) == Some(true)).then_some((id, reglage))
        })
        .collect()
}

/// Empreinte stable des étages : elle entre dans la clé du cache de
/// transcodage et dans l'empreinte du traitement d'un flux. Vide sans étage.
pub fn empreinte(etages: &[(String, Value)]) -> String {
    etages
        .iter()
        .map(|(id, reglage)| format!("{id}={reglage}"))
        .collect::<Vec<_>>()
        .join(";")
}

/// Vérifie qu'un réglage est acceptable pour ce greffon : un objet JSON de
/// taille bornée, que la fabrique du greffon accepte réellement (préparation
/// d'un étage stéréo à 48 kHz, jetée aussitôt). La sémantique des champs
/// appartient au greffon ; l'hôte ne la devine pas.
pub fn valider_reglage(id: &str, reglage: &Value) -> Result<(), String> {
    if !reglage.is_object() {
        return Err("settings must be a JSON object".into());
    }
    if serde_json::to_vec(reglage).map_or(usize::MAX, |v| v.len()) > REGLAGE_MAX_OCTETS {
        return Err(format!("settings exceed {REGLAGE_MAX_OCTETS} bytes"));
    }
    let fournisseur = fournisseur_dsp(id).ok_or("native plugin not loaded")?;
    tune_plugin_native::stage::Stage::prepare(fournisseur, 48_000, 2, reglage)
        .map(drop)
        .map_err(|e| format!("settings refused by the plugin: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> SettingsRepo {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        SettingsRepo::with_backend(std::sync::Arc::new(db))
    }

    #[test]
    fn les_quatre_emplacements_ne_sont_jamais_des_greffons_tiers() {
        for id in super::super::premium_plugins::IDS {
            assert!(!identifiant_admissible(id), "{id} pris pour un tiers");
        }
        assert!(identifiant_admissible("greffon-tiers"));
        assert!(!identifiant_admissible("../evasion"));
        assert!(!identifiant_admissible(""));
    }

    #[test]
    fn demande_suit_les_drapeaux_d_installation() {
        let s = settings();
        assert!(!demande(&s, "greffon-tiers"), "absent = pas demandé");
        s.set("plugin_greffon-tiers_installed", "true").unwrap();
        assert!(demande(&s, "greffon-tiers"));
        s.set("plugin_greffon-tiers_enabled", "false").unwrap();
        assert!(!demande(&s, "greffon-tiers"), "désactivé explicitement");
        // Un emplacement intégré ne passe jamais par ici.
        s.set("plugin_crossfeed_installed", "true").unwrap();
        assert!(!demande(&s, "crossfeed"));
    }

    #[test]
    fn sans_fournisseur_charge_aucun_etage() {
        let s = settings();
        s.set("plugin_greffon-tiers_installed", "true").unwrap();
        s.set(&cle_de_zone(1, "greffon-tiers"), r#"{"enabled":true}"#)
            .unwrap();
        assert!(!actif(&s, "greffon-tiers"));
        assert!(etages_configures(&s, 1).is_empty());
        assert!(valider_reglage("greffon-tiers", &serde_json::json!({})).is_err());
    }

    #[test]
    fn empreinte_vide_sans_etage_et_stable_sinon() {
        assert_eq!(empreinte(&[]), "");
        let a = vec![("x".to_string(), serde_json::json!({"enabled":true,"k":1}))];
        assert_eq!(empreinte(&a), empreinte(&a.clone()));
        let b = vec![("x".to_string(), serde_json::json!({"enabled":true,"k":2}))];
        assert_ne!(empreinte(&a), empreinte(&b));
    }

    #[test]
    fn un_reglage_qui_n_est_pas_un_objet_ou_trop_gros_est_refuse() {
        assert!(valider_reglage("greffon-tiers", &serde_json::json!([1])).is_err());
        let gros = serde_json::json!({"x": "a".repeat(REGLAGE_MAX_OCTETS)});
        assert!(
            valider_reglage("greffon-tiers", &gros)
                .unwrap_err()
                .contains("exceed")
        );
    }
}
