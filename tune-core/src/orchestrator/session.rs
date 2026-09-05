use super::*;

/// Après une recréation de flux à une position donnée, faut-il ENCORE envoyer
/// un `Seek` à la sortie ?
///
/// # Le défaut #2893
///
/// [`PlaybackOrchestrator::replay_zone_at_position`] recrée le flux avec
/// `PlayRequest { seek_ms: Some(position_ms), .. }` et s'arrête là. Or
/// `seek_ms` n'est honoré que par les deux bras qui **décodent** —
/// `decode_to_pcm_streaming_seeked` reçoit l'offset et démarre le PCM à la
/// bonne seconde. Tous les autres bras posent le flux dans une **session
/// fichier** (transcodage vers un temporaire, cache, passthrough natif) ou une
/// **session proxy** (CDN) : ces sessions servent depuis l'**octet 0** et ne
/// regardent jamais `seek_ms`.
///
/// Et ces deux familles sont exactement les deux moitiés de
/// `is_seekable_session` :
///
/// | session | `seek_ms` honoré | « range-seekable » |
/// |---|---|---|
/// | décodée (canal mpsc) | **oui**, par le producteur | non |
/// | fichier / proxy | **non**, servie depuis 0 | **oui** |
///
/// D'où la règle : une session seekable sur une sortie réseau a été recréée au
/// début du morceau, et il ne manque plus QUE le `Seek` SOAP pour amener le
/// renderer à la position. Une session non seekable, elle, part déjà de
/// l'offset — lui envoyer un `Seek` **doublerait** le saut, la panne de la
/// famille #1518 (« un seek à 4:30 jetait tout le PCM restant → silence
/// total »).
///
/// C'est le miroir de la condition de [`PlaybackOrchestrator::seek`], et non sa
/// copie : là-bas une session seekable se contente d'un `Seek` **sans**
/// recréation ; ici la recréation a déjà eu lieu et impose le `Seek`.
///
/// Symptôme corrigé : sur un Marantz ND8006 en DLNA, basculer le mode Pure
/// faisait repartir le morceau du début — dans les deux sens, puisque les deux
/// sens changent de bras de streaming sans jamais quitter la session fichier
/// (Jean Valjean, 0.9.126, fil 1618).
///
/// Fonction pure : la matrice de décision se teste sans orchestrateur, comme
/// [`use_file_transcode_for`] et [`streaming_needs_pretranscode`].
pub(super) fn replay_needs_output_seek(
    is_network_output: bool,
    session_is_range_seekable: bool,
    position_ms: u64,
) -> bool {
    is_network_output && session_is_range_seekable && position_ms > 0
}

/// Ce qu'une reprise doit faire de la SESSION DE FLUX d'une zone.
///
/// Reprendre « sur place » suppose que la session qui alimentait la sortie a
/// survécu à la pause. Il y a deux façons de ne pas y survivre, et surtout DEUX
/// REMÈDES qui n'ont rien à voir l'un avec l'autre. C'est tout l'objet de ce
/// type : le premier existait déjà, le second manquait, et les confondre aurait
/// été un défaut de plus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepriseDeSession {
    /// La session vit : `checked_resume` sur la sortie suffit, et c'est ce qui
    /// marche aujourd'hui.
    SurPlace,
    /// RADIO seulement (#1629). Un flux radio est un DIRECT : on ré-amorce la
    /// station, position JETÉE. On reprend le direct, pas un différé de dix-neuf
    /// minutes.
    RejouerLeDirect,
    /// PISTE (#2512). Rétablir la MÊME écoute au MÊME point. Rejouer une piste
    /// « depuis le direct » n'aurait aucun sens : l'auditeur veut retrouver son
    /// morceau là où il l'a laissé.
    RetablirALaPosition,
    /// La session est morte et rien ne permet de la rétablir. Il reste à le
    /// DIRE : un silence sans message est un défaut à lui seul.
    Expliquer,
}

/// La matrice de décision de [`PlaybackOrchestrator::resume`].
///
/// Fonction PURE, et c'est ce qui permet de l'éprouver : `paused_at` est un
/// `std::time::Instant` que `tokio::time::pause()` n'atteint pas, donc une pause
/// de vingt minutes ne se joue pas dans un test — mais un booléen, si.
///
/// La branche RADIO est la table de vérité de #1629, inchangée : rejouer dès que
/// la pause dépasse le seuil OU que le producteur de décodage est mort, jamais
/// sans URL de station. Elle a été écrite contre un cas mesuré et elle
/// fonctionne ; elle n'est ni généralisée, ni dupliquée, ni contournée.
///
/// La branche PISTE ne regarde PAS `pause_longue`. C'est délibéré et c'est le
/// cœur du correctif : une piste dont la session vit encore reprend sur place,
/// qu'on l'ait laissée trente secondes ou trois heures. Seule la mort de la
/// session — le ramasse-miettes est passé — justifie de rétablir quoi que ce
/// soit, et alors on rétablit à la position, pas au début.
pub(crate) fn reprise_de_session(
    est_radio: bool,
    rejouable: bool,
    pause_longue: bool,
    session_morte: bool,
) -> RepriseDeSession {
    if est_radio {
        if rejouable && (pause_longue || session_morte) {
            RepriseDeSession::RejouerLeDirect
        } else {
            RepriseDeSession::SurPlace
        }
    } else if !session_morte {
        RepriseDeSession::SurPlace
    } else if rejouable {
        RepriseDeSession::RetablirALaPosition
    } else {
        RepriseDeSession::Expliquer
    }
}

/// La demande de lecture qui RÉTABLIT la session d'une piste au point exact où
/// la pause l'a laissée.
///
/// C'est ce qui sépare ce correctif d'une transposition du comportement radio.
/// Le re-play d'une station jette la position — il le doit. Ici la position est
/// le cœur de la demande : `seek_ms` porte le `position_ms` que l'état de zone a
/// conservé à travers la pause. Tout le reste désigne la même écoute, d'où
/// `play_without_history` chez l'appelant : pas de seconde ligne d'historique,
/// même règle que le re-play radio.
///
/// Les champs de résolution restent `None` : `play_inner` re-résout la piste
/// depuis `track_id`/`source_id` comme au premier lancement, et une valeur
/// recopiée ici ne pourrait que le contredire.
///
/// Fonction PURE : le contrat « la MÊME piste, au MÊME point » se prouve sans
/// orchestrateur, sans sortie et sans fichier.
pub(crate) fn requete_de_retablissement(
    zone_id: i64,
    output_device_id: String,
    np: &NowPlaying,
    position_ms: u64,
) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(output_device_id),
        track_id: np.track_id,
        source: Some(np.source.clone()),
        source_id: np.source_id.clone(),
        title: Some(np.title.clone()),
        artist_name: np.artist_name.clone(),
        album_title: np.album_title.clone(),
        cover_url: np.cover_path.clone(),
        duration_ms: (np.duration_ms > 0).then_some(np.duration_ms),
        seek_ms: Some(position_ms),
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    }
}

/// La phrase que la zone rend quand sa session n'a pas survécu à la pause et
/// n'a pas pu être rétablie.
///
/// Elle existe parce que l'absence de message EST le défaut : « aucun son,
/// volume dans le vide » et pas une ligne pour dire pourquoi. Elle nomme donc
/// les trois choses que l'auditeur ne peut pas deviner — quelle piste, à quelle
/// position, et pourquoi la session n'est plus là.
///
/// `position_ms` est FACULTATIF (#3244). Sur une zone navigateur personne ne
/// mesure la position — le sondeur ne passe pas — et `position_ms` y vaut 0
/// depuis `play()`. Écrire « ne peut pas reprendre à 0:00 » ferait passer cette
/// absence de mesure pour une mesure, et désignerait le début du morceau alors
/// que l'auditeur en était peut-être à la moitié. `None` dit « je ne sais pas »
/// et la phrase le dit aussi : c'est la même distinction que
/// [`PlaybackOrchestrator::position_entretenue_par_le_sondeur`] pose pour
/// #2595, au site voisin.
///
/// Fonction PURE, éprouvée sans orchestrateur.
pub(crate) fn message_session_perdue(
    titre: &str,
    position_ms: Option<u64>,
    cause: Option<&str>,
) -> String {
    let minutes = crate::http::streamer::SESSION_IDLE_TIMEOUT.as_secs() / 60;
    let mut phrase = match position_ms {
        Some(ms) => {
            let secondes = ms / 1000;
            format!(
                "La lecture de « {titre} » ne peut pas reprendre à {}:{:02} : sa session de \
                 flux n'a pas survécu à la pause (le serveur la libère après {minutes} minutes \
                 sans lecture). Relancez la piste.",
                secondes / 60,
                secondes % 60,
            )
        }
        // Position non mesurée : on nomme la piste et la cause, jamais un
        // horodatage inventé.
        None => format!(
            "La lecture de « {titre} » ne peut pas reprendre là où elle en était : sa \
             session de flux n'a pas survécu à la pause (le serveur la libère après \
             {minutes} minutes sans lecture), et cette zone est lue par le navigateur — \
             le serveur n'y mesure pas la position de lecture. Relancez la piste."
        ),
    };
    if let Some(cause) = cause {
        phrase.push_str(&format!(" Cause : {cause}"));
    }
    phrase
}

/// Temporisation avant le `Seek` qui suit une recréation de flux réseau.
///
/// Même valeur, et même raison, que la branche réseau de
/// [`PlaybackOrchestrator::seek`] : le renderer vient de recevoir une URL
/// neuve, il faut lui laisser commencer à bufferiser avant de lui demander de
/// sauter. Le ND8006 fait déjà des `soap_retry` sur `GetTransportInfo` dans ces
/// instants-là.
pub(super) const REPLAY_OUTPUT_SEEK_SETTLE_MS: u64 = 500;

/// Temps de pose avant le seek qui suit une REPRISE sur un renderer réseau
/// (DLNA, OpenHome) : le renderer redémarre son transport, il faut le laisser
/// repartir avant de lui demander de sauter (LAT-P2 : ce temps ne retient plus
/// la réponse, il court dans une tâche détachée).
pub(super) const RESUME_OUTPUT_SEEK_SETTLE_MS: u64 = 700;

/// Le seek détaché n'a de sens que pour la lecture qui l'a demandé : entre le
/// départ de la tâche et la fin de son temps de pose, un stop, un next ou une
/// nouvelle lecture peuvent être passés — il seekerait alors la piste
/// SUIVANTE. La génération de lecture (`play_seq`) capturée au départ doit
/// être celle du moment du seek.
pub(super) fn reprise_toujours_la_notre(seq_au_depart: u64, seq_courante: u64) -> bool {
    seq_au_depart == seq_courante
}
