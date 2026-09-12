//! T8 de #2218 — **le seul témoin du dépôt qui relie le décodeur au DAC.**
//!
//! Une fixture du banc de conformité est décodée, ses octets sont poussés dans
//! la chaîne de sortie exactement comme `play_url` les pousse — décoder,
//! refuser un porteur DoP, appliquer le DSP, adapter les canaux,
//! rééchantillonner — et ce qui ressort est comparé à **l'empreinte du
//! décodeur de référence du format**, `flac -d` (libFLAC 1.5.0).
//!
//! # Ce que cela ajoute à ce qui existait
//!
//! Le dépôt avait deux moitiés, et rien entre elles :
//!
//! * `tests/flac_empreintes_reference.rs` (T1) garde le **décodeur** :
//!   `decode_to_pcm` rend bien ce que `flac -d` rend. Il s'arrête là ;
//! * `outputs/local/empreinte_du_puits_r1.rs` (R1) garde la **conversion**,
//!   contre des relevés pris sur la chaîne d'AVANT la réorganisation
//!   (`5318d073`). Il part d'une rampe synthétique, jamais d'un fichier.
//!
//! Entre les deux — entre « le décodeur est juste » et « la conversion n'a pas
//! bougé » — personne ne vérifiait que **les octets du décodeur arrivent
//! intacts au puits**. Un facteur d'échelle faux dans `pcm_bytes_to_f32`, une
//! extension de signe 24 bits ratée, un DSP qui ne serait plus neutre au
//! repos : les deux moitiés resteraient vertes. Une empreinte de R1 bougerait,
//! certes — mais elle ne dirait pas si c'est le rendu qui a dérivé ou le
//! relevé qui était faux, parce qu'elle n'est comparée à rien d'extérieur.
//! Ici la référence est **hors du dépôt** : l'outil officiel du format.
//!
//! # Comment la comparaison est possible
//!
//! Le puits reçoit des `f32`. La table de T1 est en `i32`. Le pont est exact,
//! pas approché : `pcm_bytes_to_f32` divise par `32768.0` (16 bits) ou
//! `8388608.0` (24 bits) — des puissances de deux —, et les entiers concernés
//! tiennent tous dans les 24 bits de mantisse d'un `f32`. Remultiplier par la
//! même puissance de deux redonne l'entier **au bit près**, sans arrondi.
//! Toute inexactitude visible ici est donc dans la chaîne, pas dans la mesure.
//!
//! # 🔴 Ce que ce témoin NE couvre PAS
//!
//! Écrit ici pour que la tranche suivante sache où reprendre :
//!
//! * **Le rééchantillonnage n'est pas ancré sur une référence externe.** Aucun
//!   outil du dépôt ne produit un 44,1 → 48 kHz de référence, et le sinc de
//!   `rubato` n'a pas de « bonne réponse » publiée. Les témoins d'ici jouent
//!   au format identité et en adaptation de canaux seule. Le
//!   rééchantillonnage reste gardé par les relevés de R1 — contre la version
//!   d'avant, pas contre le monde extérieur.
//! * **L'adaptation de canaux n'est ancrée que dans le sens mono → stéréo**,
//!   parce que sa spécification est une duplication et se pose donc sans
//!   recopier le code. Le repli stéréo → mono (matrice de mixage) et les
//!   montées multicanal ne le sont pas.
//! * **Le DSP n'est éprouvé qu'AU REPOS.** Égaliseur, convolveur, crossfeed et
//!   repli mono actifs modifient le signal par construction : il n'existe pas
//!   de référence externe à leur opposer. Ce que ce témoin garde, c'est que le
//!   DSP au repos est **exactement** l'identité — la propriété qui n'avait
//!   jamais été mesurée bout en bout, et dont dépend tout le discours
//!   « bit-perfect ».
//! * **Aucun porteur DoP ne traverse ces fixtures.** Le refus DoP est gardé
//!   par R1 sur un flux synthétique ; les fixtures DSD du banc
//!   (`tests/dsd_empreintes_reference.rs`) passent par la conversion DSD→PCM
//!   du décodeur, pas par un porteur DoP 24 bits.
//! * **Rien du matériel** : ni cpal, ni anneau, ni pilote, ni la matrice
//!   ASIO/WASAPI/CoreAudio/ALSA, ni les trois bras exclusifs — qui ne passent
//!   pas par le puits.
//! * **Le puits n'est pas branché dans `play_url`.** Ce fichier monte la
//!   chaîne comme `play_url` la monte ; il ne prouve pas que `play_url`
//!   l'appelle. La garde de ce point-là, c'est `BoucleProducteur` elle-même,
//!   qui est le seul chemin, et R6 (décomposition de `local.rs`) la rendra
//!   testable directement.
//! * **Où il tourne** : `outputs::local` est derrière `local-audio`, absent du
//!   job `Test` (`ci.yml:262`, `--no-default-features`). Ce fichier n'est donc
//!   exécuté que par le job `audio-embedding` (`ci.yml:357`,
//!   `cargo test -p tune-core --features audio-embedding`, qui garde les
//!   fonctionnalités par défaut donc `local-audio`) — **sous `ci:full`
//!   seulement**. Tout ce qui pouvait être gardé sans le chemin local a été
//!   mis dans `tune-output-api/tests/puits_de_capture_2218.rs`, qui tourne sur
//!   toute PR Rust.

use md5::{Digest, Md5};

use super::PousseeVersLePuits;
use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use crate::outputs::traits::{CaptureOutput, FormatOuvert};

/// Taille des tranches poussées dans l'étage.
///
/// **4 093 octets, un nombre premier** : il ne tombe juste sur aucune trame
/// des fixtures — ni 4 octets (16 bits stéréo), ni 6 (24 bits stéréo), ni 2
/// (16 bits mono). Chaque tranche laisse donc un reliquat non aligné à
/// reporter, ce qui est le régime RÉEL d'une lecture réseau et le cas où un
/// octet perdu se transforme en bruit blanc (#3849). Une taille alignée
/// rendrait ce témoin aveugle à la gestion du tampon d'attente.
const TRANCHE_DE_LECTURE: usize = 4_093;

/// (fichier, canaux, cadence, profondeur, nb d'échantillons, empreinte md5)
///
/// **Les mêmes six colonnes que `tests/flac_empreintes_reference.rs`**, et les
/// mêmes valeurs : ce sont les relevés de `flac -d` (libFLAC 1.5.0), le
/// décodeur de RÉFÉRENCE du format. Elles sont répétées ici et non importées
/// parce que T1 est un binaire de test distinct — `autotests = false`, une
/// cible par fichier —, et qu'un `pub` traversant `src/` pour servir un
/// `tests/` ferait entrer une table de test dans la bibliothèque livrée.
///
/// La répétition est sans danger dans un seul sens, et c'est celui qui compte :
/// si T1 remesure, ce fichier rougit. Un relevé qui ne tombe plus est un
/// relevé qui a tort ; on ne l'ajuste pas, on cherche pourquoi.
const FIXTURES: &[(&str, u16, u32, u16, usize, &str)] = &[
    (
        "ref_16_44100_stereo.flac",
        2,
        44_100,
        16,
        35_280,
        "b702caf5d2257a84d3953c26526dd6dc",
    ),
    (
        "ref_24_96000_stereo.flac",
        2,
        96_000,
        24,
        19_200,
        "5647a1733e4ec46e7a1dd00e10feaf3c",
    ),
    (
        "ref_16_44100_mono.flac",
        1,
        44_100,
        16,
        8_820,
        "70c9300abe8f564171e38d869720fe1d",
    ),
];

fn chemin_fixture(nom: &str) -> String {
    format!("{}/tests/fixtures/flac/{nom}", env!("CARGO_MANIFEST_DIR"))
}

/// L'empreinte d'un train d'échantillons : MD5 des `i32` petit-boutistes.
///
/// Forme identique à celle de T1 et du garde WavPack, pour que les trois
/// tables se lisent de la même façon.
fn empreinte_i32(samples: &[i32]) -> String {
    let mut h = Md5::new();
    for s in samples {
        h.update(s.to_le_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Remet les mots livrés dans la représentation de la table de référence.
///
/// Exact, pas approché — voir l'en-tête du fichier : la division de
/// `pcm_bytes_to_f32` et cette multiplication sont la même puissance de deux.
/// L'`assert` le VÉRIFIE plutôt que de le supposer : si un mot ne retombe pas
/// sur un entier, la mesure elle-même est en cause et il faut le savoir avant
/// de lire l'empreinte.
fn requantifier(mots: &[f32], profondeur: u16) -> Vec<i32> {
    let echelle: f32 = match profondeur {
        16 => 32_768.0,
        24 => 8_388_608.0,
        autre => panic!("profondeur {autre} hors de la table de référence"),
    };
    mots.iter()
        .map(|mot| {
            let entier = mot * echelle;
            assert_eq!(
                entier,
                entier.trunc(),
                "un mot livré ({mot}) ne retombe pas sur un entier à l'échelle \
                 {echelle} : la chaîne a introduit une valeur intermédiaire, et \
                 la comparaison à la table de référence n'aurait plus de sens"
            );
            entier as i32
        })
        .collect()
}

/// Ce qu'une lecture complète jusqu'au puits a produit.
struct Livraison {
    puits: CaptureOutput,
    /// Vrai si un bloc a été classé porteur DoP quelque part dans la piste.
    dop_vu: bool,
}

/// Joue les octets PCM d'une piste dans la chaîne de sortie, jusqu'au puits.
///
/// Monte l'étage comme `play_url` le monte (via le constructeur de R1), pousse
/// par tranches non alignées, et n'arrête que sur `RienAPousser` faute
/// d'octets — c'est-à-dire jamais avant la fin du flux.
fn jouer_jusqu_au_puits(
    octets: &[u8],
    cadence: u32,
    canaux: u16,
    profondeur: u16,
    cadence_de_sortie: u32,
    canaux_de_sortie: u16,
    plafond_de_retenue: usize,
) -> Livraison {
    let dsp = DspAuRepos::neuf();
    let mut etage = etage(
        &dsp,
        Vec::new(),
        cadence,
        canaux,
        profondeur,
        cadence_de_sortie,
        canaux_de_sortie,
    );
    let mut puits = CaptureOutput::avec_retenue(
        FormatOuvert::new(cadence_de_sortie, canaux_de_sortie),
        plafond_de_retenue,
    );
    let dop_vu = std::cell::Cell::new(false);
    let mut refus = |dop: bool, _sr: u32, _ch: u16| {
        if dop {
            dop_vu.set(true);
        }
        false
    };

    for tranche in octets.chunks(TRANCHE_DE_LECTURE) {
        etage.en_attente.extend_from_slice(tranche);
        loop {
            match etage.pousser(&mut puits, &mut refus, &mut |_| {}) {
                PousseeVersLePuits::Poussee { .. } => {}
                PousseeVersLePuits::RienAPousser => break,
                PousseeVersLePuits::PuitsMort { .. } => {
                    panic!("le puits de capture ne meurt jamais : il ne bloque pas")
                }
                PousseeVersLePuits::PorteurDopRefuse => {
                    panic!("aucune fixture FLAC de ce banc ne porte de DoP")
                }
            }
        }
    }

    Livraison {
        puits,
        dop_vu: dop_vu.get(),
    }
}

/// Décode la fixture et vérifie d'abord qu'elle sort du décodeur telle que
/// `flac -d` la rend — l'ancrage de T1, refait ici.
///
/// Sans cette moitié, un témoin vert dirait seulement « le puits reçoit ce que
/// le décodeur rend », ce qui serait vrai même d'un décodeur cassé.
fn decoder_et_ancrer(
    nom: &str,
    md5_reference: &str,
    echantillons: usize,
) -> crate::audio::decode::DecodedAudio {
    let chemin = chemin_fixture(nom);
    assert!(
        std::path::Path::new(&chemin).exists(),
        "fixture absente : {chemin}"
    );
    let audio = crate::audio::decode::decode_to_pcm(&chemin, None, None, 0.0, 0.0)
        .unwrap_or_else(|err| panic!("{nom} : décodage refusé : {err}"));

    assert_eq!(
        audio.samples_i32.len(),
        echantillons,
        "{nom} : le décodeur ne rend plus le compte d'échantillons de la table"
    );
    assert_eq!(
        empreinte_i32(&audio.samples_i32),
        md5_reference,
        "{nom} : le DÉCODEUR a dérivé avant même que la chaîne de sortie n'entre \
         en jeu — inutile de lire ce que le puits a reçu"
    );
    audio
}

// ───────────────────────────────────────────────────────────────────────────
// Les gardes
// ───────────────────────────────────────────────────────────────────────────

/// **La garde décisive** : ce qui arrive au puits est, bit pour bit, ce que le
/// décodeur de référence du format produit.
///
/// Trois fixtures, trois choses différentes :
///
/// * **16 bits / 44,1 kHz stéréo** — le rip CD, l'immense majorité du parc ;
/// * **24 bits / 96 kHz stéréo** — le SEUL cas où l'extension de signe sur
///   trois octets et la sonde DoP 24 bits sont exercées. Aucun témoin 16 bits
///   ne peut voir une erreur d'extension de signe : il n'y en a pas ;
/// * **16 bits / 44,1 kHz mono** — un décodeur, ou une adaptation, qui
///   supposerait deux canaux rendrait ici la moitié ou le double.
#[test]
fn le_puits_recoit_le_pcm_du_decodeur_de_reference() {
    for (nom, canaux, cadence, profondeur, echantillons, md5) in FIXTURES {
        let audio = decoder_et_ancrer(nom, md5, *echantillons);
        assert_eq!(audio.channels, u32::from(*canaux), "{nom} : canaux");
        assert_eq!(audio.sample_rate, *cadence, "{nom} : cadence");
        assert_eq!(audio.bit_depth, *profondeur, "{nom} : profondeur");

        // Format identité : le périphérique est ouvert au format de la piste,
        // donc ni adaptation de canaux ni rééchantillonnage. Ce qui reste dans
        // le trajet, c'est le décodage des octets en flottants et le DSP au
        // repos — et c'est exactement ce qu'on prétend neutre.
        let livree = jouer_jusqu_au_puits(
            &audio.pcm_bytes(),
            *cadence,
            *canaux,
            *profondeur,
            *cadence,
            *canaux,
            *echantillons,
        );
        let puits = &livree.puits;

        assert!(
            !livree.dop_vu,
            "{nom} : un bloc a été classé porteur DoP — le DSP et le volume \
             seraient alors court-circuités, et cette mesure ne dirait plus rien"
        );
        assert!(
            puits.retenue_complete(),
            "{nom} : la retenue a été tronquée — plus de mots livrés que \
             d'échantillons décodés ({} mots pour {echantillons} attendus)",
            puits.mots()
        );
        assert_eq!(
            puits.mots(),
            *echantillons as u64,
            "{nom} : le puits n'a pas reçu autant de mots que le décodeur a \
             rendu d'échantillons"
        );
        assert_eq!(
            puits.blocs_non_alignes(),
            0,
            "{nom} : un bloc livré n'était pas un multiple des {canaux} canaux \
             ouverts — toutes les trames suivantes partent décalées d'un canal"
        );

        let recus = requantifier(puits.mots_livres().expect("le puits retient"), *profondeur);
        assert_eq!(
            empreinte_i32(&recus),
            *md5,
            "{nom} : ce que le PUITS a reçu n'est plus ce que `flac -d` \
             (libFLAC 1.5.0) produit. Entre le décodeur — vérifié juste \
             au-dessus — et le puits, il n'y a que `pcm_bytes_to_f32` et le DSP \
             au repos : l'un des deux n'est plus l'identité, et ce qui part au \
             DAC n'est plus le fichier"
        );
    }
}

/// Le format publié par le puits est celui du **périphérique**, pas celui de
/// la source — et les mots livrés suivent.
///
/// Une piste mono servie sur une sortie stéréo : c'est la seule adaptation de
/// canaux dont la spécification se pose sans recopier le code — « mono est
/// dupliqué sur la paire avant » —, donc la seule qu'on puisse ancrer sur la
/// référence externe. L'attendu est construit en répétant chaque échantillon
/// de `flac -d`, jamais en appelant `adapt_channels`.
///
/// Ce que ce témoin garde en plus : `audio/tap.rs` publie depuis le DÉCODAGE
/// (neuf appels à `send_windowed_pcm`, tous dans `audio/decode.rs`, aucun dans
/// la boucle producteur — relevé le 12/09/2026), donc au format SOURCE. Un
/// consommateur y lirait ici « 1 canal », alors que 2 partent au DAC.
#[test]
fn le_format_publie_est_celui_du_peripherique() {
    let (nom, canaux, cadence, profondeur, echantillons, md5) = FIXTURES[2];
    assert_eq!(canaux, 1, "ce témoin part de la fixture MONO");

    let audio = decoder_et_ancrer(nom, md5, echantillons);
    let livree = jouer_jusqu_au_puits(
        &audio.pcm_bytes(),
        cadence,
        canaux,
        profondeur,
        cadence,
        2,
        echantillons * 2,
    );
    let puits = &livree.puits;

    assert_eq!(
        puits.format(),
        FormatOuvert::new(44_100, 2),
        "le puits publie le format de la SOURCE au lieu de celui du périphérique"
    );
    assert_eq!(
        puits.trames(),
        echantillons as u64,
        "une trame mono devient une trame stéréo : le compte de TRAMES ne change pas"
    );
    assert_eq!(
        puits.mots(),
        echantillons as u64 * 2,
        "le compte de MOTS, lui, double"
    );
    assert_eq!(
        puits.duree_livree_ms(),
        200,
        "8 820 trames à 44,1 kHz durent 200 ms"
    );
    assert_eq!(puits.blocs_non_alignes(), 0);
    assert!(puits.retenue_complete());

    let attendu: Vec<i32> = audio.samples_i32.iter().flat_map(|s| [*s, *s]).collect();
    let recus = requantifier(puits.mots_livres().expect("le puits retient"), profondeur);
    assert_eq!(
        empreinte_i32(&recus),
        empreinte_i32(&attendu),
        "mono → stéréo ne duplique plus l'échantillon sur les deux voies : ce \
         que le puits reçoit n'est plus le signal de `flac -d` répété"
    );
}

/// Le découpage des lectures amont ne change RIEN à ce qui atteint le puits.
///
/// Le producteur ne choisit pas ses tranches : elles viennent d'un tampon
/// réseau. La même piste, poussée d'un seul bloc puis par tranches de 4 093
/// octets — un nombre premier, donc jamais aligné sur une trame —, doit rendre
/// la même empreinte ET le même nombre de mots. Un reliquat jeté au lieu
/// d'être reporté décalerait tout le reste du flux : c'est le bruit blanc
/// 24 bits de #3849.
#[test]
fn le_decoupage_des_lectures_ne_change_pas_ce_qui_atteint_le_puits() {
    // La fixture 24 bits : c'est celle dont la trame fait 6 octets, donc celle
    // où un reliquat mal reporté a le plus de façons de se voir.
    let (nom, canaux, cadence, profondeur, echantillons, md5) = FIXTURES[1];
    let audio = decoder_et_ancrer(nom, md5, echantillons);
    let octets = audio.pcm_bytes();

    let par_tranches = jouer_jusqu_au_puits(
        &octets,
        cadence,
        canaux,
        profondeur,
        cadence,
        canaux,
        echantillons,
    );

    let dsp = DspAuRepos::neuf();
    let mut etage_entier = etage(
        &dsp,
        octets.clone(),
        cadence,
        canaux,
        profondeur,
        cadence,
        canaux,
    );
    let mut entier = CaptureOutput::avec_retenue(FormatOuvert::new(cadence, canaux), echantillons);
    let mut refus = |_dop: bool, _sr: u32, _ch: u16| false;
    while let PousseeVersLePuits::Poussee { .. } =
        etage_entier.pousser(&mut entier, &mut refus, &mut |_| {})
    {}

    assert_eq!(
        entier.mots(),
        par_tranches.puits.mots(),
        "le découpage change le NOMBRE de mots livrés : un reliquat non aligné \
         est jeté au lieu d'être reporté"
    );
    assert_eq!(
        entier.empreinte(),
        par_tranches.puits.empreinte(),
        "le découpage des lectures change ce qui atteint le puits — tout le \
         flux part décalé à partir de la première tranche non alignée (#3849)"
    );
    assert_ne!(
        entier.blocs(),
        par_tranches.puits.blocs(),
        "les deux lectures doivent bien avoir découpé différemment, sans quoi \
         ce témoin ne compare rien"
    );
}

/// Un puits mort en pleine piste est constaté, et la lecture s'arrête là.
///
/// C'est le périphérique arraché (#1626) joué sur un VRAI fichier : jusqu'au
/// bloc qui tue le puits, ce qui a été livré doit être exactement le début du
/// signal de référence — pas un tronçon décalé, pas du silence.
#[test]
fn un_peripherique_arrache_en_pleine_piste_arrete_la_lecture() {
    let (nom, canaux, cadence, profondeur, echantillons, md5) = FIXTURES[0];
    let audio = decoder_et_ancrer(nom, md5, echantillons);
    let octets = audio.pcm_bytes();

    let dsp = DspAuRepos::neuf();
    // Étage VIDE, alimenté d'UNE tranche : sans cela le premier bloc serait la
    // piste entière, et « arraché en pleine piste » ne voudrait plus rien dire.
    let mut etage = etage(
        &dsp,
        Vec::new(),
        cadence,
        canaux,
        profondeur,
        cadence,
        canaux,
    );
    etage
        .en_attente
        .extend_from_slice(&octets[..TRANCHE_DE_LECTURE]);
    let mut puits = CaptureOutput::avec_retenue(FormatOuvert::new(cadence, canaux), echantillons);
    let mut refus = |_dop: bool, _sr: u32, _ch: u16| false;

    // Un bloc passe, puis le périphérique disparaît.
    match etage.pousser(&mut puits, &mut refus, &mut |_| {}) {
        PousseeVersLePuits::Poussee { .. } => {}
        _ => panic!("le premier bloc doit passer"),
    }
    let livres = puits.mots() as usize;
    assert!(livres > 0 && livres < echantillons, "un bloc, pas la piste");
    puits.declarer_mort();
    etage
        .en_attente
        .extend_from_slice(&octets[TRANCHE_DE_LECTURE..2 * TRANCHE_DE_LECTURE]);

    match etage.pousser(&mut puits, &mut refus, &mut |_| {}) {
        PousseeVersLePuits::PuitsMort { trames_source } => {
            assert!(
                trames_source > 0,
                "les trames consommées sont rendues même quand le puits est mort"
            );
        }
        _ => panic!("un puits qui rend false doit être rapporté comme mort"),
    }

    let recus = requantifier(
        &puits.mots_livres().expect("le puits retient")[..livres],
        profondeur,
    );
    assert_eq!(
        empreinte_i32(&recus),
        empreinte_i32(&audio.samples_i32[..livres]),
        "ce qui a été livré avant l'arrachement n'est pas le DÉBUT du signal de \
         référence : la lecture a commencé ailleurs, ou a sauté des trames"
    );
}
