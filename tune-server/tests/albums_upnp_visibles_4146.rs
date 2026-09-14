//! Les albums d'une source UPnP indexée sont VISIBLES, sauf doublon (#4146).
//!
//! ## Ce que l'utilisateur voyait
//!
//! Mesuré sur le `.18` le 14/09/2026, une source Asset UPnP indexée :
//!
//! ```text
//! albums_by_source : { local: 4255, upnp: 51 }
//! tracks_by_source : { local: 46877, upnp: 179 }
//! ```
//!
//! Les 51 albums sont en base, les compteurs les annoncent, et la grille n'en
//! montrait aucun dans ses premières pages. **Ils existent et personne ne les
//! voit.**
//!
//! ## Les deux décisions de Bertrand, 14/09/2026
//!
//! 1. **Ouvrir les listes au `upnp`** — un album d'une source indexée paraît
//!    dans la bibliothèque.
//! 2. **Le LOCAL est prioritaire, et on n'affiche PAS l'autre** — quand un
//!    album existe des deux côtés, seul le local est rendu. Arbitrage de D1
//!    différent de celui que le chantier proposait (un badge « aussi sur X ») :
//!    c'est le **masquage** qui est retenu.
//!
//! ## Les trois propriétés gardées ici
//!
//! | # | Propriété |
//! |---|---|
//! | 1 | un album `upnp` **sans** équivalent local **apparaît** |
//! | 2 | un album `upnp` **avec** équivalent local **n'apparaît pas** — et le local, si |
//! | 3 | la ligne distante masquée **existe toujours en base**, et **redevient visible** si le local disparaît |
//!
//! La troisième est celle qui distingue **masquer** d'**effacer**, et c'est la
//! seule que ni la liste ni le compteur ne peuvent prouver seuls : elle se lit
//! sur `/library/stats` (qui compte TOUTE la table) puis en supprimant le
//! local et en redemandant la liste.
//!
//! ## Ce que cette épreuve mesure
//!
//! Le **corps JSON des ROUTES** — `/library/albums`, `/library/tracks`,
//! `/library/stats` —, jamais une condition SQL. Un test qui rejouerait le
//! prédicat ne garderait rien : il le recopierait. Le banc est posé à la main,
//! on SAIT ce qu'il contient, et l'épreuve exige que les réponses le disent.
//!
//! Le banc porte les **deux natures de distant** délibérément — un doublé, un
//! orphelin. Sans l'orphelin, « rien d'`upnp` ne sort » passerait ; sans le
//! doublé, « tout d'`upnp` sort » passerait aussi. Il faut les deux pour que
//! la seule requête qui verdisse soit la bonne.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Album, Track};
use tune_core::db::track_repo::TrackRepo;
use tune_server::state::AppState;

/// L'album doublé : **le même titre et le même artiste des deux côtés**.
/// C'est le cas que Bertrand tranche — seul le local doit être rendu.
const DOUBLE_TITRE: &str = "Big Calm";
const DOUBLE_ARTISTE: &str = "Morcheeba";

/// L'album distant SANS contrepartie locale : celui que la bibliothèque
/// cachait pour rien, et que la décision n°1 fait apparaître.
const ORPHELIN_TITRE: &str = "Superfly";
const ORPHELIN_ARTISTE: &str = "Curtis Mayfield";

/// Un album local ordinaire — le témoin que l'ouverture aux sources distantes
/// n'a rien retiré à ce qui marchait.
const LOCAL_SEUL_TITRE: &str = "Riot on an Empty Street";
const LOCAL_SEUL_ARTISTE: &str = "Kings of Convenience";

/// Ce que le banc contient, pour que le compteur de `/library/stats` soit
/// vérifiable : 2 albums locaux + 2 albums distants.
const ALBUMS_EN_BASE: i64 = 4;
const ALBUMS_LOCAUX_EN_BASE: i64 = 2;
const ALBUMS_UPNP_EN_BASE: i64 = 2;

/// Ce que la GRILLE doit rendre : tout sauf le doublon distant.
const ALBUMS_VISIBLES: usize = 3;

/// Une piste par album — le banc côté pistes suit le banc côté albums.
const PISTES_EN_BASE: i64 = 4;
const PISTES_VISIBLES: usize = 3;

/// Plancher du détecteur : un banc appauvri doit ROUGIR, pas passer à vide.
///
/// Sans album distant DOUBLÉ, la propriété 2 n'aurait rien à masquer ; sans
/// album distant ORPHELIN, la propriété 1 n'aurait rien à montrer, et un
/// `WHERE source = 'local'` remis en place repasserait au vert.
fn exige_un_banc_des_deux_natures() {
    assert_eq!(
        ALBUMS_EN_BASE,
        ALBUMS_LOCAUX_EN_BASE + ALBUMS_UPNP_EN_BASE,
        "le banc et le total attendu ont divergé"
    );
    assert!(
        ALBUMS_UPNP_EN_BASE >= 2,
        "il faut DEUX albums distants — un doublé, un orphelin — sinon l'épreuve \
         ne départage pas « ouvrir » de « tout ouvrir »"
    );
    assert!(
        ALBUMS_LOCAUX_EN_BASE >= 2,
        "il faut DEUX albums locaux — la contrepartie du doublon, et un local seul"
    );
    assert_eq!(
        ALBUMS_VISIBLES,
        (ALBUMS_EN_BASE - 1) as usize,
        "exactement UN album doit être masqué : le doublon distant"
    );
}

/// Pose le banc et rend `(id du distant doublé, id du local qui le masque)`.
fn poser_le_banc(etat: &AppState) -> (i64, i64) {
    let artistes = ArtistRepo::with_backend(etat.backend.clone());
    let albums = AlbumRepo::with_backend(etat.backend.clone());
    let pistes = TrackRepo::with_backend(etat.backend.clone());

    let mut creer = |titre: &str, artiste: &str, source: &str| -> i64 {
        let artiste_id = artistes
            .get_or_create(artiste, None, None)
            .unwrap_or_else(|e| panic!("artiste {artiste} : {e}"))
            .id;
        let mut album = Album::new(titre.to_string());
        album.artist_id = artiste_id;
        album.source = source.to_string();
        album.track_count = Some(1);
        if source != "local" {
            // Une source indexée porte son identifiant distant, et AUCUN
            // chemin de fichier : c'est précisément ce qui la rend invisible
            // à tout filtre écrit pour le disque.
            album.source_id = Some(format!("upnp:{titre}"));
        }
        let album_id = albums
            .create(&album)
            .unwrap_or_else(|e| panic!("album {titre} ({source}) : {e}"));

        let mut piste = Track::new(format!("{titre} — piste 1"));
        piste.album_id = Some(album_id);
        piste.artist_id = artiste_id;
        piste.source = source.to_string();
        if source == "local" {
            piste.file_path = Some(format!("/musique/{titre}.flac"));
        } else {
            piste.source_id = Some(format!("upnp:{titre}:1"));
        }
        pistes
            .create(&piste)
            .unwrap_or_else(|e| panic!("piste de {titre} ({source}) : {e}"));

        album_id
    };

    let local_doublon = creer(DOUBLE_TITRE, DOUBLE_ARTISTE, "local");
    let distant_double = creer(DOUBLE_TITRE, DOUBLE_ARTISTE, "upnp");
    creer(LOCAL_SEUL_TITRE, LOCAL_SEUL_ARTISTE, "local");
    creer(ORPHELIN_TITRE, ORPHELIN_ARTISTE, "upnp");

    (distant_double, local_doublon)
}

async fn corps_de(app: &Router, chemin: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap_or_else(|e| panic!("{chemin} : routeur en échec : {e}"));
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|e| panic!("{chemin} : corps illisible : {e}"));
    assert_eq!(
        statut,
        StatusCode::OK,
        "{chemin} : statut {statut}, corps {}",
        String::from_utf8_lossy(&octets)
    );
    serde_json::from_slice(&octets).unwrap_or_else(|e| panic!("{chemin} : JSON illisible : {e}"))
}

/// Les `(titre, source)` que la grille rend.
fn grille(corps: &Value) -> Vec<(String, String)> {
    corps["items"]
        .as_array()
        .unwrap_or_else(|| panic!("/library/albums : `items` doit être un tableau — {corps}"))
        .iter()
        .map(|a| {
            (
                a["title"].as_str().unwrap_or_default().to_string(),
                a["source"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// **Propriétés 1 et 2, sur la ROUTE des albums.**
#[tokio::test(flavor = "multi_thread")]
async fn i4146_la_grille_ouvre_le_upnp_et_masque_le_doublon() {
    exige_un_banc_des_deux_natures();

    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    poser_le_banc(&etat);
    let app = tune_server::routes::router(etat);

    let corps = corps_de(&app, "/api/v1/library/albums?limit=100").await;
    let rendus = grille(&corps);

    // Propriété 1 — l'album distant SANS équivalent local apparaît. C'est le
    // défaut de l'issue : sans ouverture, cette ligne est absente.
    assert!(
        rendus
            .iter()
            .any(|(t, s)| t == ORPHELIN_TITRE && s == "upnp"),
        "« {ORPHELIN_TITRE} » (upnp, sans équivalent local) doit PARAÎTRE dans la \
         grille — rendus : {rendus:?}"
    );

    // Propriété 2 — l'album distant DOUBLÉ n'apparaît pas, et le local, si.
    assert!(
        !rendus.iter().any(|(t, s)| t == DOUBLE_TITRE && s == "upnp"),
        "« {DOUBLE_TITRE} » (upnp) est doublé par un local : il ne doit PAS \
         paraître — rendus : {rendus:?}"
    );
    assert!(
        rendus
            .iter()
            .any(|(t, s)| t == DOUBLE_TITRE && s == "local"),
        "« {DOUBLE_TITRE} » (local) doit rester rendu — masquer le distant ne \
         retire rien au local — rendus : {rendus:?}"
    );

    // Le local ordinaire n'a rien perdu à l'ouverture.
    assert!(
        rendus
            .iter()
            .any(|(t, s)| t == LOCAL_SEUL_TITRE && s == "local"),
        "« {LOCAL_SEUL_TITRE} » (local) doit rester rendu — rendus : {rendus:?}"
    );

    assert_eq!(
        rendus.len(),
        ALBUMS_VISIBLES,
        "la grille doit rendre exactement {ALBUMS_VISIBLES} albums — rendus : {rendus:?}"
    );

    // Le `total` de la pagination suit la MÊME exclusion que la liste, sinon
    // la grille saute ou duplique des pages (#1391).
    assert_eq!(
        corps["total"].as_i64(),
        Some(ALBUMS_VISIBLES as i64),
        "le `total` doit compter ce que la liste rend, pas ce que la table \
         contient — corps {corps}"
    );
}

/// **Propriétés 1 et 2, sur la ROUTE des pistes.** La piste suit son album.
#[tokio::test(flavor = "multi_thread")]
async fn i4146_les_pistes_suivent_leur_album() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    poser_le_banc(&etat);
    let app = tune_server::routes::router(etat);

    let corps = corps_de(&app, "/api/v1/library/tracks?limit=100").await;
    let titres: Vec<String> = corps["items"]
        .as_array()
        .unwrap_or_else(|| panic!("/library/tracks : `items` doit être un tableau — {corps}"))
        .iter()
        .map(|t| t["title"].as_str().unwrap_or_default().to_string())
        .collect();

    assert!(
        titres.iter().any(|t| t.starts_with(ORPHELIN_TITRE)),
        "la piste de « {ORPHELIN_TITRE} » (upnp orphelin) doit PARAÎTRE — {titres:?}"
    );
    assert_eq!(
        titres
            .iter()
            .filter(|t| t.starts_with(DOUBLE_TITRE))
            .count(),
        1,
        "« {DOUBLE_TITRE} » a une piste locale et une piste distante : UNE SEULE \
         doit paraître, celle du local — {titres:?}"
    );
    assert_eq!(
        titres.len(),
        PISTES_VISIBLES,
        "la vue pistes doit rendre exactement {PISTES_VISIBLES} pistes — {titres:?}"
    );
    assert_eq!(
        corps["total"].as_i64(),
        Some(PISTES_VISIBLES as i64),
        "le `total` des pistes doit compter ce que la liste rend — corps {corps}"
    );
}

/// **Propriété 3 — masquer n'est pas effacer.**
///
/// Deux preuves, parce qu'aucune ne suffit seule :
///
/// 1. `/library/stats` compte TOUTE la table et annonce toujours les 2 albums
///    `upnp` et les 4 pistes : la ligne masquée est bien encore là ;
/// 2. on SUPPRIME la contrepartie locale, et le distant **revient** dans la
///    grille sans qu'on ait rien réindexé.
///
/// Sans la seconde, « la ligne est en base » ne dirait pas qu'elle est
/// récupérable : un masquage écrit en dur (un drapeau posé à l'indexation)
/// passerait la première et échouerait la seconde.
#[tokio::test(flavor = "multi_thread")]
async fn i4146_le_masque_n_efface_rien_et_se_leve() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let (distant_double, local_doublon) = poser_le_banc(&etat);
    let app = tune_server::routes::router(etat.clone());

    // 1. La table entière, telle que les compteurs la voient.
    let stats = corps_de(&app, "/api/v1/library/stats").await;
    assert_eq!(
        stats["albums"].as_i64(),
        Some(ALBUMS_EN_BASE),
        "les compteurs comptent la table ENTIÈRE, masqués compris — {stats}"
    );
    assert_eq!(
        stats["albums_by_source"]["upnp"].as_i64(),
        Some(ALBUMS_UPNP_EN_BASE),
        "les DEUX albums upnp doivent rester comptés — le masqué est en base — {stats}"
    );
    assert_eq!(
        stats["tracks"].as_i64(),
        Some(PISTES_EN_BASE),
        "aucune piste n'a été effacée — {stats}"
    );

    // La fiche de l'album masqué répond toujours : il est consultable et
    // jouable, il n'est qu'absent des LISTES.
    let fiche = corps_de(&app, &format!("/api/v1/library/albums/{distant_double}")).await;
    assert_eq!(
        fiche["source"].as_str(),
        Some("upnp"),
        "l'album masqué doit rester consultable par son identifiant — {fiche}"
    );

    // 2. Le local disparaît (fichier retiré, racine démontée, purge de scan).
    AlbumRepo::with_backend(etat.backend.clone())
        .delete(local_doublon)
        .expect("suppression de la contrepartie locale");

    let apres = corps_de(&app, "/api/v1/library/albums?limit=100").await;
    let rendus = grille(&apres);
    assert!(
        rendus.iter().any(|(t, s)| t == DOUBLE_TITRE && s == "upnp"),
        "le local disparu, le distant doit REDEVENIR visible sans réindexation — \
         rendus : {rendus:?}"
    );
    assert_eq!(
        apres["total"].as_i64(),
        Some(ALBUMS_VISIBLES as i64),
        "le total suit : un album de moins, un démasqué — corps {apres}"
    );
}
