//! #7922 — témoins du compte sans erreur et du contenu proposé.

use super::*;
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use std::sync::Arc;

const ZONE: i64 = 79_22;
const APPAREIL: &str = "uuid:123e4567-e89b-12d3-a456-426614174000";

fn settings() -> SettingsRepo {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    SettingsRepo::with_backend(db)
}

fn flac(rate: u32, bits: u16) -> FormatObserve {
    FormatObserve {
        codec: "flac".into(),
        sample_rate: Some(rate),
        bit_depth: Some(bits),
        dsd_mode: None,
    }
}

fn piste(device_id: &str, n: u64, ms: u64, format: Option<FormatObserve>) -> LectureRendue<'_> {
    LectureRendue {
        device_id,
        protocole_brut: "dlna",
        identite: Some(("Test Audio", "DAC 7922")),
        millisecondes: ms,
        format,
        gapless: false,
        cle_piste: (n, 0),
    }
}

/// Une heure de lecture en 15 pistes de 4 minutes.
fn une_heure(settings: &SettingsRepo, device_id: &str) {
    for n in 0..15 {
        noter_lecture(
            settings,
            ZONE,
            piste(device_id, n, 240_000, Some(flac(96_000, 24))),
        )
        .unwrap();
    }
}

#[test]
fn tune_tested_7922_le_compte_cumule_et_persiste_les_pistes_rendues() {
    let settings = settings();
    une_heure(&settings, APPAREIL);
    let etat = etat(&settings, ZONE, Some(APPAREIL));
    assert_eq!(etat.seconds_without_error, 3600);
    assert_eq!(etat.remaining_seconds, 0);
    assert!(etat.eligible, "une heure sans erreur ouvre la proposition");
    // Relu depuis les réglages, sans état en mémoire.
    assert_eq!(
        lire(&settings, ZONE).unwrap().millisecondes_sans_erreur,
        SEUIL_MS
    );
}

#[test]
fn tune_tested_7922_une_meme_fin_vue_deux_fois_ne_compte_qu_une_fois() {
    let settings = settings();
    noter_lecture(&settings, ZONE, piste(APPAREIL, 1, 240_000, None)).unwrap();
    noter_lecture(&settings, ZONE, piste(APPAREIL, 1, 240_000, None)).unwrap();
    assert_eq!(
        etat(&settings, ZONE, Some(APPAREIL)).seconds_without_error,
        240
    );
}

#[test]
fn tune_tested_7922_une_erreur_remet_le_compte_a_zero() {
    let settings = settings();
    une_heure(&settings, APPAREIL);
    noter_erreur(&settings, ZONE, APPAREIL).unwrap();
    let etat = etat(&settings, ZONE, Some(APPAREIL));
    assert_eq!(
        etat.seconds_without_error, 0,
        "une erreur de transport ou une coupure doit remettre le compte à zéro"
    );
    assert!(!etat.eligible);
    assert_eq!(etat.reason, Some("below_threshold"));
    assert_eq!(etat.remaining_seconds, 3600);
}

#[test]
fn tune_tested_7922_un_changement_d_appareil_remet_le_compte_a_zero() {
    let settings = settings();
    une_heure(&settings, APPAREIL);
    let autre = "uuid:autre-appareil";
    assert_eq!(
        etat(&settings, ZONE, Some(autre)).seconds_without_error,
        0,
        "le compte d'un autre appareil ne vaut pas pour le nouvel appareil"
    );
    noter_lecture(&settings, ZONE, piste(autre, 99, 60_000, None)).unwrap();
    assert_eq!(etat(&settings, ZONE, Some(autre)).seconds_without_error, 60);
    assert_eq!(
        etat(&settings, ZONE, Some(APPAREIL)).seconds_without_error,
        0,
        "revenir à l'ancien appareil ne rend pas son ancien compte"
    );
}

#[test]
fn tune_tested_7922_sous_le_seuil_la_proposition_est_refusee_avec_le_temps_restant() {
    let settings = settings();
    noter_lecture(&settings, ZONE, piste(APPAREIL, 1, 1_800_500, None)).unwrap();
    let etat = etat(&settings, ZONE, Some(APPAREIL));
    assert!(!etat.eligible);
    assert_eq!(etat.reason, Some("below_threshold"));
    assert_eq!(etat.seconds_without_error, 1800);
    assert_eq!(etat.remaining_seconds, 1800);
    assert!(etat.preview.is_some(), "l'aperçu se montre avant le seuil");
}

#[test]
fn tune_tested_7922_appareil_sans_identite_n_est_pas_proposable() {
    let settings = settings();
    for n in 0..15 {
        let mut lecture = piste(APPAREIL, n, 240_000, None);
        lecture.identite = Some(("192.0.2.4", "Salon de Bertrand"));
        noter_lecture(&settings, ZONE, lecture).unwrap();
    }
    let etat = etat(&settings, ZONE, Some(APPAREIL));
    assert_eq!(etat.reason, Some("device_unidentified"));
    assert!(etat.preview.is_none());
    assert_eq!(super::etat(&settings, ZONE, None).reason, Some("no_device"));
}

#[test]
fn tune_tested_7922_le_contenu_ne_porte_ni_adresse_ni_nom_ni_identifiant() {
    let settings = settings();
    settings
        .set(&format!("zone_{ZONE}_name"), "PRIVATE kitchen")
        .unwrap();
    une_heure(&settings, APPAREIL);
    let mut gapless = piste(APPAREIL, 50, 1000, Some(flac(44_100, 16)));
    gapless.gapless = true;
    noter_lecture(&settings, ZONE, gapless).unwrap();
    let apercu = etat(&settings, ZONE, Some(APPAREIL)).preview.unwrap();
    assert_eq!(apercu.manufacturer, "Test Audio");
    assert_eq!(apercu.model, "DAC 7922");
    assert_eq!(apercu.output_type, "dlna");
    assert!(apercu.gapless_observed);
    assert_eq!(
        apercu.formats.len(),
        2,
        "formats distincts réellement joués"
    );
    assert_eq!(apercu.formats[0].sample_rate, Some(96_000));
    assert_eq!(apercu.formats[0].bit_depth, Some(24));
    assert_eq!(apercu.tune_version, crate::version());
    assert_eq!(apercu.platform, std::env::consts::OS);
    let json = serde_json::to_value(&apercu).unwrap();
    let mut cles: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
    cles.sort();
    assert_eq!(
        cles,
        [
            "commercial_name",
            "device_key",
            "error_free_seconds",
            "formats",
            "gapless_observed",
            "manufacturer",
            "model",
            "output_type",
            "platform",
            "schema_version",
            "tune_version"
        ]
    );
    let texte = json.to_string();
    for interdit in [
        "PRIVATE kitchen",
        APPAREIL,
        "123e4567",
        "zone",
        "ip",
        "path",
        "ssid",
    ] {
        assert!(
            !texte.contains(interdit),
            "valeur interdite dans la proposition : {interdit} — {texte}"
        );
    }
}

#[test]
fn tune_tested_7922_une_proposition_par_appareil_et_par_installation() {
    let settings = settings();
    une_heure(&settings, APPAREIL);
    let cle = etat(&settings, ZONE, Some(APPAREIL))
        .preview
        .unwrap()
        .device_key;
    assert!(!etat(&settings, ZONE, Some(APPAREIL)).already_proposed);
    marquer_proposee(&settings, &cle).unwrap();
    marquer_proposee(&settings, &cle).unwrap();
    assert!(etat(&settings, ZONE, Some(APPAREIL)).already_proposed);
    assert_eq!(envoyees(&settings).len(), 1);
    // La clé ne dépend que du modèle et de la sortie, pas de l'identifiant
    // de l'appareil : deux exemplaires du même modèle ont la même clé.
    assert_eq!(cle, cle_appareil("test audio", "dac 7922", "dlna"));
    assert!(!cle.contains("123e4567"));
}
