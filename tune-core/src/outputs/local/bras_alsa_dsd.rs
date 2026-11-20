//! #8248 — le DSD **natif** de la sortie locale Linux : PCM ALSA `hw:` ouvert
//! en `DSD_U32_BE` ou `DSD_U32_LE`.
//!
//! Avant ce bras, une zone ALSA réglée « Natif » partait toujours en DoP :
//! seul le bras ASIO exclusif (Windows, #5643) savait recevoir le flux
//! `audio::dsd_brut`. Le pilote USB Audio de Linux expose pourtant le DSD
//! natif de nombreux DAC (Sabaj D5, fil 2730 : `DSD_U32_BE, DSD raw: DOP=0`).
//!
//! **Quand il est pris.** L'orchestrateur ne sert le flux DSD brut qu'à une
//! zone « Natif » dont la sortie est un PCM `hw:` sondé
//! ([`sonder_la_sortie`]) et dont le pilote accepte la cadence ET les voies
//! du fichier (`decider_dsd_natif_local_voies`). Le bras re-sonde le PCM à
//! l'ouverture (le pilote est libre à cet instant) : l'ordre des octets
//! (`DSD_U32_BE`/`_LE`) se décide là, par la même règle que cpal
//! (`dsd_u32_alsa::ordre_retenu`), et une cadence refusée est oubliée pour
//! que la piste suivante parte en DoP au lieu d'échouer encore.
//!
//! **Intouché.** Aucun volume, aucun ReplayGain, aucun DSP, aucune rampe :
//! le DSD natif n'a pas d'échantillon PCM à traiter (même règle que le DoP,
//! dont le volume est contourné, et que l'ASIO natif). Les octets `DsdU8`
//! du flux sont seulement rangés en mots de 32 bits (`dsd_u32_alsa`).
//!
//! **Repos.** En pause, en famine, à l'arrêt et avant chaque fermeture (donc
//! avant un déplacement, qui rouvre le flux), le rappel envoie le motif de
//! repos DSD 0x69 — jamais des zéros, qu'un DAC en mode DSD joue comme un
//! continu à pleine échelle.

use super::*;
use crate::audio::dsd_brut::{AnneauDsd, ENTETE_LEN, EnteteDsdBrut, SILENCE_DSD_MSB_FIRST};
use crate::audio::dsd_u32_alsa::{self as dsd_u32, Empaqueteur, OrdreDsdU32};

/// Ce qu'a rendu le sondage d'un PCM.
#[derive(Debug)]
pub(super) enum SondeDuPcm {
    /// Le PCM est tenu (EBUSY/EAGAIN) : rien n'est retenu.
    Occupe,
    /// Le PCM n'a pas pu être interrogé pour une autre raison.
    Erreur(String),
    /// Le PCM répond. `None` : aucun format `DSD_U32` offert.
    Repond(Option<dsd_u32::CapaciteDsdU32>),
}

/// Sonde le PCM ALSA `pcm` (nom sans préfixe d'hôte, `hw:CARD=…`) : les
/// formats `DSD_U32` offerts, l'ordre que cpal ouvrira, et les cadences DSD
/// acceptées sur `voies`. Ouvre le PCM en non bloquant et le referme : à
/// n'appeler que quand aucun flux de Tune ne le tient.
pub(super) fn sonder_le_pcm(pcm: &str, voies: u16) -> SondeDuPcm {
    use alsa::pcm::{Access, Format, HwParams, PCM};
    let p = match PCM::new(pcm, alsa::Direction::Playback, true) {
        Ok(p) => p,
        Err(e) if e.errno() == libc::EBUSY || e.errno() == libc::EAGAIN => {
            return SondeDuPcm::Occupe;
        }
        Err(e) => return SondeDuPcm::Erreur(e.to_string()),
    };
    let hw = match HwParams::any(&p) {
        Ok(hw) => hw,
        Err(e) => return SondeDuPcm::Erreur(e.to_string()),
    };
    // Même ordre de questions que cpal (`init_hw_params`) : le format se
    // choisit sur l'espace complet, avant les voies et la cadence.
    let petit = hw.test_format(Format::DSDU32LE).is_ok();
    let gros = hw.test_format(Format::DSDU32BE).is_ok();
    let Some(ordre) = dsd_u32::ordre_retenu(petit, gros) else {
        return SondeDuPcm::Repond(None);
    };
    let format = match ordre {
        OrdreDsdU32::GrosBoutiste => Format::DSDU32BE,
        OrdreDsdU32::PetitBoutiste => Format::DSDU32LE,
    };
    let cadences = if hw.set_access(Access::RWInterleaved).is_ok()
        && hw.set_format(format).is_ok()
        && hw.set_channels(u32::from(voies)).is_ok()
    {
        dsd_u32::cadences_offertes(|r| hw.test_rate(r).is_ok())
    } else {
        Vec::new()
    };
    SondeDuPcm::Repond(Some(dsd_u32::CapaciteDsdU32 {
        ordre,
        cadences,
        voies,
    }))
}

/// #9170 — pourquoi la sortie n'ouvre pas (ou pas maintenant) un PCM `hw:`.
#[derive(Debug)]
enum SansPcmMateriel {
    /// L'hôte audio n'est pas ALSA : structurel, aucun bras natif possible.
    HoteNonAlsa(String),
    /// Le nom a résolu vers un PCM logiciel (`plughw:`, `default`, PipeWire…).
    /// Observation de l'instant, pas une réponse du pilote : un `hw:` tenu
    /// ou pas encore énuméré peut laisser le nom résoudre ailleurs.
    PcmLogiciel(String),
    /// Périphérique introuvable, ou repli sur le périphérique par défaut :
    /// DAC débranché, éteint, pas encore énuméré. Ne dit RIEN du DAC.
    Introuvable(&'static str),
}

impl std::fmt::Display for SansPcmMateriel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HoteNonAlsa(h) => write!(f, "hôte {h}, pas ALSA"),
            Self::PcmLogiciel(id) => write!(f, "PCM « {id} » : pas un accès direct hw:"),
            Self::Introuvable(r) => f.write_str(r),
        }
    }
}

/// #9170 (relecture #9172) — ce qu'une ouverture native sans `hw:` laisse
/// dans `capacite_dsd_natif` : hôte non ALSA, définitivement « pas de
/// natif » ; PCM logiciel, « pas de natif » provisoire ; introuvable, la
/// capacité reste affichée mais devient provisoire. Dans les trois cas la
/// piste DSD suivante ne retente pas aveuglément le natif.
fn retenir_la_perte_du_hw(device_name: &str, raison: &SansPcmMateriel) {
    use crate::outputs::capacite_dsd_natif::{self as capacite, CapaciteConnue};
    match raison {
        SansPcmMateriel::HoteNonAlsa(_) => {
            capacite::retenir(device_name, CapaciteConnue::SortieNonAsio);
        }
        SansPcmMateriel::PcmLogiciel(id) if pcm_logiciel_generique(id) => {
            capacite::retenir(device_name, CapaciteConnue::SortieNonAsio);
        }
        SansPcmMateriel::PcmLogiciel(_) => {
            capacite::retrograder_en_provisoire(device_name);
            capacite::retenir_provisoire(device_name, CapaciteConnue::SortieNonAsio);
        }
        SansPcmMateriel::Introuvable(_) => capacite::retrograder_en_provisoire(device_name),
    }
}

/// #9170 (suivi #9172) — un PCM logiciel GÉNÉRIQUE, sans carte désignée
/// (`default`, `pipewire`, `pulse`, `dmix`, `sysdefault`, `plug`, `jack`…) :
/// le nom de la sortie désigne le mixeur du système, jamais un DAC, et ne
/// résoudra pas vers un `hw:` — verdict définitif, pas de re-sondage.
///
/// Un PCM logiciel qui désigne une carte (`default:CARD=DACZ8`,
/// `plughw:CARD=…`, `dmix:CARD=…`) reste provisoire : la même carte expose
/// aussi son `hw:`, que l'énumération peut préférer au sondage suivant (le
/// cas du fil 2951).
fn pcm_logiciel_generique(endpoint_id: &str) -> bool {
    let pcm = crate::outputs::pseudo_peripherique_alsa::pcm_alsa(endpoint_id);
    !pcm.is_empty() && !pcm.contains(':')
}

/// Le PCM ALSA matériel (`hw:`) qu'ouvrirait la sortie, sans repli.
/// `Err` dit pourquoi il n'y en a pas.
fn pcm_materiel(
    device_name: &str,
    endpoint_id: Option<&str>,
    origin_host: Option<&str>,
    audio_backend: &str,
) -> Result<(cpal::Device, String), SansPcmMateriel> {
    let host = select_host(audio_backend);
    let host_id_name: &'static str = host.id().name();
    if !host_id_name.eq_ignore_ascii_case("alsa") {
        return Err(SansPcmMateriel::HoteNonAlsa(host_id_name.to_string()));
    }
    let Some((device, fell_back)) =
        find_device_with_fallback(&host, device_name, endpoint_id, origin_host)
    else {
        return Err(SansPcmMateriel::Introuvable("périphérique introuvable"));
    };
    if fell_back {
        return Err(SansPcmMateriel::Introuvable(
            "repli sur le périphérique par défaut",
        ));
    }
    let id = device.id().map(|id| id.to_string()).unwrap_or_default();
    if !alsa_pcm_is_direct_hardware(&id) {
        return Err(SansPcmMateriel::PcmLogiciel(id));
    }
    let pcm = crate::outputs::pseudo_peripherique_alsa::pcm_alsa(&id).to_string();
    Ok((device, pcm))
}

/// Sonde la sortie `device_name` pour le DSD natif (orchestrateur et
/// `play_url`). Bloquant, bref : une ouverture non bloquante du PCM.
pub(super) fn sonder_la_sortie(
    device_name: &str,
    endpoint_id: Option<&str>,
    origin_host: Option<&str>,
    audio_backend: &str,
    voies: u16,
) -> ResultatSondeDsdAlsa {
    let pcm = match pcm_materiel(device_name, endpoint_id, origin_host, audio_backend) {
        Ok((_, pcm)) => {
            crate::outputs::capacite_dsd_natif::verdict_nouveau(device_name, None);
            pcm
        }
        Err(raison) => {
            // #9170 (fil 2951) — en INFO une fois par verdict : la cause
            // (nom résolu vers `alsa:default:CARD=…`, introuvable, repli) doit
            // se lire dans un rapport de testeur sans DEBUG.
            let texte = raison.to_string();
            if crate::outputs::capacite_dsd_natif::verdict_nouveau(device_name, Some(&texte)) {
                info!(device = %device_name, raison = %texte, "alsa_dsd_natif_pas_de_pcm_materiel");
            } else {
                debug!(device = %device_name, raison = %texte, "alsa_dsd_natif_pas_de_pcm_materiel");
            }
            return match raison {
                SansPcmMateriel::HoteNonAlsa(_) => ResultatSondeDsdAlsa::PasUnPcmMateriel,
                SansPcmMateriel::PcmLogiciel(id) if pcm_logiciel_generique(&id) => {
                    ResultatSondeDsdAlsa::PasUnPcmMateriel
                }
                SansPcmMateriel::PcmLogiciel(_) => ResultatSondeDsdAlsa::PcmLogiciel,
                SansPcmMateriel::Introuvable(_) => ResultatSondeDsdAlsa::Introuvable,
            };
        }
    };
    match sonder_le_pcm(&pcm, voies) {
        SondeDuPcm::Occupe => ResultatSondeDsdAlsa::Occupe,
        SondeDuPcm::Erreur(e) => {
            warn!(device = %device_name, pcm = %pcm, error = %e, "alsa_dsd_natif_sonde_impossible");
            ResultatSondeDsdAlsa::Occupe
        }
        SondeDuPcm::Repond(capacite) => {
            let (format, cadences) = capacite
                .map(|c| (c.ordre.nom_alsa(), c.cadences))
                .unwrap_or(("aucun", Vec::new()));
            info!(
                device = %device_name,
                pcm = %pcm,
                format,
                cadences = ?cadences,
                voies,
                "alsa_dsd_natif_capacite_sondee"
            );
            ResultatSondeDsdAlsa::Sonde { cadences, voies }
        }
    }
}

/// Ce que `play_url` confie au bras : un flux `audio::dsd_brut` dont
/// l'en-tête est déjà lu.
pub(super) struct EntreesDsdNatifAlsa {
    pub(super) device_name: String,
    pub(super) endpoint_id: Option<String>,
    pub(super) origin_host: Option<String>,
    pub(super) audio_backend: String,
    pub(super) url: String,
    pub(super) entete: EnteteDsdBrut,
    /// Les octets lus avec l'en-tête : ce qui suit `ENTETE_LEN` est le début
    /// du flux DSD.
    pub(super) header_buf: Vec<u8>,
    pub(super) reader: LecteurHttpAnnulable,
    /// Le producteur a déjà appliqué le déplacement : la position de départ
    /// n'est qu'un affichage.
    pub(super) seek_offset: u64,
    pub(super) my_generation: u64,
    pub(super) paused: Arc<AtomicBool>,
    pub(super) playing: Arc<AtomicBool>,
    pub(super) force_silent: Arc<AtomicBool>,
    pub(super) stop_rx: std::sync::mpsc::Receiver<()>,
    pub(super) open_failure: Arc<std::sync::Mutex<Option<String>>>,
    pub(super) position_ms: Arc<AtomicU64>,
    pub(super) play_generation: Arc<AtomicU64>,
    pub(super) track_ended_naturally: Arc<AtomicBool>,
    pub(super) track_ended_generation: Arc<AtomicU64>,
    pub(super) dop_active: Arc<AtomicBool>,
    pub(super) chain_exhausted: Arc<AtomicBool>,
    pub(super) device_gone: Arc<AtomicBool>,
    /// #8869 — le silence de verrouillage dont le flux est précédé.
    pub(super) silence_de_verrouillage:
        crate::audio::silence_de_verrouillage::SilenceDeVerrouillage,
}

/// Ce que le rappel écrit dans un tampon `DSD_U32` : le motif de repos tant
/// que `repos` (pause, arrêt demandé, fermeture imminente), sinon les mots de
/// l'anneau complétés par le motif en cas de famine. Rend `true` en famine.
///
/// Les tampons et l'anneau ne contiennent que des trames entières : la
/// lecture partielle d'une famine garde l'alignement des mots.
pub(super) fn remplir_le_tampon_dsd_u32(
    anneau: &AnneauDsd,
    repos: bool,
    tampon: &mut [u8],
) -> bool {
    crate::audio::dsd_brut::remplir_tampon_dsd(anneau, repos, tampon)
}

/// Durée de repos (motif 0x69) envoyée avant de fermer le PCM : assez pour
/// couvrir les périodes déjà dans le tampon du pilote.
const REPOS_AVANT_FERMETURE_MS: u64 = 120;

fn echouer(
    entrees_device: &str,
    message: String,
    open_failure: &std::sync::Mutex<Option<String>>,
    playing: &AtomicBool,
    play_generation: &AtomicU64,
    my_generation: u64,
) {
    warn!(device = %entrees_device, erreur = %message, "local_audio_alsa_dsd_natif_refuse");
    // #9170 — rien n'est parti en natif : la zone ne l'affiche pas.
    crate::outputs::capacite_dsd_natif::abandonner_transport_natif(&format!(
        "local:{entrees_device}"
    ));
    if play_generation.load(Ordering::SeqCst) == my_generation {
        if let Ok(mut slot) = open_failure.lock() {
            *slot = Some(format!("« {entrees_device} » : {message}"));
        }
        playing.store(false, Ordering::SeqCst);
    }
}

/// Joue un flux DSD brut sur le PCM `hw:` ouvert en `DSD_U32`, jusqu'à la fin
/// du flux ou l'ordre d'arrêt. Terminal. Pas d'enchaînement sans blanc : la
/// piste suivante rouvre le PCM (`chain_exhausted` levé dès l'ouverture).
pub(super) fn jouer_dsd_natif_via_alsa(entrees: EntreesDsdNatifAlsa) {
    let EntreesDsdNatifAlsa {
        device_name,
        endpoint_id,
        origin_host,
        audio_backend,
        url,
        entete,
        header_buf,
        mut reader,
        seek_offset,
        my_generation,
        paused,
        playing,
        force_silent,
        stop_rx,
        open_failure,
        position_ms,
        play_generation,
        track_ended_naturally,
        track_ended_generation,
        dop_active,
        chain_exhausted,
        device_gone,
        silence_de_verrouillage,
    } = entrees;
    let canaux = usize::from(entete.canaux);

    // Aucun DoP ici : l'état d'une piste précédente ne doit pas survivre.
    dop_active.store(false, Ordering::SeqCst);

    // ── Ouverture : re-sondage, puis flux cpal `DsdU32`. ──
    let (device, pcm) = match pcm_materiel(
        &device_name,
        endpoint_id.as_deref(),
        origin_host.as_deref(),
        &audio_backend,
    ) {
        Ok(v) => v,
        Err(raison) => {
            // #9170 (relecture #9172) — le nom ne mène plus au `hw:` qui avait
            // prouvé sa capacité : sans cela, chaque piste native échouerait
            // ici jusqu'au redémarrage. La piste suivante re-sonde.
            retenir_la_perte_du_hw(&device_name, &raison);
            echouer(
                &device_name,
                format!("flux DSD natif reçu, mais pas de PCM ALSA matériel ({raison})."),
                &open_failure,
                &playing,
                &play_generation,
                my_generation,
            );
            return;
        }
    };
    let (ordre, cadences_sondees) = match sonder_le_pcm(&pcm, entete.canaux) {
        SondeDuPcm::Repond(Some(c)) if c.cadences.contains(&entete.cadence) => {
            (c.ordre, c.cadences)
        }
        autre => {
            // #9170 (relecture #9172) — le pilote a répondu : sa réponse
            // entière remplace la capacité retenue (un autre DAC sous le même
            // nom), pour que la piste suivante parte en DoP dès ce premier
            // échec.
            if let SondeDuPcm::Repond(reponse) = &autre {
                crate::outputs::capacite_dsd_natif::retenir_reponse_du_pilote(
                    &device_name,
                    entete.canaux == crate::outputs::local::VOIES_SONDEES_DSD_ALSA,
                    entete.cadence,
                    reponse
                        .as_ref()
                        .map(|c| c.cadences.clone())
                        .unwrap_or_default(),
                );
            }
            echouer(
                &device_name,
                format!(
                    "le pilote n'accepte pas le DSD natif à {} Hz sur {} voies ({autre:?}) ; \
                     la piste suivante partira en DoP.",
                    entete.cadence, entete.canaux
                ),
                &open_failure,
                &playing,
                &play_generation,
                my_generation,
            );
            return;
        }
    };
    let cadence_de_trame = dsd_u32::cadence_de_trame(entete.cadence);
    let mut empaqueteur = Empaqueteur::new(canaux, ordre);
    let octets_par_trame = empaqueteur.octets_par_trame();
    // Deux secondes, en trames entières (le DSD et ses mots ont le même
    // nombre d'octets).
    let anneau = Arc::new(AnneauDsd::new(
        AnneauDsd::contenance_deux_secondes(entete.cadence, entete.canaux) / octets_par_trame
            * octets_par_trame,
    ));
    let repos_avant_fermeture = Arc::new(AtomicBool::new(false));
    let config = cpal::StreamConfig {
        channels: entete.canaux,
        sample_rate: cadence_de_trame,
        buffer_size: cpal::BufferSize::Default,
    };
    let anneau_rappel = anneau.clone();
    let paused_rappel = paused.clone();
    let silence_rappel = force_silent.clone();
    let fermeture_rappel = repos_avant_fermeture.clone();
    let famines = Arc::new(AtomicU64::new(0));
    let famines_rappel = famines.clone();
    let disparu = device_gone.clone();
    // Même promotion temps réel que les autres chemins de rendu, à la première
    // période de chaque fil cpal neuf.
    let mut promotion = PromotionDuFilDeRendu::nouvelle();
    let stream = match device.build_output_stream_raw(
        &config,
        cpal::SampleFormat::DsdU32,
        move |data: &mut cpal::Data, _: &cpal::OutputCallbackInfo| {
            promotion.a_la_premiere_periode();
            let repos = paused_rappel.load(Ordering::Relaxed)
                || silence_rappel.load(Ordering::Relaxed)
                || fermeture_rappel.load(Ordering::Relaxed);
            if remplir_le_tampon_dsd_u32(&anneau_rappel, repos, data.bytes_mut()) {
                famines_rappel.fetch_add(1, Ordering::Relaxed);
            }
        },
        move |e| {
            warn!(error = %e, "local_audio_alsa_dsd_natif_stream_error");
            disparu.store(true, Ordering::SeqCst);
        },
        None,
    ) {
        Ok(s) => s,
        Err(e) => {
            // Le pilote vient de répondre (re-sondage ci-dessus) puis a refusé
            // d'ouvrir cette cadence : sa réponse, moins elle, fait foi.
            crate::outputs::capacite_dsd_natif::retenir_reponse_du_pilote(
                &device_name,
                entete.canaux == crate::outputs::local::VOIES_SONDEES_DSD_ALSA,
                entete.cadence,
                cadences_sondees.clone(),
            );
            echouer(
                &device_name,
                format!(
                    "ouverture {} à {cadence_de_trame} Hz refusée : {e} ; la piste suivante \
                     partira en DoP.",
                    ordre.nom_alsa()
                ),
                &open_failure,
                &playing,
                &play_generation,
                my_generation,
            );
            return;
        }
    };
    if let Err(e) = stream.play() {
        echouer(
            &device_name,
            format!("démarrage du flux DSD natif refusé : {e}"),
            &open_failure,
            &playing,
            &play_generation,
            my_generation,
        );
        return;
    }
    if doit_declarer_chaine_epuisee(
        force_silent.load(Ordering::Relaxed),
        play_generation.load(Ordering::SeqCst),
        my_generation,
    ) {
        chain_exhausted.store(true, Ordering::SeqCst);
    }
    info!(
        device = %device_name,
        pcm = %pcm,
        url = %url,
        format = ordre.nom_alsa(),
        cadence_dsd = entete.cadence,
        cadence_de_trame,
        canaux,
        "local_audio_alsa_dsd_natif_playing"
    );
    note_opened_device("ALSA", &device_name, &pcm, Some(pcm.as_str()));
    // #9170 — le DSD natif part pour de vrai : la capacité retenue le dit,
    // quel que soit le verdict d'un sondage antérieur (et donc le texte des
    // réglages aussi). Sondé sur les voies du fichier : un stéréo, le cas
    // des DAC USB, est la valeur que l'orchestrateur consulte.
    if entete.canaux == crate::outputs::local::VOIES_SONDEES_DSD_ALSA {
        crate::outputs::capacite_dsd_natif::confirmer_natif_alsa(&device_name, cadences_sondees);
    }
    // Et la zone qui attendait ce natif l'affiche, maintenant qu'il est réel.
    crate::outputs::capacite_dsd_natif::confirmer_transport_natif(&format!("local:{device_name}"));

    // #8869 — au changement de format, du repos DSD avant le premier octet.
    super::silence_de_verrouillage_8869::pousser_dans_l_anneau_dsd(
        &silence_de_verrouillage,
        &anneau,
        entete.cadence,
        entete.canaux,
        octets_par_trame,
        || force_silent.load(Ordering::Relaxed) || device_gone.load(Ordering::Relaxed),
    );
    // ── Alimentation : DsdU8 → mots DSD_U32 → anneau. ──
    let mut mots: Vec<u8> = Vec::new();
    empaqueteur.pousser(header_buf.get(ENTETE_LEN..).unwrap_or_default(), &mut mots);
    let mut octets_par_canal: u64 = 0;
    let mut tampon = vec![0u8; 65536];
    let mut fin_de_flux = false;
    let arret = |stop_rx: &std::sync::mpsc::Receiver<()>| {
        stop_rx.try_recv().is_ok()
            || force_silent.load(Ordering::Relaxed)
            || device_gone.load(Ordering::Relaxed)
    };
    'alimentation: loop {
        while mots.len() >= octets_par_trame {
            if arret(&stop_rx) {
                break 'alimentation;
            }
            let n = anneau.pousser_trames(&mots, octets_par_trame);
            if n == 0 {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            mots.drain(..n);
            octets_par_canal += (n / canaux) as u64;
            let joue = octets_par_canal.saturating_sub((anneau.disponible() / canaux) as u64);
            position_ms.store(
                seek_offset + entete.ms_pour_octets_par_canal(joue),
                Ordering::Relaxed,
            );
        }
        if arret(&stop_rx) {
            break;
        }
        match std::io::Read::read(&mut reader, &mut tampon) {
            Ok(0) => {
                fin_de_flux = true;
                break;
            }
            Ok(n) => empaqueteur.pousser(&tampon[..n], &mut mots),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(e) => {
                warn!(error = %e, device = %device_name, "local_audio_alsa_dsd_natif_read_error");
                break;
            }
        }
    }
    drop(reader);

    if fin_de_flux {
        track_ended_naturally.store(true, Ordering::SeqCst);
        track_ended_generation.store(my_generation, Ordering::SeqCst);
        TRACK_END_NOTIFY.notify_one();
        // Laisser jouer ce que l'anneau retient, borné : 4 s, et abandon si
        // rien ne bouge en 1,5 s (pilote figé).
        let echeance = std::time::Instant::now() + std::time::Duration::from_secs(4);
        let mut dernier = anneau.disponible();
        let mut progres = std::time::Instant::now();
        while anneau.disponible() > 0 && std::time::Instant::now() < echeance {
            if arret(&stop_rx) {
                break;
            }
            let dispo = anneau.disponible();
            if dispo < dernier {
                dernier = dispo;
                progres = std::time::Instant::now();
            } else if progres.elapsed() >= std::time::Duration::from_millis(1500) {
                warn!(device = %device_name, restant = dispo, "alsa_dsd_natif_drain_timeout");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        position_ms.store(
            seek_offset + entete.ms_pour_octets_par_canal(octets_par_canal),
            Ordering::Relaxed,
        );
    }

    // Repos DSD avant de rendre le PCM : le dernier tampon du pilote ne doit
    // pas être un fragment de musique coupé ni des zéros.
    repos_avant_fermeture.store(true, Ordering::SeqCst);
    if !device_gone.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(REPOS_AVANT_FERMETURE_MS));
    }
    drop(stream);
    if play_generation.load(Ordering::SeqCst) == my_generation {
        playing.store(false, Ordering::SeqCst);
    }
    info!(
        device = %device_name,
        octets_par_canal,
        famines = famines.load(Ordering::Relaxed),
        repos = SILENCE_DSD_MSB_FIRST,
        "local_audio_alsa_dsd_natif_stopped"
    );
}

#[cfg(test)]
mod perte_du_hw_9172 {
    use super::{SansPcmMateriel, retenir_la_perte_du_hw};
    use crate::outputs::capacite_dsd_natif::{CapaciteConnue, a_resonder, connue, retenir};

    const DSD512: u32 = 22_579_200;

    fn prouve(nom: &str) {
        retenir(
            nom,
            CapaciteConnue::AlsaDsdU32 {
                cadences: vec![DSD512],
            },
        );
    }

    /// Relecture #9172 : le nom d'un DAC prouvé natif résout maintenant vers
    /// un PCM logiciel — « pas de natif » provisoire, la piste suivante part
    /// en DoP et la suivante encore re-sonde.
    #[test]
    fn un_pcm_logiciel_a_l_ouverture_retire_le_natif_prouve() {
        let nom = "9172 Z8 devenu plughw";
        prouve(nom);
        retenir_la_perte_du_hw(nom, &SansPcmMateriel::PcmLogiciel("plughw:CARD=Z8".into()));
        assert_eq!(connue(nom), Some(CapaciteConnue::SortieNonAsio));
        assert!(a_resonder(nom));
    }

    /// DAC introuvable à l'ouverture : capacité gardée mais re-sondée.
    #[test]
    fn un_dac_introuvable_a_l_ouverture_rend_la_capacite_provisoire() {
        let nom = "9172 Z8 debranche";
        prouve(nom);
        assert!(!a_resonder(nom));
        retenir_la_perte_du_hw(
            nom,
            &SansPcmMateriel::Introuvable("périphérique introuvable"),
        );
        assert!(a_resonder(nom), "la piste DSD suivante doit re-sonder");
        assert_eq!(
            connue(nom),
            Some(CapaciteConnue::AlsaDsdU32 {
                cadences: vec![DSD512]
            })
        );
    }

    /// Suivi #9172 : `default`, `pipewire`… sans carte sont définitifs ;
    /// un PCM logiciel qui désigne une carte reste provisoire (fil 2951).
    #[test]
    fn un_pcm_logiciel_generique_est_definitif() {
        use super::pcm_logiciel_generique;
        for id in [
            "alsa:default",
            "pipewire",
            "alsa:pulse",
            "dmix",
            "sysdefault",
            "plug",
        ] {
            assert!(pcm_logiciel_generique(id), "{id} : générique attendu");
        }
        for id in [
            "alsa:default:CARD=DACZ8",
            "plughw:CARD=Z8,DEV=0",
            "dmix:CARD=Z8,DEV=0",
            "sysdefault:CARD=Z8",
        ] {
            assert!(!pcm_logiciel_generique(id), "{id} : désigne une carte");
        }
        let nom = "9172 sortie pipewire";
        prouve(nom);
        retenir_la_perte_du_hw(nom, &SansPcmMateriel::PcmLogiciel("alsa:pipewire".into()));
        assert_eq!(connue(nom), Some(CapaciteConnue::SortieNonAsio));
        assert!(!a_resonder(nom), "un PCM générique ne se re-sonde pas");
    }
}
