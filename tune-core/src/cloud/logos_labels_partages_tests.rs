//! #7414 — le partage des logos de labels, éprouvé contre un FAUX site local
//! qui suit le contrat de `site-mozaiklabs#450`. Aucun appel au vrai site.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

const INSTANCE: &str = "6f1c0a52-8d3e-4b7a-9c11-2a4f5e6d7c8b";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-logo-partage-";

#[derive(Default)]
struct FauxSite {
    /// (méthode, chemin, corps JSON ou champs du formulaire)
    appels: Vec<(String, String, Value)>,
    /// Statut rendu au dépôt.
    statut_depot: u16,
    /// Un 429 à la lecture, avec Retry-After.
    trop: bool,
}

type Site = Arc<Mutex<FauxSite>>;

async fn faux_site() -> (String, Site) {
    use axum::extract::{Multipart, State};
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};

    async fn lookup(State(s): State<Site>, Json(corps): Json<Value>) -> axum::response::Response {
        let mut s = s.lock().unwrap();
        s.appels
            .push(("POST".into(), CHEMIN_CONSULTATION.into(), corps.clone()));
        if s.trop {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "120")],
                "",
            )
                .into_response();
        }
        let logos: Vec<Value> = corps["labels"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["name"] == "ECM Records")
            .map(|l| {
                json!({
                    "name": l["name"],
                    "key": "ecm records",
                    "image_url": "IMAGE",
                    "sha256": "x",
                    "musicbrainz_label_id": "5a584032-dcef-41bb-9f8b-19540116fb1c",
                    "source": "commons",
                    "provider": "Wikimedia Commons",
                    "license_status": "free",
                    "license": "Public domain",
                    "source_url": "https://commons.wikimedia.org/wiki/File:ECM.png",
                })
            })
            .collect();
        Json(json!({ "logos": logos })).into_response()
    }
    async fn depot(State(s): State<Site>, mut form: Multipart) -> axum::response::Response {
        let mut champs = serde_json::Map::new();
        while let Ok(Some(champ)) = form.next_field().await {
            let nom = champ.name().unwrap_or_default().to_string();
            let valeur = if nom == "image" {
                json!(champ.bytes().await.unwrap().len())
            } else {
                json!(champ.text().await.unwrap())
            };
            champs.insert(nom, valeur);
        }
        let mut s = s.lock().unwrap();
        s.appels.push((
            "POST".into(),
            CHEMIN_PROPOSITION.into(),
            Value::Object(champs),
        ));
        StatusCode::from_u16(s.statut_depot)
            .unwrap()
            .into_response()
    }
    async fn image(State(s): State<Site>) -> Vec<u8> {
        s.lock()
            .unwrap()
            .appels
            .push(("GET".into(), "/image".into(), Value::Null));
        PNG.to_vec()
    }
    let etat: Site = Arc::new(Mutex::new(FauxSite {
        statut_depot: 201,
        ..Default::default()
    }));
    let app = Router::new()
        .route(CHEMIN_CONSULTATION, post(lookup))
        .route(CHEMIN_PROPOSITION, post(depot))
        .route("/api/v1/community/label-logos/1/abc", get(image))
        .with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    (format!("http://{adresse}"), etat)
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn brancher(b: &Arc<dyn DbBackend>, url: &str, consentement: bool) -> PartageHttp {
    let s = SettingsRepo::with_backend(b.clone());
    s.set("mozaik_base_url", url).unwrap();
    s.set("instance_id", INSTANCE).unwrap();
    s.set("telemetry_enabled", "true").unwrap();
    s.set(
        crate::cloud::consent::CONTRIBUTION_SETTING_KEY,
        if consentement { "true" } else { "false" },
    )
    .unwrap();
    PartageHttp::depuis_reglages(b, crate::http::client::shared().clone()).expect("partage")
}

fn label(nom: &str) -> LabelAConsulter {
    LabelAConsulter {
        nom: nom.into(),
        mb_label_id: None,
        qobuz_label_id: None,
    }
}

fn proposition() -> PropositionDeLogo {
    PropositionDeLogo {
        nom: "ECM Records".into(),
        mb_label_id: Some("5a584032-dcef-41bb-9f8b-19540116fb1c".into()),
        qobuz_label_id: Some("9".into()),
        attribution: Attribution {
            source: SourceDuLogo::Commons,
            statut_licence: StatutDeLicence::Libre,
            fournisseur: "Wikimedia Commons".into(),
            licence: Some("Public domain".into()),
            licence_url: None,
            auteur: None,
            source_url: "https://commons.wikimedia.org/wiki/File:ECM.png".into(),
            fichier: None,
            douteux: false,
        },
        octets: PNG.to_vec(),
        ext: "png",
    }
}

/// UN appel pour tous les labels, et la réponse lue en attribution.
#[tokio::test]
async fn la_lecture_groupee_part_en_un_seul_appel() {
    let (url, site) = faux_site().await;
    let b = base();
    let partage = brancher(&b, &url, false);
    let mut labels: Vec<LabelAConsulter> = (0..300).map(|i| label(&format!("Label {i}"))).collect();
    labels.push(LabelAConsulter {
        qobuz_label_id: Some("9".into()),
        ..label("ECM Records")
    });

    let logos = partage.consulter(&labels).await.expect("réponse");

    let appels = site.lock().unwrap().appels.clone();
    assert_eq!(appels.len(), 1, "un seul appel pour 301 labels");
    assert_eq!(appels[0].2["labels"].as_array().unwrap().len(), 301);
    assert_eq!(appels[0].2["labels"][300]["qobuz_label_id"], "9");
    assert_eq!(logos.len(), 1);
    assert_eq!(logos[0].nom, "ECM Records");
    assert_eq!(logos[0].attribution.source, SourceDuLogo::Commons);
    assert_eq!(logos[0].attribution.statut_licence, StatutDeLicence::Libre);
    assert_eq!(
        logos[0].mb_label_id.as_deref(),
        Some("5a584032-dcef-41bb-9f8b-19540116fb1c")
    );
}

/// Seule une adresse sous la racine du site est téléchargée.
#[tokio::test]
async fn seule_une_image_du_site_est_telechargee() {
    let (url, site) = faux_site().await;
    let b = base();
    let partage = brancher(&b, &url, false);

    let octets = partage
        .telecharger(&format!("{url}/api/v1/community/label-logos/1/abc"))
        .await
        .expect("image du site");
    assert_eq!(octets, PNG);

    // Un AUTRE serveur, joignable et qui servirait l'image : jamais appelé.
    let (autre, autre_site) = faux_site().await;
    for ailleurs in [
        format!("{autre}/api/v1/community/label-logos/1/abc"),
        "file:///etc/passwd".to_string(),
    ] {
        assert!(
            partage.telecharger(&ailleurs).await.is_err(),
            "{ailleurs} ne doit pas être suivi"
        );
    }
    assert!(
        autre_site.lock().unwrap().appels.is_empty(),
        "une adresse hors du site a été téléchargée"
    );
    assert_eq!(site.lock().unwrap().appels.len(), 1);
}

/// Sans consentement communautaire, aucune proposition ne part.
#[tokio::test]
async fn rien_ne_se_propose_sans_consentement() {
    let (url, site) = faux_site().await;
    let b = base();
    let partage = brancher(&b, &url, false);

    assert!(!partage.peut_proposer());
    assert_eq!(
        partage.proposer(&proposition()).await,
        ProposeAuPartage::Plus
    );
    assert!(site.lock().unwrap().appels.is_empty());
}

/// Avec le consentement : l'installation, le consentement, les clés,
/// l'attribution et l'image partent ; 201 et 422 sont traités, 503 non.
#[tokio::test]
async fn la_proposition_porte_l_installation_et_le_consentement() {
    let (url, site) = faux_site().await;
    let b = base();
    let partage = brancher(&b, &url, true);
    assert!(partage.peut_proposer());

    assert_eq!(
        partage.proposer(&proposition()).await,
        ProposeAuPartage::Traitee
    );
    let champs = site.lock().unwrap().appels[0].2.clone();
    assert_eq!(champs["instance_id"], INSTANCE);
    assert_eq!(champs["consent"], "1");
    assert_eq!(champs["name"], "ECM Records");
    assert_eq!(
        champs["musicbrainz_label_id"],
        "5a584032-dcef-41bb-9f8b-19540116fb1c"
    );
    assert_eq!(champs["qobuz_label_id"], "9");
    assert_eq!(champs["source"], "commons");
    assert_eq!(champs["license_status"], "free");
    assert_eq!(champs["image"], PNG.len());

    site.lock().unwrap().statut_depot = 422;
    assert_eq!(
        partage.proposer(&proposition()).await,
        ProposeAuPartage::Traitee
    );
    site.lock().unwrap().statut_depot = 503;
    assert_eq!(
        partage.proposer(&proposition()).await,
        ProposeAuPartage::Plus
    );
}

/// Un 429 retient la lecture : le second appel ne part pas.
#[tokio::test]
async fn un_429_retient_la_lecture_suivante() {
    let (url, site) = faux_site().await;
    let b = base();
    let partage = brancher(&b, &url, false);
    site.lock().unwrap().trop = true;

    assert!(partage.consulter(&[label("ECM Records")]).await.is_err());
    assert!(partage.consulter(&[label("ECM Records")]).await.is_err());
    assert_eq!(
        site.lock().unwrap().appels.len(),
        1,
        "la portée est retenue"
    );
}

/// Télémétrie refusée : pas de partage du tout, ni lecture ni dépôt.
#[test]
fn le_refus_de_telemetrie_coupe_le_partage() {
    let b = base();
    let s = SettingsRepo::with_backend(b.clone());
    s.set("telemetry_enabled", "false").unwrap();
    s.set(crate::cloud::consent::CONTRIBUTION_SETTING_KEY, "true")
        .unwrap();
    assert!(PartageHttp::depuis_reglages(&b, crate::http::client::shared().clone()).is_none());
}

/// Un site injoignable rend une erreur, vite, et rien d'autre.
#[tokio::test]
async fn un_site_injoignable_ne_bloque_pas() {
    let b = base();
    let ecoute = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", ecoute.local_addr().unwrap());
    drop(ecoute);
    let partage = brancher(&b, &url, true);
    let debut = std::time::Instant::now();
    assert!(partage.consulter(&[label("ECM Records")]).await.is_err());
    assert!(debut.elapsed() <= DELAI_LECTURE + Duration::from_secs(1));
}
