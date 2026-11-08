//! #8077 — un album LOCAL complété depuis Qobuz, sur SQLite, par le vrai
//! routeur et une doublure de l'API Qobuz (voir `apports_qobuz_8077/commun.rs`).
//! Jumelle PostgreSQL : `pg_apports_qobuz_8077.rs`.

#[path = "apports_qobuz_8077/commun.rs"]
mod commun;

use commun::Etat;

fn etat() -> Etat {
    Etat::new(":memory:", 0, Default::default()).unwrap()
}

#[tokio::test]
async fn upc_complete_sans_ecraser_puis_retrait_complet_8077() {
    commun::upc_complete_sans_ecraser_puis_retrait(etat(), "q8077").await;
}

#[tokio::test]
async fn titre_douteux_non_applique_puis_confirme_8077() {
    commun::titre_douteux_non_applique_puis_confirme(etat(), "q8077").await;
}

#[tokio::test]
async fn titre_pistes_differentes_non_applique_8077() {
    commun::titre_pistes_differentes_non_applique(etat(), "q8077").await;
}

#[tokio::test]
async fn titre_sur_applique_8077() {
    commun::titre_sur_applique(etat(), "q8077").await;
}

#[tokio::test]
async fn sans_compte_qobuz_aucun_appel_8077() {
    commun::sans_compte_aucun_appel(etat(), "q8077").await;
}
