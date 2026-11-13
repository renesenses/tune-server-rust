//! #7478 — la surveillance des dossiers relit ses exclusions sans
//! redémarrage : motifs par emplacement ET liste globale.

use super::surveillant_retouche_tests_4896::base;
use super::{
    ecarte_du_surveillant, exclusions_du_surveillant, recharger_les_exclusions, settle_partition,
};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::scanner::watcher::{ChangeType, FileChange};

#[test]
fn la_surveillance_suit_les_reglages_sans_redemarrer_7478() {
    let db = base();
    let settings = SettingsRepo::with_backend(db.clone());
    let fichier = "/srv/banc-7478-surveillant/Jazz/Incoming/a.flac";

    recharger_les_exclusions(&db);
    assert!(!ecarte_du_surveillant(fichier, &[]));

    settings
        .set(
            tune_core::scanner::exclusions::CLE_EXCLUSIONS_PAR_EMPLACEMENT,
            r#"[{"root": "/srv/banc-7478-surveillant", "patterns": ["**/Incoming/**"]}]"#,
        )
        .unwrap();
    settings
        .set("scan_exclude_paths", r#"["brouillons-7478"]"#)
        .unwrap();
    recharger_les_exclusions(&db);
    assert!(
        ecarte_du_surveillant(fichier, &[]),
        "motif de l'emplacement relu"
    );
    assert_eq!(
        exclusions_du_surveillant().globales(),
        ["brouillons-7478"],
        "liste globale relue"
    );
    let (prets, en_attente) = settle_partition(
        vec![FileChange {
            path: fichier.into(),
            change_type: ChangeType::Deleted,
        }],
        &[],
    );
    assert!(
        prets.is_empty() && en_attente.is_empty(),
        "l'événement est écarté"
    );

    // Rendu : le fichier redevient suivi, toujours sans redémarrage.
    settings
        .delete(tune_core::scanner::exclusions::CLE_EXCLUSIONS_PAR_EMPLACEMENT)
        .unwrap();
    settings.delete("scan_exclude_paths").unwrap();
    recharger_les_exclusions(&db);
    assert!(!ecarte_du_surveillant(fichier, &[]));
    assert!(exclusions_du_surveillant().globales().is_empty());
}
