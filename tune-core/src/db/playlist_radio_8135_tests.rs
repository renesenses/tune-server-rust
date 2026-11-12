//! #8135 — une radio comme élément d'une playlist : rangement, lecture,
//! station retirée.
use super::*;
use crate::db::migrations::run_migrations;
use crate::db::playlist_repo::PlaylistRepo;
use crate::db::profile_repo::ProfileRepo;
use crate::db::sqlite::SqliteDb;

const FIP: &str = "https://icecast.radiofrance.fr/fip-hifi.aac";

fn base() -> Arc<dyn DbBackend> {
    let sql = SqliteDb::open_in_memory().unwrap();
    sql.init_schema().unwrap();
    run_migrations(&sql).unwrap();
    Arc::new(sql)
}

fn station(db: &Arc<dyn DbBackend>, nom: &str, url: &str) -> RadioStation {
    let mut radio = RadioStation {
        id: None,
        name: nom.into(),
        url: url.into(),
        homepage: None,
        logo_url: Some(format!("https://logos.invalid/{nom}.png")),
        country: None,
        language: None,
        genre: None,
        codec: None,
        bitrate: None,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    };
    radio.id = Some(RadioRepo::with_backend(db.clone()).create(&radio).unwrap());
    radio
}

fn titre(source: &str, id: &str) -> ServiceEntry {
    ServiceEntry {
        source: source.into(),
        source_id: id.into(),
        title: format!("Titre {id}"),
        artist: Some("Artiste".into()),
        album: None,
        album_source_id: None,
        duration_ms: Some(180_000),
        cover_url: None,
    }
}

/// Une playlist « pistes + radio » se range et se relit, radio en dernier.
fn playlist_avec_radio(db: &Arc<dyn DbBackend>, radio: &RadioStation) -> i64 {
    let profil = ProfileRepo::with_backend(db.clone())
        .create("reveil-8135", None, None)
        .unwrap();
    let repo = PlaylistRepo::with_backend(db.clone());
    let id = repo.create("Réveil", None, profil).unwrap();
    let ecrites = repo
        .add_entries_deduped(
            id,
            &[
                EntryContent::Service(titre("qobuz", "q-1")),
                EntryContent::Service(ligne_de_radio(radio).unwrap()),
            ],
            None,
        )
        .unwrap();
    assert_eq!(ecrites.len(), 2);
    id
}

#[test]
fn une_radio_se_range_dans_une_playlist_et_garde_sa_place_8135() {
    let db = base();
    let fip = station(&db, "FIP", FIP);
    let id = playlist_avec_radio(&db, &fip);
    let lignes = PlaylistRepo::with_backend(db.clone())
        .get_entries(id)
        .unwrap();
    assert_eq!(lignes.len(), 2);
    let radio = lignes[1]
        .service()
        .expect("la radio est une ligne de service");
    assert!(est_une_radio(radio));
    assert_eq!(station_de_la_ligne(radio), fip.id);
    assert_eq!(radio.title, "FIP");

    // Ajoutée deux fois, elle n'entre qu'une fois (même clef de dédoublonnage).
    let encore = PlaylistRepo::with_backend(db.clone())
        .add_entries_deduped(
            id,
            &[EntryContent::Service(ligne_de_radio(&fip).unwrap())],
            None,
        )
        .unwrap();
    assert!(encore.is_empty());
}

#[test]
fn a_la_lecture_la_radio_devient_son_flux_en_fin_de_file_8135() {
    let db = base();
    let fip = station(&db, "FIP", FIP);
    let id = playlist_avec_radio(&db, &fip);
    let entries = PlaylistRepo::with_backend(db.clone())
        .get_entries(id)
        .unwrap();
    let jouees = lignes_a_jouer(&db, entries.clone(), &flux_enregistre());

    // Le titre de service ne bouge pas.
    assert_eq!(jouees[0], entries[0]);
    // La radio porte son FLUX, comme `play_radio` le passe à l'orchestrateur :
    // sans cela la file recevait `source_id = "1"`, une adresse qui ne joue rien.
    let flux = jouees[1].service().expect("la radio reste jouable");
    assert_eq!(flux.source, "radio");
    assert_eq!(flux.source_id, FIP);
    assert_eq!(flux.title, "FIP");
    assert_eq!(flux.artist.as_deref(), Some("Live Radio"));
    assert_eq!(flux.duration_ms, None, "un direct n'a pas de durée");
    assert_eq!(jouees[1].id, entries[1].id, "la ligne garde son rang");
}

#[test]
fn le_flux_suit_le_choix_installe_comme_une_radio_jouee_seule_8135() {
    let db = base();
    let fip = station(&db, "FIP", FIP);
    let id = playlist_avec_radio(&db, &fip);
    let entries = PlaylistRepo::with_backend(db.clone())
        .get_entries(id)
        .unwrap();
    let choix: ChoixDuFlux = Arc::new(|_r: &RadioStation| Some("https://flux.invalid/fr".into()));
    let jouees = lignes_a_jouer(&db, entries.clone(), &choix);
    assert_eq!(
        jouees[1].service().unwrap().source_id,
        "https://flux.invalid/fr"
    );

    // Flux refusé (pays exclu) : la ligne est sautée, jamais jouée à tort.
    let refus: ChoixDuFlux = Arc::new(|_r: &RadioStation| None);
    let jouees = lignes_a_jouer(&db, entries, &refus);
    assert!(jouees[1].missing().is_some());
}

#[test]
fn une_station_retiree_reste_visible_et_se_saute_sans_casser_la_lecture_8135() {
    let db = base();
    let fip = station(&db, "FIP", FIP);
    let id = playlist_avec_radio(&db, &fip);
    RadioRepo::with_backend(db.clone())
        .delete(fip.id.unwrap())
        .unwrap();
    let entries = PlaylistRepo::with_backend(db.clone())
        .get_entries(id)
        .unwrap();
    assert_eq!(entries.len(), 2, "la ligne reste dans la playlist");

    let vue = decrire(&db, entries[1].service().unwrap());
    assert_eq!(vue["radio"], true);
    assert_eq!(vue["unavailable"], true);
    assert_eq!(vue["title"], "FIP");
    assert_eq!(vue["radio_id"], json!(fip.id));

    let jouees = lignes_a_jouer(&db, entries, &flux_enregistre());
    let m = jouees[1]
        .missing()
        .expect("indisponible = introuvable à la lecture");
    assert_eq!(m.title.as_deref(), Some("FIP"));
    assert!(jouees[0].service().is_some(), "le reste se joue");
}

#[test]
fn une_station_presente_est_decrite_disponible_8135() {
    let db = base();
    let fip = station(&db, "FIP", FIP);
    let vue = decrire(&db, &ligne_de_radio(&fip).unwrap());
    assert_eq!(vue["unavailable"], false);
    assert_eq!(vue["source"], "radio");
    assert_eq!(vue["cover_path"], "https://logos.invalid/FIP.png");
}

#[test]
fn une_ancienne_ligne_radio_portant_une_adresse_se_joue_telle_quelle_8135() {
    let db = base();
    let e = PlaylistEntry {
        id: 9,
        position: 0,
        content: EntryContent::Service(titre("radio", FIP)),
    };
    let jouees = lignes_a_jouer(&db, vec![e], &flux_enregistre());
    assert_eq!(jouees[0].service().unwrap().source_id, FIP);
    assert_eq!(decrire(&db, &titre("radio", FIP))["unavailable"], false);
}

#[test]
fn export_et_import_rangent_une_radio_comme_une_radio_8135() {
    // Une adresse qu'aucune station livrée avec Tune ne porte : l'import
    // prend la PREMIÈRE station de même adresse (`get_by_url`).
    const PROPRE: &str = "https://flux.invalid/radio-8135.aac";
    let db = base();
    let fip = station(&db, "FIP", PROPRE);
    let ligne = ligne_de_radio(&fip).unwrap();
    assert_eq!(adresse_a_exporter(&db, &ligne).as_deref(), Some(PROPRE));
    assert_eq!(adresse_a_exporter(&db, &titre("qobuz", "1")), None);

    // L'adresse exportée redevient la même radio à l'import.
    assert_eq!(
        station_d_une_adresse(&db, &format!(" {PROPRE} ")),
        Some(ligne)
    );
    assert_eq!(
        station_d_une_adresse(&db, "https://inconnue.invalid/flux"),
        None
    );
    assert_eq!(station_d_une_adresse(&db, "/musique/a.flac"), None);

    RadioRepo::with_backend(db.clone())
        .delete(fip.id.unwrap())
        .unwrap();
    assert_eq!(
        adresse_a_exporter(&db, &ligne_de_radio(&fip).unwrap()),
        None
    );
}
