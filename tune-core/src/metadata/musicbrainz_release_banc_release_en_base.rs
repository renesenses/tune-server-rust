//! Banc : une seule requête de release par album, gardée en base (#4805, A → E).
//!
//! Idée : MetaRust (`cache.rs`, `fetch_release_with_credits`), de Xavier Joly.
//!
//! Mêmes 40 albums et mêmes réponses de recherche enregistrées que le banc
//! #4805 ([`super::banc_4805`]), sans réseau. Une passe « identification +
//! crédits » complète, comptée requête par requête :
//!
//! * **identification** — la recherche de pressages telle qu'`identifier_album`
//!   l'appelle (1 ou 2 requêtes), puis, si un pressage est retenu, son détail ;
//! * **crédits** — la VRAIE passe [`super::super::credits_release`], sur une
//!   base SQLite en mémoire qui porte les albums identifiés.
//!
//! « avant » : le détail sans base (`lookup_release_detail`, comme jusqu'ici),
//! puis une requête de crédits par disque candidat — la boucle d'avant ce lot
//! n'en faisait ni plus ni moins.
//! « après » : le détail par [`super::lookup_release_detail_gardee`] (via sa
//! couture), qui garde la réponse ; la passe des crédits la relit.
//!
//! Le transport des releases est un compteur : il rend une release minimale
//! (`{"id": …, "media": []}`) et note les `inc` demandés.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::banc_4805::{ALBUMS, CANDIDATS, chemin_fixture, rejouer};
use super::{
    INC_CREDITS_RELEASE, INC_DETAIL_RELEASE, INC_RELEASE_COMPLET, LectureRelease, Provenance,
    artiste_de_requete, lire_release_gardee_par, recherche_de_pressages,
};
use crate::db::backend::{DbBackend, ToSqlValue};

/// Une requête MusicBrainz espacée par le limiteur partagé coûte environ
/// 1,1 s (1 s de créneau + la réponse) : le chiffre du constat du 23/09 et
/// de l'analyse MetaRust.
const SECONDES_PAR_REQUETE: f64 = 1.1;

fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn ajouter_album(b: &Arc<dyn DbBackend>, titre: &str, mbid: &str) {
    b.execute(
        "INSERT INTO albums (title, musicbrainz_release_id) VALUES (?, ?)",
        &[&titre as &dyn ToSqlValue, &mbid as &dyn ToSqlValue],
    )
    .unwrap();
}

/// Le transport de releases du banc : compte, note les `inc`, rend une
/// release minimale.
#[derive(Default)]
struct Compteur {
    demandes: RefCell<Vec<(String, String)>>,
}

impl Compteur {
    fn interroger(&self, id: String, inc: &str) -> std::future::Ready<LectureRelease> {
        self.demandes
            .borrow_mut()
            .push((id.clone(), inc.to_string()));
        std::future::ready(LectureRelease::Lue(json!({ "id": id, "media": [] })))
    }
    fn n(&self) -> usize {
        self.demandes.borrow().len()
    }
}

#[derive(Default, Debug)]
struct Bilan {
    recherches: usize,
    details: usize,
    credits: usize,
}

impl Bilan {
    fn total(&self) -> usize {
        self.recherches + self.details + self.credits
    }
}

/// 🔴 Le banc. Imprime le nombre de requêtes avant / après et l'estimation
/// pour 6 000 albums ; garde trois propriétés : la passe des crédits ne fait
/// plus AUCUNE requête sur un album identifié dans la même passe, l'identi-
/// fication n'en fait pas une de plus, et la réponse gardée porte bien les
/// `inc` des crédits.
#[tokio::test]
async fn banc_release_en_base_identification_plus_credits() {
    let brut = std::fs::read_to_string(chemin_fixture()).expect("fixture du banc #4805");
    let fixture: Value = serde_json::from_str(&brut).expect("fixture JSON");
    let reponses: BTreeMap<String, Value> =
        serde_json::from_value(fixture["reponses"].clone()).expect("reponses");

    let base_avant = base();
    let base_apres = base();
    let (mut avant, mut apres) = (Bilan::default(), Bilan::default());
    let (releases_avant, releases_apres) = (Compteur::default(), Compteur::default());
    let mut identifies = 0usize;

    for &(_, titre, album, pistes, n) in ALBUMS {
        // 1. La recherche : la même avant et après (ce lot n'y touche pas).
        let recherches = RefCell::new(0usize);
        let mut rejoue = rejouer(&reponses);
        let compte = |requete: String, fetch: usize| {
            *recherches.borrow_mut() += 1;
            rejoue(requete, fetch)
        };
        let artiste = artiste_de_requete(Some(album), pistes);
        let r = recherche_de_pressages(titre, &artiste, Some(n), CANDIDATS, compte).await;
        let recherches = recherches.into_inner();
        avant.recherches += recherches;
        apres.recherches += recherches;
        let Some(meilleur) = r.meilleur() else {
            continue;
        };
        identifies += 1;
        let mbid = meilleur.release_id.clone();

        // 2. Le détail du pressage retenu.
        // avant : `lookup_release_detail`, sans base — une requête, rien de gardé.
        let _ = releases_avant
            .interroger(mbid.clone(), INC_DETAIL_RELEASE)
            .await;
        avant.details += 1;
        // après : la couture de `lookup_release_detail_gardee`.
        let deja = releases_apres.n();
        let (lecture, provenance) =
            lire_release_gardee_par(&base_apres, &mbid, INC_DETAIL_RELEASE, |id, inc| {
                releases_apres.interroger(id, inc)
            })
            .await;
        assert!(matches!(lecture, LectureRelease::Lue(_)));
        // Deux albums du banc peuvent retenir le même pressage : le second le
        // trouve en base.
        if provenance == Provenance::Reseau {
            assert_eq!(releases_apres.n(), deja + 1);
        }
        apres.details += releases_apres.n() - deja;

        ajouter_album(&base_avant, titre, &mbid);
        ajouter_album(&base_apres, titre, &mbid);
    }

    // 3. Les crédits.
    // avant : la boucle d'avant faisait UNE `lookup_release_credits` par disque
    // candidat, sans exception (`remplir_credits_depuis_musicbrainz` @ base du
    // lot) — le nombre de candidats EST son nombre de requêtes.
    avant.credits = super::super::credits_release::albums_candidats(&base_avant).len();
    // après : la VRAIE passe, sur la base où l'identification a gardé ses
    // réponses.
    let pendant = releases_apres.n();
    let av_apres = super::super::credits_release::remplir_credits_par(
        base_apres.clone(),
        "banc-apres",
        &|_| {},
        |id, inc| releases_apres.interroger(id, inc),
    )
    .await;
    apres.credits = releases_apres.n() - pendant;

    let gain = avant.total() - apres.total();
    let par_album = |b: &Bilan| b.total() as f64 / ALBUMS.len() as f64;
    let taux = identifies as f64 / ALBUMS.len() as f64;
    // Extrapolation de la MESURE : requêtes économisées par album du banc,
    // portées à 6 000 albums de même composition.
    let requetes_6000 = 6000.0 * gain as f64 / ALBUMS.len() as f64;
    let gain_6000 = requetes_6000 * SECONDES_PAR_REQUETE;
    println!("\n| passe identification + crédits | avant | après |\n|---|---:|---:|");
    println!(
        "| recherches | {} | {} |",
        avant.recherches, apres.recherches
    );
    println!(
        "| détails de release | {} | {} |",
        avant.details, apres.details
    );
    println!(
        "| releases relues par les crédits | {} | {} |",
        avant.credits, apres.credits
    );
    println!(
        "| **total** | **{}** | **{}** |",
        avant.total(),
        apres.total()
    );
    println!(
        "albums : {} ; identifiés : {identifies} ; requêtes / album : {:.2} → {:.2} ; \
         économisées : {gain}",
        ALBUMS.len(),
        par_album(&avant),
        par_album(&apres)
    );
    println!(
        "6 000 albums de même composition (identifiés : {:.0} %) : {:.0} requêtes de \
         moins, soit {:.0} s ≈ {} h {:02} min à {SECONDES_PAR_REQUETE} s la requête",
        taux * 100.0,
        requetes_6000,
        gain_6000,
        (gain_6000 / 3600.0) as u64,
        ((gain_6000 % 3600.0) / 60.0) as u64
    );

    assert_eq!(
        avant.credits, av_apres.processed,
        "les deux passes de crédits n'ont pas traité les mêmes disques"
    );
    assert_eq!(av_apres.errors, 0);
    assert_eq!(
        apres.credits, 0,
        "après : la passe des crédits a refait {} requête(s) sur des albums que \
         l'identification venait de lire",
        apres.credits
    );
    assert!(
        apres.details <= avant.details,
        "l'identification fait plus de requêtes qu'avant : {} > {}",
        apres.details,
        avant.details
    );
    assert!(apres.total() < avant.total());
    // Ce qui part sur le réseau après : le jeu complet, une fois.
    assert!(
        releases_apres
            .demandes
            .borrow()
            .iter()
            .all(|(_, inc)| inc == INC_RELEASE_COMPLET),
        "une release est partie sans les `inc` des crédits"
    );
    assert!(super::super::musicbrainz_release_cache::inc_couvre(
        INC_RELEASE_COMPLET,
        INC_CREDITS_RELEASE
    ));
}

/// #4805 D + idée 3 — la lecture du choix d'édition passe par la release
/// gardée : la première lecture part sur le réseau avec les `inc` complets
/// ET ceux du choix (`release-groups`), la seconde est servie par la base, et
/// la réponse gardée couvre aussi la passe des crédits. Un `recording/{id}`
/// n'est jamais gardé.
#[tokio::test]
async fn la_lecture_du_choix_d_edition_est_gardee_en_base() {
    use super::super::choix_de_pressage::INC_DETAIL;
    use super::super::musicbrainz_release_cache::inc_couvre;
    use super::lire_sur_musicbrainz_gardee_par;
    let b = base();
    let demandes: RefCell<Vec<(String, String)>> = RefCell::default();
    let transport = |chemin: String, inc: String| {
        demandes.borrow_mut().push((chemin.clone(), inc));
        std::future::ready(Ok(Some(json!({ "id": chemin, "media": [] }))))
    };
    let mbid = "11111111-2222-3333-4444-555555555555";
    let chemin = format!("release/{mbid}");
    let un = lire_sur_musicbrainz_gardee_par(&b, chemin.clone(), INC_DETAIL, transport)
        .await
        .unwrap();
    assert!(un.is_some());
    assert_eq!(demandes.borrow().len(), 1);
    let inc_reseau = demandes.borrow()[0].1.clone();
    assert!(inc_couvre(&inc_reseau, INC_DETAIL), "{inc_reseau}");
    assert!(inc_couvre(&inc_reseau, INC_RELEASE_COMPLET), "{inc_reseau}");
    let deux = lire_sur_musicbrainz_gardee_par(&b, chemin, INC_DETAIL, transport)
        .await
        .unwrap();
    assert_eq!(deux, un);
    assert_eq!(
        demandes.borrow().len(),
        1,
        "la seconde lecture vient de la base"
    );
    let (credits, provenance) =
        lire_release_gardee_par(&b, mbid, INC_CREDITS_RELEASE, |_, _| async {
            LectureRelease::Panne("ne doit pas partir".into())
        })
        .await;
    assert!(matches!(credits, LectureRelease::Lue(_)));
    assert_eq!(provenance, Provenance::Base);
    for _ in 0..2 {
        lire_sur_musicbrainz_gardee_par(&b, "recording/abc".into(), "releases", transport)
            .await
            .unwrap();
    }
    assert_eq!(
        demandes.borrow().len(),
        3,
        "un enregistrement n'est pas gardé"
    );
}
