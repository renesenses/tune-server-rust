//! Clé du serveur : coffre, inscription sur le compte (#6603).

use std::sync::Mutex;

use super::*;
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn reglages(b: &Arc<dyn DbBackend>) -> SettingsRepo {
    SettingsRepo::with_backend(b.clone())
}

/// Serveur relié à un compte (`server_id`, session SSO, jeton de liaison),
/// derrière un relais qui ouvre des sessions par laissez-passer.
fn relie(b: &Arc<dyn DbBackend>) {
    let s = reglages(b);
    s.set(LAISSEZ_PASSER_ACTIF, "true").unwrap();
    s.set("server_id", "srv-1").unwrap();
    s.set("mozaik_access_token", "jeton-du-compte").unwrap();
    s.set(crate::cloud::library_sync::CLE_JETON_DE_LIAISON, "liaison")
        .unwrap();
}

#[derive(Default)]
struct FauxSite {
    recus: Mutex<Vec<(String, String, String, CorpsInscription)>>,
    refuse: bool,
}

#[async_trait]
impl InscriptionDeCle for FauxSite {
    async fn inscrire(
        &self,
        server_id: &str,
        jeton_du_compte: &str,
        jeton_de_liaison: &str,
        corps: &CorpsInscription,
    ) -> Result<(), String> {
        self.recus.lock().unwrap().push((
            server_id.to_string(),
            jeton_du_compte.to_string(),
            jeton_de_liaison.to_string(),
            corps.clone(),
        ));
        if self.refuse {
            Err("inscription de la cle : HTTP 403".into())
        } else {
            Ok(())
        }
    }
}

#[test]
fn la_cle_est_creee_une_fois_puis_relue_du_coffre() {
    let b = base();
    assert!(cle_existante(&b).unwrap().is_none());
    let a = cle_du_serveur(&b).unwrap().cle_publique();
    let c = cle_du_serveur(&b).unwrap().cle_publique();
    assert_eq!(a, c, "la cle a change d'un appel a l'autre");
    assert_eq!(cle_existante(&b).unwrap().unwrap().cle_publique(), a);
}

#[test]
fn la_graine_vit_dans_le_coffre_et_ne_sort_pas_des_reglages_publies() {
    let b = base();
    let cle = cle_du_serveur(&b).unwrap();
    let coffre = reglages(&b).get("credentials_vault").unwrap().unwrap();
    assert!(coffre.contains(SERVICE_DU_COFFRE));
    // Le nom du réglage est classé secret : masqué en lecture, retiré des
    // sauvegardes exportées.
    assert!(crate::secrets::est_secret("credentials_vault"));
    // La clé publique, elle, n'est pas la graine.
    assert!(!coffre.contains(&cle.cle_publique()));
}

#[tokio::test]
async fn la_cle_s_inscrit_sur_le_compte_avec_la_preuve_de_sa_possession_une_seule_fois() {
    let b = base();
    let site = FauxSite::default();
    // Pas de clé : rien.
    relie(&b);
    assert_eq!(inscrire_si_besoin(&b, &site).await, Ok(false));

    let cle = cle_du_serveur(&b).unwrap();
    assert_eq!(inscrire_si_besoin(&b, &site).await, Ok(true));
    {
        let recus = site.recus.lock().unwrap();
        assert_eq!(recus.len(), 1);
        let (sid, jeton, liaison, corps) = &recus[0];
        assert_eq!((sid.as_str(), jeton.as_str()), ("srv-1", "jeton-du-compte"));
        assert_eq!(liaison, "liaison");
        assert_eq!(corps.public_key, cle.cle_publique());
        assert_eq!(corps.alg, "Ed25519");
        assert_eq!(
            tune_laissez_passer::verifier_inscription_serveur(
                &corps.public_key,
                "srv-1",
                &corps.preuve
            ),
            Ok(())
        );
    }
    // Déjà inscrite : rien ne repart.
    assert_eq!(inscrire_si_besoin(&b, &site).await, Ok(false));
    assert_eq!(site.recus.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn sans_liaison_au_compte_rien_n_est_inscrit_et_un_refus_se_retentera() {
    let b = base();
    cle_du_serveur(&b).unwrap();
    let site = FauxSite::default();
    reglages(&b).set("server_id", "srv-1").unwrap();
    reglages(&b)
        .set("mozaik_access_token", "jeton-du-compte")
        .unwrap();
    assert_eq!(inscrire_si_besoin(&b, &site).await, Ok(false));
    assert!(site.recus.lock().unwrap().is_empty());

    relie(&b);
    let refus = FauxSite {
        refuse: true,
        ..Default::default()
    };
    assert!(inscrire_si_besoin(&b, &refus).await.is_err());
    assert!(reglages(&b).get(CLE_INSCRITE).unwrap().is_none());
    assert_eq!(inscrire_si_besoin(&b, &site).await, Ok(true));
}

#[test]
fn le_lien_herite_n_est_coupe_que_si_le_relais_l_a_dit() {
    let b = base();
    let s = reglages(&b);
    assert!(!lien_herite_coupe(&s));
    s.set(LIEN_HERITE_ACCEPTE, "true").unwrap();
    assert!(!lien_herite_coupe(&s));
    s.set(LIEN_HERITE_ACCEPTE, "false").unwrap();
    assert!(lien_herite_coupe(&s));
}

#[tokio::test]
async fn tant_que_le_relais_n_annonce_pas_le_laissez_passer_rien_n_est_inscrit() {
    let b = base();
    cle_du_serveur(&b).unwrap();
    relie(&b);
    let site = FauxSite::default();
    for annonce in [None, Some("false")] {
        match annonce {
            None => reglages(&b).delete(LAISSEZ_PASSER_ACTIF).unwrap(),
            Some(v) => reglages(&b).set(LAISSEZ_PASSER_ACTIF, v).unwrap(),
        }
        assert_eq!(inscrire_si_besoin(&b, &site).await, Ok(false));
    }
    assert!(
        site.recus.lock().unwrap().is_empty(),
        "une inscription est partie vers un site qui ne l'attend pas"
    );
}

#[tokio::test]
async fn inscription_http_presente_la_liaison_du_serveur_7282() {
    use axum::{
        Json, Router,
        extract::{Path, State},
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    async fn recevoir(
        State(recus): State<Arc<AtomicUsize>>,
        Path(sid): Path<String>,
        entetes: HeaderMap,
        Json(corps): Json<serde_json::Value>,
    ) -> StatusCode {
        recus.fetch_add(1, Ordering::SeqCst);
        if entetes.get("authorization").and_then(|h| h.to_str().ok())
            != Some("Bearer jeton-du-compte")
            || entetes
                .get("x-tune-server-token")
                .and_then(|h| h.to_str().ok())
                != Some("liaison")
        {
            return StatusCode::FORBIDDEN;
        }
        let preuve_valide = tune_laissez_passer::verifier_inscription_serveur(
            corps["public_key"].as_str().unwrap_or_default(),
            &sid,
            corps["preuve"].as_str().unwrap_or_default(),
        )
        .is_ok();
        if preuve_valide {
            StatusCode::OK
        } else {
            StatusCode::FORBIDDEN
        }
    }
    let recus = Arc::new(AtomicUsize::new(0));
    let ecoute = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let app = Router::new()
        .route("/api/v1/remote/servers/{sid}/key", post(recevoir))
        .with_state(recus.clone());
    let serveur = tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    let b = base();
    relie(&b);
    cle_du_serveur(&b).unwrap();
    let site = InscriptionHttp {
        client: crate::http::client::shared().clone(),
        base: format!("http://{adresse}"),
    };
    let resultat = inscrire_si_besoin(&b, &site).await;
    assert_eq!(
        resultat,
        Ok(true),
        "la requête réelle doit présenter la liaison attendue, sinon le site refuse 403"
    );
    assert!(reglages(&b).get(CLE_INSCRITE).unwrap().is_some());
    assert_eq!(inscrire_si_besoin(&b, &site).await, Ok(false));
    assert_eq!(
        recus.load(Ordering::SeqCst),
        1,
        "une inscription réussie ne doit pas repartir au battement suivant"
    );
    serveur.abort();
}
