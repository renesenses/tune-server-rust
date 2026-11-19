//! Lignes `auth_tokens_<service>` scellées par le coffre : écriture scellée,
//! migration transparente d'une ligne en clair au démarrage, ligne illisible
//! laissée en place. Faux TIDAL injoignable : aucun appel réseau abouti.
use super::*;
use crate::db::sqlite::SqliteDb;
use crate::streaming::tidal::TidalService;

const CLE: &str = "auth_tokens_tidal";
const LIGNE_EN_CLAIR: &str = r#"{"access_token":"jeton-acces-temoin","refresh_token":"jeton-rafraichissement-temoin","username":"temoin","country_code":"FR","user_id":42}"#;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Un registre avec un TIDAL qui ne joint rien (port fermé) : `post_restore`
/// échoue vite, sans effacer la session.
fn registre() -> ServiceRegistry {
    let mut r = ServiceRegistry::new();
    let mut svc = TidalService::pour_un_faux_tidal("http://127.0.0.1:9".into());
    // Aucun jeton au départ : seule la ligne restaurée en apporte.
    StreamingService::restore_tokens(&mut svc, &serde_json::json!({}));
    r.register(Box::new(svc));
    r
}

async fn connecte(r: &ServiceRegistry) -> bool {
    let svc = r.get("tidal").unwrap();
    let svc = svc.read().await;
    svc.save_tokens()
        .is_some_and(|t| t["access_token"] == "jeton-acces-temoin")
}

/// Une ligne écrite en clair par une version antérieure : restaurée telle
/// quelle (personne n'est déconnecté), puis réécrite scellée.
#[tokio::test]
async fn une_ligne_en_clair_est_restauree_puis_scellee() {
    let dir = tempfile::tempdir().unwrap();
    let coffre = Coffre::new(dir.path());
    let db = base();
    let reglages = SettingsRepo::with_backend(db.clone());
    reglages.set(CLE, LIGNE_EN_CLAIR).unwrap();

    let r = registre();
    r.restore_all_tokens_avec(&db, Some(&coffre)).await;
    assert!(connecte(&r).await, "session restaurée");

    let stockee = reglages.get(CLE).unwrap().unwrap();
    assert!(est_scelle(&stockee), "ligne non scellée : {stockee}");
    assert!(!stockee.contains("jeton-acces-temoin"));
    assert!(!stockee.contains("jeton-rafraichissement-temoin"));
    assert_eq!(coffre.ouvrir(&stockee).unwrap(), LIGNE_EN_CLAIR);

    // Au démarrage suivant, la ligne scellée se restaure de même.
    let r2 = registre();
    r2.restore_all_tokens_avec(&db, Some(&coffre)).await;
    assert!(
        connecte(&r2).await,
        "session restaurée depuis la ligne scellée"
    );
}

/// La sauvegarde scelle, et une ligne inchangée n'est pas réécrite (le
/// scellement change à chaque fois : on compare le contenu).
#[tokio::test]
async fn la_sauvegarde_scelle_et_ne_reecrit_pas_une_ligne_inchangee() {
    let dir = tempfile::tempdir().unwrap();
    let coffre = Coffre::new(dir.path());
    let db = base();
    let reglages = SettingsRepo::with_backend(db.clone());
    reglages.set(CLE, LIGNE_EN_CLAIR).unwrap();
    let r = registre();
    r.restore_all_tokens_avec(&db, Some(&coffre)).await;

    r.save_all_tokens_avec(&db, Some(&coffre)).await;
    let premiere = reglages.get(CLE).unwrap().unwrap();
    assert!(est_scelle(&premiere));
    r.save_all_tokens_avec(&db, Some(&coffre)).await;
    assert_eq!(reglages.get(CLE).unwrap().unwrap(), premiere);
}

/// Contre-épreuve : sans coffre (outils, essais), rien ne change — la ligne
/// reste en clair, comme avant.
#[tokio::test]
async fn sans_coffre_la_ligne_reste_en_clair() {
    let db = base();
    let reglages = SettingsRepo::with_backend(db.clone());
    reglages.set(CLE, LIGNE_EN_CLAIR).unwrap();
    let r = registre();
    r.restore_all_tokens_avec(&db, None).await;
    assert!(connecte(&r).await);
    assert_eq!(reglages.get(CLE).unwrap().unwrap(), LIGNE_EN_CLAIR);
    r.save_all_tokens_avec(&db, None).await;
    assert!(!est_scelle(&reglages.get(CLE).unwrap().unwrap()));
}

/// Une ligne scellée sous une AUTRE clé maîtresse : ni restaurée, ni
/// effacée, ni réécrite.
#[tokio::test]
async fn une_ligne_illisible_reste_en_place() {
    let autre = tempfile::tempdir().unwrap();
    let scellee = Coffre::new(autre.path()).sceller(LIGNE_EN_CLAIR).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let coffre = Coffre::new(dir.path());
    let db = base();
    let reglages = SettingsRepo::with_backend(db.clone());
    reglages.set(CLE, &scellee).unwrap();

    let r = registre();
    r.restore_all_tokens_avec(&db, Some(&coffre)).await;
    assert!(!connecte(&r).await);
    assert_eq!(reglages.get(CLE).unwrap().unwrap(), scellee);
    assert_eq!(
        ouvrir_les_jetons(Some(&coffre), &scellee),
        LigneDeJetons::Illisible
    );
}

#[test]
fn ouvrir_et_sceller_les_jetons() {
    let dir = tempfile::tempdir().unwrap();
    let coffre = Coffre::new(dir.path());
    let scellee = sceller_les_jetons(Some(&coffre), LIGNE_EN_CLAIR);
    assert_eq!(
        ouvrir_les_jetons(Some(&coffre), &scellee),
        LigneDeJetons::Scellee(LIGNE_EN_CLAIR.into())
    );
    assert_eq!(
        ouvrir_les_jetons(Some(&coffre), LIGNE_EN_CLAIR),
        LigneDeJetons::EnClair(LIGNE_EN_CLAIR.into())
    );
    assert_eq!(ouvrir_les_jetons(None, &scellee), LigneDeJetons::Illisible);
    assert_eq!(sceller_les_jetons(None, LIGNE_EN_CLAIR), LIGNE_EN_CLAIR);
}
