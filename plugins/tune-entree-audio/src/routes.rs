//! Les routes du greffon, montées par l'hôte sous `/api/v1/ext/entree-audio`.
//!
//! * `GET  /entrees` — les entrées du système : nom, canaux, fréquences et
//!   formats natifs ;
//! * `POST /jouer`   — `{ "entree": "Yeti X", "zone_id": 3 }` (option :
//!   `amorce_ms`) : capte l'entrée à son format natif et la joue en direct ;
//! * `POST /arreter` — arrête la zone qui écoute et la capture ;
//! * `GET  /etat`    — entrée active, fréquence captée, crête, dérive,
//!   sous-remplissages et débordements, autorisation macOS.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::autorisation::{self, Autorisation, LireAutorisation};
use crate::controleur::Controleur;
use crate::format::dbfs;
use crate::hote::titre;
use crate::peripheriques::CAPTURE_NON_COMPILEE;

/// Tune ne capte que du PCM : un flux compressé encapsulé (IEC 61937) n'est
/// pas décodé.
pub const DIAGNOSTIC_IEC61937: &str = "L'entrée reçoit un flux COMPRESSÉ (Dolby Digital, DTS… encapsulé en IEC 61937), pas du PCM : Tune ne le décode pas et il sonnerait comme du bruit. Régler la source (TV, lecteur) en sortie audio PCM / stéréo.";

#[derive(Clone)]
pub struct EtatRoutes {
    pub controleur: Arc<Controleur>,
    pub autorisation: LireAutorisation,
}

pub fn router(etat: EtatRoutes) -> Router<()> {
    Router::new()
        .route("/entrees", get(entrees))
        .route("/jouer", post(jouer))
        .route("/arreter", post(arreter))
        .route("/etat", get(etat_courant))
        .with_state(etat)
}

fn refus(code: StatusCode, motif: &str, message: String) -> Response {
    (code, Json(json!({ "error": motif, "message": message }))).into_response()
}

fn non_compilee(etat: &EtatRoutes) -> bool {
    etat.controleur.peripheriques().pile() == "aucune"
}

/// `type` du contrat des sources (#5065) : `entree`, `virtuelle` ou `hdmi`.
pub fn type_de_source(nom: &str, virtuelle: Option<bool>) -> &'static str {
    if virtuelle == Some(true) {
        "virtuelle"
    } else if nom.to_lowercase().contains("hdmi") {
        "hdmi"
    } else {
        "entree"
    }
}

/// `id` du contrat des sources (#5065) : `entree:<nom en minuscules, tirets>`.
pub fn id_de_source(nom: &str) -> String {
    let mut s = String::from("entree:");
    let mut tiret = false;
    for c in nom.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            s.push(c);
            tiret = false;
        } else if !tiret && s.len() > "entree:".len() {
            s.push('-');
            tiret = true;
        }
    }
    s.trim_end_matches('-').to_string()
}

/// `etat` du contrat des sources (#5065) pour l'entrée ÉCOUTÉE.
fn etat_de_source(i: &crate::controleur::Instantane, autorisation: Autorisation) -> &'static str {
    use crate::autorisation::SILENCE_SUSPECT_S;
    if matches!(i.fin, Some(crate::anneau::Fin::Erreur(_))) {
        return "indisponible";
    }
    let silence = i.silence_numerique_s >= SILENCE_SUSPECT_S;
    match autorisation {
        Autorisation::Refusee => "autorisation_refusee",
        Autorisation::NonDemandee if silence => "autorisation_refusee",
        _ if silence || i.crete == 0.0 => "silence",
        _ => "signal",
    }
}

/// Au-delà, le système audio est tenu pour muet : la route répond 504 et
/// `/etat` dit pourquoi.
pub const SYSTEME_MUET: Duration = Duration::from_secs(10);

fn systeme_muet() -> Response {
    refus(
        StatusCode::GATEWAY_TIMEOUT,
        "systeme_audio_sans_reponse",
        "Le système audio ne répond pas (CoreAudio) : voir `diagnostic` dans /etat.".into(),
    )
}

async fn entrees(State(etat): State<EtatRoutes>) -> Response {
    let c = etat.controleur.clone();
    let liste = tokio::time::timeout(
        SYSTEME_MUET,
        tokio::task::spawn_blocking(move || c.peripheriques().lister()),
    )
    .await;
    let Ok(liste) = liste else {
        return systeme_muet();
    };
    match liste {
        Ok(Ok(v)) => {
            let active = etat.controleur.instantane();
            let autorisation = (etat.autorisation)();
            let v: Vec<Value> = v
                .into_iter()
                .map(|e| {
                    let ecoutee = active.as_ref().filter(|i| i.entree == e.nom);
                    let mut o = serde_json::to_value(&e).unwrap_or_default();
                    // Champs du contrat des sources (#5065). `etat` n'est
                    // connu que de l'entrée écoutée : `null` pour les autres.
                    o["id_systeme"] = json!(e.id);
                    o["id"] = json!(id_de_source(&e.nom));
                    o["type"] = json!(type_de_source(&e.nom, e.virtuelle));
                    o["etat"] = json!(ecoutee.map(|i| etat_de_source(i, autorisation)));
                    o["detail"] = json!({
                        "frequence": e.frequence_courante,
                        "canaux": e.canaux,
                        "niveau_db": ecoutee.and_then(|i| dbfs(i.crete)),
                        "virtuelle": e.virtuelle,
                    });
                    o
                })
                .collect();
            Json(json!({
                "pile": etat.controleur.peripheriques().pile(),
                "entrees": v,
            }))
            .into_response()
        }
        Ok(Err(e)) if non_compilee(&etat) => {
            refus(StatusCode::NOT_IMPLEMENTED, "capture_non_compilee", e)
        }
        Ok(Err(e)) => refus(StatusCode::BAD_GATEWAY, "enumeration", e),
        Err(e) => refus(StatusCode::INTERNAL_SERVER_ERROR, "interne", e.to_string()),
    }
}

#[derive(Deserialize)]
struct DemandeJouer {
    entree: String,
    zone_id: i64,
    amorce_ms: Option<u64>,
}

async fn jouer(State(etat): State<EtatRoutes>, Json(d): Json<DemandeJouer>) -> Response {
    if non_compilee(&etat) {
        return refus(
            StatusCode::NOT_IMPLEMENTED,
            "capture_non_compilee",
            CAPTURE_NON_COMPILEE.into(),
        );
    }
    if let Some(ms) = d.amorce_ms {
        *etat
            .controleur
            .amorce
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Duration::from_millis(ms.clamp(100, 10_000));
    }
    // `entree:<slug>` (contrat des sources, #5065) : retrouver le nom.
    let mut entree = d.entree.clone();
    if entree.starts_with("entree:") {
        let c = etat.controleur.clone();
        if let Ok(Ok(liste)) = tokio::task::spawn_blocking(move || c.peripheriques().lister()).await
        {
            if let Some(e) = liste.iter().find(|e| id_de_source(&e.nom) == entree) {
                entree = e.nom.clone();
            }
        }
    }
    let joue =
        tokio::time::timeout(SYSTEME_MUET * 2, etat.controleur.jouer(&entree, d.zone_id)).await;
    let Ok(joue) = joue else {
        return systeme_muet();
    };
    match joue {
        Ok((nom, format)) => Json(json!({
            "zone_id": d.zone_id,
            "entree": nom,
            "titre": titre(&nom),
            "frequence": format.frequence,
            "bits": format.bits,
            "canaux": format.canaux,
        }))
        .into_response(),
        Err(e) if e.starts_with("aucune entrée audio") => {
            refus(StatusCode::NOT_FOUND, "entree_inconnue", e)
        }
        Err(e) => refus(StatusCode::BAD_GATEWAY, "lecture", e),
    }
}

async fn arreter(State(etat): State<EtatRoutes>) -> Json<Value> {
    match etat.controleur.arreter().await {
        Some((entree, zone)) => Json(json!({ "arretee": true, "entree": entree, "zone_id": zone })),
        None => Json(json!({ "arretee": false })),
    }
}

async fn etat_courant(State(etat): State<EtatRoutes>) -> Json<Value> {
    let c = &etat.controleur;
    let autorisation: Autorisation = (etat.autorisation)();
    let instantane = c.instantane();
    let application = autorisation::application_responsable();
    let compresse = instantane.as_ref().is_some_and(|i| i.blocs_iec61937 > 0);
    let demarrage = c.demarrage_en_cours();
    let muet = demarrage
        .as_ref()
        .filter(|(_, d)| *d >= Duration::from_secs(5));
    let diagnostic = if let Some((entree, d)) = muet {
        Some(format!(
            "Le système audio ne répond pas depuis {} s à l'ouverture de « {entree} ». Constaté sur macOS pour un serveur lancé par launchd ou ssh, sans application responsable : lancer Tune depuis « Tune Server.app » (ou un terminal autorisé) et l'autoriser dans {}.",
            d.as_secs(),
            autorisation::REGLAGE
        ))
    } else if compresse {
        // Le plus grave d'abord : un flux Dolby/DTS servi comme du PCM est du
        // bruit à pleine échelle.
        Some(DIAGNOSTIC_IEC61937.to_string())
    } else {
        autorisation::diagnostic(
            autorisation,
            instantane
                .as_ref()
                .map(|i| i.silence_numerique_s)
                .unwrap_or(0.0),
            instantane.is_some(),
            &application,
        )
    };
    let relance = *c.derniere_relance.lock().unwrap_or_else(|e| e.into_inner());
    let commun = json!({
        "pile": c.peripheriques().pile(),
        "capture_compilee": !non_compilee(&etat),
        "autorisation": autorisation,
        "application_responsable": application,
        "diagnostic": diagnostic,
        "relances_de_frequence": c.relances_de_frequence.load(Ordering::Relaxed),
        "derniere_relance": relance.map(|(de, vers)| json!({ "de": de, "vers": vers })),
        "demarrage_en_cours": demarrage
            .as_ref()
            .map(|(e, d)| json!({ "entree": e, "depuis_s": d.as_secs_f64() })),
    });
    let Some(i) = instantane else {
        let mut v = commun;
        v["active"] = json!(false);
        return Json(v);
    };
    let d = &i.derives;
    let mut v = commun;
    let details = json!({
        "active": true,
        // Contrat des sources (#5065).
        "id": id_de_source(&i.entree),
        "type": type_de_source(&i.entree, None),
        "etat": etat_de_source(&i, autorisation),
        "detail": {
            "frequence": i.format.frequence,
            "canaux": i.format.canaux,
            "niveau_db": dbfs(i.crete),
        },
        "entree": i.entree,
        "titre": titre(&i.entree),
        "zone_id": i.zone_id,
        "depuis_s": i.depuis_s,
        "frequence": i.format.frequence,
        "bits": i.format.bits,
        "canaux": i.format.canaux,
        "niveau_crete": i.crete,
        "niveau_crete_dbfs": dbfs(i.crete),
        "silence_numerique_s": i.silence_numerique_s,
        "derive_ppm": d.derive_ppm,
        "derive_source": d.derive_source,
        "position_contre_hote_ppm": d.position_contre_hote_ppm,
        "trames_captees": i.trames_captees,
        "capture_contre_hote_ppm": d.capture_contre_hote_ppm,
        "consommation_contre_hote_ppm": d.consommation_contre_hote_ppm,
        "consommateur_regule": d.consommateur_regule,
        "fenetre_de_mesure_s": d.fenetre_s,
        "tampon_ms": d.tampon_ms,
        "tampon_de_regime_ms": d.tampon_de_regime_ms,
        "latence_ms": d.latence_ms,
        "sous_remplissages": i.sous_remplissages,
        "debordements": i.debordements,
        "trames_perdues": i.trames_perdues,
        "flux_compresse_iec61937": compresse,
        "reprises": i.reprises,
        "trames_retirees": i.trames_retirees,
        "trames_comblees": i.trames_comblees,
        "compensation": {
            "methode": "tampon_avec_reprise",
            "reechantillonne": false,
            "bit_perfect": i.reprises == 0 && i.debordements == 0,
        },
        "fin": i.fin.map(|f| format!("{f:?}")),
    });
    if let (Some(o), Some(extra)) = (v.as_object_mut(), details.as_object()) {
        for (k, x) in extra {
            o.insert(k.clone(), x.clone());
        }
    }
    Json(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hote::tests::HoteTemoin;
    use crate::peripheriques::AucunePile;
    use crate::simule::Simulees;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn appel(
        r: Router<()>,
        methode: &str,
        uri: &str,
        corps: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder().method(methode).uri(uri);
        let req = match corps {
            Some(c) => req
                .header("content-type", "application/json")
                .body(Body::from(c.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let rep = r.oneshot(req).await.unwrap();
        let code = rep.status();
        let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
            .await
            .unwrap();
        (code, serde_json::from_slice(&octets).unwrap())
    }

    fn accordee() -> Autorisation {
        Autorisation::Accordee
    }
    fn jamais() -> Autorisation {
        Autorisation::NonDemandee
    }

    #[tokio::test]
    async fn entrees_decrit_canaux_frequences_et_formats() {
        let c = Controleur::new(
            Simulees::avec("Yeti X", 48_000),
            Arc::new(HoteTemoin::default()),
        );
        let r = router(EtatRoutes {
            controleur: c,
            autorisation: accordee,
        });
        let (code, v) = appel(r, "GET", "/entrees", None).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["pile"], "simule");
        let e = &v["entrees"][0];
        assert_eq!(e["nom"], "Yeti X");
        assert_eq!(e["canaux"], 2);
        assert_eq!(e["frequence_courante"], 48_000);
        assert_eq!(e["formats"][0], "i16");
        // Contrat des sources (#5065).
        assert_eq!(e["id"], "entree:yeti-x");
        assert_eq!(e["type"], "entree");
        assert!(
            e["etat"].is_null(),
            "inconnu tant qu'elle n'est pas écoutée"
        );
        assert_eq!(e["detail"]["frequence"], 48_000);
        assert_eq!(e["detail"]["virtuelle"], false);
    }

    #[test]
    fn les_identifiants_et_types_suivent_le_contrat_des_sources() {
        assert_eq!(id_de_source("Loopback Audio"), "entree:loopback-audio");
        assert_eq!(
            id_de_source("Micro de « aphone »"),
            "entree:micro-de-aphone"
        );
        assert_eq!(type_de_source("Loopback Audio", Some(true)), "virtuelle");
        assert_eq!(type_de_source("USB3 HDMI Capture", Some(false)), "hdmi");
        assert_eq!(type_de_source("Yeti X", None), "entree");
    }

    #[tokio::test]
    async fn jouer_lance_la_zone_et_etat_rend_la_capture() {
        let hote = Arc::new(HoteTemoin::default());
        let c = Controleur::new(Simulees::avec("Yeti X", 48_000), hote.clone());
        let r = router(EtatRoutes {
            controleur: c.clone(),
            autorisation: accordee,
        });
        let (code, v) = appel(
            r.clone(),
            "POST",
            "/jouer",
            Some(json!({"entree": "Yeti X", "zone_id": 7})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["titre"], "Entrée audio — Yeti X");
        assert_eq!(v["frequence"], 48_000);
        let joues = hote.joues.lock().await.clone();
        assert_eq!(joues.len(), 1);
        assert_eq!(joues[0].0, 7);
        assert_eq!(joues[0].1.entree, "Yeti X");

        tokio::time::sleep(Duration::from_millis(80)).await;
        let (_, v) = appel(r.clone(), "GET", "/etat", None).await;
        assert_eq!(v["active"], true);
        assert_eq!(v["entree"], "Yeti X");
        assert_eq!(v["zone_id"], 7);
        assert_eq!(v["frequence"], 48_000);
        assert_eq!(v["autorisation"], "accordee");
        assert!(v["niveau_crete_dbfs"].as_f64().unwrap() < 0.0);
        assert_eq!(v["debordements"], 0);
        assert_eq!(v["compensation"]["methode"], "tampon_avec_reprise");
        assert_eq!(v["etat"], "signal");
        assert_eq!(v["id"], "entree:yeti-x");
        assert_eq!(v["detail"]["canaux"], 2);

        let (_, v) = appel(r.clone(), "POST", "/arreter", None).await;
        assert_eq!(v["arretee"], true);
        assert_eq!(hote.arretees.lock().await.clone(), vec![7]);
        let (_, v) = appel(r, "GET", "/etat", None).await;
        assert_eq!(v["active"], false);
    }

    /// Contre le silence inexpliqué : une autorisation jamais accordée ET une
    /// capture qui ne rend que des zéros se DISENT dans `/etat`.
    #[tokio::test]
    async fn des_zeros_sans_autorisation_sont_dits_en_clair() {
        let s = Simulees::avec("Loopback Audio", 44_100);
        s.rendre_des_zeros("Loopback Audio");
        let c = Controleur::new(s, Arc::new(HoteTemoin::default()));
        c.assurer_capture("Loopback Audio").unwrap();
        let r = router(EtatRoutes {
            controleur: c,
            autorisation: jamais,
        });
        // Le silence numérique se compte en temps de CAPTURE (trames), pas
        // en temps d'horloge : on attend qu'il ait dépassé le seuil.
        let mut v = Value::Null;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            v = appel(r.clone(), "GET", "/etat", None).await.1;
            if v["silence_numerique_s"].as_f64().unwrap_or(0.0) >= 3.0 {
                break;
            }
        }
        assert_eq!(v["autorisation"], "non_demandee");
        assert!(v["silence_numerique_s"].as_f64().unwrap() >= 3.0, "{v}");
        let d = v["diagnostic"]
            .as_str()
            .expect("un diagnostic, pas un silence");
        assert!(d.contains("zéros"), "{d}");
        assert!(d.contains("Microphone"), "{d}");
        assert!(v["niveau_crete_dbfs"].is_null());
        assert_eq!(v["etat"], "autorisation_refusee");
    }

    /// Un système audio qui ne répond pas (constaté : serveur lancé par
    /// launchd) ne doit pas rendre `/etat` muet : il répond, et dit pourquoi.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn un_systeme_audio_muet_est_dit_par_etat_au_lieu_de_le_bloquer() {
        let s = Simulees::avec("Yeti X", 48_000);
        s.muet.store(true, std::sync::atomic::Ordering::SeqCst);
        let c = Controleur::new(s.clone(), Arc::new(HoteTemoin::default()));
        let c2 = c.clone();
        let bloque = tokio::task::spawn_blocking(move || c2.assurer_capture("Yeti X"));
        let r = router(EtatRoutes {
            controleur: c,
            autorisation: jamais,
        });
        tokio::time::sleep(Duration::from_millis(5_300)).await;
        // Dans une tâche à part : si le gestionnaire se bloquait, c'est ELLE
        // qui resterait prise, et le délai ci-dessous tomberait quand même.
        let etat = tokio::spawn(appel(r, "GET", "/etat", None));
        let reponse = tokio::time::timeout(Duration::from_secs(2), etat).await;
        s.muet.store(false, std::sync::atomic::Ordering::SeqCst);
        let (code, v) = reponse
            .expect("/etat ne doit pas attendre le système audio")
            .unwrap();
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["demarrage_en_cours"]["entree"], "Yeti X", "{v}");
        let d = v["diagnostic"].as_str().unwrap();
        assert!(d.contains("ne répond pas"), "{d}");
        bloque.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn les_refus_disent_pourquoi() {
        let c = Controleur::new(
            Simulees::avec("Yeti X", 48_000),
            Arc::new(HoteTemoin::default()),
        );
        let r = router(EtatRoutes {
            controleur: c,
            autorisation: accordee,
        });
        let (code, v) = appel(
            r,
            "POST",
            "/jouer",
            Some(json!({"entree": "Absente", "zone_id": 1})),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"], "entree_inconnue");

        let c = Controleur::new(Arc::new(AucunePile), Arc::new(HoteTemoin::default()));
        let r = router(EtatRoutes {
            controleur: c,
            autorisation: accordee,
        });
        let (code, v) = appel(r.clone(), "GET", "/entrees", None).await;
        assert_eq!(code, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(v["error"], "capture_non_compilee");
        let (_, v) = appel(r, "GET", "/etat", None).await;
        assert_eq!(v["capture_compilee"], false);
        assert_eq!(v["active"], false);
    }
}
