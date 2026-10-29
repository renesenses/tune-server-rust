//! #7051 — Radio Bergeijk (fil forum 2443) : titre et artiste affichés, fond
//! gris à la place du logo.
//!
//! Les formes viennent des vraies données relevées le 27/10 (anonymisées) :
//! la fiche de l'annuaire porte un logo RELATIF que le serveur rend absolu
//! (`https://mozaiklabs.fr/storage/radio-logos/radio-metadata/S…webp`), son
//! flux est un Ogg FLAC dont les titres passent par les commentaires du flux,
//! sans pochette ; et la station n'est PAS dans « Mes radios ».
//!
//! ⚠ La mémoire des logos de lancement est GLOBALE au processus, et les tests
//! tournent en parallèle : chaque test prend une adresse de flux à lui.

use super::logo_de_lancement::{
    Logos, STATIONS_RETENUES_AU_PLUS, logo_de_la_station, retenir_dans,
    retenir_le_logo_de_lancement,
};
use super::{station_du_now_playing, vignette_du_pas_radio};
use crate::db::migrations;
use crate::db::radio_repo::RadioRepo;
use crate::db::sqlite::SqliteDb;

const LOGO_FICHE: &str = "https://mozaiklabs.fr/storage/radio-logos/radio-metadata/S009901.webp";

fn mes_radios() -> RadioRepo {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    RadioRepo::new(db)
}

/// LE défaut. `POST /radios/directory/{id}/play/{zone}` lance la station avec
/// le logo de sa fiche, mais elle n'est pas en base : le sondeur ne trouvait
/// aucun logo et le premier titre du flux (sans pochette) effaçait la
/// vignette.
#[test]
fn la_station_jouee_depuis_l_annuaire_garde_son_logo_au_premier_titre() {
    let flux = "https://stream.radio-exemple-7051.nl/listen/radio_exemple/flac";
    let repo = mes_radios();
    assert!(
        station_du_now_playing(&repo, flux).is_none(),
        "la station doit être absente de « Mes radios », comme celle du ticket"
    );

    // Ce que fait `jouer_un_flux` au lancement.
    retenir_le_logo_de_lancement(flux, Some(LOGO_FICHE));

    // Ce que fait le sondeur quand le titre arrive : pas de pochette.
    let logo = logo_de_la_station(station_du_now_playing(&repo, flux).as_ref(), flux);
    assert_eq!(
        vignette_du_pas_radio(None, logo.as_deref()).as_deref(),
        Some(LOGO_FICHE),
        "station hors de « Mes radios » : le premier titre du flux efface le logo \
         avec lequel elle a été lancée (fond gris, #7051)"
    );
}

/// Le sondeur lit l'adresse telle que la lecture l'a posée ; une variation
/// de schéma, de casse ou de barre finale ne perd pas le logo.
#[test]
fn le_logo_se_retrouve_a_la_normalisation_pres() {
    retenir_le_logo_de_lancement(
        "https://stream.radio-exemple-7051b.nl/listen/radio_exemple/flac",
        Some(LOGO_FICHE),
    );
    assert_eq!(
        logo_de_la_station(
            None,
            "http://Stream.Radio-Exemple-7051b.nl/listen/radio_exemple/flac/"
        )
        .as_deref(),
        Some(LOGO_FICHE)
    );
}

/// La pochette du titre, quand la station en donne une, reste prioritaire.
#[test]
fn la_pochette_du_titre_reste_prioritaire() {
    let flux = "https://stream.radio-exemple-7051c.nl/listen/radio_exemple/flac";
    retenir_le_logo_de_lancement(flux, Some(LOGO_FICHE));
    let logo = logo_de_la_station(None, flux);
    assert_eq!(
        vignette_du_pas_radio(Some("https://pochettes.example/titre.jpg"), logo.as_deref())
            .as_deref(),
        Some("https://pochettes.example/titre.jpg")
    );
}

/// Une station de « Mes radios » garde le logo de sa ligne : c'est lui que
/// l'utilisateur a pu choisir.
#[test]
fn le_logo_de_mes_radios_passe_avant_celui_du_lancement() {
    let flux = "https://icecast.radiofrance.fr/fip-hifi.aac";
    let repo = mes_radios();
    let mut fip = station_du_now_playing(&repo, flux).expect("FIP est semée par la migration 33");
    fip.logo_url = Some("https://exemple.local/logo-choisi.png".into());
    repo.update(&fip).unwrap();

    retenir_le_logo_de_lancement(flux, Some(LOGO_FICHE));
    assert_eq!(
        logo_de_la_station(station_du_now_playing(&repo, flux).as_ref(), flux).as_deref(),
        Some("https://exemple.local/logo-choisi.png")
    );
}

/// Relancé sans logo, le flux n'hérite pas de celui d'un lancement précédent.
#[test]
fn un_lancement_sans_logo_efface_le_precedent() {
    let flux = "https://stream.radio-exemple-7051d.nl/listen/radio_exemple/flac";
    retenir_le_logo_de_lancement(flux, Some(LOGO_FICHE));
    retenir_le_logo_de_lancement(flux, Some("  "));
    assert_eq!(logo_de_la_station(None, flux), None);
}

/// Le sondeur prend bien le logo par [`logo_de_la_station`] : relire
/// `station.logo_url` seul remettrait le fond gris sans qu'aucun témoin
/// ci-dessus ne le voie. La fonction est définie ailleurs : sa présence dans
/// `radio.rs` ne peut venir que de l'appel.
#[test]
fn le_sondeur_prend_le_logo_par_logo_de_la_station() {
    let sondeur = include_str!("radio.rs");
    let debut = sondeur
        .find("let station = station_du_now_playing(")
        .expect("le sondeur cherche la station en base");
    let pochette = sondeur
        .find("vignette_du_pas_radio(meta.cover_url.as_deref(), logo_station.as_deref())")
        .expect("le sondeur choisit la vignette du pas");
    let entre = &sondeur[debut..pochette];
    assert!(
        entre.contains("let logo_station =")
            && entre
                .contains("logo_de_lancement::logo_de_la_station(station.as_ref(), &stream_url)"),
        "le sondeur doit prendre le logo par logo_de_la_station, sinon une station \
         hors de « Mes radios » perd son logo au premier titre (#7051)"
    );
}

/// Revue de #7051 : la mémoire pleine ne se vide plus d'un coup. Seul le
/// lancement le plus ancien est oublié ; la station lancée en dernier, celle
/// qui joue, garde son logo. Sur une mémoire locale, pas la globale.
#[test]
fn la_memoire_pleine_n_oublie_que_le_plus_ancien_lancement() {
    let mut logos = Logos::new();
    for i in 0..STATIONS_RETENUES_AU_PLUS as u64 {
        retenir_dans(&mut logos, format!("flux-{i}"), LOGO_FICHE, i + 1);
    }
    let rang = STATIONS_RETENUES_AU_PLUS as u64 + 1;
    retenir_dans(&mut logos, "flux-qui-joue".into(), LOGO_FICHE, rang);
    assert_eq!(logos.len(), STATIONS_RETENUES_AU_PLUS);
    assert!(
        logos.contains_key("flux-qui-joue"),
        "la station lancée en dernier doit garder son logo"
    );
    assert!(
        !logos.contains_key("flux-0"),
        "le lancement le plus ancien est celui qu'on oublie"
    );
    assert!(
        logos.contains_key("flux-1"),
        "mémoire pleine : un seul lancement est oublié, pas toute la mémoire"
    );
}
