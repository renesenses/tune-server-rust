#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::routing::post;
    use tokio::sync::Mutex;

    use crate::outputs::dlna::DlnaOutput;
    use crate::outputs::traits::{OutputTarget, PlayMedia, TransportState};

    #[derive(Clone)]
    struct MockState {
        play_count: Arc<AtomicU32>,
        pause_count: Arc<AtomicU32>,
        stop_count: Arc<AtomicU32>,
        seek_count: Arc<AtomicU32>,
        set_next_count: Arc<AtomicU32>,
        volume_count: Arc<AtomicU32>,
        /// Nombre d'actions SOAP `GetMute` reçues. Le poller interroge chaque
        /// renderer à 1 Hz pendant toute la lecture (#2263) : ce compteur est
        /// là pour prouver que `get_status` n'en émet plus AUCUNE.
        get_mute_count: Arc<AtomicU32>,
        transport_state: Arc<Mutex<String>>,
        last_seek_target: Arc<Mutex<String>>,
        /// Simule Platinum/1.0.5.13 : tout SetAVTransportURI/SetNext dont le
        /// corps dépasse cette taille reçoit `500` au corps VIDE — le serveur
        /// n'a « pas su lire » la requête.
        set_uri_max_corps: Arc<Mutex<Option<usize>>>,
        /// Simule l'Eversolo coincé : Stop est acquitté mais IGNORÉ tant
        /// qu'un Pause n'est pas passé d'abord.
        stop_exige_pause: Arc<Mutex<bool>>,
        /// Ce que GetMediaInfo rapporte comme CurrentURI. Mis à jour par un
        /// SetAVTransportURI accepté, sauf si `media_info_fige` est vrai.
        current_uri: Arc<Mutex<String>>,
        media_info_fige: Arc<Mutex<bool>>,
        /// Corps des SetAVTransportURI reçus, dans l'ordre.
        set_uri_corps: Arc<Mutex<Vec<String>>>,
        /// « Salon » (#2581) : ce renderer refuse `Play` avec le code UPnP 701
        /// « Transition not available » quand il ne tient AUCUN média.
        salon_701_sans_media: Arc<Mutex<bool>>,
        /// « Salon » (#2581) : il refuse aussi les `n` premiers `Play` — il
        /// charge encore l'URI, et se déclare TRANSITIONING pendant ce temps.
        refus_701_restants: Arc<Mutex<u32>>,
        /// « Salon » (#2581) : un `Stop` lui fait OUBLIER son média. C'est le
        /// piège du barème aveugle : le Stop du premier réessai le prive du
        /// média, et tout `Play` suivant est un 701 de plus.
        stop_oublie_le_media: Arc<Mutex<bool>>,
        /// « Salon » (#2581) : le prochain `SetAVTransportURI` accepté est
        /// aussitôt perdu — une seule fois.
        oublie_le_media_une_fois: Arc<Mutex<bool>>,
        /// Nombre de `Play` REFUSÉS avec un 701.
        play_refus_701: Arc<AtomicU32>,
        /// Le renderer ACQUITTE le premier `Play` puis refuse tous les suivants
        /// avec un 701 — c'est le `Play` de la relance (« acquitté mais jamais
        /// appliqué ») qui se fait refuser. Ce chemin-là repose l'URI et rejoue,
        /// exactement comme la boucle de #2581, mais sa réponse n'était pas
        /// relue : le refus passait pour un acquittement.
        relance_701: Arc<Mutex<bool>>,
        /// Nombre TOTAL d'actions SOAP reçues, tous services confondus.
        ///
        /// L'instrument de mesure de #2263 : le sondeur passe par `get_status`
        /// une fois par seconde et par zone pendant toute la lecture, et ce
        /// compteur dit combien d'actions cela coûte VRAIMENT au renderer.
        /// Annoncer une réduction sans le lire serait une promesse.
        actions_soap: Arc<AtomicU32>,
        /// Position rendue par `GetPositionInfo`, en ms.
        position_ms: Arc<AtomicU32>,
        /// La position avance-t-elle d'une seconde à chaque `GetPositionInfo` ?
        /// Un renderer figé (position immobile alors qu'il se dit en lecture)
        /// est le cas que la contre-vérification doit rattraper.
        position_avance: Arc<AtomicBool>,
        /// Le renderer REFUSE les `SUBSCRIBE` : c'est le repli qu'on veut
        /// pouvoir prouver, pas seulement décrire.
        abonnement_refuse: Arc<AtomicBool>,
        /// Le renderer refuse tout RENOUVELLEMENT (le premier `SUBSCRIBE`
        /// passe, les suivants sont rejetés) — un appareil redémarré qui ne
        /// connaît plus le SID.
        renouvellement_refuse: Arc<AtomicBool>,
        /// Nombre de `SUBSCRIBE` reçus.
        subscribe_count: Arc<AtomicU32>,
        /// `TransportState` que le renderer POUSSE dans son `LastChange`. Peut
        /// différer volontairement de `transport_state`, celui qu'il rend en
        /// SOAP : c'est le renderer qui ment par évènement.
        etat_pousse: Arc<Mutex<String>>,
        /// Le renderer n'émet PAS `CurrentTrackDuration` dans son `LastChange`.
        duree_jamais_poussee: Arc<AtomicBool>,
        /// Quand c'est `Some`, `SetVolume` est REFUSÉ avec ce code UPnP.
        ///
        /// C'est la panne d'Eric (#1393, fil forum) : un renderer Diretta et un
        /// PC vu comme zone DLNA n'appliquaient pas le volume. Un renderer
        /// logiciel sans RenderingControl complet répond `602 Optional Action
        /// Not Implemented` — statut HTTP 500 AVEC corps, la forme qu'un vrai
        /// appareil rend et que `soap_action` restitue telle quelle.
        volume_refus_upnp: Arc<Mutex<Option<(u16, &'static str)>>>,
    }

    impl Default for MockState {
        fn default() -> Self {
            Self {
                play_count: Arc::new(AtomicU32::new(0)),
                pause_count: Arc::new(AtomicU32::new(0)),
                stop_count: Arc::new(AtomicU32::new(0)),
                seek_count: Arc::new(AtomicU32::new(0)),
                set_next_count: Arc::new(AtomicU32::new(0)),
                volume_count: Arc::new(AtomicU32::new(0)),
                get_mute_count: Arc::new(AtomicU32::new(0)),
                transport_state: Arc::new(Mutex::new("STOPPED".into())),
                last_seek_target: Arc::new(Mutex::new(String::new())),
                set_uri_max_corps: Arc::new(Mutex::new(None)),
                stop_exige_pause: Arc::new(Mutex::new(false)),
                current_uri: Arc::new(Mutex::new(String::new())),
                media_info_fige: Arc::new(Mutex::new(false)),
                set_uri_corps: Arc::new(Mutex::new(Vec::new())),
                salon_701_sans_media: Arc::new(Mutex::new(false)),
                refus_701_restants: Arc::new(Mutex::new(0)),
                stop_oublie_le_media: Arc::new(Mutex::new(false)),
                oublie_le_media_une_fois: Arc::new(Mutex::new(false)),
                play_refus_701: Arc::new(AtomicU32::new(0)),
                relance_701: Arc::new(Mutex::new(false)),
                actions_soap: Arc::new(AtomicU32::new(0)),
                position_ms: Arc::new(AtomicU32::new(90_000)),
                // Position IMMOBILE par défaut : c'est ce que le renderer
                // bouchonné rendait avant #2263, et les tests écrits contre lui
                // le supposent. Chaque test qui veut un appareil qui avance
                // vraiment l'arme explicitement.
                position_avance: Arc::new(AtomicBool::new(false)),
                abonnement_refuse: Arc::new(AtomicBool::new(false)),
                renouvellement_refuse: Arc::new(AtomicBool::new(false)),
                subscribe_count: Arc::new(AtomicU32::new(0)),
                etat_pousse: Arc::new(Mutex::new("PLAYING".into())),
                duree_jamais_poussee: Arc::new(AtomicBool::new(false)),
                volume_refus_upnp: Arc::new(Mutex::new(None)),
            }
        }
    }

    fn hms(ms: u32) -> String {
        let s = ms / 1000;
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    }

    /// Le document `LastChange` d'`AVTransport` tel qu'un renderer l'envoie :
    /// un XML ÉCHAPPÉ dans le texte de la propriété, pas un XML imbriqué.
    fn propertyset_avtransport(etat: &str, uri: &str, duree: &str) -> String {
        // Une durée vide = renderer qui n'émet PAS `CurrentTrackDuration`.
        // La spec ne l'impose pas, et certains s'en dispensent.
        let duree_xml = if duree.is_empty() {
            String::new()
        } else {
            format!(r#"<CurrentTrackDuration val="{duree}"/>"#)
        };
        let interieur = format!(
            r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0"><TransportState val="{etat}"/><CurrentTrackURI val="{uri}"/>{duree_xml}</InstanceID></Event>"#
        );
        let echappe = interieur
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        format!(
            r#"<?xml version="1.0"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>{echappe}</LastChange></e:property></e:propertyset>"#
        )
    }

    /// Idem pour `RenderingControl`, avec ses trois voies — la voie `Master`
    /// n'est pas la dernière du document, exprès.
    fn propertyset_renderingcontrol(volume: u32, muet: u32) -> String {
        let interieur = format!(
            r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/RCS/"><InstanceID val="0"><Volume channel="Master" val="{volume}"/><Mute channel="Master" val="{muet}"/><Volume channel="LF" val="11"/><Volume channel="RF" val="99"/></InstanceID></Event>"#
        );
        let echappe = interieur
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        format!(
            r#"<?xml version="1.0"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>{echappe}</LastChange></e:property></e:propertyset>"#
        )
    }

    /// Envoie un `NOTIFY` GENA à l'adresse de rappel, comme le ferait
    /// l'appareil juste après avoir accepté l'abonnement.
    async fn notifier(callback: &str, corps: String) {
        let Ok(method) = reqwest::Method::from_bytes(b"NOTIFY") else {
            return;
        };
        let client = crate::http::client::builder().build().unwrap_or_default();
        let _ = client
            .request(method, callback)
            .header("NT", "upnp:event")
            .header("NTS", "upnp:propchange")
            .header("Content-Type", "text/xml")
            .body(corps)
            .send()
            .await;
    }

    fn callback_de(entetes: &axum::http::HeaderMap) -> Option<String> {
        entetes
            .get("CALLBACK")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim_matches(['<', '>']).to_string())
    }

    /// `eventSubURL` d'`AVTransport` du renderer bouchonné.
    async fn abonnement_avtransport(
        State(state): State<MockState>,
        entetes: axum::http::HeaderMap,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let n = state.subscribe_count.fetch_add(1, Ordering::Relaxed);
        if state.abonnement_refuse.load(Ordering::Relaxed)
            || (n > 0 && state.renouvellement_refuse.load(Ordering::Relaxed))
        {
            return (axum::http::StatusCode::PRECONDITION_FAILED, "").into_response();
        }
        if let Some(cb) = callback_de(&entetes) {
            let etat = state.etat_pousse.lock().await.clone();
            let duree = if state.duree_jamais_poussee.load(Ordering::Relaxed) {
                ""
            } else {
                "0:05:00"
            };
            // Le `NOTIFY` initial part AVANT que la réponse au `SUBSCRIBE` ne
            // soit rendue — l'ordre le plus dur, et un ordre que GENA autorise :
            // rien n'oblige l'appareil à attendre que notre client ait fini de
            // lire sa réponse. C'est l'ordre qui perdait l'état initial tant
            // que le gestionnaire de rappel était enregistré après coup, et
            // comme `AVTransport` n'émet plus rien tant que rien ne change,
            // l'abonnement restait muet pour toujours. Le laisser en tâche
            // détachée rendrait ce test complice du bogue : il gagnerait la
            // course une fois sur deux et se dirait vert.
            notifier(
                &cb,
                propertyset_avtransport(&etat, "http://tune.test/piste.flac", duree),
            )
            .await;
        }
        ([("SID", "uuid:mock-av"), ("TIMEOUT", "Second-300")], "").into_response()
    }

    async fn abonnement_renderingcontrol(
        State(state): State<MockState>,
        entetes: axum::http::HeaderMap,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        if state.abonnement_refuse.load(Ordering::Relaxed) {
            return (axum::http::StatusCode::PRECONDITION_FAILED, "").into_response();
        }
        if let Some(cb) = callback_de(&entetes) {
            notifier(&cb, propertyset_renderingcontrol(42, 0)).await;
        }
        ([("SID", "uuid:mock-rc"), ("TIMEOUT", "Second-300")], "").into_response()
    }

    fn extract_action(body: &str) -> String {
        // Find <u:ACTION in the SOAP body
        if let Some(start) = body.find("<u:") {
            let rest = &body[start + 3..];
            if let Some(end) = rest.find(|c: char| c == ' ' || c == '>') {
                return rest[..end].to_string();
            }
        }
        String::new()
    }

    fn extract_tag(xml: &str, tag: &str) -> String {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        if let Some(s) = xml.find(&open) {
            let s = s + open.len();
            if let Some(e) = xml[s..].find(&close) {
                return xml[s..s + e].to_string();
            }
        }
        String::new()
    }

    fn soap_ok(action: &str, inner: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">{inner}</u:{action}Response></s:Body></s:Envelope>"#
        )
    }

    /// La faute SOAP EXACTE relevée dans le journal de FabienM (#2581),
    /// rendue comme un vrai renderer la rend : statut HTTP 500 AVEC corps.
    fn soap_701() -> String {
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>701</errorCode><errorDescription>Transition not available</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#.to_string()
    }

    /// Une faute SOAP UPnP quelconque, à la forme exacte de `soap_701()`.
    ///
    /// Sert le refus de `SetVolume` (#1393) : `602 Optional Action Not
    /// Implemented` est ce que rend un renderer logiciel dont le
    /// RenderingControl est décoratif — le « PC vu comme zone DLNA » d'Eric.
    fn soap_fault_upnp(code: u16, description: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>{code}</errorCode><errorDescription>{description}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#
        )
    }

    async fn av_handler(State(state): State<MockState>, body: String) -> axum::response::Response {
        use axum::response::IntoResponse;
        let action = extract_action(&body);
        state.actions_soap.fetch_add(1, Ordering::Relaxed);
        match action.as_str() {
            "SetAVTransportURI" => {
                state.set_uri_corps.lock().await.push(body.clone());
                if let Some(max) = *state.set_uri_max_corps.lock().await
                    && body.len() > max
                {
                    // Platinum : statut d'échec, corps vide, requête jetée.
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, String::new())
                        .into_response();
                }
                let uri = extract_tag(&body, "CurrentURI");
                if !*state.media_info_fige.lock().await {
                    *state.current_uri.lock().await = uri;
                }
                // « Salon » (#2581) : média accepté puis aussitôt perdu.
                {
                    let mut oubli = state.oublie_le_media_une_fois.lock().await;
                    if *oubli {
                        *oubli = false;
                        state.current_uri.lock().await.clear();
                        *state.transport_state.lock().await = "NO_MEDIA_PRESENT".into();
                    }
                }
                soap_ok("SetAVTransportURI", "").into_response()
            }
            "Play" => {
                // « Salon » (#2581) : sans média, la transition est impossible.
                if *state.salon_701_sans_media.lock().await
                    && state.current_uri.lock().await.is_empty()
                {
                    state.play_refus_701.fetch_add(1, Ordering::Relaxed);
                    *state.transport_state.lock().await = "NO_MEDIA_PRESENT".into();
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, soap_701())
                        .into_response();
                }
                // « Salon » (#2581) : il charge encore l'URI.
                let charge_encore = {
                    let mut restants = state.refus_701_restants.lock().await;
                    if *restants > 0 {
                        *restants -= 1;
                        true
                    } else {
                        false
                    }
                };
                if charge_encore {
                    state.play_refus_701.fetch_add(1, Ordering::Relaxed);
                    *state.transport_state.lock().await = "TRANSITIONING".into();
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, soap_701())
                        .into_response();
                }
                // Le premier `Play` est acquitté ; le suivant — celui de la
                // relance — se fait refuser. Déterministe : c'est le COMPTE de
                // Play acceptés qui décide, jamais une horloge.
                if *state.relance_701.lock().await && state.play_count.load(Ordering::Relaxed) >= 1
                {
                    state.play_refus_701.fetch_add(1, Ordering::Relaxed);
                    *state.transport_state.lock().await = "STOPPED".into();
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, soap_701())
                        .into_response();
                }
                state.play_count.fetch_add(1, Ordering::Relaxed);
                *state.transport_state.lock().await = "PLAYING".into();
                soap_ok("Play", "").into_response()
            }
            "Pause" => {
                state.pause_count.fetch_add(1, Ordering::Relaxed);
                *state.transport_state.lock().await = "PAUSED_PLAYBACK".into();
                // Le Pause libère le Stop (simulation du zombie Eversolo).
                *state.stop_exige_pause.lock().await = false;
                soap_ok("Pause", "").into_response()
            }
            "Stop" => {
                state.stop_count.fetch_add(1, Ordering::Relaxed);
                if !*state.stop_exige_pause.lock().await {
                    *state.transport_state.lock().await = "STOPPED".into();
                    // « Salon » (#2581) : ce Stop lui fait oublier son média.
                    if *state.stop_oublie_le_media.lock().await {
                        state.current_uri.lock().await.clear();
                        *state.transport_state.lock().await = "NO_MEDIA_PRESENT".into();
                    }
                }
                // Acquitté dans TOUS les cas — c'est le comportement observé.
                soap_ok("Stop", "").into_response()
            }
            "Seek" => {
                state.seek_count.fetch_add(1, Ordering::Relaxed);
                *state.last_seek_target.lock().await = extract_tag(&body, "Target");
                soap_ok("Seek", "").into_response()
            }
            "SetNextAVTransportURI" => {
                state.set_next_count.fetch_add(1, Ordering::Relaxed);
                if let Some(max) = *state.set_uri_max_corps.lock().await
                    && body.len() > max
                {
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, String::new())
                        .into_response();
                }
                soap_ok("SetNextAVTransportURI", "").into_response()
            }
            "GetTransportInfo" => {
                let ts = state.transport_state.lock().await.clone();
                soap_ok(
                    "GetTransportInfo",
                    &format!(
                        "<CurrentTransportState>{ts}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>"
                    ),
                )
                .into_response()
            }
            "GetMediaInfo" => {
                let uri = state.current_uri.lock().await.clone();
                soap_ok(
                    "GetMediaInfo",
                    &format!("<NrTracks>1</NrTracks><CurrentURI>{uri}</CurrentURI>"),
                )
                .into_response()
            }
            "GetPositionInfo" => {
                // La position avance d'une seconde par relevé, comme sur un
                // appareil qui joue. Un renderer FIGÉ (`position_avance` à
                // faux) rend deux fois la même : c'est ce cas-là que la
                // contre-vérification de l'état poussé doit rattraper.
                let ms = if state.position_avance.load(Ordering::Relaxed) {
                    state.position_ms.fetch_add(1000, Ordering::Relaxed) + 1000
                } else {
                    state.position_ms.load(Ordering::Relaxed)
                };
                let t = hms(ms);
                soap_ok(
                    "GetPositionInfo",
                    &format!(
                        "<Track>1</Track><TrackDuration>0:05:00</TrackDuration><TrackMetaData></TrackMetaData><TrackURI></TrackURI><RelTime>{t}</RelTime><AbsTime>{t}</AbsTime><RelCount>0</RelCount><AbsCount>0</AbsCount>"
                    ),
                )
                .into_response()
            }
            _ => soap_ok(&action, "").into_response(),
        }
    }

    async fn rc_handler(State(state): State<MockState>, body: String) -> axum::response::Response {
        use axum::response::IntoResponse;
        let action = extract_action(&body);
        state.actions_soap.fetch_add(1, Ordering::Relaxed);
        match action.as_str() {
            "SetVolume" => {
                // Compté AVANT le refus : la commande a bien été émise, c'est
                // la réponse qui dit non. Sans ce compteur, un test vert ne
                // distinguerait pas « refusé par l'appareil » de « jamais
                // envoyé ».
                state.volume_count.fetch_add(1, Ordering::Relaxed);
                if let Some((code, description)) = *state.volume_refus_upnp.lock().await {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        soap_fault_upnp(code, description),
                    )
                        .into_response();
                }
                soap_ok("SetVolume", "").into_response()
            }
            "GetVolume" => {
                soap_ok("GetVolume", "<CurrentVolume>50</CurrentVolume>").into_response()
            }
            "SetMute" => soap_ok("SetMute", "").into_response(),
            // Répond « coupé » exprès : si `get_status` interrogeait encore le
            // renderer, le statut rendu porterait `muted = true` et les tests
            // de #2263 le verraient.
            "GetMute" => {
                state.get_mute_count.fetch_add(1, Ordering::Relaxed);
                soap_ok("GetMute", "<CurrentMute>1</CurrentMute>").into_response()
            }
            _ => soap_ok(&action, "").into_response(),
        }
    }

    async fn start_mock(state: MockState) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/AVTransport", post(av_handler))
            .route("/RenderingControl", post(rc_handler))
            // `SUBSCRIBE` n'est pas une méthode HTTP standard : `any` est la
            // seule façon de l'atteindre depuis axum.
            .route(
                "/AVTransport/event",
                axum::routing::any(abonnement_avtransport),
            )
            .route(
                "/RenderingControl/event",
                axum::routing::any(abonnement_renderingcontrol),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn make_dlna(base: &str) -> DlnaOutput {
        DlnaOutput::new(
            "Mock Renderer".into(),
            "mock-dlna-001".into(),
            "127.0.0.1".into(),
            format!("{base}/AVTransport"),
            format!("{base}/RenderingControl"),
            None,
        )
    }

    /// Un média « réel » : les champs du Locatelli DSD128 du terrain, BOM
    /// U+FEFF dans l'artiste compris. Le DIDL COMPLET de ce média dépasse un
    /// segment TCP ; c'est lui qui déclenchait le `500 Error Parsing XML Body`
    /// de Platinum/1.0.5.13.
    fn media_locatelli(url: &str) -> PlayMedia<'_> {
        PlayMedia {
            url,
            mime_type: "application/x-dsd",
            title: Some("1. Andante: Locatelli Violin Concerto No. 2 in C Minor, Op. 3, No. 2"),
            artist: Some("Jacobs, Lisa\u{feff}The String Soloists"),
            album: Some(
                "2016 L'Arte del Violino (Locatelli Violin Concertos) (DSD128 Binaural) - Lisa Jacobs, The String Soloists",
            ),
            cover_url: Some(
                "http://192.168.1.18:8888/api/v1/library/artwork/4db36f948b9122ce30c05249450e1b3e",
            ),
            duration_ms: Some(487_560),
            file_size: Some(688_779_678),
            ..Default::default()
        }
    }

    /// LA contre-épreuve du 500 avalé (saga DMP-A8, 25/08).
    ///
    /// Le mock joue Platinum : tout SetAVTransportURI dont le corps dépasse
    /// 1200 octets reçoit `500` au corps VIDE. L'ancien code prenait ce 500
    /// pour un acquittement (il ne lisait que le corps), envoyait Play, et la
    /// zone « jouait » une piste jamais transmise. Le nouveau code doit :
    /// 1. voir l'échec de lecture ;
    /// 2. descendre l'échelle de DIDL jusqu'à un corps qui passe ;
    /// 3. finir avec l'URI réellement APPLIQUÉE chez le renderer.
    #[tokio::test]
    async fn un_500_sans_corps_fait_descendre_l_echelle_didl() {
        let state = MockState::default();
        *state.set_uri_max_corps.lock().await = Some(1200);
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        let url = "http://192.168.1.18:8888/stream/echelle-didl.dsf";
        output.play_media(&media_locatelli(url)).await.unwrap();

        let corps = state.set_uri_corps.lock().await.clone();
        assert!(
            corps.len() >= 2,
            "le DIDL complet devait échouer puis être réduit, {} envoi(s)",
            corps.len()
        );
        assert!(
            corps[0].len() > 1200,
            "le premier envoi devait porter le DIDL complet ({} octets)",
            corps[0].len()
        );
        assert!(
            corps.last().unwrap().len() <= 1200,
            "le dernier envoi devait tenir sous la limite de lecture"
        );
        assert_eq!(
            *state.current_uri.lock().await,
            url,
            "l'URI doit être réellement appliquée après la descente d'échelle"
        );
        handle.abort();
    }

    /// L'échec du DIDL complet est une propriété de l'APPAREIL, pas de la
    /// piste (#2394) : une fois le niveau qui passe constaté, les lectures
    /// suivantes doivent démarrer là, sans re-payer l'aller-retour raté (un
    /// warn + ~une requête perdue par piste, constaté sur DMP-A8).
    #[tokio::test]
    async fn le_niveau_didl_qui_passe_est_appris_pour_l_appareil() {
        let state = MockState::default();
        *state.set_uri_max_corps.lock().await = Some(1200);
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        output
            .play_media(&media_locatelli(
                "http://192.168.1.18:8888/stream/apprentissage-a.dsf",
            ))
            .await
            .unwrap();
        let envois_premier = state.set_uri_corps.lock().await.len();
        assert!(
            envois_premier >= 2,
            "le premier play devait échouer au complet puis réduire ({envois_premier} envoi(s))"
        );

        output
            .play_media(&media_locatelli(
                "http://192.168.1.18:8888/stream/apprentissage-b.dsf",
            ))
            .await
            .unwrap();
        let corps = state.set_uri_corps.lock().await.clone();
        assert_eq!(
            corps.len(),
            envois_premier + 1,
            "le second play doit envoyer UN seul SetAVTransportURI, au niveau appris"
        );
        assert!(
            corps.last().unwrap().len() <= 1200,
            "l'envoi appris doit tenir d'emblée sous la limite de lecture"
        );
        handle.abort();
    }

    /// Le DIDL minimal doit VRAIMENT tenir sous un segment TCP (~1448 octets
    /// pour l'enveloppe SOAP complète, en-têtes HTTP en sus) — sinon l'échelle
    /// ne résout rien. Mesuré sur le média réel le plus verbeux du terrain.
    #[tokio::test]
    async fn le_didl_minimal_tient_sous_un_segment() {
        let media = media_locatelli(
            "http://192.168.1.18:8888/stream/aaaaaaaa-bbbb-cccc-dddd-eeeeffff0000.dsf",
        );
        let minimal =
            DlnaOutput::didl_metadata_minimale_pour_test(&media, "1", "application/x-dsd");
        // L'enveloppe SOAP ajoute ~330 octets autour du DIDL échappé.
        assert!(
            minimal.len() + 330 + media.url.len() < 1200,
            "DIDL minimal trop gros : {} octets échappés",
            minimal.len()
        );
        let complet = DlnaOutput::didl_metadata_pour_test(&media, "1", "application/x-dsd");
        assert!(
            complet.len() > minimal.len(),
            "le complet doit être plus riche que le minimal"
        );
    }

    /// Un WAV servi à un renderer qui a appris le DIDL réduit — ALAC 24/192 +
    /// « Forcer le WAV », la configuration d'Yves (forum #1437).
    fn media_wav(url: &str, sample_rate: u32, bit_depth: u32) -> PlayMedia<'_> {
        PlayMedia {
            url,
            mime_type: "audio/wav",
            title: Some("Piste transcodée en WAV"),
            duration_ms: Some(300_000),
            file_size: Some(345_600_044),
            sample_rate: Some(sample_rate),
            bit_depth: Some(bit_depth),
            channels: Some(2),
            ..Default::default()
        }
    }

    /// #1137 et #1458 ne valaient QUE pour le DIDL complet.
    ///
    /// Le DIDL réduit ne transmettait ni profondeur ni fréquence, donc
    /// `dlna_flags_for_mime_bd_sr(mime, None, None)` retombait sur
    /// `PN=LPCM` — le profil 16 bits / 48 kHz — pour un WAV 24 bits ou hi-res.
    /// Un renderer strict rabat alors le flux sur le profil annoncé, lit des
    /// échantillons désalignés et joue du SILENCE. Et comme le niveau réduit
    /// est APPRIS par appareil (#2394), le défaut valait pour toutes les pistes
    /// suivantes, pas seulement la première.
    #[tokio::test]
    async fn le_didl_minimal_ne_ment_plus_sur_le_profil_lpcm() {
        // 16 bits mais 192 kHz : hors profil par la FRÉQUENCE (#1458) — c'est
        // exactement ce que produit « Forcer le WAV 16 bits » sur un ALAC
        // 24/192 sans plafond de fréquence.
        let hires = media_wav("http://192.168.1.18:8888/stream/alac-192.wav", 192_000, 16);
        let didl = DlnaOutput::didl_metadata_minimale_pour_test(&hires, "1", "audio/wav");
        assert!(
            !didl.contains("DLNA.ORG_PN=LPCM"),
            "192 kHz n'est pas du profil LPCM : {didl}"
        );

        // 24 bits à 48 kHz : hors profil par la PROFONDEUR (#1137).
        let vingt_quatre = media_wav("http://192.168.1.18:8888/stream/wav24.wav", 48_000, 24);
        let didl = DlnaOutput::didl_metadata_minimale_pour_test(&vingt_quatre, "1", "audio/wav");
        assert!(
            !didl.contains("DLNA.ORG_PN=LPCM"),
            "24 bits n'est pas du profil LPCM : {didl}"
        );

        // Le protocolInfo reste là — sans lui, le DMP-A8 accepte l'URI et ne
        // vient jamais chercher le flux.
        assert!(
            didl.contains("DLNA.ORG_OP=01"),
            "protocolInfo perdu : {didl}"
        );
    }

    /// TÉMOIN de la parade : dans le profil, le `PN` reste annoncé. Les
    /// renderers laxistes qui exigent un `PN` pour accepter le flux ne sont pas
    /// touchés — sans ce cas, « ne jamais annoncer LPCM » passerait aussi.
    #[tokio::test]
    async fn le_didl_minimal_garde_le_profil_lpcm_quand_il_est_vrai() {
        for sr in [44_100_u32, 48_000] {
            let media = media_wav("http://192.168.1.18:8888/stream/cd.wav", sr, 16);
            let didl = DlnaOutput::didl_metadata_minimale_pour_test(&media, "1", "audio/wav");
            assert!(
                didl.contains("DLNA.ORG_PN=LPCM"),
                "{sr} Hz / 16 bits reste du LPCM : {didl}"
            );
        }
    }

    /// CONTRE-ÉPREUVE PERMANENTE de l'injection de panne.
    ///
    /// Les deux tests ci-dessus ne prouvent quelque chose que si le calcul du
    /// profil dépend VRAIMENT des valeurs transmises. Ce cas fige l'état
    /// d'avant : sans profondeur ni fréquence, la fonction de profil annonce
    /// `PN=LPCM` — donc retirer le branchement de `didl_metadata_minimale`
    /// FAIT échouer `le_didl_minimal_ne_ment_plus_sur_le_profil_lpcm`, il ne le
    /// rend pas vacuement vert.
    #[test]
    fn sans_les_valeurs_le_profil_lpcm_serait_annonce_quand_meme() {
        let sans_rien = crate::outputs::didl::dlna_flags_for_mime_bd_sr("audio/wav", None, None);
        assert!(
            sans_rien.contains("DLNA.ORG_PN=LPCM"),
            "l'état d'avant doit rester reproductible, sinon la parade ne prouve rien : {sans_rien}"
        );
    }

    /// Le profil se corrige SANS grossir l'enveloppe : le DIDL réduit existe
    /// pour tenir sous un segment TCP, il ne doit gagner aucun attribut
    /// `sampleFrequency` / `bitsPerSample` / `nrAudioChannels` au passage.
    #[tokio::test]
    async fn le_didl_minimal_n_ecrit_aucun_attribut_audio() {
        let media = media_wav("http://192.168.1.18:8888/stream/alac-192.wav", 192_000, 16);
        let didl = DlnaOutput::didl_metadata_minimale_pour_test(&media, "1", "audio/wav");
        for attribut in ["sampleFrequency", "bitsPerSample", "nrAudioChannels"] {
            assert!(
                !didl.contains(attribut),
                "le DIDL réduit ne doit pas porter {attribut} : {didl}"
            );
        }
        let complet = DlnaOutput::didl_metadata_pour_test(&media, "1", "audio/wav");
        assert!(
            complet.contains("sampleFrequency"),
            "le DIDL complet, lui, garde ses attributs : {complet}"
        );
        assert!(
            didl.len() < complet.len(),
            "le réduit doit rester plus court que le complet"
        );
    }

    /// Le zombie Eversolo : Stop ACQUITTÉ mais IGNORÉ tant qu'un Pause n'est
    /// pas passé. La prise de contrôle doit escalader Pause→Stop au lieu de
    /// marteler des Stop sourds pendant 2 s.
    #[tokio::test]
    async fn un_stop_ignore_est_debloque_par_pause() {
        let state = MockState::default();
        *state.transport_state.lock().await = "PLAYING".into();
        *state.stop_exige_pause.lock().await = true;
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        let url = "http://192.168.1.18:8888/stream/zombie.dsf";
        output.play_media(&media_locatelli(url)).await.unwrap();

        assert!(
            state.pause_count.load(Ordering::Relaxed) >= 1,
            "l'escalade Pause devait être tentée face au Stop ignoré"
        );
        assert_eq!(*state.current_uri.lock().await, url);
        handle.abort();
    }

    /// L'échec définitif doit NOMMER l'URI que le renderer tient, et vider le
    /// média quand ce flux est le NÔTRE — il va mourir avec la session, et le
    /// DMP-A8 ressasse une URI morte jusqu'à bloquer toute prise de contrôle.
    #[tokio::test]
    async fn l_echec_nomme_l_uri_tenue_et_vide_notre_media_mort() {
        let state = MockState::default();
        // GetMediaInfo fige une AUTRE URI de NOTRE serveur : la vérification
        // doit échouer, l'erreur la nommer, et le vidage partir.
        *state.media_info_fige.lock().await = true;
        *state.current_uri.lock().await =
            "http://192.168.1.18:8888/stream/vieux-flux-mort.flac".into();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        let err = output
            .play_media(&media_locatelli(
                "http://192.168.1.18:8888/stream/nouveau.dsf",
            ))
            .await
            .unwrap_err();

        assert!(
            err.contains("vieux-flux-mort"),
            "l'erreur doit nommer l'URI tenue : {err}"
        );
        let corps = state.set_uri_corps.lock().await.clone();
        let dernier = corps.last().unwrap();
        assert!(
            dernier.contains("<CurrentURI></CurrentURI>"),
            "le média mort (notre hôte) devait être vidé, dernier envoi : {}",
            &dernier[..dernier.len().min(200)]
        );
        handle.abort();
    }

    /// Un flux ÉTRANGER (autre serveur) n'est PAS vidé : c'est peut-être une
    /// lecture légitime pilotée ailleurs.
    #[tokio::test]
    async fn un_flux_etranger_n_est_pas_vide() {
        let state = MockState::default();
        *state.media_info_fige.lock().await = true;
        *state.current_uri.lock().await =
            "http://192.168.1.42:8888/stream/lecture-d-un-autre-serveur.flac".into();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        let err = output
            .play_media(&media_locatelli(
                "http://192.168.1.18:8888/stream/nouveau.dsf",
            ))
            .await
            .unwrap_err();
        assert!(err.contains("lecture-d-un-autre-serveur"));
        let corps = state.set_uri_corps.lock().await.clone();
        assert!(
            !corps.last().unwrap().contains("<CurrentURI></CurrentURI>"),
            "un flux étranger ne doit jamais être vidé"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn dlna_play_and_status() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        output
            .play_media(&PlayMedia {
                url: "http://example.com/track.wav",
                mime_type: "audio/wav",
                title: Some("Test Track"),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(state.play_count.load(Ordering::Relaxed) >= 1);
        let status = output.get_status().await.unwrap();
        assert_eq!(status.state, TransportState::Playing);
        handle.abort();
    }

    #[tokio::test]
    async fn dlna_pause_resume_stop() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        output
            .play_media(&PlayMedia {
                url: "http://example.com/t.wav",
                mime_type: "audio/wav",
                ..Default::default()
            })
            .await
            .unwrap();

        output.pause().await.unwrap();
        assert_eq!(state.pause_count.load(Ordering::Relaxed), 1);

        output.resume().await.unwrap();

        output.stop().await.unwrap();
        assert!(state.stop_count.load(Ordering::Relaxed) >= 1);
        handle.abort();
    }

    #[tokio::test]
    async fn dlna_seek() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        output.seek(90_000).await.unwrap();
        assert_eq!(state.seek_count.load(Ordering::Relaxed), 1);
        assert_eq!(*state.last_seek_target.lock().await, "0:01:30");
        handle.abort();
    }

    #[tokio::test]
    async fn dlna_set_volume() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        output.set_volume(0.75).await.unwrap();
        assert_eq!(state.volume_count.load(Ordering::Relaxed), 1);
        handle.abort();
    }

    /// #1393 — un `SetVolume` REFUSÉ par le renderer doit remonter en erreur.
    ///
    /// Eric (fil forum, Windows 0.9.61) : « le volume ne fait rien » vers un
    /// renderer Diretta et vers un PC vu comme zone DLNA. Trois couches se
    /// mettaient d'accord sur un changement qui n'avait pas eu lieu — la sortie
    /// lisait la faute UPnP, écrivait un WARN, et rendait `Ok(())` ; le curseur
    /// bougeait, la valeur tenait en base, et le son ne changeait pas.
    ///
    /// Corrigé par #1417. Rien ne l'empêchait de revenir : les tests de
    /// l'orchestrateur couvrent le contrat AU-DESSUS (un backend qui refuse ne
    /// modifie ni mémoire ni base — `un_backend_qui_refuse_ne_modifie_ni_
    /// memoire_ni_base`), et `dlna_set_volume` ne couvre que le succès. Ramener
    /// ce `Ok(())` les laisserait TOUS verts, et Eric n'entendrait toujours
    /// rien.
    ///
    /// Les deux moitiés sont dans le même test, et c'est délibéré : la seconde
    /// est la contre-épreuve permanente de la première. Sans elle, un mock
    /// devenu injoignable, un port fermé, un `set_volume` qui échouerait pour
    /// n'importe quelle autre raison rendraient le premier `unwrap_err()` vert
    /// sans rien prouver.
    #[tokio::test]
    async fn un_volume_refuse_par_le_renderer_remonte_en_erreur() {
        // 1) Le renderer refuse : 602 « Optional Action Not Implemented »,
        //    statut HTTP 500 AVEC corps — ce que rend un RenderingControl
        //    décoratif.
        let refus = MockState::default();
        *refus.volume_refus_upnp.lock().await = Some((602, "Optional Action Not Implemented"));
        let (base, handle) = start_mock(refus.clone()).await;
        let output = make_dlna(&base);

        let erreur = output
            .set_volume(0.75)
            .await
            .expect_err("un renderer qui répond UPnPError ne doit JAMAIS donner Ok(())");

        // La commande a bien été ÉMISE : l'échec vient de la réponse, pas d'un
        // abandon en amont.
        assert_eq!(
            refus.volume_count.load(Ordering::Relaxed),
            1,
            "le SetVolume doit partir avant d'être refusé"
        );
        // Le message est celui que l'auditeur lit, pas une trace de transport :
        // il nomme l'appareil et dit quoi faire.
        assert!(
            erreur.contains("Mock Renderer"),
            "le message doit nommer l'appareil : {erreur}"
        );
        assert!(
            erreur.contains("sur l'appareil"),
            "le message doit dire où régler le volume : {erreur}"
        );
        assert!(
            !erreur.contains("soap send") && !erreur.contains("soap read"),
            "un refus n'est pas une panne de transport : {erreur}"
        );
        handle.abort();

        // 2) CONTRE-ÉPREUVE. Même harnais, même appel, refus retiré : le
        //    résultat doit être `Ok`. C'est ce qui prouve que la moitié
        //    ci-dessus échoue à cause du refus injecté, et de rien d'autre.
        let temoin = MockState::default();
        let (base, handle) = start_mock(temoin.clone()).await;
        let output = make_dlna(&base);

        output
            .set_volume(0.75)
            .await
            .expect("sans refus injecté, le même appel doit réussir");
        assert_eq!(temoin.volume_count.load(Ordering::Relaxed), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn dlna_set_next_gapless() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        output
            .set_next_media(&PlayMedia {
                url: "http://example.com/next.wav",
                mime_type: "audio/wav",
                title: Some("Next"),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(state.set_next_count.load(Ordering::Relaxed), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn dlna_get_position() {
        let state = MockState::default();
        *state.transport_state.lock().await = "PLAYING".into();
        let (base, handle) = start_mock(state).await;
        let output = make_dlna(&base);

        let status = output.get_status().await.unwrap();
        assert_eq!(status.state, TransportState::Playing);
        assert_eq!(status.position_ms, 90_000);
        assert_eq!(status.duration_ms, 300_000);
        handle.abort();
    }

    // ---- #2263 : le poller ne réveille plus le renderer pour rien ----------

    /// `get_status` n'émet plus l'action SOAP `GetMute`.
    ///
    /// Le poller interroge chaque zone DLNA à 1 Hz **pendant toute la
    /// lecture**. Chaque tick valait quatre actions SOAP, dont un `GetMute`
    /// dont personne ne lisait le résultat : l'état « coupé » que voient
    /// l'interface, la base et les évènements est écrit uniquement par
    /// `Orchestrator::set_mute`, jamais par `OutputStatus.muted`. Une action
    /// sur quatre, à chaque seconde, pour rien.
    #[tokio::test]
    async fn dlna_get_status_n_interroge_plus_le_mute() {
        let state = MockState::default();
        *state.transport_state.lock().await = "PLAYING".into();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        for _ in 0..3 {
            let status = output.get_status().await.unwrap();
            // Le mock répond `CurrentMute = 1` : un `muted` vrai signerait un
            // aller-retour SOAP encore vivant.
            assert!(
                !status.muted,
                "get_status a lu le mute du renderer au lieu de l'état local"
            );
        }

        assert_eq!(
            state.get_mute_count.load(Ordering::Relaxed),
            0,
            "get_status émet encore des GetMute — le renderer est réveillé à 1 Hz pour rien"
        );
        handle.abort();
    }

    /// La suppression du `GetMute` ne casse pas la fonction : l'état coupé
    /// reste celui que Tune a posé par `set_mute`, sans un seul aller-retour
    /// de lecture.
    #[tokio::test]
    async fn dlna_set_mute_reste_visible_dans_le_statut() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        assert!(!output.get_status().await.unwrap().muted);

        output.set_mute(true).await.unwrap();
        assert!(
            output.get_status().await.unwrap().muted,
            "l'état coupé posé par Tune n'est plus rapporté"
        );

        output.set_mute(false).await.unwrap();
        assert!(!output.get_status().await.unwrap().muted);

        assert_eq!(state.get_mute_count.load(Ordering::Relaxed), 0);
        handle.abort();
    }

    // ---- #1984 : le renderer raccroche avant d'avoir fini de répondre --------

    /// Lit une requête HTTP entière (en-têtes + corps annoncé par
    /// `Content-Length`) pour ne pas répondre avant que le client ait fini
    /// d'écrire — sinon la réponse part dans un socket que le client est encore
    /// en train de remplir, et le test échouerait pour une raison qui n'est pas
    /// celle qu'il mesure.
    async fn lire_requete_entiere(sock: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;

        let mut brut = Vec::new();
        let mut tampon = [0u8; 4096];
        loop {
            let n = sock.read(&mut tampon).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            brut.extend_from_slice(&tampon[..n]);
            let texte = String::from_utf8_lossy(&brut).to_string();
            let Some(fin_entetes) = texte.find("\r\n\r\n") else {
                continue;
            };
            let attendu: usize = texte
                .lines()
                .find_map(|l| {
                    let (nom, valeur) = l.split_once(':')?;
                    nom.eq_ignore_ascii_case("content-length")
                        .then(|| valeur.trim().parse().ok())?
                })
                .unwrap_or(0);
            if brut.len() >= fin_entetes + 4 + attendu {
                return texte;
            }
        }
        String::from_utf8_lossy(&brut).to_string()
    }

    /// Renderer qui se comporte comme le Marantz ND8006 de Jean Valjean : la
    /// **première** connexion est acceptée, la requête lue, puis le socket
    /// refermé sans le moindre octet de réponse — le symptôme d'une pile HTTP
    /// embarquée qui a raccroché sur une connexion mutualisée. Les connexions
    /// suivantes répondent normalement.
    ///
    /// Renvoie l'URL de base et un compteur de connexions acceptées.
    async fn renderer_qui_raccroche_une_fois()
    -> (String, Arc<AtomicU32>, tokio::task::JoinHandle<()>) {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connexions = Arc::new(AtomicU32::new(0));
        let compteur = connexions.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let rang = compteur.fetch_add(1, Ordering::SeqCst);
                if rang == 0 {
                    // Le socket mort : on lit, on ne répond pas, on raccroche.
                    let _ = lire_requete_entiere(&mut sock).await;
                    drop(sock);
                    continue;
                }
                let _ = lire_requete_entiere(&mut sock).await;
                let corps = concat!(
                    r#"<?xml version="1.0"?>"#,
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">"#,
                    "<s:Body><u:GetProtocolInfoResponse><Sink>",
                    "http-get:*:audio/L16;rate=44100;channels=2:*,",
                    "http-get:*:audio/L24;rate=96000;channels=2:*,",
                    "http-get:*:audio/flac:*",
                    "</Sink></u:GetProtocolInfoResponse></s:Body></s:Envelope>",
                );
                let reponse = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{corps}",
                    corps.len()
                );
                let _ = sock.write_all(reponse.as_bytes()).await;
                let _ = sock.flush().await;
                // Laisser le client lire avant de fermer.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (format!("http://127.0.0.1:{port}"), connexions, handle)
    }

    /// #1984 — « Impossible de lire les capacités du renderer » alors que
    /// l'appareil est allumé, détecté et en train de jouer.
    ///
    /// La sonde `GetProtocolInfo` tombait sur une connexion que le renderer
    /// avait refermée. `reqwest` rend alors une erreur qui n'est ni
    /// `is_connect()` ni `is_timeout()` : la garde de `soap_action` ne la
    /// reconnaissait pas, sortait par le bras « erreur définitive », et
    /// n'essayait pas une seconde fois. `caps` restait vide, et le bouton
    /// « WAV 24 bits » restait grisé alors que le Sink annonce `audio/L24`.
    #[tokio::test]
    async fn les_capacites_se_lisent_malgre_une_connexion_raccrochee() {
        let (base, connexions, handle) = renderer_qui_raccroche_une_fois().await;
        let output = DlnaOutput::new(
            "Marantz ND8006".into(),
            "uuid:56fcb4ae".into(),
            "127.0.0.1".into(),
            format!("{base}/upnp/control/renderer_dvc/AVTransport"),
            format!("{base}/upnp/control/renderer_dvc/RenderingControl"),
            Some(format!(
                "{base}/upnp/control/renderer_dvc/ConnectionManager"
            )),
        );

        let caps = output.probe_capabilities().await;

        assert!(
            caps.probed,
            "la sonde doit aboutir : le renderer répond dès la deuxième connexion"
        );
        assert!(
            caps.lpcm24,
            "audio/L24 est annoncé — le bouton 24 bits doit s'armer"
        );
        assert!(caps.flac, "audio/flac est annoncé");
        assert_eq!(
            connexions.load(Ordering::SeqCst),
            2,
            "il faut exactement une seconde tentative, sur une connexion neuve"
        );
        handle.abort();
    }

    /// LA scène du journal de FabienM (#2581), rejouée de bout en bout.
    ///
    /// Le renderer « Salon » refuse le premier `Play` avec un 701 — il charge
    /// encore l'URI — et un `Stop` lui fait oublier son média. L'ancien barème
    /// envoyait justement un Stop au premier réessai : le média disparaissait,
    /// les quatre `Play` suivants étaient quatre 701 de plus, la zone était
    /// arrêtée après 36 s. Lire `GetTransportInfo` avant de rejouer suffit à
    /// ne PAS envoyer ce Stop : le renderer finit de charger et joue.
    #[tokio::test]
    async fn un_701_de_chargement_ne_declenche_plus_le_stop_qui_tue_le_media() {
        let state = MockState::default();
        *state.salon_701_sans_media.lock().await = true;
        *state.refus_701_restants.lock().await = 1;
        *state.stop_oublie_le_media.lock().await = true;
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        let url = "http://192.168.1.74:8085/stream/salon-chargement.flac";
        output
            .play_media(&PlayMedia {
                url,
                mime_type: "audio/flac",
                title: Some("Never Let Me Down Again"),
                ..Default::default()
            })
            .await
            .expect("le renderer chargeait, il fallait le laisser finir");

        assert_eq!(
            state.play_refus_701.load(Ordering::Relaxed),
            1,
            "un seul 701 : le suivant devait passer"
        );
        assert_eq!(
            state.play_count.load(Ordering::Relaxed),
            1,
            "exactement un Play accepté"
        );
        assert_eq!(
            state.stop_count.load(Ordering::Relaxed),
            1,
            "un seul Stop — celui d'ouverture. Le barème n'en a PAS ajouté : c'est ce Stop-là qui privait le renderer de son média"
        );
        assert_eq!(
            *state.current_uri.lock().await,
            url,
            "le renderer doit finir sur NOTRE flux"
        );
        handle.abort();
    }

    /// L'autre bras du 701 : le renderer ne tient PLUS de média. Aucun délai
    /// n'y peut rien — sans réarmement de l'URI, chaque `Play` est un 701 de
    /// plus, ce que les cinq tentatives du journal ont démontré. Le journal
    /// prouve aussi le remède : dès qu'un `SetAVTransportURI` est rejoué, la
    /// même piste part du premier coup.
    #[tokio::test]
    async fn un_701_sans_media_rearme_l_uri_et_la_piste_part() {
        let state = MockState::default();
        *state.salon_701_sans_media.lock().await = true;
        // Le SetAVTransportURI d'ouverture est accepté… puis perdu.
        *state.oublie_le_media_une_fois.lock().await = true;
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        let url = "http://192.168.1.74:8085/stream/salon-sans-media.flac";
        output
            .play_media(&PlayMedia {
                url,
                mime_type: "audio/flac",
                title: Some("Never Let Me Down Again"),
                ..Default::default()
            })
            .await
            .expect("l'URI réarmée, la piste doit partir");

        assert_eq!(
            state.play_refus_701.load(Ordering::Relaxed),
            1,
            "un seul 701 : le réarmement devait suffire"
        );
        assert!(
            state.set_uri_corps.lock().await.len() >= 2,
            "l'URI devait être REPOSÉE, pas seulement redemandée en Play"
        );
        assert_eq!(
            *state.current_uri.lock().await,
            url,
            "le renderer doit finir sur NOTRE flux"
        );
        handle.abort();
    }

    /// LA SŒUR NUE de #2581 : `play_media` contient DEUX séquences
    /// `SetAVTransportURI` → `Play`. La première a la lecture du 701 (la boucle
    /// à cinq essais, corrigée pour #2581) ; la seconde — la relance d'un Play
    /// « acquitté mais jamais appliqué » — repose l'URI, rejoue… et **jetait la
    /// réponse**. Un 701 sur ce Play ressortait donc sous le message « Le
    /// renderer a acquitté Play mais joue toujours une autre source » : deux
    /// affirmations fausses, sur le message même qui avait poussé un testeur à
    /// réinstaller son système entier (#2396).
    ///
    /// Le mock est déterministe : c'est le NOMBRE de `Play` acceptés qui décide
    /// du refus, jamais une horloge — aucune course, aucun tour sur deux.
    #[tokio::test]
    async fn un_701_sur_le_play_de_la_relance_est_nomme_et_non_travesti() {
        let state = MockState::default();
        // Le renderer acquitte tout mais n'applique jamais l'URI : c'est ce qui
        // déclenche la relance (l'Eversolo de la course des 5 ms).
        *state.media_info_fige.lock().await = true;
        // …et il refuse le Play de cette relance avec le 701 de FabienM.
        *state.relance_701.lock().await = true;
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        let url = "http://192.168.1.74:8085/stream/relance-701.flac";
        let erreur = output
            .play_media(&PlayMedia {
                url,
                mime_type: "audio/flac",
                title: Some("Never Let Me Down Again"),
                ..Default::default()
            })
            .await
            .expect_err("l'URI n'est jamais appliquée : la lecture doit échouer");

        assert_eq!(
            state.play_refus_701.load(Ordering::Relaxed),
            1,
            "le Play de la relance devait bien être refusé une fois"
        );
        assert!(
            erreur.contains("701"),
            "le message doit NOMMER le code que l'appareil a rendu : {erreur}"
        );
        assert!(
            erreur.contains("REFUSÉ"),
            "le message doit dire que l'appareil a refusé, pas acquitté : {erreur}"
        );
        assert!(
            !erreur.contains("joue toujours une autre source"),
            "l'appareil ne joue RIEN d'autre : il a refusé la transition — {erreur}"
        );
        handle.abort();
    }

    // ─────────────────────────────────────────────────────────────────────
    // #2263 — Évènements GENA sur le chemin DLNA, et « silence UPnP »
    // ─────────────────────────────────────────────────────────────────────

    /// Le même renderer, mais abonnable : ses `eventSubURL` sont branchées et
    /// un VRAI récepteur GENA écoute derrière.
    async fn dlna_abonnable(base: &str, silence: bool) -> DlnaOutput {
        let listener = crate::outputs::oh_events::UpnpEventListener::new("127.0.0.1".into())
            .await
            .expect("récepteur GENA");
        let mut urls = std::collections::HashMap::new();
        urls.insert(
            "avtransport".to_string(),
            format!("{base}/AVTransport/event"),
        );
        urls.insert(
            "renderingcontrol".to_string(),
            format!("{base}/RenderingControl/event"),
        );
        make_dlna(base)
            .with_upnp_events(Some(std::sync::Arc::new(listener)), urls)
            .with_upnp_silence(silence)
    }

    /// Attend que l'abonnement soit ÉTABLI, avec une échéance, et échoue fort
    /// s'il ne l'est pas.
    ///
    /// Ce n'est pas une temporisation d'espoir : ce qui suit ne mesure quoi que
    /// ce soit qu'à cette condition, alors la condition est posée en assertion.
    /// Un test d'abonnement qui « mord un tour sur deux » est un test qui
    /// compte AVANT que l'abonnement existe — ici c'est impossible.
    async fn attendre_abonnement(sortie: &DlnaOutput, attendu: bool) {
        let echeance = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let etat = sortie.etat_evenements().await;
            if etat.abonne == attendu {
                return;
            }
            if std::time::Instant::now() >= echeance {
                panic!(
                    "abonnement GENA attendu = {attendu}, obtenu {} après 5 s",
                    etat.abonne
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Attend que le renderer ait livré son état initial COMPLET — celui du
    /// transport (AVTransport) *et* celui du volume (RenderingControl).
    ///
    /// Deux abonnements, deux `NOTIFY` indépendants : `is_live()` est vrai dès
    /// le premier arrivé. Compter les actions à ce moment-là donnerait tantôt
    /// dix, tantôt onze — un `GetVolume` de plus tant que RenderingControl n'a
    /// pas parlé. Ce n'est pas une gigue à masquer, c'est une PRÉCONDITION de
    /// la mesure : on l'attend, et on échoue fort si elle ne vient pas.
    async fn attendre_etat_initial_complet(sortie: &DlnaOutput) {
        let echeance = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let s = sortie.get_status().await.expect("get_status");
            if (s.volume * 100.0).round() as u32 == 42 && s.state == TransportState::Playing {
                return;
            }
            if std::time::Instant::now() >= echeance {
                panic!("état initial poussé incomplet après 5 s : {s:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Combien d'actions SOAP coûtent DIX relevés d'état — soit dix tours du
    /// sondeur, soit dix secondes de lecture à la cadence actuelle.
    async fn actions_pour_dix_releves(sortie: &DlnaOutput, compteur: &Arc<AtomicU32>) -> u32 {
        compteur.store(0, Ordering::Relaxed);
        for _ in 0..10 {
            sortie.get_status().await.expect("get_status");
        }
        compteur.load(Ordering::Relaxed)
    }

    /// Le barème d'AVANT : trois actions par relevé, trente pour dix tours.
    ///
    /// C'est la contre-épreuve des deux tests suivants — sans elle, « on est
    /// passé à dix » ne se compare à rien. Et c'est aussi la preuve du repli :
    /// ce renderer-ci REFUSE l'abonnement, et la lecture n'en sait rien.
    #[tokio::test]
    async fn un_renderer_qui_refuse_l_abonnement_garde_les_trois_actions() {
        let state = MockState::default();
        state.abonnement_refuse.store(true, Ordering::Relaxed);
        let (base, handle) = start_mock(state.clone()).await;
        let sortie = dlna_abonnable(&base, false).await;

        sortie
            .play_media(&media_locatelli(&format!("{base}/flux.flac")))
            .await
            .expect("play");
        attendre_abonnement(&sortie, false).await;

        let actions = actions_pour_dix_releves(&sortie, &state.actions_soap).await;
        assert_eq!(
            actions, 30,
            "abonnement refusé : le relevé doit rester à GetPositionInfo + GetTransportInfo + GetVolume"
        );
        handle.abort();
    }

    /// Défaut, abonnement tenu : UNE action par relevé au lieu de trois.
    ///
    /// L'état, le volume et la coupure arrivent poussés ; seule la position
    /// reste mesurée, parce qu'aucun renderer ne la pousse de façon fiable.
    #[tokio::test]
    async fn les_evenements_ramenent_le_releve_a_une_seule_action() {
        let state = MockState::default();
        state.position_avance.store(true, Ordering::Relaxed);
        let (base, handle) = start_mock(state.clone()).await;
        let sortie = dlna_abonnable(&base, false).await;

        sortie
            .play_media(&media_locatelli(&format!("{base}/flux.flac")))
            .await
            .expect("play");
        attendre_abonnement(&sortie, true).await;
        attendre_etat_initial_complet(&sortie).await;

        let actions = actions_pour_dix_releves(&sortie, &state.actions_soap).await;
        assert_eq!(
            actions, 10,
            "abonnement tenu : seul GetPositionInfo doit rester (mesuré : {actions})"
        );

        // Et ce qui arrive par évènement est JUSTE, pas seulement bon marché.
        let statut = sortie.get_status().await.expect("get_status");
        assert_eq!(statut.state, TransportState::Playing, "état poussé");
        assert_eq!(
            (statut.volume * 100.0).round() as u32,
            42,
            "volume de la voie Master, pas celui de LF (11) ni de RF (99)"
        );
        assert!(!statut.muted, "Mute=0 poussé par RenderingControl");
        let etat = sortie.etat_evenements().await;
        assert!(etat.abonne);
        assert!(
            !etat.position_extrapolee,
            "hors mode silence la position doit rester MESURÉE"
        );
        handle.abort();
    }

    /// « Silence UPnP » : plus AUCUNE action pendant la lecture.
    #[tokio::test]
    async fn le_silence_upnp_ne_coute_plus_aucune_action() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let sortie = dlna_abonnable(&base, true).await;

        sortie
            .play_media(&media_locatelli(&format!("{base}/flux.flac")))
            .await
            .expect("play");
        attendre_abonnement(&sortie, true).await;

        let actions = actions_pour_dix_releves(&sortie, &state.actions_soap).await;
        assert_eq!(
            actions, 0,
            "silence UPnP : le renderer ne doit plus rien recevoir (mesuré : {actions})"
        );

        let statut = sortie.get_status().await.expect("get_status");
        assert_eq!(statut.state, TransportState::Playing);
        assert_eq!(statut.duration_ms, 300_000, "durée poussée par LastChange");
        assert_eq!(
            statut.current_uri.as_deref(),
            Some("http://tune.test/piste.flac")
        );
        // Le prix, et il est DIT : la position n'est plus une mesure.
        assert!(
            sortie.etat_evenements().await.position_extrapolee,
            "le mode silence doit s'annoncer comme extrapolant la position"
        );
        handle.abort();
    }

    /// Le silence n'est jamais un aveuglement : sans abonnement tenu, l'option
    /// armée ne change rien et la sortie sonde comme avant.
    ///
    /// Contre-épreuve du garde : si `get_status` se contentait de regarder
    /// l'interrupteur, ce test rendrait 0 action et un état inventé.
    #[tokio::test]
    async fn le_silence_arme_sans_abonnement_sonde_quand_meme() {
        let state = MockState::default();
        state.abonnement_refuse.store(true, Ordering::Relaxed);
        let (base, handle) = start_mock(state.clone()).await;
        let sortie = dlna_abonnable(&base, true).await;

        sortie
            .play_media(&media_locatelli(&format!("{base}/flux.flac")))
            .await
            .expect("play");
        attendre_abonnement(&sortie, false).await;

        let actions = actions_pour_dix_releves(&sortie, &state.actions_soap).await;
        assert_eq!(
            actions, 30,
            "option armée mais abonnement absent : repli COMPLET sur le sondage"
        );
        assert!(!sortie.etat_evenements().await.position_extrapolee);
        handle.abort();
    }

    /// La position extrapolée avance avec l'horloge, et le déplacement fait
    /// PAR Tune la recale tout de suite.
    #[tokio::test]
    async fn la_position_extrapolee_avance_et_le_seek_la_recale() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let sortie = dlna_abonnable(&base, true).await;

        sortie
            .play_media(&media_locatelli(&format!("{base}/flux.flac")))
            .await
            .expect("play");
        attendre_abonnement(&sortie, true).await;

        let debut = sortie.get_status().await.expect("get_status").position_ms;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let apres = sortie.get_status().await.expect("get_status").position_ms;
        assert!(
            apres >= debut + 300,
            "la position extrapolée doit suivre l'horloge : {debut} → {apres}"
        );

        sortie.seek(120_000).await.expect("seek");
        let recale = sortie.get_status().await.expect("get_status").position_ms;
        assert!(
            (120_000..121_000).contains(&recale),
            "un déplacement passé par Tune recale l'ancre tout de suite : {recale}"
        );
        handle.abort();
    }

    /// Le renderer se dit en lecture par évènement, mais sa position ne bouge
    /// plus : au bout de deux secondes, Tune va trancher à la source.
    ///
    /// C'est la garde qui tient la promesse « pas moins juste qu'avant » : un
    /// appareil qui accepte l'abonnement puis se tait n'a pas le droit de
    /// figer la file indéfiniment.
    #[tokio::test]
    async fn un_etat_pousse_que_la_position_dement_est_arbitre_en_soap() {
        let state = MockState::default();
        state.position_avance.store(true, Ordering::Relaxed);
        let (base, handle) = start_mock(state.clone()).await;
        let sortie = dlna_abonnable(&base, false).await;

        sortie
            .play_media(&media_locatelli(&format!("{base}/flux.flac")))
            .await
            .expect("play");
        attendre_abonnement(&sortie, true).await;
        attendre_etat_initial_complet(&sortie).await;

        // L'appareil s'est ARRÊTÉ, et son évènement dit toujours « PLAYING ».
        state.position_avance.store(false, Ordering::Relaxed);
        *state.transport_state.lock().await = "STOPPED".into();

        // Premier relevé contradictoire : on n'en conclut rien, un tour où la
        // position n'a pas bougé n'est pas une preuve.
        state.actions_soap.store(0, Ordering::Relaxed);
        let tot = sortie.get_status().await.expect("get_status");
        assert_eq!(
            tot.state,
            TransportState::Playing,
            "trop tôt pour douter : l'état poussé tient encore"
        );
        assert_eq!(
            state.actions_soap.load(Ordering::Relaxed),
            1,
            "aucune action de plus tant que la contradiction ne dure pas"
        );

        // Passé le délai, on va lire l'état à la source. L'attente est PLUS
        // LONGUE que le seuil, jamais une course contre lui.
        tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;
        state.actions_soap.store(0, Ordering::Relaxed);
        let tard = sortie.get_status().await.expect("get_status");
        assert_eq!(
            tard.state,
            TransportState::Stopped,
            "la contradiction dure : c'est le transport qui tranche"
        );
        assert_eq!(
            state.actions_soap.load(Ordering::Relaxed),
            2,
            "l'arbitrage coûte UN GetTransportInfo de plus, et seulement là"
        );
        handle.abort();
    }

    /// Un renouvellement REFUSÉ (l'appareil a redémarré, il ne connaît plus le
    /// SID) doit couper l'abonnement, pas laisser servir un état gelé.
    #[tokio::test]
    async fn un_renouvellement_refuse_rend_l_abonnement_mort() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;

        let etat = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::outputs::oh_events::EventState::default(),
        ));
        let listener = crate::outputs::oh_events::UpnpEventListener::new("127.0.0.1".into())
            .await
            .expect("récepteur");
        let id = listener
            .subscribe(&format!("{base}/AVTransport/event"), etat.clone())
            .await
            .expect("abonnement accepté");
        assert!(etat.lock().await.alive, "abonnement tenu après SUBSCRIBE");

        state.renouvellement_refuse.store(true, Ordering::Relaxed);
        listener.renouveler_maintenant().await;
        assert!(
            !etat.lock().await.alive,
            "un 412 au renouvellement doit tuer l'abonnement, pas passer pour un succès"
        );

        listener.unsubscribe(&id).await;
        handle.abort();
    }

    /// L'état initial arrive AVANT la réponse au `SUBSCRIBE`, et il doit être
    /// retenu quand même.
    ///
    /// Le renderer bouchonné émet son `NOTIFY` avant de répondre — l'ordre que
    /// tout appareil rapide peut produire. Enregistrer le gestionnaire de
    /// rappel après coup jetait cet état-là en silence, et `AVTransport`
    /// n'émettant plus rien tant que rien ne change, l'abonnement restait muet
    /// jusqu'à la piste suivante : abonné pour l'ordinateur, inutile en fait.
    #[tokio::test]
    async fn l_etat_initial_arrive_avant_la_reponse_au_subscribe_et_tient() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let sortie = dlna_abonnable(&base, false).await;

        sortie
            .play_media(&media_locatelli(&format!("{base}/flux.flac")))
            .await
            .expect("play");

        // Aucune attente : dès le retour de `play_media`, les deux `NOTIFY`
        // initiaux sont derrière nous — le renderer les a émis avant de
        // répondre. Ce qui suit tombe donc à faux si l'un d'eux a été perdu.
        let etat = sortie.etat_evenements().await;
        assert!(
            etat.abonne,
            "l'état poussé avant la réponse au SUBSCRIBE doit être retenu"
        );
        let statut = sortie.get_status().await.expect("get_status");
        assert_eq!(statut.state, TransportState::Playing);
        assert_eq!((statut.volume * 100.0).round() as u32, 42);
        handle.abort();
    }

    /// Un renderer qui ne pousse PAS sa durée ne doit pas rendre le mode
    /// silence aveugle sur ce point : Tune connaît la durée de la piste avant
    /// même de lui envoyer l'URI.
    ///
    /// Sans ce repli, l'option opt-in dégraderait une TROISIÈME chose sans
    /// l'annoncer — et une durée à zéro désarme la garde de fin de piste du
    /// sondeur.
    #[tokio::test]
    async fn le_silence_tient_la_duree_que_tune_connait_quand_l_appareil_se_tait() {
        let state = MockState::default();
        state.duree_jamais_poussee.store(true, Ordering::Relaxed);
        let (base, handle) = start_mock(state.clone()).await;
        let sortie = dlna_abonnable(&base, true).await;

        // L'URI ANNONCÉE par le renderer dans son évènement est celle-ci : le
        // couple (URI, durée) ne sert que s'il s'apparie.
        let url = "http://tune.test/piste.flac".to_string();
        let mut media = media_locatelli(&url);
        media.duration_ms = Some(487_560);
        sortie.play_media(&media).await.expect("play");
        attendre_abonnement(&sortie, true).await;

        state.actions_soap.store(0, Ordering::Relaxed);
        let statut = sortie.get_status().await.expect("get_status");
        assert_eq!(
            statut.duration_ms, 487_560,
            "durée de la bibliothèque, faute d'une durée poussée"
        );
        assert_eq!(
            state.actions_soap.load(Ordering::Relaxed),
            0,
            "et sans aller la demander à l'appareil : le silence reste le silence"
        );
        handle.abort();
    }

    /// La durée d'une piste ne doit JAMAIS être servie pour une autre : elle
    /// ferait sauter la garde « position au-delà de la fin » en plein morceau.
    #[tokio::test]
    async fn une_duree_annoncee_ne_vaut_que_pour_son_uri() {
        let state = MockState::default();
        state.duree_jamais_poussee.store(true, Ordering::Relaxed);
        let (base, handle) = start_mock(state.clone()).await;
        let sortie = dlna_abonnable(&base, true).await;

        // On joue une piste dont l'URL n'est PAS celle que le renderer
        // annoncera dans son évènement : l'appariement doit échouer.
        let mut media = media_locatelli("http://tune.test/AUTRE-piste.flac");
        media.duration_ms = Some(487_560);
        sortie.play_media(&media).await.expect("play");
        attendre_abonnement(&sortie, true).await;

        let statut = sortie.get_status().await.expect("get_status");
        assert_eq!(
            statut.current_uri.as_deref(),
            Some("http://tune.test/piste.flac"),
            "c'est bien une AUTRE piste que le renderer dit tenir"
        );
        assert_eq!(
            statut.duration_ms, 0,
            "durée inconnue vaut mieux que la durée d'une autre piste"
        );
        handle.abort();
    }
}
