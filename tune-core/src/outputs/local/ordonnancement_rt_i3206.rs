//! #3206 — la sentinelle du fil de rendu : demander UNE fois, publier l'état.
//!
//! La décision de priorité et le verdict du noyau sont éprouvés dans
//! `crate::audio::ordonnancement_rt` (porte `test` de la CI, sans cpal). Ce
//! fichier tient ce qui vit sous `local-audio` : la sentinelle capturée par les
//! fermetures de rendu, et le champ `realtime` de `LocalBackendStatus` que
//! lisent `/system/diagnostics` et le chemin du signal.
//!
//! ⚠️ Ce qu'aucune épreuve ne peut établir ici : que le fil de cpal a bien été
//! promu sur un DAC réel. La machine de compilation n'a pas de carte son.

use super::*;

/// Les deux épreuves écrivent le même état global : une à la fois.
static UNE_A_LA_FOIS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// La sentinelle demande à sa première période et publie l'état ; la seconde
/// période ne redemande rien.
#[test]
fn la_sentinelle_demande_une_fois_et_publie_l_etat() {
    let _seul = UNE_A_LA_FOIS.lock().unwrap_or_else(|e| e.into_inner());
    let (issue_publiee, statut) = std::thread::spawn(|| {
        let mut promotion = PromotionDuFilDeRendu::nouvelle();
        assert!(!promotion.faite);
        promotion.a_la_premiere_periode();
        assert!(promotion.faite, "la première période arme la sentinelle");
        let issue = OBSERVED_REALTIME.read().unwrap().clone();
        // Seconde période : rien ne change, même si l'état est écrasé entre-temps.
        note_realtime_scheduling(
            crate::audio::ordonnancement_rt::OrdonnancementTempsReel::SansObjet,
        );
        promotion.a_la_premiere_periode();
        let apres = OBSERVED_REALTIME.read().unwrap().clone();
        assert_eq!(
            apres,
            Some(crate::audio::ordonnancement_rt::OrdonnancementTempsReel::SansObjet),
            "la seconde période ne doit pas redemander"
        );
        (issue, active_backend_status("auto"))
    })
    .join()
    .expect("la sentinelle ne panique jamais");

    let issue = issue_publiee.expect("la première période publie un état");
    #[cfg(target_os = "linux")]
    assert_ne!(
        issue,
        crate::audio::ordonnancement_rt::OrdonnancementTempsReel::SansObjet,
        "sous Linux la demande est toujours posée au noyau"
    );
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        issue,
        crate::audio::ordonnancement_rt::OrdonnancementTempsReel::SansObjet
    );
    assert!(
        statut.realtime.is_some(),
        "LocalBackendStatus.realtime doit porter l'état"
    );
}

/// Le champ sort dans le JSON que lisent les diagnostics, avec le discriminant.
#[test]
fn l_etat_sort_dans_le_statut_serialise() {
    let _seul = UNE_A_LA_FOIS.lock().unwrap_or_else(|e| e.into_inner());
    use crate::audio::ordonnancement_rt::OrdonnancementTempsReel;
    note_realtime_scheduling(OrdonnancementTempsReel::Refuse {
        priority: 70,
        rlimit_rtprio: Some(0),
        cause: "Operation not permitted".into(),
    });
    let json = serde_json::to_value(active_backend_status("auto")).unwrap();
    assert_eq!(json["realtime"]["state"], "refuse", "{json}");
    assert_eq!(json["realtime"]["priority"], 70);
    assert_eq!(json["realtime"]["cause"], "Operation not permitted");

    let mut sans = backend_status(None, None, "auto");
    sans.realtime = None;
    let json = serde_json::to_value(sans).unwrap();
    assert!(
        json.get("realtime").is_none(),
        "absent plutôt que faux : {json}"
    );
}
