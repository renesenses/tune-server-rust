use super::*;

pub(super) struct ZonePollState {
    pub(super) gapless_sent: bool,
    pub(super) stopped_ticks: u8,
    /// Ticks consecutifs ou le renderer rapporte une URI qui n'est pas la
    /// notre. Trois d'affilee avant de parler : une transition de piste peut
    /// montrer un instant l'URI precedente.
    pub(super) tenue_etrangere_ticks: u8,
    /// Le conflit a deja ete signale pour cette generation de piste : on ne
    /// harcele pas l'utilisateur a chaque tick.
    pub(super) tenue_signalee: bool,
    /// Ticks to ignore Stopped state after a gapless advance, so the
    /// poller doesn't re-send play_from_queue to a renderer that already
    /// transitioned via SetNextAVTransportURI.
    pub(super) gapless_cooldown: u8,
    /// Consecutive poll failures — used for exponential backoff.
    /// After N failures, skip 2^min(N,4) ticks before retrying.
    pub(super) consecutive_errors: u8,
    pub(super) backoff_remaining: u8,
    /// Comptabilité du journal (#2566), sans effet sur les deux champs
    /// ci-dessus : le recul et le compte d'erreurs sont tenus par le site
    /// d'appel, avant elle, et ce sont eux que lisent `poll_failed_past_end`
    /// et l'arrêt de zone. Voir [`JournalSondage`].
    pub(super) journal: JournalSondage,
    pub(super) total_polls: u64,
    pub(super) total_errors: u64,
    pub(super) last_latency_ms: u32,
    pub(super) max_latency_ms: u32,
    pub(super) last_radio_poll: Instant,
    /// When SetNextAVTransportURI was sent — used to guard against
    /// false track-end detection during gapless transitions on renderers
    /// like Eversolo DMP-A6 that briefly report Stopped or reset position.
    pub(super) gapless_sent_at: Option<Instant>,
    /// Last polled position in milliseconds — used to detect position
    /// resets (jumps from >30s to <5s) that signal a gapless transition.
    pub(super) last_position_ms: u64,
    /// Peak position reached in the current track — high-water mark used
    /// to verify that enough of the track was actually played before
    /// accepting a gapless transition.
    pub(super) peak_position_ms: u64,
    /// Identity key (`decisions::scrobble_track_key`) of the track already
    /// scrobbled, latched once it crosses the Last.fm threshold (50% / 4 min)
    /// so it scrobbles exactly once. A plain boolean here was only reset on
    /// `track_generation` changes, which gapless advances skip — so every
    /// gapless-reached track was silently dropped (#1113). The identity key
    /// re-arms on any track change regardless of the advance path.
    pub(super) scrobbled_key: Option<String>,
    /// Tick counter for throttling DB position saves.
    pub(super) ticks_since_db_save: u64,
    /// When the current track started playing (wall clock).
    /// Used to reject false gapless transitions that happen too soon.
    pub(super) track_started_at: Option<Instant>,
    /// The `ZoneState::last_seek_at` instant we last folded into
    /// `track_started_at`. A user seek moves the play position without moving
    /// the wall clock, which starves every wall-clock guard downstream
    /// (`played_enough`, `ended_naturally_wall_ok`): seek to the end right
    /// after start and the real track end is rejected as a spurious
    /// renderer signal — playback just stops instead of advancing (DEvir,
    /// v0.9.0-rc4). On each NEW seek we rewind `track_started_at` by the seek
    /// target so `wall_elapsed` matches "played at 1x from the start" again.
    pub(super) last_seek_seen: Option<Instant>,
    /// Tracks the `ZoneState::track_generation` we last observed.
    /// When the generation changes (new track started via `play()`),
    /// we reset all per-track state so stale values from the previous
    /// track cannot trigger false gapless advances or premature track ends.
    pub(super) track_generation: u64,
    /// When the orchestrator loaded the current track (track_generation changed).
    /// Used for the startup grace period — DLNA renderers report Stopped while
    /// buffering a new stream, especially after transcoding delays.
    pub(super) track_loaded_at: Instant,
    /// Counts ticks where the output reports Playing but position_ms has
    /// reached or exceeded the known track duration.  After
    /// POSITION_PAST_END_TICKS consecutive ticks in this state, the poller
    /// treats the track as ended even though the output hasn't reported
    /// Stopped.  This handles local/cpal outputs where the playback thread
    /// may be slow to set `playing = false`.
    pub(super) past_end_ticks: u8,
    /// Set to true after `gapless_natural_end_advancing_metadata` — the poller
    /// advanced metadata expecting the renderer to auto-transition.  If the
    /// renderer stays Stopped after gapless_cooldown expires, this flag lets
    /// the poller detect the stuck state and force a play_from_queue.
    pub(super) gapless_advance_pending: bool,
    /// Counts Stopped ticks after gapless_cooldown expires while
    /// gapless_advance_pending is true.  When this reaches
    /// GAPLESS_STUCK_THRESHOLD, the poller gives up on the gapless
    /// transition and forces play_from_queue.
    pub(super) gapless_stuck_ticks: u8,
    pub(super) last_bytes_sent: u64,
    /// Consecutive `Playing` polls with neither renderer position nor served
    /// bytes progressing. See `decisions::dlna_playing_stall_eligible`.
    pub(super) playing_stall_ticks: u8,
    /// Ticks CONSECUTIFS ou la position rapportee est a la fin de la piste — ou
    /// au-dela — alors que l'appareil annonce toujours jouer. Voir
    /// [`decisions::position_au_dela_de_la_duree`] et [`DEPASSEMENT_DUREE_TICKS`].
    ///
    /// Compte dans le bras `Playing` uniquement : une zone en pause n'y passe
    /// pas, donc une pause de vingt minutes ne gonfle pas ce compteur — c'est
    /// precisement ce que l'horloge murale (`track_started_at`, jamais repliee
    /// a la reprise) ne sait pas faire. Remis a zero des que la position quitte
    /// la zone de fin, et a chaque changement de piste.
    pub(super) depassement_duree_ticks: u8,
    /// Latch par piste : l'incoherence a deja ete DITE une fois (journal +
    /// metrique de zone). Sans lui la boucle ecrirait la meme ligne chaque
    /// seconde pendant tout le temps que dure le blocage.
    pub(super) depassement_duree_signale: bool,
    /// Ticks pendant lesquels on a refusé de conclure à une fin naturelle parce
    /// que le flux servi était manifestement incomplet (voir
    /// STALL_DECLINE_MAX_TICKS). Remis à zéro à chaque changement de piste.
    pub(super) stall_declines: u8,
    pub(super) radio_stopped_ticks: u8,
    /// Last position (ms) the renderer reported on the previous radio poll.
    /// An advancing position means the renderer is actually streaming even
    /// when it (mis)reports TransportState=Stopped for a live source — the
    /// Yamaha R-N2000A does this on MP3 ICEcast streams (AAC plays fine).
    pub(super) last_radio_position_ms: u64,
    /// Last volume the renderer reported (0.0–1.0) on a previous poll. Used to
    /// distinguish a real external volume change (the value moved) from a
    /// renderer that persistently reports a stale default (e.g. Devialet at
    /// 50%), which must not overwrite the user's saved volume.
    pub(super) last_device_volume: Option<f64>,
    /// Per-track latch for the DLNA poll-fail wall-clock end-of-track fallback
    /// (`decisions::poll_failed_past_end`). The Err poll branch can't remove the
    /// poll state (it holds a live borrow), so this ensures the fallback fires at
    /// most once per track. Cleared on every track-generation change.
    pub(super) wall_clock_end_fired: bool,
    /// Instrumentation latch (#1239): last `should_arm_gapless` decision we
    /// emitted in the `gapless_arm_trace` INFO line for the current track. The
    /// trace fires only when this value flips (arming window opens/closes) —
    /// `None` on a fresh track forces one line per track — so it never spams at
    /// the ~1 s tick rate. Read-only diagnostic; drives no playback decision.
    pub(super) gapless_arm_logged: Option<bool>,
    /// Verrou par piste (#2394) : la position de file pour laquelle
    /// `prepare_gapless` a constaté « suivant DSD sur DLNA, gapless refusé ».
    /// Sans lui, la fenêtre d'armement re-résout la piste suivante À CHAQUE
    /// tick — création puis destruction d'une session fichier par seconde
    /// pendant toute la fin d'une piste DSD (constaté sur DMP-A8, 96
    /// occurrences en 2 h). On ne peut PAS poser `gapless_sent = true` comme
    /// pour la sortie exclusive : sur DLNA, ce drapeau active les détecteurs
    /// de transition (durée/position) et le DMP-A8 rapporte des durées
    /// inexactes — fausse transition garantie. Cleared au changement de
    /// génération et à chaque transition, comme `gapless_arm_logged`.
    pub(super) gapless_dsd_skip_pos: Option<i64>,
    /// La LIGNE de file (`queue_items.id`) que le renderer a ACCEPTEE comme
    /// piste suivante, et la position qu'elle occupait alors (#3026).
    ///
    /// L'armement ne laissait aucune trace de ce qu'il avait arme. A la
    /// transition, le poller avancait donc sur `next_position()` — l'index+1
    /// COURANT — qui n'est plus la piste armee des que la file a bouge entre
    /// les deux. Un « Lire ensuite » dans les 30 dernieres secondes suffit : le
    /// renderer joue ce qu'on lui a envoye, l'ecran nomme l'inseree, et le
    /// compteur de fin de piste adopte la duree de l'INSEREE — d'ou la coupure
    /// de l'audio reellement en cours (`dlna_frozen_end=true`, journal Sandro
    /// du 01/09 a 14:23:10).
    pub(super) gapless_armed: Option<ArmedNext>,
    /// Suivi de la FAMINE de l'anneau audio de cette zone (#3318).
    ///
    /// Le sondeur est le seul endroit qui relise ces compteurs à intervalle
    /// régulier ; sans lui, l'instant où l'anneau s'est vidé — le seul que
    /// l'auditeur entende — ne figure dans aucun journal. Voir
    /// [`decisions::SuiviFamine`].
    ///
    /// N'entre dans AUCUNE décision : rien ici n'arrête, ne relance ni
    /// n'avance quoi que ce soit. C'est un champ de constat.
    pub(super) famine: decisions::SuiviFamine,
    /// L'instant du dernier relévé versé à [`ZonePollState::famine`].
    ///
    /// L'horloge murale vit ICI et pas dans le suivi : celui-ci reste
    /// purement comptable, donc testable sans dormir. `None` tant qu'aucun
    /// relévé n'a été pris, ou après une remise à zéro (pause, arrêt, sortie
    /// sans anneau).
    pub(super) famine_releve_at: Option<Instant>,
    /// REF-9 (#2219) — l'état de lecture, en OMBRE, à côté des 39 champs.
    ///
    /// Écrit par les 22 transitions nommées de [`super::fsm::Transition`]
    /// que `tick` appelle juste après ses écritures de drapeaux ; vérifié
    /// contre les drapeaux par [`ZonePollState::coherent`] en fin de tour.
    /// AUCUNE décision de `tick` ne le lit.
    pub(super) etat: EtatDeLecture,
}

impl ZonePollState {
    /// Etat de sondage neuf pour une zone qui vient d'entrer en lecture.
    ///
    /// Etait construit en ligne, champ par champ, a un seul endroit. Il en
    /// faut desormais deux — la zone avec peripherique et celle sans — et
    /// recopier vingt-neuf champs est le genre de chose qui diverge en
    /// silence.
    pub(super) fn new(track_generation: u64) -> Self {
        Self {
            gapless_sent: false,
            stopped_ticks: 0,
            tenue_etrangere_ticks: 0,
            tenue_signalee: false,
            gapless_cooldown: 0,
            consecutive_errors: 0,
            backoff_remaining: 0,
            journal: JournalSondage::default(),
            total_polls: 0,
            total_errors: 0,
            last_latency_ms: 0,
            max_latency_ms: 0,
            last_radio_poll: Instant::now(),
            gapless_sent_at: None,
            last_position_ms: 0,
            peak_position_ms: 0,
            scrobbled_key: None,
            ticks_since_db_save: 0,
            track_started_at: None,
            last_seek_seen: None,
            track_generation: track_generation,
            track_loaded_at: Instant::now(),
            past_end_ticks: 0,
            gapless_advance_pending: false,
            gapless_stuck_ticks: 0,
            last_bytes_sent: 0,
            playing_stall_ticks: 0,
            depassement_duree_ticks: 0,
            depassement_duree_signale: false,
            stall_declines: 0,
            radio_stopped_ticks: 0,
            last_radio_position_ms: 0,
            last_device_volume: None,
            wall_clock_end_fired: false,
            gapless_arm_logged: None,
            gapless_dsd_skip_pos: None,
            gapless_armed: None,
            famine: decisions::SuiviFamine::default(),
            famine_releve_at: None,
            etat: EtatDeLecture::Neuve,
        }
    }
}

/// Issue de `prepare_gapless` : distinguer « rien à armer / échec (re-tenter
/// au prochain tick) » de « suivant DSD sur DLNA (inutile de re-tenter pour
/// cette position — verrou `gapless_dsd_skip_pos`, #2394) ».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GaplessPrep {
    /// Le renderer a accepte la piste suivante. Porte la LIGNE de file
    /// reellement envoyee (`None` si la file n'a pas su la rendre) : c'est
    /// elle, et non l'index, qui decide ou avancer a la transition (#3026).
    Armed(Option<ArmedNext>),
    DsdNextSkipped,
    NotArmed,
}

/// Ce que le renderer a ACCEPTE comme piste suivante — a distinguer de ce que
/// la file designe comme suivante : les deux divergent des qu'on insere (#3026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ArmedNext {
    /// `queue_items.id`. Stable quand `insert_at` decale les positions.
    pub(super) row_id: i64,
    /// La position occupee AU MOMENT de l'armement. Journalisee seule : elle
    /// dit de combien la file a glisse sous l'armement.
    pub(super) position: i64,
}

// ── REF-9 (#2219) — l'énumération d'états, en ombre ─────────────────────
//
// Huit variantes, une par ligne de la proposition de
// `docs/refonte/ref9-etats-du-sondeur.md`. Décisions du 12/09 (par défaut,
// arbitrage de Bertrand attendu) : « Arrêtée » est un état ; le second sens
// de `gapless_sent` sur sortie exclusive est une variante distincte
// (`Armement::Renonce`) ; le retrait depuis la branche d'erreur de sonde passe
// par une transition nommée (`FinParHorlogeMurale` → `Terminee`).
//
// Une variante ne porte que ce qui la DISTINGUE : la ligne armée, l'état
// d'avant l'arrêt, l'état d'avant la panne de sonde, le motif terminal. Les
// compteurs et les horloges que la proposition lui attribue restent dans les
// champs de `ZonePollState` tant que l'ombre ne pilote rien : les recopier
// ferait deux écrivains pour un même fait, et c'est précisément ce que
// l'invariant doit rendre impossible.

/// L'état de lecture d'une zone, tel que la machine à états le dira.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EtatDeLecture {
    /// Piste chargée, aucun échantillon honnête encore (grâce de
    /// chargement, `stale_start_position`). Aussi l'état d'une zone sans
    /// périphérique, que rien ne fait avancer.
    Neuve,
    /// Le renderer joue, rien n'est armé.
    Lecture,
    /// `SetNext` accepté — ou renoncé sur sortie exclusive.
    Armee { armement: Armement },
    /// Le renderer dit `Stopped` alors que Tune joue ; on compte
    /// (`stopped_ticks`). Porte l'état d'où l'on vient, car l'armement
    /// survit à l'arrêt (armé, garde expirée, `Stopped` × 5 → attente
    /// d'enchaînement).
    Arretee { depuis: Depuis },
    /// Les métadonnées sont prêtes à avancer ; on attend que le renderer
    /// rejoue pour confirmer l'enchaînement.
    AvancePendante,
    /// Flux sans fin : ni pic, ni fin, ni gapless.
    Radio,
    /// La sonde ne répond pas ; recul exponentiel. Porte l'état d'avant,
    /// que le prochain succès de sonde restitue.
    SondageEnEchec { precedent: Box<EtatDeLecture> },
    /// Terminal : l'orchestrateur agit et l'état est retiré — sauf sur la
    /// branche d'erreur de sonde, où l'emprunt l'interdit et où l'état
    /// survit avec le verrou `wall_clock_end_fired`.
    Terminee(Issue),
}

/// Ce que « armé » veut dire — les deux sens du drapeau `gapless_sent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Armement {
    /// `SetNextAVTransportURI` accepté (`tick.rs`, `GaplessPrep::Armed`).
    /// `ligne` : ce que le renderer a accepté, `None` si la file n'a pas su
    /// la rendre — la même valeur que `gapless_armed`.
    Accepte { ligne: Option<ArmedNext> },
    /// Sortie exclusive (ASIO / WASAPI exclusif) : on a renoncé à armer,
    /// rien n'est parti ; `gapless_sent` n'est levé que pour cesser de
    /// re-tenter (`gapless_skipped_exclusive_output`).
    Renonce,
}

/// D'où vient un arrêt compté.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Depuis {
    Lecture,
    Armee(Armement),
}

/// L'issue d'un état terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Issue {
    /// La piste est finie : `handle_track_end` enchaîne.
    Finie(MotifFin),
    /// La zone est coupée : `orchestrator.stop` (ou relance « démarrage
    /// mort »).
    Coupee(CauseDeCoupure),
}

/// Les cinq motifs de `decisions::motif_fin`, plus la fin prononcée sur
/// sonde en échec, qui n'a pas de motif aujourd'hui (aucun `track_end_gap`
/// n'est journalisé sur cette branche).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MotifFin {
    FinNaturelleApresArret,
    AvanceGaplessBloquee,
    PositionAuDelaDeLaFin,
    FinNaturelleLocale,
    DsdDlnaPicAtteint,
    HorlogeMuraleSurSondeEnEchec,
}

#[cfg(test)]
impl MotifFin {
    /// L'étiquette que `track_end_gap` journalise pour ce motif — ce que les
    /// témoins confrontent au `motif_fin_de_piste` que `tick` écrit.
    pub(super) fn etiquette(self) -> &'static str {
        match self {
            MotifFin::FinNaturelleApresArret => decisions::motif_fin::FIN_NATURELLE_APRES_STOPPED,
            MotifFin::AvanceGaplessBloquee => decisions::motif_fin::AVANCE_GAPLESS_BLOQUEE,
            MotifFin::PositionAuDelaDeLaFin => decisions::motif_fin::POSITION_AU_DELA_DE_LA_FIN,
            MotifFin::FinNaturelleLocale => decisions::motif_fin::FIN_NATURELLE_LOCALE,
            MotifFin::DsdDlnaPicAtteint => decisions::motif_fin::DSD_DLNA_PIC_ATTEINT,
            MotifFin::HorlogeMuraleSurSondeEnEchec => "dlna_poll_failed_wall_clock",
        }
    }
}

/// Les quatre causes d'arrêt de zone que `tick` prononce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CauseDeCoupure {
    /// `renderer_stalled_not_advancing_stopping_zone` : fin naturelle
    /// refusée dix fois sur un flux incomplet.
    RendererCale,
    /// `playback_failure_stopping_zone` : `Stopped` × 30 et compteur
    /// d'octets MESURÉ à sec.
    FluxASec,
    /// `dlna_playing_without_progress_stopping_zone` : `Playing` × 30 sans
    /// progrès ni octets.
    LectureSansProgres,
    /// `radio_renderer_stopped_giving_up` : six ticks `Stopped` sans
    /// position, ou station déjà refusée.
    RadioAbandonnee,
}

/// Ce que l'invariant a trouvé en désaccord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Incoherence {
    /// `coherent()` : l'état et un drapeau ne disent pas la même chose.
    Drapeau {
        etat: &'static str,
        drapeau: &'static str,
        attendu: String,
        lu: String,
    },
    /// `transition()` : cette transition n'est pas prévue depuis cet état.
    TransitionInattendue { etat: String, transition: String },
}

impl std::fmt::Display for Incoherence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Incoherence::Drapeau {
                etat,
                drapeau,
                attendu,
                lu,
            } => write!(f, "etat={etat} drapeau={drapeau} attendu={attendu} lu={lu}"),
            Incoherence::TransitionInattendue { etat, transition } => {
                write!(
                    f,
                    "transition_inattendue etat={etat} transition={transition}"
                )
            }
        }
    }
}

impl EtatDeLecture {
    /// Le nom de la variante, pour les rapports.
    pub(super) fn nom(&self) -> &'static str {
        match self {
            EtatDeLecture::Neuve => "Neuve",
            EtatDeLecture::Lecture => "Lecture",
            EtatDeLecture::Armee { .. } => "Armee",
            EtatDeLecture::Arretee { .. } => "Arretee",
            EtatDeLecture::AvancePendante => "AvancePendante",
            EtatDeLecture::Radio => "Radio",
            EtatDeLecture::SondageEnEchec { .. } => "SondageEnEchec",
            EtatDeLecture::Terminee(_) => "Terminee",
        }
    }
}

/// Les drapeaux que l'invariant confronte à l'état. Un relevé, pas l'état :
/// c'est ce qui permet de vérifier l'état d'AVANT une panne de sonde avec
/// les mêmes champs.
struct Drapeaux {
    gapless_sent: bool,
    gapless_advance_pending: bool,
    wall_clock_end_fired: bool,
    stopped_ticks: u8,
    gapless_armed: Option<ArmedNext>,
    gapless_sent_at_pose: bool,
    consecutive_errors: u8,
}

impl Drapeaux {
    fn attendre<T: PartialEq + std::fmt::Debug>(
        etat: &'static str,
        drapeau: &'static str,
        attendu: T,
        lu: T,
    ) -> Result<(), Incoherence> {
        if attendu == lu {
            Ok(())
        } else {
            Err(Incoherence::Drapeau {
                etat,
                drapeau,
                attendu: format!("{attendu:?}"),
                lu: format!("{lu:?}"),
            })
        }
    }

    /// Ce qu'un armement impose : `gapless_sent` levé, `gapless_armed`
    /// égal à la ligne acceptée (ou vide si l'on a renoncé).
    fn armement(&self, etat: &'static str, armement: Armement) -> Result<(), Incoherence> {
        Self::attendre(etat, "gapless_sent", true, self.gapless_sent)?;
        match armement {
            Armement::Accepte { ligne } => {
                Self::attendre(etat, "gapless_armed", ligne, self.gapless_armed)
            }
            Armement::Renonce => {
                Self::attendre(etat, "gapless_armed", None, self.gapless_armed)?;
                Self::attendre(etat, "gapless_sent_at", false, self.gapless_sent_at_pose)
            }
        }
    }

    /// Rien d'armé, rien de pendant : `Neuve`, `Lecture`, `Radio` et
    /// l'origine `Depuis::Lecture` d'un arrêt.
    fn desarme(&self, etat: &'static str) -> Result<(), Incoherence> {
        Self::attendre(etat, "gapless_sent", false, self.gapless_sent)?;
        Self::attendre(etat, "gapless_armed", None, self.gapless_armed)?;
        Self::attendre(etat, "gapless_sent_at", false, self.gapless_sent_at_pose)
    }

    /// La table de cohérence, état par état — sans bras `_`.
    fn disent(&self, etat: &EtatDeLecture) -> Result<(), Incoherence> {
        let nom = etat.nom();
        match etat {
            EtatDeLecture::Terminee(Issue::Finie(MotifFin::HorlogeMuraleSurSondeEnEchec)) => {
                // Le seul état terminal qui survive au tour : le verrou par
                // piste est levé, et il le reste jusqu'au changement de
                // génération.
                return Self::attendre(
                    nom,
                    "wall_clock_end_fired",
                    true,
                    self.wall_clock_end_fired,
                );
            }
            EtatDeLecture::Terminee(Issue::Finie(_))
            | EtatDeLecture::Terminee(Issue::Coupee(_)) => {
                // Retiré de `poll_states` dans le même tour : rien à confronter.
                return Ok(());
            }
            EtatDeLecture::Neuve
            | EtatDeLecture::Lecture
            | EtatDeLecture::Armee { .. }
            | EtatDeLecture::Arretee { .. }
            | EtatDeLecture::AvancePendante
            | EtatDeLecture::Radio
            | EtatDeLecture::SondageEnEchec { .. } => {}
        }
        // Hors état terminal, la fin par horloge murale n'a pas été prononcée.
        Self::attendre(
            nom,
            "wall_clock_end_fired",
            false,
            self.wall_clock_end_fired,
        )?;
        match etat {
            EtatDeLecture::Neuve | EtatDeLecture::Lecture | EtatDeLecture::Radio => {
                self.desarme(nom)?;
                Self::attendre(
                    nom,
                    "gapless_advance_pending",
                    false,
                    self.gapless_advance_pending,
                )?;
                Self::attendre(nom, "stopped_ticks", 0, self.stopped_ticks)
            }
            EtatDeLecture::Armee { armement } => {
                self.armement(nom, *armement)?;
                Self::attendre(
                    nom,
                    "gapless_advance_pending",
                    false,
                    self.gapless_advance_pending,
                )?;
                Self::attendre(nom, "stopped_ticks", 0, self.stopped_ticks)
            }
            EtatDeLecture::Arretee { depuis } => {
                match depuis {
                    Depuis::Lecture => self.desarme(nom)?,
                    Depuis::Armee(armement) => self.armement(nom, *armement)?,
                }
                Self::attendre(
                    nom,
                    "gapless_advance_pending",
                    false,
                    self.gapless_advance_pending,
                )?;
                Self::attendre(nom, "stopped_ticks > 0", true, self.stopped_ticks > 0)
            }
            EtatDeLecture::AvancePendante => {
                self.desarme(nom)?;
                Self::attendre(
                    nom,
                    "gapless_advance_pending",
                    true,
                    self.gapless_advance_pending,
                )?;
                Self::attendre(nom, "stopped_ticks", 0, self.stopped_ticks)
            }
            EtatDeLecture::SondageEnEchec { precedent } => {
                Self::attendre(
                    nom,
                    "consecutive_errors > 0",
                    true,
                    self.consecutive_errors > 0,
                )?;
                // Les drapeaux de lecture n'ont pas bougé pendant la panne :
                // l'état d'avant doit encore les décrire.
                self.disent(precedent)
            }
            EtatDeLecture::Terminee(_) => Ok(()),
        }
    }
}

impl ZonePollState {
    /// L'invariant REF-9 : `etat` et la combinaison de drapeaux disent la
    /// même chose. `Err` nomme l'état et le drapeau en désaccord.
    ///
    /// Vérifié sous `debug_assertions` à la fin de chaque tour de `tick`,
    /// et par chaque témoin de `temoins_de_transitions_ref9` après la
    /// transition qu'il rejoue. N'entre dans aucune décision.
    pub(super) fn coherent(&self) -> Result<(), Incoherence> {
        Drapeaux {
            gapless_sent: self.gapless_sent,
            gapless_advance_pending: self.gapless_advance_pending,
            wall_clock_end_fired: self.wall_clock_end_fired,
            stopped_ticks: self.stopped_ticks,
            gapless_armed: self.gapless_armed,
            gapless_sent_at_pose: self.gapless_sent_at.is_some(),
            consecutive_errors: self.consecutive_errors,
        }
        .disent(&self.etat)
    }
}
