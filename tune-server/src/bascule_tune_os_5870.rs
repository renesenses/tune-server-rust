//! #5870 — bascule d'office, sur Tune OS, des zones locales posées sur le
//! greffon ALSA `default` vers le `hw:` du DAC USB (décision de Bertrand du
//! 12/10/2026).
//!
//! Sur Tune OS, `default` est `plug` → `dmix` de la carte 0, à cadence fixe
//! (48 kHz) : un DAC USB branché reste bloqué à 48 kHz quelle que soit la
//! piste. `ca801f8f1` fait viser le `hw:` aux installations NEUVES ; une
//! installation existante garde sa zone sur `default`. Ce module la déplace,
//! une fois.
//!
//! Garde-fous, tous nécessaires :
//! - **Tune OS seulement** (`/etc/tune-appliance` ou `TUNE_APPLIANCE=1`, la
//!   même détection que `routes::appliance::is_appliance`). Sur un bureau
//!   Linux, le `hw:` est souvent tenu par PipeWire : on n'y touche pas.
//! - **Un seul DAC USB matériel** présent dans le parc : zéro ou deux, rien ne
//!   bouge (un choix entre deux DAC n'appartient qu'à l'utilisateur).
//! - **Jamais une zone choisie à la main.** La provenance n'est enregistrée
//!   nulle part. Elle se déduit de l'étiquette : seule la création automatique
//!   pose l'étiquette générique (« This Computer » et ses traductions,
//!   `config::nom_de_zone_locale`). Une zone sur `default` qui porte un autre
//!   nom a été créée ou renommée par l'utilisateur : elle reste où elle est.
//!   Limite connue : une zone créée à la main sur « Default Audio Device » ET
//!   nommée exactement « This Computer » serait prise pour automatique.
//! - Zone **masquée** (supprimée) : jamais ressuscitée ni déplacée.
//! - Le DAC a **déjà une zone** (visible ou masquée) : rien ne bouge, la
//!   colonne `output_device_id` est unique et l'utilisateur a déjà la sienne.
//! - **Une seule fois** : le marqueur [`MARQUEUR`] est posé dès que la règle
//!   a été évaluée sur Tune OS avec un parc non vide, quel qu'en soit le
//!   verdict. Un DAC branché des mois plus tard ne déplace donc pas une zone
//!   dans le dos de l'utilisateur.
//!
//! La décision est une fonction pure ([`decider`]) ; l'application
//! ([`appliquer`]) ne fait qu'écrire ce qu'elle autorise et le journaliser.

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::pseudo_peripherique_alsa::est_un_puits;

/// Réglage-marqueur : présent = la migration a été évaluée, ne plus la jouer.
pub(crate) const MARQUEUR: &str = "migration_5870_zones_tune_os_vers_hw";

/// Une sortie énumérée, telle que la règle la voit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Sortie<'a> {
    pub nom: &'a str,
    pub endpoint_id: &'a str,
    pub est_le_defaut: bool,
}

/// Une zone locale existante, telle que la règle la voit.
#[derive(Debug, Clone)]
pub(crate) struct ZoneExistante {
    pub id: i64,
    pub nom: String,
    pub output_device_id: String,
    pub masquee: bool,
}

/// Une bascule autorisée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bascule {
    pub zone_id: i64,
    pub ancien_device_id: String,
    pub nouveau_device_id: String,
    pub endpoint_id: String,
}

/// Le verdict de la règle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Hors Tune OS, ou parc vide : rien n'est décidé, aucun marqueur.
    NonEvaluee,
    /// Évaluée, rien à déplacer ; le motif est journalisé.
    Rien(&'static str),
    /// Évaluée : ces zones changent de sortie.
    Basculer(Vec<Bascule>),
}

fn est_alsa(endpoint_id: &str) -> bool {
    endpoint_id
        .split_once(':')
        .is_some_and(|(hote, _)| hote.eq_ignore_ascii_case("alsa"))
}

/// Le DAC USB matériel, s'il y en a exactement un.
fn dac_usb_unique<'a>(sorties: &[Sortie<'a>]) -> Result<Sortie<'a>, &'static str> {
    let mut dacs = sorties.iter().filter(|s| {
        est_alsa(s.endpoint_id)
            && !crate::startup::defaut_alsa_logiciel(s.endpoint_id)
            && !est_un_puits(s.endpoint_id, s.nom)
            && s.nom.to_lowercase().contains("usb")
    });
    match (dacs.next(), dacs.next()) {
        (Some(dac), None) => Ok(*dac),
        (None, _) => Err("aucun_dac_usb"),
        (Some(_), Some(_)) => Err("plusieurs_dac_usb"),
    }
}

/// La règle, sans base ni matériel.
pub(crate) fn decider(
    sur_tune_os: bool,
    sorties: &[Sortie<'_>],
    zones: &[ZoneExistante],
) -> Verdict {
    if !sur_tune_os || sorties.is_empty() {
        return Verdict::NonEvaluee;
    }
    let Some(defaut) = sorties
        .iter()
        .find(|s| s.est_le_defaut && crate::startup::defaut_alsa_logiciel(s.endpoint_id))
    else {
        return Verdict::Rien("sortie_systeme_non_logicielle");
    };
    let ancien = format!("local:{}", defaut.nom);
    let candidates: Vec<&ZoneExistante> = zones
        .iter()
        .filter(|z| z.output_device_id == ancien && !z.masquee)
        .collect();
    if candidates.is_empty() {
        return Verdict::Rien("aucune_zone_sur_default");
    }
    let dac = match dac_usb_unique(sorties) {
        Ok(dac) => dac,
        Err(motif) => return Verdict::Rien(motif),
    };
    let nouveau = format!("local:{}", dac.nom);
    if zones.iter().any(|z| z.output_device_id == nouveau) {
        return Verdict::Rien("le_dac_a_deja_une_zone");
    }
    let automatiques: Vec<&&ZoneExistante> = candidates
        .iter()
        .filter(|z| tune_core::config::est_etiquette_locale_generique(&z.nom))
        .collect();
    // Une seule zone peut porter le DAC (colonne unique) : avec deux zones
    // automatiques sur `default`, laquelle garder n'est pas à deviner.
    match automatiques.as_slice() {
        [] => Verdict::Rien("zone_choisie_a_la_main"),
        [zone] => Verdict::Basculer(vec![Bascule {
            zone_id: zone.id,
            ancien_device_id: ancien,
            nouveau_device_id: nouveau,
            endpoint_id: dac.endpoint_id.to_string(),
        }]),
        _ => Verdict::Rien("plusieurs_zones_automatiques_sur_default"),
    }
}

/// Lit les zones locales existantes (masquées comprises).
fn zones_locales(zone_repo: &ZoneRepo) -> Result<Vec<ZoneExistante>, String> {
    Ok(zone_repo
        .sorties_locales_et_leur_identite()?
        .into_iter()
        .map(|z| ZoneExistante {
            id: z.id,
            nom: zone_repo
                .get(z.id)
                .ok()
                .flatten()
                .map(|zone| zone.name)
                .unwrap_or_default(),
            output_device_id: z.output_device_id,
            masquee: z.masquee,
        })
        .collect())
}

/// Joue la migration si elle ne l'a pas encore été. Rend le verdict, pour les
/// tests et le journal.
pub(crate) fn appliquer(
    zone_repo: &ZoneRepo,
    settings: &SettingsRepo,
    sur_tune_os: bool,
    sorties: &[Sortie<'_>],
) -> Verdict {
    if !sur_tune_os || sorties.is_empty() {
        return Verdict::NonEvaluee;
    }
    if settings.get(MARQUEUR).ok().flatten().is_some() {
        return Verdict::NonEvaluee;
    }
    let zones = match zones_locales(zone_repo) {
        Ok(z) => z,
        Err(e) => {
            // Base illisible : on retentera au prochain démarrage.
            tracing::warn!(error = %e, "bascule_tune_os_5870_zones_illisibles");
            return Verdict::NonEvaluee;
        }
    };
    let verdict = decider(sur_tune_os, sorties, &zones);
    match &verdict {
        Verdict::NonEvaluee => return verdict,
        Verdict::Rien(motif) => {
            tracing::info!(motif, "bascule_tune_os_5870_rien_a_faire");
        }
        Verdict::Basculer(bascules) => {
            for b in bascules {
                let ecrit = zone_repo
                    .update_output_device(b.zone_id, &b.nouveau_device_id)
                    .and_then(|()| zone_repo.update_output_endpoint_id(b.zone_id, &b.endpoint_id));
                match ecrit {
                    Ok(()) => tracing::info!(
                        zone_id = b.zone_id,
                        avant = %b.ancien_device_id,
                        apres = %b.nouveau_device_id,
                        endpoint_id = %b.endpoint_id,
                        "zone_tune_os_basculee_vers_le_dac"
                    ),
                    Err(e) => {
                        tracing::warn!(
                            zone_id = b.zone_id,
                            error = %e,
                            "zone_tune_os_bascule_echouee"
                        );
                        // Pas de marqueur : la prochaine tentative rejouera.
                        return Verdict::NonEvaluee;
                    }
                }
            }
        }
    }
    if let Err(e) = settings.set(MARQUEUR, "1") {
        tracing::warn!(error = %e, "bascule_tune_os_5870_marqueur_non_pose");
    }
    verdict
}

/// #6774 — après une bascule, ramène l'état de lecture EN MÉMOIRE de chaque
/// zone basculée au niveau de la base, à la baisse seulement
/// (`reglages_de_volume::aligner_la_lecture`). Sans effet pour tout autre
/// verdict.
pub(crate) async fn realigner_la_lecture(state: &crate::state::AppState, verdict: &Verdict) {
    if let Verdict::Basculer(bascules) = verdict {
        for b in bascules {
            tune_core::audio::reglages_de_volume::aligner_la_lecture(
                &state.backend,
                &state.playback,
                b.zone_id,
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::backend::DbBackend;

    fn base() -> Arc<dyn DbBackend> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    const DEFAUT: Sortie<'static> = Sortie {
        nom: "Default Audio Device",
        endpoint_id: "alsa:default",
        est_le_defaut: true,
    };
    const DAC_Z8: Sortie<'static> = Sortie {
        nom: "Eversolo DAC-Z8, USB Audio",
        endpoint_id: "alsa:hw:CARD=Z8,DEV=0",
        est_le_defaut: false,
    };
    const AUTRE_DAC: Sortie<'static> = Sortie {
        nom: "Topping E30, USB Audio",
        endpoint_id: "alsa:hw:CARD=E30,DEV=0",
        est_le_defaut: false,
    };
    const HDMI: Sortie<'static> = Sortie {
        nom: "vc4-hdmi-0, MAI PCM i2s-hifi-0",
        endpoint_id: "alsa:hw:CARD=vc4hdmi0,DEV=0",
        est_le_defaut: false,
    };

    /// Le parc de GgB (fil 2163) : `default`, le HDMI, le DAC-Z8.
    fn parc_ggb() -> Vec<Sortie<'static>> {
        vec![DEFAUT, HDMI, DAC_Z8]
    }

    fn repos(db: &Arc<dyn DbBackend>) -> (ZoneRepo, SettingsRepo) {
        (
            ZoneRepo::with_backend(db.clone()),
            SettingsRepo::with_backend(db.clone()),
        )
    }

    fn zone_de(repo: &ZoneRepo, nom: &str, device: &str) -> i64 {
        repo.get_or_create(nom, Some("local"), device).unwrap().0
    }

    /// Tune OS, un seul DAC USB, la zone automatique « This Computer » sur
    /// `default` : elle passe sur le `hw:` du DAC, et le marqueur est posé.
    #[test]
    fn sur_tune_os_avec_un_seul_dac_la_zone_automatique_bascule() {
        let db = base();
        let (zones, settings) = repos(&db);
        let id = zone_de(&zones, "This Computer", "local:Default Audio Device");

        let verdict = appliquer(&zones, &settings, true, &parc_ggb());

        assert_eq!(
            verdict,
            Verdict::Basculer(vec![Bascule {
                zone_id: id,
                ancien_device_id: "local:Default Audio Device".into(),
                nouveau_device_id: "local:Eversolo DAC-Z8, USB Audio".into(),
                endpoint_id: "alsa:hw:CARD=Z8,DEV=0".into(),
            }])
        );
        let zone = zones.get(id).unwrap().unwrap();
        assert_eq!(
            zone.output_device_id.as_deref(),
            Some("local:Eversolo DAC-Z8, USB Audio")
        );
        assert_eq!(
            zones
                .endpoint_id_de_la_sortie("local:Eversolo DAC-Z8, USB Audio")
                .as_deref(),
            Some("alsa:hw:CARD=Z8,DEV=0")
        );
        assert!(settings.get(MARQUEUR).unwrap().is_some());
    }

    /// Une étiquette traduite (« Cet ordinateur ») est aussi automatique.
    #[test]
    fn l_etiquette_traduite_compte_comme_automatique() {
        let db = base();
        let (zones, settings) = repos(&db);
        let id = zone_de(&zones, "Cet ordinateur", "local:Default Audio Device");
        assert!(matches!(
            appliquer(&zones, &settings, true, &parc_ggb()),
            Verdict::Basculer(_)
        ));
        assert_eq!(
            zones.get(id).unwrap().unwrap().output_device_id.as_deref(),
            Some("local:Eversolo DAC-Z8, USB Audio")
        );
    }

    /// Hors Tune OS (bureau Linux, PipeWire) : rien ne bouge, aucun marqueur.
    #[test]
    fn hors_tune_os_rien_ne_bouge() {
        let db = base();
        let (zones, settings) = repos(&db);
        let id = zone_de(&zones, "This Computer", "local:Default Audio Device");
        assert_eq!(
            appliquer(&zones, &settings, false, &parc_ggb()),
            Verdict::NonEvaluee
        );
        assert_eq!(
            zones.get(id).unwrap().unwrap().output_device_id.as_deref(),
            Some("local:Default Audio Device")
        );
        assert!(settings.get(MARQUEUR).unwrap().is_none());
    }

    /// Deux DAC USB : le choix n'est pas à deviner.
    #[test]
    fn avec_deux_dac_usb_rien_ne_bouge() {
        let db = base();
        let (zones, settings) = repos(&db);
        let id = zone_de(&zones, "This Computer", "local:Default Audio Device");
        let parc = [DEFAUT, HDMI, DAC_Z8, AUTRE_DAC];
        assert_eq!(
            appliquer(&zones, &settings, true, &parc),
            Verdict::Rien("plusieurs_dac_usb")
        );
        assert_eq!(
            zones.get(id).unwrap().unwrap().output_device_id.as_deref(),
            Some("local:Default Audio Device")
        );
    }

    /// Aucun DAC USB (HDMI seul) : rien ne bouge.
    #[test]
    fn sans_dac_usb_rien_ne_bouge() {
        let db = base();
        let (zones, settings) = repos(&db);
        zone_de(&zones, "This Computer", "local:Default Audio Device");
        assert_eq!(
            appliquer(&zones, &settings, true, &[DEFAUT, HDMI]),
            Verdict::Rien("aucun_dac_usb")
        );
    }

    /// Une zone sur `default` que l'utilisateur a nommée lui-même a été
    /// choisie à la main : elle n'est jamais déplacée.
    #[test]
    fn une_zone_choisie_a_la_main_n_est_jamais_deplacee() {
        let db = base();
        let (zones, settings) = repos(&db);
        let id = zone_de(&zones, "Salon (dmix)", "local:Default Audio Device");
        assert_eq!(
            appliquer(&zones, &settings, true, &parc_ggb()),
            Verdict::Rien("zone_choisie_a_la_main")
        );
        assert_eq!(
            zones.get(id).unwrap().unwrap().output_device_id.as_deref(),
            Some("local:Default Audio Device")
        );
    }

    /// Le DAC a déjà sa zone : on ne la double pas.
    #[test]
    fn le_dac_qui_a_deja_une_zone_n_en_recoit_pas_une_seconde() {
        let db = base();
        let (zones, settings) = repos(&db);
        let id = zone_de(&zones, "This Computer", "local:Default Audio Device");
        zone_de(
            &zones,
            "Eversolo DAC-Z8",
            "local:Eversolo DAC-Z8, USB Audio",
        );
        assert_eq!(
            appliquer(&zones, &settings, true, &parc_ggb()),
            Verdict::Rien("le_dac_a_deja_une_zone")
        );
        assert_eq!(
            zones.get(id).unwrap().unwrap().output_device_id.as_deref(),
            Some("local:Default Audio Device")
        );
    }

    /// Une seule fois : au second démarrage, même une zone revenue sur
    /// `default` n'est plus déplacée.
    #[test]
    fn la_bascule_n_est_pas_rejouee_au_second_demarrage() {
        let db = base();
        let (zones, settings) = repos(&db);
        let id = zone_de(&zones, "This Computer", "local:Default Audio Device");
        assert!(matches!(
            appliquer(&zones, &settings, true, &parc_ggb()),
            Verdict::Basculer(_)
        ));
        // L'utilisateur remet sa zone sur `default` à la main.
        zones
            .update_output_device(id, "local:Default Audio Device")
            .unwrap();
        assert_eq!(
            appliquer(&zones, &settings, true, &parc_ggb()),
            Verdict::NonEvaluee
        );
        assert_eq!(
            zones.get(id).unwrap().unwrap().output_device_id.as_deref(),
            Some("local:Default Audio Device")
        );
    }

    /// Évaluée sans rien à faire, la règle pose quand même son marqueur : un
    /// DAC branché plus tard ne déplace pas la zone dans le dos de
    /// l'utilisateur.
    #[test]
    fn un_verdict_negatif_pose_aussi_le_marqueur() {
        let db = base();
        let (zones, settings) = repos(&db);
        let id = zone_de(&zones, "This Computer", "local:Default Audio Device");
        assert_eq!(
            appliquer(&zones, &settings, true, &[DEFAUT, HDMI]),
            Verdict::Rien("aucun_dac_usb")
        );
        assert_eq!(
            appliquer(&zones, &settings, true, &parc_ggb()),
            Verdict::NonEvaluee
        );
        assert_eq!(
            zones.get(id).unwrap().unwrap().output_device_id.as_deref(),
            Some("local:Default Audio Device")
        );
    }

    /// Une zone supprimée (masquée) n'est ni déplacée ni ressuscitée.
    #[test]
    fn une_zone_masquee_n_est_pas_deplacee() {
        let zones = vec![ZoneExistante {
            id: 3,
            nom: "This Computer".into(),
            output_device_id: "local:Default Audio Device".into(),
            masquee: true,
        }];
        assert_eq!(
            decider(true, &parc_ggb(), &zones),
            Verdict::Rien("aucune_zone_sur_default")
        );
    }

    /// #6774 — point laissé ouvert par #6797 : la bascule désarme PURE ou le
    /// volume fixe EN BASE, mais l'état de lecture en mémoire (chargé avant,
    /// 100 % pour un volume fixe) restait à 100 %. Il est ramené au niveau de
    /// la base — 30 % du curseur au plus sans repère (décision du 23/10) — à
    /// la baisse seulement, et rien n'est envoyé à l'appareil.
    #[tokio::test]
    async fn la_bascule_ramene_l_etat_de_lecture_a_la_baisse_seulement() {
        let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let (zones, settings) = repos(&state.backend);
        let id = zone_de(&zones, "This Computer", "local:Default Audio Device");
        zones.update_volume(id, 100.0).unwrap();
        zones.update_fixed_volume(id, true).unwrap();
        state.playback.set_volume(id, 1.0).await;
        let verdict = appliquer(&zones, &settings, true, &parc_ggb());
        assert!(matches!(verdict, Verdict::Basculer(_)), "{verdict:?}");
        assert!(!zones.get(id).unwrap().unwrap().fixed_volume);

        realigner_la_lecture(&state, &verdict).await;

        let base = zones.get(id).unwrap().unwrap().volume / 100.0;
        let memoire = state.playback.get_state(id).await.volume;
        assert!(
            memoire <= base + 1e-12,
            "#6774 : l'état de lecture reste à {memoire} au-dessus de la base {base} après la bascule"
        );
        assert!(
            (base - tune_core::audio::volume_scale::gain_perceptif(0.30)).abs() < 1e-12,
            "sortie locale sans repère : 30 % du curseur db60, reçu {base}"
        );

        // Jamais une hausse : une mémoire plus basse que la base reste.
        state.playback.set_volume(id, 0.001).await;
        realigner_la_lecture(&state, &verdict).await;
        assert_eq!(state.playback.get_state(id).await.volume, 0.001);
    }

    /// Le démarrage appelle bien le réalignement juste après la bascule.
    #[test]
    fn le_demarrage_realigne_la_lecture_apres_la_bascule() {
        let source = include_str!("startup.rs");
        let bascule = source
            .find("crate::bascule_tune_os_5870::appliquer(")
            .expect("la bascule au démarrage");
        let realigne = source
            .find("crate::bascule_tune_os_5870::realigner_la_lecture(state, &verdict)")
            .expect("#6774 : le démarrage ne réaligne pas la lecture après la bascule");
        assert!(realigne > bascule);
    }
}
