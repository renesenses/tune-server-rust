use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::zone_repo::ZoneRepo;

fn zone_repo() -> (Arc<dyn DbBackend>, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo.create("Salon", Some("dlna"), Some("dev-1")).unwrap();
    (backend, id)
}

/// Le réglage doit être ÉTEINT par défaut.
///
/// C'est l'invariant central de cette fonctionnalité : un renderer qui
/// annonce l'AAC peut le refuser dans un conteneur ou à un débit donné.
/// Activé d'office, cela produirait un silence inexpliqué chez ceux dont le
/// matériel a menti — le pire symptôme, celui qu'on ne relie jamais à sa
/// cause. Celui qui l'active sait ce que son appareil fait vraiment.
#[test]
fn aac_passthrough_is_off_until_the_user_asks_for_it() {
    let (backend, id) = zone_repo();
    let repo = ZoneRepo::with_backend(backend);
    assert!(
        !repo.get_aac_passthrough(id),
        "le passthrough AAC ne doit jamais être actif par défaut"
    );
    repo.update_aac_passthrough(id, true).unwrap();
    assert!(repo.get_aac_passthrough(id));
    repo.update_aac_passthrough(id, false).unwrap();
    assert!(!repo.get_aac_passthrough(id));
}

/// Les deux réglages sont indépendants : activer l'AAC ne doit pas activer
/// l'ALAC, et réciproquement. Ils partagent le conteneur MP4 côté format,
/// ce qui rend la confusion facile à écrire et invisible à l'usage.
#[test]
fn aac_and_alac_settings_never_leak_into_each_other() {
    let (backend, id) = zone_repo();
    let repo = ZoneRepo::with_backend(backend);
    repo.update_aac_passthrough(id, true).unwrap();
    assert!(repo.get_aac_passthrough(id));
    assert!(
        !repo.get_alac_passthrough(id),
        "activer l'AAC a activé l'ALAC"
    );
    repo.update_aac_passthrough(id, false).unwrap();
    repo.update_alac_passthrough(id, true).unwrap();
    assert!(repo.get_alac_passthrough(id));
    assert!(
        !repo.get_aac_passthrough(id),
        "activer l'ALAC a activé l'AAC"
    );
}
