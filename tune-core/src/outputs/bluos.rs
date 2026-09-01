use reqwest::Client;
use tracing::{debug, info, warn};

use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState};

pub struct BluosOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    client: Client,
}

impl BluosOutput {
    pub fn new(name: String, device_id: String, host: String, port: u16) -> Self {
        Self {
            name,
            device_id,
            host,
            port,
            client: crate::http::client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap(),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// L'URL `/Add` envoyee au Node, construite a UN SEUL endroit.
    ///
    /// `play_media` et `set_next_media` la fabriquaient chacun de leur cote,
    /// quinze lignes identiques au caractere pres. Ce sont deux soeurs du meme
    /// chemin : toute correction portee a l'une (l'encodage du parametre `url`
    /// est le candidat nomme par #1996) aurait laisse l'autre nue, et le defaut
    /// gapless ne se voit qu'a la fin du morceau — la ou il est le plus cher a
    /// diagnostiquer. Une seule construction, deux appelants.
    ///
    /// BluOS attend `url` SANS re-encodage : `.query()` doublerait l'encodage
    /// de `http://` dans l'URL de flux, et le Node echouerait en silence. Les
    /// autres parametres, eux, sont bien encodes.
    ///
    /// Le texte « en cours de lecture » du Node se pose via title1/title2/
    /// title3, PAS via title/artist/album : la BluOS Custom Integration API
    /// impose « title1, title2 and title3 MUST be used […] Do not use values
    /// such as album, artist and name ». Le Node ignore silencieusement les
    /// mauvais noms (Bilou, fil « Lecture BluOS »). D'ou title1=titre,
    /// title2=artiste, title3=album — les trois lignes qu'il relit dans son
    /// XML de statut (`<title1>…` a `get_status`).
    fn build_add_url(&self, media: &PlayMedia<'_>) -> String {
        let mut add_url = format!("{}/Add?url={}", self.base_url(), media.url);
        if let Some(t) = media.title {
            add_url.push_str(&format!("&title1={}", urlencoding::encode(t)));
        }
        if let Some(a) = media.artist {
            add_url.push_str(&format!("&title2={}", urlencoding::encode(a)));
        }
        if let Some(al) = media.album {
            add_url.push_str(&format!("&title3={}", urlencoding::encode(al)));
        }
        if let Some(img) = media.cover_url {
            add_url.push_str(&format!("&image={}", urlencoding::encode(img)));
        }
        add_url
    }

    async fn api_get(&self, path: &str, params: &[(&str, &str)]) -> Result<String, String> {
        let url = format!("{}/{}", self.base_url(), path);
        let resp = self
            .client
            .get(&url)
            .query(params)
            .send()
            .await
            .map_err(|e| format!("bluos {path}: {e}"))?;
        // The status was previously ignored: a Node answering 404/500 came back
        // as Ok(body), so play_media logged `bluos_play` and the orchestrator
        // logged `output_play_sent` while the Node had in fact refused the
        // command and never fetched the stream (Bilou, forum #1239 in 0.9.51 —
        // no stream_request at all after bluos_play). Surface it instead.
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("bluos read {path}: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "bluos {path}: HTTP {status} — {}",
                truncate_body(&body)
            ));
        }
        Ok(body)
    }
}

/// Valeur d'un attribut XML simple (`length="0"`) ou le texte d'une balise
/// (`<state>pause</state>`) dans la reponse du Node. Volontairement naif : les
/// reponses BluOS tiennent en une ligne et on ne veut pas d'un parseur XML
/// pour deux attributs.
fn xml_attr<'a>(body: &'a str, tag: &str, attr: &str) -> Option<&'a str> {
    let tag_pos = body.find(&format!("<{tag}"))?;
    let rest = &body[tag_pos..];
    let end = rest.find('>')?;
    let head = &rest[..end];
    let at = head.find(&format!("{attr}=\""))? + attr.len() + 2;
    let val = &head[at..];
    let close = val.find('"')?;
    Some(&val[..close])
}

fn xml_text<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let len = body[start..].find(&close)?;
    Some(body[start..start + len].trim())
}

/// Le Node annonce-t-il une file VIDE apres un `/Add` ?
///
/// `<playlist length="0" …>` : l'entree n'est pas entree en file. Une reponse
/// qu'on ne sait pas lire ne repond pas `true` — on ne signale que ce qu'on a
/// effectivement compris.
fn queue_stayed_empty(add_body: &str) -> bool {
    xml_attr(add_body, "playlist", "length") == Some("0")
}

/// Le Node a-t-il refuse la piste tout en repondant 200 ?
///
/// Vrai seulement quand les DEUX reponses concordent : la file annoncee par
/// `/Add` est vide ET `/Play` laisse l'appareil en pause. Un Node qui joue
/// annonce une file non vide et un etat `play` / `stream` ; un Node dont on ne
/// sait pas lire le dialecte ne produit ni l'un ni l'autre et passe donc pour
/// fonctionnel — c'est le sens de defaut qu'on veut, on ne casse personne.
fn add_play_rejected(add_body: &str, play_body: &str) -> bool {
    let queue_empty = xml_attr(add_body, "playlist", "length") == Some("0");
    let still_paused = xml_text(play_body, "state") == Some("pause");
    queue_empty && still_paused
}

/// Keep a Node reply short enough to log without flooding the journal.
fn truncate_body(body: &str) -> String {
    let one_line = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= 300 {
        one_line
    } else {
        let cut: String = one_line.chars().take(300).collect();
        format!("{cut}…")
    }
}

#[async_trait::async_trait]
impl OutputTarget for BluosOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "bluos"
    }

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, true)
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        // Clear the Node's internal play queue before starting a new track.
        // Without this, tracks from a previous album stayed queued and the Node
        // auto-advanced onto them at every track transition (Scordia: a new CD
        // plays track 1, then jumps to the previous CD's tracks — "in memory"
        // yet absent from Tune's own queue/history, because they live on the
        // Node). Fire-and-forget: a failed Clear must not block playback.
        let _ = self.api_get("Clear", &[]).await;

        // Play THROUGH the Node's queue (/Add then /Play?id=0), not as a
        // /Play?url= custom stream. The custom stream lives OUTSIDE the queue:
        // on Bilou's Node (0.9.49 log) the gapless /Add?prepend=1 entry was
        // never fetched at end of track — the Node just stopped, the poller saw
        // stopped/pos=0 for ~25 s and killed the zone (stopped_early_waiting →
        // bluos_stop). Queue entries also render their title1/2/3 lines on the
        // Node display, where the custom-stream play showed the title only.
        //
        // Construction partagee avec `set_next_media` : cf `build_add_url`.
        let add_url = self.build_add_url(media);
        let add_resp = self
            .client
            .get(&add_url)
            .send()
            .await
            .map_err(|e| format!("bluos Add: {e}"))?;
        let add_status = add_resp.status();
        let add_body = add_resp
            .text()
            .await
            .map_err(|e| format!("bluos Add read: {e}"))?;
        if !add_status.is_success() {
            // Meme angle mort que #1874, sur l'autre facon de refuser : on
            // savait que le Node avait dit non, jamais A QUOI. Le refus par
            // code HTTP n'ecrivait rien du tout dans le journal — l'`Err` remonte
            // a l'utilisateur, pas au diagnostic.
            warn!(
                device = %self.name,
                add_url = %add_url,
                status = %add_status,
                reply = %truncate_body(&add_body),
                "bluos_add_http_error"
            );
            return Err(format!(
                "bluos Add: HTTP {add_status} — {}",
                truncate_body(&add_body)
            ));
        }
        // Both replies used to be discarded, which is why a Node that refused
        // the queue entry looked identical in the journal to one that accepted
        // it. The Add reply also carries the id the Node actually assigned —
        // the `id=0` below is an assumption we have never verified against a
        // real device, and it is the prime suspect for #1239.
        info!(device = %self.name, reply = %truncate_body(&add_body), "bluos_add_reply");
        // Start the queue at its (single, freshly added) first entry.
        let play_body = self.api_get("Play", &[("id", "0")]).await?;
        info!(device = %self.name, reply = %truncate_body(&play_body), "bluos_play_reply");
        // Le Node a repondu 200 aux deux appels, et n'a rien fait.
        //
        // Bilou, 12/08/2026, Node en 0.9.68 : trois tentatives, trois fois
        //   Add  -> <playlist length="0" id="1761">   (rien n'est entre en file)
        //   Play -> <state>pause</state>              (Play sur une file vide)
        // Tune declarait `output_sent=true` a chaque fois, parce qu'il ne
        // regardait que le statut HTTP. Cote utilisateur : la position avance,
        // aucun son, et le poller finit par tuer la zone au bout des 45 s de
        // grace — deux fils forum ouverts sur un materiel qui n'a rien.
        //
        // On exige les DEUX signaux avant de conclure a l'echec. Chacun pris
        // seul pourrait mentir sur un Node dont on ne connait pas le dialecte ;
        // ensemble — file vide ET reste en pause — ils ne laissent pas de place
        // au doute, et un Node qui joue vraiment n'en produit aucun des deux.
        if add_play_rejected(&add_body, &play_body) {
            warn!(
                device = %self.name,
                // L'URL envoyee manquait au journal, et c'est elle qui manque
                // pour diagnostiquer. Sans elle on sait que le Node a refuse,
                // jamais CE QU'IL a refuse : une adresse injoignable depuis le
                // lecteur, un caractere qui casse le decoupage des parametres,
                // une pochette trop longue… Trois allers-retours avec Bilou
                // (17/08/2026) se sont arretes faute de cette ligne.
                add_url = %add_url,
                add_reply = %truncate_body(&add_body),
                play_reply = %truncate_body(&play_body),
                "bluos_add_rejected_empty_queue"
            );
            return Err(format!(
                "Le lecteur BluOS « {} » a accepte la commande mais n'a rien mis en file : sa file est restee vide et il est reste en pause. Rien ne sera joue.",
                self.name
            ));
        }
        info!(
            device = %self.name,
            url = media.url,
            title = media.title.unwrap_or(""),
            artist = media.artist.unwrap_or(""),
            album = media.album.unwrap_or(""),
            image = media.cover_url.unwrap_or(""),
            "bluos_play"
        );
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.api_get("Pause", &[]).await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.api_get("Play", &[]).await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.api_get("Stop", &[]).await?;
        info!(device = %self.name, "bluos_stop");
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let seconds = (position_ms / 1000).to_string();
        self.api_get("Play", &[("seek", &seconds)]).await?;
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let level = (volume * 100.0).round().clamp(0.0, 100.0) as u32;
        self.api_get("Volume", &[("level", &level.to_string())])
            .await?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let val = if muted { "on" } else { "off" };
        self.api_get("Volume", &[("mute", val)]).await?;
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let xml = self.api_get("Status", &[]).await?;

        let state = match extract_tag(&xml, "state")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "play" | "stream" => TransportState::Playing,
            "pause" => TransportState::Paused,
            "connecting" | "buffering" => TransportState::Transitioning,
            _ => TransportState::Stopped,
        };

        let position_ms = extract_tag(&xml, "secs")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| (s * 1000.0) as u64)
            .unwrap_or(0);

        let duration_ms = extract_tag(&xml, "totlen")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| (s * 1000.0) as u64)
            .unwrap_or(0);

        let volume = extract_tag(&xml, "volume")
            .and_then(|v| v.parse::<f64>().ok())
            .map(|v| v / 100.0)
            .unwrap_or(0.5);

        let muted = extract_tag(&xml, "mute")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false);

        let current_uri = extract_tag(&xml, "streamUrl").or_else(|| extract_tag(&xml, "song"));

        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms,
            volume,
            muted,
            current_uri,
            track_title: extract_tag(&xml, "title1"),
            track_artist: extract_tag(&xml, "artist"),
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
            .get(format!("{}/Status", self.base_url()))
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .is_ok()
    }

    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        // Append the next track to the Node's queue (plain /Add, NO prepend=1).
        // All-in-queue model: play_media now puts the current track IN the
        // Node's queue (/Add + /Play?id=0), so the gapless next track must be
        // appended AFTER it — prepend=1 would insert it BEFORE the current
        // entry and it would never be reached. The old model (current track as
        // a /Play?url= custom stream + /Add?prepend=1) froze gapless entirely:
        // on Bilou's Node (0.9.49 log, 05/08) the prepended entry was never
        // fetched at end of track — no stream_request, Node stopped at pos=0
        // for ~25 s until the poller killed the zone (stopped_early_waiting →
        // bluos_stop).
        // Exactement la meme URL que `play_media` — meme constructeur, y compris
        // le title1/title2/title3 (le Node ignore title/artist/album), pour que
        // la piste preparee en gapless porte elle aussi son texte et pas
        // seulement sa pochette. Cf `build_add_url`.
        let add_url = self.build_add_url(media);
        // La reponse etait jetee en entier — statut ET corps. Un Node qui
        // repondait 404, ou qui acceptait l'appel sans rien mettre en file,
        // etait indiscernable d'un Node qui a bien pris la piste suivante.
        //
        // C'est le meme angle mort que `play_media` avant #1514, mais sur le
        // chemin gapless, ou il est PLUS couteux a diagnostiquer : le defaut
        // ne se voit qu'a la fin du morceau en cours, et se lit comme « le
        // Node s'arrete entre les pistes » plutot que comme un refus.
        //
        // On ne peut pas appliquer ici la regle des deux signaux de #1514 :
        // `set_next_media` n'envoie pas de `Play`, donc il n'y a pas d'etat de
        // transport a confronter. On journalise donc la file annoncee, et on
        // avertit quand elle est vide — sans faire echouer l'appel : une
        // preparation gapless ratee doit degrader vers une transition normale,
        // pas interrompre la lecture en cours.
        let add_resp = self
            .client
            .get(&add_url)
            .send()
            .await
            .map_err(|e| format!("bluos Add: {e}"))?;
        let add_status = add_resp.status();
        let add_body = add_resp.text().await.unwrap_or_default();
        if !add_status.is_success() {
            warn!(
                device = %self.name,
                add_url = %add_url,
                status = %add_status,
                reply = %truncate_body(&add_body),
                "bluos_add_http_error_gapless"
            );
            return Err(format!(
                "bluos Add (gapless): HTTP {add_status} — {}",
                truncate_body(&add_body)
            ));
        }
        if queue_stayed_empty(&add_body) {
            // `add_url` manquait ici, et NULLE PART ailleurs sur ce chemin.
            //
            // #1874 posait le probleme en une phrase — « l'URL envoyee n'est
            // journalisee que sur le chemin qui reussit » — et la PR #1870 l'a
            // reglee pour `play_media` seulement. La soeur gapless est restee
            // nue : on y savait que le Node avait laisse sa file vide, jamais
            // sur QUELLE URL. C'est precisement le champ qui a permis d'ecarter
            // l'hypothese d'encodage sur l'autre chemin (#1996) ; sans lui, le
            // meme diagnostic est impossible ici.
            warn!(
                device = %self.name,
                add_url = %add_url,
                reply = %truncate_body(&add_body),
                "bluos_set_next_queue_still_empty"
            );
        } else {
            debug!(device = %self.name, reply = %truncate_body(&add_body), "bluos_set_next_reply");
        }
        info!(
            device = %self.name,
            url = media.url,
            title = media.title.unwrap_or(""),
            artist = media.artist.unwrap_or(""),
            album = media.album.unwrap_or(""),
            image = media.cover_url.unwrap_or(""),
            "bluos_set_next"
        );
        Ok(())
    }
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let text = xml[start..end].trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_xml() {
        let xml = r#"<status><state>play</state><secs>123.4</secs><totlen>300.0</totlen><volume>50</volume><title1>Test Song</title1><artist>Test Artist</artist></status>"#;
        assert_eq!(extract_tag(xml, "state"), Some("play".into()));
        assert_eq!(extract_tag(xml, "secs"), Some("123.4".into()));
        assert_eq!(extract_tag(xml, "volume"), Some("50".into()));
        assert_eq!(extract_tag(xml, "title1"), Some("Test Song".into()));
    }

    #[test]
    fn parse_empty_tags() {
        let xml = "<status><state>stop</state><secs></secs></status>";
        assert_eq!(extract_tag(xml, "state"), Some("stop".into()));
        assert_eq!(extract_tag(xml, "secs"), None);
    }

    #[test]
    fn truncate_body_collapses_whitespace() {
        assert_eq!(
            truncate_body("  <addsong\n  id=\"1\"/>  "),
            "<addsong id=\"1\"/>"
        );
    }

    #[test]
    fn truncate_body_caps_long_replies() {
        let out = truncate_body(&"x".repeat(500));
        assert_eq!(out.chars().count(), 301);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn parse_mute_status() {
        let xml = "<status><mute>on</mute><volume>42</volume></status>";
        let muted = extract_tag(xml, "mute")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false);
        assert!(muted);
    }

    // ── Le Node repond 200 et ne joue rien (Bilou, 12/08/2026) ──────────
    //
    // Trois tentatives, trois fois la meme paire de reponses : file vide et
    // etat pause. Tune declarait `output_sent=true` a chaque fois.

    const ADD_EMPTY: &str = r#"<?xml version="1.0" encoding="UTF-8"?> <playlist length="0" id="1761" shuffle="0" repeat="2"></playlist>"#;
    const ADD_OK: &str = r#"<?xml version="1.0" encoding="UTF-8"?> <playlist length="1" id="1762" shuffle="0" repeat="2"></playlist>"#;
    const PLAY_PAUSE: &str = r#"<?xml version="1.0" encoding="UTF-8"?> <state>pause</state>"#;
    const PLAY_STREAM: &str = r#"<?xml version="1.0" encoding="UTF-8"?> <state>stream</state>"#;

    #[test]
    fn file_vide_et_pause_est_un_refus() {
        assert!(add_play_rejected(ADD_EMPTY, PLAY_PAUSE));
    }

    #[test]
    fn file_remplie_qui_joue_passe() {
        assert!(!add_play_rejected(ADD_OK, PLAY_STREAM));
    }

    #[test]
    fn file_remplie_mais_en_pause_ne_declenche_rien() {
        // Un Node peut rester une fraction de seconde en pause apres un Play
        // reussi. Le signal seul ne suffit pas — il faut aussi la file vide.
        assert!(!add_play_rejected(ADD_OK, PLAY_PAUSE));
    }

    #[test]
    fn file_vide_mais_qui_joue_ne_declenche_rien() {
        assert!(!add_play_rejected(ADD_EMPTY, PLAY_STREAM));
    }

    #[test]
    fn dialecte_inconnu_passe_pour_fonctionnel() {
        // Sens de defaut : un Node dont on ne sait pas lire les reponses ne
        // doit jamais etre declare en panne sur notre ignorance.
        assert!(!add_play_rejected("<ok/>", "<ok/>"));
        assert!(!add_play_rejected("", ""));
    }

    #[test]
    fn xml_attr_et_xml_text_lisent_les_reponses_reelles() {
        assert_eq!(xml_attr(ADD_EMPTY, "playlist", "length"), Some("0"));
        assert_eq!(xml_attr(ADD_OK, "playlist", "length"), Some("1"));
        assert_eq!(xml_attr(ADD_OK, "playlist", "absent"), None);
        assert_eq!(xml_text(PLAY_PAUSE, "state"), Some("pause"));
        assert_eq!(xml_text(PLAY_STREAM, "state"), Some("stream"));
        assert_eq!(xml_text("<state>pause", "state"), None);
    }

    // ── Chemin gapless : la reponse du Node n'etait pas lue du tout ────────
    //
    // Meme angle mort que `play_media` avant #1514, mais plus couteux a
    // diagnostiquer : le defaut ne se voit qu'a la fin du morceau en cours et
    // se lit comme « le Node s'arrete entre les pistes ».

    #[test]
    fn file_vide_apres_add_gapless_est_signalee() {
        assert!(queue_stayed_empty(ADD_EMPTY));
    }

    #[test]
    fn file_remplie_ne_signale_rien() {
        assert!(!queue_stayed_empty(ADD_OK));
    }

    #[test]
    fn reponse_illisible_ne_signale_rien() {
        // On ne signale que ce qu'on a effectivement compris : un Node dont on
        // ne connait pas le dialecte ne doit pas remplir le journal.
        assert!(!queue_stayed_empty("<ok/>"));
        assert!(!queue_stayed_empty(""));
    }

    // ── L'URL envoyee au Node : une construction, deux chemins de refus ────
    //
    // #1874 : « l'URL envoyee n'est journalisee que sur le chemin qui reussit ».
    // La PR #1870 a corrige `play_media` ; `set_next_media` est restee nue, et
    // aucun des deux refus par code HTTP n'ecrivait quoi que ce soit.

    /// Le flux exact du journal de Bilou du 20/08/2026 (#1996), champ par champ.
    /// C'est la seule URL `/Add` du dossier dont on ait la trace ecrite.
    const FLUX_BILOU: &str =
        "http://192.168.1.12:8888/stream/968625a7-3a25-48a1-a86a-b962ce981046.flac";
    const POCHETTE_BILOU: &str =
        "http://192.168.1.12:8888/api/v1/library/artwork/8050fd92adf127e5743911262ca65407";

    fn media_bilou(url: &str) -> PlayMedia<'_> {
        PlayMedia {
            url,
            mime_type: "audio/flac",
            title: Some("Come on In"),
            artist: Some("Bridge City Sinners"),
            album: Some("Bridge City Sinners"),
            cover_url: Some(POCHETTE_BILOU),
            ..Default::default()
        }
    }

    #[test]
    fn l_url_add_reproduit_celle_du_journal_de_bilou() {
        // `url` part BRUT (le re-encoder casserait `http://` cote Node) tandis
        // que title1/2/3 et image sont encodes. L'asymetrie est deliberee ;
        // elle est ici figee sur la seule trace ecrite qu'on ait du terrain.
        let node = BluosOutput::new(
            "Salon".into(),
            "bluos-192.168.1.23-11000".into(),
            "192.168.1.23".into(),
            11000,
        );
        assert_eq!(
            node.build_add_url(&media_bilou(FLUX_BILOU)),
            format!(
                "http://192.168.1.23:11000/Add?url={FLUX_BILOU}\
                 &title1=Come%20on%20In\
                 &title2=Bridge%20City%20Sinners\
                 &title3=Bridge%20City%20Sinners\
                 &image=http%3A%2F%2F192.168.1.12%3A8888%2Fapi%2Fv1%2Flibrary%2Fartwork%2F8050fd92adf127e5743911262ca65407"
            )
        );
    }

    #[test]
    fn un_media_sans_metadonnees_n_ajoute_aucun_parametre_vide() {
        let node = BluosOutput::new("Salon".into(), "d".into(), "192.168.1.23".into(), 11000);
        let nu = PlayMedia {
            url: FLUX_BILOU,
            mime_type: "audio/flac",
            ..Default::default()
        };
        assert_eq!(
            node.build_add_url(&nu),
            format!("http://192.168.1.23:11000/Add?url={FLUX_BILOU}")
        );
    }

    // ── Banc d'essai : un Node bouchonne, en local ────────────────────────
    //
    // Aucun trafic ne sort de la machine. Le bouchon LIT la requete avant de
    // repondre (axum s'en charge) — un mock qui ferme sans lire provoque un RST
    // qui detruit la reponse en vol.

    #[derive(Clone, Default)]
    struct JournalCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl JournalCapture {
        fn texte(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for JournalCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
        type Writer = JournalCapture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[derive(Clone)]
    struct NodeBouchon {
        /// Les URI `/Add` recues, path + query, telles quelles.
        recues: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        code_add: axum::http::StatusCode,
        corps_add: String,
    }

    async fn add_bouchon(
        axum::extract::State(etat): axum::extract::State<NodeBouchon>,
        uri: axum::http::Uri,
    ) -> (axum::http::StatusCode, String) {
        etat.recues.lock().unwrap().push(uri.to_string());
        (etat.code_add, etat.corps_add.clone())
    }

    async fn demarrer_bouchon(etat: NodeBouchon) -> (u16, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new()
            .route("/Add", axum::routing::get(add_bouchon))
            .route(
                "/Clear",
                axum::routing::get(|| async { ADD_EMPTY.to_string() }),
            )
            .route(
                "/Play",
                axum::routing::get(|| async { PLAY_PAUSE.to_string() }),
            )
            .with_state(etat);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (port, handle)
    }

    #[tokio::test]
    async fn le_refus_gapless_nomme_l_url_envoyee_comme_le_fait_play_media() {
        // LA contre-epreuve de #1874 sur la soeur oubliee par #1870.
        //
        // Le Node accepte l'appel gapless et laisse sa file vide — exactement la
        // reponse du terrain. L'avertissement doit dire CE QU'IL a refuse, pas
        // seulement qu'il a refuse. Sans le champ `add_url`, ce test tombe.
        let recues = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (port, tache) = demarrer_bouchon(NodeBouchon {
            recues: recues.clone(),
            code_add: axum::http::StatusCode::OK,
            corps_add: ADD_EMPTY.to_string(),
        })
        .await;
        let node = BluosOutput::new("Salon".into(), "d".into(), "127.0.0.1".into(), port);

        let journal = JournalCapture::default();
        let abonne = tracing_subscriber::fmt()
            .with_writer(journal.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let media = media_bilou(FLUX_BILOU);
        let attendu = node.build_add_url(&media);
        // `set_default` et non `with_default` : la closure de `with_default` ne
        // peut pas `await`, et un `block_on` imbrique figerait l'executeur
        // mono-thread du test avec le bouchon dedans.
        let garde = tracing::subscriber::set_default(abonne);
        let issue = node.set_next_media(&media).await;
        drop(garde);
        tache.abort();

        // Une preparation gapless ratee degrade vers une transition normale :
        // elle ne doit PAS interrompre la lecture en cours.
        assert!(issue.is_ok(), "{issue:?}");

        let log = journal.texte();
        assert!(log.contains("bluos_set_next_queue_still_empty"), "{log}");
        assert!(
            log.contains("add_url="),
            "le refus gapless doit porter l'URL envoyee : {log}"
        );
        assert!(
            log.contains("968625a7-3a25-48a1-a86a-b962ce981046.flac"),
            "l'URL de flux doit etre lisible dans le journal : {log}"
        );
        assert!(
            log.contains("title1=Come%20on%20In"),
            "les parametres envoyes doivent etre lisibles : {log}"
        );

        // Et le chemin gapless envoie EXACTEMENT ce que `build_add_url` fabrique
        // — c'est ce qui empeche les deux soeurs de diverger a nouveau.
        let recues = recues.lock().unwrap().clone();
        assert_eq!(recues.len(), 1, "{recues:?}");
        assert!(
            attendu.ends_with(&recues[0]),
            "envoye={} attendu={attendu}",
            recues[0]
        );
    }

    #[tokio::test]
    async fn un_add_refuse_par_code_http_nomme_aussi_l_url_envoyee() {
        // L'autre facon de refuser : le Node repond 404. Rien n'etait journalise
        // du tout — l'`Err` remonte a l'utilisateur, jamais au diagnostic.
        let recues = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (port, tache) = demarrer_bouchon(NodeBouchon {
            recues,
            code_add: axum::http::StatusCode::NOT_FOUND,
            corps_add: "<nothing/>".to_string(),
        })
        .await;
        let node = BluosOutput::new("Salon".into(), "d".into(), "127.0.0.1".into(), port);

        let journal = JournalCapture::default();
        let abonne = tracing_subscriber::fmt()
            .with_writer(journal.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let media = media_bilou(FLUX_BILOU);
        let garde = tracing::subscriber::set_default(abonne);
        let issue = node.play_media(&media).await;
        drop(garde);
        tache.abort();

        assert!(issue.is_err(), "un Add en 404 doit remonter une erreur");
        let log = journal.texte();
        assert!(log.contains("bluos_add_http_error"), "{log}");
        assert!(
            log.contains("968625a7-3a25-48a1-a86a-b962ce981046.flac"),
            "le refus par code HTTP doit nommer l'URL envoyee : {log}"
        );
    }
}
