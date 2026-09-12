//! T8 de #2218 — le contrat du puits de capture, **sur toute PR Rust**.
//!
//! # Où ce fichier tourne, et pourquoi il est ici
//!
//! `tune-output-api` n'a **aucune fonctionnalité** et figure dans le `-p` du
//! job `Test` de `ci.yml` (ligne 262), qui tourne sur toutes les PR Rust avec
//! `--no-default-features --features oaat,cloud-relay,bandcamp`. Ce fichier y
//! est donc **exécuté**, pas seulement compilé.
//!
//! La garde décisive de la tranche — une fixture du banc jouée d'un bout à
//! l'autre jusqu'au puits — ne peut pas vivre ici : elle a besoin de
//! `EtageDeConversion`, qui est privé de `tune_core::outputs::local`, lui-même
//! derrière `local-audio`. Elle vit dans
//! `tune-core/src/outputs/local/capture_bout_en_bout_2218.rs` et ne tourne que
//! sous `ci:full` (job `audio-embedding`, `ci.yml` ligne 357). Le partage est
//! délibéré : **tout ce qui peut être gardé sans le chemin local l'est ici.**
//!
//! # 🔴 Ce que ce fichier ne couvre PAS
//!
//! * Rien de la chaîne de lecture : ni décodage, ni DSP, ni adaptation de
//!   canaux, ni rééchantillonnage. Il éprouve **le puits**, pas ce qui le
//!   remplit. C'est `capture_bout_en_bout_2218.rs` qui relie les deux.
//! * Le format publié est ici celui qu'on a **déclaré** à l'ouverture. Que ce
//!   soit bien celui du périphérique, et non celui de la source, ne se mesure
//!   qu'en jouant : c'est le témoin `le_format_publie_est_celui_du_peripherique`
//!   de l'autre fichier.
//! * Aucun fil : le trait prend `&mut self`, un seul producteur écrit.

use tune_output_api::{CaptureOutput, EMPREINTE_DU_VIDE, FormatOuvert, PuitsDEchantillons};

/// FNV-1a 64 bits de la représentation petit-boutiste de `1.0_f32`, soit les
/// quatre octets `00 00 80 3F`.
///
/// Un RELEVÉ, pas une valeur attendue — et il ne vient pas de ce module :
/// FNV-1a est une spécification publiée (décalage de base
/// `0xcbf29ce484222325`, nombre premier `0x100000001b3`, XOR puis
/// multiplication, un octet à la fois). Quatre tours à la main donnent
/// `0x4b72477f9c5c2f98`. Si ce chiffre ne tombe plus, c'est le hachage du
/// puits qui a changé — et les quatre relevés de R1
/// (`empreinte_du_puits_r1.rs`), mesurés sur la chaîne d'avant la
/// réorganisation, ne tomberaient plus non plus.
const EMPREINTE_DE_UN: u64 = 0x4b72_477f_9c5c_2f98;

fn puits(cadence: u32, canaux: u16) -> CaptureOutput {
    CaptureOutput::ouvert(FormatOuvert::new(cadence, canaux))
}

/// Un puits qui n'a rien reçu porte le décalage de base, et rien d'autre.
#[test]
fn un_puits_neuf_ne_pretend_rien_avoir_recu() {
    let p = puits(44_100, 2);

    assert_eq!(p.empreinte(), EMPREINTE_DU_VIDE, "empreinte du vide");
    assert_eq!(p.mots(), 0);
    assert_eq!(p.blocs(), 0);
    assert_eq!(p.trames(), 0);
    assert_eq!(p.duree_livree_ms(), 0);
    assert!(p.vivant(), "un puits neuf consomme");
    assert!(
        p.mots_livres().is_none(),
        "un puits sans retenue ne rend aucun mot, il ne rend pas une tranche vide"
    );
}

/// Le hachage est celui de FNV-1a sur les OCTETS, et il est verrouillé.
#[test]
fn l_empreinte_est_celle_de_fnv1a_sur_les_octets_du_flottant() {
    let mut p = puits(44_100, 1);
    assert!(p.ecrire(&[1.0]));

    assert_eq!(
        p.empreinte(),
        EMPREINTE_DE_UN,
        "le puits ne hache plus les octets petit-boutistes du f32 avec FNV-1a : \
         les relevés de R1 ne tomberaient plus"
    );
}

/// Deux nombres mathématiquement égaux, deux représentations : le puits les
/// distingue.
///
/// `0.0 == -0.0` est vrai en flottant. Un puits qui hacherait la VALEUR ne
/// verrait donc pas une inversion de signe sur un silence — et un DAC, si.
#[test]
fn le_puits_voit_la_representation_pas_la_valeur() {
    let mut positif = puits(44_100, 1);
    let mut negatif = puits(44_100, 1);
    positif.ecrire(&[0.0]);
    negatif.ecrire(&[-0.0]);

    assert_eq!(0.0_f32, -0.0_f32, "les deux valeurs SONT égales");
    assert_ne!(
        positif.empreinte(),
        negatif.empreinte(),
        "le puits hache la valeur et non les octets : un zéro négatif passerait \
         inaperçu"
    );
}

/// L'ordre compte. Deux blocs permutés ne rendent pas la même empreinte.
#[test]
fn l_empreinte_depend_de_l_ordre_de_livraison() {
    let mut avant = puits(44_100, 2);
    avant.ecrire(&[0.25, -0.5]);
    avant.ecrire(&[0.75, -1.0]);

    let mut apres = puits(44_100, 2);
    apres.ecrire(&[0.75, -1.0]);
    apres.ecrire(&[0.25, -0.5]);

    assert_eq!(avant.mots(), apres.mots(), "même nombre de mots");
    assert_ne!(
        avant.empreinte(),
        apres.empreinte(),
        "deux blocs permutés rendent la même empreinte : le puits ne verrait pas \
         un entrelacement inversé"
    );
}

/// Le découpage en blocs n'est PAS dans l'empreinte.
///
/// Le producteur découpe selon ce que l'amont lui rend — un tampon réseau, une
/// fin de fichier. Un puits dont l'empreinte dépendrait du découpage rendrait
/// un rouge à chaque lecture, et ne garderait rien du tout.
#[test]
fn le_decoupage_en_blocs_ne_change_pas_l_empreinte() {
    let mots: Vec<f32> = (0..1_000).map(|i| (i as f32) / 1_000.0 - 0.5).collect();

    let mut entier = puits(44_100, 2);
    entier.ecrire(&mots);

    let mut coupe = puits(44_100, 2);
    for tranche in mots.chunks(7) {
        coupe.ecrire(tranche);
    }

    assert_eq!(
        entier.empreinte(),
        coupe.empreinte(),
        "l'empreinte dépend du découpage : elle mesurerait le tampon de lecture \
         et non le signal"
    );
    assert_eq!(entier.mots(), coupe.mots());
    assert_ne!(entier.blocs(), coupe.blocs(), "les blocs, eux, diffèrent");
}

/// Le format publié est celui de l'OUVERTURE, et la durée s'en déduit.
///
/// 44 100 mots stéréo, c'est 22 050 trames — 500 ms à 44,1 kHz, 459 ms à
/// 48 kHz. Lire la durée à la cadence de la source au lieu de celle du
/// périphérique se trompe de 8,7 %.
#[test]
fn le_format_ouvert_est_publie_et_porte_la_duree() {
    let mut p = puits(48_000, 2);
    p.ecrire(&vec![0.0; 44_100]);

    assert_eq!(p.format(), FormatOuvert::new(48_000, 2));
    assert_eq!(p.trames(), 22_050, "44 100 mots stéréo font 22 050 trames");
    assert_eq!(
        p.duree_livree_ms(),
        459,
        "22 050 trames à 48 kHz durent 459 ms — 500 ms serait la lecture à la \
         cadence de la SOURCE"
    );
}

/// Un bloc dont la longueur n'est pas un multiple des canaux est COMPTÉ.
///
/// Aucun anneau ne peut le signaler : il range des mots, pas des trames. Un
/// seul bloc impair et toutes les trames suivantes sont décalées d'un canal —
/// la voie gauche part à droite et n'y revient jamais.
#[test]
fn un_bloc_non_aligne_sur_les_canaux_est_compte() {
    let mut p = puits(44_100, 2);
    p.ecrire(&[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(p.blocs_non_alignes(), 0, "quatre mots stéréo sont alignés");

    p.ecrire(&[0.5, 0.6, 0.7]);
    assert_eq!(
        p.blocs_non_alignes(),
        1,
        "trois mots livrés sur une sortie stéréo décalent tout ce qui suit"
    );
}

/// Un bloc vide n'est pas une fin de flux, et il est compté à part.
#[test]
fn un_bloc_vide_est_compte_sans_rien_hacher() {
    let mut p = puits(44_100, 2);
    assert!(p.ecrire(&[]), "un bloc vide ne tue pas le puits");

    assert_eq!(p.blocs(), 1);
    assert_eq!(p.blocs_vides(), 1);
    assert_eq!(p.mots(), 0);
    assert_eq!(
        p.empreinte(),
        EMPREINTE_DU_VIDE,
        "un bloc vide ne doit pas faire avancer l'empreinte"
    );
}

/// Un puits mort rend `false`, et continue de compter ce qu'on lui a donné.
///
/// La moitié du contrat de `PuitsDEchantillons` : `false` veut dire « le
/// consommateur est mort », jamais « arrêt demandé ». Les mots du dernier bloc
/// ont bel et bien traversé la conversion, et le producteur les compte comme
/// position (voir `PousseeVersLePuits::PuitsMort`).
#[test]
fn un_puits_mort_rend_false_sans_cesser_de_compter() {
    let mut p = puits(44_100, 2);
    assert!(p.ecrire(&[0.1, 0.2]));
    p.declarer_mort();

    assert!(!p.vivant());
    assert!(
        !p.ecrire(&[0.3, 0.4]),
        "un puits déclaré mort doit rendre false"
    );
    assert_eq!(
        p.mots(),
        4,
        "les mots du bloc refusé restent comptés : ils ont traversé la conversion"
    );
}

/// La retenue rend EXACTEMENT ce qui a été livré, dans l'ordre.
#[test]
fn la_retenue_rend_les_mots_livres_dans_l_ordre() {
    let mut p = CaptureOutput::avec_retenue(FormatOuvert::new(44_100, 2), 8);
    p.ecrire(&[0.1, 0.2]);
    p.ecrire(&[0.3, 0.4, 0.5, 0.6]);

    assert!(
        p.retenue_complete(),
        "six mots tiennent sous un plafond de huit"
    );
    assert_eq!(
        p.mots_livres(),
        Some(&[0.1_f32, 0.2, 0.3, 0.4, 0.5, 0.6][..])
    );
}

/// Une retenue tronquée est CONSTATÉE, jamais silencieuse.
///
/// C'est la garde du faux vert de cette tranche : un témoin qui comparerait
/// une référence entière à une retenue rognée verrait un tronçon et le
/// déclarerait conforme.
#[test]
fn une_retenue_tronquee_se_declare() {
    let mut p = CaptureOutput::avec_retenue(FormatOuvert::new(44_100, 2), 3);
    p.ecrire(&[0.1, 0.2]);
    assert!(p.retenue_complete());

    p.ecrire(&[0.3, 0.4, 0.5]);
    assert!(
        !p.retenue_complete(),
        "le plafond a été dépassé sans que le puits le dise : un témoin \
         comparerait une référence entière à un tronçon et le croirait conforme"
    );
    assert_eq!(
        p.mots_livres().map(<[f32]>::len),
        Some(3),
        "la retenue s'arrête au plafond"
    );
    assert_eq!(
        p.mots(),
        5,
        "le COMPTE, lui, reste celui de tout ce qui a été livré"
    );
    assert_ne!(
        p.empreinte(),
        EMPREINTE_DU_VIDE,
        "et l'empreinte aussi porte les cinq mots : elle ne dépend pas de la retenue"
    );
}
