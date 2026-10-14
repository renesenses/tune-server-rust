use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState};

/// Default HQPlayer Control API port (v4/v5).
pub const HQPLAYER_DEFAULT_PORT: u16 = 4321;
/// HQPlayer v6 default control port.
pub const HQPLAYER_V6_PORT: u16 = 8019;
/// Ports to try when auto-detecting HQPlayer.
pub const HQPLAYER_PROBE_PORTS: &[u16] = &[4321, 8019];

/// XML declaration that heads the command stream of a control connection.
///
/// It is written **once per connection**, before the first command — never
/// again (see [`frame_message`] and #4023).
const XML_HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;

/// A live control connection, and whether its XML declaration is already out.
///
/// HQPlayer parses everything a client writes on one control socket as a
/// *single* XML stream: the declaration belongs at its head, and a second
/// declaration in the middle is a fatal parse error for a streaming reader.
/// The flag is per-connection on purpose — a reconnect starts a brand new
/// stream, which needs the declaration again.
struct Control {
    stream: TcpStream,
    header_sent: bool,
}

/// Frame one command for the wire.
///
/// The XML declaration goes out only when the connection has not seen it yet.
/// Every command is newline-terminated so two consecutive commands stay
/// visibly separate in the stream.
fn frame_message(xml_body: &str, header_sent: bool) -> String {
    if header_sent {
        format!("{xml_body}\n")
    } else {
        format!("{XML_HEADER}\n{xml_body}\n")
    }
}

/// HQPlayer uses a custom TCP protocol with XML messages.
/// Commands are sent as XML fragments; responses are XML documents.
pub struct HqplayerOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    /// Persistent TCP connection to HQPlayer (reconnects on failure).
    connection: Arc<Mutex<Option<Control>>>,
    /// « J'ai déjà dit que je ne comprenais pas la réponse `Status`. »
    ///
    /// Le sondage tourne en boucle : sans ce garde, une réponse d'une forme
    /// inattendue écrirait une ligne toutes les secondes.
    etat_inconnu_dit: Arc<AtomicBool>,
    /// #4851 — la piste suivante que Tune a posée d'avance dans la playlist
    /// de HQPlayer, tant que HQPlayer n'est pas passé dessus.
    ///
    /// Un `SetNextAVTransportURI` DLNA REMPLACE la suivante ; un
    /// `PlaylistAdd clear="0"` l'AJOUTE. Sans ce suivi, un second armement
    /// (« Lire ensuite » dans la fenêtre, préparation expirée) laisserait la
    /// suivante périmée dans la playlist, jouée AVANT la bonne.
    enchainement: Arc<std::sync::Mutex<Enchainement>>,
    /// #6115 — le dernier état de transport que `Status` a rapporté.
    ///
    /// `play_media` le lit pour savoir si une piste joue (ou est en pause)
    /// au moment de lancer un AUTRE titre : il faut alors arrêter HQPlayer
    /// avant de vider sa playlist. Un HQPlayer jamais sondé vaut `None`, et
    /// l'enchaînement d'avant le correctif reste celui-là.
    dernier_etat: Arc<std::sync::Mutex<Option<TransportState>>>,
}

/// #6115 — la borne d'attente de l'arrêt avant de lancer un autre titre.
const ATTENTE_ARRET: std::time::Duration = std::time::Duration::from_secs(2);
/// #6115 — l'intervalle entre deux `Status` pendant cette attente.
const SONDE_ARRET: std::time::Duration = std::time::Duration::from_millis(100);

/// #4851 — ce que la sortie sait de la suivante posée chez HQPlayer.
#[derive(Debug, Default)]
struct Enchainement {
    /// L'URI ajoutée d'avance, que HQPlayer n'a pas encore commencé à jouer.
    suivante: Option<String>,
    /// La dernière position lue par `Status`, pour voir HQPlayer repartir de
    /// zéro sur la suivante quand il ne rapporte pas d'URI.
    position_ms: u64,
    /// La dernière durée lue par `Status`.
    duree_ms: u64,
}

/// #4851 — HQPlayer est-il passé de lui-même sur la suivante posée ?
///
/// Deux preuves, l'une ou l'autre :
/// - `Status` nomme l'URI de la suivante (`<metadata uri="…">`) ;
/// - la position, qui était dans les 35 dernières secondes de la piste,
///   retombe sous 5 s. C'est la forme de la transition quand HQPlayer ne
///   rapporte pas d'URI ; un recul manuel au début de la piste ne se fait
///   pas, en pratique, à moins de 35 s de sa fin.
fn passee_sur_la_suivante(
    suivante: &str,
    uri_en_cours: Option<&str>,
    position_avant_ms: u64,
    duree_avant_ms: u64,
    position_ms: u64,
) -> bool {
    if uri_en_cours.map(str::trim) == Some(suivante) {
        return true;
    }
    duree_avant_ms > 0
        && position_avant_ms + 35_000 >= duree_avant_ms
        && position_ms < 5_000
        && position_avant_ms > position_ms + 5_000
}

/// #4851 — l'index que HQPlayer donne LUI-MÊME à `uri` dans sa réponse
/// `PlaylistGet` (`<PlaylistItem index="…" uri="…"/>`).
///
/// L'index est relu, jamais calculé : le protocole ne dit pas s'il part de
/// 0 ou de 1, et `PlaylistRemove` attend celui de HQPlayer.
fn index_dans_la_playlist(xml: &str, uri: &str) -> Option<String> {
    use quick_xml::{Reader, events::Event};

    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event().ok()? {
            Event::Start(tag) | Event::Empty(tag)
                if tag.name().as_ref().eq_ignore_ascii_case(b"PlaylistItem") =>
            {
                let mut index = None;
                let mut meme_uri = false;
                for attribute in tag.attributes().flatten() {
                    let valeur = attribute
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .ok()?;
                    match attribute.key.as_ref() {
                        b"index" => index = Some(valeur.trim().to_string()),
                        b"uri" => meme_uri = valeur.trim() == uri,
                        _ => {}
                    }
                }
                if meme_uri {
                    return index;
                }
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

impl HqplayerOutput {
    pub fn new(name: String, device_id: String, host: String, port: u16) -> Self {
        Self {
            name,
            device_id,
            host,
            port,
            connection: Arc::new(Mutex::new(None)),
            etat_inconnu_dit: Arc::new(AtomicBool::new(false)),
            enchainement: Arc::new(std::sync::Mutex::new(Enchainement::default())),
            dernier_etat: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn dernier_etat(&self) -> std::sync::MutexGuard<'_, Option<TransportState>> {
        self.dernier_etat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// #6115 — arrête HQPlayer et attend que `Status` rapporte l'arrêt,
    /// au plus [`ATTENTE_ARRET`].
    ///
    /// Retour terrain (hifi25nl, rc4) : en pleine lecture, `PlaylistAdd
    /// clear="1"` puis `Play` laisse HQPlayer sur le titre précédent ; son
    /// intégration Audiolinux envoie `hqp-control2 --stop` avant `--play`.
    /// La source de HQPlayer Control 6.0.1 montre que `Stop` est une commande
    /// à part, acquittée (`stopComplete`), et que l'état `3` (STOPREQ) précède
    /// l'arrêt effectif (`0`) : d'où l'attente sur `Status`, pas seulement
    /// l'envoi. La borne dépassée n'est pas une erreur : le lancement suit,
    /// et le journal le dit.
    async fn arreter_et_attendre(&self) -> Result<(), String> {
        self.action("<Stop />").await?;
        let debut = tokio::time::Instant::now();
        let limite = debut + ATTENTE_ARRET;
        loop {
            let reste = limite.saturating_duration_since(tokio::time::Instant::now());
            if reste.is_zero() {
                break;
            }
            match tokio::time::timeout(reste, self.command(r#"<Status subscribe="0" />"#)).await {
                Ok(Ok(reponse)) => {
                    if etat_reconnu(&reponse) == Some(TransportState::Stopped) {
                        *self.dernier_etat() = Some(TransportState::Stopped);
                        info!(
                            device = %self.name,
                            attente_ms = debut.elapsed().as_millis() as u64,
                            "hqplayer_arret_avant_lancement"
                        );
                        return Ok(());
                    }
                }
                Ok(Err(e)) => {
                    warn!(device = %self.name, error = %e, "hqplayer_arret_status_illisible");
                    return Ok(());
                }
                Err(_) => {
                    // Réponse coupée en plein vol : la connexion ne doit pas
                    // garder un reste de `Status` pour la commande suivante.
                    *self.connection.lock().await = None;
                    break;
                }
            }
            tokio::time::sleep(
                SONDE_ARRET.min(limite.saturating_duration_since(tokio::time::Instant::now())),
            )
            .await;
        }
        warn!(
            device = %self.name,
            borne_ms = ATTENTE_ARRET.as_millis() as u64,
            "hqplayer_arret_non_confirme — lancement sans arret rapporte par Status"
        );
        Ok(())
    }

    fn enchainement(&self) -> std::sync::MutexGuard<'_, Enchainement> {
        self.enchainement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// #4851 — retire de la playlist de HQPlayer une suivante posée d'avance
    /// et devenue périmée. Son index est relu par `PlaylistGet`.
    ///
    /// Absente de la playlist, il n'y a rien à retirer. Une playlist
    /// illisible est une erreur : ajouter la nouvelle suivante par-dessus
    /// ferait jouer les deux.
    async fn retirer_la_suivante(&self, uri: &str) -> Result<(), String> {
        let playlist = self.command(r#"<PlaylistGet picture="0" />"#).await?;
        let Some(index) = index_dans_la_playlist(&playlist, uri) else {
            info!(device = %self.name, uri, "hqplayer_suivante_perimee_absente");
            return Ok(());
        };
        let xml = format!(r#"<PlaylistRemove index="{}" />"#, escape_xml(&index));
        self.action(&xml).await?;
        info!(device = %self.name, uri, index, "hqplayer_suivante_perimee_retiree");
        Ok(())
    }

    /// Probe a host to find which port HQPlayer is listening on.
    /// Tries each port with a TCP connect + GetInfo handshake.
    pub async fn probe_port(host: &str) -> Option<u16> {
        Self::probe_port_parmi(host, HQPLAYER_PROBE_PORTS).await
    }

    /// Comme [`probe_port`](Self::probe_port), mais sur la liste de ports
    /// donnée, dans l'ordre donné.
    ///
    /// Le sondeur s'en sert pour essayer le port **configuré** avant les deux
    /// ports standards : le panneau Services laisse saisir un port, et
    /// `probe_port` ne regardait que 4321 puis 8019 — un HQPlayer sur un
    /// autre port n'était jamais détecté, réglage renseigné.
    ///
    /// `hqplayer_port_detected` est en `debug!` et non en `info!` : ce sondage
    /// tourne toutes les 60 s et la ligne sortait à chaque tour pour dire que
    /// rien n'avait changé (#4025). Le port découvert est journalisé au
    /// niveau utile par l'appelant, quand il CHANGE quelque chose.
    pub async fn probe_port_parmi(host: &str, ports: &[u16]) -> Option<u16> {
        for &port in ports {
            match probe_hqplayer(host, port).await {
                Ok(true) => {
                    debug!(host, port, "hqplayer_port_detected");
                    return Some(port);
                }
                Ok(false) => {
                    debug!(host, port, "hqplayer_port_not_hqp");
                }
                Err(e) => {
                    debug!(host, port, error = %e, "hqplayer_port_probe_failed");
                }
            }
        }
        None
    }

    /// Les ports à essayer quand `configure` est le port enregistré dans les
    /// réglages : le sien d'abord, puis les standards qu'il ne double pas.
    pub fn ports_a_sonder(configure: u16) -> Vec<u16> {
        let mut ports = vec![configure];
        ports.extend(
            HQPLAYER_PROBE_PORTS
                .iter()
                .copied()
                .filter(|p| *p != configure),
        );
        ports
    }

    /// Get or establish a TCP connection to HQPlayer.
    async fn get_connection(&self) -> Result<(), String> {
        let mut conn = self.connection.lock().await;
        if conn.is_some() {
            return Ok(());
        }
        let addr = format!("{}:{}", self.host, self.port);
        let stream =
            tokio::time::timeout(std::time::Duration::from_secs(5), TcpStream::connect(&addr))
                .await
                .map_err(|_| format!("hqplayer connect timeout: {addr}"))?
                .map_err(|e| format!("hqplayer connect failed {addr}: {e}"))?;
        // Brand new stream: its XML declaration has not been written yet.
        *conn = Some(Control {
            stream,
            header_sent: false,
        });
        Ok(())
    }

    /// Write an XML command over the persistent connection (reconnecting once on a
    /// broken pipe), then apply `mode` to decide what happens after the write.
    ///
    /// This is the single place that owns the connection + one-reconnect logic, so
    /// both the blocking QUERY path (`send_command`) and the fire-and-forget ACTION
    /// path (`send_action`) share it — they differ only in `PostWrite`.
    async fn send_inner(&self, xml_body: &str, mode: PostWrite) -> Result<String, String> {
        let mut conn = self.connection.lock().await;

        // Try to use existing connection, reconnect if needed
        let control = match conn.as_mut() {
            Some(c) => c,
            None => {
                drop(conn);
                self.get_connection().await?;
                conn = self.connection.lock().await;
                conn.as_mut()
                    .ok_or_else(|| "hqplayer: no connection after reconnect".to_string())?
            }
        };

        // Framing depends on the connection we actually got: the declaration
        // heads a fresh stream and is never repeated on an established one.
        let message = frame_message(xml_body, control.header_sent);

        // Raw protocol logging: exact bytes we put on the wire. Cheap, debug-level.
        // Lets us learn v6 behavior from the field (v6 stays silent on actions).
        debug!(
            device = %self.name,
            bytes = message.len(),
            raw = %message.replace('\n', "\\n"),
            "hqplayer_send"
        );

        // Send the command
        if let Err(e) = control.stream.write_all(message.as_bytes()).await {
            // Connection broken, drop it and retry once
            *conn = None;
            drop(conn);
            self.get_connection().await?;
            let mut conn2 = self.connection.lock().await;
            let control2 = conn2
                .as_mut()
                .ok_or_else(|| "hqplayer: no connection after retry".to_string())?;
            // Fresh stream after the reconnect: it needs the declaration again,
            // so re-frame instead of replaying the bytes built for the old one.
            let retry = frame_message(xml_body, control2.header_sent);
            debug!(
                device = %self.name,
                bytes = retry.len(),
                raw = %retry.replace('\n', "\\n"),
                "hqplayer_send_retry"
            );
            control2
                .stream
                .write_all(retry.as_bytes())
                .await
                .map_err(|e2| format!("hqplayer write retry failed: {e}, then {e2}"))?;
            control2.header_sent = true;
            return post_write(&mut control2.stream, mode).await;
        }

        control.header_sent = true;
        post_write(&mut control.stream, mode).await
    }

    /// Send an XML QUERY command and receive the (complete-XML) response.
    /// Used for commands HQPlayer genuinely answers: `<GetInfo/>`, `<Status/>`.
    async fn send_command(&self, xml_body: &str) -> Result<String, String> {
        self.send_inner(xml_body, PostWrite::ReadResponse).await
    }

    /// Fire-and-forget send for ACTION/transport commands.
    ///
    /// HQPlayer v4/v5 acknowledge transport commands (PlaylistAdd/Play/Pause/Stop/
    /// Seek/Volume) with an XML reply; HQPlayer **6** accepts and executes them but
    /// stays SILENT. So we must NOT block on a full response — doing so hits the 5s
    /// `read_response` timeout on v6 and fails the whole play ("hqplayer read
    /// timeout"). Instead we write the command and do a very short, non-fatal drain:
    /// a v4/v5 ack is consumed (so it can't pollute the next `<Status/>` read), while
    /// v6 silence simply times out — which we treat as SUCCESS.
    async fn send_action(&self, xml_body: &str) -> Result<(), String> {
        let drained = self.send_inner(xml_body, PostWrite::DrainBrief).await?;
        // Field telemetry: did v4/v5 ack, or is this a silent v6?
        if drained.trim().is_empty() {
            debug!(device = %self.name, cmd = %xml_body, "hqplayer_action_sent no_reply");
        } else {
            debug!(
                device = %self.name,
                cmd = %xml_body,
                bytes_back = drained.len(),
                reply = %drained.trim(),
                "hqplayer_action_sent"
            );
        }
        Ok(())
    }

    /// Send a QUERY command, dropping connection on error (for next retry).
    async fn command(&self, xml_body: &str) -> Result<String, String> {
        match self.send_command(xml_body).await {
            Ok(response) => Ok(response),
            Err(e) => {
                // Drop connection so next call reconnects
                let mut conn = self.connection.lock().await;
                *conn = None;
                Err(e)
            }
        }
    }

    /// Send an ACTION command (fire-and-forget), dropping connection on a real
    /// write/socket error so the next call reconnects. A short-drain timeout is
    /// NOT an error here (that is the normal, expected v6 case).
    async fn action(&self, xml_body: &str) -> Result<(), String> {
        match self.send_action(xml_body).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let mut conn = self.connection.lock().await;
                *conn = None;
                Err(e)
            }
        }
    }
}

/// What to do on the connection after an XML command has been written.
#[derive(Clone, Copy)]
enum PostWrite {
    /// Block until a complete XML document arrives (QUERY commands answer).
    ReadResponse,
    /// Fire-and-forget: briefly drain + discard any ack; timeout == success
    /// (ACTION/transport commands; v6 stays silent).
    DrainBrief,
}

/// Apply the post-write behavior selected by `mode` to an established stream.
async fn post_write(stream: &mut TcpStream, mode: PostWrite) -> Result<String, String> {
    match mode {
        PostWrite::ReadResponse => read_response(stream).await,
        PostWrite::DrainBrief => drain_brief(stream).await,
    }
}

/// Fire-and-forget drain for ACTION/transport commands (see `send_action`).
///
/// Waits a short window for a *possible* v4/v5 ack and discards it; a v6 renderer
/// stays silent, so the first read simply times out and we return success with no
/// bytes. When an ack does arrive we keep draining with tiny follow-up reads so a
/// partial ack can never pollute the next `read_response` (e.g. a later
/// `<Status/>`). Returns whatever bytes were drained, for logging only.
async fn drain_brief(stream: &mut TcpStream) -> Result<String, String> {
    let mut buf = vec![0u8; 8192];
    let window = std::time::Duration::from_millis(400);

    // First peek: wait up to `window` for a v4/v5 ack. v6 silence -> timeout -> OK.
    let mut drained = match tokio::time::timeout(window, stream.read(&mut buf)).await {
        Ok(Ok(0)) => return Err("hqplayer: connection closed".to_string()),
        Ok(Ok(n)) => String::from_utf8_lossy(&buf[..n]).into_owned(),
        Ok(Err(e)) => return Err(format!("hqplayer read error: {e}")),
        Err(_) => return Ok(String::new()), // timeout == v6 silence == success
    };

    // An ack arrived (v4/v5): drain any remaining bytes with tiny non-blocking
    // follow-up reads so nothing is left in the socket for the next query.
    let mop = std::time::Duration::from_millis(50);
    loop {
        match tokio::time::timeout(mop, stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => drained.push_str(&String::from_utf8_lossy(&buf[..n])),
            Ok(Err(_)) => break,
            Err(_) => break, // no more data queued
        }
    }
    Ok(drained)
}

/// Read XML response from HQPlayer TCP stream.
/// HQPlayer sends responses terminated by a closing XML tag.
/// We read until we get a complete XML document or timeout.
async fn read_response(stream: &mut TcpStream) -> Result<String, String> {
    let mut buf = vec![0u8; 8192];
    let mut response = String::new();
    let timeout = std::time::Duration::from_secs(5);

    loop {
        let n = tokio::time::timeout(timeout, stream.read(&mut buf))
            .await
            .map_err(|_| "hqplayer read timeout".to_string())?
            .map_err(|e| format!("hqplayer read error: {e}"))?;

        if n == 0 {
            return Err("hqplayer: connection closed".to_string());
        }

        response.push_str(
            std::str::from_utf8(&buf[..n]).map_err(|e| format!("hqplayer: invalid utf8: {e}"))?,
        );

        // Check if we have a complete response (ends with a closing tag)
        let trimmed = response.trim();
        if is_complete_xml(trimmed) {
            break;
        }
    }

    Ok(response)
}

/// Heuristic: XML response is complete when it ends with a closing tag whose
/// name matches the first element opened in the document.
/// Falls back to self-closing tag detection for simple responses.
fn is_complete_xml(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Find the first real element (skip <?xml ...?> processing instruction)
    let search = trimmed;
    let mut pos = 0;
    let root_tag: Option<&str> = loop {
        match search[pos..].find('<') {
            None => break None,
            Some(offset) => {
                let start = pos + offset;
                if search[start..].starts_with("<?") {
                    // Processing instruction -- skip past "?>"
                    if let Some(end) = search[start..].find("?>") {
                        pos = start + end + 2;
                        continue;
                    }
                    break None;
                }
                // Extract tag name (stops at space, /, or >)
                let after = &search[start + 1..];
                let end = after
                    .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                    .unwrap_or(after.len());
                if end > 0 {
                    break Some(&after[..end]);
                }
                break None;
            }
        }
    };

    match root_tag {
        Some(tag) => {
            // Check for self-closing root element
            let open_tag_prefix = format!("<{}", tag);
            if let Some(open_pos) = trimmed.find(&open_tag_prefix) {
                let after_open = &trimmed[open_pos + open_tag_prefix.len()..];
                // Find the end of this opening tag
                if let Some(gt_pos) = after_open.find('>') {
                    let before_gt = after_open[..gt_pos].trim_end();
                    if before_gt.ends_with('/') {
                        // Self-closing root: <Tag ... />
                        // Complete only if this is at the end of the document
                        let tag_end = open_pos + open_tag_prefix.len() + gt_pos + 1;
                        return trimmed[tag_end..].trim().is_empty();
                    }
                }
            }

            // Check for matching closing tag at end
            let close = format!("</{}>", tag);
            trimmed.ends_with(&close)
        }
        None => {
            // No root tag found, check generic self-closing
            trimmed.ends_with("/>")
        }
    }
}

/// Probe whether a given host:port speaks the HQPlayer control protocol.
/// Sends `<GetInfo />` and checks for valid XML response.
async fn probe_hqplayer(host: &str, port: u16) -> Result<bool, String> {
    let addr = format!("{host}:{port}");
    let mut stream =
        tokio::time::timeout(std::time::Duration::from_secs(3), TcpStream::connect(&addr))
            .await
            .map_err(|_| format!("probe timeout: {addr}"))?
            .map_err(|e| format!("probe connect: {addr}: {e}"))?;

    // Throw-away connection, one command: it heads its own XML stream.
    let cmd = frame_message("<GetInfo />", false);
    stream
        .write_all(cmd.as_bytes())
        .await
        .map_err(|e| format!("probe write: {e}"))?;

    let response = read_response(&mut stream).await?;
    // A valid HQPlayer response contains XML with version/product info
    Ok(
        response.contains("HQPlayer")
            || response.contains("hqplayer")
            || response.contains("<Info"),
    )
}

/// L'état de transport **reconnu** dans une réponse `<Status>` de HQPlayer,
/// ou `None` quand son champ d'état n'est pas reconnu.
///
/// Séparé de [`parse_state_from_xml`] pour une raison précise : la valeur de
/// repli est `Stopped`, et `Stopped` n'est pas un état neutre pour le
/// sondeur. C'est **lui** qui déclenche l'avance de file après cinq sondes
/// (`STOPPED_TICKS_THRESHOLD`) et l'arrêt de zone après trente
/// (`STOPPED_FAILURE_THRESHOLD`). « Je n'ai pas compris la réponse » et « le
/// lecteur est à l'arrêt » ne peuvent donc pas rendre la même chose sans que
/// personne ne le sache : le repli reste, mais il est désormais **dit**.
fn etat_reconnu(xml: &str) -> Option<TransportState> {
    use quick_xml::{Reader, events::Event};

    fn state(value: &str) -> Option<TransportState> {
        match value.trim().to_ascii_lowercase().as_str() {
            "0" | "stopped" => Some(TransportState::Stopped),
            "1" | "paused" => Some(TransportState::Paused),
            "2" | "playing" => Some(TransportState::Playing),
            // Signalyst STATE_STOPREQ: stop requested, not yet stopped.
            // Wait for STATE_STOPPED before the poller considers a track end.
            "3" | "transitioning" | "buffering" => Some(TransportState::Transitioning),
            _ => None,
        }
    }

    // Read the actual Status state, not another attribute or track metadata.
    // The official control interface uses an integer attribute; retain the
    // textual attribute / direct State child accepted by older integrations.
    let mut reader = Reader::from_str(xml);
    let mut depth = 0;
    loop {
        let event = reader.read_event().ok()?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) if depth == 0 => {
                if !tag.name().as_ref().eq_ignore_ascii_case(b"Status") {
                    return None;
                }
                for attribute in tag.attributes() {
                    let attribute = attribute.ok()?;
                    if attribute.key.as_ref().eq_ignore_ascii_case(b"state") {
                        let value = attribute
                            .decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                reader.decoder(),
                            )
                            .ok()?;
                        return state(&value);
                    }
                }
                if empty {
                    return None;
                }
                depth = 1;
            }
            Event::Start(tag) => {
                if depth == 1 && tag.name().as_ref().eq_ignore_ascii_case(b"State") {
                    let value = reader.read_text(tag.name()).ok()?;
                    let decoded = value.decode().ok()?;
                    let value = quick_xml::escape::unescape(&decoded).ok()?;
                    return state(&value);
                }
                depth += 1;
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return None;
                }
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// Parse transport state from HQPlayer XML status response.
///
/// Le comportement ne change pas : une réponse non reconnue vaut `Stopped`.
/// Voir [`etat_reconnu`] pour ce que cela coûte, et `get_status` pour la
/// ligne qui le signale.
fn parse_state_from_xml(xml: &str) -> TransportState {
    etat_reconnu(xml).unwrap_or(TransportState::Stopped)
}

/// Extract an attribute value from XML by attribute name.
fn extract_xml_attr(xml: &str, attr_name: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr_name);
    if let Some(start) = xml.find(&pattern) {
        let after = &xml[start + pattern.len()..];
        if let Some(end) = after.find('"') {
            return Some(after[..end].to_string());
        }
    }
    // Also try single quotes
    let pattern_sq = format!("{}='", attr_name);
    if let Some(start) = xml.find(&pattern_sq) {
        let after = &xml[start + pattern_sq.len()..];
        if let Some(end) = after.find('\'') {
            return Some(after[..end].to_string());
        }
    }
    None
}

/// Extract text content between XML tags: `<tag>content</tag>`.
fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    if let Some(start_pos) = xml.find(&open) {
        let after_open = &xml[start_pos + open.len()..];
        // Skip to end of opening tag
        if let Some(gt) = after_open.find('>') {
            let content_start = &after_open[gt + 1..];
            if let Some(end_pos) = content_start.find(&close) {
                let text = content_start[..end_pos].trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

#[async_trait::async_trait]
impl OutputTarget for HqplayerOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "hqplayer"
    }

    fn capabilities(&self) -> OutputCapabilities {
        // #4851 — `can_gapless` : la suivante est posée d'avance dans la
        // playlist de HQPlayer (`set_next_media`), qui enchaîne lui-même.
        OutputCapabilities::v1(true, true, true, true, false, true).with_percent_volume()
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        info!(device = %self.name, url = media.url, "hqplayer_play");
        // #4851 — `clear="1"` vide la playlist : plus de suivante posée.
        self.enchainement().suivante = None;

        // #6115 — un AUTRE titre alors qu'une piste joue ou est en pause :
        // HQPlayer d'abord arrêté — `PlaylistClear` garde la piste qui JOUE
        // et un `Play` sur un moteur qui joue ne change rien (pont LMS, moteur
        // 6.0.4). L'enchaînement sans blanc (#4851) ne passe pas par ici
        // (`set_next_media`, `clear="0"`) et n'est pas touché.
        let etat = *self.dernier_etat();
        if matches!(
            etat,
            Some(TransportState::Playing | TransportState::Paused | TransportState::Transitioning)
        ) {
            self.arreter_et_attendre().await.map_err(|e| {
                warn!(error = %e, "hqplayer_stop_before_play_failed");
                e
            })?;
        }

        // ACTION commands go through the fire-and-forget path: HQPlayer 6 executes
        // transport commands but sends NO reply, so blocking on a response would
        // hit the 5s read timeout and fail the play. `action` writes and only
        // briefly drains any v4/v5 ack. See `send_action` for the full rationale.
        //
        // This is the only place that puts SEVERAL commands on one control
        // connection, which is why #4023 showed up here and nowhere else: the
        // second one must NOT be preceded by another XML declaration, or
        // HQPlayer's stream parser dies on it and `Play` is never executed.

        // #6115 (suite) — vider la playlist par un `PlaylistClear` EXPLICITE,
        // quel que soit l'état, arrêt compris (Piero, fil 2240 : « even if I
        // stop the previous one »). `clear="1"` seul ne suffit pas : le pont
        // LMS-HQPlayer de Simon Arnold l'a mesuré ignoré par le moteur 6.0.4
        // (`PlaylistAdd` répond OK et AJOUTE), la playlist devient
        // [précédent, nouveau] et `Play` repart sur l'entrée courante, le
        // précédent. Son chargement vérifié en direct est `Stop`,
        // `PlaylistClear`, `PlaylistAdd`, `Play`. `clear="1"` reste écrit :
        // c'est l'orthographe documentée, et il ne coûte rien.
        self.action("<PlaylistClear />").await.map_err(|e| {
            warn!(error = %e, "hqplayer_playlist_clear_failed");
            e
        })?;

        // Add URI to playlist (clear existing, start playing)
        let xml = format!(
            r#"<PlaylistAdd uri="{}" queued="0" clear="1"></PlaylistAdd>"#,
            escape_xml(media.url)
        );
        self.action(&xml).await.map_err(|e| {
            warn!(error = %e, "hqplayer_playlist_add_failed");
            e
        })?;

        // Issue play command
        self.action("<Play />").await.map_err(|e| {
            warn!(error = %e, "hqplayer_play_failed");
            e
        })?;

        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        // Fire-and-forget: v6 does not ack transport commands. See `send_action`.
        self.action("<Pause />").await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.action("<Play />").await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.action("<Stop />").await?;
        self.enchainement().suivante = None;
        info!(device = %self.name, "hqplayer_stop");
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let seconds = position_ms as f64 / 1000.0;
        let xml = format!(r#"<Seek position="{seconds:.1}" />"#);
        self.action(&xml).await?;
        Ok(())
    }

    /// #4851 — l'enchaînement par la file de HQPlayer : la suivante est
    /// AJOUTÉE à sa playlist pendant que la piste en cours joue, sans la
    /// vider (`clear="0"`) et sans `Play`. HQPlayer passe de lui-même à
    /// l'entrée suivante de sa playlist ; le sondeur le voit à la durée et à
    /// la position de `Status`, comme après un `SetNextAVTransportURI`.
    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let precedente = self.enchainement().suivante.clone();
        if let Some(precedente) = precedente {
            if precedente == media.url {
                debug!(device = %self.name, url = media.url, "hqplayer_suivante_deja_posee");
                return Ok(());
            }
            self.retirer_la_suivante(&precedente).await?;
            self.enchainement().suivante = None;
        }
        let xml = format!(
            r#"<PlaylistAdd uri="{}" queued="0" clear="0"></PlaylistAdd>"#,
            escape_xml(media.url)
        );
        self.action(&xml).await.map_err(|e| {
            warn!(error = %e, "hqplayer_suivante_add_failed");
            e
        })?;
        self.enchainement().suivante = Some(media.url.to_string());
        info!(device = %self.name, url = media.url, "hqplayer_suivante_posee");
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        // HQPlayer volume is a dB value or a linear scale depending on config.
        // The Volume command takes a value; we pass 0-100 linear.
        let level = (volume * 100.0).round().clamp(0.0, 100.0) as u32;
        let xml = format!(r#"<Volume value="{level}" />"#);
        // Fire-and-forget: v6 does not ack transport commands. See `send_action`.
        self.action(&xml).await?;
        Ok(())
    }

    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Err("mute not supported by the HQPlayer protocol".into())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let response = self.command(r#"<Status subscribe="0" />"#).await?;

        // #4023 — une réponse dont l'état n'est pas reconnu retombe sur
        // `Stopped`, et `Stopped` est ce qui fait avancer la file au bout de
        // cinq sondes puis arrêter la zone au bout de trente. Si un jour un
        // HQPlayer répond dans une forme que ce parseur ne couvre pas, le
        // symptôme est « l'album s'arrête après une piste » et RIEN dans le
        // journal ne le dit. Une ligne, UNE seule par sortie : ce sondage
        // tourne en boucle et #4025 vient justement de le faire taire.
        if etat_reconnu(&response).is_none() && !self.etat_inconnu_dit.swap(true, Ordering::Relaxed)
        {
            warn!(
                device = %self.name,
                reponse = %response.trim().chars().take(400).collect::<String>(),
                "hqplayer_status_etat_inconnu — aucun etat reconnu dans la reponse Status ; \
                 lue comme `stopped`, ce qui fait avancer la file puis arreter la zone"
            );
        }

        let state = parse_state_from_xml(&response);
        // #6115 — seul un état RECONNU est retenu : le repli `Stopped` d'une
        // réponse incomprise ne doit pas faire sauter l'arrêt de `play_media`.
        if let Some(reconnu) = etat_reconnu(&response) {
            *self.dernier_etat() = Some(reconnu);
        }
        let position = extract_xml_attr(&response, "position")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let duration = extract_xml_attr(&response, "duration")
            .or_else(|| extract_xml_attr(&response, "length"))
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let volume = extract_xml_attr(&response, "volume")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| v / 100.0)
            .unwrap_or(1.0);

        let track_title =
            extract_xml_text(&response, "Title").or_else(|| extract_xml_attr(&response, "title"));
        let track_artist =
            extract_xml_text(&response, "Artist").or_else(|| extract_xml_attr(&response, "artist"));
        let current_uri =
            extract_xml_text(&response, "Uri").or_else(|| extract_xml_attr(&response, "uri"));

        let position_ms = (position * 1000.0) as u64;
        let duration_ms = (duration * 1000.0) as u64;

        // #4851 — HQPlayer a-t-il commencé la suivante posée d'avance ? Elle
        // devient alors la piste en cours, et un armement ultérieur n'a plus
        // rien à retirer.
        {
            let mut suivi = self.enchainement();
            if let Some(suivante) = suivi.suivante.as_deref()
                && passee_sur_la_suivante(
                    suivante,
                    current_uri.as_deref(),
                    suivi.position_ms,
                    suivi.duree_ms,
                    position_ms,
                )
            {
                debug!(device = %self.name, uri = suivante, "hqplayer_suivante_commencee");
                suivi.suivante = None;
            }
            suivi.position_ms = position_ms;
            suivi.duree_ms = duration_ms;
        }

        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms,
            volume,
            muted: false,
            current_uri,
            track_title,
            track_artist,
            ended_naturally: false,
            // A renderer plays at 1x: keep the poller's wall-clock guards.
            realtime: true,
            // Aucune sortie hors la locale ne produit du DoP : le DSD y part
            // tel quel ou transcode, jamais empaquete dans du PCM 24 bits.
            dop_active: false,
        })
    }

    async fn is_available(&self) -> bool {
        probe_hqplayer(&self.host, self.port).await.unwrap_or(false)
    }
}

/// Escape special XML characters in a string.
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port() {
        assert_eq!(HQPLAYER_DEFAULT_PORT, 4321);
    }

    #[test]
    fn v6_port() {
        assert_eq!(HQPLAYER_V6_PORT, 8019);
    }

    #[test]
    fn output_type() {
        let hqp = HqplayerOutput::new("HQPlayer".into(), "hqp-1".into(), "localhost".into(), 4321);
        assert_eq!(hqp.output_type(), "hqplayer");
    }

    #[test]
    fn host_returned() {
        let hqp = HqplayerOutput::new(
            "HQPlayer".into(),
            "hqp-1".into(),
            "192.168.1.100".into(),
            4321,
        );
        assert_eq!(hqp.host(), Some("192.168.1.100"));
    }

    #[test]
    fn parse_state_playing() {
        let xml = r#"<Status state="playing" position="10.5" duration="300.0"/>"#;
        assert_eq!(parse_state_from_xml(xml), TransportState::Playing);
    }

    #[test]
    fn parse_state_paused() {
        let xml = r#"<Status state="paused" position="10.5"/>"#;
        assert_eq!(parse_state_from_xml(xml), TransportState::Paused);
    }

    #[test]
    fn parse_state_stopped() {
        let xml = r#"<Status state="stopped"/>"#;
        assert_eq!(parse_state_from_xml(xml), TransportState::Stopped);
    }

    #[test]
    fn extract_attr() {
        let xml = r#"<Status state="playing" position="42.5" duration="180.0" volume="75"/>"#;
        assert_eq!(extract_xml_attr(xml, "position"), Some("42.5".into()));
        assert_eq!(extract_xml_attr(xml, "duration"), Some("180.0".into()));
        assert_eq!(extract_xml_attr(xml, "volume"), Some("75".into()));
        assert_eq!(extract_xml_attr(xml, "missing"), None);
    }

    #[test]
    fn extract_text() {
        let xml = r#"<Status><Title>Test Song</Title><Artist>Test Artist</Artist></Status>"#;
        assert_eq!(extract_xml_text(xml, "Title"), Some("Test Song".into()));
        assert_eq!(extract_xml_text(xml, "Artist"), Some("Test Artist".into()));
    }

    #[test]
    fn escape_xml_chars() {
        assert_eq!(
            escape_xml(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn is_complete_self_closing() {
        assert!(is_complete_xml(r#"<Status state="stopped" />"#));
        assert!(is_complete_xml(
            r#"<?xml version="1.0"?><Info name="HQPlayer"/>"#
        ));
    }

    #[test]
    fn is_complete_closing_tag() {
        assert!(is_complete_xml("<Status><Title>x</Title></Status>"));
        assert!(is_complete_xml("<LibraryGet></LibraryGet>"));
    }

    #[test]
    fn is_not_complete() {
        assert!(!is_complete_xml("<Status"));
        assert!(!is_complete_xml(""));
        assert!(!is_complete_xml("<Status><Title>x</Title>"));
    }
}

#[cfg(test)]
#[path = "hqplayer_state_tests_4377.rs"]
mod state_tests_4377;

#[cfg(test)]
#[path = "hqplayer_enchainement_tests_4851.rs"]
pub(crate) mod enchainement_tests_4851;

#[cfg(test)]
#[path = "hqplayer_lancement_tests_6115.rs"]
mod lancement_tests_6115;
