//! Mesure reproductible, sans compte ni données réelles.
//! cargo run -p tune-core --no-default-features --example banc_sauvegarde_7286
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tune_core::cloud::sauvegarde_config as sc;
use tune_core::db::{
    backend::DbBackend, migrations, settings_repo::SettingsRepo, sqlite::SqliteDb,
};

fn mesurer(nom: &str, b: &Arc<dyn DbBackend>, cle: &sc::CleLocale) -> Result<(), String> {
    let contenu = sc::construire(b)?;
    let clair = serde_json::to_vec(&contenu).map_err(|e| e.to_string())?;
    let blob = sc::chiffrer(cle, &contenu)?;
    let (relu, _) = sc::dechiffrer(&blob, Some(cle), None).map_err(|e| format!("{e:?}"))?;
    if relu != contenu {
        return Err("le contenu ne survit pas au chiffrement et à la réouverture".into());
    }
    let requete = json!({"server_id":"synthetic-benchmark", "server_label":null,
        "key_id":cle.key_id, "format_version":sc::FORMAT_BLOB_VERSION, "replaces":null, "payload":blob});
    println!(
        "{}",
        json!({"scenario":nom,"clair_octets":clair.len(),
        "payload_octets":blob.len(),"requete_json_octets":serde_json::to_vec(&requete).unwrap().len(),
        "limite_site_par_defaut_octets":4194304,"reouverture_identique":true})
    );
    Ok(())
}
fn main() -> Result<(), String> {
    let db = SqliteDb::open_in_memory().map_err(|e| e.to_string())?;
    db.init_schema().map_err(|e| e.to_string())?;
    migrations::run_migrations(&db).map_err(|e| e.to_string())?;
    let b: Arc<dyn DbBackend> = Arc::new(db);
    let s = SettingsRepo::with_backend(b.clone());
    sc::creer_la_cle(&s, "phrase synthetique du banc 7286")?;
    let cle = sc::cle_locale(&s)?.ok_or("clé synthétique absente")?;
    mesurer("base_vide", &b, &cle)?;
    b.execute(
        "INSERT INTO artists(id,name) VALUES(1,'Artiste synthétique')",
        &[],
    )?;
    b.execute("BEGIN", &[])?;
    for debut in (1..=36_000).step_by(1000) {
        let lignes = (debut..debut + 1000)
            .map(|n| format!("({n},'Album {n}',1)"))
            .collect::<Vec<_>>()
            .join(",");
        b.execute(
            &format!("INSERT INTO albums(id,title,artist_id) VALUES {lignes}"),
            &[],
        )?;
    }
    for debut in (1_u64..=600_000).step_by(1000) {
        let lignes = (debut..debut+1000).map(|n| {
            let album = (n - 1) % 36_000 + 1;
            let empreinte = format!("{:x}", Sha256::digest(n.to_le_bytes()));
            format!("({n},'Piste {n}',{album},1,'/bibliotheque-synthetique/{album}/{empreinte}.flac')")
        }).collect::<Vec<_>>().join(",");
        b.execute(
            &format!("INSERT INTO tracks(id,title,album_id,artist_id,file_path) VALUES {lignes}"),
            &[],
        )?;
    }
    b.execute("COMMIT", &[])?;
    mesurer("600000_pistes_36000_albums_sans_playlist", &b, &cle)?;
    b.execute(
        "INSERT INTO playlists(id,name) VALUES(1,'Sélection synthétique')",
        &[],
    )?;
    b.execute("INSERT INTO playlist_tracks(playlist_id,track_id,position) SELECT 1,id,id-1 FROM tracks WHERE id<=60000", &[])?;
    mesurer("meme_base_avec_playlist_60000_titres", &b, &cle)?;
    Ok(())
}
