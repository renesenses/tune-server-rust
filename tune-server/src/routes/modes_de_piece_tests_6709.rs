//! #6709 — `POST /api/v1/eq/room-modes`, par le ROUTEUR MONTÉ.
//!
//! La route rend des bandes que `POST /zones/{id}/eq` accepte telles quelles,
//! toutes en atténuation ; une pièce absurde est refusée en 400 avec sa clé.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> axum::Router {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    crate::routes::router(state)
}

async fn poster(app: &axum::Router, corps: String) -> (StatusCode, Value) {
    let req = Request::post("/api/v1/eq/room-modes")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(corps))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn reference() -> Value {
    json!({
        "length_m": 5.0,
        "width_m": 4.0,
        "height_m": 2.5,
        "listener_from_back_wall_m": 1.0,
        "speaker_from_front_wall_m": 0.8,
    })
}

#[tokio::test]
async fn la_route_rend_des_attenuations_que_l_egaliseur_accepte_6709() {
    let (code, v) = poster(&app(), reference().to_string()).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    let filtres = v["filters"].as_array().expect("filters");
    assert!(
        (5..=10).contains(&filtres.len()),
        "{} filtres : {v}",
        filtres.len()
    );
    for f in filtres {
        // Ce que `set_eq` fait de chaque bande (`serde_json::from_value`) :
        // le même type, donc le même chemin que l'égaliseur paramétrique.
        let bande: tune_core::audio::eq::EqBandSpec =
            serde_json::from_value(f.clone()).expect("bande lisible par POST /zones/{id}/eq");
        assert!(
            bande.gain <= -3.0 && bande.gain >= -20.0,
            "gain {} dB rendu par la route : jamais de gain positif (#6709)",
            bande.gain
        );
        assert!((4.0..=20.0).contains(&bande.q), "Q {}", bande.q);
        assert!((20.0..=250.0).contains(&bande.freq), "{} Hz", bande.freq);
        assert_eq!(bande.band_type, "peak");
        assert_eq!(bande.channel, None);
    }
    // Le profil que ces bandes donnent ne pousse nulle part.
    let profil = tune_core::audio::eq::EqProfile {
        enabled: true,
        bands: filtres
            .iter()
            .map(|f| serde_json::from_value(f.clone()).unwrap())
            .collect(),
        ..Default::default()
    };
    assert!(profil.max_reponse_db_at(0, 48_000.0) <= 1e-9);

    // Les modes de la pièce de référence, calculés à la main :
    // (1,0,0) = 343/2/5 = 34,3 Hz ; (0,2,0) = 343/4 = 85,75 Hz (voir
    // tune-core). (1,1,0) n'est pas rendu : l'auditeur, au milieu de la
    // largeur, est sur son nœud, il n'est pas parmi les plus couplés.
    let modes = v["modes"].as_array().unwrap();
    let freq = |n: [u32; 3]| {
        modes
            .iter()
            .find(|m| m["n"] == json!(n))
            .and_then(|m| m["freq"].as_f64())
    };
    assert_eq!(freq([1, 0, 0]), Some(34.3));
    assert_eq!(freq([0, 2, 0]), Some(85.75));
    assert_eq!(v["assumptions"]["speed_of_sound_m_s"], 343.0);
    assert_eq!(v["bounds"]["gain_db"], json!([-20.0, -3.0]));
}

#[tokio::test]
async fn une_piece_absurde_est_refusee_en_400_avec_son_champ_6709() {
    let app = app();
    let cas: [(&str, Value, &str, Value); 8] = [
        (
            "longueur nulle",
            json!(0.0),
            "room_value_out_of_bounds",
            json!("length_m"),
        ),
        (
            "longueur négative",
            json!(-4.0),
            "room_value_out_of_bounds",
            json!("length_m"),
        ),
        (
            "longueur énorme",
            json!(1e300),
            "room_value_out_of_bounds",
            json!("length_m"),
        ),
        (
            "longueur texte",
            json!("cinq"),
            "room_body_invalid",
            Value::Null,
        ),
        (
            "longueur absente",
            Value::Null,
            "room_body_invalid",
            Value::Null,
        ),
        (
            "largeur nulle",
            json!(0.0),
            "room_value_out_of_bounds",
            json!("width_m"),
        ),
        (
            "hauteur négative",
            json!(-2.5),
            "room_value_out_of_bounds",
            json!("height_m"),
        ),
        (
            "auditeur dehors",
            json!(9.0),
            "room_value_out_of_bounds",
            json!("listener_from_back_wall_m"),
        ),
    ];
    for (nom, valeur, cle, champ) in cas {
        let mut corps = reference();
        let cible = match nom {
            n if n.starts_with("largeur") => "width_m",
            n if n.starts_with("hauteur") => "height_m",
            n if n.starts_with("auditeur") => "listener_from_back_wall_m",
            _ => "length_m",
        };
        if valeur.is_null() {
            corps.as_object_mut().unwrap().remove(cible);
        } else {
            corps[cible] = valeur;
        }
        let (code, v) = poster(&app, corps.to_string()).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{nom} : {v}");
        assert_eq!(v["error"], cle, "{nom} : {v}");
        assert_eq!(v["field"], champ, "{nom} : {v}");
        assert!(
            v["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{nom} : {v}"
        );
        assert!(
            v.get("filters").is_none(),
            "{nom} : aucun filtre sur un refus"
        );
    }
    // Corps qui n'est pas du JSON.
    let (code, v) = poster(&app, "pas du json".into()).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "room_body_invalid");
    // Nombre de filtres hors de 5..10.
    let mut corps = reference();
    corps["count"] = json!(12);
    let (code, v) = poster(&app, corps.to_string()).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["field"], "count");
}

/// L'arrondi des gains va vers plus d'atténuation : −3 reste −3, jamais 0.
#[test]
fn l_arrondi_des_gains_ne_remonte_jamais_6709() {
    use tune_core::audio::modes_de_piece as m;
    let mut prop = m::proposer(&m::Piece {
        length_m: 5.0,
        width_m: 4.0,
        height_m: 2.5,
        listener_from_back_wall_m: 1.0,
        speaker_from_front_wall_m: 0.8,
        listener_from_side_wall_m: None,
        listener_height_m: None,
        speaker_height_m: None,
        rt60_s: None,
        count: None,
    })
    .unwrap();
    prop.filtres = vec![
        m::FiltreDeMode::new(40.0, 3.0, 7.0, vec![]),
        m::FiltreDeMode::new(50.0, 7.04, 7.0, vec![]),
        m::FiltreDeMode::new(60.0, 20.0, 7.0, vec![]),
    ];
    let v = super::corps_des_modes(&prop, 60);
    let gains: Vec<f64> = v["filters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["gain"].as_f64().unwrap())
        .collect();
    assert_eq!(gains, vec![-3.0, -7.1, -20.0]);
}
