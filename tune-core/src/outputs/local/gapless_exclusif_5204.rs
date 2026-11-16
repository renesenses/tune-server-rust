//! #5204 — sortie locale en mode exclusif : l'enchaînement sans blanc.
//!
//! Jean Valjean, fil 1890, 0.9.165, WASAPI exclusif + PURE : « une latence
//! inférieure à 1 s lors du changement de piste ». Le journal montrait le
//! périphérique ARRÊTÉ puis RÉINITIALISÉ au même format (88,2 kHz / 32 bits)
//! entre deux pistes, parce que `supports_internal_gapless()` rendait
//! `!exclusive_mode` et que le bras WASAPI sortait à l'EOF sans consommer la
//! suivante.
//!
//! Ce qui est éprouvé ici, sans carte son, sur Linux :
//!
//! 1. la capacité publiée au sondeur suit le bras réellement emprunté ;
//! 2. la règle de la frontière : même format ⇒ enchaîner, sinon rouvrir ;
//! 3. la chaîne native elle-même (`chaine_native.rs`, la boucle du bras
//!    WASAPI) : deux pistes de même format passent dans le MÊME puits, sans
//!    le refermer ; une piste d'un autre format n'y entre pas ;
//! 4. la lecture de l'en-tête de la piste suivante, partagée avec le chemin
//!    cpal.

use std::collections::VecDeque;
use std::io::Cursor;

use super::chaine_native::{
    FinDeChaine, HoteDeChaineNative, ReserveDeLaChaine, Suivante, jouer_la_chaine_native,
};
use super::enchainement_exclusif::{
    BrasDeLecture, EnchainementNatif, RefusDEnchainement, bras_de_lecture,
    decider_l_enchainement_natif, lire_l_entete_enchainee,
};
use super::etage_natif::{EcritureNative, EtageNatif, spec_du_puits_natif};
use super::*;
use crate::outputs::traits::{CaptureOutputNatif, ProfondeurPcm};

// ─── 1. La capacité publiée ─────────────────────────────────────────────────

/// LE témoin de l'issue, au niveau de la sortie : une sortie réglée
/// « exclusif » avec le backend WASAPI doit annoncer l'enchaînement interne.
///
/// Sur `main`, `supports_internal_gapless()` rendait `!exclusive_mode` : ce
/// test y rougit. Sous Windows, c'est le bras WASAPI (qui enchaîne depuis
/// #5204) ; sous Linux, l'exclusif n'existe pas et `play_url` prend le bras
/// cpal partagé, qui a toujours enchaîné — la capacité mentait donc aussi là.
/// macOS est exclu parce que « wasapi » n'y existe pas : son bras exclusif est
/// CoreAudio « hog », qui enchaîne depuis #5451 (`gapless_coreaudio_5451.rs`).
#[cfg(not(target_os = "macos"))]
#[test]
fn une_sortie_exclusive_wasapi_annonce_l_enchainement_interne_5204() {
    let sortie = LocalOutput::with_options("Haut-parleurs".into(), true, "wasapi");
    assert!(
        sortie.supports_internal_gapless(),
        "#5204 : en WASAPI exclusif, la sortie doit annoncer l'enchaînement \
         interne — sinon le sondeur n'arme pas le gapless et chaque piste \
         ferme puis rouvre le périphérique au même format"
    );
    assert!(
        sortie.capabilities().can_gapless,
        "une seule vérité gapless : la capacité publiée suit la sonde"
    );
    // La sonde reste vivante : une chaîne épuisée ne promet plus rien.
    sortie.set_chain_exhausted_for_test(true);
    assert!(!sortie.supports_internal_gapless());
}

/// La règle pure du bras, pour les quatre plateformes/backends.
#[test]
fn le_bras_emprunte_decide_de_la_capacite_5204() {
    use BrasDeLecture::*;
    let cas = [
        // (os, feature asio, exclusif, backend) → bras attendu
        (("windows", true, true, "wasapi"), WasapiExclusif),
        (("windows", false, true, "auto"), WasapiExclusif),
        (("windows", true, true, "asio"), AsioExclusif),
        (("windows", false, true, "asio"), CpalPartage),
        (("windows", true, false, "asio"), CpalPartage),
        (("macos", false, true, "auto"), CoreAudioExclusif),
        (("macos", false, false, "auto"), CpalPartage),
        (("linux", false, true, "auto"), CpalPartage),
    ];
    for ((os, asio, exclusif, backend), attendu) in cas {
        assert_eq!(
            bras_de_lecture(os, asio, exclusif, backend),
            attendu,
            "{os} asio={asio} exclusif={exclusif} backend={backend}"
        );
    }
    assert!(
        WasapiExclusif.sait_enchainer(),
        "#5204 : WASAPI exclusif enchaîne à format égal"
    );
    assert!(CpalPartage.sait_enchainer());
    assert!(
        AsioExclusif.sait_enchainer(),
        "#5204 (seconde tranche) : ASIO exclusif enchaîne à format égal sur \
         sa route native — voir `gapless_asio_5204.rs`"
    );
    assert!(
        CoreAudioExclusif.sait_enchainer(),
        "#5451 : CoreAudio exclusif enchaîne à format égal — voir \
         `gapless_coreaudio_5451.rs`"
    );
}

// ─── 2. La règle de la frontière ────────────────────────────────────────────

fn spec(cadence: u32, profondeur: ProfondeurPcm, canaux: u16) -> AudioSpec {
    AudioSpec::nouvelle(cadence, profondeur, canaux).unwrap()
}

/// Même format ⇒ enchaîner ; cadence, profondeur ou canaux différents ⇒
/// rouvrir (le périphérique exclusif ne convertit rien).
#[test]
fn a_format_egal_on_enchaine_sinon_on_rouvre_5204() {
    let source = spec(88_200, ProfondeurPcm::Entier32, 2);
    let ouvert = FormatOuvert::new(88_200, 2);
    assert_eq!(
        decider_l_enchainement_natif(source, ouvert, source),
        EnchainementNatif::Enchainer,
        "le cas du relevé : 88,2 kHz / 32 bits → 88,2 kHz / 32 bits"
    );
    for (suivante, quoi) in [
        (spec(44_100, ProfondeurPcm::Entier32, 2), "cadence"),
        (spec(88_200, ProfondeurPcm::Entier24, 2), "profondeur"),
        (spec(88_200, ProfondeurPcm::Entier32, 1), "canaux"),
    ] {
        assert_eq!(
            decider_l_enchainement_natif(source, ouvert, suivante),
            EnchainementNatif::Rouvrir,
            "{quoi} différent(e) : il faut rouvrir le périphérique"
        );
    }
}

// ─── 3. La chaîne native ────────────────────────────────────────────────────

struct Dsp {
    eq: std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: AtomicBool,
    mono_downmix: AtomicBool,
    volume: AtomicU32,
}

impl Dsp {
    fn au_repos() -> Self {
        Self {
            eq: std::sync::Mutex::new(None),
            convolver: std::sync::Mutex::new(None),
            crossfeed: std::sync::Mutex::new(None),
            pure_bypass: AtomicBool::new(false),
            mono_downmix: AtomicBool::new(false),
            volume: AtomicU32::new(UNITE_DU_GAIN_DE_RENDU),
        }
    }

    fn etage(&self, spec: AudioSpec) -> EtageNatif<'_> {
        EtageNatif::monter(
            spec,
            FormatOuvert::new(spec.cadence(), spec.canaux()),
            &self.volume,
            &self.eq,
            &self.convolver,
            &self.crossfeed,
            &self.pure_bypass,
            &self.mono_downmix,
        )
    }
}

/// Un WAV PCM entier canonique, en-tête de 44 octets puis `pcm`.
fn wav(cadence: u32, bits: u16, canaux: u16, pcm: &[u8]) -> Vec<u8> {
    let block_align = canaux * bits / 8;
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&canaux.to_le_bytes());
    w.extend_from_slice(&cadence.to_le_bytes());
    w.extend_from_slice(&(cadence * u32::from(block_align)).to_le_bytes());
    w.extend_from_slice(&block_align.to_le_bytes());
    w.extend_from_slice(&bits.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

fn pcm(octets: usize, graine: usize) -> Vec<u8> {
    (0..octets)
        .map(|i| (i * 31 + graine * 7 + 3) as u8)
        .collect()
}

/// L'hôte factice : une réserve de WAV à enchaîner, et le compte de ce que
/// la chaîne a publié.
#[derive(Default)]
struct HoteFactice {
    reserve: VecDeque<Vec<u8>>,
    enchainements: u32,
}

impl ReserveDeLaChaine for HoteFactice {
    type Lecteur = Cursor<Vec<u8>>;

    fn silence_force(&self) -> bool {
        false
    }

    fn preparer_la_suivante(&mut self) -> Suivante<Self::Lecteur> {
        let Some(octets) = self.reserve.pop_front() else {
            return Suivante::Aucune;
        };
        let mut lecteur = Cursor::new(octets);
        match lire_l_entete_enchainee(&mut lecteur, &AtomicBool::new(false)) {
            Ok(entete) => Suivante::Prete { lecteur, entete },
            Err(_) => Suivante::Refusee,
        }
    }

    fn piste_enchainee(&mut self) {
        self.enchainements += 1;
    }
}

impl HoteDeChaineNative for HoteFactice {
    fn arret_recu(&mut self) -> bool {
        false
    }

    fn publier_le_verdict(&mut self, _dop: bool, _bit_perfect: bool) {}

    fn publier_la_position(&mut self, _position_ms: u64) {}
}

/// Ce que `play_url` fait avant le bras : lire l'en-tête de la première
/// piste, pousser l'amorce. Rend le lecteur positionné et les trames.
fn demarrer(
    premier: Vec<u8>,
    etage: &mut EtageNatif<'_>,
    puits: &mut CaptureOutputNatif,
) -> (Cursor<Vec<u8>>, u64) {
    let mut lecteur = Cursor::new(premier);
    let entete = lire_l_entete_enchainee(&mut lecteur, &AtomicBool::new(false)).unwrap();
    let trames = match etage.decoder_et_pousser(entete.amorce(), puits) {
        EcritureNative::Poussee { trames_source, .. }
        | EcritureNative::PuitsMort { trames_source, .. } => trames_source,
        EcritureNative::RienAPousser => 0,
    };
    (lecteur, trames)
}

/// LE témoin de l'enchaînement : deux pistes de même format passent dans le
/// MÊME puits — celui du flux ouvert — sans qu'il soit refermé, et le puits
/// reçoit exactement les mots des deux pistes, bout à bout.
///
/// Avant #5204, le bras rendait la main à l'EOF de la première piste : la
/// seconde n'atteignait jamais ce puits (le sondeur la relançait par
/// `play_url`, qui rouvrait le périphérique).
#[test]
fn deux_pistes_de_meme_format_passent_dans_le_meme_flux_5204() {
    let dsp = Dsp::au_repos();
    let format = spec(48_000, ProfondeurPcm::Entier16, 2);
    let pcm_a = pcm(4 * 3_000, 1);
    let pcm_b = pcm(4 * 2_000, 2);

    let mut puits = CaptureOutputNatif::ouvrir(spec_du_puits_natif(format));
    let mut etage = dsp.etage(format);
    let (lecteur, trames) = demarrer(wav(48_000, 16, 2, &pcm_a), &mut etage, &mut puits);
    let mut hote = HoteFactice::default();
    hote.reserve.push_back(wav(48_000, 16, 2, &pcm_b));

    let issue = jouer_la_chaine_native(&mut hote, lecteur, &mut etage, &mut puits, trames, 0);

    assert_eq!(
        issue.pistes_enchainees, 1,
        "#5204 : la piste suivante de même format doit être enchaînée dans \
         le flux ouvert, pas laissée au sondeur (fermeture + réouverture)"
    );
    assert_eq!(
        hote.enchainements, 1,
        "le morceau suivant est publié une fois"
    );
    assert_eq!(issue.fin, FinDeChaine::RienEnReserve);
    assert!(issue.http_eof, "la dernière piste a atteint sa fin de flux");
    assert_eq!(issue.trames, 2_000, "la position est celle de la piste B");
    assert_eq!(
        puits.trames(),
        5_000,
        "le puits du flux ouvert a reçu A puis B"
    );
    assert_eq!(puits.blocs_refuses(), 0, "le puits n'a jamais été tué");

    // Bout à bout, à l'octet près : la même chose qu'une seule piste A+B.
    let mut temoin = CaptureOutputNatif::ouvrir(spec_du_puits_natif(format));
    let mut etage_temoin = dsp.etage(format);
    let a_puis_b: Vec<u8> = pcm_a.iter().chain(pcm_b.iter()).copied().collect();
    etage_temoin.decoder_et_pousser(&a_puis_b, &mut temoin);
    assert_eq!(puits.empreinte(), temoin.empreinte());
}

/// La contrepartie : à format différent, la suivante n'entre PAS dans le
/// flux ouvert. La chaîne rend la main en le disant — la fin naturelle
/// rouvrira le périphérique au nouveau format, comme en 0.9.165.
#[test]
fn une_piste_d_un_autre_format_n_entre_pas_dans_le_flux_5204() {
    let dsp = Dsp::au_repos();
    let format = spec(48_000, ProfondeurPcm::Entier16, 2);
    let pcm_a = pcm(4 * 1_000, 1);

    let mut puits = CaptureOutputNatif::ouvrir(spec_du_puits_natif(format));
    let mut etage = dsp.etage(format);
    let (lecteur, trames) = demarrer(wav(48_000, 16, 2, &pcm_a), &mut etage, &mut puits);
    let mut hote = HoteFactice::default();
    hote.reserve
        .push_back(wav(44_100, 16, 2, &pcm(4 * 1_000, 2)));

    let issue = jouer_la_chaine_native(&mut hote, lecteur, &mut etage, &mut puits, trames, 0);

    assert_eq!(issue.fin, FinDeChaine::FormatDifferent);
    assert!(issue.http_eof, "la piste A s'est terminée normalement");
    assert_eq!(issue.pistes_enchainees, 0);
    assert_eq!(hote.enchainements, 0, "le morceau courant reste publié");
    assert_eq!(issue.trames, 1_000);
    assert_eq!(
        puits.trames(),
        1_000,
        "rien de la piste B dans le flux ouvert"
    );
    assert_eq!(etage.spec(), format, "l'étage n'a pas été touché");
}

/// Rien en réserve, ou une suivante illisible : la piste se termine comme
/// avant #5204, et la chaîne le dit.
#[test]
fn sans_suivante_lisible_la_piste_se_termine_comme_avant_5204() {
    let dsp = Dsp::au_repos();
    let format = spec(48_000, ProfondeurPcm::Entier16, 2);
    for (reserve, fin) in [
        (None, FinDeChaine::RienEnReserve),
        (
            Some(b"ID3 pas un wav".to_vec()),
            FinDeChaine::SuivanteRefusee,
        ),
    ] {
        let mut puits = CaptureOutputNatif::ouvrir(spec_du_puits_natif(format));
        let mut etage = dsp.etage(format);
        let (lecteur, trames) =
            demarrer(wav(48_000, 16, 2, &pcm(4 * 500, 1)), &mut etage, &mut puits);
        let mut hote = HoteFactice::default();
        hote.reserve.extend(reserve);
        let issue = jouer_la_chaine_native(&mut hote, lecteur, &mut etage, &mut puits, trames, 0);
        assert_eq!(issue.fin, fin);
        assert!(issue.http_eof);
        assert_eq!(issue.pistes_enchainees, 0);
        assert_eq!(puits.trames(), 500);
    }
}

/// Trois pistes au même format : la chaîne ne s'arrête pas à la deuxième.
#[test]
fn la_chaine_continue_tant_que_le_format_ne_change_pas_5204() {
    let dsp = Dsp::au_repos();
    let format = spec(88_200, ProfondeurPcm::Entier32, 2);
    let mut puits = CaptureOutputNatif::ouvrir(spec_du_puits_natif(format));
    let mut etage = dsp.etage(format);
    let (lecteur, trames) = demarrer(wav(88_200, 32, 2, &pcm(8 * 700, 1)), &mut etage, &mut puits);
    let mut hote = HoteFactice::default();
    hote.reserve.push_back(wav(88_200, 32, 2, &pcm(8 * 600, 2)));
    hote.reserve.push_back(wav(88_200, 32, 2, &pcm(8 * 500, 3)));
    let issue = jouer_la_chaine_native(&mut hote, lecteur, &mut etage, &mut puits, trames, 0);
    assert_eq!(issue.pistes_enchainees, 2);
    assert_eq!(puits.trames(), 1_800);
    assert_eq!(issue.trames, 500);
}

/// La nouvelle piste reprend sa décision PCM/DoP à zéro : la quarantaine
/// 24 bits est réarmée, et le reste non aligné de la précédente est jeté.
#[test]
fn l_enchainement_rearme_la_quarantaine_24_bits_5204() {
    let dsp = Dsp::au_repos();
    let format = spec(96_000, ProfondeurPcm::Entier24, 2);
    let mut puits = CaptureOutputNatif::ouvrir(spec_du_puits_natif(format));
    let mut etage = dsp.etage(format);
    // 40 trames + 2 octets : la quarantaine se ferme, un reste traîne.
    etage.decoder_et_pousser(&pcm(6 * 40 + 2, 1), &mut puits);
    assert!(!etage.quarantaine_24_bits_ouverte());
    assert_eq!(etage.en_attente().len(), 2);

    assert!(etage.enchainer_la_piste(format, &[]).is_ok());
    assert!(
        etage.quarantaine_24_bits_ouverte(),
        "la nouvelle piste doit être classée PCM/DoP pour elle-même"
    );
    assert!(etage.en_attente().is_empty());
    assert!(!etage.dop_verrouille());

    assert!(
        etage
            .enchainer_la_piste(spec(48_000, ProfondeurPcm::Entier24, 2), &[])
            .is_err()
    );
}

// ─── 4. L'en-tête de la piste suivante ──────────────────────────────────────

#[test]
fn l_en_tete_suivant_se_lit_et_se_type_5204() {
    let pcm_b = pcm(4 * 10, 1);
    let mut lecteur = Cursor::new(wav(44_100, 16, 2, &pcm_b));
    let entete = lire_l_entete_enchainee(&mut lecteur, &AtomicBool::new(false)).unwrap();
    assert_eq!(entete.spec, spec(44_100, ProfondeurPcm::Entier16, 2));
    assert_eq!(
        entete.amorce(),
        &pcm_b[..],
        "le PCM lu avec l'en-tête est rendu"
    );

    let mut vide = Cursor::new(Vec::new());
    assert_eq!(
        lire_l_entete_enchainee(&mut vide, &AtomicBool::new(false)).unwrap_err(),
        RefusDEnchainement::EnteteVide
    );
    let mut flac = Cursor::new(b"fLaC\0\0\0\"".repeat(10));
    assert_eq!(
        lire_l_entete_enchainee(&mut flac, &AtomicBool::new(false)).unwrap_err(),
        RefusDEnchainement::PasDuWav
    );
    let mut arrete = Cursor::new(wav(44_100, 16, 2, &pcm_b));
    assert_eq!(
        lire_l_entete_enchainee(&mut arrete, &AtomicBool::new(true)).unwrap_err(),
        RefusDEnchainement::Interrompu
    );
}

/// #8788 — les tags placés APRÈS data ne sont pas des échantillons. Le
/// lecteur partagé et les bras exclusifs prennent cette même amorce.
#[test]
fn le_wav_enchaine_ne_joue_pas_le_chunk_list_apres_data_8788() {
    let audio = pcm(6 * 16, 7);
    let mut fichier = wav(705_600, 24, 2, &audio);
    fichier.extend_from_slice(b"LIST");
    fichier.extend_from_slice(&18u32.to_le_bytes());
    fichier.extend_from_slice(b"INFOISFT");
    fichier.extend_from_slice(&6u32.to_le_bytes());
    fichier.extend_from_slice(b"Codex\0");
    let taille_riff = fichier.len() as u32 - 8;
    fichier[4..8].copy_from_slice(&taille_riff.to_le_bytes());
    let mut lecteur = Cursor::new(fichier);
    let entete = lire_l_entete_enchainee(&mut lecteur, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        entete.amorce(),
        audio,
        "#8788 : les métadonnées LIST après data sont livrées comme du PCM"
    );
}

#[test]
fn un_wav_data_vide_ne_joue_pas_ses_metadonnees_8788() {
    let mut fichier = wav(44_100, 16, 2, &[]);
    fichier.extend_from_slice(b"LIST");
    fichier.extend_from_slice(&4u32.to_le_bytes());
    fichier.extend_from_slice(b"INFO");
    let taille_riff = fichier.len() as u32 - 8;
    fichier[4..8].copy_from_slice(&taille_riff.to_le_bytes());
    let entete =
        lire_l_entete_enchainee(&mut Cursor::new(fichier), &AtomicBool::new(false)).unwrap();
    assert!(
        entete.amorce().is_empty(),
        "#8788 : un data vide devient du bruit de métadonnées"
    );
}

#[test]
fn le_wav_http_s_arrete_a_data_apres_plusieurs_lectures_8788() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    for enchaine in [false, true] {
        for taille_inconnue in [None, Some(u32::MAX), Some(0x7FFF_FFFF - 36)] {
            let audio = pcm(12_000, 11);
            let mut fichier = wav(705_600, 24, 2, &audio);
            if let Some(taille) = taille_inconnue {
                fichier[40..44].copy_from_slice(&taille.to_le_bytes());
            } else {
                fichier.extend_from_slice(b"LIST");
                fichier.extend_from_slice(&4u32.to_le_bytes());
                fichier.extend_from_slice(b"INFO");
                let taille_riff = fichier.len() as u32 - 8;
                fichier[4..8].copy_from_slice(&taille_riff.to_le_bytes());
            }
            let serveur = TcpListener::bind("127.0.0.1:0").unwrap();
            let adresse = serveur.local_addr().unwrap();
            let fil = std::thread::spawn(move || {
                let (mut socket, _) = serveur.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut demande = [0; 4096];
                socket.read(&mut demande).unwrap();
                let mut reponse = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n", fichier.len()).into_bytes();
                reponse.extend(fichier);
                socket.write_all(&reponse).unwrap();
            });
            let arret = Arc::new(AtomicBool::new(false));
            let mut lecteur = super::lecture_http::LecteurHttpAnnulable::ouvrir(
                &format!("http://{adresse}/piste.wav"),
                arret.clone(),
            )
            .unwrap();
            let mut rendu = if enchaine {
                let entete = lire_l_entete_enchainee(&mut lecteur, &arret).unwrap();
                lecteur.borner(entete.restant);
                entete.amorce().to_vec()
            } else {
                let mut entete = vec![0; 4096];
                let n = lecteur.read(&mut entete).unwrap();
                entete.truncate(n);
                let (_, _, _, debut) = parse_wav_header(&entete).unwrap();
                lecteur.borner(super::borner_donnees_wav(&mut entete, debut));
                entete[debut..].to_vec()
            };
            lecteur.read_to_end(&mut rendu).unwrap();
            fil.join().unwrap();
            assert_eq!(
                rendu, audio,
                "#8788 : le lecteur HTTP livre des tags ou tronque le WAV en continu (enchaîné={enchaine}, taille={taille_inconnue:?})"
            );
        }
    }
}
