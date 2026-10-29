//! #3179 — import réel puis relecture SQLite : l'artiste affiché vient de
//! `tracks.artist_id`, pas du nom non persisté que portait la structure Track.
//! La règle ARTIST / ALBUMARTIST est validée par Bertrand le 27/10/2026.

use super::*;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;

const SCOTT: &str = "Scott Hamilton Quintet";
const WARREN: &str = "Warren Vaché";
// Identifiants fictifs, distincts : aucune requête MusicBrainz.
const MBID_SCOTT: &str = "11111111-1111-4111-8111-111111111111";
const MBID_WARREN: &str = "22222222-2222-4222-8222-222222222222";

fn base() -> (tempfile::TempDir, Arc<dyn DbBackend>) {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteDb::open(dir.path().join("import.db").to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    (dir, Arc::new(db))
}

fn fichier(racine: &std::path::Path, numero: u32, artiste: Option<&str>) -> ScannedFile {
    ScannedFile {
        path: racine
            .join("Swingin Young Scott")
            .join(format!("{numero:02}.flac"))
            .to_string_lossy()
            .into_owned(),
        metadata: Some(TrackMetadata {
            title: Some(format!("Piste {numero}")),
            artist: artiste.map(Into::into),
            album: Some("Swingin' Young Scott".into()),
            album_artist: Some(SCOTT.into()),
            compilation: Some(false),
            track_number: Some(numero),
            ..Default::default()
        }),
        unsupported: None,
        audio_hash: None,
        file_size: 4096,
        mtime: 1_700_000_000.0,
    }
}

fn importer(
    db: &Arc<dyn DbBackend>,
    racine: &std::path::Path,
    fichiers: &[ScannedFile],
) -> Vec<Track> {
    let mut imp = TrackImporter::new(db.clone(), true, racine.join("cache"), PorteeDuScan::TOUT)
        .avec_pochettes_differees();
    imp.begin_batch(fichiers);
    let repo = TrackRepo::with_backend(db.clone());
    for f in fichiers {
        let (mut piste, _) = imp.import(f).expect("piste importée");
        if let Some(avant) = repo.get_by_path(&f.path).unwrap() {
            piste.id = avant.id;
            assert_eq!(repo.update_batch(&[piste]).unwrap(), 1);
        } else {
            assert_eq!(repo.create_batch(&[piste]).unwrap(), 1);
        }
    }
    fichiers
        .iter()
        .map(|f| repo.get_by_path(&f.path).unwrap().expect("piste persistée"))
        .collect()
}

fn artiste_d_album(db: &Arc<dyn DbBackend>, piste: &Track) -> Artist {
    let album = AlbumRepo::with_backend(db.clone())
        .get(piste.album_id.unwrap())
        .unwrap()
        .unwrap();
    assert!(!album.is_compilation, "COMPILATION=0 doit rester respecté");
    ArtistRepo::with_backend(db.clone())
        .get(album.artist_id.unwrap())
        .unwrap()
        .unwrap()
}

#[test]
fn les_pistes_non_compilation_gardent_leur_artiste_3179() {
    let (dir, db) = base();
    let fichiers = [
        fichier(dir.path(), 1, Some(SCOTT)),
        fichier(dir.path(), 2, Some(WARREN)),
    ];
    let pistes = importer(&db, dir.path(), &fichiers);
    assert_eq!(pistes[0].artist_name.as_deref(), Some(SCOTT));
    assert_eq!(
        pistes[1].artist_name.as_deref(),
        Some(WARREN),
        "ARTIST de la piste doit survivre à l'import SQLite hors compilation (#3179)"
    );
    assert_ne!(pistes[0].artist_id, pistes[1].artist_id);
    assert_eq!(pistes[0].album_id, pistes[1].album_id, "un seul album");
    assert_eq!(artiste_d_album(&db, &pistes[1]).name, SCOTT);
}

#[test]
fn le_mbid_de_l_invite_ne_devient_pas_celui_de_l_album_3179() {
    let (dir, db) = base();
    let mut f = fichier(dir.path(), 1, Some(WARREN));
    let meta = f.metadata.as_mut().unwrap();
    meta.musicbrainz_artist_id = Some(MBID_WARREN.into());
    meta.artist_sort = Some("Vaché, Warren".into());
    meta.album_artist_sort = Some("Hamilton, Scott Quintet".into());
    let pistes = importer(&db, dir.path(), &[f]);
    let album = artiste_d_album(&db, &pistes[0]);
    assert_eq!(album.name, SCOTT);
    assert_eq!(
        album.musicbrainz_id, None,
        "le MBID de l'invité ne doit pas identifier l'artiste d'album (#3179)"
    );
    assert_eq!(album.sort_name.as_deref(), Some("Hamilton, Scott Quintet"));
    let invite = ArtistRepo::with_backend(db.clone())
        .get(pistes[0].artist_id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(invite.name, WARREN);
    assert_eq!(invite.musicbrainz_id.as_deref(), Some(MBID_WARREN));
    assert_eq!(invite.sort_name.as_deref(), Some("Vaché, Warren"));
}

#[test]
fn un_meme_artiste_reutilise_sa_fiche_et_son_mbid_3179() {
    let (dir, db) = base();
    let mut f = fichier(dir.path(), 1, Some(SCOTT));
    f.metadata.as_mut().unwrap().musicbrainz_artist_id = Some(MBID_SCOTT.into());
    let pistes = importer(&db, dir.path(), &[f]);
    let album = artiste_d_album(&db, &pistes[0]);
    assert_eq!(album.id, pistes[0].artist_id);
    assert_eq!(album.musicbrainz_id.as_deref(), Some(MBID_SCOTT));
}

#[test]
fn sans_balise_artist_le_repli_n_invente_pas_un_invite_3179() {
    let (dir, db) = base();
    let sans_artiste = fichier(dir.path(), 1, None);
    let mut par_chemin = fichier(dir.path(), 2, Some("Nom du dossier"));
    par_chemin.metadata.as_mut().unwrap().artist_from_path = true;
    let pistes = importer(&db, dir.path(), &[sans_artiste, par_chemin]);
    for piste in pistes {
        assert_eq!(piste.artist_name.as_deref(), Some(SCOTT));
        assert_eq!(piste.artist_id, artiste_d_album(&db, &piste).id);
    }
}

#[test]
fn sans_albumartist_le_nom_d_album_n_est_pas_devine_3179() {
    let (dir, db) = base();
    let mut fichiers = [
        fichier(dir.path(), 1, Some(WARREN)),
        fichier(dir.path(), 2, Some(SCOTT)),
    ];
    for f in &mut fichiers {
        f.metadata.as_mut().unwrap().album_artist = None;
    }
    let pistes = importer(&db, dir.path(), &fichiers);
    assert_eq!(pistes[0].album_id, pistes[1].album_id);
    assert_eq!(
        artiste_d_album(&db, &pistes[0]).name,
        WARREN,
        "l'artiste d'album reste celui du premier fichier, sans conjecture"
    );
    assert_eq!(pistes[0].artist_name.as_deref(), Some(WARREN));
    assert_eq!(
        pistes[1].artist_name.as_deref(),
        Some(SCOTT),
        "le deuxième ARTIST n'est pas remplacé par celui de l'album (#3179)"
    );
}

#[test]
fn un_rescan_corrige_le_rattachement_de_piste_ancien_3179() {
    let (dir, db) = base();
    let fichiers = [
        fichier(dir.path(), 1, Some(SCOTT)),
        fichier(dir.path(), 2, Some(WARREN)),
    ];
    let avant = importer(&db, dir.path(), &fichiers);
    // État de l'ancien import : l'invité pointe lui aussi sur l'artiste d'album.
    db.execute(
        "UPDATE tracks SET artist_id = ? WHERE file_path = ?",
        &[&avant[0].artist_id, &fichiers[1].path],
    )
    .unwrap();
    assert_eq!(
        TrackRepo::with_backend(db.clone())
            .get_by_path(&fichiers[1].path)
            .unwrap()
            .unwrap()
            .artist_name
            .as_deref(),
        Some(SCOTT)
    );
    let apres = importer(&db, dir.path(), &fichiers);
    assert_eq!(apres[1].id, avant[1].id, "la ligne existante est conservée");
    assert_eq!(apres[1].album_id, avant[1].album_id);
    assert_eq!(
        apres[1].artist_name.as_deref(),
        Some(WARREN),
        "le rescan doit réparer l'artiste de la piste en base (#3179)"
    );
}
