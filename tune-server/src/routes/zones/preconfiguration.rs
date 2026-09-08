//! Poser les réglages d'un appareil reconnu, **sans jamais écraser ce que
//! quelqu'un a posé à la main** (#3589, volet B — côté serveur).
//!
//! # 🔴 Le problème que ce module ouvre, et qu'il ne referme qu'à moitié
//!
//! « Ne jamais écraser un réglage posé à la main » suppose de distinguer
//! « réglage à sa valeur par défaut » de « réglage posé à cette valeur ».
//! **Cette distinction n'existait pas dans ce dépôt**, mesuré le 08/09/2026 :
//!
//! * les sept colonnes de `zones` sont créées `INTEGER DEFAULT 0`
//!   (`tune-core/src/db/migrations.rs:2718-2760`) et relues à travers un
//!   `COALESCE(<colonne>, 0)` qui rend un `bool` nu
//!   (`tune-core/src/db/zone_repo.rs:1528-1543` et ses six voisines) : NULL et
//!   « décoché » arrivent identiques, et `create()` n'insère de toute façon que
//!   `(name, output_type, output_device_id)` — les autres colonnes tombent sur
//!   leur défaut, jamais sur NULL ;
//! * les réglages clé/valeur (`zone_{id}_gain_trim_db`,
//!   `zone_{id}_upnp_silence`) sont **supprimés** quand l'utilisateur revient au
//!   défaut, et le dépôt le revendique :
//!   « la clé est supprimée à la désactivation plutôt qu'écrite à « false »,
//!   pour que l'absence de clé et le défaut désarmé soient un seul et même
//!   état » (`tune-server/src/routes/zones/ecriture.rs:568-571`).
//!
//! Autrement dit : **aucun des huit réglages ne portait la trace de son
//! auteur.** Un « ne pas écraser » fondé sur la valeur aurait donc écrasé
//! exactement le geste le plus fréquent — celui de l'utilisateur qui DÉCOCHE
//! une case parce que son appareil n'en veut pas.
//!
//! # Ce que ce module ajoute : une marque d'auteur, pas une devinette
//!
//! [`marquer_pose`] écrit `zone_{id}_pose_{champ}` à la **première** écriture
//! venue de la main — c'est-à-dire du `PATCH /zones/{id}`, la seule porte par
//! laquelle un humain règle une zone. La marque n'est **jamais** effacée, pas
//! même quand la valeur revient au défaut : c'est tout son intérêt, et c'est ce
//! qui la distingue de la convention « clé supprimée » ci-dessus.
//!
//! # 🔴 Ce que la marque ne peut PAS savoir : le passé
//!
//! Une zone créée avant cette version n'a aucune marque. La préconfiguration
//! ne peut donc **pas** être rejouée sur le parc existant : elle prendrait
//! toutes les zones du monde pour des zones neuves. C'est pourquoi
//! [`preconfigurer_zone`] pose `zone_{id}_preconfig` au moment de la création,
//! et **refuse d'agir sur une zone qui ne le porte pas** : seule une zone née
//! avec le dispositif est préconfigurable, aujourd'hui comme à chaque
//! rafraîchissement futur du catalogue.
//!
//! C'est une limite assumée, pas un oubli : le seul autre moyen d'y arriver
//! serait de demander à chaque possesseur ce qu'il a réglé, ce qui n'est pas
//! une option.

use super::*;

use std::sync::Arc;

use tune_core::cloud::tune_tested;
use tune_core::db::backend::DbBackend;
use tune_core::device_preconfig::{Origine, Valeur, preconfigurer};

/// Marque « ce réglage a été posé à la main sur cette zone ».
pub(crate) fn cle_pose(zone_id: i64, champ: &str) -> String {
    format!("zone_{zone_id}_pose_{champ}")
}

/// Marque « cette zone est née avec le dispositif de provenance ».
pub(crate) fn cle_preconfigurable(zone_id: i64) -> String {
    format!("zone_{zone_id}_preconfig")
}

/// Les champs dont la provenance est suivie. Volontairement restreint aux
/// réglages que la préconfiguration sait poser : marquer `name` ou `volume`
/// gonflerait la table des réglages sans que personne n'y lise jamais rien.
pub(crate) fn champ_suivi(champ: &str) -> bool {
    tune_core::device_preconfig::CLES_CONNUES.contains(&champ) || champ == "max_sample_rate"
}

/// Note qu'un humain vient de poser ce réglage. Idempotent, silencieux en cas
/// d'échec d'écriture : perdre une marque ne doit jamais faire échouer le
/// `PATCH` que l'utilisateur a demandé.
pub(crate) fn marquer_pose(db: &Arc<dyn DbBackend>, zone_id: i64, champ: &str) {
    if !champ_suivi(champ) {
        return;
    }
    let settings = SettingsRepo::with_backend(db.clone());
    let _ = settings.set(&cle_pose(zone_id, champ), "1");
}

/// Ce réglage a-t-il été posé à la main sur cette zone ?
pub(crate) fn deja_pose(settings: &SettingsRepo, zone_id: i64, champ: &str) -> bool {
    settings
        .get(&cle_pose(zone_id, champ))
        .ok()
        .flatten()
        .is_some_and(|v| v == "1")
}

/// Ouvre la provenance sur une zone qui vient d'être créée.
///
/// À appeler **au moment de la création, et là seulement** : c'est le seul
/// instant où « aucune marque » veut dire « personne n'a rien réglé » plutôt
/// que « cette zone est antérieure au dispositif ».
pub(crate) fn ouvrir_provenance(db: &Arc<dyn DbBackend>, zone_id: i64) {
    let settings = SettingsRepo::with_backend(db.clone());
    let _ = settings.set(&cle_preconfigurable(zone_id), "1");
}

/// Préconfigure une zone à partir de son identité.
///
/// Quirks du catalogue embarqué d'abord, réglages validés ensuite — voir
/// [`tune_core::device_preconfig`]. Rend le nombre de réglages réellement
/// posés ; `0` quand l'appareil n'est reconnu par aucune des deux sources, ce
/// qui est le cas le plus fréquent et parfaitement normal.
///
/// Ne fait rien si la zone ne porte pas [`cle_preconfigurable`] : voir l'entête
/// de module, « ce que la marque ne peut pas savoir ».
pub(crate) fn preconfigurer_zone(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    brand: Option<&str>,
    model: Option<&str>,
    output_type: Option<&str>,
) -> usize {
    let settings = SettingsRepo::with_backend(db.clone());
    if settings
        .get(&cle_preconfigurable(zone_id))
        .ok()
        .flatten()
        .is_none()
    {
        return 0;
    }
    let (Some(brand), Some(model)) = (brand, model) else {
        return 0;
    };
    if brand.trim().is_empty() || model.trim().is_empty() {
        return 0;
    }

    let quirks = tune_core::device_catalog::quirks_for(brand, model);
    // Hors ligne, ou site jamais joint : `catalogue_range` rend `None` et seuls
    // les quirks embarqués jouent — exactement le comportement d'avant #3589.
    let catalogue = tune_tested::catalogue_range(&settings);
    let valide = catalogue
        .as_ref()
        .and_then(|c| c.appareil(brand, model, output_type));

    if let Some(dev) = valide {
        let ignorees = tune_core::device_preconfig::cles_ignorees(dev);
        if !ignorees.is_empty() {
            info!(
                zone = zone_id,
                cles = %ignorees.join(","),
                "tune_tested_reglages_non_pris_en_charge"
            );
        }
    }

    let propositions = preconfigurer(&quirks, valide, &|champ| {
        deja_pose(&settings, zone_id, champ)
    });
    if propositions.is_empty() {
        return 0;
    }

    let repo = ZoneRepo::with_backend(db.clone());
    let mut poses = 0usize;
    for p in &propositions {
        let ecrit = match (p.cle, p.valeur) {
            ("dlna_native_flac", Valeur::Drapeau(v)) => repo.update_dlna_native_flac(zone_id, v),
            ("alac_passthrough", Valeur::Drapeau(v)) => repo.update_alac_passthrough(zone_id, v),
            ("aac_passthrough", Valeur::Drapeau(v)) => repo.update_aac_passthrough(zone_id, v),
            ("dlna_lpcm", Valeur::Drapeau(v)) => repo.update_dlna_lpcm(zone_id, v),
            ("dlna_cap_16bit", Valeur::Drapeau(v)) => repo.update_dlna_cap_16bit(zone_id, v),
            ("dlna_wav24", Valeur::Drapeau(v)) => repo.update_dlna_wav24(zone_id, v),
            ("dlna_play_delay_ms", Valeur::Duree(ms)) => {
                repo.update_dlna_play_delay_ms(zone_id, ms)
            }
            ("max_sample_rate", Valeur::Frequence(hz)) => {
                repo.update_max_sample_rate(zone_id, Some(hz))
            }
            ("upnp_silence", Valeur::Drapeau(v)) => {
                let cle = crate::config::cle_silence_upnp(zone_id);
                if v {
                    settings.set(&cle, "true")
                } else {
                    // Même convention que le PATCH : le défaut désarmé est
                    // l'absence de clé, jamais un « false » écrit.
                    settings.delete(&cle)
                }
            }
            ("gain_trim_db", Valeur::Trim(db)) => {
                let cle = format!("zone_{zone_id}_gain_trim_db");
                if db == 0.0 {
                    settings.delete(&cle)
                } else {
                    settings.set(&cle, &db.to_string())
                }
            }
            // Une proposition dont la valeur ne colle pas à sa clé serait un
            // défaut de `device_preconfig`, pas une donnée du site : le dire
            // plutôt que l'écrire de travers.
            (cle, valeur) => {
                warn!(zone = zone_id, cle, ?valeur, "preconfiguration_incoherente");
                continue;
            }
        };
        match ecrit {
            Ok(()) => {
                poses += 1;
                info!(
                    zone = zone_id,
                    cle = p.cle,
                    origine = ?p.origine,
                    "zone_preconfiguree"
                );
            }
            Err(e) => warn!(
                zone = zone_id,
                cle = p.cle,
                error = %e,
                "preconfiguration_non_persistee"
            ),
        }
    }

    if poses > 0 {
        let depuis_le_site = propositions
            .iter()
            .filter(|p| p.origine == Origine::TuneTested)
            .count();
        info!(
            zone = zone_id,
            brand, model, poses, depuis_le_site, "zone_preconfiguree_bilan"
        );
    }
    poses
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::db::migrations;
    use tune_core::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().expect("base mémoire");
        db.init_schema().expect("schéma");
        migrations::run_migrations(&db).expect("migrations");
        Arc::new(db)
    }

    /// Une zone neuve, née avec la provenance ouverte — comme le fait
    /// `POST /zones` et la découverte SSDP.
    fn zone_neuve(db: &Arc<dyn DbBackend>) -> i64 {
        let repo = ZoneRepo::with_backend(db.clone());
        let id = repo
            .create("Salon", Some("dlna"), Some("uuid:test"))
            .expect("zone créée");
        ouvrir_provenance(db, id);
        id
    }

    /// L'enveloppe du site, avec un appareil validé qui existe AUSSI dans le
    /// catalogue embarqué (Eversolo DMP-A8), pour que les deux sources se
    /// rencontrent.
    fn ranger_catalogue(db: &Arc<dyn DbBackend>, reglages: &str) {
        let settings = SettingsRepo::with_backend(db.clone());
        let cat: tune_tested::CatalogueValide = serde_json::from_str(&format!(
            r#"{{"version":1788800000,"count":1,
                 "settings_vocabulary":"tune.renderer.v1",
                 "devices":[{{"brand":"Eversolo","model":"DMP-A8",
                   "output_type":"dlna","settings":{reglages}}}]}}"#
        ))
        .expect("catalogue de test lisible");
        assert!(matches!(
            tune_tested::ranger(&settings, &cat),
            tune_tested::Issue::Range { .. }
        ));
    }

    /// 🔴 Le cœur du volet B, vérifié sur la BASE et non sur une liste de
    /// propositions : un réglage posé à la main garde sa valeur, même quand le
    /// catalogue dit le contraire, et ses voisins non posés sont bien écrits.
    #[test]
    fn la_preconfiguration_n_ecrase_pas_un_reglage_pose_a_la_main() {
        let db = base();
        let id = zone_neuve(&db);
        let repo = ZoneRepo::with_backend(db.clone());

        // L'utilisateur a DÉCOCHÉ `alac_passthrough` — le geste que la valeur
        // seule ne sait pas distinguer du défaut, et que la marque sauve.
        repo.update_alac_passthrough(id, false).unwrap();
        marquer_pose(&db, id, "alac_passthrough");

        ranger_catalogue(
            &db,
            r#"{"alac_passthrough":true,"aac_passthrough":true,"gain_trim_db":-3}"#,
        );

        let poses = preconfigurer_zone(&db, id, Some("Eversolo"), Some("DMP-A8"), Some("dlna"));

        assert!(
            !repo.get_alac_passthrough(id),
            "la préconfiguration a écrasé un réglage posé à la main"
        );
        assert!(
            repo.get_aac_passthrough(id),
            "le voisin non posé aurait dû être préconfiguré"
        );
        let settings = SettingsRepo::with_backend(db.clone());
        assert_eq!(
            settings.get(&format!("zone_{id}_gain_trim_db")).unwrap(),
            Some("-3".to_string()),
            "🔴 le trim rond doit survivre au voyage (il arrive ENTIER du JSON)"
        );
        assert_eq!(poses, 2);
    }

    /// La marque survit au retour au défaut : c'est ce que la convention
    /// « clé supprimée à la désactivation » ne sait pas dire.
    #[test]
    fn la_marque_survit_au_retour_au_defaut() {
        let db = base();
        let id = zone_neuve(&db);
        let settings = SettingsRepo::with_backend(db.clone());

        marquer_pose(&db, id, "gain_trim_db");
        // Retour à 0 : la convention du dépôt SUPPRIME la clé de valeur…
        settings.delete(&format!("zone_{id}_gain_trim_db")).unwrap();
        // …mais la marque, elle, reste.
        assert!(deja_pose(&settings, id, "gain_trim_db"));

        ranger_catalogue(&db, r#"{"gain_trim_db":-3}"#);
        assert_eq!(
            preconfigurer_zone(&db, id, Some("Eversolo"), Some("DMP-A8"), Some("dlna")),
            0,
            "un trim remis à 0 à la main ne doit pas être repréconfiguré"
        );
        assert_eq!(
            settings.get(&format!("zone_{id}_gain_trim_db")).unwrap(),
            None
        );
    }

    /// 🔴 Une zone du PARC EXISTANT ne porte aucune marque : la
    /// préconfiguration doit refuser d'y toucher, sans quoi elle prendrait
    /// chaque réglage de chaque utilisateur pour un défaut.
    #[test]
    fn une_zone_sans_provenance_n_est_jamais_preconfiguree() {
        let db = base();
        let repo = ZoneRepo::with_backend(db.clone());
        let id = repo
            .create("Ancienne", Some("dlna"), Some("uuid:vieux"))
            .expect("zone créée");
        // Pas d'`ouvrir_provenance` : c'est une zone d'avant #3589.
        ranger_catalogue(&db, r#"{"aac_passthrough":true}"#);

        assert_eq!(
            preconfigurer_zone(&db, id, Some("Eversolo"), Some("DMP-A8"), Some("dlna")),
            0
        );
        assert!(!repo.get_aac_passthrough(id));
    }

    /// Hors ligne : aucun catalogue rangé. Seuls les quirks embarqués jouent,
    /// et l'instance se comporte comme avant #3589.
    #[test]
    fn hors_ligne_la_zone_ne_recoit_que_les_quirks_embarques() {
        let db = base();
        let id = zone_neuve(&db);
        let settings = SettingsRepo::with_backend(db.clone());
        assert!(tune_tested::catalogue_range(&settings).is_none());

        // Un modèle du catalogue embarqué qui porte un plafond de fréquence.
        // Choisi dans la donnée, pas supposé : le test le vérifie d'abord.
        let quirks = tune_core::device_catalog::quirks_for("Sonos", "One");
        assert_eq!(
            quirks.max_sample_rate,
            Some(48_000),
            "le catalogue embarqué a changé — ce test ne garde plus rien"
        );

        let poses = preconfigurer_zone(&db, id, Some("Sonos"), Some("One"), Some("dlna"));
        assert_eq!(poses, 1);
        let zone = ZoneRepo::with_backend(db.clone()).get(id).unwrap().unwrap();
        assert_eq!(zone.max_sample_rate, Some(48_000));
    }

    /// Un appareil que ni le catalogue embarqué ni le site ne connaissent : la
    /// zone reste exactement comme la création l'a laissée.
    #[test]
    fn un_appareil_inconnu_ne_pose_rien() {
        let db = base();
        let id = zone_neuve(&db);
        ranger_catalogue(&db, r#"{"aac_passthrough":true}"#);
        assert_eq!(
            preconfigurer_zone(
                &db,
                id,
                Some("Marque Inconnue"),
                Some("XYZ-1"),
                Some("dlna")
            ),
            0
        );
    }
}
