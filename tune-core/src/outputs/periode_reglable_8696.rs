//! #8696 — la période WASAPI exclusive RÉGLABLE.
//!
//! ## Le fait (fils forum 2805, 2821, 2911)
//!
//! Didier, SMSL SU-8 en USB, WASAPI exclusif, période de 10 ms : environ deux
//! réveils du fil de rendu par seconde arrivent ~40 ms après le précédent,
//! toujours 4 périodes pile. À chaque fois, 3 périodes ne sont pas servies et
//! le DAC rejoue ce qu'il a en tampon (« un vinyle qui saute »). L'anneau est
//! plein, MMCSS « Pro Audio » est obtenu, l'EcoQoS est refusé, le fil tourne
//! sur un cœur P : la cause n'est pas dans Tune, elle n'est pas établie.
//!
//! ## L'atténuation
//!
//! La musique n'a pas besoin de 10 ms : le volume, la pause et les changements
//! de piste passent par l'anneau. Ouvrir à une période plus longue (50 ms)
//! fait qu'un trou de 30 à 40 ms tient dans le tampon en cours au lieu d'en
//! sauter trois. Ce n'est PAS le défaut : #4357 rappelle que certains pilotes
//! (HDMI) tiennent mal les périodes inhabituelles. Le réglage est donc opt-in,
//! par zone (`zone_{id}_wasapi_periode_ms`) ou pour tout le serveur par la
//! variable d'environnement [`VARIABLE_D_ENVIRONNEMENT`]. Le réglage de zone
//! l'emporte. Sans l'un ni l'autre : la période par défaut du pilote, comme
//! avant (`periode_exclusive_4357`).
//!
//! Si le pilote refuse la période demandée, l'ouverture reprend à la période
//! par défaut ([`PeriodeRetenue::periodes_a_essayer`]) : un réglage
//! inadapté ne coûte jamais la lecture. Le refus pour simple défaut
//! d'alignement reste traité par la reprise alignée de #2208.
//!
//! Hors FFI, pour être jugé par `cargo test` sur toutes les plateformes.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// La variable d'environnement qui règle la période pour toutes les zones
/// WASAPI exclusives du serveur, en millisecondes (`0` ou `auto` : défaut).
pub const VARIABLE_D_ENVIRONNEMENT: &str = "TUNE_WASAPI_EXCLUSIVE_PERIOD_MS";

/// Les valeurs proposées par l'interface (Réglages de la zone). `None`
/// (« Auto ») n'y figure pas : c'est l'absence de réglage.
pub const CHOIX_MS: [u32; 4] = [10, 20, 50, 100];

/// Bornes d'une période demandée, en ms. Sous 3 ms, aucun pilote USB ne
/// tient ; au-delà de 500 ms, `Initialize(EXCLUSIVE)` refuse la durée
/// (`AUDCLNT_E_BUFFER_SIZE_ERROR`) et la pause se ferait attendre.
pub const PERIODE_MIN_MS: u32 = 3;
pub const PERIODE_MAX_MS: u32 = 500;

/// La clé du réglage de zone. Absente : la zone suit la variable
/// d'environnement, puis le pilote.
pub fn cle_de_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_wasapi_periode_ms")
}

/// Lit une valeur de réglage : un entier de millisecondes, borné à
/// [`PERIODE_MIN_MS`]..=[`PERIODE_MAX_MS`]. `0`, `auto`, vide ou illisible :
/// `None` (la période par défaut du pilote).
pub fn lire_valeur(valeur: &str) -> Option<u32> {
    let v = valeur.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("auto") {
        return None;
    }
    let ms: u32 = v.parse().ok()?;
    if ms == 0 {
        return None;
    }
    Some(ms.clamp(PERIODE_MIN_MS, PERIODE_MAX_MS))
}

/// D'où vient la période demandée — journalisé à l'ouverture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrigineDeLaPeriode {
    Zone,
    VariableDEnvironnement,
}

/// La période demandée pour une ouverture : celle de la zone, sinon celle de
/// la variable d'environnement, sinon aucune.
pub fn periode_demandee_ms(
    reglage_de_zone: Option<&str>,
    variable_d_environnement: Option<&str>,
) -> Option<(u32, OrigineDeLaPeriode)> {
    if let Some(ms) = reglage_de_zone.and_then(lire_valeur) {
        return Some((ms, OrigineDeLaPeriode::Zone));
    }
    variable_d_environnement
        .and_then(lire_valeur)
        .map(|ms| (ms, OrigineDeLaPeriode::VariableDEnvironnement))
}

/// Le réglage de la zone tel qu'il est PERSISTÉ (`None` : Auto). C'est ce que
/// la fiche de zone publie et ce que l'orchestrateur pose sur la sortie
/// locale à chaque lecture.
pub fn reglage_de_zone(db: &Arc<dyn DbBackend>, zone_id: i64) -> Option<u32> {
    SettingsRepo::with_backend(db.clone())
        .get(&cle_de_zone(zone_id))
        .ok()
        .flatten()
        .as_deref()
        .and_then(lire_valeur)
}

/// La période demandée pour une ouverture, réglage de zone (déjà lu, `0` =
/// Auto) puis variable d'environnement du processus.
pub fn periode_demandee_pour_l_ouverture(
    reglage_de_zone_ms: u32,
) -> Option<(u32, OrigineDeLaPeriode)> {
    let zone = (reglage_de_zone_ms > 0).then(|| reglage_de_zone_ms.to_string());
    let env = std::env::var(VARIABLE_D_ENVIRONNEMENT).ok();
    periode_demandee_ms(zone.as_deref(), env.as_deref())
}

/// La période retenue pour `Initialize(EXCLUSIVE)`, et celle de repli.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodeRetenue {
    /// Ce qui est présenté au pilote en premier, en 100 ns.
    pub periode_100ns: i64,
    /// La période de `periode_exclusive_4357` (défaut du pilote), en 100 ns.
    pub automatique_100ns: i64,
    /// Vrai quand `periode_100ns` vient d'un réglage.
    pub demandee: bool,
}

impl PeriodeRetenue {
    /// Les périodes à présenter au pilote, dans l'ordre : la demandée, puis
    /// l'automatique si elle diffère. Un refus de la première (hors défaut
    /// d'alignement) n'arrête pas la lecture.
    pub fn periodes_a_essayer(&self) -> Vec<i64> {
        let mut v = vec![self.periode_100ns];
        if self.automatique_100ns != self.periode_100ns {
            v.push(self.automatique_100ns);
        }
        v
    }
}

/// La période d'ouverture : la règle de #4357 (défaut du pilote, sinon
/// minimale), remplacée par la période demandée quand il y en a une — jamais
/// sous la minimale annoncée par le pilote.
pub fn periode_retenue_100ns(
    par_defaut: i64,
    minimale: i64,
    demandee_ms: Option<u32>,
) -> Result<PeriodeRetenue, String> {
    let automatique = super::periode_exclusive_4357::periode_exclusive_100ns(par_defaut, minimale)?;
    let Some(ms) = demandee_ms.filter(|ms| *ms > 0) else {
        return Ok(PeriodeRetenue {
            periode_100ns: automatique,
            automatique_100ns: automatique,
            demandee: false,
        });
    };
    let demandee = i64::from(ms.clamp(PERIODE_MIN_MS, PERIODE_MAX_MS)) * 10_000;
    let demandee = if minimale > 0 {
        demandee.max(minimale)
    } else {
        demandee
    };
    Ok(PeriodeRetenue {
        periode_100ns: demandee,
        automatique_100ns: automatique,
        demandee: demandee != automatique,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sans_reglage_la_periode_reste_celle_du_pilote() {
        // Le SMSL SU-8 de Didier : 10 ms par défaut, 3 ms minimum.
        let p = periode_retenue_100ns(100_000, 30_000, None).unwrap();
        assert_eq!(p.periode_100ns, 100_000);
        assert!(!p.demandee);
        assert_eq!(p.periodes_a_essayer(), vec![100_000]);
    }

    #[test]
    fn cinquante_ms_demandees_ouvrent_a_cinquante_puis_replient_au_defaut() {
        let p = periode_retenue_100ns(100_000, 30_000, Some(50)).unwrap();
        assert_eq!(p.periode_100ns, 500_000);
        assert!(p.demandee);
        assert_eq!(p.periodes_a_essayer(), vec![500_000, 100_000]);
    }

    #[test]
    fn la_periode_demandee_ne_descend_jamais_sous_la_minimale_du_pilote() {
        let p = periode_retenue_100ns(100_000, 80_000, Some(5)).unwrap();
        assert_eq!(p.periode_100ns, 80_000);
    }

    #[test]
    fn demander_la_periode_par_defaut_n_ajoute_aucun_repli() {
        let p = periode_retenue_100ns(100_000, 30_000, Some(10)).unwrap();
        assert_eq!(p.periodes_a_essayer(), vec![100_000]);
        assert!(!p.demandee);
    }

    #[test]
    fn un_pilote_sans_periode_reste_une_erreur_meme_avec_un_reglage() {
        assert!(periode_retenue_100ns(0, 0, Some(50)).is_err());
    }

    #[test]
    fn les_valeurs_de_reglage_sont_lues_et_bornees() {
        assert_eq!(lire_valeur("50"), Some(50));
        assert_eq!(lire_valeur(" 20 "), Some(20));
        assert_eq!(lire_valeur("auto"), None);
        assert_eq!(lire_valeur("AUTO"), None);
        assert_eq!(lire_valeur("0"), None);
        assert_eq!(lire_valeur(""), None);
        assert_eq!(lire_valeur("cinquante"), None);
        assert_eq!(lire_valeur("-5"), None);
        assert_eq!(lire_valeur("1"), Some(PERIODE_MIN_MS));
        assert_eq!(lire_valeur("5000"), Some(PERIODE_MAX_MS));
    }

    #[test]
    fn le_reglage_de_zone_l_emporte_sur_la_variable_d_environnement() {
        assert_eq!(
            periode_demandee_ms(Some("100"), Some("50")),
            Some((100, OrigineDeLaPeriode::Zone))
        );
        assert_eq!(
            periode_demandee_ms(None, Some("50")),
            Some((50, OrigineDeLaPeriode::VariableDEnvironnement))
        );
        // « Auto » sur la zone : la variable d'environnement vaut.
        assert_eq!(
            periode_demandee_ms(Some("auto"), Some("50")),
            Some((50, OrigineDeLaPeriode::VariableDEnvironnement))
        );
        assert_eq!(periode_demandee_ms(None, None), None);
        assert_eq!(periode_demandee_ms(Some("0"), Some("")), None);
    }

    #[test]
    fn les_choix_de_l_interface_sont_dans_les_bornes() {
        for ms in CHOIX_MS {
            assert_eq!(lire_valeur(&ms.to_string()), Some(ms));
        }
        assert_eq!(cle_de_zone(7), "zone_7_wasapi_periode_ms");
    }

    /// Le branchement : l'ouverture essaie les périodes dans l'ordre, rouvre
    /// un client neuf avant le repli, et ne réessaie pas un endpoint tenu
    /// ailleurs. Comparé sans blancs.
    #[test]
    fn l_ouverture_essaie_la_periode_reglee_puis_celle_du_pilote() {
        let source: String = include_str!("wasapi_exclusive.rs")
            .split_whitespace()
            .collect();
        let demandee = source.find("periode_demandee_pour_l_ouverture(reglage_periode_ms)");
        let essais = source.find("retenue.periodes_a_essayer().into_iter().enumerate()");
        let client_neuf = source.find("ifessai>0{");
        let init = source.find("matchinitialiser_a_la_periode(");
        let occupe = source.find("hr.is_some_and(|hr|message_peripherique_occupe(hr).is_some())");
        for (nom, p) in [
            ("période demandée", demandee),
            ("essais", essais),
            ("client neuf", client_neuf),
            ("initialisation", init),
            ("endpoint occupé", occupe),
        ] {
            assert!(p.is_some(), "absent de wasapi_exclusive.rs : {nom}");
        }
        assert!(demandee < essais && essais < client_neuf && client_neuf < init && init < occupe);
    }
}
