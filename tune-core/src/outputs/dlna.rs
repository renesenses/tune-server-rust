use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use reqwest::Client;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use super::didl::{DidlBuilder, ProtocolStyle};
use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState};
use crate::http::error as http_error;

const AV_TRANSPORT_URN: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL_URN: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const SOAP_MAX_RETRIES: usize = 2;

/// Préfixe des erreurs SOAP dues à un **timeout**, par opposition à un refus de
/// connexion.
///
/// La distinction porte une information que l'orchestrateur exploite : un
/// timeout ne prouve pas que la commande a été rejetée. La requête a très bien
/// pu atteindre un renderer lent et être exécutée — nous n'avons simplement pas
/// eu la réponse à temps. Détruire la session de flux dans ce cas garantit que
/// le renderer, lorsqu'il ira chercher l'URL, tombera sur un 404 et affichera
/// « chanson non trouvée » (Cyrus Stream X2 de JP).
///
/// Toute modification de cette chaîne doit suivre dans
/// `orchestrator::command_may_have_landed`.
pub const SOAP_TIMEOUT_PREFIX: &str = "soap timeout:";

/// Préfixe des erreurs « statut HTTP d'échec SANS corps SOAP ».
///
/// Un défaut SOAP légitime voyage DANS un 500 avec un corps `UPnPError` — le
/// spec UPnP l'impose — et nos appelants le lisent (la reprise 714 en dépend).
/// Mais un 500 au corps VIDE n'est pas un défaut SOAP : c'est un serveur qui
/// n'a pas su LIRE la requête. Platinum/1.0.5.13 (Eversolo DMP-A8) répond
/// `500 Bad Request: Error Parsing XML Body`, corps vide, quand le corps
/// dépasse un segment TCP : il parse sa première lecture et jette le reste —
/// les octets suivants restent dans la Send-Q (constaté sur .18, 25/08).
/// L'ancien code ne regardait que le corps : ce 500 vide passait pour un
/// acquittement, et la zone « jouait » une piste que le renderer n'avait
/// jamais reçue.
pub(crate) const SOAP_HTTP_SANS_CORPS_PREFIX: &str = "soap http sans corps:";
/// Timeout for the fire-and-forget Stop sent before SetAVTransportURI.
/// Kept short (2s) because we don't need the response — SetAVTransportURI
/// implicitly stops the current track on compliant renderers.
const STOP_BEFORE_PLAY_TIMEOUT_MS: u64 = 2000;

pub struct DlnaOutput {
    name: String,
    device_id: String,
    host: String,
    av_transport_url: String,
    rendering_control_url: String,
    client: Client,
    /// Short-timeout client used for fire-and-forget Stop before play.
    stop_client: Client,
    /// Pause between SetAVTransportURI and Play, in ms. Interior-mutable so a
    /// per-zone override (Settings → renderer panel) can be applied live to the
    /// already-registered output without rebuilding it. 0 = no delay.
    play_delay_ms: AtomicU64,
    /// Alternates between false ("1") and true ("2") so that consecutive
    /// DIDL items sent via SetAVTransportURI / SetNextAVTransportURI use
    /// different item IDs.  Renderers like Marantz ND8006 cache DIDL
    /// metadata keyed by item id — using the same id for both current and
    /// next track causes the renderer to display stale metadata (wrong
    /// duration, format) on every other track.
    next_item_id_flip: AtomicBool,
    /// Niveau de DIDL appris pour CET appareil (0 = complet, 1 = minimal,
    /// 2 = vide). La pile Platinum de l'Eversolo ne lit qu'un segment TCP de
    /// requête : le DIDL complet déborde et finit en « 500 sans corps », le
    /// minimal passe — mais l'échelle re-payait l'aller-retour raté À CHAQUE
    /// piste (un warn + ~200 ms par SetURI/SetNext, constaté sur DMP-A8,
    /// #2394). Une fois le niveau qui passe constaté, on démarre là. Jamais
    /// remonté en cours de vie du process : la pile du renderer ne change pas ;
    /// un redémarrage de Tune repart du complet.
    didl_niveau_appris: AtomicU8,
    /// Dernier état « coupé » que **Tune** a posé sur cet appareil, via
    /// `set_mute`.
    ///
    /// `get_status` le rend tel quel au lieu d'aller le redemander au
    /// renderer : le poller interroge chaque zone DLNA à 1 Hz pendant toute la
    /// lecture, et l'action SOAP `GetMute` y valait une requête sur quatre —
    /// pour une valeur que **personne ne lisait**. L'état coupé qu'affichent
    /// l'interface, la base (`zones.muted`) et les évènements est écrit
    /// uniquement par `Orchestrator::set_mute` ; `OutputStatus.muted` ne le
    /// nourrit nulle part (#2263).
    ///
    /// Même convention que les autres sorties sans évènements — AirPlay,
    /// SlimProto, Squeezebox tiennent déjà leur mute en local. Conséquence
    /// assumée : une coupure faite **sur l'appareil lui-même** (télécommande
    /// physique) n'est plus reflétée dans `GET /api/devices/{id}/status`,
    /// seule route qui expose ce champ.
    muted: AtomicBool,
    /// Micromega M-One uses a proprietary TCP protocol on port 7000 for volume.
    micromega_ip: Option<String>,
    /// URL for the ConnectionManager service (used to query GetProtocolInfo).
    /// Falls back to av_transport_url if not available.
    connection_manager_url: Option<String>,
}

impl DlnaOutput {
    pub fn new(
        name: String,
        device_id: String,
        host: String,
        av_transport_url: String,
        rendering_control_url: String,
        connection_manager_url: Option<String>,
    ) -> Self {
        let micromega_ip = if name.to_lowercase().contains("micromega") {
            let ip = host
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .split(':')
                .next()
                .unwrap_or("")
                .to_string();
            if !ip.is_empty() {
                info!(device = %name, ip = %ip, "micromega_device_detected — proprietary volume on port 7000");
                Some(ip)
            } else {
                None
            }
        } else {
            None
        };
        Self {
            name,
            device_id,
            host,
            av_transport_url,
            rendering_control_url,
            client: crate::http::client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            stop_client: crate::http::client::builder()
                .timeout(std::time::Duration::from_millis(
                    STOP_BEFORE_PLAY_TIMEOUT_MS,
                ))
                .build()
                .unwrap_or_default(),
            play_delay_ms: AtomicU64::new(0),
            next_item_id_flip: AtomicBool::new(false),
            didl_niveau_appris: AtomicU8::new(0),
            muted: AtomicBool::new(false),
            micromega_ip,
            connection_manager_url,
        }
    }

    pub fn with_play_delay(self, delay_ms: u64) -> Self {
        self.play_delay_ms.store(delay_ms, Ordering::Relaxed);
        self
    }

    /// Update the SetAVTransportURI→Play delay on an already-registered output
    /// (via &self downcast in the zone PATCH handler). Takes effect on the next
    /// play; no rebuild needed.
    pub fn set_play_delay(&self, delay_ms: u64) {
        self.play_delay_ms.store(delay_ms, Ordering::Relaxed);
    }

    /// Current SetAVTransportURI→Play delay in ms.
    pub fn play_delay_ms(&self) -> u64 {
        self.play_delay_ms.load(Ordering::Relaxed)
    }

    /// Send a SOAP action without retries and with the short-timeout client.
    /// Used for the fire-and-forget Stop before play — we don't need to wait
    /// for the response because SetAVTransportURI implicitly replaces the
    /// current track.  Returns immediately after the single attempt.
    async fn soap_action_fast(
        &self,
        url: &str,
        service: &str,
        action: &str,
        body: &str,
    ) -> Result<(), String> {
        let soap = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action} xmlns:u="{service}">
      {body}
    </u:{action}>
  </s:Body>
</s:Envelope>"#
        );
        let soap_action = format!("{service}#{action}");

        match self
            .stop_client
            .post(url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPAction", format!("\"{soap_action}\""))
            .body(soap)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("soap_fast: {}", http_error::chain(&e))),
        }
    }

    async fn soap_action(
        &self,
        url: &str,
        service: &str,
        action: &str,
        body: &str,
    ) -> Result<String, String> {
        let soap = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action} xmlns:u="{service}">
      {body}
    </u:{action}>
  </s:Body>
</s:Envelope>"#
        );

        let soap_action = format!("{service}#{action}");
        let mut last_err = String::new();
        let mut last_was_timeout = false;

        for attempt in 0..=SOAP_MAX_RETRIES {
            if attempt > 0 {
                let delay = 200 * (1 << (attempt - 1));
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                debug!(device = %self.name, action, attempt, "soap_retry");
            }

            match self
                .client
                .post(url)
                .header("Content-Type", "text/xml; charset=utf-8")
                .header("SOAPAction", format!("\"{soap_action}\""))
                .body(soap.clone())
                .send()
                .await
            {
                Ok(resp) => {
                    let statut = resp.status();
                    match resp.text().await {
                        Ok(text) => {
                            // Statut d'échec + corps vide : le renderer n'a pas
                            // lu la requête (voir SOAP_HTTP_SANS_CORPS_PREFIX).
                            // Un échec AVEC corps reste rendu tel quel — c'est
                            // un défaut SOAP que l'appelant sait interpréter.
                            if !statut.is_success() && text.trim().is_empty() {
                                return Err(format!(
                                    "{SOAP_HTTP_SANS_CORPS_PREFIX} {statut} sur {action}"
                                ));
                            }
                            return Ok(text);
                        }
                        Err(e) => last_err = format!("soap read: {}", http_error::chain(&e)),
                    }
                }
                // `is_connection_closed_early` : le renderer a raccroché avant
                // d'avoir fini sa réponse. Sans ce troisième prédicat, la panne
                // ressortait par le bras « erreur définitive » ci-dessous et
                // n'était JAMAIS réessayée — le Marantz ND8006 de Jean Valjean
                // échouait dès la première tentative (#1984), y compris sur le
                // GetProtocolInfo qui arme le bouton « 24 bits ».
                //
                // La deuxième tentative repart sur une connexion neuve : celle
                // qui vient d'échouer a été évacuée du pool par l'échec même.
                // C'est ce qui rend le simple réessai suffisant, sans avoir à
                // désactiver la mutualisation vers tous les renderers.
                Err(e)
                    if e.is_connect()
                        || e.is_timeout()
                        || http_error::is_connection_closed_early(&e) =>
                {
                    last_was_timeout = e.is_timeout();
                    last_err = format!("soap send: {}", http_error::chain(&e));
                }
                Err(e) => return Err(format!("soap send: {}", http_error::chain(&e))),
            }
        }

        http_error::hint_if_local_network_denied(&last_err);
        warn!(device = %self.name, action, error = %last_err, "soap_all_retries_failed");
        // Voir SOAP_TIMEOUT_PREFIX : un timeout laisse la commande peut-être
        // exécutée, un refus de connexion non.
        if last_was_timeout {
            Err(format!("{SOAP_TIMEOUT_PREFIX} {last_err}"))
        } else {
            Err(last_err)
        }
    }

    async fn av_action(&self, action: &str, body: &str) -> Result<String, String> {
        self.soap_action(&self.av_transport_url, AV_TRANSPORT_URN, action, body)
            .await
    }

    async fn rc_action(&self, action: &str, body: &str) -> Result<String, String> {
        self.soap_action(
            &self.rendering_control_url,
            RENDERING_CONTROL_URN,
            action,
            body,
        )
        .await
    }

    /// Réenvoie `SetAVTransportURI` au niveau de DIDL qui a fini par passer
    /// pour cet appareil : rejouer le complet referait échouer la lecture chez
    /// Platinum. Deux appelants — le réarmement d'un 701 sans média (#2581) et
    /// la relance d'un Play acquitté mais jamais appliqué.
    async fn reposer_uri(
        &self,
        media: &PlayMedia<'_>,
        item_id: &'static str,
        mime: &str,
        niveau_didl: u8,
    ) -> Result<String, String> {
        let metadata = match niveau_didl {
            0 => Self::didl_metadata_mime(media, item_id, mime),
            1 => Self::didl_metadata_minimale(media, item_id, mime),
            _ => String::new(),
        };
        self.av_action(
            "SetAVTransportURI",
            &format!(
                "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{metadata}</CurrentURIMetaData>",
                media.url
            ),
        )
        .await
    }

    fn didl_metadata(media: &PlayMedia<'_>, item_id: &str) -> String {
        Self::didl_metadata_mime(media, item_id, media.mime_type)
    }

    /// Like [`Self::didl_metadata`] but announces an explicit `mime` instead of
    /// `media.mime_type`. Used to align the announced MIME with the renderer's
    /// GetProtocolInfo Sink spelling (Beoplay A9 / Sink audio/x-flac, forum
    /// 714) and for the 714 PCM fallback.
    fn didl_metadata_mime(media: &PlayMedia<'_>, item_id: &str, mime: &str) -> String {
        let is_dsd = mime.contains("dsd") || mime.contains("dsf");
        DidlBuilder::new(media.title.unwrap_or("Unknown"), media.url, mime)
            .protocol_style(ProtocolStyle::Dlna)
            .live_stream(media.live_stream)
            .byte_seekable(media.byte_seekable)
            .dlna_art_profile(true)
            .include_upnp_artist(true)
            .item_id(item_id)
            .artist_opt(media.artist)
            .album_opt(media.album)
            .album_art_opt(media.cover_url)
            .duration_ms_opt(media.duration_ms)
            .file_size_opt(media.file_size)
            .sample_rate_opt(if is_dsd { None } else { media.sample_rate })
            .bit_depth_opt(if is_dsd { None } else { media.bit_depth })
            .channels_opt(if is_dsd { None } else { media.channels })
            .build_escaped()
    }

    /// DIDL réduit au strict jouable : titre, ressource, protocolInfo, durée.
    ///
    /// Ni artiste, ni album, ni pochette : la pile Platinum/1.0.5.13 de
    /// l'Eversolo ne lit qu'un segment TCP de requête — un DIDL complet
    /// (~1,9 Ko d'enveloppe) déborde et finit en `500 Error Parsing XML Body`,
    /// quand les mêmes octets passent en une seule trame. Ce DIDL-ci tient
    /// l'enveloppe sous un segment. Le protocolInfo reste : sans lui, le
    /// DMP-A8 accepte l'URI d'un `.dsf` mais ne vient jamais le chercher.
    fn didl_metadata_minimale(media: &PlayMedia<'_>, item_id: &str, mime: &str) -> String {
        DidlBuilder::new(media.title.unwrap_or("Unknown"), media.url, mime)
            .protocol_style(ProtocolStyle::Dlna)
            .live_stream(media.live_stream)
            .byte_seekable(media.byte_seekable)
            .item_id(item_id)
            .duration_ms_opt(media.duration_ms)
            .build_escaped()
    }

    /// Accès de test aux deux niveaux de DIDL (budget de taille mesuré dans
    /// `dlna_test.rs` — l'échelle ne vaut que si le minimal tient un segment).
    #[cfg(test)]
    pub(crate) fn didl_metadata_pour_test(
        media: &PlayMedia<'_>,
        item_id: &str,
        mime: &str,
    ) -> String {
        Self::didl_metadata_mime(media, item_id, mime)
    }

    #[cfg(test)]
    pub(crate) fn didl_metadata_minimale_pour_test(
        media: &PlayMedia<'_>,
        item_id: &str,
        mime: &str,
    ) -> String {
        Self::didl_metadata_minimale(media, item_id, mime)
    }

    /// Return the next item id ("1" or "2") and flip the toggle.
    /// Alternating ids prevents renderers (Marantz ND8006 etc.) from
    /// displaying stale cached metadata when the same id is reused for
    /// consecutive tracks.
    fn next_item_id(&self) -> &'static str {
        let prev = self.next_item_id_flip.fetch_xor(true, Ordering::Relaxed);
        if prev { "2" } else { "1" }
    }

    fn parse_time(time_str: &str) -> u64 {
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() == 3 {
            let h: u64 = parts[0].parse().unwrap_or(0);
            let m: u64 = parts[1].parse().unwrap_or(0);
            let s_parts: Vec<&str> = parts[2].split('.').collect();
            let s: u64 = s_parts[0].parse().unwrap_or(0);
            let frac_ms: u64 = if s_parts.len() > 1 {
                let frac = s_parts[1];
                let val: u64 = frac.parse().unwrap_or(0);
                match frac.len() {
                    1 => val * 100,
                    2 => val * 10,
                    3 => val,
                    _ => val / 10u64.pow(frac.len() as u32 - 3),
                }
            } else {
                0
            };
            (h * 3600 + m * 60 + s) * 1000 + frac_ms
        } else {
            0
        }
    }

    fn format_time(ms: u64) -> String {
        let total_secs = ms / 1000;
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        let s = total_secs % 60;
        format!("{h}:{m:02}:{s:02}")
    }
}

#[async_trait::async_trait]
impl OutputTarget for DlnaOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "dlna"
    }

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, true)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        // Fire-and-forget Stop with a tight deadline: give the renderer up to
        // 500ms to acknowledge Stop, then proceed regardless.  Most renderers
        // accept SetAVTransportURI while playing (implicit stop), but we still
        // send Stop for renderers like DMP-A8 that need it.  The short deadline
        // ensures we don't block 2-10s waiting for a slow SOAP response.
        let stop_fut = self.soap_action_fast(
            &self.av_transport_url,
            AV_TRANSPORT_URN,
            "Stop",
            "<InstanceID>0</InstanceID>",
        );
        tokio::select! {
            result = stop_fut => {
                match result {
                    Ok(()) => debug!(device = %self.name, "dlna_play_pre_stop_ok"),
                    Err(e) => debug!(device = %self.name, error = %e, "dlna_play_pre_stop_ignored"),
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                debug!(device = %self.name, "dlna_play_pre_stop_timeout_proceeding");
            }
        }

        // Un Stop ACQUITTÉ n'est pas un Stop APPLIQUÉ. L'Eversolo répond OK
        // puis met ~1-2 s à s'arrêter ; un SetAVTransportURI envoyé 5 ms plus
        // tard est acquitté… et ignoré — il continue son flux précédent (la
        // course des 5 ms, .42, 24/08 ; la même séquence espacée de 2 s est
        // acceptée). On attend l'arrêt réel, borné à ~2 s, et on continue quoi
        // qu'il arrive : c'est une politesse, jamais une barrière. Le renderer
        // déjà arrêté — le cas nominal — coûte UN GetTransportInfo.
        for attente in 0..8u32 {
            match self
                .av_action("GetTransportInfo", "<InstanceID>0</InstanceID>")
                .await
            {
                Ok(resp) if arret_effectif(&resp) => {
                    if attente > 0 {
                        debug!(device = %self.name, polls = attente + 1, "dlna_pre_stop_arret_confirme");
                    }
                    break;
                }
                // Un renderer sans GetTransportInfo ne doit rien bloquer.
                Err(_) => break,
                Ok(_) if attente == 7 => {
                    warn!(device = %self.name, "dlna_pre_stop_jamais_applique_on_continue");
                }
                Ok(_) => {
                    // À mi-parcours, escalader : l'Eversolo coincé en
                    // TRANSITIONING (flux mort qu'il ressasse) ACQUITTE les
                    // Stop sans les exécuter — seul Pause→Stop le libère
                    // (constaté par SOAP direct sur le DMP-A8, 25/08 : Stop →
                    // toujours PLAYING ; Pause → PAUSED_PLAYBACK ; Stop →
                    // STOPPED).
                    if attente == 3 {
                        debug!(device = %self.name, "dlna_pre_stop_escalade_pause_puis_stop");
                        let _ = self.av_action("Pause", "<InstanceID>0</InstanceID>").await;
                        let _ = self.av_action("Stop", "<InstanceID>0</InstanceID>").await;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
        }

        let item_id = self.next_item_id();

        // First attempt: announce `media.mime_type` UNCHANGED — exactly the
        // previous behaviour. The Sink is NOT probed here: a healthy renderer
        // (Sonos & co) accepts this MIME, so the happy path does ZERO extra
        // GetProtocolInfo round-trip (no latency added in nominal playback).
        // The Sink is probed ONLY when a 714 actually occurs (see below).
        let mut attempt_mime = media.mime_type.to_string();
        // Sink probed lazily on the first 714 and reused across the ≤2 retries.
        let mut sink: Vec<String> = Vec::new();
        let mut tried_exact = false;
        let mut tried_fallback = false;
        // Échelle de métadonnées : DIDL complet → minimal → vide. On ne
        // descend que sur un échec de LECTURE de la requête (500 sans corps,
        // Platinum) — jamais sur un défaut SOAP, qui a sa propre reprise 714.
        // On démarre au niveau APPRIS pour cet appareil : re-payer l'échec du
        // complet à chaque piste coûtait un aller-retour et un warn par SetURI
        // (DMP-A8, #2394) pour finir au même DIDL minimal de toute façon.
        let mut niveau_didl: u8 = self.didl_niveau_appris.load(Ordering::Relaxed);
        let debut_set_uri = std::time::Instant::now();
        loop {
            let metadata = match niveau_didl {
                0 => Self::didl_metadata_mime(media, item_id, &attempt_mime),
                1 => Self::didl_metadata_minimale(media, item_id, &attempt_mime),
                _ => String::new(),
            };
            let set_uri_resp = match self.av_action("SetAVTransportURI", &format!(
                "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{metadata}</CurrentURIMetaData>",
                media.url
            )).await {
                Ok(r) => r,
                Err(e) if e.starts_with(SOAP_HTTP_SANS_CORPS_PREFIX) && niveau_didl < 2 => {
                    niveau_didl += 1;
                    warn!(
                        device = %self.name,
                        ctrl = %self.av_transport_url,
                        niveau = niveau_didl,
                        error = %e,
                        "dlna_set_uri_corps_illisible_didl_reduit"
                    );
                    continue;
                }
                Err(e) => return Err(e),
            };

            if !(set_uri_resp.contains("UPnPError") || set_uri_resp.contains("<errorCode>")) {
                self.didl_niveau_appris
                    .store(niveau_didl, Ordering::Relaxed);
                // Le SUCCÈS se journalise, pas seulement l'échec. Sans cette
                // ligne, un SetAVTransportURI lent laisse un trou muet et
                // l'incident n'est plus instruisable : dans le journal de
                // FabienM (#2581), 23,5 s s'écoulent entre « flux prêt » et le
                // premier refus de Play sans une seule trace de la sortie.
                info!(
                    device = %self.name,
                    url = media.url,
                    niveau_didl,
                    advertised_mime = %attempt_mime,
                    duree_ms = debut_set_uri.elapsed().as_millis() as u64,
                    "dlna_set_uri_ok"
                );
                break;
            }

            // Error 714 ("Illegal MIME-type"): the renderer parsed the DIDL but
            // its ConnectionManager Sink does not list the announced MIME.
            // Beoplay A9 / Sink audio/x-flac, forum 714: strict renderers (B&O,
            // Lyngdorf) reject `audio/flac` when their Sink only lists
            // `audio/x-flac`, even though they decode the stream. ONLY here (on
            // a real 714) do we pay a single GetProtocolInfo probe, then retry
            // up to twice: (a) with the exact Sink spelling, (b) with a PCM
            // profile the Sink lists. Strict renderers gate on the announced
            // MIME but decode by content, so a Sink-accepted label lets the
            // actual FLAC bytes through.
            let is_714 = set_uri_resp.contains(">714<")
                || set_uri_resp.to_lowercase().contains("illegal mime");

            if is_714 && (!tried_exact || !tried_fallback) {
                // Probe the Sink once, on the first 714 only.
                if sink.is_empty() {
                    sink = self.get_protocol_info().await.unwrap_or_default();
                }

                // Retry (a): announce the exact spelling the Sink lists
                // (e.g. audio/x-flac) if it differs from what we just sent.
                if !tried_exact {
                    tried_exact = true;
                    let exact = advertised_mime_for_sink(media.mime_type, &sink);
                    if !exact.eq_ignore_ascii_case(&attempt_mime) {
                        warn!(
                            device = %self.name,
                            advertised_mime = %attempt_mime,
                            exact_mime = %exact,
                            sink = ?sink,
                            "dlna_set_uri_714_exact_spelling_retry"
                        );
                        attempt_mime = exact;
                        continue;
                    }
                }

                // Retry (b): fall back to a PCM MIME the Sink lists (audio/wav
                // then audio/L16) if we have not tried it yet.
                if !tried_fallback {
                    tried_fallback = true;
                    if let Some(fb) = fallback_mime_from_sink(&sink) {
                        if !fb.eq_ignore_ascii_case(&attempt_mime) {
                            warn!(
                                device = %self.name,
                                advertised_mime = %attempt_mime,
                                fallback_mime = %fb,
                                sink = ?sink,
                                "dlna_set_uri_714_pcm_fallback_retry"
                            );
                            attempt_mime = fb;
                            continue;
                        }
                    }
                }
            }

            if is_714 {
                // Surface the exact MIME + Sink so the mismatch is diagnosable
                // from a single log line (Mickaël, #1146: TIDAL → Beoplay 714).
                warn!(
                    device = %self.name,
                    advertised_mime = %attempt_mime,
                    live_stream = media.live_stream,
                    sink_entries = sink.len(),
                    sink = ?sink,
                    response = %set_uri_resp,
                    "dlna_set_uri_illegal_mime_714"
                );
                return Err(format!(
                    "SetAVTransportURI rejected 714 Illegal MIME-type: renderer Sink does not accept advertised MIME '{attempt_mime}' (sink has {} entries); rejected: {set_uri_resp}",
                    sink.len()
                ));
            }
            warn!(device = %self.name, response = %set_uri_resp, "dlna_set_uri_error");
            return Err(format!("SetAVTransportURI rejected: {set_uri_resp}"));
        }

        let play_delay = self.play_delay_ms.load(Ordering::Relaxed);
        if play_delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(play_delay)).await;
        }

        // Retry Play with backoff — some renderers (Revox S100, stagefright-based)
        // reject Play immediately after SetAVTransportURI while still loading the URI.
        // On first 501, send another Stop then retry — the Revox needs an explicit
        // Stop after SetAVTransportURI when it was already playing.
        //
        // Le 701 « Transition not available » ne dit pas « je suis en panne » :
        // il dit « pas CETTE transition, MAINTENANT » — et le renderer sait
        // dans quel état il est. Le barème aveugle répondait à côté (#2581,
        // journal FabienM du 27/08) : cinq refus 701 en 11,4 s, zone arrêtée
        // après 36 s… et la MÊME piste vers le MÊME appareil part du premier
        // coup 1,8 s plus tard, dès qu'un SetAVTransportURI est rejoué. On lit
        // donc le transport avant de réessayer : il charge encore → le laisser
        // finir, sans le Stop du barème qui le ferait retomber ; il ne tient
        // plus de média → lui réarmer l'URI, sans quoi chaque Play suivant est
        // un 701 de plus. Le renderer qui ne dit rien d'exploitable garde le
        // barème historique, au mot près.
        let mut last_err = String::new();
        let mut reprise = RepriseApresRefus::StopPuisPlay;
        for attempt in 0..5u32 {
            if attempt > 0 {
                let delay = match attempt {
                    1 => 500,
                    2 => 1500,
                    3 => 3000,
                    _ => 4000,
                };
                match reprise {
                    RepriseApresRefus::StopPuisPlay if attempt == 1 => {
                        debug!(device = %self.name, "dlna_play_retry_sending_stop");
                        let _ = self.av_action("Stop", "<InstanceID>0</InstanceID>").await;
                        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                        let _ = self
                            .av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
                            .await;
                    }
                    RepriseApresRefus::ReArmerUri => {
                        info!(device = %self.name, attempt, "dlna_play_701_rearmement_uri");
                        let _ = self
                            .reposer_uri(media, item_id, &attempt_mime, niveau_didl)
                            .await;
                    }
                    // Chargement en cours, ou barème historique hors du premier
                    // essai : ne rien envoyer de plus.
                    _ => {}
                }
                info!(device = %self.name, attempt, delay_ms = delay, "dlna_play_retry");
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            let play_resp = self
                .av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
                .await?;

            if !play_resp.contains("UPnPError") && !play_resp.contains("<errorCode>") {
                if attempt > 0 {
                    info!(device = %self.name, attempt, "dlna_play_retry_succeeded");
                }
                last_err.clear();
                break;
            }
            warn!(device = %self.name, attempt, response = %play_resp, "dlna_play_error");
            last_err = format!("Play rejected: {play_resp}");
            // Un 701 nomme un état : on va le LIRE plutôt que le deviner. Un
            // renderer sans GetTransportInfo ne change rien au barème.
            reprise = if est_701(&play_resp) {
                let etat = self
                    .av_action("GetTransportInfo", "<InstanceID>0</InstanceID>")
                    .await
                    .ok()
                    .and_then(|xml| extract_tag(&xml, "CurrentTransportState"));
                let choix = reprise_apres_refus_play(&play_resp, etat.as_deref());
                info!(
                    device = %self.name,
                    attempt,
                    etat = etat.as_deref().unwrap_or("-"),
                    reprise = ?choix,
                    "dlna_play_701_transport_lu"
                );
                choix
            } else {
                RepriseApresRefus::StopPuisPlay
            };
        }
        if !last_err.is_empty() {
            return Err(last_err);
        }

        // Le Play est acquitté — est-il APPLIQUÉ ? Dans la course des 5 ms,
        // l'Eversolo répond OK à toute la séquence et garde l'URI précédente :
        // la zone affichait « playing » sur la position de l'ancienne piste,
        // et l'utilisateur relançait à la main. On relit l'URI courante ; en
        // cas d'écart, UNE relance complète, puis un échec VISIBLE plutôt
        // qu'un état menteur. Une URI qu'on ne sait pas interpréter (renderer
        // qui réécrit) ne conclut rien — zéro régression sur ces appareils.
        let mut applique = UriVerdict::Indeterminee;
        let mut uri_tenue: Option<String> = None;
        'verif: for relance in 0..2u32 {
            for essai in 0..3u32 {
                let resp = self
                    .av_action("GetMediaInfo", "<InstanceID>0</InstanceID>")
                    .await;
                let uri = match &resp {
                    Ok(xml) => extract_tag(xml, "CurrentURI"),
                    // Un renderer sans GetMediaInfo ne doit rien bloquer.
                    Err(_) => break 'verif,
                };
                uri_tenue = uri.clone();
                applique = verdict_uri_appliquee(uri.as_deref(), media.url);
                match applique {
                    UriVerdict::Appliquee | UriVerdict::Indeterminee => break 'verif,
                    UriVerdict::PasAppliquee if essai < 2 => {
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    }
                    UriVerdict::PasAppliquee => {}
                }
            }
            if relance == 0 {
                warn!(device = %self.name, url = media.url, ctrl = %self.av_transport_url, "dlna_play_acquitte_mais_pas_applique_relance");
                let _ = self
                    .reposer_uri(media, item_id, &attempt_mime, niveau_didl)
                    .await;
                let _ = self
                    .av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
                    .await;
            }
        }
        if applique == UriVerdict::PasAppliquee {
            warn!(
                device = %self.name,
                url = media.url,
                ctrl = %self.av_transport_url,
                tenue = uri_tenue.as_deref().unwrap_or("-"),
                "dlna_play_jamais_applique"
            );
            // Si le renderer tient un flux de NOTRE serveur, ce flux va mourir
            // avec la session que l'appelant s'apprête à démonter — et le
            // DMP-A8 ressasse une URI morte en zombie (PLAYING/TRANSITIONING,
            // sourd aux Stop) jusqu'à bloquer toute prise de contrôle
            // ultérieure. On vide son média, au mieux. Un flux ÉTRANGER, lui,
            // est peut-être une lecture légitime d'un autre serveur : on n'y
            // touche pas.
            let notre_origine: String = media
                .url
                .splitn(4, '/')
                .take(3)
                .collect::<Vec<_>>()
                .join("/");
            if uri_tenue
                .as_deref()
                .is_some_and(|u| !notre_origine.is_empty() && u.starts_with(&notre_origine))
            {
                debug!(device = %self.name, "dlna_echec_vidage_du_media_mort");
                let _ = self
                    .av_action(
                        "SetAVTransportURI",
                        "<InstanceID>0</InstanceID><CurrentURI></CurrentURI><CurrentURIMetaData></CurrentURIMetaData>",
                    )
                    .await;
            }
            let detail = match uri_tenue.as_deref() {
                Some(u) if !u.trim().is_empty() => format!("il tient encore : {u}"),
                _ => "URI non appliquée après relance".to_string(),
            };
            return Err(format!(
                "Le renderer a acquitté Play mais joue toujours une autre source ({detail})"
            ));
        }

        info!(device = %self.name, url = media.url, ctrl = %self.av_transport_url, delay_ms = play_delay, "dlna_play");
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.av_action("Pause", "<InstanceID>0</InstanceID>")
            .await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
            .await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.av_action("Stop", "<InstanceID>0</InstanceID>").await?;
        info!(device = %self.name, "dlna_stop");
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let target = Self::format_time(position_ms);
        self.av_action(
            "Seek",
            &format!("<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{target}</Target>"),
        )
        .await?;
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        if let Some(ip) = &self.micromega_ip {
            let target_vol = volume * 100.0;
            let msg = format!("GET /volume HTTP/1.0\r\n\r\nvolume={target_vol:.1}\r\n");
            let addr = format!("{ip}:7000");
            match tokio::time::timeout(std::time::Duration::from_secs(3), TcpStream::connect(&addr))
                .await
            {
                Ok(Ok(mut stream)) => {
                    let _ = stream.write_all(msg.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    debug!(device = %self.name, volume = target_vol, "micromega_volume_set");
                }
                Ok(Err(e)) => {
                    warn!(device = %self.name, volume = target_vol, error = %e, "micromega_volume_error");
                }
                Err(_) => {
                    warn!(device = %self.name, volume = target_vol, "micromega_volume_timeout");
                }
            }
            return Ok(());
        }
        let level = (volume * 100.0).round() as u32;
        let resp = self.rc_action("SetVolume", &format!(
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>{level}</DesiredVolume>"
        )).await?;
        if resp.contains("UPnPError") || resp.contains("<errorCode>") {
            // Sonos rejects RenderingControl SetVolume with 401.
            // Try GroupRenderingControl on the same host instead.
            if self.device_id.contains("RINCON") {
                let grc_url = self
                    .rendering_control_url
                    .replace("/RenderingControl/", "/GroupRenderingControl/");
                let grc_resp = self
                    .soap_action(
                        &grc_url,
                        "urn:schemas-upnp-org:service:GroupRenderingControl:1",
                        "SetGroupVolume",
                        &format!(
                            "<InstanceID>0</InstanceID><DesiredVolume>{level}</DesiredVolume>"
                        ),
                    )
                    .await?;
                if grc_resp.contains("UPnPError") || grc_resp.contains("<errorCode>") {
                    warn!(device = %self.name, level, response = %grc_resp, "sonos_group_volume_rejected");
                    return Err(format!(
                        "« {} » a refusé le réglage de volume. Réglez-le sur l'appareil lui-même.",
                        self.name
                    ));
                }
                debug!(device = %self.name, level, "sonos_group_volume_ok");
                return Ok(());
            }
            // The renderer answered, and said no. Reporting Ok() here — as this
            // did — made the slider move, the value persist, and nothing come
            // out of the speakers any louder: three layers agreeing on a change
            // that never happened (Eric, forum, renderer Diretta + PC vu comme
            // zone DLNA). Say it instead.
            warn!(device = %self.name, level, response = %resp, "dlna_set_volume_rejected");
            return Err(format!(
                "« {} » a refusé le réglage de volume. Réglez-le sur l'appareil lui-même.",
                self.name
            ));
        }
        debug!(device = %self.name, level, "dlna_set_volume_ok");
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let val = if muted { "1" } else { "0" };
        self.rc_action("SetMute", &format!(
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredMute>{val}</DesiredMute>"
        )).await?;
        // Mémorisé seulement après un SetMute accepté : `get_status` ne
        // redemande plus rien au renderer (#2263), donc ce champ est la seule
        // source du `muted` rendu — il ne doit jamais annoncer une coupure que
        // l'appareil a refusée.
        self.muted.store(muted, Ordering::Relaxed);
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let position_resp = self
            .av_action("GetPositionInfo", "<InstanceID>0</InstanceID>")
            .await?;
        let transport_resp = self
            .av_action("GetTransportInfo", "<InstanceID>0</InstanceID>")
            .await?;
        let volume_resp = if self.device_id.contains("RINCON") {
            let grc_url = self
                .rendering_control_url
                .replace("/RenderingControl/", "/GroupRenderingControl/");
            self.soap_action(
                &grc_url,
                "urn:schemas-upnp-org:service:GroupRenderingControl:1",
                "GetGroupVolume",
                "<InstanceID>0</InstanceID>",
            )
            .await
            .unwrap_or_default()
        } else {
            self.rc_action(
                "GetVolume",
                "<InstanceID>0</InstanceID><Channel>Master</Channel>",
            )
            .await?
        };
        // Pas de `GetMute` ici. Le poller passe par cette fonction une fois
        // par seconde et par zone pendant TOUTE la lecture : l'action valait
        // un quart du trafic SOAP envoyé au renderer, pour une valeur que
        // personne ne lisait (#2263). L'état coupé se lit maintenant en local.
        let state = if transport_resp.contains("PLAYING") {
            TransportState::Playing
        } else if transport_resp.contains("PAUSED") {
            TransportState::Paused
        } else if transport_resp.contains("TRANSITIONING") {
            TransportState::Transitioning
        } else {
            TransportState::Stopped
        };

        let position_ms = extract_tag(&position_resp, "RelTime")
            .map(|t| Self::parse_time(&t))
            .unwrap_or(0);
        let duration_ms = extract_tag(&position_resp, "TrackDuration")
            .map(|t| Self::parse_time(&t))
            .unwrap_or(0);
        let volume = extract_tag(&volume_resp, "CurrentVolume")
            .and_then(|v| v.parse::<f64>().ok())
            .map(|v| v / 100.0)
            .unwrap_or(0.5);
        let muted = self.muted.load(Ordering::Relaxed);
        let current_uri = extract_tag(&position_resp, "TrackURI");

        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms,
            volume,
            muted,
            current_uri,
            track_title: extract_tag(&position_resp, "dc:title"),
            track_artist: extract_tag(&position_resp, "dc:creator"),
            ended_naturally: false,
            // A renderer plays at 1x: keep the poller's wall-clock guards.
            realtime: true,
            // Aucune sortie hors la locale ne produit du DoP : le DSD y part
            // tel quel ou transcode, jamais empaquete dans du PCM 24 bits.
            dop_active: false,
        })
    }

    async fn is_available(&self) -> bool {
        self.client
            .get(&self.av_transport_url)
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .is_ok()
    }

    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let item_id = self.next_item_id();
        // Même échelle que le SetAVTransportURI du play : le DIDL complet du
        // gapless a la même taille, donc le même échec de lecture chez
        // Platinum — et un gapless silencieusement perdu, c'est une file qui
        // s'arrête entre deux pistes.
        let mut resp = None;
        // Même départ au niveau appris que le play : l'échec du DIDL complet
        // est une propriété de l'appareil, pas de la piste (#2394).
        for niveau in self.didl_niveau_appris.load(Ordering::Relaxed)..=2 {
            let metadata = match niveau {
                0 => Self::didl_metadata(media, item_id),
                1 => Self::didl_metadata_minimale(media, item_id, media.mime_type),
                _ => String::new(),
            };
            match self.av_action("SetNextAVTransportURI", &format!(
                "<InstanceID>0</InstanceID><NextURI>{}</NextURI><NextURIMetaData>{metadata}</NextURIMetaData>",
                media.url
            )).await {
                Ok(r) => {
                    self.didl_niveau_appris.store(niveau, Ordering::Relaxed);
                    resp = Some(r);
                    break;
                }
                Err(e) if e.starts_with(SOAP_HTTP_SANS_CORPS_PREFIX) && niveau < 2 => {
                    warn!(device = %self.name, niveau = niveau + 1, error = %e, "dlna_set_next_corps_illisible_didl_reduit");
                }
                Err(e) => return Err(e),
            }
        }
        let resp =
            resp.ok_or_else(|| "SetNextAVTransportURI: aucune tentative aboutie".to_string())?;
        if resp.contains("UPnPError") || resp.contains("<errorCode>") {
            warn!(device = %self.name, response = %resp, "dlna_set_next_rejected");
            return Err(format!("SetNextAVTransportURI rejected: {resp}"));
        }
        info!(device = %self.name, url = media.url, "dlna_set_next");
        Ok(())
    }
}

impl DlnaOutput {
    pub async fn get_protocol_info(&self) -> Result<Vec<String>, String> {
        let cm_url = self
            .connection_manager_url
            .as_deref()
            .unwrap_or(&self.av_transport_url);
        let body = self
            .soap_action(
                cm_url,
                "urn:schemas-upnp-org:service:ConnectionManager:1",
                "GetProtocolInfo",
                "",
            )
            .await?;
        let sink = extract_tag(&body, "Sink").unwrap_or_default();
        Ok(sink
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    }
}

#[derive(Debug, Clone, Default)]
pub struct DsdCapability {
    pub supports_dsf: bool,
    pub supports_dff: bool,
    pub dsf_mime: Option<String>,
}

/// Read native DSD support out of a non-empty GetProtocolInfo Sink.
///
/// Split out of `probe_dsd_support` so the parsing can be unit-tested without a
/// live renderer: the caller owns the "did the probe even succeed" question
/// (`Option`), this owns "what does the Sink say".
///
/// `dsf_mime` keeps the renderer's own spelling of the MIME (3rd colon-separated
/// field of `http-get:*:audio/dsf:*`), because some renderers only accept the
/// exact MIME they advertise rather than the generic `application/x-dsd`.
fn parse_dsd_capability(protocols: &[String]) -> DsdCapability {
    let mut cap = DsdCapability::default();
    for proto in protocols {
        let lower = proto.to_lowercase();
        if lower.contains("x-dsd")
            || lower.contains("audio/dsf")
            || lower.contains("audio/x-dsf")
            || lower.contains("application/x-dsd")
            || lower.contains("application/dsf")
            || lower.contains("audio/vnd.dsd")
        {
            cap.supports_dsf = true;
            if cap.dsf_mime.is_none() {
                let parts: Vec<&str> = proto.split(':').collect();
                if parts.len() >= 3 {
                    cap.dsf_mime = Some(parts[2].trim().to_string());
                }
            }
        }
        if lower.contains("audio/dff") || lower.contains("x-dff") || lower.contains("audio/x-dff") {
            cap.supports_dff = true;
        }
    }
    cap
}

impl DlnaOutput {
    /// Probe the renderer's GetProtocolInfo Sink for native DSD support.
    ///
    /// `Some(cap)` when the Sink was actually read — including a conclusive
    /// "this renderer does not do DSD" (all flags false). `None` when the probe
    /// was **inconclusive**: GetProtocolInfo failed, or the Sink came back
    /// empty. The caller must fall back conservatively for `None` but must NOT
    /// cache it — same rule as `supports_mime` below. A transient
    /// GetProtocolInfo failure (renderer asleep, busy, or slow to answer right
    /// after discovery) would otherwise pin a DSD-capable renderer to the
    /// DSD→PCM transcode path for the whole session, with no way to recover
    /// short of restarting the server.
    pub async fn probe_dsd_support(&self) -> Option<DsdCapability> {
        let protocols = match self.get_protocol_info().await {
            Ok(p) => p,
            Err(e) => {
                warn!(device = %self.name, error = %e, "dsd_probe_protocol_info_failed");
                return None;
            }
        };
        if protocols.is_empty() {
            debug!(device = %self.name, "dsd_probe_empty_sink");
            return None;
        }
        debug!(device = %self.name, protocols = ?protocols, "dsd_probe_protocol_info_raw");
        let cap = parse_dsd_capability(&protocols);
        info!(device = %self.name, supports_dsf = cap.supports_dsf, supports_dff = cap.supports_dff, dsf_mime = ?cap.dsf_mime, protocols_count = protocols.len(), "dsd_probe_result");
        Some(cap)
    }

    /// Probe the renderer's GetProtocolInfo Sink to check if a given MIME type
    /// is supported.  Protocol info entries have the format:
    ///   `http-get:*:audio/flac:*`
    /// The third colon-separated field is the MIME type.
    /// `Some(true)`/`Some(false)` when the Sink was successfully probed;
    /// `None` when the probe failed or the Sink was empty (inconclusive). The
    /// caller falls back conservatively for `None` but must NOT cache it — a
    /// transient GetProtocolInfo failure on a budget renderer (Marco's Denon
    /// Ceol N12) must not poison FLAC support for the whole session, forcing a
    /// WAV transcode on every track even though the renderer decodes FLAC.
    pub async fn supports_mime(&self, mime: &str) -> Option<bool> {
        let protocols = match self.get_protocol_info().await {
            Ok(p) => p,
            Err(e) => {
                debug!(device = %self.name, error = %e, mime, "protocol_info_unavailable");
                return None;
            }
        };
        if protocols.is_empty() {
            debug!(device = %self.name, mime, "protocol_info_empty_sink");
            return None;
        }
        if protocol_sink_supports_mime(mime, &protocols) {
            return Some(true);
        }
        info!(device = %self.name, mime, protocols_count = protocols.len(), "dlna_mime_not_supported_by_renderer");
        Some(false)
    }

    /// One-shot capability probe for the renderer-config UI: reads the
    /// GetProtocolInfo `Sink` ONCE and summarises which audio formats it
    /// advertises, so the user can pick a sensible output override (native FLAC,
    /// native ALAC, forced WAV/LPCM…) with evidence rather than by trial. A
    /// failed/empty probe returns `probed: false` (inconclusive — the renderer
    /// may still decode more than it advertises; the negotiation fallbacks stay
    /// in charge).
    pub async fn probe_capabilities(&self) -> RendererCapabilities {
        match self.get_protocol_info().await {
            Ok(sink) if !sink.is_empty() => renderer_caps_from_sink(sink),
            // Le `_ =>` d'origine avalait l'erreur : l'utilisateur voyait
            // « impossible de lire les capacités » et le journal ne portait
            // AUCUNE trace de la sonde (#1984). Dire lequel des deux cas s'est
            // produit — l'appel a échoué, ou le Sink est vide — coûte une ligne
            // et distingue « injoignable » de « joignable mais muet ».
            Ok(_) => {
                warn!(device = %self.name, "renderer_caps_probe_empty_sink");
                RendererCapabilities::inconclusive("empty_sink")
            }
            Err(e) => {
                warn!(device = %self.name, error = %e, "renderer_caps_probe_failed");
                RendererCapabilities::inconclusive("soap_failed")
            }
        }
    }
}

/// What a DLNA renderer advertises in its GetProtocolInfo `Sink`. `probed` is
/// false when the Sink could not be read (empty/timeout) — everything else is
/// then meaningless and the UI should say "couldn't read capabilities".
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RendererCapabilities {
    pub probed: bool,
    /// Stable machine-readable cause when `probed` is false. The API, not the
    /// translated UI, knows whether SOAP failed or returned an empty Sink.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    pub flac: bool,
    /// Plain `audio/wav` / `audio/x-wav`.
    pub wav: bool,
    /// 16-bit LPCM (`audio/L16`) — the standard DLNA WAV profile.
    pub lpcm16: bool,
    /// 24-bit LPCM (`audio/L24`) — gates the "WAV 24-bit" override.
    pub lpcm24: bool,
    pub alac: bool,
    pub aac: bool,
    pub mp3: bool,
    pub dsd: bool,
    /// Raw Sink entries, for an advanced/debug view.
    pub sink: Vec<String>,
}

impl RendererCapabilities {
    fn inconclusive(reason: &'static str) -> Self {
        Self {
            reason: Some(reason),
            ..Self::default()
        }
    }
}

/// Pure Sink → capabilities mapping (unit-tested; `probe_capabilities` wraps it
/// around the SOAP call).
fn renderer_caps_from_sink(sink: Vec<String>) -> RendererCapabilities {
    // Param-aware match: LPCM entries carry `;rate=…;channels=…` after the MIME
    // (`audio/L16;rate=44100;channels=2`), so we compare the base MIME only.
    // Also accepts the `audio/x-…` legacy variant and the `*` wildcard, like
    // `protocol_sink_supports_mime` (which only handles the param-less case).
    let has = |want: &str| -> bool {
        let want = want.to_lowercase();
        let alt = want
            .strip_prefix("audio/x-")
            .map(|r| format!("audio/{r}"))
            .or_else(|| want.strip_prefix("audio/").map(|r| format!("audio/x-{r}")));
        sink.iter().any(|p| {
            let Some(field) = p.split(':').nth(2) else {
                return false;
            };
            let mime = field.trim().to_lowercase();
            let base = mime.split(';').next().unwrap_or(&mime).trim();
            base == want || base == "*" || alt.as_deref() == Some(base)
        })
    };
    let dsd = sink.iter().any(|p| {
        let l = p.to_lowercase();
        l.contains("x-dsd")
            || l.contains("audio/dsf")
            || l.contains("audio/dff")
            || l.contains("audio/x-dsf")
            || l.contains("audio/x-dff")
            || l.contains("audio/vnd.dsd")
            || l.contains("application/x-dsd")
    });
    RendererCapabilities {
        probed: true,
        reason: None,
        flac: has("audio/flac"),
        wav: has("audio/wav"),
        lpcm16: has("audio/l16"),
        lpcm24: has("audio/l24"),
        // ALAC is rarely advertised distinctly; renderers expose it as m4a/mp4.
        alac: has("audio/x-m4a") || has("audio/alac") || has("audio/mp4"),
        aac: has("audio/aac") || has("audio/mp4"),
        mp3: has("audio/mpeg"),
        dsd,
        sink,
    }
}

/// Whether a renderer's GetProtocolInfo `Sink` entries advertise support for
/// `mime`. Each entry looks like `http-get:*:audio/flac:*` (the third
/// colon-separated field is the MIME type).
///
/// Matches the exact MIME, a `*` wildcard, and the legacy `x-` variant: many
/// renderers advertise `audio/x-flac` for `audio/flac` (Denon Ceol N12,
/// Marco), and forcing WAV on those wastes bandwidth and loses bit-perfect
/// FLAC the renderer could decode natively.
fn protocol_sink_supports_mime(mime: &str, protocols: &[String]) -> bool {
    let mime_lower = mime.to_lowercase();
    let mime_alt = if let Some(rest) = mime_lower.strip_prefix("audio/x-") {
        format!("audio/{rest}")
    } else if let Some(rest) = mime_lower.strip_prefix("audio/") {
        format!("audio/x-{rest}")
    } else {
        mime_lower.clone()
    };
    for proto in protocols {
        let fields: Vec<&str> = proto.split(':').collect();
        if fields.len() >= 3 {
            let proto_mime = fields[2].trim().to_lowercase();
            if proto_mime == mime_lower || proto_mime == mime_alt || proto_mime == "*" {
                return true;
            }
        }
    }
    false
}

/// Base MIME (third colon-separated field, params stripped) of a Sink entry
/// such as `http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM`.
fn sink_entry_base_mime(entry: &str) -> Option<String> {
    let field = entry.split(':').nth(2)?;
    let mime = field.trim();
    Some(mime.split(';').next().unwrap_or(mime).trim().to_string())
}

/// Choose the MIME spelling to announce in the DIDL / SetAVTransportURI given
/// the renderer's GetProtocolInfo `Sink`.
///
/// Beoplay A9 / Sink audio/x-flac, forum 714: strict renderers (B&O, Lyngdorf)
/// reject SetAVTransportURI with 714 "Illegal MIME-type" when the announced
/// MIME differs from the exact spelling listed in their Sink, even though they
/// can decode the stream. If `desired` is already listed we keep it; if only a
/// known alias is listed (`audio/flac`↔`audio/x-flac`, `audio/mpeg`↔`audio/mp3`,
/// `audio/wav`↔`audio/x-wav`) we announce the spelling the Sink actually lists;
/// otherwise `desired` is returned unchanged (empty/unknown Sink ⇒ previous
/// behaviour, no regression).
fn advertised_mime_for_sink(desired: &str, sink: &[String]) -> String {
    let listed: Vec<String> = sink
        .iter()
        .filter_map(|e| sink_entry_base_mime(e))
        .collect();
    // Already listed verbatim (case-insensitive): announce as-is.
    if listed.iter().any(|b| b.eq_ignore_ascii_case(desired)) {
        return desired.to_string();
    }
    let aliases: &[&str] = match desired.to_lowercase().as_str() {
        "audio/flac" => &["audio/x-flac"],
        "audio/x-flac" => &["audio/flac"],
        "audio/mpeg" => &["audio/mp3"],
        "audio/mp3" => &["audio/mpeg"],
        "audio/wav" => &["audio/x-wav"],
        "audio/x-wav" => &["audio/wav"],
        _ => &[],
    };
    for alias in aliases {
        if let Some(found) = listed.iter().find(|b| b.eq_ignore_ascii_case(alias)) {
            return found.clone();
        }
    }
    desired.to_string()
}

/// Pick a universally-decodable PCM MIME the renderer's `Sink` lists, for the
/// one-shot 714 fallback (Beoplay A9 / forum 714). Prefers WAV, then LPCM
/// (`audio/L16`), returning the exact spelling the Sink uses so the announced
/// MIME passes the renderer's strict Sink check. `None` when the Sink lists no
/// PCM profile.
fn fallback_mime_from_sink(sink: &[String]) -> Option<String> {
    for want in ["audio/wav", "audio/x-wav", "audio/l16"] {
        if let Some(found) = sink
            .iter()
            .filter_map(|e| sink_entry_base_mime(e))
            .find(|b| b.eq_ignore_ascii_case(want))
        {
            return Some(found);
        }
    }
    None
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

/// Le renderer a-t-il réellement cessé de jouer, d'après sa réponse
/// `GetTransportInfo` ? Un Stop acquitté n'est pas un Stop appliqué :
/// l'Eversolo répond OK puis met ~1-2 s à s'arrêter, et un
/// SetAVTransportURI envoyé dans cette fenêtre est acquitté… et ignoré
/// (la course des 5 ms, .42, 24/08).
fn arret_effectif(transport_resp: &str) -> bool {
    !transport_resp.contains("PLAYING") && !transport_resp.contains("TRANSITIONING")
}

/// Verdict sur l'URI que le renderer dit tenir après notre Play.
#[derive(Debug, PartialEq, Eq)]
enum UriVerdict {
    /// C'est bien la nôtre : le Play est appliqué.
    Appliquee,
    /// Vide, ou un flux Tune qui n'est pas le nôtre : le renderer a acquitté
    /// toute la séquence et joue toujours autre chose.
    PasAppliquee,
    /// Une URI étrangère qu'on ne sait pas interpréter (un renderer qui
    /// réécrit, un GetMediaInfo exotique) : on ne conclut rien.
    Indeterminee,
}

/// La partie discriminante de l'URL d'un flux : son chemin (`/stream/…`).
/// L'hôte peut différer entre ce qu'on envoie et ce que le renderer
/// rapporte (résolution DNS, réécriture d'IP) — le chemin, lui, est unique.
fn chemin_du_flux(url: &str) -> &str {
    url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .and_then(|reste| reste.find('/').map(|i| &reste[i..]))
        .unwrap_or(url)
}

fn verdict_uri_appliquee(current_uri: Option<&str>, url_attendue: &str) -> UriVerdict {
    let Some(uri) = current_uri else {
        return UriVerdict::Indeterminee;
    };
    let uri = uri.trim();
    if uri.is_empty() {
        return UriVerdict::PasAppliquee;
    }
    if uri.contains(chemin_du_flux(url_attendue)) {
        return UriVerdict::Appliquee;
    }
    if uri.contains("/stream/") {
        // Un flux Tune — le périmé d'avant notre Play, ou celui d'un autre
        // serveur : dans les deux cas, PAS ce qu'on vient d'envoyer.
        return UriVerdict::PasAppliquee;
    }
    UriVerdict::Indeterminee
}

/// Un refus SOAP portant le code UPnP **701 « Transition not available »**.
/// Ce n'est pas une panne : le renderer refuse LA TRANSITION à cet instant.
fn est_701(reponse_play: &str) -> bool {
    reponse_play.contains(">701<")
        || reponse_play
            .to_ascii_lowercase()
            .contains("transition not available")
}

/// Ce qu'il faut envoyer — ou ne pas envoyer — avant de redemander `Play`
/// après un refus.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum RepriseApresRefus {
    /// Barème historique : au PREMIER essai, un Stop puis un Play (écrit pour
    /// le Revox S100 et son 501). Conduite par défaut, inchangée.
    StopPuisPlay,
    /// 701 alors que le transport charge encore l'URI : le laisser finir. Un
    /// Stop ici le ferait retomber et rendrait le 701 suivant certain.
    Attendre,
    /// 701 alors que le transport ne tient plus de média : sans réarmement de
    /// l'URI, chaque `Play` suivant est un 701 de plus. C'est ce que montre le
    /// journal de FabienM (#2581) — cinq refus, puis un succès immédiat dès
    /// qu'un `SetAVTransportURI` est rejoué.
    ReArmerUri,
}

/// Décide de la reprise à partir du refus reçu et de l'état que le transport
/// déclare (`CurrentTransportState`). On ne dévie du barème historique que sur
/// une information POSITIVE : un renderer muet, ou qui n'a pas
/// `GetTransportInfo`, garde exactement l'ancienne conduite.
fn reprise_apres_refus_play(reponse_play: &str, etat_transport: Option<&str>) -> RepriseApresRefus {
    if !est_701(reponse_play) {
        return RepriseApresRefus::StopPuisPlay;
    }
    match etat_transport.map(|e| e.trim().to_ascii_uppercase()) {
        Some(e) if e.contains("TRANSITIONING") => RepriseApresRefus::Attendre,
        Some(e) if e.contains("NO_MEDIA_PRESENT") || e.contains("STOPPED") => {
            RepriseApresRefus::ReArmerUri
        }
        _ => RepriseApresRefus::StopPuisPlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La faute SOAP EXACTE relevée dans le journal de FabienM (#2581).
    const FAUTE_701: &str = concat!(
        "<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>",
        "<detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">",
        "<errorCode>701</errorCode>",
        "<errorDescription>Transition not available</errorDescription>",
        "</UPnPError></detail></s:Fault>"
    );

    /// 501 Action Failed — le refus pour lequel le barème Stop+Play a été écrit
    /// (Revox S100). Il ne doit RIEN changer de son comportement.
    const FAUTE_501: &str = "<UPnPError><errorCode>501</errorCode><errorDescription>Action Failed</errorDescription></UPnPError>";

    #[test]
    fn le_701_se_reconnait_au_code_comme_au_libelle() {
        assert!(est_701(FAUTE_701));
        assert!(est_701(
            "<errorDescription>Transition not available</errorDescription>"
        ));
        // Un 7010 n'est pas un 701, et les autres codes du fichier non plus.
        assert!(!est_701("<errorCode>7010</errorCode>"));
        assert!(!est_701(FAUTE_501));
        assert!(!est_701("<errorCode>714</errorCode>"));
    }

    /// #2581 — le renderer charge encore l'URI : lui envoyer le Stop du barème
    /// le ferait retomber. On attend.
    #[test]
    fn un_701_pendant_le_chargement_fait_attendre_sans_stop() {
        assert_eq!(
            reprise_apres_refus_play(FAUTE_701, Some("TRANSITIONING")),
            RepriseApresRefus::Attendre
        );
    }

    /// #2581 — le transport ne tient plus de média : sans réarmement de l'URI,
    /// les cinq tentatives sont cinq 701 d'avance.
    #[test]
    fn un_701_sans_media_rearme_l_uri() {
        for etat in ["NO_MEDIA_PRESENT", "STOPPED", " no_media_present "] {
            assert_eq!(
                reprise_apres_refus_play(FAUTE_701, Some(etat)),
                RepriseApresRefus::ReArmerUri,
                "état {etat}"
            );
        }
    }

    /// Zéro régression : un renderer qui ne dit rien d'exploitable garde le
    /// barème historique, au mot près.
    #[test]
    fn un_701_muet_ne_devie_pas_du_bareme_historique() {
        for etat in [None, Some(""), Some("PLAYING"), Some("RECORDING")] {
            assert_eq!(
                reprise_apres_refus_play(FAUTE_701, etat),
                RepriseApresRefus::StopPuisPlay,
                "état {etat:?}"
            );
        }
    }

    /// Zéro régression : le 501 du Revox garde son Stop+Play, QUEL QUE SOIT
    /// l'état déclaré par le transport.
    #[test]
    fn un_refus_qui_n_est_pas_un_701_garde_le_stop_du_revox() {
        for etat in [
            None,
            Some("TRANSITIONING"),
            Some("NO_MEDIA_PRESENT"),
            Some("STOPPED"),
        ] {
            assert_eq!(
                reprise_apres_refus_play(FAUTE_501, etat),
                RepriseApresRefus::StopPuisPlay,
                "état {etat:?}"
            );
        }
    }

    /// L'état lu dans la boucle vient d'un `GetTransportInfo` complet : le
    /// chaînage extraction → décision doit tenir sur la réponse RÉELLE.
    #[test]
    fn l_etat_se_lit_dans_la_reponse_get_transport_info() {
        let reponse = concat!(
            "<u:GetTransportInfoResponse>",
            "<CurrentTransportState>NO_MEDIA_PRESENT</CurrentTransportState>",
            "<CurrentTransportStatus>OK</CurrentTransportStatus>",
            "<CurrentSpeed>1</CurrentSpeed>",
            "</u:GetTransportInfoResponse>"
        );
        let etat = extract_tag(reponse, "CurrentTransportState");
        assert_eq!(etat.as_deref(), Some("NO_MEDIA_PRESENT"));
        assert_eq!(
            reprise_apres_refus_play(FAUTE_701, etat.as_deref()),
            RepriseApresRefus::ReArmerUri
        );
    }

    #[test]
    fn caps_from_sink_maps_advertised_formats() {
        // A typical hi-fi renderer Sink: FLAC (x- variant), 16-bit LPCM, MP3,
        // AAC/MP4, and DSF — but NOT 24-bit LPCM.
        let sink = vec![
            "http-get:*:audio/x-flac:DLNA.ORG_PN=FLAC".to_string(),
            "http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM".to_string(),
            "http-get:*:audio/mpeg:DLNA.ORG_PN=MP3".to_string(),
            "http-get:*:audio/mp4:*".to_string(),
            "http-get:*:audio/x-dsf:*".to_string(),
        ];
        let c = renderer_caps_from_sink(sink);
        assert!(c.probed);
        assert!(c.flac, "x-flac must count as FLAC");
        assert!(c.lpcm16, "audio/L16 present");
        assert!(!c.lpcm24, "no audio/L24 advertised");
        assert!(c.mp3 && c.aac && c.dsd);
        assert_eq!(c.reason, None, "une sonde concluante ne porte aucun refus");
    }

    #[test]
    fn une_sonde_inconclusive_expose_une_raison_stable() {
        let c = RendererCapabilities::inconclusive("empty_sink");
        let json = serde_json::to_value(c).unwrap();

        assert_eq!(json["probed"], false);
        assert_eq!(json["reason"], "empty_sink");
    }

    #[test]
    fn parse_dsd_capability_keeps_the_renderer_own_mime() {
        // Yamaha R-N2000A-shaped Sink: the renderer advertises its own spelling
        // of the DSD MIME, and we must serve that one back rather than the
        // generic application/x-dsd (cf. the passthrough path in orchestrator).
        let sink = vec![
            "http-get:*:audio/L16;rate=44100;channels=2:*".to_string(),
            "http-get:*:audio/dsf:*".to_string(),
        ];
        let cap = parse_dsd_capability(&sink);
        assert!(cap.supports_dsf);
        assert!(!cap.supports_dff);
        assert_eq!(cap.dsf_mime.as_deref(), Some("audio/dsf"));
    }

    #[test]
    fn parse_dsd_capability_reports_no_dsd_for_a_pcm_only_sink() {
        // A conclusive negative — distinct from a failed probe, which never
        // reaches this function (probe_dsd_support returns None instead).
        let sink = vec![
            "http-get:*:audio/mpeg:*".to_string(),
            "http-get:*:audio/L16;rate=44100;channels=2:*".to_string(),
        ];
        let cap = parse_dsd_capability(&sink);
        assert!(!cap.supports_dsf);
        assert!(!cap.supports_dff);
        assert_eq!(cap.dsf_mime, None);
    }

    #[test]
    fn parse_dsd_capability_detects_dff_and_x_dsd_variants() {
        let sink = vec![
            "http-get:*:audio/x-dsd:*".to_string(),
            "http-get:*:audio/x-dff:*".to_string(),
        ];
        let cap = parse_dsd_capability(&sink);
        assert!(cap.supports_dsf, "x-dsd counts as DSD");
        assert!(cap.supports_dff);
        assert_eq!(cap.dsf_mime.as_deref(), Some("audio/x-dsd"));
    }

    #[test]
    fn caps_from_sink_flags_l24_when_present() {
        let sink = vec!["http-get:*:audio/L24;rate=96000;channels=2:*".to_string()];
        let c = renderer_caps_from_sink(sink);
        assert!(c.lpcm24, "audio/L24 gates the WAV 24-bit override");
        assert!(!c.flac);
    }

    #[test]
    fn protocol_sink_matches_x_flac_variant() {
        // Denon Ceol N12 (Marco) advertises FLAC as `audio/x-flac`. Asking for
        // `audio/flac` must match it so we passthrough instead of forcing WAV.
        let sink = vec![
            "http-get:*:audio/x-flac:DLNA.ORG_PN=FLAC".to_string(),
            "http-get:*:audio/mpeg:*".to_string(),
        ];
        assert!(protocol_sink_supports_mime("audio/flac", &sink));
        assert!(protocol_sink_supports_mime("audio/x-flac", &sink));
        // Exact and wildcard still work; an unadvertised format is rejected.
        assert!(protocol_sink_supports_mime("audio/mpeg", &sink));
        assert!(!protocol_sink_supports_mime("audio/aac", &sink));
        assert!(protocol_sink_supports_mime(
            "audio/flac",
            &["http-get:*:*:*".to_string()]
        ));
    }

    #[test]
    fn advertised_mime_rewrites_flac_to_sink_x_flac() {
        // Beoplay A9 (forum 714): Sink lists audio/x-flac but NOT audio/flac.
        // We must announce the exact spelling the Sink lists, else 714.
        let sink = vec![
            "http-get:*:audio/x-flac:DLNA.ORG_PN=FLAC".to_string(),
            "http-get:*:audio/wav:*".to_string(),
            "http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM".to_string(),
        ];
        assert_eq!(
            advertised_mime_for_sink("audio/flac", &sink),
            "audio/x-flac"
        );
    }

    #[test]
    fn advertised_mime_keeps_exact_sink_spelling() {
        // Sink lists audio/flac verbatim → announce it unchanged.
        let sink = vec!["http-get:*:audio/flac:DLNA.ORG_PN=FLAC".to_string()];
        assert_eq!(advertised_mime_for_sink("audio/flac", &sink), "audio/flac");
    }

    #[test]
    fn advertised_mime_unchanged_when_alias_absent() {
        // Neither audio/flac nor audio/x-flac listed, and empty Sink → keep
        // the desired MIME unchanged (previous behaviour, no regression).
        let sink = vec!["http-get:*:audio/mpeg:*".to_string()];
        assert_eq!(advertised_mime_for_sink("audio/flac", &sink), "audio/flac");
        assert_eq!(advertised_mime_for_sink("audio/flac", &[]), "audio/flac");
    }

    #[test]
    fn advertised_mime_rewrites_mpeg_mp3_alias() {
        let sink = vec!["http-get:*:audio/mp3:*".to_string()];
        assert_eq!(advertised_mime_for_sink("audio/mpeg", &sink), "audio/mp3");
    }

    #[test]
    fn fallback_mime_prefers_wav_then_l16() {
        let sink = vec![
            "http-get:*:audio/x-flac:*".to_string(),
            "http-get:*:audio/wav:*".to_string(),
            "http-get:*:audio/L16;rate=44100;channels=2:*".to_string(),
        ];
        assert_eq!(fallback_mime_from_sink(&sink).as_deref(), Some("audio/wav"));

        let sink_l16 = vec!["http-get:*:audio/L16;rate=44100;channels=2:*".to_string()];
        assert_eq!(
            fallback_mime_from_sink(&sink_l16).as_deref(),
            Some("audio/L16")
        );

        let sink_none = vec!["http-get:*:audio/x-flac:*".to_string()];
        assert_eq!(fallback_mime_from_sink(&sink_none), None);
    }

    #[test]
    fn parse_time_works() {
        assert_eq!(DlnaOutput::parse_time("0:03:45"), 225_000);
        assert_eq!(DlnaOutput::parse_time("1:00:00"), 3_600_000);
        assert_eq!(DlnaOutput::parse_time("0:00:00.000"), 0);
    }

    #[test]
    fn parse_time_fractional_seconds() {
        assert_eq!(DlnaOutput::parse_time("0:04:16.487"), 256_487);
        assert_eq!(DlnaOutput::parse_time("0:03:46.5"), 226_500);
        assert_eq!(DlnaOutput::parse_time("0:03:46.50"), 226_500);
        assert_eq!(DlnaOutput::parse_time("0:03:46.500"), 226_500);
        assert_eq!(DlnaOutput::parse_time("0:00:01.1"), 1_100);
        assert_eq!(DlnaOutput::parse_time("0:00:01.12"), 1_120);
        assert_eq!(DlnaOutput::parse_time("0:00:01.123"), 1_123);
    }

    #[test]
    fn format_time_works() {
        assert_eq!(DlnaOutput::format_time(225_000), "0:03:45");
        assert_eq!(DlnaOutput::format_time(3_600_000), "1:00:00");
    }

    /// La course des 5 ms (.42, 24/08) : un Stop acquitté n'est pas appliqué.
    #[test]
    fn arret_effectif_lit_l_etat_du_transport() {
        assert!(!arret_effectif(
            "<CurrentTransportState>PLAYING</CurrentTransportState>"
        ));
        assert!(!arret_effectif(
            "<CurrentTransportState>TRANSITIONING</CurrentTransportState>"
        ));
        assert!(arret_effectif(
            "<CurrentTransportState>STOPPED</CurrentTransportState>"
        ));
        assert!(arret_effectif(
            "<CurrentTransportState>NO_MEDIA_PRESENT</CurrentTransportState>"
        ));
        // PAUSED_PLAYBACK : le transport n'avance plus, l'URI peut changer.
        assert!(arret_effectif(
            "<CurrentTransportState>PAUSED_PLAYBACK</CurrentTransportState>"
        ));
    }

    #[test]
    fn verdict_uri_notre_flux_est_applique() {
        let url = "http://192.168.1.42:8888/stream/abc-123.wav";
        assert_eq!(verdict_uri_appliquee(Some(url), url), UriVerdict::Appliquee);
        // L'hôte peut différer (IP réécrite) — le chemin suffit.
        assert_eq!(
            verdict_uri_appliquee(Some("http://tune.local:8888/stream/abc-123.wav"), url),
            UriVerdict::Appliquee
        );
    }

    #[test]
    fn verdict_uri_vide_ou_flux_perime_n_est_pas_applique() {
        let url = "http://192.168.1.42:8888/stream/abc-123.wav";
        // L'Eversolo qui garde l'URI d'avant : vide, ou un autre flux Tune.
        assert_eq!(
            verdict_uri_appliquee(Some(""), url),
            UriVerdict::PasAppliquee
        );
        assert_eq!(
            verdict_uri_appliquee(Some("http://192.168.1.42:8888/stream/vieux-flux.flac"), url),
            UriVerdict::PasAppliquee
        );
        // Le flux d'un AUTRE serveur Tune : pas le nôtre non plus.
        assert_eq!(
            verdict_uri_appliquee(Some("http://192.168.1.18:8888/stream/xyz.wav"), url),
            UriVerdict::PasAppliquee
        );
    }

    #[test]
    fn verdict_uri_etrangere_ou_absente_ne_conclut_rien() {
        let url = "http://192.168.1.42:8888/stream/abc-123.wav";
        // Un renderer qui réécrit (Sonos et ses URI propriétaires) ne doit
        // JAMAIS être déclaré en échec sur cette seule base.
        assert_eq!(
            verdict_uri_appliquee(Some("x-rincon-queue:RINCON_123#0"), url),
            UriVerdict::Indeterminee
        );
        assert_eq!(verdict_uri_appliquee(None, url), UriVerdict::Indeterminee);
    }

    #[test]
    fn extract_tag_works() {
        let xml = "<RelTime>0:03:45</RelTime><TrackDuration>0:05:30</TrackDuration>";
        assert_eq!(extract_tag(xml, "RelTime"), Some("0:03:45".into()));
        assert_eq!(extract_tag(xml, "TrackDuration"), Some("0:05:30".into()));
        assert_eq!(extract_tag(xml, "Missing"), None);
    }

    #[test]
    fn didl_metadata_with_cover_and_album() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Test Track"),
                artist: Some("Test Artist"),
                album: Some("Test Album"),
                cover_url: Some("http://example.com/cover.jpg"),
                duration_ms: Some(256_000),
                file_size: Some(50_000_000),
                ..Default::default()
            },
            "1",
        );
        assert!(didl.contains("Test Track"));
        assert!(didl.contains("Test Artist"));
        assert!(didl.contains("Test Album"));
        assert!(didl.contains("albumArtURI"));
        assert!(didl.contains("cover.jpg"));
        assert!(
            didl.contains("dlna:profileID"),
            "albumArtURI must include dlna:profileID"
        );
        assert!(
            didl.contains("JPEG_TN"),
            "albumArtURI must use JPEG_TN profile"
        );
        assert!(
            didl.contains("xmlns:dlna"),
            "DIDL-Lite must declare xmlns:dlna namespace"
        );
        assert!(
            didl.contains("DLNA.ORG_OP=01"),
            "protocolInfo must include DLNA.ORG_OP"
        );
        assert!(
            didl.contains("DLNA.ORG_FLAGS="),
            "protocolInfo must include DLNA.ORG_FLAGS"
        );
        assert!(didl.contains("size="), "res must include size attribute");
        assert!(
            didl.contains("duration="),
            "res must include duration attribute"
        );
    }

    #[test]
    fn didl_metadata_without_cover() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Title"),
                ..Default::default()
            },
            "1",
        );
        assert!(didl.contains("Title"));
        assert!(!didl.contains("albumArtURI"));
        assert!(!didl.contains("upnp:album"));
        assert!(!didl.contains("dc:creator"));
        assert!(!didl.contains("size="));
        assert!(!didl.contains("duration="));
    }

    #[test]
    fn didl_metadata_null_artist_string() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Title"),
                artist: Some("null"),
                ..Default::default()
            },
            "1",
        );
        assert!(
            !didl.contains("dc:creator"),
            "literal 'null' artist must be omitted"
        );
    }

    #[test]
    fn didl_metadata_empty_artist() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Title"),
                artist: Some(""),
                ..Default::default()
            },
            "1",
        );
        assert!(!didl.contains("dc:creator"), "empty artist must be omitted");
    }

    #[test]
    fn didl_escapes_special_chars() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream?a=1&b=2",
                mime_type: "audio/flac",
                title: Some("Rock & Roll"),
                artist: Some("AC/DC"),
                ..Default::default()
            },
            "1",
        );
        // build_escaped() double-escapes ampersands: first XML-escape for
        // DIDL content, then partial_escape for SOAP embedding.
        // "&" -> "&amp;" (XML) -> "&amp;amp;" (SOAP partial escape)
        // Note: quotes are NOT escaped (partial_escape), matching what
        // Denon/Marantz renderers expect in SOAP text content.
        assert!(didl.contains("Rock &amp;amp; Roll"));
        assert!(didl.contains("AC/DC"));
        assert!(didl.contains("a=1&amp;amp;b=2"));
    }

    #[test]
    fn didl_dlna_flags_wav() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/s",
                mime_type: "audio/wav",
                title: Some("T"),
                ..Default::default()
            },
            "1",
        );
        assert!(
            didl.contains("DLNA.ORG_PN=LPCM"),
            "WAV must have LPCM profile"
        );
    }

    #[test]
    fn didl_dlna_flags_mp3() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/s",
                mime_type: "audio/mpeg",
                title: Some("T"),
                ..Default::default()
            },
            "1",
        );
        assert!(
            didl.contains("DLNA.ORG_PN=MP3"),
            "MP3 must have MP3 profile"
        );
    }

    #[test]
    fn didl_metadata_includes_audio_params() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/s.wav",
                mime_type: "audio/wav",
                title: Some("DSD Track"),
                sample_rate: Some(176_400),
                bit_depth: Some(24),
                channels: Some(2),
                ..Default::default()
            },
            "1",
        );
        assert!(
            didl.contains("sampleFrequency=\"176400\""),
            "DIDL must include sampleFrequency for DSD->PCM"
        );
        assert!(
            didl.contains("bitsPerSample=\"24\""),
            "DIDL must include bitsPerSample for DSD->PCM"
        );
        assert!(
            didl.contains("nrAudioChannels=\"2\""),
            "DIDL must include nrAudioChannels for DSD->PCM"
        );
    }

    #[test]
    fn parse_time_edge_cases() {
        assert_eq!(DlnaOutput::parse_time(""), 0);
        assert_eq!(DlnaOutput::parse_time("NOT_A_TIME"), 0);
        assert_eq!(DlnaOutput::parse_time("0:00:00"), 0);
        assert_eq!(DlnaOutput::parse_time("0:00:01"), 1_000);
        assert_eq!(DlnaOutput::parse_time("23:59:59.999"), 86_399_999);
    }

    #[test]
    fn parse_time_dmp_a6_scenario() {
        // DMP-A6 reports "0:03:46" for a track that's actually 4:16.487.
        // With fractional parsing, "0:03:46.000" should give exactly 226000ms,
        // and "0:04:16.487" should give exactly 256487ms.
        let renderer_dur = DlnaOutput::parse_time("0:03:46");
        let track_dur = DlnaOutput::parse_time("0:04:16.487");
        assert_eq!(renderer_dur, 226_000);
        assert_eq!(track_dur, 256_487);
        let diff = (track_dur as i64 - renderer_dur as i64).unsigned_abs();
        assert!(diff > 2000, "difference should exceed gapless threshold");
    }

    #[test]
    fn format_time_roundtrip() {
        for ms in [0, 1000, 60_000, 225_000, 3_600_000, 86_399_000] {
            let formatted = DlnaOutput::format_time(ms);
            let parsed = DlnaOutput::parse_time(&formatted);
            assert_eq!(parsed, ms, "roundtrip failed for {ms}ms -> {formatted}");
        }
    }

    #[test]
    fn didl_item_id_alternates() {
        // Verify that consecutive tracks get different item IDs to prevent
        // Marantz ND8006 (and similar) from displaying cached metadata.
        let didl_1 = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/track1",
                mime_type: "audio/flac",
                title: Some("Track 1"),
                ..Default::default()
            },
            "1",
        );
        let didl_2 = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/track2",
                mime_type: "audio/flac",
                title: Some("Track 2"),
                ..Default::default()
            },
            "2",
        );
        assert!(didl_1.contains("id=\"1\""), "first track should have id=1");
        assert!(didl_2.contains("id=\"2\""), "second track should have id=2");
        assert!(didl_1.contains("Track 1"));
        assert!(didl_2.contains("Track 2"));
    }

    #[test]
    fn native_flac_next_track_didl_is_complete() {
        // #1132 (native FLAC): the gapless SetNextAVTransportURI DIDL must carry
        // the SAME full metadata as the initial SetAVTransportURI item — title,
        // artist, album, protocolInfo (format), duration AND a size that matches
        // the bytes the renderer will actually receive. A queued item missing
        // any of these makes the Marantz ND 8006 lose the format/duration/
        // progress display when it transitions to the next track. Both the
        // current-track and next-track paths build via `didl_metadata`, so a
        // single assertion covers the queued item too.
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/track2.flac",
                mime_type: "audio/flac",
                title: Some("So What"),
                artist: Some("Miles Davis"),
                album: Some("Kind of Blue"),
                duration_ms: Some(562_000),
                file_size: Some(50_000_000),
                sample_rate: Some(96_000),
                bit_depth: Some(24),
                channels: Some(2),
                ..Default::default()
            },
            "2",
        );
        assert!(didl.contains("So What"), "title present");
        assert!(didl.contains("Miles Davis"), "artist present");
        assert!(didl.contains("Kind of Blue"), "album present");
        assert!(didl.contains("audio/flac"), "format/protocolInfo present");
        assert!(didl.contains("DLNA.ORG_OP=01"), "DLNA flags present");
        assert!(
            didl.contains("duration=\"0:09:22.000\""),
            "duration present on the queued FLAC item"
        );
        assert!(
            didl.contains("size=\"50000000\""),
            "size present on the queued FLAC item"
        );
        assert!(didl.contains("sampleFrequency=\"96000\""));
        assert!(didl.contains("bitsPerSample=\"24\""));
    }
}
