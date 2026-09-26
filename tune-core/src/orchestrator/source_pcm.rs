//! Résolution d'une ligne dont la `source` est fournie par un greffon en PCM
//! (#4863). Voir `crate::source_pcm` pour le contrat.
//!
//! Le flux est servi par une session FINIE de l'`AudioStreamer` — la même
//! famille que le transcodage en WAV d'une piste locale : en-tête RIFF
//! envoyé dans le canal (`wav_header_included`), longueur exacte annoncée
//! (`file_size`), fin d'entrée signalée (`end_session_input`) pour que la
//! sortie voie un vrai EOF et enchaîne la piste suivante.

use super::*;
use crate::source_pcm::{
    Consommation, FinDePompe, FournisseurPcm, inscrire_direct, pomper, pomper_sans_fin,
    retirer_direct,
};
use std::sync::atomic::{AtomicBool, Ordering};

/// Profondeur du canal d'une session EN DIRECT, en tronçons. Petite exprès :
/// c'est le consommateur qui doit donner le rythme. Un canal de 256 tronçons
/// (celui d'une piste) cacherait des secondes de retard et la dérive avec.
pub(super) const CANAL_DIRECT: usize = 8;

impl PlaybackOrchestrator {
    /// Le registre des sources PCM fournies par les greffons.
    pub fn sources_pcm(&self) -> &crate::source_pcm::SourcesPcm {
        &self.sources_pcm
    }

    /// #5065 — le registre commun des sources physiques, que les greffons
    /// natifs reçoivent par l'orchestrateur de leurs `HostServices`.
    pub fn sources_physiques(&self) -> &Arc<crate::sources_physiques::RegistreSources> {
        &self.sources_physiques
    }

    pub(super) async fn resolve_source_pcm(
        &self,
        source: &str,
        fournisseur: Arc<dyn FournisseurPcm>,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        // #5051 — une source sans fin se sert comme une radio.
        if fournisseur.en_direct() {
            return self
                .resolve_source_pcm_direct(source, fournisseur, req)
                .await;
        }
        let source_id = req
            .source_id
            .clone()
            .ok_or_else(|| format!("source « {source} » : source_id requis"))?;
        let depuis_ms = req.seek_ms.unwrap_or(0);
        // #5079 — une lecture EXPLICITE (pas un pré-armement gapless, qui
        // doit laisser finir la piste qui joue) arrête d'abord les pompes que
        // la zone fait encore tourner : un seul bras de lecture ne sert pas
        // deux positions à la fois.
        if self.levels_attach_allowed(req.zone_id) {
            arreter_les_pompes_de_la_zone(req.zone_id).await;
        }
        // L'ouverture parle au matériel (lecture de la TOC d'un disque) : hors
        // du fil asynchrone.
        let ouvrir = {
            let f = fournisseur.clone();
            let sid = source_id.clone();
            tokio::task::spawn_blocking(move || f.ouvrir(&sid, depuis_ms))
        };
        let flux = ouvrir
            .await
            .map_err(|e| format!("source « {source} » : ouverture interrompue ({e})"))??;
        let format = flux.format;
        let octets = flux.octets;
        let data_size = u32::try_from(octets)
            .map_err(|_| format!("source « {source} » : flux trop long pour un en-tête WAV"))?;
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: format.frequence,
            bit_depth: format.bits,
            channels: format.canaux,
            // La longueur EXACTE : elle devient le Content-Length (voir le
            // contrat de longueur dans `crate::source_pcm`).
            file_size: Some(44 + octets),
            duration_ms: Some(octets * 1000 / format.octets_par_seconde().max(1)),
            seek_ms: (depuis_ms > 0).then_some(depuis_ms),
            ..Default::default()
        };
        let (session_id, tx, data_ready) = self.streamer.create_session(info, false, 256).await;
        {
            let sessions = self.streamer.sessions_state();
            let sessions = sessions.lock().await;
            if let Some(s) = sessions.get(&session_id) {
                s.wav_header_included
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        info!(
            zone_id = req.zone_id,
            source,
            source_id = %source_id,
            depuis_ms,
            octets,
            stream_id = %session_id,
            "source_pcm_session_ouverte"
        );

        // #5078 — les niveaux (crête-mètre, vu-mètre, spectre) du PCM pompé.
        let niveaux = self
            .niveaux_de_la_source_pcm(req.zone_id, &session_id, depuis_ms)
            .await;

        let streamer = self.streamer.clone();
        let bus = self.event_bus.clone();
        let zone_id = req.zone_id;
        let sid = session_id.clone();
        let nom = source.to_string();
        let titre = req.title.clone().unwrap_or_default();
        let pompe = PompeDeZone::inscrire(zone_id);
        let mut lecteur = LectureArretable {
            flux: flux.lecteur,
            pompe: pompe.clone(),
        };
        tokio::spawn(async move {
            let rt = tokio::runtime::Handle::current();
            let arretee = pompe.arret.clone();
            let fin = tokio::task::spawn_blocking(move || {
                let entete = crate::audio::wav::build_wav_header_with_data_size(
                    format.canaux,
                    format.frequence,
                    format.bits,
                    data_size,
                );
                if rt.block_on(tx.send(entete.to_vec())).is_err() {
                    return FinDePompe::ConsommateurParti;
                }
                data_ready.notify_one();
                pomper(&mut lecteur, octets, |t| {
                    if let Some(ltx) = &niveaux {
                        crate::audio::tap::send_windowed_pcm(
                            ltx,
                            &t,
                            format.bits,
                            format.canaux,
                            format.frequence,
                        );
                    }
                    // Revérifié APRÈS l'envoi : une pompe bloquée sur un canal
                    // plein ne relit pas le disque une fois remplacée.
                    rt.block_on(tx.send(t)).is_ok() && !arretee.load(Ordering::SeqCst)
                })
            })
            .await;
            pompe.desinscrire();
            if pompe.arret.load(Ordering::SeqCst) {
                // Remplacée par une lecture plus récente de la zone : ni
                // erreur, ni EOF. La session est retirée par cette lecture ;
                // un EOF ici ferait croire à la fin naturelle de la piste.
                debug!(stream_id = %sid, "source_pcm_pompe_remplacee");
                return;
            }
            match fin {
                Ok(FinDePompe::Complete) => debug!(stream_id = %sid, "source_pcm_complete"),
                Ok(FinDePompe::ConsommateurParti) => {
                    debug!(stream_id = %sid, "source_pcm_consommateur_parti")
                }
                Ok(FinDePompe::Interrompue { remis, raison }) => {
                    warn!(
                        zone_id,
                        source = %nom,
                        stream_id = %sid,
                        remis,
                        attendus = octets,
                        raison = %raison,
                        "source_pcm_interrompue"
                    );
                    if let Some(bus) = bus {
                        bus.emit(
                            "zone.playback_error",
                            serde_json::json!({
                                "zone_id": zone_id,
                                "error": format!("Lecture de « {titre} » interrompue : {raison}"),
                                "fatal": true,
                            }),
                        );
                    }
                }
                Err(e) => warn!(stream_id = %sid, error = %e, "source_pcm_tache_paniquee"),
            }
            // Contenu FINI : fermer l'entrée pour que la sortie voie l'EOF.
            streamer.end_session_input(&sid).await;
        });

        let url = self
            .streamer
            .get_stream_url(&session_id, &self.server_ip(), "wav");
        Ok(ResolvedStream {
            url,
            mime_type: "audio/wav".into(),
            title: req.title.clone().unwrap_or_default(),
            artist: req.artist_name.clone(),
            album: req.album_title.clone(),
            duration_ms: Some(flux.duree_ms as i64),
            source: source.to_string(),
            cover_url: req.cover_url.clone(),
            stream_id: Some(session_id),
            file_size: Some(44 + octets),
            sample_rate: Some(format.frequence),
            bit_depth: Some(format.bits as u32),
            channels: Some(format.canaux as u32),
            origin_url: None,
            bitrate_kbps: None,
        })
    }

    /// #5051 — une source PCM EN DIRECT : session de radio (sans longueur,
    /// en-tête WAV indéterminé, corps découpé à la volée), pompe sans fin.
    pub(super) async fn resolve_source_pcm_direct(
        &self,
        source: &str,
        fournisseur: Arc<dyn FournisseurPcm>,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let source_id = req
            .source_id
            .clone()
            .ok_or_else(|| format!("source « {source} » : source_id requis"))?;
        // L'ouverture reçoit le compteur de consommation de la session, qui
        // n'existe qu'une fois le format connu : lié après coup. Une référence
        // FAIBLE : tenir la session garderait son canal ouvert après son
        // retrait, et la pompe ne verrait jamais le consommateur partir.
        type Faible = std::sync::Weak<crate::http::streamer::StreamSession>;
        let cellule: Arc<std::sync::OnceLock<Faible>> = Arc::default();
        let consommation = {
            let c = cellule.clone();
            Consommation::new(move || {
                c.get()
                    .and_then(|s| s.upgrade())
                    .map(|s| s.bytes_sent.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(0)
            })
        };
        let ouvrir = {
            let f = fournisseur.clone();
            let sid = source_id.clone();
            tokio::task::spawn_blocking(move || f.ouvrir_direct(&sid, consommation))
        };
        let flux = ouvrir
            .await
            .map_err(|e| format!("source « {source} » : ouverture interrompue ({e})"))??;
        let format = flux.format;
        // L'en-tête WAV du corps HTTP lit sa profondeur dans `info` ; la
        // fréquence et les canaux, dans le format détecté publié plus bas.
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: format.frequence,
            bit_depth: format.bits,
            channels: format.canaux,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };
        let (session_id, tx, data_ready, session) =
            self.streamer.create_radio_session(info, CANAL_DIRECT).await;
        let _ = cellule.set(Arc::downgrade(&session));
        session.publish_detected_output_format(format.frequence, format.canaux);
        session.publish_radio_source(crate::http::streamer::RadioSourceInfo {
            format: Some("wav"),
            sample_rate: Some(format.frequence),
            bit_depth: Some(format.bits),
        });
        if let Some(etat) = flux.etat.clone() {
            inscrire_direct(&session_id, etat);
        }
        info!(
            zone_id = req.zone_id,
            source,
            source_id = %source_id,
            frequence = format.frequence,
            bits = format.bits,
            canaux = format.canaux,
            stream_id = %session_id,
            "source_pcm_direct_ouverte"
        );

        // Les instruments de la zone (crête, vu-mètre, spectre) : le MÊME et
        // UNIQUE relais que le mode « longueur connue » (#5078) — un
        // forwarder cadencé par `levels_forwarder_if_allowed`, ou, pendant un
        // pré-armement gapless, l'attente de l'adoption. Le PCM est tapé au
        // moment où il part vers la sortie ; rien d'autre ne décode ce flux
        // pour les niveaux, et aucun second forwarder n'est ouvert ici.
        let mut niveaux = self
            .niveaux_de_la_source_pcm(req.zone_id, &session_id, 0)
            .await;
        let streamer = self.streamer.clone();
        let bus = self.event_bus.clone();
        let zone_id = req.zone_id;
        let sid = session_id.clone();
        let nom = source.to_string();
        let titre = req.title.clone().unwrap_or_default();
        let mut lecteur = flux.lecteur;
        // Faible, pour la même raison que la consommation.
        let session_fin = Arc::downgrade(&session);
        drop(session);
        tokio::spawn(async move {
            let rt = tokio::runtime::Handle::current();
            let fin = tokio::task::spawn_blocking(move || {
                let mut premier = true;
                pomper_sans_fin(&mut *lecteur, |t| {
                    if let Some(n) = &niveaux {
                        if !crate::audio::tap::send_windowed_pcm(
                            n,
                            &t,
                            format.bits,
                            format.canaux,
                            format.frequence,
                        ) {
                            niveaux = None;
                        }
                    }
                    let ok = rt.block_on(tx.send(t)).is_ok();
                    if premier {
                        // Le corps HTTP attend ce signal avant l'en-tête.
                        data_ready.notify_one();
                        premier = false;
                    }
                    ok
                })
            })
            .await;
            if let Some(s) = session_fin.upgrade() {
                s.producer_done
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            retirer_direct(&sid);
            match fin {
                Ok(FinDePompe::Complete) => info!(stream_id = %sid, "source_pcm_direct_terminee"),
                Ok(FinDePompe::ConsommateurParti) => {
                    info!(stream_id = %sid, "source_pcm_direct_consommateur_parti")
                }
                Ok(FinDePompe::Interrompue { remis, raison }) => {
                    warn!(
                        zone_id,
                        source = %nom,
                        stream_id = %sid,
                        remis,
                        raison = %raison,
                        "source_pcm_direct_interrompue"
                    );
                    if let Some(bus) = bus {
                        bus.emit(
                            "zone.playback_error",
                            serde_json::json!({
                                "zone_id": zone_id,
                                "error": format!("« {titre} » s'est interrompue : {raison}"),
                                "fatal": true,
                            }),
                        );
                    }
                }
                Err(e) => warn!(stream_id = %sid, error = %e, "source_pcm_direct_tache_paniquee"),
            }
            // Le flux est fini : que la sortie voie l'EOF.
            streamer.end_session_input(&sid).await;
        });

        let url = self
            .streamer
            .get_stream_url(&session_id, &self.server_ip(), "wav");
        Ok(ResolvedStream {
            url,
            mime_type: "audio/wav".into(),
            title: req.title.clone().unwrap_or_default(),
            artist: req.artist_name.clone(),
            album: req.album_title.clone(),
            // En direct : ni durée, ni longueur.
            duration_ms: None,
            source: source.to_string(),
            cover_url: req.cover_url.clone(),
            stream_id: Some(session_id),
            file_size: None,
            sample_rate: Some(format.frequence),
            bit_depth: Some(format.bits as u32),
            channels: Some(format.canaux as u32),
            origin_url: None,
            bitrate_kbps: None,
        })
    }

    /// #5078 (GgB, fil 1945) — la source PCM n'alimentait AUCUN instrument :
    /// elle ouvrait sa session sans forwarder de niveaux, là où les autres
    /// chemins qui tiennent le PCM en main en attachent un. Même forwarder
    /// cadencé qu'eux : il publie une fenêtre quand la sortie la joue
    /// (fil 1908), pas quand la pompe la lit.
    ///
    /// Pendant un pré-armement gapless, aucun forwarder ne peut naître : il
    /// serait daté de la piste qui joue encore (voir `levels_prewarm`). Les
    /// fenêtres attendent alors l'avance qui adopte ce flux
    /// ([`adopter_les_niveaux_pre_armes`]) : relire le disque pour les
    /// niveaux, comme le fait la sonde d'une piste de service, le ferait
    /// sauter d'une piste à l'autre.
    async fn niveaux_de_la_source_pcm(
        &self,
        zone_id: i64,
        session_id: &str,
        depuis_ms: u64,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>> {
        let bus = self.event_bus.clone()?;
        if self.levels_attach_allowed(zone_id) {
            return self
                .levels_forwarder_if_allowed(zone_id, depuis_ms as i64)
                .await;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (adopte_tx, adopte_rx) = tokio::sync::oneshot::channel();
        niveaux_en_attente().insert(session_id.to_string(), adopte_tx);
        tokio::spawn(relayer_les_niveaux_pre_armes(
            bus,
            self.playback.clone(),
            self.streamer.clone(),
            zone_id,
            session_id.to_string(),
            depuis_ms,
            rx,
            adopte_rx,
        ));
        Some(tx)
    }
}

type Adoption = tokio::sync::oneshot::Sender<u64>;

/// Les sessions PCM pré-armées dont les niveaux attendent l'avance gapless,
/// par identifiant de session (un UUID : unique d'un orchestrateur à l'autre).
fn niveaux_en_attente() -> std::sync::MutexGuard<'static, HashMap<String, Adoption>> {
    static EN_ATTENTE: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Adoption>>> =
        std::sync::LazyLock::new(Default::default);
    EN_ATTENTE.lock().unwrap_or_else(|e| e.into_inner())
}

/// L'avance gapless vient d'adopter `stream_id` : si c'est une source PCM
/// pré-armée, ses niveaux démarrent, sous le `play_seq` de la zone.
pub(super) fn adopter_les_niveaux_pre_armes(stream_id: &str, play_seq: u64) {
    if let Some(adoption) = niveaux_en_attente().remove(stream_id) {
        let _ = adoption.send(play_seq);
    }
}

/// Au plus deux minutes d'audio tenues en attente : la pompe devance la
/// sortie de la capacité de la session (256 tronçons, ~47 s de CD).
const ATTENTE_MAX: std::time::Duration = std::time::Duration::from_secs(120);

#[allow(clippy::too_many_arguments)]
async fn relayer_les_niveaux_pre_armes(
    bus: Arc<crate::event_bus::EventBus>,
    playback: Arc<PlaybackManager>,
    streamer: Arc<AudioStreamer>,
    zone_id: i64,
    session_id: String,
    depuis_ms: u64,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::audio::tap::RawWindow>,
    mut adopte: tokio::sync::oneshot::Receiver<u64>,
) {
    let mut tampon = std::collections::VecDeque::new();
    let mut tenu = std::time::Duration::ZERO;
    let mut ecarte = std::time::Duration::ZERO;
    let mut pompe_finie = false;
    let mut controle = tokio::time::interval(std::time::Duration::from_secs(2));
    let play_seq = loop {
        tokio::select! {
            r = &mut adopte => match r {
                Ok(seq) => break seq,
                Err(_) => return,
            },
            f = rx.recv(), if !pompe_finie => match f {
                Some(f) => {
                    tenu += f.window;
                    tampon.push_back(f);
                    while tenu > ATTENTE_MAX {
                        let Some(vieille) = tampon.pop_front() else { break };
                        tenu -= vieille.window;
                        ecarte += vieille.window;
                    }
                }
                None => pompe_finie = true,
            },
            _ = controle.tick() => {
                // Flux jamais adopté (file changée, zone arrêtée) : sa
                // session a disparu, l'attente aussi.
                let vivante = streamer.sessions_state().lock().await.contains_key(&session_id);
                if !vivante {
                    niveaux_en_attente().remove(&session_id);
                    return;
                }
            }
        }
    };
    let fwd = super::spawn_paced_levels_forwarder(
        bus,
        playback,
        zone_id,
        play_seq,
        (depuis_ms as i64).saturating_add(ecarte.as_millis() as i64),
    );
    for f in tampon {
        if fwd.send(f).is_err() {
            return;
        }
    }
    while let Some(f) = rx.recv().await {
        if fwd.send(f).is_err() {
            return;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// #5079 — une pompe par zone à la fois sur le disque
// ─────────────────────────────────────────────────────────────────────────
//
// GgB (fil 1945) : choisir une autre piste pendant la lecture coupe le son
// 3 à 4 s. Mesuré sur un SuperDrive (Genesis, *A Trick of the Tail*) : la
// piste visée arrive à ~5× le temps réel quand la pompe précédente est
// arrêtée, mais tombe à 0,55× quand elle lit encore, le bras sautant d'une
// position à l'autre (4 s de manque sur 5 s d'audio). Or la pompe d'une
// piste lit à pleine vitesse jusqu'à remplir son canal de 256 tronçons
// (~41 s de CD), et rien ne l'arrêtait avant qu'un `send` échoue, c'est-à-
// dire après que la nouvelle piste avait déjà démarré.

/// Une pompe de source PCM en cours, et de quoi l'arrêter.
struct PompeDeZone {
    zone_id: i64,
    arret: Arc<AtomicBool>,
    /// Tenu pendant chaque lecture du disque : l'arrêt l'attend, pour
    /// qu'aucune lecture de l'ancienne pompe ne chevauche l'ouverture.
    lecture: std::sync::Mutex<()>,
}

fn pompes() -> std::sync::MutexGuard<'static, HashMap<i64, Vec<Arc<PompeDeZone>>>> {
    static POMPES: std::sync::LazyLock<std::sync::Mutex<HashMap<i64, Vec<Arc<PompeDeZone>>>>> =
        std::sync::LazyLock::new(Default::default);
    POMPES.lock().unwrap_or_else(|e| e.into_inner())
}

impl PompeDeZone {
    fn inscrire(zone_id: i64) -> Arc<Self> {
        let p = Arc::new(Self {
            zone_id,
            arret: Arc::new(AtomicBool::new(false)),
            lecture: std::sync::Mutex::new(()),
        });
        pompes().entry(zone_id).or_default().push(p.clone());
        p
    }

    fn desinscrire(self: &Arc<Self>) {
        let mut m = pompes();
        if let Some(v) = m.get_mut(&self.zone_id) {
            v.retain(|p| !Arc::ptr_eq(p, self));
            if v.is_empty() {
                m.remove(&self.zone_id);
            }
        }
    }
}

/// Arrête les pompes de la zone et attend que leur lecture en cours finisse.
async fn arreter_les_pompes_de_la_zone(zone_id: i64) {
    let a_arreter = pompes().remove(&zone_id).unwrap_or_default();
    if a_arreter.is_empty() {
        return;
    }
    for p in &a_arreter {
        p.arret.store(true, Ordering::SeqCst);
    }
    let n = a_arreter.len();
    let _ = tokio::task::spawn_blocking(move || {
        for p in &a_arreter {
            drop(p.lecture.lock().unwrap_or_else(|e| e.into_inner()));
        }
    })
    .await;
    debug!(zone_id, pompes = n, "source_pcm_pompes_arretees");
}

/// Le flux du fournisseur, qui ne lit plus rien une fois sa pompe arrêtée.
struct LectureArretable {
    flux: Box<dyn std::io::Read + Send>,
    pompe: Arc<PompeDeZone>,
}

impl std::io::Read for LectureArretable {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let _garde = self.pompe.lecture.lock().unwrap_or_else(|e| e.into_inner());
        if self.pompe.arret.load(Ordering::SeqCst) {
            // Fin courte : la tâche voit l'arrêt et n'en fait pas une erreur.
            return Ok(0);
        }
        self.flux.read(buf)
    }
}

#[cfg(test)]
mod direct_5051 {
    //! #5051 — une source PCM EN DIRECT passe par l'orchestrateur comme une
    //! radio : session sans longueur, ni durée ni `Content-Length`, corps qui
    //! remet TOUT ce que le fournisseur rend, et le mode « longueur connue »
    //! inchangé à côté.

    use std::io::Read;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::sync::Mutex;

    use crate::db::migrations::run_migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::http::streamer::AudioStreamer;
    use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
    use crate::outputs::registry::OutputRegistry;
    use crate::playback::PlaybackManager;
    use crate::source_pcm::{
        Compensation, Consommation, EtatDirect, FluxDirect, FluxPcm, FormatPcm, FournisseurPcm,
        compensation_du_direct,
    };
    use crate::streaming::registry::ServiceRegistry;

    fn orchestrateur() -> PlaybackOrchestrator {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        PlaybackOrchestrator::new(
            Arc::new(db),
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            Some("127.0.0.1".into()),
        )
    }

    /// Rend `restants` blocs de 1 000 octets numérotés, puis la fin normale.
    struct Blocs {
        restants: u32,
        n: u8,
    }
    impl Read for Blocs {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.restants == 0 {
                return Ok(0);
            }
            self.restants -= 1;
            self.n = self.n.wrapping_add(1);
            let k = buf.len().min(1000);
            buf[..k].fill(self.n);
            Ok(k)
        }
    }

    struct Etat;
    impl EtatDirect for Etat {
        fn compensation(&self) -> Compensation {
            Compensation {
                methode: "tampon_avec_reprise",
                reechantillonne: false,
                reprises: 0,
                derive_ppm: Some(1.5),
            }
        }
    }

    struct Direct {
        blocs: u32,
        consommation_vue: Arc<AtomicU64>,
    }
    impl FournisseurPcm for Direct {
        fn ouvrir(&self, _: &str, _: u64) -> Result<FluxPcm, String> {
            panic!("une source en direct ne s'ouvre jamais en longueur connue");
        }
        fn en_direct(&self) -> bool {
            true
        }
        fn ouvrir_direct(&self, _: &str, c: Consommation) -> Result<FluxDirect, String> {
            self.consommation_vue
                .store(c.octets() + 1, Ordering::SeqCst);
            Ok(FluxDirect {
                format: FormatPcm {
                    frequence: 48_000,
                    canaux: 2,
                    bits: 24,
                },
                lecteur: Box::new(Blocs {
                    restants: self.blocs,
                    n: 0,
                }),
                etat: Some(Arc::new(Etat)),
            })
        }
    }

    fn demande() -> PlayRequest {
        PlayRequest {
            zone_id: 1,
            source: Some("entree-audio".into()),
            source_id: Some("Loopback Audio".into()),
            title: Some("Entrée audio — Loopback Audio".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn une_source_en_direct_est_servie_sans_longueur_comme_une_radio() {
        let orch = orchestrateur();
        let vue = Arc::new(AtomicU64::new(0));
        // Bien plus que les 8 tronçons du canal : la pompe ne s'arrête qu'à
        // la fin décidée par le lecteur.
        orch.sources_pcm().inscrire(
            "entree-audio",
            Arc::new(Direct {
                blocs: 300,
                consommation_vue: vue.clone(),
            }),
        );
        let r = orch.resolve_stream(&demande()).await.unwrap();
        assert_eq!(r.duration_ms, None, "un direct n'a pas de durée");
        assert_eq!(r.file_size, None, "un direct n'a pas de longueur");
        assert_eq!(r.source, "entree-audio");
        assert_eq!(
            (r.sample_rate, r.bit_depth, r.channels),
            (Some(48_000), Some(24), Some(2))
        );
        assert!(r.url.ends_with(".wav"), "{}", r.url);
        assert_eq!(
            vue.load(Ordering::SeqCst),
            1,
            "compteur de consommation passé"
        );
        let sid = r.stream_id.clone().unwrap();
        let session = orch.streamer.sessions_state().lock().await[&sid].clone();
        assert!(session.is_radio, "servie comme une radio : sans longueur");
        assert_eq!(session.info.bit_depth, 24);
        assert_eq!(session.detected_output_format(), Some((48_000, 2)));
        assert_eq!(
            compensation_du_direct(&sid).map(|c| c.methode),
            Some("tampon_avec_reprise")
        );
        let mut total = 0usize;
        let mut premiers = Vec::new();
        while let Some(t) = session.recv_chunk().await {
            if premiers.len() < 3 {
                premiers.push(t[0]);
            }
            total += t.len();
        }
        // Aucun en-tête dans le canal : c'est le corps HTTP d'une radio qui
        // écrit l'en-tête de longueur indéterminée.
        assert_eq!(premiers, vec![1, 2, 3]);
        assert_eq!(total, 300 * 1000);
        // Fin du flux : l'état de compensation est retiré.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(compensation_du_direct(&sid).is_none());
    }
}
