//! `POST /zones/{id}/browser-error` — le LECTEUR d'une zone navigateur dit
//! pourquoi il ne joue pas (#5975).
//!
//! Alex Campbell (Safari 18.6, Docker, une seule zone « This computer », fils
//! forum 2178 et 2246) : « Tourist » et la première piste d'une playlist ne
//! jouent pas. Le journal joint au rapport montrait des `stream_request`, puis
//! `api_next_requested` : rien ne disait POURQUOI l'élément `<audio>` avait
//! lâché. La raison n'existait que dans la console de Safari — `MediaError`
//! 4 (format ou réponse refusés) n'est pas 2 (réseau), et un `play()` refusé
//! hors geste (`NotAllowedError`) n'est pas une panne du flux. Trois semaines
//! d'issue sans pouvoir trancher entre ces causes.
//!
//! Le client appelle cette route sur l'évènement `error` de l'élément, et sur
//! un `play()` refusé. Le serveur écrit UNE ligne `browser_media_error` qui
//! met côte à côte le code du navigateur et ce qu'il a servi pour ce flux :
//! format, MIME, voie (fichier, mandataire, conversion, direct radio), longueur
//! annoncée. Le prochain rapport envoyé depuis Tune porte donc la cause.
//!
//! Corps : `{"stream_id": "…", "kind": "media_error" | "play_rejected",
//! "code": 4, "message": "…", "position_ms": 0}` — tous facultatifs.
//!
//! Réponses : 200 `{"status": "logged", "diagnostic": {…}}` ; 404 zone
//! inconnue ; 409 `not_a_browser_zone`. La route n'agit sur rien : elle
//! journalise et rend ce qu'elle a journalisé.

use super::*;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::http::streamer::StreamSession;

/// Au-delà, un message de navigateur ne dit plus rien d'utile au journal.
const LONGUEUR_MAX_DU_MESSAGE: usize = 300;

#[derive(Debug, Deserialize)]
pub(super) struct ErreurMediaRequest {
    #[serde(default)]
    stream_id: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    code: Option<u16>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    position_ms: Option<i64>,
}

/// Le nom d'un code `MediaError` (HTML, « media elements ») — ou celui d'un
/// refus de `play()`, qui n'a pas de code.
pub(super) fn nom_du_code(kind: &str, code: Option<u16>) -> &'static str {
    if kind == "play_rejected" {
        return "PLAY_REJECTED";
    }
    match code {
        Some(1) => "MEDIA_ERR_ABORTED",
        Some(2) => "MEDIA_ERR_NETWORK",
        Some(3) => "MEDIA_ERR_DECODE",
        Some(4) => "MEDIA_ERR_SRC_NOT_SUPPORTED",
        _ => "inconnu",
    }
}

/// Ce que le code veut dire pour qui lit le journal du testeur.
pub(super) fn lecture_du_code(kind: &str, code: Option<u16>, message: &str) -> &'static str {
    if kind == "play_rejected" {
        return if message.contains("NotAllowedError") {
            "lecture refusée hors geste (politique de lecture automatique) : le flux n'est pas en cause"
        } else if message.contains("NotSupportedError") {
            "le navigateur ne sait pas lire la source servie (format, conteneur ou Content-Type)"
        } else {
            "play() refusé par le navigateur"
        };
    }
    match code {
        Some(1) => "chargement interrompu par le client",
        Some(2) => "réseau : la connexion du flux a échoué en cours de route",
        Some(3) => {
            "décodage interrompu : octets que le navigateur ne sait pas décoder en cours de lecture"
        }
        Some(4) => {
            "le navigateur refuse ce qui a été servi : codec ou conteneur, Content-Type, ou réponse à sa sonde de plage"
        }
        _ => "erreur média sans code",
    }
}

/// Ce que le serveur a servi pour ce flux, tel qu'on peut le relire.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(super) struct CeQuiAEteServi {
    pub voie: &'static str,
    pub format: String,
    pub mime: String,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
    /// `None` : aucune longueur annoncée — le corps part en chunké.
    pub longueur_annoncee: Option<u64>,
    /// Extension du fichier servi, pour une session de fichier.
    pub extension: Option<String>,
}

pub(super) async fn ce_qui_a_ete_servi(session: &StreamSession) -> CeQuiAEteServi {
    let fichier = session.file_path.lock().await.clone();
    let mandataire = session.proxy_url.lock().await.is_some();
    let info = &session.info;
    let (voie, longueur_annoncee) = if fichier.is_some() {
        ("fichier", info.file_size)
    } else if mandataire {
        ("mandataire", info.file_size)
    } else if session.is_radio {
        ("direct radio", None)
    } else if info.format == "wav" {
        ("conversion", info.wav_content_length())
    } else {
        ("conversion", info.file_size)
    };
    let extension = fichier.as_deref().and_then(|p| {
        std::path::Path::new(p)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
    });
    CeQuiAEteServi {
        voie,
        format: info.format.clone(),
        mime: info.mime_type.clone(),
        sample_rate: info.sample_rate,
        bit_depth: info.bit_depth,
        channels: info.channels,
        longueur_annoncee,
        extension,
    }
}

fn tronquer(message: &str) -> String {
    if message.chars().count() <= LONGUEUR_MAX_DU_MESSAGE {
        return message.to_string();
    }
    let mut s: String = message.chars().take(LONGUEUR_MAX_DU_MESSAGE).collect();
    s.push('…');
    s
}

pub(super) async fn signaler_erreur_media_navigateur(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    headers: axum::http::HeaderMap,
    Json(corps): Json<ErreurMediaRequest>,
) -> axum::response::Response {
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten();
    let Some(zone) = zone else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "zone_not_found" })),
        )
            .into_response();
    };
    if zone.output_type.as_deref() != Some("browser") {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "not_a_browser_zone" })),
        )
            .into_response();
    }

    let kind = match corps.kind.as_deref() {
        Some("play_rejected") => "play_rejected",
        _ => "media_error",
    };
    let message = tronquer(corps.message.as_deref().unwrap_or(""));
    let stream_id = corps.stream_id.clone().unwrap_or_default();
    let session = if stream_id.is_empty() {
        None
    } else {
        let sessions = state.streamer.sessions_state();
        let sessions = sessions.lock().await;
        sessions.get(&stream_id).cloned()
    };
    let servi = match session {
        Some(ref s) => Some(ce_qui_a_ete_servi(s).await),
        None => None,
    };
    let agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    let nom = nom_du_code(kind, corps.code);
    let lecture = lecture_du_code(kind, corps.code, &message);

    warn!(
        zone_id,
        stream_id = %stream_id,
        kind,
        code = corps.code.unwrap_or(0),
        code_nom = nom,
        message = %message,
        position_ms = corps.position_ms.unwrap_or(0),
        agent,
        voie = servi.as_ref().map(|s| s.voie).unwrap_or("session inconnue"),
        format = servi.as_ref().map(|s| s.format.as_str()).unwrap_or("-"),
        mime = servi.as_ref().map(|s| s.mime.as_str()).unwrap_or("-"),
        sample_rate = servi.as_ref().map(|s| s.sample_rate).unwrap_or(0),
        bit_depth = servi.as_ref().map(|s| s.bit_depth).unwrap_or(0),
        longueur_annoncee = ?servi.as_ref().and_then(|s| s.longueur_annoncee),
        extension = servi.as_ref().and_then(|s| s.extension.as_deref()).unwrap_or("-"),
        lecture,
        "browser_media_error — le lecteur de la zone navigateur ne joue pas ce flux (#5975)"
    );

    Json(json!({
        "status": "logged",
        "diagnostic": {
            "kind": kind,
            "code": corps.code,
            "code_nom": nom,
            "lecture": lecture,
            "message": message,
            "stream_id": stream_id,
            "servi": servi,
        }
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use tune_core::http::streamer::StreamInfo;

    const SAFARI: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
                          (KHTML, like Gecko) Version/18.6 Safari/605.1.15";

    async fn appeler(app: &axum::Router, zone_id: i64, corps: &str) -> (StatusCode, Value) {
        let requete = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/zones/{zone_id}/browser-error"))
            .header("content-type", "application/json")
            .header("User-Agent", SAFARI)
            .body(Body::from(corps.to_string()))
            .unwrap();
        let reponse = app.clone().oneshot(requete).await.unwrap();
        let statut = reponse.status();
        let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    fn zone(state: &AppState, id: i64, type_de_sortie: &str) {
        state
            .backend
            .execute(
                "INSERT INTO zones (id,name,output_type) VALUES (?1,?2,?3)",
                &[&id, &"This computer", &type_de_sortie],
            )
            .unwrap();
    }

    /// Le cas d'Alex : une piste de bibliothèque servie en fichier, que Safari
    /// refuse (`MediaError` 4). La ligne dit le code ET ce qui a été servi —
    /// voie, format, MIME, extension, longueur — sans console Safari.
    #[tokio::test]
    async fn une_erreur_4_dit_ce_qui_a_ete_servi_5975() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        zone(&state, 5_975, "browser");
        let dir = tempfile::tempdir().unwrap();
        let chemin = dir.path().join("tourist.m4a");
        std::fs::write(&chemin, b"ftyp").unwrap();
        let info = StreamInfo {
            format: "m4a".into(),
            mime_type: "audio/mp4".into(),
            sample_rate: 44_100,
            bit_depth: 16,
            channels: 2,
            file_size: Some(4),
            ..Default::default()
        };
        let sid = state
            .streamer
            .create_file_session(info, chemin.to_string_lossy().to_string(), false)
            .await;
        let app = crate::routes::router(state.clone());
        let (statut, v) = appeler(
            &app,
            5_975,
            &format!(r#"{{"stream_id":"{sid}","kind":"media_error","code":4,"message":"","position_ms":0}}"#),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "POST /browser-error : {v}");
        let d = &v["diagnostic"];
        assert_eq!(d["code_nom"], "MEDIA_ERR_SRC_NOT_SUPPORTED", "{v}");
        assert_eq!(d["servi"]["voie"], "fichier", "{v}");
        assert_eq!(d["servi"]["mime"], "audio/mp4", "{v}");
        assert_eq!(d["servi"]["extension"], "m4a", "{v}");
        assert_eq!(d["servi"]["longueur_annoncee"], 4, "{v}");
    }

    /// Une conversion WAV sans durée n'annonce aucune longueur : le corps part
    /// en chunké, ce que Safari refuse à sa sonde `bytes=0-1`. Le diagnostic
    /// doit le montrer (`longueur_annoncee: null`, voie `conversion`).
    #[tokio::test]
    async fn une_conversion_sans_longueur_se_voit_5975() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        zone(&state, 5_976, "browser");
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44_100,
            bit_depth: 16,
            channels: 2,
            ..Default::default()
        };
        let (sid, _tx, _pret) = state.streamer.create_session(info, false, 8).await;
        let app = crate::routes::router(state.clone());
        let (statut, v) =
            appeler(&app, 5_976, &format!(r#"{{"stream_id":"{sid}","code":4}}"#)).await;
        assert_eq!(statut, StatusCode::OK, "{v}");
        assert_eq!(v["diagnostic"]["servi"]["voie"], "conversion", "{v}");
        assert!(
            v["diagnostic"]["servi"]["longueur_annoncee"].is_null(),
            "{v}"
        );
    }

    /// Un `play()` refusé hors geste n'est pas une panne du flux, et la ligne
    /// le dit ; un flux inconnu (session expirée) ne fait pas échouer la route.
    #[tokio::test]
    async fn un_refus_de_lecture_automatique_est_nomme_5975() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        zone(&state, 5_977, "browser");
        let app = crate::routes::router(state.clone());
        let (statut, v) = appeler(
            &app,
            5_977,
            r#"{"stream_id":"disparu","kind":"play_rejected","message":"NotAllowedError: The request is not allowed"}"#,
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{v}");
        assert_eq!(v["diagnostic"]["code_nom"], "PLAY_REJECTED", "{v}");
        assert!(
            v["diagnostic"]["lecture"]
                .as_str()
                .unwrap()
                .contains("hors geste"),
            "{v}"
        );
        assert!(v["diagnostic"]["servi"].is_null(), "{v}");
    }

    /// Mêmes refus que `/browser-playing` : zone inconnue, zone à appareil.
    #[tokio::test]
    async fn zone_inconnue_ou_a_appareil_5975() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        zone(&state, 5_978, "dlna");
        let app = crate::routes::router(state.clone());
        let (statut, v) = appeler(&app, 5_978, r#"{"code":4}"#).await;
        assert_eq!(statut, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"], "not_a_browser_zone");
        let (statut, _) = appeler(&app, 999_999, r#"{"code":4}"#).await;
        assert_eq!(statut, StatusCode::NOT_FOUND);
    }

    #[test]
    fn un_message_trop_long_est_tronque() {
        let long = "é".repeat(LONGUEUR_MAX_DU_MESSAGE + 10);
        let t = tronquer(&long);
        assert_eq!(t.chars().count(), LONGUEUR_MAX_DU_MESSAGE + 1);
        assert!(t.ends_with('…'));
    }
}
