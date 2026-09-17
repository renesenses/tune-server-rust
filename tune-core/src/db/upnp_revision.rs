//! Compteur ui4 du catalogue, entretenu par les déclencheurs SQL (#4201).
use super::backend::DbBackend;

pub fn read(db: &dyn DbBackend) -> Result<u32, String> {
    let row = db
        .query_one_strong("SELECT value FROM upnp_catalog_revision WHERE id = 1", &[])?
        .ok_or("compteur UPnP absent")?;
    row.first()
        .and_then(|v| v.as_i64())
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| "compteur UPnP invalide".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{migrations, sqlite::SqliteDb};

    #[test]
    fn system_update_id_modifications_reelles_transaction_et_redemarrage() {
        let dir = tempfile::tempdir().unwrap();
        let chemin = dir.path().join("revision.db");
        let avant;
        {
            let db = SqliteDb::open(chemin.to_str().unwrap()).unwrap();
            db.init_schema().unwrap();
            migrations::run_migrations(&db).unwrap();
            let debut = read(&db).unwrap();
            db.execute_batch("INSERT INTO tracks (id,title) VALUES (98765,'Avant');")
                .unwrap();
            assert_ne!(
                read(&db).unwrap(),
                debut,
                "un ajout change le SystemUpdateID"
            );
            let ajout = read(&db).unwrap();
            db.execute_batch("UPDATE tracks SET title = title WHERE id = 98765;")
                .unwrap();
            assert_eq!(
                read(&db).unwrap(),
                ajout,
                "réécriture identique sans invalidation"
            );
            db.execute_batch(
                "UPDATE tracks SET file_mtime = 123, comments = 'note' WHERE id = 98765;",
            )
            .unwrap();
            assert_eq!(
                read(&db).unwrap(),
                ajout,
                "les champs non publiés ne changent pas le compteur"
            );
            assert!(
                db.write_tx(&mut |tx| {
                    tx.execute("UPDATE tracks SET title = 'Annulé' WHERE id = 98765", &[])?;
                    Err("annulation volontaire".into())
                })
                .is_err()
            );
            assert_eq!(
                read(&db).unwrap(),
                ajout,
                "le compteur est annulé avec la transaction"
            );
            db.execute_batch("UPDATE tracks SET title = 'Après' WHERE id = 98765;")
                .unwrap();
            assert_ne!(
                read(&db).unwrap(),
                ajout,
                "une correction de tags change le compteur"
            );
            avant = read(&db).unwrap();
        }
        let db = SqliteDb::open(chemin.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        assert_eq!(
            read(&db).unwrap(),
            avant,
            "la migration rejouée conserve le compteur après réouverture"
        );
        db.execute_batch("UPDATE upnp_catalog_revision SET value = 4294967295 WHERE id = 1; DELETE FROM tracks WHERE id = 98765;").unwrap();
        assert_eq!(
            read(&db).unwrap(),
            0,
            "le compteur reste dans ui4 après débordement"
        );
    }

    #[test]
    fn system_update_id_surveille_toutes_les_familles_publiees() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        for sql in [
            "INSERT INTO artists (id,name) VALUES (98765,'Artiste')",
            "INSERT INTO albums (id,title,artist_id) VALUES (98765,'Album',98765)",
            "INSERT INTO tracks (id,title,album_id) VALUES (98765,'Piste',98765)",
            "INSERT INTO playlists (id,name,profile_id) VALUES (98765,'Liste',1)",
            "INSERT INTO playlist_tracks (playlist_id,track_id,position) VALUES (98765,98765,0)",
            "INSERT INTO radio_stations (id,name,url) VALUES (98765,'Radio','http://exemple')",
            "INSERT INTO hidden_items (profile_id,item_type,item_id) VALUES (1,'album','98765')",
            "INSERT INTO track_metadata (track_id,key,value) VALUES (98765,'upnp_res_url','http://nas/piste')",
            "UPDATE albums SET cover_path = 'nouvelle' WHERE id = 98765",
            "UPDATE playlist_tracks SET position = 1 WHERE playlist_id = 98765",
            "DELETE FROM hidden_items WHERE item_id = '98765'",
            "UPDATE artists SET name = 'Autre artiste' WHERE id = 98765",
            "UPDATE artists SET sort_name = 'Tri' WHERE id = 98765",
            "UPDATE radio_stations SET is_favorite = 1 WHERE id = 98765",
        ] {
            let avant = read(&db).unwrap();
            db.execute_batch(sql)
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
            assert_ne!(read(&db).unwrap(), avant, "changement non annoncé : {sql}");
        }
        let avant = read(&db).unwrap();
        db.execute_batch("INSERT INTO track_metadata (track_id,key,value) VALUES (98765,'rg_track_gain','-3'); UPDATE radio_stations SET play_count = 42 WHERE id = 98765;").unwrap();
        assert_eq!(
            read(&db).unwrap(),
            avant,
            "analyse acoustique et compteurs de lecture sans invalidation"
        );
    }
}
