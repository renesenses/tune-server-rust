//! #6132 — la sortie double branchée sur le chemin partagé cpal.
//!
//! La zone joue sur son périphérique (A) comme d'habitude. Quand la zone est
//! groupée avec une autre zone locale de la machine, la route de groupement
//! pose `zone_{id}_sortie_double` ; le fil de lecture ouvre alors en plus le
//! périphérique B, en mode partagé, et installe un [`Recopieur`] dans
//! l'emplacement que le rappel de A consulte à chaque période : ce que A joue
//! part aussi vers B, l'horloge de B asservie à celle de A
//! (`outputs::sortie_double`).
//!
//! Étape 2 :
//!
//! - **latence matérielle** : chaque rappel lit dans ses horodatages cpal ce
//!   que sa carte met entre le rappel et le convertisseur
//!   (`playback − callback`) ; le recopieur nourrit B d'autant plus tôt que sa
//!   latence dépasse celle de A. `sync_delay_ms` des deux zones s'y ajoute en
//!   réglage de secours (pilote sans horodatage) ;
//! - **volume par membre** : quand B joue, A rend sa période au volume unité,
//!   la recopie, puis applique SON volume ; B applique le volume de SA zone
//!   (curseur de la zone B, ReplayGain et compensation de la piste de A).
//!
//! Limites, dites dans le journal plutôt que tues :
//!
//! - **ASIO** : un seul flux dans tout le processus (`ASIO_DEVICE_LOCK`) ;
//! - **mode exclusif** (WASAPI, CoreAudio), chemin compressé et format entier :
//!   la seconde sortie n'est ouverte que sur le chemin partagé en `f32` ;
//! - **« bit-perfect strict »** : B est rééchantillonné (de quelques ppm) ;
//! - **DoP** : un flux DoP rééchantillonné n'est plus du DSD, B se tait ;
//! - B doit accepter le même nombre de canaux que A.

use super::*;
use crate::outputs::sortie_double::{
    AnneauSecondaire, CONSIGNE_PAR_DEFAUT_S, RETARD_MAX_S, Recopieur, SecondeSortieDemandee,
    SortieMultiple,
};
use std::sync::atomic::AtomicUsize;

/// « Pas de valeur » dans les atomiques de latence et d'instant.
const AUCUNE: u64 = u64::MAX;

/// L'anneau SPSC de B, et ce que le rappel de B publie pour le recopieur.
pub(super) struct AnneauDeB {
    anneau: RingBuf,
    periode: AtomicUsize,
    /// La période que le rappel de B demande, publiée dès son premier rappel.
    periode_annoncee: AtomicUsize,
    /// Origine commune des instants des deux rappels.
    origine: std::time::Instant,
    /// Instant de la dernière lecture de B, en ns depuis `origine`.
    derniere_lecture_ns: AtomicU64,
    /// Latence matérielle de B, en ns, lue dans ses horodatages.
    latence_b_ns: AtomicU64,
    /// Avance publiée par A (latence de A + décalage manuel), en bits `f64`.
    avance_a: AtomicU64,
}

impl AnneauDeB {
    fn nouveau(capacite: usize) -> Self {
        Self {
            anneau: RingBuf::new(capacite),
            periode: AtomicUsize::new(0),
            periode_annoncee: AtomicUsize::new(0),
            origine: std::time::Instant::now(),
            derniere_lecture_ns: AtomicU64::new(AUCUNE),
            latence_b_ns: AtomicU64::new(AUCUNE),
            avance_a: AtomicU64::new(0f64.to_bits()),
        }
    }

    fn maintenant_ns(&self) -> u64 {
        self.origine.elapsed().as_nanos() as u64
    }
}

impl AnneauSecondaire for AnneauDeB {
    fn pousser(&self, echantillons: &[f32]) -> usize {
        self.anneau.push(echantillons)
    }
    fn en_attente(&self) -> usize {
        self.anneau.available()
    }
    fn periode_du_lecteur(&self) -> usize {
        self.periode.load(Ordering::Relaxed)
    }
    fn depuis_la_derniere_lecture_s(&self) -> Option<f64> {
        match self.derniere_lecture_ns.load(Ordering::Acquire) {
            AUCUNE => None,
            t => Some(self.maintenant_ns().saturating_sub(t) as f64 * 1e-9),
        }
    }
    fn latence_du_lecteur_s(&self) -> Option<f64> {
        match self.latence_b_ns.load(Ordering::Relaxed) {
            AUCUNE => None,
            ns => Some(ns as f64 * 1e-9),
        }
    }
    fn publier_l_avance_de_a(&self, avance_s: f64) {
        self.avance_a.store(avance_s.to_bits(), Ordering::Relaxed);
    }
    fn periode_annoncee(&self) -> usize {
        self.periode_annoncee.load(Ordering::Relaxed)
    }
}

/// La latence matérielle qu'un rappel de sortie lit dans ses horodatages :
/// l'instant prévu de lecture moins l'instant du rappel. `None` quand le
/// pilote ne la donne pas (instants égaux ou incohérents).
pub(super) fn latence_du_rappel(info: &cpal::OutputCallbackInfo) -> Option<f64> {
    let t = info.timestamp();
    t.playback
        .duration_since(&t.callback)
        .map(|d| d.as_secs_f64())
        .filter(|s| *s > 0.0 && *s < 1.0)
}

/// Les sorties B d'un groupe, servies ensemble par le rappel de A.
pub(super) type Recopie = SortieMultiple<Arc<AnneauDeB>>;

/// Ce que le rappel de A consulte à chaque période. Inactif : A joue seul,
/// exactement comme avant.
pub(super) struct Emplacement {
    actif: AtomicBool,
    /// Un réajustement du retard commun est demandé (pause, piste suivante) ;
    /// relevé par le rappel de A, qui le transmet à la recopie.
    reajuster: AtomicBool,
    recopie: std::sync::Mutex<Option<Recopie>>,
}

pub(super) type EmplacementDeRecopie = Arc<Emplacement>;

pub(super) fn emplacement_vide() -> EmplacementDeRecopie {
    Arc::new(Emplacement {
        actif: AtomicBool::new(false),
        reajuster: AtomicBool::new(false),
        recopie: std::sync::Mutex::new(None),
    })
}

impl Emplacement {
    /// Une recopie est-elle installée ? Lu par le rappel de A pour savoir s'il
    /// doit rendre au volume unité (le volume de A s'applique APRÈS la
    /// recopie, celui de chaque B dans le rappel de ce B).
    pub(super) fn est_actif(&self) -> bool {
        self.actif.load(Ordering::Acquire)
    }

    /// Demander que le retard commun soit recalculé au prochain silence
    /// (jamais pendant le son : voir `SortieMultiple::demander_un_reajustement`).
    pub(super) fn demander_un_reajustement(&self) {
        self.reajuster.store(true, Ordering::Release);
    }

    fn installer(&self, recopie: Recopie) {
        *self.recopie.lock().unwrap_or_else(|e| e.into_inner()) = Some(recopie);
        self.actif.store(true, Ordering::Release);
    }

    fn retirer(&self) -> Option<Recopie> {
        self.actif.store(false, Ordering::Release);
        self.recopie
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

/// Dans le rappel de A, après le rendu (rampe et silence compris) :
/// recopier la période vers chaque B, et la remplacer par sa version
/// retardée du retard commun.
///
/// `try_lock` : le rappel temps réel n'attend jamais. Le verrou n'est pris
/// ailleurs qu'à l'installation et au retrait de la recopie.
pub(super) fn recopier_la_periode(
    emplacement: &EmplacementDeRecopie,
    data: &mut [f32],
    latence_a: Option<f64>,
) {
    if let Ok(mut garde) = emplacement.recopie.try_lock()
        && let Some(recopie) = garde.as_mut()
    {
        if emplacement.reajuster.swap(false, Ordering::AcqRel) {
            recopie.demander_un_reajustement();
        }
        recopie.traiter(data, latence_a);
    }
}

/// Le rappel `f32` partagé de A, avec ou sans seconde sortie.
///
/// Sans recopie : exactement le rendu d'avant. Avec : rendu au volume unité,
/// recopie vers B, puis volume de A — chaque membre du groupe a le sien.
#[allow(clippy::too_many_arguments)]
pub(super) fn rendre_la_periode_de_a(
    emplacement: &EmplacementDeRecopie,
    unite: &AtomicU32,
    ring: &RingBuf,
    volume: &AtomicU32,
    paused: &AtomicBool,
    silent: &AtomicBool,
    data_started: &AtomicBool,
    ramp: &mut crate::audio::soft_mute::SoftMuteRamp,
    armed_ms: u32,
    min_buffer_samples: usize,
    data: &mut [f32],
    info: &cpal::OutputCallbackInfo,
) {
    if !emplacement.est_actif() {
        render_local_shared_f32_callback(
            ring,
            volume,
            paused,
            silent,
            data_started,
            ramp,
            armed_ms,
            min_buffer_samples,
            data,
        );
        return;
    }
    render_local_shared_f32_callback(
        ring,
        unite,
        paused,
        silent,
        data_started,
        ramp,
        armed_ms,
        min_buffer_samples,
        data,
    );
    // #6132 — la pause est un des deux moments où le retard commun peut se
    // réajuster : la demande attend que la ligne à retard ne rende plus que
    // du silence.
    if paused.load(Ordering::Relaxed) {
        emplacement.demander_un_reajustement();
    }
    recopier_la_periode(emplacement, data, latence_du_rappel(info));
    let v = volume.load(Ordering::Relaxed) as f32 / 1000.0;
    if v != 1.0 {
        for s in data.iter_mut() {
            *s *= v;
        }
    }
}

/// Le gain de B, en millièmes : le curseur de la zone B composé avec le
/// ReplayGain et la compensation de la piste que A joue — la même règle que
/// le volume de A ([`effective_volume_units`]). Sans curseur connu pour B, B
/// suit le volume de A.
pub(crate) fn gain_de_b(volume_b: Option<u32>, facteur_de_rendu_a: u32, volume_a: u32) -> u32 {
    match volume_b {
        Some(v) => effective_volume_units(v, facteur_de_rendu_a, false),
        None => volume_a,
    }
}

/// Pourquoi la seconde sortie n'a pas été ouverte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefusDeSecondeSortie {
    /// ASIO ne tient qu'un flux par machine.
    Asio,
    /// La zone est en « bit-perfect strict » : B serait rééchantillonné.
    BitPerfectStrict,
    /// A n'a pas été ouvert en `f32` partagé (cascade entière).
    FormatDeA,
    /// La seconde sortie est le périphérique même de A.
    MemePeripherique,
    /// Aucun périphérique de ce nom sur l'hôte ouvert.
    Introuvable,
    /// B n'offre pas le nombre de canaux de A.
    Canaux { a: u16, b: u16 },
    /// cpal a refusé d'ouvrir ou de démarrer B.
    Ouverture(String),
}

impl RefusDeSecondeSortie {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Asio => "asio_un_seul_flux",
            Self::BitPerfectStrict => "bitperfect_strict",
            Self::FormatDeA => "format_de_a",
            Self::MemePeripherique => "meme_peripherique",
            Self::Introuvable => "introuvable",
            Self::Canaux { .. } => "canaux",
            Self::Ouverture(_) => "ouverture",
        }
    }
}

/// La décision qui ne demande pas cpal : peut-on même essayer ?
pub(crate) fn refus_prealable(
    hote: &str,
    strict_bitperfect: bool,
    a_en_f32: bool,
    nom_a: &str,
    nom_b: &str,
) -> Option<RefusDeSecondeSortie> {
    if hote.eq_ignore_ascii_case("asio") {
        return Some(RefusDeSecondeSortie::Asio);
    }
    if strict_bitperfect {
        return Some(RefusDeSecondeSortie::BitPerfectStrict);
    }
    if !a_en_f32 {
        return Some(RefusDeSecondeSortie::FormatDeA);
    }
    if nom_a.trim().eq_ignore_ascii_case(nom_b.trim()) {
        return Some(RefusDeSecondeSortie::MemePeripherique);
    }
    None
}

/// Les sorties B ouvertes et en marche. Les garder vivantes garde les B qui
/// jouent ; les lâcher retire la recopie de l'emplacement de A, puis ferme
/// les B.
pub(super) struct SecondesSorties {
    emplacement: EmplacementDeRecopie,
    noms: Vec<String>,
    // Dernier champ : fermés APRÈS le retrait de la recopie (ordre de `Drop`).
    _flux: Vec<cpal::Stream>,
}

impl Drop for SecondesSorties {
    fn drop(&mut self) {
        if let Some(recopie) = self.emplacement.retirer() {
            let retard_commun_ms = recopie.retard_commun_ms();
            let reajustements = recopie.reajustements();
            for ((recopieur, _), nom) in recopie.membres().iter().zip(&self.noms) {
                let releve = recopieur.releve();
                info!(
                    seconde_sortie = %nom,
                    retard_commun_ms,
                    reajustements,
                    ecart_ms = releve.ecart_ms,
                    derive_ppm = releve.derive_ppm,
                    correction_ppm = releve.correction_ppm,
                    latence_a_ms = ?releve.latence_a_ms,
                    latence_b_ms = ?releve.latence_b_ms,
                    compensation_ms = releve.compensation_ms,
                    echantillons_refuses = releve.echantillons_refuses,
                    trames_produites = releve.trames_produites,
                    "sortie_double_releve"
                );
            }
        }
    }
}

/// Une sortie B ouverte et démarrée, pas encore servie : son flux, son
/// recopieur, son anneau.
pub(super) struct SortieBOuverte {
    flux: cpal::Stream,
    recopieur: Recopieur,
    anneau: Arc<AnneauDeB>,
    nom: String,
}

/// Installer la recopie de A vers toutes les sorties B ouvertes. Avant le
/// démarrage de A : le retard commun se règle pendant son silence de départ.
pub(super) fn installer_les_secondes_sorties(
    emplacement: &EmplacementDeRecopie,
    canaux: u16,
    cadence_a: u32,
    ouvertes: Vec<SortieBOuverte>,
) -> Option<SecondesSorties> {
    if ouvertes.is_empty() {
        return None;
    }
    let mut membres = Vec::new();
    let mut flux = Vec::new();
    let mut noms = Vec::new();
    for b in ouvertes {
        membres.push((b.recopieur, b.anneau));
        flux.push(b.flux);
        noms.push(b.nom);
    }
    emplacement.installer(SortieMultiple::nouvelle(
        usize::from(canaux),
        cadence_a,
        CONSIGNE_PAR_DEFAUT_S,
        membres,
    ));
    Some(SecondesSorties {
        emplacement: emplacement.clone(),
        noms,
        _flux: flux,
    })
}

/// Le périphérique `nom` sur `host`, SANS repli et sans rien noter pour
/// l'écran : l'observation du périphérique ouvert est celle de A.
fn trouver_sans_repli(host: &cpal::Host, nom: &str) -> Option<cpal::Device> {
    let open_host: &'static str = host.id().name();
    let (devices, identities): (Vec<cpal::Device>, Vec<DeviceIdentity>) = host
        .output_devices()
        .map(|devs| devs.collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|device| {
            let identity = DeviceIdentity {
                endpoint_id: device.id().map(|id| id.to_string()).unwrap_or_default(),
                raw_name: device
                    .description()
                    .map(|desc| desc.name().to_string())
                    .unwrap_or_else(|_| "Unknown".into()),
                host: open_host.to_string(),
            };
            (device, identity)
        })
        .filter(|(_, identity)| !is_null_sink(&identity.endpoint_id, &identity.raw_name))
        .unzip();
    match resolve_device(nom, None, None, open_host, &identities) {
        DeviceResolution::Matched(m) => devices.into_iter().nth(m.index()),
        _ => None,
    }
}

/// Ce que le rappel de B lit de A : son volume, son facteur de rendu, son
/// drapeau DoP.
#[derive(Clone)]
pub(super) struct ReglagesDeA {
    pub(super) volume: Arc<AtomicU32>,
    pub(super) facteur_de_rendu: Arc<AtomicU32>,
    pub(super) dop: Arc<AtomicBool>,
}

/// Ouvrir B à la cadence de A si B l'accepte, sinon à sa cadence par défaut
/// (le recopieur convertit), et le démarrer. B joue du silence jusqu'à ce
/// que la recopie, installée ensuite pour toutes les B à la fois, le nourrisse.
pub(super) fn ouvrir_la_seconde_sortie(
    host: &cpal::Host,
    demande: &SecondeSortieDemandee,
    cadence_a: u32,
    canaux: u16,
    reglages_a: ReglagesDeA,
) -> Result<SortieBOuverte, RefusDeSecondeSortie> {
    let nom = demande.nom.as_str();
    let device = trouver_sans_repli(host, nom).ok_or(RefusDeSecondeSortie::Introuvable)?;
    let config = match find_matching_config(&device, canaux, cadence_a) {
        Some(c) if c.channels == canaux => avec_periode(c),
        _ => {
            let defaut = device
                .default_output_config()
                .map_err(|e| RefusDeSecondeSortie::Ouverture(e.to_string()))?;
            if defaut.channels() != canaux {
                return Err(RefusDeSecondeSortie::Canaux {
                    a: canaux,
                    b: defaut.channels(),
                });
            }
            config_de_flux(canaux, defaut.sample_rate())
        }
    };
    let cadence_b = config.sample_rate;
    let mut recopieur = Recopieur::nouveau(
        usize::from(canaux),
        cadence_a,
        cadence_b,
        CONSIGNE_PAR_DEFAUT_S,
    );
    recopieur.regler_le_decalage_manuel(demande.decalage_manuel_s);
    let consigne = recopieur.consigne();
    // De quoi tenir la consigne, le plus grand retard commun et deux périodes.
    let capacite = (((CONSIGNE_PAR_DEFAUT_S + RETARD_MAX_S) * 2.0 * f64::from(cadence_b)) as usize)
        .max(16_384)
        * usize::from(canaux);
    let anneau = Arc::new(AnneauDeB::nouveau(capacite));
    let anneau_cb = anneau.clone();
    let volume_b = demande.volume_utilisateur.clone();
    let ReglagesDeA {
        volume: volume_a,
        facteur_de_rendu,
        dop,
    } = reglages_a;
    let mut demarre = false;
    let mut gain_precedent: Option<f32> = None;
    let nom_erreur = nom.to_string();
    let flux = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                let latence_b = latence_du_rappel(info);
                anneau_cb
                    .periode_annoncee
                    .store(data.len(), Ordering::Relaxed);
                if let Some(l) = latence_b {
                    anneau_cb
                        .latence_b_ns
                        .store((l * 1e9) as u64, Ordering::Relaxed);
                }
                if !demarre {
                    let avance_a = f64::from_bits(anneau_cb.avance_a.load(Ordering::Relaxed));
                    let compensation = avance_a - latence_b.unwrap_or(0.0);
                    if anneau_cb.anneau.available()
                        < consigne.seuil_de_depart(data.len(), compensation)
                    {
                        data.fill(0.0);
                        return;
                    }
                    demarre = true;
                }
                let lu = anneau_cb.anneau.pop(data);
                anneau_cb.periode.store(data.len(), Ordering::Relaxed);
                anneau_cb
                    .derniere_lecture_ns
                    .store(anneau_cb.maintenant_ns(), Ordering::Release);
                data[lu..].fill(0.0);
                // Un flux DoP rééchantillonné n'est plus du DSD : silence.
                if dop.load(Ordering::Relaxed) {
                    data.fill(0.0);
                    return;
                }
                let gain = gain_de_b(
                    volume_b.as_ref().map(|v| v.load(Ordering::Relaxed)),
                    facteur_de_rendu.load(Ordering::Relaxed),
                    volume_a.load(Ordering::Relaxed),
                ) as f32
                    / 1000.0;
                // Rampe sur la période : un curseur bougé ne claque pas.
                let depart = gain_precedent.unwrap_or(gain);
                gain_precedent = Some(gain);
                let n = data.len().max(1) as f32;
                for (i, s) in data.iter_mut().enumerate() {
                    *s *= depart + (gain - depart) * (i as f32 / n);
                }
            },
            move |e: cpal::StreamError| {
                warn!(seconde_sortie = %nom_erreur, erreur = %e, "sortie_double_erreur_de_flux");
            },
            None,
        )
        .map_err(|e| RefusDeSecondeSortie::Ouverture(e.to_string()))?;
    flux.play()
        .map_err(|e| RefusDeSecondeSortie::Ouverture(e.to_string()))?;
    info!(
        seconde_sortie = %nom,
        cadence_a,
        cadence_b,
        canaux,
        consigne_ms = CONSIGNE_PAR_DEFAUT_S * 1000.0,
        decalage_manuel_ms = demande.decalage_manuel_s * 1000.0,
        volume_propre = demande.volume_utilisateur.is_some(),
        "sortie_double_ouverte"
    );
    Ok(SortieBOuverte {
        flux,
        recopieur,
        anneau,
        nom: nom.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asio_strict_et_format_entier_refusent_la_seconde_sortie() {
        assert_eq!(
            refus_prealable("Asio", false, true, "A", "B"),
            Some(RefusDeSecondeSortie::Asio)
        );
        assert_eq!(
            refus_prealable("Wasapi", true, true, "A", "B"),
            Some(RefusDeSecondeSortie::BitPerfectStrict)
        );
        assert_eq!(
            refus_prealable("Wasapi", false, false, "A", "B"),
            Some(RefusDeSecondeSortie::FormatDeA)
        );
        assert_eq!(
            refus_prealable("Alsa", false, true, "DAC", " dac "),
            Some(RefusDeSecondeSortie::MemePeripherique)
        );
        assert_eq!(
            refus_prealable("Wasapi", false, true, "DAC", "Interface"),
            None
        );
    }

    /// Le chemin réel : le rappel de A tel que la sortie locale le monte,
    /// appelé à la main sur un `RingBuf` réel.
    #[test]
    fn le_rappel_de_a_recopie_vers_l_anneau_de_b() {
        let emplacement = emplacement_vide();
        let mut periode = vec![0.25f32; 960];
        // Vide : A joue seul, rien ne change.
        assert!(!emplacement.est_actif());
        recopier_la_periode(&emplacement, &mut periode, None);
        assert!(periode.iter().all(|s| *s == 0.25));

        let anneau = Arc::new(AnneauDeB::nouveau(48_000));
        let autre = Arc::new(AnneauDeB::nouveau(48_000));
        emplacement.installer(SortieMultiple::nouvelle(
            2,
            48_000,
            0.010,
            vec![
                (Recopieur::nouveau(2, 48_000, 48_000, 0.010), anneau.clone()),
                (Recopieur::nouveau(2, 48_000, 44_100, 0.010), autre.clone()),
            ],
        ));
        assert!(emplacement.est_actif());
        recopier_la_periode(&emplacement, &mut periode, Some(0.005));
        // A est retardé de 10 ms (480 trames) : sa première période est muette.
        assert!(periode.iter().all(|s| *s == 0.0));
        // Chaque B a reçu la période, au filtre près, et l'avance de A est publiée.
        assert!(anneau.en_attente() >= 2 * (480 - 32));
        // La seconde suiveuse aussi, à sa cadence (44,1 kHz).
        assert!(autre.en_attente() >= 2 * (441 - 32));
        assert_eq!(
            f64::from_bits(anneau.avance_a.load(Ordering::Relaxed)),
            0.005
        );
        assert!(emplacement.retirer().is_some());
        assert!(!emplacement.est_actif());
    }

    /// #6132 étape 2 — volume par membre : B suit SON curseur, composé avec le
    /// ReplayGain de la piste ; la zone B coupée (curseur à 0) se tait même
    /// quand A joue fort ; sans curseur connu, B suit A.
    #[test]
    fn le_gain_de_b_suit_le_curseur_de_sa_zone() {
        // ReplayGain de la piste : 0,5.
        assert_eq!(gain_de_b(Some(800), 500, 1000), 400);
        assert_eq!(gain_de_b(Some(0), 1000, 1000), 0);
        assert_eq!(gain_de_b(Some(1000), 1000, 200), 1000);
        assert_eq!(gain_de_b(None, 1000, 300), 300);
    }

    /// #6132 — la demande de réajustement (pause, piste suivante) passe de
    /// l'emplacement à la recopie au rappel suivant de A, et y attend son
    /// silence.
    #[test]
    fn la_demande_de_reajustement_atteint_la_recopie() {
        let emplacement = emplacement_vide();
        emplacement.installer(SortieMultiple::nouvelle(
            1,
            48_000,
            0.010,
            vec![(
                Recopieur::nouveau(1, 48_000, 48_000, 0.010),
                Arc::new(AnneauDeB::nouveau(48_000)),
            )],
        ));
        let mut son = vec![0.25f32; 480];
        // Premier rappel : le retard commun se règle puis se fige.
        recopier_la_periode(&emplacement, &mut son, None);
        emplacement.demander_un_reajustement();
        let mut son = vec![0.25f32; 480];
        recopier_la_periode(&emplacement, &mut son, None);
        assert!(!emplacement.reajuster.load(Ordering::Acquire));
        let garde = emplacement.recopie.lock().unwrap();
        let recopie = garde.as_ref().unwrap();
        assert!(
            recopie.reajustement_en_attente(),
            "pendant le son, la demande doit attendre"
        );
        assert_eq!(recopie.reajustements(), 0);
    }
}
