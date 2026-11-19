use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::traits::{AnnonceSuivante, OutputStatus, PlayMedia, TransportState};

const PREBUFFER_THRESHOLD_MS: u64 = 15_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum GaplessState {
    Idle,
    Monitoring,
    Ready,
}

pub struct GaplessHandler {
    state: Mutex<GaplessState>,
    next_url_set: Mutex<bool>,
    enabled: Mutex<bool>,
    prebuffer_threshold_ms: u64,
}

impl GaplessHandler {
    pub fn new(enabled: bool) -> Self {
        Self {
            state: Mutex::new(GaplessState::Idle),
            next_url_set: Mutex::new(false),
            enabled: Mutex::new(enabled),
            prebuffer_threshold_ms: PREBUFFER_THRESHOLD_MS,
        }
    }

    pub async fn set_enabled(&self, enabled: bool) {
        *self.enabled.lock().await = enabled;
        if !enabled {
            self.reset().await;
        }
    }

    pub async fn is_enabled(&self) -> bool {
        *self.enabled.lock().await
    }

    pub async fn on_play_start(&self) {
        if !*self.enabled.lock().await {
            return;
        }
        *self.state.lock().await = GaplessState::Monitoring;
        *self.next_url_set.lock().await = false;
        debug!("gapless_monitoring_start");
    }

    pub async fn check_prebuffer(
        &self,
        status: &OutputStatus,
        zone_id: i64,
        queue_position: i64,
        queue_length: i64,
        orchestrator: &PlaybackOrchestrator,
        device_id: &str,
    ) -> bool {
        if !*self.enabled.lock().await {
            return false;
        }

        let current_state = self.state.lock().await.clone();
        if current_state != GaplessState::Monitoring {
            return false;
        }

        if status.state != TransportState::Playing || status.duration_ms == 0 {
            return false;
        }

        let remaining_ms = status.duration_ms.saturating_sub(status.position_ms);
        if remaining_ms > self.prebuffer_threshold_ms {
            return false;
        }

        if *self.next_url_set.lock().await {
            return false;
        }

        let next_pos = queue_position + 1;
        if next_pos >= queue_length {
            return false;
        }

        match orchestrator.resolve_queue_item_url(zone_id, next_pos).await {
            Ok(resolved) => {
                // #1894 — le pré-armement annonçait `DLNA.ORG_OP=01` en dur sur
                // une session-canal que `play` annonce, elle, non-seekable.
                let byte_seekable = orchestrator
                    .media_byte_seekable(resolved.stream_id.as_deref())
                    .await;
                let outputs = orchestrator.outputs.lock().await;
                if let Some(output) = outputs.get(device_id) {
                    let out = output.lock().await;
                    let media = PlayMedia {
                        url: &resolved.url,
                        mime_type: &resolved.mime_type,
                        title: Some(&resolved.title),
                        artist: resolved.artist.as_deref(),
                        album: resolved.album.as_deref(),
                        cover_url: resolved.cover_url.as_deref(),
                        duration_ms: resolved.duration_ms,
                        file_size: resolved.file_size,
                        file_path: None,
                        sample_rate: resolved.sample_rate,
                        bit_depth: resolved.bit_depth,
                        channels: resolved.channels,
                        live_stream: false,
                        byte_seekable,
                        // A staged queue item is a finite track: only the live
                        // and proxy paths have an upstream URL to carry.
                        origin_url: None,
                        source: resolved.source.as_deref(),
                        source_id: resolved.source_id.as_deref(),
                        track_number: resolved.track_number,
                        disc_number: resolved.disc_number,
                    };
                    match out.set_next_media(&media).await {
                        Ok(()) => {
                            *self.state.lock().await = GaplessState::Ready;
                            *self.next_url_set.lock().await = true;
                            info!(
                                zone_id,
                                next_pos,
                                title = ?resolved.title,
                                "gapless_next_url_set"
                            );
                            return true;
                        }
                        Err(e) => {
                            debug!(error = %e, "gapless_set_next_failed");
                        }
                    }
                } else {
                    // #2270 — la seule branche qui laissait une session-canal
                    // vivante SANS ecrire une seule ligne.
                    //
                    // `resolve_queue_item_url` ci-dessus a deja cree la session
                    // (`stream_session_created`). Si la sortie n'est pas dans
                    // le registre vivant a cet instant, ce `if let` tombait a
                    // cote sans `else` : la session n'etait remise a personne,
                    // personne ne la lisait, et le seul indice restant etait
                    // `stale_session_removed` une demi-heure plus tard. C'est
                    // mot pour mot la sequence de journal de Dimitri.
                    //
                    // Le chien de garde `local_stream_never_consumed` (#2653)
                    // ne couvre pas ce cas et ne le peut pas : il est arme dans
                    // `orchestrator::transport`, juste avant `send_to_output`,
                    // et son commentaire dit exclure les sessions « merely
                    // prepared in advance for gapless playback ». Un flux
                    // prepare pour une sortie absente n'arme donc RIEN.
                    //
                    // `warn!` et non `debug!` : le niveau par defaut est `info`
                    // (tune-server/src/config.rs), donc un `debug!` ici serait
                    // une trace que le terrain ne remonte jamais — c'est deja
                    // le cas de `gapless_set_next_failed` et de
                    // `play_not_announced_output_not_sent`.
                    warn!(
                        zone_id,
                        next_pos,
                        device_id = %device_id,
                        stream_id = ?resolved.stream_id,
                        "gapless_sortie_absente_du_registre_session_orpheline"
                    );
                }
            }
            Err(e) => {
                debug!(error = %e, "gapless_resolve_failed");
            }
        }

        false
    }

    pub async fn on_track_end(&self) {
        let was_ready = *self.state.lock().await == GaplessState::Ready;
        self.reset().await;
        if was_ready {
            info!("gapless_transition_complete");
        }
    }

    pub async fn reset(&self) {
        *self.state.lock().await = GaplessState::Idle;
        *self.next_url_set.lock().await = false;
    }

    pub async fn status(&self) -> serde_json::Value {
        let enabled = *self.enabled.lock().await;
        let state = self.state.lock().await.clone();
        let next_ready = *self.next_url_set.lock().await;
        serde_json::json!({
            "enabled": enabled,
            "state": format!("{state:?}"),
            "next_track_ready": next_ready,
            "prebuffer_threshold_ms": self.prebuffer_threshold_ms,
        })
    }
}

/// Un marqueur de format — extension, type MIME, ou `format` scanné — désigne-t-il du DSD ?
///
/// UNE seule définition, pour les trois endroits qui la posaient chacun de leur
/// côté : le garde de fin de piste DSD sur DLNA
/// (`decisions::dlna_dsd_reached_end`), le refus d'armement DSD sur DLNA
/// (`prepare_gapless`, #402) et la promesse faite à la file
/// ([`enchainement_sans_blanc`]). Deux copies, c'est la porte ouverte à ce que
/// la file promette ce que le poller refuse ensuite.
pub fn est_dsd(marqueur: &str) -> bool {
    let m = marqueur.to_lowercase();
    m.contains("dsd") || m.contains("dsf") || m.contains("dff")
}

/// #6910 — un renderer DLNA TIENT-il un suivant en DSD brut ?
///
/// La garde de #402 refusait d'armer tout suivant DSD sur DLNA : le HiFi Rose
/// RS130 acquittait `SetNextAVTransportURI` pour un flux DSD sans jamais y
/// passer, et l'album se coupait après la piste 1. Elle mordait aussi sur les
/// renderers qui lisent le DSD natif ET enchaînent (fil 2412, The Wall pistes
/// 2-3 : un blanc entre deux pistes enchaînées sur le disque).
///
/// Vrai **seulement** si les trois conditions tiennent :
///
/// 1. le renderer annonce le DSD dans son `GetProtocolInfo` — un sondage
///    CONCLUANT (`Some(true)`) — OU la zone est en mode DSD « natif »
///    (`natif_force`) et le sondage a conclu (`Some(false)`) : le DSD brut part
///    alors QUAND MÊME pour chaque piste (`should_dsd_passthrough`), le
///    renderer le joue déjà, et lui refuser la suivante ne laissait qu'un blanc
///    à chaque piste (Eversolo DMP-A8, fil 2885, #8969). `None` (pas sondé,
///    sondage échoué) garde la garde ;
/// 2. son SCPD d'AVTransport annonce `SetNextAVTransportURI`
///    ([`AnnonceSuivante::Annoncee`]) ; `Inconnue` garde la garde ;
/// 3. il n'est pas sous la garde rendue aux renderers qui lâchent le suivant
///    DSD armé (`deja_lache`, voir [`EchecsDsdSuivant`]) : un renderer comme le
///    RS130, qui annonce les deux et ne suit pas, perd
///    [`ECHECS_DSD_SUIVANT_AVANT_GARDE`] transitions, puis retrouve la garde ;
///    à chaque échéance il retente, et un seul échec la lui rend, pour une
///    durée doublée.
///
/// Une seule définition, lue par le sondeur (`prepare_gapless`) et par la
/// promesse de la file ([`enchainement_sans_blanc`]).
pub fn dsd_suivant_tenu_sur_dlna(
    annonce_dsd: Option<bool>,
    natif_force: bool,
    annonce_suivante: AnnonceSuivante,
    deja_lache: bool,
) -> bool {
    refus_dsd_suivant_sur_dlna(annonce_dsd, natif_force, annonce_suivante, deja_lache).is_none()
}

/// #6335 — POURQUOI un suivant DSD n'est pas armé sur un renderer DLNA.
///
/// La ligne `gapless_skipped_dsd_next_dlna` ne portait que la zone et le type
/// MIME : un renderer qui n'annonce pas le DSD, un sondage qui n'a pas abouti
/// et un SCPD jamais lu laissaient la même trace. C'est pourtant ce qui sépare
/// une limite de l'appareil d'un défaut de Tune.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusDsdSuivant {
    /// `GetProtocolInfo` pas sondé, ou sondage sans réponse (`None`).
    DsdNonSonde,
    /// Le renderer n'annonce pas le DSD (`Some(false)`) et la zone n'est pas
    /// en mode « natif » : le DSD n'est pas envoyé brut à ce renderer.
    DsdNonAnnonce,
    /// SCPD d'AVTransport ni connu ni lu : Tune ne sait pas si
    /// `SetNextAVTransportURI` est permis.
    SuivanteInconnue,
    /// Le SCPD d'AVTransport ne porte pas `SetNextAVTransportURI`.
    SuivanteNonAnnoncee,
    /// Le renderer a lâché des suivants DSD armés : la garde lui est rendue
    /// pour un temps (voir [`EchecsDsdSuivant`]).
    DejaLache,
}

impl RefusDsdSuivant {
    /// Le mot du journal.
    pub fn raison(self) -> &'static str {
        match self {
            RefusDsdSuivant::DsdNonSonde => "dsd_non_sonde",
            RefusDsdSuivant::DsdNonAnnonce => "dsd_non_annonce",
            RefusDsdSuivant::SuivanteInconnue => "setnext_inconnu",
            RefusDsdSuivant::SuivanteNonAnnoncee => "setnext_non_annonce",
            RefusDsdSuivant::DejaLache => "deja_lache",
        }
    }
}

/// #6335 — la première des trois conditions de [`dsd_suivant_tenu_sur_dlna`]
/// qui manque, `None` si le suivant DSD peut être armé.
///
/// `natif_force` : la zone est en mode DSD « natif » — réglage explicite qui
/// envoie le DSD brut même au renderer qui ne l'annonce pas (#8969). Il ne
/// lève que `Some(false)` : un sondage sans réponse (`None`) garde la garde.
/// Les deux autres conditions tiennent toujours, et un renderer qui lâche le
/// suivant DSD armé retrouve la garde (`deja_lache`) : une transition perdue,
/// puis la garde.
pub fn refus_dsd_suivant_sur_dlna(
    annonce_dsd: Option<bool>,
    natif_force: bool,
    annonce_suivante: AnnonceSuivante,
    deja_lache: bool,
) -> Option<RefusDsdSuivant> {
    match annonce_dsd {
        None => return Some(RefusDsdSuivant::DsdNonSonde),
        Some(false) if !natif_force => return Some(RefusDsdSuivant::DsdNonAnnonce),
        Some(_) => {}
    }
    match annonce_suivante {
        AnnonceSuivante::Inconnue => return Some(RefusDsdSuivant::SuivanteInconnue),
        AnnonceSuivante::NonAnnoncee => return Some(RefusDsdSuivant::SuivanteNonAnnoncee),
        AnnonceSuivante::Annoncee => {}
    }
    deja_lache.then_some(RefusDsdSuivant::DejaLache)
}

/// #9206 — échecs constatés avant de rendre la garde DSD à un appareil la
/// PREMIÈRE fois.
///
/// #6910 la rendait au premier : une seule fausse conclusion (fil 2961, une
/// fin coupée pendant que le renderer basculait sur le flux armé) supprimait
/// le sans-blanc DSD pour toute la session, alors que la transition d'avant
/// venait d'enchaîner sans blanc. Après une première garde, l'appareil est
/// récidiviste : un seul échec la lui rend (voir [`EchecsDsdSuivant`]).
pub const ECHECS_DSD_SUIVANT_AVANT_GARDE: u32 = 2;

/// #9206 — fenêtre dans laquelle les échecs se cumulent avant la première
/// garde ; un échec isolé plus ancien est oublié. Large (une séance
/// d'écoute) : deux échecs séparés par une longue piste comptent.
pub const FENETRE_ECHECS_DSD_SUIVANT: std::time::Duration =
    std::time::Duration::from_secs(6 * 60 * 60);

/// #9206 — durée de la PREMIÈRE garde ; passé ce délai, le suivant DSD est
/// de nouveau tenté (au lieu de « jusqu'au redémarrage »). Chaque récidive
/// la double, jusqu'à [`DUREE_GARDE_DSD_SUIVANT_MAX`].
pub const DUREE_GARDE_DSD_SUIVANT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// #9206 — plafond de la garde d'un récidiviste.
pub const DUREE_GARDE_DSD_SUIVANT_MAX: std::time::Duration =
    std::time::Duration::from_secs(8 * 60 * 60);

/// #9206 — ce qu'un échec constaté change pour l'appareil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueEchecDsdSuivant {
    /// Échec noté, la garde n'est pas (encore) rendue.
    Note { echecs: u32 },
    /// La garde est rendue pour `duree` (la `garde`-ième fois).
    GardeRendue {
        echecs: u32,
        garde: u32,
        duree: std::time::Duration,
    },
    /// La garde était déjà rendue : rien ne change.
    DejaGarde,
}

/// #6910 / #9206 — la mémoire, par appareil, des suivants DSD armés et lâchés.
///
/// - première garde : [`ECHECS_DSD_SUIVANT_AVANT_GARDE`] échecs dans
///   [`FENETRE_ECHECS_DSD_SUIVANT`], ou un seul si l'appareil avoue avoir
///   perdu la suivante ;
/// - la garde échoit : l'appareil retente le suivant DSD, mais reste
///   RÉCIDIVISTE (`gardes` est conservé) : un seul échec lui rend la garde,
///   pour une durée doublée à chaque fois (30 min, 1 h, 2 h… plafond 8 h).
///   Un RS130 (#402) ne reperd donc qu'une transition par échéance, de plus
///   en plus rares.
#[derive(Debug, Clone, Default)]
pub struct EchecsDsdSuivant {
    echecs: u32,
    premier_echec: Option<std::time::Instant>,
    garde_depuis: Option<std::time::Instant>,
    /// Gardes déjà rendues à cet appareil depuis le démarrage.
    gardes: u32,
}

impl EchecsDsdSuivant {
    /// Durée de la `garde`-ième garde (1 = la première).
    pub fn duree_de_la_garde(garde: u32) -> std::time::Duration {
        let facteur = 1u32 << garde.saturating_sub(1).min(16);
        DUREE_GARDE_DSD_SUIVANT
            .saturating_mul(facteur)
            .min(DUREE_GARDE_DSD_SUIVANT_MAX)
    }

    /// La garde est-elle rendue à `maintenant` ? Une garde échue est levée et
    /// ses échecs oubliés ; la récidive (`gardes`) est retenue.
    pub fn garde_active(&mut self, maintenant: std::time::Instant) -> bool {
        match self.garde_depuis {
            Some(t)
                if maintenant.saturating_duration_since(t)
                    < Self::duree_de_la_garde(self.gardes) =>
            {
                true
            }
            Some(_) => {
                self.garde_depuis = None;
                self.echecs = 0;
                self.premier_echec = None;
                false
            }
            None => false,
        }
    }

    /// Note un échec. `immediat` : l'appareil a dit lui-même avoir perdu la
    /// suivante (`SuivantePreparee::Perdue`) — preuve suffisante à elle seule.
    pub fn noter_echec(
        &mut self,
        maintenant: std::time::Instant,
        immediat: bool,
    ) -> IssueEchecDsdSuivant {
        if self.garde_active(maintenant) {
            return IssueEchecDsdSuivant::DejaGarde;
        }
        if self
            .premier_echec
            .is_some_and(|t| maintenant.saturating_duration_since(t) >= FENETRE_ECHECS_DSD_SUIVANT)
        {
            self.echecs = 0;
            self.premier_echec = None;
        }
        self.echecs = self.echecs.saturating_add(1);
        self.premier_echec.get_or_insert(maintenant);
        let recidiviste = self.gardes > 0;
        if immediat || recidiviste || self.echecs >= ECHECS_DSD_SUIVANT_AVANT_GARDE {
            self.gardes = self.gardes.saturating_add(1);
            self.garde_depuis = Some(maintenant);
            IssueEchecDsdSuivant::GardeRendue {
                echecs: self.echecs,
                garde: self.gardes,
                duree: Self::duree_de_la_garde(self.gardes),
            }
        } else {
            IssueEchecDsdSuivant::Note {
                echecs: self.echecs,
            }
        }
    }

    /// Échecs notés depuis la dernière garde (banc).
    pub fn echecs(&self) -> u32 {
        self.echecs
    }
}

/// Ce qu'une ligne de file peut promettre à la ligne **affichée** juste après elle.
///
/// C'est la question exacte que pose le badge « Gapless » de la file : entre
/// CETTE ligne et la SUIVANTE À L'ÉCRAN, y aura-t-il un blanc ?
///
/// Le badge n'a jamais rien affiché, pour personne, sur aucune zone : le champ
/// `gapless_next` que le client lit n'existait dans **aucune** structure
/// sérialisée du serveur — il ne survivait que dans `docs/contrat-web.json`,
/// hérité du serveur Python (Jean Valjean, fil 631, 15/06/2026, #2934). Il est
/// calculé ici à partir des MÊMES refus que le poller applique au moment
/// d'armer, jamais d'un défaut constant : un indicateur qui ment est pire que
/// pas d'indicateur.
#[derive(Debug, Clone, Copy)]
pub struct EnchainementAffiche<'a> {
    /// Réglage `zones.gapless_enabled` de la zone.
    pub gapless_enabled: bool,
    /// `capabilities().can_gapless` de la sortie réellement enregistrée.
    ///
    /// `false` couvre aussi la sortie inconnue (zone navigateur, sortie
    /// disparue) : on ne promet pas ce qu'on ne peut pas constater.
    pub output_can_gapless: bool,
    /// `output_type()` de la zone (`"dlna"`, `"local"`, `"oaat"`…).
    pub output_type: &'a str,
    /// `prefers_local_file_gapless()` : la sortie ne sait mettre en attente
    /// qu'un FICHIER local (OAAT en DSD natif ou en PCM direct).
    pub output_prefers_local_file: bool,
    /// Index AFFICHÉ de la ligne examinée.
    pub index: i64,
    /// Index que la file désignera réellement comme suivant quand cette ligne
    /// se terminera — [`crate::poller::PositionPoller::next_position_after`],
    /// la seule décision « piste suivante » du serveur. Sous aléatoire ou sous
    /// répétition-une il n'est PAS `index + 1` : l'ordre affiché ne dit alors
    /// rien de l'ordre joué.
    pub successeur_reel: Option<i64>,
    /// `format` de la ligne affichée juste après.
    pub successeur_format: Option<&'a str>,
    /// La ligne affichée juste après a-t-elle un fichier local ?
    pub successeur_est_fichier_local: bool,
    /// #6910 — [`dsd_suivant_tenu_sur_dlna`] pour la sortie de la zone : le
    /// sondeur armera-t-il un suivant DSD sur ce renderer DLNA ?
    pub dsd_suivant_tenu: bool,
}

/// Vrai **seulement** si l'enchaînement vers la ligne affichée suivante se fera
/// réellement sans blanc.
///
/// Chaque refus reproduit un refus que le poller applique déjà, et le renvoie à
/// son journal :
///
/// | refus | journal du poller |
/// |---|---|
/// | l'ordre affiché n'est pas l'ordre joué | — (aléatoire / répétition-une) |
/// | zone `gapless_enabled = 0` | le poller n'entre pas dans la branche d'armement |
/// | sortie qui ne chaîne pas depuis sa boucle | `gapless_skipped_exclusive_output` |
/// | suivant DSD sur un DLNA qui ne le tient pas (#402, #6910) | `gapless_skipped_dsd_next_dlna` |
/// | suivant sans fichier local sur OAAT DSD natif | `gapless_local_file_skipped_no_local_next` |
pub fn enchainement_sans_blanc(i: &EnchainementAffiche<'_>) -> bool {
    // L'ordre affiché doit ÊTRE l'ordre joué. Sous aléatoire, la ligne d'en
    // dessous n'est pas celle qui va suivre ; sous répétition-une, c'est la
    // même qui repart. Promettre là serait mentir sur QUI s'enchaîne — la
    // dernière ligne d'une file, qui n'a pas de suivant, tombe ici aussi.
    if i.successeur_reel != Some(i.index + 1) {
        return false;
    }
    if !i.gapless_enabled {
        return false;
    }
    // Sortie qui ne sait pas chaîner depuis sa propre boucle de lecture :
    // Chromecast, slimproto, AirPlay, Squeezebox, HQPlayer, sortie locale en
    // mode exclusif (ASIO / WASAPI exclusif), et sortie locale ou OAAT dont la
    // chaîne s'est épuisée.
    if !i.output_can_gapless {
        return false;
    }
    // #402 — un renderer DLNA accepte `SetNextAVTransportURI` pour un flux DSD
    // et ne le consomme jamais (HiFi Rose RS130, Benjithom). Le poller refuse
    // d'armer ; la file ne le promet donc pas. Le même DSD sur une sortie
    // locale garde sa chaîne interne et reste promis. #6910 : sauf renderer
    // qui annonce le DSD et `SetNext` et n'a pas lâché de suivant DSD.
    if i.output_type == "dlna" && i.successeur_format.is_some_and(est_dsd) && !i.dsd_suivant_tenu {
        return false;
    }
    // OAAT en DSD natif / PCM direct lit le `.dsf` suivant sur le disque : un
    // suivant en streaming, sans fichier local, n'est pas armé.
    if i.output_prefers_local_file && !i.successeur_est_fichier_local {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── #2934 : ce que la file promet d'une ligne à la suivante ─────────────

    fn ligne() -> EnchainementAffiche<'static> {
        EnchainementAffiche {
            gapless_enabled: true,
            output_can_gapless: true,
            output_type: "local",
            output_prefers_local_file: false,
            index: 0,
            successeur_reel: Some(1),
            successeur_format: Some("flac"),
            successeur_est_fichier_local: true,
            dsd_suivant_tenu: false,
        }
    }

    #[test]
    fn est_dsd_reconnait_les_trois_marqueurs() {
        assert!(est_dsd("dsf"));
        assert!(est_dsd("DFF"));
        assert!(est_dsd("audio/x-dsd"));
        assert!(!est_dsd("flac"));
        assert!(!est_dsd("audio/x-flac"));
        assert!(!est_dsd(""));
    }

    #[test]
    fn enchaine_quand_tout_est_reuni() {
        assert!(enchainement_sans_blanc(&ligne()));
    }

    #[test]
    fn derniere_ligne_ne_promet_rien() {
        let mut l = ligne();
        l.successeur_reel = None;
        assert!(!enchainement_sans_blanc(&l));
    }

    #[test]
    fn ordre_affiche_different_de_l_ordre_joue_ne_promet_rien() {
        // Aléatoire : la ligne d'en dessous n'est pas celle qui va suivre.
        let mut l = ligne();
        l.successeur_reel = Some(7);
        assert!(!enchainement_sans_blanc(&l));
        // Répétition-une : c'est la MÊME ligne qui repart.
        let mut l = ligne();
        l.successeur_reel = Some(0);
        assert!(!enchainement_sans_blanc(&l));
    }

    #[test]
    fn zone_sans_gapless_ne_promet_rien() {
        let mut l = ligne();
        l.gapless_enabled = false;
        assert!(!enchainement_sans_blanc(&l));
    }

    #[test]
    fn sortie_qui_ne_chaine_pas_ne_promet_rien() {
        // Chromecast, slimproto, local en mode exclusif, chaîne épuisée…
        let mut l = ligne();
        l.output_can_gapless = false;
        assert!(!enchainement_sans_blanc(&l));
    }

    #[test]
    fn dsd_sur_dlna_ne_promet_rien_mais_dsd_en_local_promet() {
        // #402 : le renderer accepte SetNext et ne le consomme jamais.
        let mut dlna = ligne();
        dlna.output_type = "dlna";
        dlna.successeur_format = Some("dsf");
        assert!(!enchainement_sans_blanc(&dlna));
        // Le refus vise le COUPLE DLNA+DSD, pas l'un des deux :
        let mut dlna_flac = ligne();
        dlna_flac.output_type = "dlna";
        assert!(enchainement_sans_blanc(&dlna_flac));
        let mut local_dsd = ligne();
        local_dsd.successeur_format = Some("dsf");
        assert!(enchainement_sans_blanc(&local_dsd));
    }

    /// #6910 — le renderer DSD natif qui annonce `SetNext` : le suivant DSD
    /// est promis. Le refus de #402 reste pour tous les autres.
    #[test]
    fn dsd_sur_dlna_promis_au_renderer_qui_le_tient_6910() {
        let mut dlna = ligne();
        dlna.output_type = "dlna";
        dlna.successeur_format = Some("dsf");
        dlna.dsd_suivant_tenu = true;
        assert!(
            enchainement_sans_blanc(&dlna),
            "#6910 : renderer qui annonce le DSD et SetNext — le suivant DSD doit être promis"
        );
    }

    /// #6910 — chaque condition manquante garde la garde de #402.
    #[test]
    fn dsd_suivant_tenu_exige_les_trois_conditions_6910() {
        use AnnonceSuivante::*;
        assert!(dsd_suivant_tenu_sur_dlna(
            Some(true),
            false,
            Annoncee,
            false
        ));
        // DSD non sondé, ou non annoncé hors mode « natif ».
        assert!(!dsd_suivant_tenu_sur_dlna(None, false, Annoncee, false));
        assert!(!dsd_suivant_tenu_sur_dlna(
            Some(false),
            false,
            Annoncee,
            false
        ));
        // SCPD illisible ou action absente.
        assert!(!dsd_suivant_tenu_sur_dlna(
            Some(true),
            false,
            Inconnue,
            false
        ));
        assert!(!dsd_suivant_tenu_sur_dlna(
            Some(true),
            false,
            NonAnnoncee,
            false
        ));
        // Le renderer a déjà lâché un suivant DSD armé (RS130, #402).
        assert!(!dsd_suivant_tenu_sur_dlna(
            Some(true),
            false,
            Annoncee,
            true
        ));
    }

    /// #8969 (fil 2885, Eversolo DMP-A8) — zone en DSD « natif » : le DSD brut
    /// part même sans annonce du renderer, la suivante l'est aussi. Le natif ne
    /// lève QUE le `Some(false)` : sondage sans réponse, SCPD sans `SetNext`
    /// ou renderer qui a déjà lâché un suivant DSD gardent la garde.
    #[test]
    fn natif_force_leve_seulement_le_dsd_non_annonce_8969() {
        use AnnonceSuivante::*;
        assert!(dsd_suivant_tenu_sur_dlna(
            Some(false),
            true,
            Annoncee,
            false
        ));
        assert!(!dsd_suivant_tenu_sur_dlna(None, true, Annoncee, false));
        assert!(!dsd_suivant_tenu_sur_dlna(
            Some(false),
            true,
            Inconnue,
            false
        ));
        assert!(!dsd_suivant_tenu_sur_dlna(
            Some(false),
            true,
            NonAnnoncee,
            false
        ));
        assert!(!dsd_suivant_tenu_sur_dlna(
            Some(false),
            true,
            Annoncee,
            true
        ));
        assert_eq!(
            refus_dsd_suivant_sur_dlna(Some(false), true, Annoncee, true)
                .map(RefusDsdSuivant::raison),
            Some("deja_lache")
        );
    }

    /// #9206 — un seul échec ne rend plus la garde ; le deuxième, si ; la
    /// garde échoit, l'appareil retente ; récidiviste, un échec la lui rend
    /// aussitôt, pour une durée doublée, plafonnée.
    #[test]
    fn la_garde_dsd_suit_deux_echecs_puis_la_recidive_9206() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut m = EchecsDsdSuivant::default();
        assert_eq!(
            m.noter_echec(t0, false),
            IssueEchecDsdSuivant::Note { echecs: 1 }
        );
        assert!(!m.garde_active(t0), "un seul échec ne rend pas la garde");
        // Deux échecs séparés d'une longue piste (> 30 min) comptent.
        let t1 = t0 + Duration::from_secs(45 * 60);
        assert_eq!(
            m.noter_echec(t1, false),
            IssueEchecDsdSuivant::GardeRendue {
                echecs: 2,
                garde: 1,
                duree: DUREE_GARDE_DSD_SUIVANT
            }
        );
        assert!(m.garde_active(t1));
        assert_eq!(m.noter_echec(t1, false), IssueEchecDsdSuivant::DejaGarde);
        assert!(m.garde_active(t1 + DUREE_GARDE_DSD_SUIVANT - Duration::from_secs(1)));
        let t2 = t1 + DUREE_GARDE_DSD_SUIVANT;
        assert!(
            !m.garde_active(t2),
            "la garde échoit : plus de « jusqu'au redémarrage »"
        );
        // Récidive : un seul échec, garde doublée.
        assert_eq!(
            m.noter_echec(t2, false),
            IssueEchecDsdSuivant::GardeRendue {
                echecs: 1,
                garde: 2,
                duree: DUREE_GARDE_DSD_SUIVANT * 2
            }
        );
        assert!(m.garde_active(t2 + DUREE_GARDE_DSD_SUIVANT));
        assert!(!m.garde_active(t2 + DUREE_GARDE_DSD_SUIVANT * 2));
        assert_eq!(
            EchecsDsdSuivant::duree_de_la_garde(10),
            DUREE_GARDE_DSD_SUIVANT_MAX,
            "plafond"
        );
    }

    /// #9206 — deux échecs trop éloignés ne se cumulent pas ; la suivante
    /// que l'appareil dit avoir perdue rend la garde tout de suite.
    #[test]
    fn echecs_isoles_oublies_et_perte_avouee_immediate_9206() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut m = EchecsDsdSuivant::default();
        m.noter_echec(t0, false);
        assert_eq!(
            m.noter_echec(
                t0 + FENETRE_ECHECS_DSD_SUIVANT + Duration::from_secs(1),
                false
            ),
            IssueEchecDsdSuivant::Note { echecs: 1 }
        );
        let mut avoue = EchecsDsdSuivant::default();
        assert!(matches!(
            avoue.noter_echec(t0, true),
            IssueEchecDsdSuivant::GardeRendue { garde: 1, .. }
        ));
        assert!(avoue.garde_active(t0));
    }

    /// #6335 — le refus nomme la condition qui manque, une par cas.
    #[test]
    fn le_refus_dsd_suivant_nomme_la_condition_manquante_6335() {
        use AnnonceSuivante::*;
        let r = |dsd, suivante, lache| refus_dsd_suivant_sur_dlna(dsd, false, suivante, lache);
        assert_eq!(r(Some(true), Annoncee, false), None);
        assert_eq!(
            r(None, Annoncee, false).map(RefusDsdSuivant::raison),
            Some("dsd_non_sonde")
        );
        assert_eq!(
            r(Some(false), Annoncee, false).map(RefusDsdSuivant::raison),
            Some("dsd_non_annonce")
        );
        assert_eq!(
            r(Some(true), Inconnue, false).map(RefusDsdSuivant::raison),
            Some("setnext_inconnu")
        );
        assert_eq!(
            r(Some(true), NonAnnoncee, false).map(RefusDsdSuivant::raison),
            Some("setnext_non_annonce")
        );
        assert_eq!(
            r(Some(true), Annoncee, true).map(RefusDsdSuivant::raison),
            Some("deja_lache")
        );
    }

    #[test]
    fn oaat_dsd_natif_ne_promet_qu_un_suivant_local() {
        let mut streaming = ligne();
        streaming.output_type = "oaat";
        streaming.output_prefers_local_file = true;
        streaming.successeur_est_fichier_local = false;
        assert!(!enchainement_sans_blanc(&streaming));
        let mut local = streaming;
        local.successeur_est_fichier_local = true;
        assert!(enchainement_sans_blanc(&local));
    }

    #[test]
    fn un_suivant_en_streaming_reste_promis_sur_une_sortie_ordinaire() {
        // Le garde « fichier local » est propre aux sorties qui l'exigent :
        // une piste Qobuz s'enchaîne normalement sur une sortie DLNA.
        let mut l = ligne();
        l.output_type = "dlna";
        l.successeur_format = None;
        l.successeur_est_fichier_local = false;
        assert!(enchainement_sans_blanc(&l));
    }

    #[tokio::test]
    async fn starts_disabled() {
        let h = GaplessHandler::new(false);
        assert!(!h.is_enabled().await);
    }

    #[tokio::test]
    async fn enable_disable() {
        let h = GaplessHandler::new(true);
        assert!(h.is_enabled().await);
        h.set_enabled(false).await;
        assert!(!h.is_enabled().await);
    }

    #[tokio::test]
    async fn monitoring_on_play() {
        let h = GaplessHandler::new(true);
        h.on_play_start().await;
        let state = h.state.lock().await.clone();
        assert_eq!(state, GaplessState::Monitoring);
    }

    #[tokio::test]
    async fn reset_clears_state() {
        let h = GaplessHandler::new(true);
        h.on_play_start().await;
        h.reset().await;
        let state = h.state.lock().await.clone();
        assert_eq!(state, GaplessState::Idle);
    }

    #[tokio::test]
    async fn status_json() {
        let h = GaplessHandler::new(true);
        h.on_play_start().await;
        let s = h.status().await;
        assert_eq!(s["enabled"], true);
        assert_eq!(s["state"], "Monitoring");
        assert_eq!(s["next_track_ready"], false);
    }

    #[tokio::test]
    async fn no_monitoring_when_disabled() {
        let h = GaplessHandler::new(false);
        h.on_play_start().await;
        let state = h.state.lock().await.clone();
        assert_eq!(state, GaplessState::Idle);
    }

    /// Le pre-armement NOMME la sortie absente au lieu de se taire (#2270).
    ///
    /// `resolve_queue_item_url` a deja cree une session-canal quand on arrive
    /// a `outputs.get(device_id)`. Quand ce `if let` tombe a cote, la session
    /// reste vivante, personne ne la lit, et avant le 11/09/2026 il n'y avait
    /// RIEN dans le journal avant `stale_session_removed`, une demi-heure plus
    /// tard — la sequence exacte du signalement de Dimitri.
    ///
    /// Garde de cablage, faible et assumee : elle relit le texte du module.
    /// C'est le seul filet possible ici. Le chien de garde
    /// `local_stream_never_consumed` (#2653) ne couvre pas cette branche et ne
    /// le peut pas : il est arme dans `orchestrator::transport`, juste avant
    /// `send_to_output`, et son commentaire dit exclure les sessions « merely
    /// prepared in advance for gapless playback ». Et reproduire le cas
    /// demanderait une sortie qui disparait du registre entre la resolution et
    /// le pre-armement, hors de portee sans materiel audio.
    #[test]
    fn le_prearmement_gapless_nomme_la_sortie_absente() {
        let source = include_str!("gapless.rs");
        assert!(
            source.contains("\"gapless_sortie_absente_du_registre_session_orpheline\""),
            "le pre-armement gapless est redevenu muet quand la sortie manque \
             au registre : la session-canal deja creee reste orpheline et le \
             journal ne portera plus que `stale_session_removed`, trente \
             minutes trop tard (#2270)"
        );
        assert!(
            source.contains("warn!("),
            "le marqueur existe mais plus au niveau `warn` : le niveau par \
             defaut est `info`, un `debug!` ne remonterait jamais du terrain \
             (#2270)"
        );
    }
}
