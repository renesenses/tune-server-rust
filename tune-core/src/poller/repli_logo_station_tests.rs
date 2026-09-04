use super::{station_du_now_playing, vignette_du_pas_radio};
use crate::db::migrations;
use crate::db::radio_repo::RadioRepo;
use crate::db::sqlite::SqliteDb;

const FIP: &str = "https://icecast.radiofrance.fr/fip-hifi.aac";

fn repo_avec_fip(logo: Option<&str>) -> RadioRepo {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let repo = RadioRepo::new(db);
    // Les 24 stations semées le sont par la migration 33 : on ne récrit
    // pas FIP, on lui pose le logo que le rattrapage mozaiklabs lui aurait
    // donné.
    let mut fip = repo
        .list()
        .unwrap()
        .into_iter()
        .find(|s| s.url == FIP)
        .expect("FIP est semée par la migration 33");
    fip.logo_url = logo.map(str::to_string);
    repo.update(&fip).unwrap();
    repo
}

/// LE défaut. `POST /radios/{id}/play/{zone}` écrit dans `source_id`
/// **l'URL du flux**, jamais l'identifiant numérique de la ligne
/// (`tune-server/src/routes/radios.rs`, `play_radio` :
/// `source_id: Some(radio.url.clone())`). Le sondeur, lui, ne cherchait la
/// station que par `source_id.parse::<i64>()`. La branche qui lit
/// `station.logo_url` était donc MORTE sur le chemin de lecture normal :
/// `logo_station` restait `None` pour les 24 stations livrées — y compris
/// les 20 auxquelles le rattrapage mozaiklabs avait bel et bien posé un
/// logo. Le repli n'avait rien à replier parce qu'il ne lisait rien.
#[test]
fn la_station_se_retrouve_par_l_url_du_flux_que_pose_le_play() {
    let repo = repo_avec_fip(Some("https://mozaiklabs.fr/storage/radios/fip.png"));
    let station = station_du_now_playing(&repo, FIP)
        .expect("le play pose l'URL du flux dans source_id : il faut savoir la relire");
    assert_eq!(station.name, "FIP");
    assert_eq!(
        station.logo_url.as_deref(),
        Some("https://mozaiklabs.fr/storage/radios/fip.png")
    );
}

/// L'identifiant numérique reste servi : d'autres appelants peuvent
/// l'écrire, et une station supprimée ne doit pas ressusciter.
#[test]
fn l_identifiant_numerique_continue_de_marcher() {
    let repo = repo_avec_fip(Some("https://mozaiklabs.fr/storage/radios/fip.png"));
    let id = repo
        .list()
        .unwrap()
        .into_iter()
        .find(|s| s.url == FIP)
        .and_then(|s| s.id)
        .unwrap();
    let station = station_du_now_playing(&repo, &id.to_string()).unwrap();
    assert_eq!(station.name, "FIP");
    assert!(station_du_now_playing(&repo, "999999").is_none());
}

/// Une station absente de la base — flux collé à la main, import M3U —
/// ne trouve rien, et ce n'est pas une erreur.
#[test]
fn une_station_inconnue_ne_trouve_rien() {
    let repo = repo_avec_fip(None);
    assert!(station_du_now_playing(&repo, "https://stream.inconnu.example/x.mp3").is_none());
}

/// Le second défaut, et il est écrit noir sur blanc dans le commentaire
/// que le code se donnait à lui-même : « dès qu'un titre a posé sa
/// pochette, `cover_path` la porte, et le titre suivant — une chronique,
/// un jingle — hériterait de la pochette du précédent au lieu de revenir
/// au logo ». C'est exactement ce que faisait le troisième repli
/// `.or_else(|| np.cover_path.clone())`. Mieux vaut le micro générique
/// qu'une pochette fausse : on n'illustre pas le journal de 13 h avec la
/// pochette de la chanson d'avant.
///
/// La pochette courante n'est plus un argument du tout : le pas suivant ne
/// peut donc PLUS hériter de celle du précédent, et c'est le compilateur
/// qui le tient, pas ce test. Ce test-ci garde le résultat.
#[test]
fn un_pas_sans_pochette_ne_recycle_pas_celle_du_titre_precedent() {
    assert_eq!(
        vignette_du_pas_radio(None, None),
        None,
        "sans pochette de titre ni logo de station, il ne faut RIEN afficher"
    );
}

/// Un `logo_url` vide ou blanc en base — import, saisie à la main — n'est
/// pas un logo. `Option::or` ne le voit pas : `Some("")` gagne contre
/// `None` et l'on publie une URL vide.
#[test]
fn un_logo_vide_en_base_ne_compte_pas_pour_un_logo() {
    assert_eq!(vignette_du_pas_radio(None, Some("")), None);
    assert_eq!(vignette_du_pas_radio(None, Some("   ")), None);
    assert_eq!(
        vignette_du_pas_radio(Some(""), Some("https://x/logo.png")),
        Some("https://x/logo.png".to_string()),
        "une pochette de titre vide doit laisser la main au logo"
    );
}

/// Le sens de l'ordre, demandé par Bertrand : « mettre la pochette de
/// l'album et non le logo de la radio ». Garde anti-régression sur
/// 74677e35 / #2109.
#[test]
fn la_pochette_du_titre_passe_avant_le_logo() {
    assert_eq!(
        vignette_du_pas_radio(
            Some("https://api.radiofrance/visual.jpg"),
            Some("https://mozaiklabs.fr/storage/radios/fip.png"),
        ),
        Some("https://api.radiofrance/visual.jpg".to_string())
    );
    assert_eq!(
        vignette_du_pas_radio(None, Some("https://mozaiklabs.fr/storage/radios/fip.png")),
        Some("https://mozaiklabs.fr/storage/radios/fip.png".to_string())
    );
}
