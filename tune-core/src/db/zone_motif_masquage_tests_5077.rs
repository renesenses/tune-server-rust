//! #5077 — motif du masquage d'une zone, et réparation sûre.
//!
//! Chaque chemin qui masque pose SON motif ; le démasquage l'efface ; la
//! réparation n'agit que sur `appareil_ignore` dont l'appareil est libéré, et
//! ne touche jamais un motif NUL ni une `suppression_utilisateur`. La même
//! batterie tourne sur PostgreSQL (`pg_5077_*`), dans des tables temporaires
//! copiées du schéma migré.
use std::sync::Arc;

use super::*;
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::EtatDeMasquage;

fn base_migree() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn ignorer(db: &Arc<dyn DbBackend>, device_id: &str, host: &str, name: &str, protocole: &str) {
    IgnoredDeviceRepo::with_backend(db.clone())
        .ignore(&IgnoredDevice {
            device_id: device_id.into(),
            mac: String::new(),
            host: host.into(),
            name: name.into(),
            device_type: protocole.into(),
            created_at: None,
        })
        .unwrap();
}

/// Crée une zone et lui pose son hôte (la découverte le fait).
fn zone(
    repo: &ZoneRepo,
    db: &Arc<dyn DbBackend>,
    nom: &str,
    protocole: &str,
    device_id: &str,
    hote: &str,
) -> i64 {
    let id = repo.create(nom, Some(protocole), Some(device_id)).unwrap();
    db.execute(
        "UPDATE zones SET host = ? WHERE id = ?",
        &[&hote as &dyn crate::db::backend::ToSqlValue, &id],
    )
    .unwrap();
    id
}

fn etat(repo: &ZoneRepo, id: i64) -> EtatDeMasquage {
    repo.etat_de_masquage(id).unwrap().expect("zone inconnue")
}

fn motif(repo: &ZoneRepo, id: i64) -> Option<String> {
    etat(repo, id).motif
}

// ── L'énumération ───────────────────────────────────────────────────────

#[test]
fn les_motifs_se_relisent_et_seul_appareil_ignore_est_reparable() {
    for m in MotifMasquage::TOUS {
        assert_eq!(MotifMasquage::depuis_stocke(Some(m.as_str())), Some(m));
        // serde et la colonne disent la même chose.
        assert_eq!(
            serde_json::to_value(m).unwrap(),
            serde_json::Value::String(m.as_str().into())
        );
        assert_eq!(m.reparable(), m == MotifMasquage::AppareilIgnore, "{m:?}");
    }
    assert_eq!(MotifMasquage::depuis_stocke(None), None, "NUL = inconnu");
    assert_eq!(MotifMasquage::depuis_stocke(Some("  ")), None);
    assert_eq!(
        MotifMasquage::depuis_stocke(Some("motif_d_une_version_future")),
        Some(MotifMasquage::Autre),
        "un texte inconnu n'est jamais réparable"
    );
    assert!(!MotifMasquage::Autre.reparable());
    assert!(MotifMasquage::SuppressionUtilisateur.ecrase_un_masquage_existant());
    assert!(!MotifMasquage::AppareilIgnore.ecrase_un_masquage_existant());
}

// ── Chaque chemin pose son motif ────────────────────────────────────────

fn chaque_chemin_pose_son_motif(db: Arc<dyn DbBackend>) {
    let repo = ZoneRepo::with_backend(db.clone());

    // Suppression par l'utilisateur.
    let a = zone(&repo, &db, "Salon", "dlna", "uuid:salon", "10.0.0.2");
    repo.delete(a).unwrap();
    let e = etat(&repo, a);
    assert!(e.masquee);
    assert_eq!(e.motif.as_deref(), Some("suppression_utilisateur"));
    assert!(e.masquee_le.is_some(), "la date du masquage manque");

    // Cascade d'« Ignorer », reflet, fusion : `masquer` avec leur motif.
    for (m, dev) in [
        (MotifMasquage::AppareilIgnore, "uuid:ignore"),
        (MotifMasquage::ZoneReflet, "uuid:reflet"),
        (MotifMasquage::Fusion, "uuid:fusion"),
    ] {
        let id = zone(&repo, &db, dev, "dlna", dev, "10.0.0.3");
        assert_eq!(repo.masquer(id, m).unwrap(), 1);
        let e = etat(&repo, id);
        assert!(e.masquee, "{m:?}");
        assert_eq!(e.motif.as_deref(), Some(m.as_str()));
        assert!(e.masquee_le.is_some());
    }

    // Jumelle locale générique.
    let ancienne = repo
        .create("Cet ordinateur", Some("local"), Some("local:Ancien"))
        .unwrap();
    let vivante = repo
        .create("This Computer", Some("local"), Some("local:Vivant"))
        .unwrap();
    assert_eq!(repo.hide_duplicate_generic_local(vivante).unwrap(), 1);
    assert_eq!(
        motif(&repo, ancienne).as_deref(),
        Some("doublon_local_generique")
    );
    assert!(etat(&repo, ancienne).masquee_le.is_some());
    assert_eq!(etat(&repo, vivante), EtatDeMasquage::default());

    // Le démasquage efface motif ET date.
    repo.unhide(a).unwrap();
    assert_eq!(etat(&repo, a), EtatDeMasquage::default());

    // Un masquage AUTOMATIQUE ne réécrit pas une suppression : sans cela, une
    // réparation future ressusciterait une zone que l'utilisateur a retirée.
    repo.delete(a).unwrap();
    assert_eq!(repo.masquer(a, MotifMasquage::AppareilIgnore).unwrap(), 0);
    assert_eq!(motif(&repo, a).as_deref(), Some("suppression_utilisateur"));
    // …mais une suppression explicite remplace un motif réparable.
    let b = zone(&repo, &db, "Cuisine", "dlna", "uuid:cuisine", "10.0.0.4");
    repo.masquer(b, MotifMasquage::AppareilIgnore).unwrap();
    repo.delete(b).unwrap();
    assert_eq!(motif(&repo, b).as_deref(), Some("suppression_utilisateur"));

    // « Supprimer toutes les zones » : toutes, motif `suppression_totale`.
    repo.unhide(vivante).unwrap();
    repo.delete_all().unwrap();
    for id in [a, b, ancienne, vivante] {
        let e = etat(&repo, id);
        assert!(e.masquee);
        assert_eq!(e.motif.as_deref(), Some("suppression_totale"), "zone {id}");
    }
}

#[test]
fn sqlite_5077_chaque_chemin_pose_son_motif() {
    chaque_chemin_pose_son_motif(base_migree());
}

/// La fusion réelle (`fusionner`) masque le doublon au motif `fusion`.
#[test]
fn la_fusion_masque_le_doublon_au_motif_fusion() {
    let db = base_migree();
    let repo = ZoneRepo::with_backend(db.clone());
    let doublon = zone(
        &repo,
        &db,
        "Chambre",
        "dlna",
        "uuid:RINCON_1_MR",
        "10.0.0.9",
    );
    let cible = zone(
        &repo,
        &db,
        "Chambre - Sonos",
        "dlna",
        "uuid:RINCON_1",
        "10.0.0.9",
    );
    repo.fusionner(doublon, cible).unwrap();
    assert_eq!(motif(&repo, doublon).as_deref(), Some("fusion"));
    assert_eq!(etat(&repo, cible), EtatDeMasquage::default());
}

// ── La réparation ──────────────────────────────────────────────────────

fn la_reparation_n_agit_que_sur_appareil_ignore_libere(db: Arc<dyn DbBackend>) {
    let repo = ZoneRepo::with_backend(db.clone());

    // L'appareil de `liberee` a été ignoré puis débloqué.
    let liberee = zone(&repo, &db, "Eversolo", "dlna", "uuid:dmp-a6", "10.0.0.20");
    repo.masquer(liberee, MotifMasquage::AppareilIgnore)
        .unwrap();
    // `toujours` : son appareil est ENCORE ignoré.
    let toujours = zone(&repo, &db, "Bureau", "dlna", "uuid:bureau", "10.0.0.21");
    repo.masquer(toujours, MotifMasquage::AppareilIgnore)
        .unwrap();
    ignorer(&db, "uuid:bureau", "10.0.0.21", "Bureau", "dlna");
    // `voisine` : autre identifiant, mais un appareil du MÊME hôte et du
    // même protocole est encore ignoré — la règle large de la cascade.
    let voisine = zone(&repo, &db, "Chambre", "dlna", "uuid:chambre-2", "10.0.0.22");
    repo.masquer(voisine, MotifMasquage::AppareilIgnore)
        .unwrap();
    ignorer(&db, "uuid:chambre-1", "10.0.0.22", "Autre nom", "dlna");
    // `autre_protocole` : même hôte, mais l'ignoré est l'entrée AirPlay — le
    // cas de Villerio : la zone DLNA n'est pas la sienne.
    let autre_protocole = zone(&repo, &db, "DMP-A8", "dlna", "uuid:dmp-a8", "10.0.0.23");
    repo.masquer(autre_protocole, MotifMasquage::AppareilIgnore)
        .unwrap();
    ignorer(
        &db,
        "eversolo,1",
        "10.0.0.23",
        "Eversolo AirPlay",
        "airplay",
    );
    // `inconnue` : masquée avant la migration — motif NUL.
    let inconnue = zone(&repo, &db, "Villerio", "dlna", "uuid:villerio", "10.0.0.24");
    db.execute(
        "UPDATE zones SET is_hidden = 1 WHERE id = ?",
        &[&inconnue as &dyn crate::db::backend::ToSqlValue],
    )
    .unwrap();
    // `supprimee` : supprimée par l'utilisateur, appareil jamais ignoré.
    let supprimee = zone(&repo, &db, "Garage", "dlna", "uuid:garage", "10.0.0.25");
    repo.delete(supprimee).unwrap();
    // `totale`, `reflet`, `jumelle` : d'autres motifs, jamais réparés.
    let reflet = zone(
        &repo,
        &db,
        "Tune (Tune)",
        "dlna",
        "uuid:reflet",
        "10.0.0.26",
    );
    repo.masquer(reflet, MotifMasquage::ZoneReflet).unwrap();

    let rapport = reparer_les_masquages_surs(db.clone()).unwrap();
    assert_eq!(rapport.demasquees, vec![liberee, autre_protocole]);
    assert_eq!(rapport.gardees, vec![toujours, voisine]);

    assert_eq!(etat(&repo, liberee), EtatDeMasquage::default());
    assert_eq!(etat(&repo, autre_protocole), EtatDeMasquage::default());
    for id in [toujours, voisine] {
        assert_eq!(motif(&repo, id).as_deref(), Some("appareil_ignore"));
        assert!(etat(&repo, id).masquee);
    }
    let e = etat(&repo, inconnue);
    assert!(e.masquee, "un motif NUL ne doit JAMAIS être démasqué");
    assert_eq!(e.motif, None);
    let e = etat(&repo, supprimee);
    assert!(
        e.masquee,
        "une suppression utilisateur ne doit JAMAIS être démasquée"
    );
    assert_eq!(e.motif.as_deref(), Some("suppression_utilisateur"));
    assert!(etat(&repo, reflet).masquee);

    // Rejouée : plus rien à faire.
    let rapport = reparer_les_masquages_surs(db.clone()).unwrap();
    assert!(rapport.demasquees.is_empty());

    // Débloquer `bureau` libère `toujours` au passage suivant — et pas
    // `voisine`, dont le voisin reste ignoré.
    IgnoredDeviceRepo::with_backend(db.clone())
        .unignore("uuid:bureau")
        .unwrap();
    let rapport = reparer_les_masquages_surs(db.clone()).unwrap();
    assert_eq!(rapport.demasquees, vec![toujours]);
    assert!(etat(&repo, voisine).masquee);
}

#[test]
fn sqlite_5077_la_reparation_n_agit_que_sur_appareil_ignore_libere() {
    la_reparation_n_agit_que_sur_appareil_ignore_libere(base_migree());
}

/// Liste d'ignorés illisible : rien n'est démasqué. Une erreur de lecture
/// n'est pas une liste vide.
#[test]
fn liste_d_ignores_illisible_rien_n_est_demasque() {
    let db = base_migree();
    let repo = ZoneRepo::with_backend(db.clone());
    let id = zone(&repo, &db, "Eversolo", "dlna", "uuid:dmp-a6", "10.0.0.20");
    repo.masquer(id, MotifMasquage::AppareilIgnore).unwrap();
    db.execute("DROP TABLE ignored_devices", &[]).unwrap();
    assert!(reparer_les_masquages_surs(db.clone()).is_err());
    assert!(etat(&repo, id).masquee);
}

/// Le prédicat pur : une zone sans identifiant d'appareil est gardée ; la MAC
/// d'un AUTRE protocole ne la vise pas ; hôte + nom la vise.
#[test]
fn le_predicat_est_conservateur() {
    let zone =
        |device_id: &str, host: &str, nom: &str, proto: &str, mac: Option<&str>| ZoneMasquee {
            id: 1,
            name: nom.into(),
            device_id: device_id.into(),
            protocole: proto.into(),
            host: host.into(),
            mac: mac.map(Into::into),
            motif: Some("appareil_ignore".into()),
            masquee_le: None,
        };
    let ignore = |device_id: &str, host: &str, nom: &str, proto: &str, mac: &str| IgnoredDevice {
        device_id: device_id.into(),
        mac: mac.into(),
        host: host.into(),
        name: nom.into(),
        device_type: proto.into(),
        created_at: None,
    };
    assert!(zone_encore_visee_par_un_ignore(
        &zone("", "", "", "", None),
        &[]
    ));
    assert!(!zone_encore_visee_par_un_ignore(
        &zone("uuid:a", "10.0.0.1", "A", "dlna", None),
        &[]
    ));
    // MAC identique, protocole différent, autre hôte : pas la même sortie.
    assert!(!zone_encore_visee_par_un_ignore(
        &zone("uuid:a", "10.0.0.1", "A", "dlna", Some("AA:BB:CC:DD:EE:FF")),
        &[ignore(
            "ap",
            "10.0.0.9",
            "B",
            "airplay",
            "AA:BB:CC:DD:EE:FF"
        )]
    ));
    // Même MAC, même protocole : visée.
    assert!(zone_encore_visee_par_un_ignore(
        &zone("uuid:a", "10.0.0.1", "A", "dlna", Some("AA:BB:CC:DD:EE:FF")),
        &[ignore(
            "uuid:b",
            "10.0.0.9",
            "B",
            "dlna",
            "aa-bb-cc-dd-ee-ff"
        )]
    ));
    // Même hôte, protocole de l'ignoré inconnu : dans le doute, visée.
    assert!(zone_encore_visee_par_un_ignore(
        &zone("uuid:a", "10.0.0.1", "A", "dlna", None),
        &[ignore("x", "10.0.0.1", "B", "", "")]
    ));
}

// ── Migration ──────────────────────────────────────────────────────────

fn colonnes_zones(db: &SqliteDb) -> Vec<String> {
    let conn = db.connection().lock().unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(zones)").unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// La 112 pose les deux colonnes sur une base NEUVE comme sur une base
/// ANCIENNE ; un masquage d'avant la migration naît de motif INCONNU, et la
/// réparation n'y touche pas. Et la jumelle PG 075 existe et est enregistrée.
#[test]
fn la_migration_112_pose_le_motif_sans_rien_deviner_5077() {
    let neuve = SqliteDb::open_in_memory().unwrap();
    neuve.init_schema().unwrap();
    crate::db::migrations::run_migrations(&neuve).unwrap();
    let c = colonnes_zones(&neuve);
    for a in ["motif_masquage", "masquee_le"] {
        assert!(c.iter().any(|x| x == a), "base neuve : `zones.{a}` manque");
    }

    // Base ANCIENNE : `zones` d'avant la 112, une zone déjà masquée.
    let ancienne = SqliteDb::open_in_memory().unwrap();
    ancienne
        .execute_batch(
            "CREATE TABLE zones (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                output_type TEXT,
                output_device_id TEXT,
                is_hidden INTEGER DEFAULT 0
            );
            INSERT INTO zones (name, output_type, output_device_id, is_hidden)
                VALUES ('Zone de Villerio', 'dlna', 'uuid:villerio', 1);",
        )
        .unwrap();
    ancienne.init_schema().unwrap();
    crate::db::migrations::run_migrations(&ancienne).unwrap();
    let c = colonnes_zones(&ancienne);
    for a in ["motif_masquage", "masquee_le"] {
        assert!(
            c.iter().any(|x| x == a),
            "base ancienne : `zones.{a}` manque ({c:?})"
        );
    }
    let db: Arc<dyn DbBackend> = Arc::new(ancienne);
    let repo = ZoneRepo::with_backend(db.clone());
    let id = 1; // la seule ligne, AUTOINCREMENT
    let e = etat(&repo, id);
    assert!(e.masquee, "le masquage existant survit");
    assert_eq!((e.motif, e.masquee_le), (None, None), "motif INCONNU");
    reparer_les_masquages_surs(db).unwrap();
    assert!(
        etat(&repo, id).masquee,
        "un motif inconnu n'est jamais réparé"
    );

    // La jumelle PG.
    let racine = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fichier = "075_zones_motif_masquage.sql";
    let sql_pg = std::fs::read_to_string(racine.join("migrations/postgres").join(fichier))
        .expect("la jumelle PG 075 n'existe pas");
    assert!(sql_pg.contains("VALUES (75, 'zones_motif_masquage')"));
    for a in ["motif_masquage", "masquee_le"] {
        assert!(sql_pg.contains(&format!("ADD COLUMN IF NOT EXISTS {a} TEXT")));
    }
    let migrations = include_str!("migrations.rs");
    assert!(
        migrations.contains(fichier),
        "{fichier} n'est pas enregistrée"
    );
    assert!(migrations.contains("(\n        75,\n        \"zones_motif_masquage\""));
    // Le schéma de bascule SQLite -> PG et sa passe de rattrapage.
    let pg_migrate = include_str!("pg_migrate.rs");
    for a in ["motif_masquage", "masquee_le"] {
        assert!(
            pg_migrate.contains(&format!("    {a} TEXT"))
                && pg_migrate.contains(&format!(
                    "ALTER TABLE zones ADD COLUMN IF NOT EXISTS {a} TEXT"
                )),
            "pg_migrate.rs ne porte pas `{a}`"
        );
    }
}

/// Base sans les colonnes de la 112 (migration pas encore passée) : supprimer
/// une zone la masque QUAND MÊME, sans motif — et la démasquer aussi.
#[test]
fn sans_les_colonnes_le_masquage_passe_quand_meme() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE zones (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            output_type TEXT,
            output_device_id TEXT,
            is_hidden INTEGER DEFAULT 0,
            last_track_id INTEGER
        );
        INSERT INTO zones (name, output_type, output_device_id) VALUES ('A', 'dlna', 'uuid:a');",
    )
    .unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(db.clone());
    let lire = || -> i64 {
        db.query_one("SELECT is_hidden FROM zones WHERE id = 1", &[])
            .unwrap()
            .unwrap()[0]
            .as_i64()
            .unwrap()
    };
    repo.delete(1).unwrap();
    assert_eq!(lire(), 1);
    repo.unhide(1).unwrap();
    assert_eq!(lire(), 0);
    assert_eq!(repo.masquer(1, MotifMasquage::AppareilIgnore).unwrap(), 1);
    assert_eq!(lire(), 1);
    repo.unhide(1).unwrap();
    repo.delete_all().unwrap();
    assert_eq!(lire(), 1);
    // La réparation, elle, refuse de travailler à l'aveugle.
    assert!(reparer_les_masquages_surs(db.clone()).is_err());
}

// ── PostgreSQL ─────────────────────────────────────────────────────────

/// Une connexion unique sur la base de `TUNE_TEST_PG_URL` (schéma migré par
/// l'étape « Apply PG migrations »), avec `zones` et `ignored_devices`
/// masquées par des tables TEMPORAIRES copiées du vrai schéma : rien n'est
/// écrit dans la base partagée.
#[cfg(feature = "postgres")]
async fn pg_temporaire() -> Option<(sqlx::PgPool, Arc<dyn DbBackend>)> {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return None;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    for colonne in ["motif_masquage", "masquee_le"] {
        let n = db
            .query_one(
                "SELECT COUNT(*) FROM information_schema.columns \
                 WHERE table_schema = 'public' AND table_name = 'zones' AND column_name = ?",
                &[&colonne],
            )
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        assert_eq!(n, 1, "la migration PG 075 n'a pas posé `zones.{colonne}`");
    }
    // La 075 rejouée : idempotente.
    sqlx::raw_sql(include_str!(
        "../../migrations/postgres/075_zones_motif_masquage.sql"
    ))
    .execute(&pool)
    .await
    .expect("la 075 doit se rejouer sans erreur");
    for table in ["zones", "ignored_devices"] {
        db.execute(
            &format!("CREATE TEMP TABLE {table} (LIKE public.{table} INCLUDING ALL)"),
            &[],
        )
        .unwrap();
    }
    // Colonnes que seul `ensure_schema` pose sur une base native : la copie
    // du schéma des scripts ne les porte pas forcément.
    for ddl in [
        "ALTER TABLE zones ADD COLUMN IF NOT EXISTS is_hidden SMALLINT DEFAULT 0",
        "ALTER TABLE zones ADD COLUMN IF NOT EXISTS host TEXT",
        "ALTER TABLE zones ADD COLUMN IF NOT EXISTS mac TEXT",
    ] {
        db.execute(ddl, &[]).unwrap();
    }
    Some((pool, db))
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_5077_chaque_chemin_pose_son_motif_et_la_reparation_est_sure() {
    let Some((pool, db)) = pg_temporaire().await else {
        return;
    };
    // `zone()` lie `?` : le moteur PostgreSQL les traduit.
    let repo = ZoneRepo::with_backend(db.clone());
    let a = zone(&repo, &db, "Salon", "dlna", "uuid:salon", "10.0.0.2");
    repo.delete(a).unwrap();
    assert_eq!(motif(&repo, a).as_deref(), Some("suppression_utilisateur"));
    assert!(etat(&repo, a).masquee_le.is_some());
    repo.unhide(a).unwrap();
    assert_eq!(etat(&repo, a), EtatDeMasquage::default());
    db.execute("DELETE FROM zones", &[]).unwrap();

    la_reparation_n_agit_que_sur_appareil_ignore_libere(db.clone());
    db.execute("DELETE FROM zones", &[]).unwrap();
    db.execute("DELETE FROM ignored_devices", &[]).unwrap();
    chaque_chemin_pose_son_motif(db);
    pool.close().await;
}
