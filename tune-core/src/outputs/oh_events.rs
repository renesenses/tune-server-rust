//! Client GENA (abonnement aux évènements UPnP) partagé par les deux dialectes
//! que Tune parle sur le réseau.
//!
//! Le transport est le même pour tous : un `SUBSCRIBE` HTTP vers l'`eventSubURL`
//! du service, un serveur HTTP local qui reçoit les `NOTIFY`, un renouvellement
//! avant expiration. Seule la CHARGE diffère :
//!
//! * **OpenHome** (`urn:av-openhome-org:*`) publie des propriétés PLATES —
//!   `<TransportState>Playing</TransportState>` — directement dans le
//!   `propertyset`.
//! * **UPnP AV / DLNA** (`urn:schemas-upnp-org:service:AVTransport:1`,
//!   `RenderingControl:1`) publie UNE seule propriété, `LastChange`, dont le
//!   texte est un second document XML échappé
//!   (`<Event><InstanceID val="0"><TransportState val="PLAYING"/>…`).
//!
//! [`EventState::apply_properties`] aplatit le second cas avant d'appliquer,
//! si bien que les deux dialectes finissent dans le même état. Un renderer
//! OpenHome n'émet JAMAIS `LastChange` : le chemin ajouté pour DLNA est
//! strictement additif et ne change aucun verdict OpenHome (#2263).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use reqwest::Client;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::traits::TransportState;

const SUBSCRIBE_TIMEOUT: &str = "Second-300";
const RENEW_INTERVAL_SECS: u64 = 250;
const EVENT_STALE_SECS: u64 = 10;

static SUB_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_path_id() -> String {
    format!("oh{}", SUB_COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[derive(Debug, Default)]
pub struct EventState {
    pub transport_state: Option<TransportState>,
    pub volume: Option<u32>,
    pub muted: Option<bool>,
    pub track_uri: Option<String>,
    /// Durée de la piste courante en ms, poussée par `CurrentTrackDuration`
    /// dans le `LastChange` d'AVTransport. Jamais renseignée par OpenHome, qui
    /// tient la durée dans son service `Time`, interrogé en SOAP.
    pub duration_ms: Option<u64>,
    /// Position poussée par le renderer (`RelativeTimePosition`), et l'instant
    /// où elle est arrivée.
    ///
    /// **Rare.** La position n'est pas une variable évènementielle obligatoire
    /// d'`AVTransport:1` : la plupart des renderers ne l'émettent jamais. C'est
    /// exactement pour cela que le mode par défaut continue de lire la position
    /// en SOAP, et que le mode « silence » doit l'extrapoler. Quand elle arrive
    /// tout de même, elle re-cale l'extrapolation gratuitement.
    pub position_ms: Option<u64>,
    pub position_at: Option<Instant>,
    pub track_title: Option<String>,
    pub track_artist: Option<String>,
    pub last_update: Option<Instant>,
    /// L'abonnement GENA qui nourrit cet état est-il encore tenu ?
    ///
    /// Posé à `true` par un `SUBSCRIBE` accepté, remis à `false` dès qu'un
    /// renouvellement est refusé ou n'aboutit pas. C'est le seul repli FRANC :
    /// [`EventState::is_fresh`] ne convient qu'aux services qui poussent en
    /// continu (le `Time` d'OpenHome, une propriété par seconde). Un
    /// `AVTransport` DLNA n'émet QUE sur changement — au bout de dix secondes
    /// de lecture paisible il n'a plus rien à dire, et `is_fresh` le
    /// déclarerait mort à tort.
    pub alive: bool,
}

impl EventState {
    /// Un évènement est arrivé il y a moins de [`EVENT_STALE_SECS`].
    ///
    /// Critère des services qui poussent EN CONTINU (OpenHome `Time`). Ne
    /// convient pas à DLNA : voir [`EventState::is_live`].
    pub fn is_fresh(&self) -> bool {
        self.last_update
            .map(|t| t.elapsed().as_secs() < EVENT_STALE_SECS)
            .unwrap_or(false)
    }

    /// L'abonnement est tenu ET le renderer a déjà livré au moins un état.
    ///
    /// Critère des services qui n'émettent QUE sur changement (AVTransport,
    /// RenderingControl). GENA impose au renderer d'envoyer l'état complet
    /// juste après le `SUBSCRIBE` : tant que `last_update` est vide, l'appareil
    /// a accepté l'abonnement sans jamais rien dire — on ne lui fait pas
    /// confiance et on sonde.
    pub fn is_live(&self) -> bool {
        self.alive && self.last_update.is_some()
    }

    fn apply_properties(&mut self, props: &HashMap<String, String>) {
        // UPnP AV emballe tout dans `LastChange`, un document XML échappé DANS
        // le texte de la propriété. On l'aplatit d'abord ; les clés obtenues
        // (`TransportState`, `Volume`, `Mute`…) rejoignent alors le même
        // traitement que les propriétés plates d'OpenHome.
        let last_change = props.get("LastChange").map(|xml| parse_last_change(xml));
        let lu = |cle: &str| -> Option<&String> {
            last_change
                .as_ref()
                .and_then(|m| m.get(cle))
                .or_else(|| props.get(cle))
        };

        if let Some(state) = lu("TransportState") {
            // OpenHome dit « Playing »/« Paused »/« Buffering » ; DLNA dit
            // « PLAYING »/« PAUSED_PLAYBACK »/« TRANSITIONING »/« STOPPED »/
            // « NO_MEDIA_PRESENT ». La comparaison insensible à la casse rend
            // les MÊMES verdicts qu'avant sur les trois graphies OpenHome ;
            // les graphies ajoutées, OpenHome ne les émet pas.
            let s = state.trim();
            self.transport_state = Some(if s.eq_ignore_ascii_case("Playing") {
                TransportState::Playing
            } else if s.eq_ignore_ascii_case("Paused")
                || s.eq_ignore_ascii_case("PAUSED_PLAYBACK")
                || s.eq_ignore_ascii_case("PAUSED_RECORDING")
            {
                TransportState::Paused
            } else if s.eq_ignore_ascii_case("Buffering") || s.eq_ignore_ascii_case("TRANSITIONING")
            {
                TransportState::Transitioning
            } else {
                TransportState::Stopped
            });
        }

        if let Some(vol) = lu("Volume").and_then(|v| v.parse().ok()) {
            self.volume = Some(vol);
        }

        if let Some(mute) = lu("Mute") {
            self.muted = Some(mute == "1" || mute.eq_ignore_ascii_case("true"));
        }

        // `Uri` = OpenHome (service Info) ; `CurrentTrackURI` = DLNA. On ne
        // retient `AVTransportURI` qu'à défaut : sur un renderer à liste de
        // lecture il désigne le conteneur, pas la piste en cours.
        if let Some(uri) = lu("Uri")
            .or_else(|| lu("CurrentTrackURI"))
            .or_else(|| lu("AVTransportURI"))
            .filter(|u| !u.is_empty())
        {
            self.track_uri = Some(uri.clone());
        }

        // « NOT_IMPLEMENTED » est la réponse normalisée d'un renderer qui ne
        // connaît pas sa durée : `parse_time` en fait 0, et 0 signifie « durée
        // inconnue » partout ailleurs dans Tune. On n'écrase donc pas une durée
        // déjà connue avec ce 0.
        if let Some(d) =
            lu("CurrentTrackDuration").map(|v| crate::outputs::dlna::parse_upnp_time(v))
            && d > 0
        {
            self.duration_ms = Some(d);
        }

        if let Some(p) = lu("RelativeTimePosition")
            .filter(|v| v.contains(':'))
            .map(|v| crate::outputs::dlna::parse_upnp_time(v))
        {
            self.position_ms = Some(p);
            self.position_at = Some(Instant::now());
        }

        if let Some(meta) = lu("CurrentTrackMetaData").filter(|m| !m.is_empty()) {
            // Le DIDL arrive lui aussi échappé DANS l'attribut `val` ; le
            // parseur du `LastChange` l'a déjà déséchappé une fois.
            if let Some(t) = extraire_balise(meta, "dc:title") {
                self.track_title = Some(t);
            }
            if let Some(a) =
                extraire_balise(meta, "dc:creator").or_else(|| extraire_balise(meta, "upnp:artist"))
            {
                self.track_artist = Some(a);
            }
        }

        self.last_update = Some(Instant::now());
    }
}

/// Extrait le texte de la première `<balise>…</balise>` trouvée.
///
/// Le DIDL des évènements est court et sert à l'affichage : un scan de chaînes
/// suffit, comme le fait déjà `dlna::extract_tag` sur les réponses SOAP.
fn extraire_balise(xml: &str, tag: &str) -> Option<String> {
    let ouvrante = format!("<{tag}");
    let fermante = format!("</{tag}>");
    let debut = xml.find(&ouvrante)?;
    let apres_attributs = xml[debut..].find('>')? + debut + 1;
    let fin = xml[apres_attributs..].find(&fermante)? + apres_attributs;
    let texte = xml[apres_attributs..fin].trim();
    if texte.is_empty() {
        return None;
    }
    Some(match unescape(texte) {
        Ok(s) => s.to_string(),
        Err(_) => texte.to_string(),
    })
}

/// Aplatit un document `LastChange` UPnP AV en couples `nom → val`.
///
/// Forme attendue, une fois le texte de la propriété déséchappé :
///
/// ```xml
/// <Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/">
///   <InstanceID val="0">
///     <TransportState val="PLAYING"/>
///     <CurrentTrackDuration val="0:03:45"/>
///   </InstanceID>
/// </Event>
/// ```
///
/// Deux règles portent de la correction, pas du confort :
///
/// * **Instance 0 seulement.** Un renderer multi-instances (rare, mais la
///   spec l'autorise) décrirait plusieurs flux dans le même document ; mélanger
///   leurs états rendrait un transport qui n'est celui de personne.
/// * **Voie `Master` seulement.** `RenderingControl` émet un `<Volume>` PAR
///   voie (`Master`, `LF`, `RF`). Prendre la dernière rencontrée ferait sauter
///   le volume affiché d'un canal à l'autre à chaque évènement.
pub fn parse_last_change(xml: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    // Aucune balise `InstanceID` rencontrée = document sans enveloppe : on
    // prend tout, plutôt que de rendre un état vide.
    let mut instance_courante: Option<String> = None;

    loop {
        let event = reader.read_event_into(&mut buf);
        let (tag, attrs) = match event {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                let mut val: Option<String> = None;
                let mut channel: Option<String> = None;
                for attr in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(attr.key.local_name().as_ref()).to_string();
                    // Même déséchappement que le texte des propriétés : les
                    // `val` d'un `LastChange` portent régulièrement du DIDL
                    // entier, donc des `&lt;` en pagaille.
                    let brut = String::from_utf8_lossy(&attr.value);
                    let value = match unescape(&brut) {
                        Ok(v) => v.to_string(),
                        Err(_) => brut.to_string(),
                    };
                    match key.as_str() {
                        "val" => val = Some(value),
                        "channel" => channel = Some(value),
                        _ => {}
                    }
                }
                (tag, (val, channel))
            }
            Ok(Event::End(ref e)) => {
                if String::from_utf8_lossy(e.local_name().as_ref()) == "InstanceID" {
                    instance_courante = None;
                }
                buf.clear();
                continue;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {
                buf.clear();
                continue;
            }
        };
        let (val, channel) = attrs;

        if tag == "InstanceID" {
            instance_courante = Some(val.unwrap_or_default());
        } else if tag != "Event"
            && let Some(val) = val
            && instance_courante
                .as_deref()
                .map(|i| i == "0")
                .unwrap_or(true)
            && channel
                .as_deref()
                .map(|c| c.eq_ignore_ascii_case("Master"))
                .unwrap_or(true)
        {
            result.insert(tag, val);
        }
        buf.clear();
    }

    result
}

/// Récepteur GENA partagé : un seul port HTTP local pour tous les abonnements,
/// quel que soit le dialecte (OpenHome ou UPnP AV / DLNA).
pub struct UpnpEventListener {
    port: u16,
    server_ip: String,
    client: Client,
    handlers: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<EventState>>>>>,
    subscriptions: Arc<RwLock<HashMap<String, String>>>,
}

/// Nom historique, conservé pour les consommateurs hors arbre : le récepteur
/// n'a jamais rien eu de spécifiquement OpenHome (#2263).
pub type OpenHomeEventListener = UpnpEventListener;

impl UpnpEventListener {
    pub async fn new(server_ip: String) -> Result<Self, String> {
        let listener = match TcpListener::bind(("0.0.0.0", 8890)).await {
            Ok(l) => l,
            Err(_) => TcpListener::bind(("0.0.0.0", 0))
                .await
                .map_err(|e| format!("bind oh_events: {e}"))?,
        };
        let port = listener
            .local_addr()
            .map_err(|e| format!("local addr: {e}"))?
            .port();

        let handlers: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<EventState>>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let subscriptions: Arc<RwLock<HashMap<String, String>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let client = crate::http::client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| format!("client: {e}"))?;

        // HTTP NOTIFY receiver
        let h = handlers.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let h = h.clone();
                        tokio::spawn(handle_notify(stream, h));
                    }
                    Err(e) => warn!(error = %e, "oh_event_accept_error"),
                }
            }
        });

        // Subscription renewal loop
        let subs = subscriptions.clone();
        let h2 = handlers.clone();
        let ip = server_ip.clone();
        let rc = client.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(RENEW_INTERVAL_SECS));
            loop {
                interval.tick().await;
                renew_all(&rc, &subs, &h2, &ip, port).await;
            }
        });

        info!(port, "oh_event_listener_started");

        Ok(Self {
            port,
            server_ip,
            client,
            handlers,
            subscriptions,
        })
    }

    fn callback_base(&self) -> String {
        format!("http://{}:{}", self.server_ip, self.port)
    }

    pub async fn subscribe(
        &self,
        event_sub_url: &str,
        state: Arc<tokio::sync::Mutex<EventState>>,
    ) -> Option<String> {
        let path_id = next_path_id();
        let callback_url = format!("{}/oh-event/{}", self.callback_base(), path_id);

        let method = reqwest::Method::from_bytes(b"SUBSCRIBE").ok()?;

        // Le gestionnaire est posé AVANT d'émettre le `SUBSCRIBE`, pas après.
        //
        // GENA veut que le renderer envoie l'état complet aussitôt qu'il a
        // accepté l'abonnement, et rien ne l'oblige à attendre que NOTRE client
        // ait fini de lire sa réponse : sur un réseau rapide — ou en boucle
        // locale — le premier `NOTIFY` arrive pendant ce `send()`. Enregistré
        // après, il tombait sur une table qui ne connaissait pas encore le
        // chemin de rappel, et l'état initial partait à la poubelle en silence.
        // Le service AVTransport n'en émet pas d'autre tant que rien ne change :
        // l'abonnement restait alors muet pour toujours.
        self.handlers
            .write()
            .await
            .insert(path_id.clone(), state.clone());

        let accepte = match self
            .client
            .request(method, event_sub_url)
            .header("CALLBACK", format!("<{callback_url}>"))
            .header("NT", "upnp:event")
            .header("TIMEOUT", SUBSCRIBE_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => true,
            Ok(r) => {
                debug!(url = event_sub_url, status = %r.status(), "oh_subscribe_rejected");
                false
            }
            Err(e) => {
                debug!(url = event_sub_url, error = %e, "oh_subscribe_failed");
                false
            }
        };

        if !accepte {
            // Abonnement refusé : on retire le gestionnaire posé d'avance,
            // sinon un `NOTIFY` égaré nourrirait un état que personne ne
            // renouvelle.
            self.handlers.write().await.remove(&path_id);
            return None;
        }

        // L'abonnement est tenu. Le drapeau ne remonte QUE par ici : un
        // renouvellement réussi ne ressuscite pas un abonnement déjà déclaré
        // mort, c'est la sortie qui doit se réabonner. Volontaire — une sortie
        // qui partage un état entre plusieurs services (AVTransport +
        // RenderingControl, ou les cinq services OpenHome) doit retomber sur le
        // sondage dès qu'UN seul de ses abonnements lâche, jamais sur un état
        // à moitié nourri.
        state.lock().await.alive = true;

        self.subscriptions
            .write()
            .await
            .insert(path_id.clone(), event_sub_url.to_string());

        debug!(url = event_sub_url, path_id = %path_id, "oh_subscribed");
        Some(path_id)
    }

    /// Passe un tour de renouvellement TOUT DE SUITE, au lieu d'attendre le
    /// prochain réveil de la boucle.
    ///
    /// C'est la boucle elle-même mise à disposition : elle appelle exactement
    /// cette fonction toutes les [`RENEW_INTERVAL_SECS`] secondes. Sans ce
    /// point d'entrée, vérifier qu'un renouvellement refusé coupe bien
    /// l'abonnement demanderait d'attendre quatre minutes — donc ne serait
    /// jamais vérifié.
    pub async fn renouveler_maintenant(&self) {
        renew_all(
            &self.client,
            &self.subscriptions,
            &self.handlers,
            &self.server_ip,
            self.port,
        )
        .await;
    }

    pub async fn unsubscribe(&self, path_id: &str) {
        if let Some(state) = self.handlers.write().await.remove(path_id) {
            state.lock().await.alive = false;
        }
        if let Some(event_url) = self.subscriptions.write().await.remove(path_id)
            && let Ok(method) = reqwest::Method::from_bytes(b"UNSUBSCRIBE")
        {
            let _ = self
                .client
                .request(method, &event_url)
                .header("SID", path_id)
                .send()
                .await;
        }
    }
}

async fn handle_notify(
    stream: tokio::net::TcpStream,
    handlers: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<EventState>>>>>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);

    let mut request_line = String::new();
    if buf_reader.read_line(&mut request_line).await.is_err() {
        return;
    }

    let path_id = request_line
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.strip_prefix("/oh-event/"))
        .map(|s| s.to_string());

    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if buf_reader.read_line(&mut line).await.is_err() || line.trim().is_empty() {
            break;
        }
        if line.to_ascii_lowercase().starts_with("content-length:") {
            content_length = line
                .split(':')
                .nth(1)
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 && buf_reader.read_exact(&mut body).await.is_err() {
        let _ = writer.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        return;
    }

    let _ = writer
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
        .await;

    if let Some(path_id) = path_id {
        let body_str = String::from_utf8_lossy(&body);
        let properties = parse_propertyset(&body_str);
        if !properties.is_empty() {
            let handlers = handlers.read().await;
            if let Some(state) = handlers.get(&path_id) {
                state.lock().await.apply_properties(&properties);
                debug!(path_id = %path_id, props = ?properties.keys().collect::<Vec<_>>(), "oh_event_applied");
            }
        }
    }
}

async fn renew_all(
    client: &Client,
    subscriptions: &Arc<RwLock<HashMap<String, String>>>,
    handlers: &Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<EventState>>>>>,
    server_ip: &str,
    port: u16,
) {
    let subs = subscriptions.read().await.clone();
    if subs.is_empty() {
        return;
    }
    let Ok(method) = reqwest::Method::from_bytes(b"SUBSCRIBE") else {
        return;
    };
    let mut perdus = 0usize;
    for (path_id, event_url) in &subs {
        let callback = format!("http://{}:{}/oh-event/{}", server_ip, port, path_id);
        let result = client
            .request(method.clone(), event_url)
            .header("CALLBACK", format!("<{callback}>"))
            .header("NT", "upnp:event")
            .header("TIMEOUT", SUBSCRIBE_TIMEOUT)
            .send()
            .await;
        // Un renouvellement REFUSÉ est un abonnement perdu, au même titre
        // qu'une requête qui n'aboutit pas : l'ancien code ne regardait que
        // l'erreur de transport, et un 412 « Precondition Failed » (SID
        // inconnu du renderer après son redémarrage) passait pour un succès —
        // l'état gelé continuait alors d'être servi comme s'il était vivant.
        let tenu = match result {
            Ok(resp) if resp.status().is_success() => true,
            Ok(resp) => {
                debug!(url = event_url, status = %resp.status(), "oh_renew_rejected");
                false
            }
            Err(e) => {
                debug!(url = event_url, error = %e, "oh_renew_failed");
                false
            }
        };
        if !tenu {
            perdus += 1;
            if let Some(state) = handlers.read().await.get(path_id) {
                state.lock().await.alive = false;
            }
        }
    }
    debug!(count = subs.len(), perdus, "oh_subscriptions_renewed");
}

/// Aplatit un `propertyset` GENA en couples `nom → texte`.
///
/// Le texte d'une propriété est ACCUMULÉ, jamais pris au dernier morceau.
/// quick-xml ne rend pas une suite de caractères contenant des entités comme un
/// seul `Text` : chaque `&lt;`, `&amp;`, `&quot;` sort en `GeneralRef` distinct
/// et coupe le texte en tranches. L'ancienne écriture posait chaque tranche
/// dans la table à la place de la précédente, et ne gardait donc que la
/// dernière — une propriété sans entité (`<TransportState>Playing</>`) passait,
/// tandis qu'un `LastChange` DLNA, qui n'est QUE des entités, se réduisait à
/// son dernier fragment. Le même piège attendait OpenHome sur la première URI
/// contenant un `&`.
fn parse_propertyset(xml: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut in_property = false;
    let mut current_tag = String::new();
    let mut accumule = String::new();

    // Referme la propriété en cours : c'est ici, et seulement ici, que le
    // texte complet entre dans la table.
    fn poser(
        result: &mut HashMap<String, String>,
        current_tag: &mut String,
        accumule: &mut String,
    ) {
        if !current_tag.is_empty() {
            let texte = accumule.trim();
            if !texte.is_empty() {
                result.insert(current_tag.clone(), texte.to_string());
            }
        }
        current_tag.clear();
        accumule.clear();
    }

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if tag == "property" {
                    in_property = true;
                } else if in_property {
                    poser(&mut result, &mut current_tag, &mut accumule);
                    current_tag = tag;
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if tag == "property" {
                    poser(&mut result, &mut current_tag, &mut accumule);
                    in_property = false;
                } else if in_property && tag == current_tag {
                    poser(&mut result, &mut current_tag, &mut accumule);
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_property
                    && !current_tag.is_empty()
                    && let Ok(decoded) = e.decode()
                {
                    match unescape(&decoded) {
                        Ok(s) => accumule.push_str(&s),
                        Err(_) => accumule.push_str(&decoded),
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if in_property && !current_tag.is_empty() {
                    let nom = String::from_utf8_lossy(e.as_ref()).to_string();
                    // `&#38;` / `&#x26;` d'abord, puis les cinq entités que XML
                    // définit sans DTD. Une entité inconnue est recopiée telle
                    // qu'elle est écrite : perdre un caractère vaut mieux que
                    // perdre le document, et aucune ne nous intéresse.
                    if let Some(c) = e.resolve_char_ref().ok().flatten() {
                        accumule.push(c);
                    } else {
                        accumule.push_str(match nom.as_str() {
                            "lt" => "<",
                            "gt" => ">",
                            "amp" => "&",
                            "apos" => "'",
                            "quot" => "\"",
                            _ => "",
                        });
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_propertyset_basic() {
        let xml = r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
  <e:property>
    <TransportState>Playing</TransportState>
  </e:property>
  <e:property>
    <Volume>42</Volume>
  </e:property>
</e:propertyset>"#;
        let props = parse_propertyset(xml);
        assert_eq!(props.get("TransportState"), Some(&"Playing".to_string()));
        assert_eq!(props.get("Volume"), Some(&"42".to_string()));
    }

    #[test]
    fn parse_propertyset_mute_and_uri() {
        let xml = r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
  <e:property><Mute>true</Mute></e:property>
  <e:property><Uri>http://example.com/track.flac</Uri></e:property>
</e:propertyset>"#;
        let props = parse_propertyset(xml);
        assert_eq!(props.get("Mute"), Some(&"true".to_string()));
        assert_eq!(
            props.get("Uri"),
            Some(&"http://example.com/track.flac".to_string())
        );
    }

    #[test]
    fn parse_propertyset_empty() {
        let props = parse_propertyset("<e:propertyset></e:propertyset>");
        assert!(props.is_empty());
    }

    #[test]
    fn event_state_freshness() {
        let mut state = EventState::default();
        assert!(!state.is_fresh());
        state.last_update = Some(Instant::now());
        assert!(state.is_fresh());
    }

    #[test]
    fn event_state_apply() {
        let mut state = EventState::default();
        let mut props = HashMap::new();
        props.insert("TransportState".to_string(), "Playing".to_string());
        props.insert("Volume".to_string(), "75".to_string());
        props.insert("Mute".to_string(), "0".to_string());
        state.apply_properties(&props);
        assert_eq!(state.transport_state, Some(TransportState::Playing));
        assert_eq!(state.volume, Some(75));
        assert_eq!(state.muted, Some(false));
        assert!(state.is_fresh());
    }

    #[test]
    fn event_state_apply_uri() {
        let mut state = EventState::default();
        let mut props = HashMap::new();
        props.insert("Uri".to_string(), "http://10.0.0.1/stream.flac".to_string());
        state.apply_properties(&props);
        assert_eq!(
            state.track_uri,
            Some("http://10.0.0.1/stream.flac".to_string())
        );
    }

    /// Le `LastChange` d'AVTransport tel qu'un renderer l'envoie, une fois le
    /// texte de la propriété déséchappé.
    const LAST_CHANGE_AVT: &str = r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0"><TransportState val="PLAYING"/><CurrentTrackURI val="http://10.0.0.5/x.flac"/><CurrentTrackDuration val="0:04:16"/></InstanceID></Event>"#;

    #[test]
    fn last_change_avtransport_est_aplati() {
        let m = parse_last_change(LAST_CHANGE_AVT);
        assert_eq!(m.get("TransportState"), Some(&"PLAYING".to_string()));
        assert_eq!(
            m.get("CurrentTrackURI"),
            Some(&"http://10.0.0.5/x.flac".to_string())
        );
        assert_eq!(m.get("CurrentTrackDuration"), Some(&"0:04:16".to_string()));
        // L'enveloppe n'est pas une propriété.
        assert!(!m.contains_key("InstanceID"));
        assert!(!m.contains_key("Event"));
    }

    /// `RenderingControl` émet un `<Volume>` PAR VOIE. Sans filtre, la dernière
    /// rencontrée l'emporte — ici `RF` à 99 — et le volume affiché sauterait
    /// d'un canal à l'autre à chaque évènement.
    #[test]
    fn seule_la_voie_master_est_retenue() {
        let xml = r#"<Event><InstanceID val="0"><Volume channel="Master" val="42"/><Volume channel="LF" val="11"/><Volume channel="RF" val="99"/><Mute channel="Master" val="1"/></InstanceID></Event>"#;
        let m = parse_last_change(xml);
        assert_eq!(m.get("Volume"), Some(&"42".to_string()));
        assert_eq!(m.get("Mute"), Some(&"1".to_string()));
    }

    /// Un document multi-instances ne doit pas mélanger deux flux.
    #[test]
    fn seule_l_instance_zero_est_retenue() {
        let xml = r#"<Event><InstanceID val="0"><TransportState val="PLAYING"/></InstanceID><InstanceID val="1"><TransportState val="STOPPED"/></InstanceID></Event>"#;
        assert_eq!(
            parse_last_change(xml).get("TransportState"),
            Some(&"PLAYING".to_string())
        );
    }

    /// Les graphies DLNA arrivent à bon port…
    #[test]
    fn les_graphies_dlna_du_transport_sont_comprises() {
        for (brut, attendu) in [
            ("PLAYING", TransportState::Playing),
            ("PAUSED_PLAYBACK", TransportState::Paused),
            ("PAUSED_RECORDING", TransportState::Paused),
            ("TRANSITIONING", TransportState::Transitioning),
            ("STOPPED", TransportState::Stopped),
            ("NO_MEDIA_PRESENT", TransportState::Stopped),
        ] {
            let mut st = EventState::default();
            let mut props = HashMap::new();
            props.insert(
                "LastChange".to_string(),
                format!(r#"<Event><InstanceID val="0"><TransportState val="{brut}"/></InstanceID></Event>"#),
            );
            st.apply_properties(&props);
            assert_eq!(st.transport_state, Some(attendu), "graphie {brut}");
        }
    }

    /// …sans rien changer aux verdicts OpenHome, qui passaient déjà.
    ///
    /// C'est la garde du « on ne change la vérité de personne » : les trois
    /// graphies plates d'OpenHome, et le fait que l'inconnu vaut arrêté.
    #[test]
    fn les_verdicts_openhome_sont_inchanges() {
        for (brut, attendu) in [
            ("Playing", TransportState::Playing),
            ("Paused", TransportState::Paused),
            ("Buffering", TransportState::Transitioning),
            ("Nimportequoi", TransportState::Stopped),
        ] {
            let mut st = EventState::default();
            let mut props = HashMap::new();
            props.insert("TransportState".to_string(), brut.to_string());
            st.apply_properties(&props);
            assert_eq!(st.transport_state, Some(attendu), "graphie {brut}");
        }
    }

    #[test]
    fn le_last_change_nourrit_duree_uri_et_metadonnees() {
        let didl = r#"&lt;DIDL-Lite&gt;&lt;item&gt;&lt;dc:title&gt;Andante&lt;/dc:title&gt;&lt;dc:creator&gt;Lisa Jacobs&lt;/dc:creator&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;"#;
        let mut props = HashMap::new();
        props.insert(
            "LastChange".to_string(),
            format!(
                r#"<Event><InstanceID val="0"><CurrentTrackDuration val="0:04:16"/><CurrentTrackURI val="http://h/x.flac"/><CurrentTrackMetaData val="{didl}"/></InstanceID></Event>"#
            ),
        );
        let mut st = EventState::default();
        st.apply_properties(&props);
        assert_eq!(st.duration_ms, Some(256_000));
        assert_eq!(st.track_uri.as_deref(), Some("http://h/x.flac"));
        assert_eq!(st.track_title.as_deref(), Some("Andante"));
        assert_eq!(st.track_artist.as_deref(), Some("Lisa Jacobs"));
    }

    /// `NOT_IMPLEMENTED` ne doit pas écraser une durée déjà connue par un zéro
    /// qui, partout ailleurs dans Tune, veut dire « durée inconnue ».
    #[test]
    fn une_duree_non_implementee_n_efface_pas_celle_qu_on_a() {
        let mut st = EventState::default();
        let mut props = HashMap::new();
        props.insert(
            "LastChange".to_string(),
            r#"<Event><InstanceID val="0"><CurrentTrackDuration val="0:04:16"/></InstanceID></Event>"#.to_string(),
        );
        st.apply_properties(&props);
        props.insert(
            "LastChange".to_string(),
            r#"<Event><InstanceID val="0"><CurrentTrackDuration val="NOT_IMPLEMENTED"/></InstanceID></Event>"#.to_string(),
        );
        st.apply_properties(&props);
        assert_eq!(st.duration_ms, Some(256_000));
    }

    /// LE point de bascule du dossier.
    ///
    /// `AVTransport` n'émet QUE sur changement : dix secondes de lecture
    /// paisible et il n'a plus rien dit. Juger sa fraîcheur comme celle du
    /// service `Time` d'OpenHome — qui pousse une propriété par seconde — le
    /// déclarerait mort à tort, et le chemin DLNA retomberait sur ses trois
    /// actions par seconde en pure perte.
    #[test]
    fn un_abonnement_silencieux_reste_vivant_meme_quand_il_n_est_plus_frais() {
        let mut st = EventState::default();
        st.alive = true;
        st.last_update = Some(Instant::now() - std::time::Duration::from_secs(120));
        assert!(!st.is_fresh(), "deux minutes sans évènement : plus frais");
        assert!(st.is_live(), "…mais toujours abonné, donc digne de foi");

        // Le repli, lui, est franc.
        st.alive = false;
        assert!(
            !st.is_live(),
            "abonnement perdu : on retombe sur le sondage"
        );
    }

    /// Un abonnement accepté par un appareil qui n'a JAMAIS rien poussé ne
    /// vaut rien : on ne sert pas un état vide comme une vérité.
    #[test]
    fn un_abonnement_sans_le_moindre_evenement_ne_vaut_pas_vivant() {
        let mut st = EventState::default();
        st.alive = true;
        assert!(!st.is_live());
    }
}
