//! Une clé d'API enregistrée AVANT la jonction avec le coffre partagé
//! (`tune_core::coffre`, décision du 12/10/2026) reste lisible après la mise à
//! jour, sans que l'utilisateur ait rien à ressaisir, puis passe sous la clé
//! maîtresse partagée du serveur (`tune-secrets.cle`).
//!
//! La valeur scellée et la clé maîtresse ci-dessous ont été produites par
//! l'ANCIEN coffre du greffon (`plugins/tune-assistant-ia/src/coffre.rs` de
//! `chantier/assistant-ia`, commit 7502ed7be), tel qu'un utilisateur de ce
//! chantier les a sur disque et en base. Ce ne sont pas des secrets : un
//! tirage d'essai, sur une clé d'API fictive.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_assistant_ia::coffre::{self, FICHIER_CLE};
use tune_assistant_ia::fournisseur::{
    Acces, Demande, ErreurFournisseur, Fabrique, Fournisseur, Reponse,
};
use tune_assistant_ia::reglages::{CLE_API_CHIFFREE, CLE_FOURNISSEUR, EtatCle, cle_en_clair};
use tune_assistant_ia::{Etat, Passerelle, router_vers};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;

/// La clé d'API fictive que l'ancien coffre a scellée.
const CLAIR: &str = "sk-ant-api03-AVANT-LA-JONCTION-0123456789-wxyz";
/// Le contenu de `assistant-ia.cle` qui l'a scellée.
const ANCIENNE_CLE_MAITRESSE: &str =
    "5af0369322c39fb2cfbea2e8846faffbc5b142230a6c266e1e885d30cfeee2b8";
/// La valeur rangée sous `assistant_ia_api_key_chiffree` par l'ancien coffre.
const SCELLE_AVANT: &str = r#"{"v":1,"nonce":"64f5584d2312e597e047aaa74c64fb7f008213324aad347f","ct":"c64c8713509cd9e7c967cdb236f35c602cc2845d416165668f84412bee841999859f9a1850adb62762d902c10e4a2417f8b508ad90422f166ef5913610ec"}"#;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Le serveur d'un utilisateur du chantier, tel qu'il est avant la mise à
/// jour : l'ancienne clé maîtresse à côté de la base, la clé scellée en base.
fn serveur_d_avant() -> (Arc<dyn DbBackend>, tempfile::TempDir) {
    let db = base();
    let dossier = tempfile::tempdir().unwrap();
    std::fs::write(dossier.path().join(FICHIER_CLE), ANCIENNE_CLE_MAITRESSE).unwrap();
    let s = SettingsRepo::with_backend(db.clone());
    s.set(CLE_FOURNISSEUR, "anthropic").unwrap();
    s.set(CLE_API_CHIFFREE, SCELLE_AVANT).unwrap();
    (db, dossier)
}

fn reglage(db: &Arc<dyn DbBackend>, nom: &str) -> Option<String> {
    SettingsRepo::with_backend(db.clone()).get(nom).unwrap()
}

/// Le faux fournisseur retient la clé avec laquelle l'appel est parti.
#[derive(Clone, Default)]
struct Temoin {
    cle_recue: Arc<Mutex<Option<String>>>,
}

struct Faux;

#[async_trait]
impl Fournisseur for Faux {
    async fn completer(&self, _d: &Demande) -> Result<Reponse, ErreurFournisseur> {
        Ok(Reponse {
            texte: "{\"ok\": true}".into(),
            jetons_entree: 0,
            jetons_sortie: 0,
        })
    }
}

impl Temoin {
    fn fabrique(&self) -> Fabrique {
        let t = self.clone();
        Arc::new(move |a: Acces| -> Box<dyn Fournisseur> {
            *t.cle_recue.lock().unwrap() = a.cle.clone();
            Box::new(Faux)
        })
    }
}

async fn appel(app: &axum::Router, methode: &str, uri: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(methode)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let rep = app.clone().oneshot(req).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    (statut, String::from_utf8(octets.to_vec()).unwrap())
}

/// Le chemin du démarrage (`AssistantIaPlugin::setup`) : la passerelle migre,
/// puis l'état des routes ouvre le coffre du greffon.
#[tokio::test]
async fn une_cle_enregistree_avant_la_mise_a_jour_reste_lisible_et_sert_aux_appels() {
    let (db, dossier) = serveur_d_avant();

    Passerelle::new(db.clone(), dossier.path()).migrer();

    // 1. Lisible, telle quelle.
    match cle_en_clair(&db, &coffre::du_greffon(dossier.path())) {
        EtatCle::Ouverte(k) => assert_eq!(k, CLAIR),
        EtatCle::Illisible => panic!("la clé d'avant la mise à jour ne s'ouvre plus"),
        EtatCle::Absente => panic!("la clé d'avant la mise à jour a disparu"),
    }

    // 2. Passée sous la clé PARTAGÉE : le coffre de tune-core l'ouvre seul,
    //    sans l'ancien fichier.
    let apres = reglage(&db, CLE_API_CHIFFREE).unwrap();
    assert_ne!(apres, SCELLE_AVANT, "la valeur doit avoir été rescellée");
    assert!(!apres.contains("AVANT"), "{apres}");
    assert!(dossier.path().join(tune_core::coffre::FICHIER_CLE).exists());
    assert_eq!(
        tune_core::coffre::Coffre::new(dossier.path())
            .ouvrir(&apres)
            .unwrap(),
        CLAIR
    );

    // 3. Les routes : l'écran voit la clé posée, ses quatre derniers
    //    caractères seulement ; un appel part avec la bonne clé.
    let temoin = Temoin::default();
    let etat = Etat::new(
        db.clone(),
        dossier.path(),
        dossier.path().join("cache"),
        Vec::new(),
        temoin.fabrique(),
    );
    let app = router_vers(etat);
    let (s, corps) = appel(&app, "GET", "/reglages").await;
    assert_eq!(s, StatusCode::OK, "{corps}");
    assert!(
        !corps.contains(CLAIR),
        "la clé ne sort jamais en clair : {corps}"
    );
    assert!(!corps.contains("AVANT-LA-JONCTION"), "{corps}");
    let v: Value = serde_json::from_str(&corps).unwrap();
    assert_eq!(v["cle_definie"], true, "{v}");
    assert_eq!(v["cle_illisible"], false, "{v}");
    assert_eq!(v["cle_fin"], "wxyz", "{v}");

    let (s, corps) = appel(&app, "POST", "/tester").await;
    assert_eq!(s, StatusCode::OK, "{corps}");
    assert_eq!(temoin.cle_recue.lock().unwrap().as_deref(), Some(CLAIR));

    // 4. Idempotent : un second démarrage ne réécrit rien.
    Passerelle::new(db.clone(), dossier.path()).migrer();
    assert_eq!(reglage(&db, CLE_API_CHIFFREE).unwrap(), apres);
}

/// Même si le passage n'a pas pu réécrire la valeur (disque plein, base en
/// lecture seule), l'ancienne clé maîtresse l'ouvre encore : rien n'est perdu.
#[test]
fn sans_le_passage_l_ancienne_cle_ouvre_encore() {
    let (db, dossier) = serveur_d_avant();
    match cle_en_clair(&db, &coffre::du_greffon(dossier.path())) {
        EtatCle::Ouverte(k) => assert_eq!(k, CLAIR),
        _ => panic!("la clé d'avant la mise à jour doit s'ouvrir sans migration"),
    }
    assert_eq!(
        reglage(&db, CLE_API_CHIFFREE).as_deref(),
        Some(SCELLE_AVANT)
    );
}

/// Une clé posée APRÈS la mise à jour part directement sous la clé partagée :
/// l'ancien fichier n'y est pour rien.
#[test]
fn une_cle_posee_apres_la_mise_a_jour_est_scellee_sous_la_cle_partagee() {
    let db = base();
    let dossier = tempfile::tempdir().unwrap();
    let c = coffre::du_greffon(dossier.path());
    tune_assistant_ia::reglages::appliquer(
        &db,
        &c,
        &tune_assistant_ia::reglages::Modification {
            fournisseur: Some("anthropic".into()),
            cle: Some(CLAIR.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let scelle = reglage(&db, CLE_API_CHIFFREE).unwrap();
    assert!(!scelle.contains("AVANT"), "{scelle}");
    assert!(
        !dossier.path().join(FICHIER_CLE).exists(),
        "l'ancien fichier ne naît plus"
    );
    assert_eq!(
        tune_core::coffre::Coffre::new(dossier.path())
            .ouvrir(&scelle)
            .unwrap(),
        CLAIR
    );
}
