//! #9044 — oublier un appareil absent : la ligne part, et l'appareil revenu
//! retrouve une zone neuve au lieu d'une zone masquée qui le rend invisible.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::{MotifMasquage, ZoneRepo};

fn verifier(db: Arc<dyn DbBackend>, prefixe: &str) {
    let repo = ZoneRepo::with_backend(db.clone());
    let absent = format!("{prefixe}-absent");
    let present = format!("{prefixe}-present");
    let za = repo
        .create("Ampli du salon 9044", Some("dlna"), Some(&absent))
        .unwrap();
    let zp = repo
        .create("Enceinte 9044", Some("airplay"), Some(&present))
        .unwrap();
    repo.set_online_by_device(&absent, true).unwrap();
    repo.set_online_by_device(&absent, false).unwrap();
    repo.set_online_by_device(&present, true).unwrap();

    let zones = repo.zones_de_l_appareil(&absent).unwrap();
    assert_eq!(zones.len(), 1, "{zones:?}");
    let z = &zones[0];
    assert_eq!(z.id, za);
    assert_eq!(z.name, "Ampli du salon 9044");
    assert_eq!(z.output_type.as_deref(), Some("dlna"));
    assert!(!z.online && !z.hidden && z.motif.is_none() && !z.sortie_liberee);
    assert!(z.oubliable());
    assert!(repo.zones_de_l_appareil(&present).unwrap()[0].online);

    // Une zone supprimée (masquée) par l'utilisateur : oubliable, et c'est
    // elle qui bloque le retour de l'appareil.
    repo.masquer(za, MotifMasquage::SuppressionUtilisateur)
        .unwrap();
    let z = &repo.zones_de_l_appareil(&absent).unwrap()[0];
    assert!(z.hidden);
    assert_eq!(z.motif.as_deref(), Some("suppression_utilisateur"));
    assert!(z.oubliable());
    let (meme, nee) = repo.get_or_create("Ampli", Some("dlna"), &absent).unwrap();
    assert_eq!(
        (meme, nee),
        (za, false),
        "masquée : l'appareil ne revient pas"
    );

    // Oublier : la LIGNE part.
    assert_eq!(repo.supprimer_definitivement(za).unwrap(), 1);
    assert!(repo.zones_de_l_appareil(&absent).unwrap().is_empty());
    assert!(repo.get(za).unwrap().is_none());
    assert_eq!(repo.supprimer_definitivement(za).unwrap(), 0);
    // L'appareil revient : zone NEUVE, autre identifiant, visible.
    let (neuve, nee) = repo.get_or_create("Ampli", Some("dlna"), &absent).unwrap();
    assert!(nee, "l'appareil revenu retrouve une zone");
    assert_ne!(neuve, za, "identifiant jamais réutilisé");
    assert!(!repo.is_device_hidden(&absent));

    // Les garde-fous de la découverte ne sont JAMAIS oubliables.
    for motif in [
        MotifMasquage::Fusion,
        MotifMasquage::ZoneReflet,
        MotifMasquage::AppareilIgnore,
    ] {
        let reflet = format!("{prefixe}-{}", motif.as_str());
        let zr = repo
            .create("Garde 9044", Some("dlna"), Some(&reflet))
            .unwrap();
        repo.masquer(zr, motif).unwrap();
        let z = &repo.zones_de_l_appareil(&reflet).unwrap()[0];
        assert_eq!(z.motif.as_deref(), Some(motif.as_str()));
        assert!(
            !z.oubliable(),
            "{} : garde-fou de la découverte",
            motif.as_str()
        );
        repo.supprimer_definitivement(zr).unwrap();
    }

    // #8987 — une zone supprimée qui a LÂCHÉ sa sortie appartient encore à
    // l'appareil qu'elle tenait ; l'oubli emporte la ligne ET la clé.
    let liberee_de = format!("{prefixe}-repris");
    let zl = repo
        .create("Ancienne 9044", Some("dlna"), Some(&liberee_de))
        .unwrap();
    repo.masquer(zl, MotifMasquage::SuppressionUtilisateur)
        .unwrap();
    db.execute(
        &format!("UPDATE zones SET output_device_id = NULL WHERE id = {zl}"),
        &[],
    )
    .unwrap();
    SettingsRepo::with_backend(db.clone())
        .set(&ZoneRepo::cle_sortie_liberee(zl), &liberee_de)
        .unwrap();
    let z = &repo.zones_de_l_appareil(&liberee_de).unwrap()[0];
    assert_eq!(z.id, zl);
    assert!(z.sortie_liberee && z.hidden && z.oubliable());
    assert_eq!(repo.zone_masquee_qui_tenait(&liberee_de), Some(zl));
    assert_eq!(repo.supprimer_definitivement(zl).unwrap(), 1);
    assert_eq!(repo.sortie_liberee(zl), None, "la clé part avec la ligne");
    assert_eq!(repo.zone_masquee_qui_tenait(&liberee_de), None);

    for id in [neuve, zp] {
        repo.supprimer_definitivement(id).unwrap();
    }
}

#[test]
fn oublier_un_appareil_supprime_sa_zone_et_le_laisse_revenir_sqlite() {
    let db = crate::db::zone_repo::tests::test_db_migree();
    verifier(Arc::new(db), "sqlite-9044");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn oublier_un_appareil_supprime_sa_zone_et_le_laisse_revenir_pg() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent — épreuve PostgreSQL #9044 sautée");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.expect("connexion PG");
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = sqlx::raw_sql(*sql).execute(&pool).await;
    }
    crate::db::migrations::run_pg_migrations(&pool)
        .await
        .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    let prefixe = format!("pg-9044-{}", std::process::id());
    verifier(db, &prefixe);
    pool.close().await;
}
