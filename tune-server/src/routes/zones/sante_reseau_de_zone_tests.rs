//! #3318 — la route `network-health` doit rendre la famine d'anneau que le
//! sondeur a mesurée.
//!
//! ## Ce que ces épreuves gardent, et pourquoi elles ne répliquent rien
//!
//! `ZonePollerMetrics` porte deux mesures de ce que le DAC n'a PAS reçu.
//! `GET /zones/sync-status` les rend gratuitement — il sérialise la structure
//! entière. `GET /zones/{id}/network-health` construit son objet champ par
//! champ : les deux mesures y manquaient, alors que le commit qui les a
//! livrées annonçait les DEUX routes.
//!
//! Ce n'est pas une subtilité de présentation. Une route qui répond sans le
//! champ ne se lit pas « je ne sais pas » : elle se lit « rien à signaler ».
//! Une zone dont l'anneau audio se vidait pendant des dizaines de secondes
//! rendait ici exactement le même document qu'une zone saine.
//!
//! Les épreuves appellent [`corps_network_health`] — la fonction que le
//! gestionnaire de route appelle, son seul constructeur de corps — et lisent
//! le JSON par clé. Elles ne relisent pas le `json!` ligne à ligne.

use super::corps_network_health;
use tune_core::poller::ZonePollerMetrics;

/// Une zone dont l'anneau s'est vidé quarante fois, pour 200 ms de silence
/// réellement envoyé au DAC. C'est la forme du relevé de #3318.
fn metrique_avec_famine() -> ZonePollerMetrics {
    ZonePollerMetrics {
        total_polls: 1_200,
        total_errors: 0,
        consecutive_errors: 0,
        last_latency_ms: 12,
        max_latency_ms: 340,
        lecture_au_dela_de_la_duree: false,
        famine_anneau_evenements: 40,
        famine_anneau_silence_ms: 200,
        ..Default::default()
    }
}

/// Le cœur du défaut : la mesure existe côté sondeur et n'arrivait pas
/// jusqu'à l'appelant de la route.
#[test]
fn la_route_rend_la_famine_d_anneau_mesuree_par_le_sondeur() {
    let corps = corps_network_health(7, &metrique_avec_famine(), 4_000_000, Some(1000.0));

    assert_eq!(
        corps["famine_anneau_evenements"].as_u64(),
        Some(40),
        "la route doit rendre les rappels servis à court comptés par le \
         sondeur — corps rendu : {corps}"
    );
    assert_eq!(
        corps["famine_anneau_silence_ms"].as_u64(),
        Some(200),
        "la route doit rendre le silence RÉELLEMENT envoyé au DAC : c'est la \
         seule mesure de ce que l'auditeur n'a pas entendu — corps rendu : \
         {corps}"
    );
}

/// L'autre moitié, et c'est celle qui compte pour lire un relevé : une zone
/// saine doit annoncer ZÉRO, pas rien.
///
/// `0` et l'absence de clé se ressemblent à l'œil et ne disent pas la même
/// chose. `null` (clé absente) veut dire « cette version ne mesure pas » ;
/// `0` veut dire « mesuré, et l'anneau ne s'est jamais vidé ». Sans cette
/// distinction, un relevé pris sur une zone qui va bien ne permet PAS
/// d'écarter la famine — et écarter une piste vaut autant que la retenir.
#[test]
fn une_zone_saine_annonce_zero_famine_et_non_une_absence_de_champ() {
    let corps = corps_network_health(7, &ZonePollerMetrics::default(), 0, None);

    assert!(
        corps["famine_anneau_evenements"].is_u64(),
        "champ absent : un relevé sans famine ne se distinguerait pas d'une \
         version qui ne mesure rien — corps rendu : {corps}"
    );
    assert!(
        corps["famine_anneau_silence_ms"].is_u64(),
        "champ absent : idem — corps rendu : {corps}"
    );
    assert_eq!(corps["famine_anneau_evenements"].as_u64(), Some(0));
    assert_eq!(corps["famine_anneau_silence_ms"].as_u64(), Some(0));
}

/// Les sept champs d'origine restent : la mesure ajoutée ne devait rien
/// prendre à personne.
#[test]
fn les_champs_deja_rendus_le_restent() {
    let corps = corps_network_health(7, &metrique_avec_famine(), 4_000_000, Some(1000.0));

    assert_eq!(corps["zone_id"].as_i64(), Some(7));
    assert_eq!(corps["bytes_sent"].as_u64(), Some(4_000_000));
    assert_eq!(corps["bitrate_kbps"].as_f64(), Some(1000.0));
    assert_eq!(corps["poll_latency_ms"].as_u64(), Some(12));
    assert_eq!(corps["max_latency_ms"].as_u64(), Some(340));
    assert_eq!(corps["poll_errors"].as_u64(), Some(0));
    assert_eq!(corps["total_polls"].as_u64(), Some(1_200));
}

/// `bitrate_kbps` absent ne devient pas `0.0` : `None` dit « pas de quoi
/// mesurer », `0.0` affirmerait que rien ne circule. Cette distinction-là
/// était déjà acquise ; on la garde en passant, puisque l'épreuve ci-dessus
/// en pose la jumelle pour la famine.
#[test]
fn un_debit_non_mesurable_reste_nul_et_non_zero() {
    let corps = corps_network_health(7, &ZonePollerMetrics::default(), 0, None);

    assert!(corps["bitrate_kbps"].is_null(), "corps rendu : {corps}");
}
