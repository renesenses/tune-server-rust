//! REF-8 préparatoire de #2219 — **le puits des mots entiers**, sur toute PR
//! Rust.
//!
//! # Où ce fichier tourne
//!
//! `tune-output-api` n'a aucune fonctionnalité et figure dans le `-p` du job
//! `Test` de `ci.yml` (ligne 262). Ces témoins sont donc **exécutés** à chaque
//! PR Rust, pas seulement compilés.
//!
//! # Ce que ces témoins gardent
//!
//! * (a) Un bloc DoP synthétique — marqueurs `0x05`/`0xFA` alternés dans
//!   l'octet de poids fort de mots 24 bits, identiques sur les deux canaux,
//!   la forme de `native_windows_ring_preserves_every_dop_marker_and_payload_byte`
//!   dans `outputs/local/tests.rs` — traverse le puits **octet pour octet** :
//!   l'empreinte du puits est égale à un FNV-1a calculé ICI, indépendamment,
//!   sur les octets envoyés.
//! * (b) Un bloc à spec différente est refusé, compté, et le motif dit quelle
//!   spec était attendue et laquelle est venue.
//! * (c) Un bloc non aligné est haché entier, compté non aligné, et son reste
//!   est **gardé** puis complété par le bloc suivant : rien n'est perdu.
//! * (d) Un bloc vide laisse le puits vivant.
//!
//! # 🔴 Ce que ces témoins NE voient PAS
//!
//! * Aucun bras exclusif : CoreAudio, ASIO et WASAPI ne sont pas migrés par
//!   cette tranche et ne sont pas ici. Ce fichier éprouve **le puits**, pas ce
//!   qui le remplira.
//! * Aucune décision DoP : le puits ne renifle pas les marqueurs, et le témoin
//!   (a) ne le lui demande pas. Il vérifie que les octets sortent comme ils
//!   sont entrés, ce qui est la seule chose qui garde un DoP vivant.

use tune_output_api::{
    AudioSpec, CaptureOutputNatif, EMPREINTE_DU_VIDE, ProfondeurPcm, PuitsNatif, RefusNatif,
};

/// FNV-1a 64 bits, écrit ici et pas importé : c'est la référence indépendante
/// contre laquelle l'empreinte du puits est comparée.
fn fnv1a_de_reference(octets: &[u8]) -> u64 {
    let mut empreinte: u64 = 0xcbf2_9ce4_8422_2325;
    for octet in octets {
        empreinte ^= u64::from(*octet);
        empreinte = empreinte.wrapping_mul(0x0000_0100_0000_01b3);
    }
    empreinte
}

fn stereo_24(cadence: u32) -> AudioSpec {
    AudioSpec::nouvelle(cadence, ProfondeurPcm::Entier24, 2).expect("deux canaux")
}

/// Un bloc DoP synthétique : `trames` trames stéréo 24 bits petit-boutistes,
/// la charge DSD dans les deux octets bas (pseudo-aléatoire, déterministe), le
/// marqueur `0x05`/`0xFA` alternant d'une trame à l'autre dans l'octet de poids
/// fort, identique sur les deux canaux.
fn bloc_dop_synthetique(trames: usize) -> Vec<u8> {
    let mut graine: u32 = 0x2218_2219;
    let mut prochain = move || {
        graine = graine.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (graine >> 24) as u8
    };
    let mut octets = Vec::with_capacity(trames * 6);
    for trame in 0..trames {
        let marqueur = if trame % 2 == 0 { 0x05 } else { 0xFA };
        for _canal in 0..2 {
            octets.push(prochain());
            octets.push(prochain());
            octets.push(marqueur);
        }
    }
    octets
}

/// (a) Le DoP traverse octet pour octet : l'empreinte du puits est celle des
/// octets envoyés, calculée indépendamment.
#[test]
fn un_bloc_dop_traverse_le_puits_octet_pour_octet() {
    let spec = stereo_24(176_400);
    let envoye = bloc_dop_synthetique(64);
    assert_eq!(envoye.len(), 64 * 2 * 3);
    // La fabrique produit bien la forme DoP attendue, avant de la faire
    // traverser : sans cela, le témoin garderait un bloc quelconque.
    for (trame, paire) in envoye.chunks_exact(6).enumerate() {
        let marqueur = if trame % 2 == 0 { 0x05 } else { 0xFA };
        assert_eq!(paire[2], marqueur, "marqueur gauche, trame {trame}");
        assert_eq!(paire[5], marqueur, "marqueur droit, trame {trame}");
    }

    let mut puits = CaptureOutputNatif::ouvrir(spec);
    assert!(puits.ecrire(spec.bloc(&envoye)), "le puits est vivant");

    assert_eq!(
        puits.empreinte(),
        fnv1a_de_reference(&envoye),
        "l'empreinte du puits doit être celle des octets envoyés"
    );
    assert_ne!(puits.empreinte(), EMPREINTE_DU_VIDE);
    assert_eq!(puits.octets(), envoye.len() as u64);
    assert_eq!(puits.trames(), 64);
    assert_eq!(puits.blocs(), 1);
    assert_eq!(puits.blocs_non_alignes(), 0);
    assert_eq!(puits.blocs_refuses(), 0);
    assert!(puits.reste_en_attente().is_empty());
    assert!(puits.dernier_refus().is_none());
    assert!(puits.vivant());
}

/// (a bis) Le hachage est celui du flux, pas du bloc : la même charge DoP en
/// un bloc ou en huit donne la même empreinte. C'est ce qui rend (c) possible.
#[test]
fn l_empreinte_ne_depend_pas_du_decoupage_en_blocs() {
    let spec = stereo_24(176_400);
    let envoye = bloc_dop_synthetique(64);

    let mut en_un = CaptureOutputNatif::ouvrir(spec);
    assert!(en_un.ecrire(spec.bloc(&envoye)));

    let mut en_huit = CaptureOutputNatif::ouvrir(spec);
    for morceau in envoye.chunks(envoye.len() / 8) {
        assert!(en_huit.ecrire(spec.bloc(morceau)));
    }

    assert_eq!(en_un.empreinte(), en_huit.empreinte());
    assert_eq!(en_un.trames(), en_huit.trames());
    assert_eq!(en_huit.blocs(), 8);
}

/// (b) Un bloc à spec différente est refusé, compté, et le motif nomme les deux
/// formats.
#[test]
fn un_bloc_a_spec_differente_est_refuse_avec_les_deux_specs_nommees() {
    let attendue = stereo_24(44_100);
    let venue = AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier16, 2).expect("deux canaux");
    let octets = [0u8; 24];

    let mut puits = CaptureOutputNatif::ouvrir(attendue);
    assert!(
        puits.ecrire(attendue.bloc(&octets)),
        "au bon format, accepté"
    );
    let empreinte_avant = puits.empreinte();

    assert!(
        !puits.ecrire(venue.bloc(&octets)),
        "un bloc à spec différente doit être REFUSÉ (rendre false)"
    );

    assert_eq!(puits.blocs_refuses(), 1, "le refus est compté");
    assert_eq!(puits.blocs(), 2, "l'appel refusé compte comme un appel");
    assert_eq!(
        puits.empreinte(),
        empreinte_avant,
        "un bloc refusé n'est pas haché"
    );
    assert_eq!(
        puits.octets(),
        24,
        "un bloc refusé n'est pas compté en octets"
    );

    let motif = puits.dernier_refus().expect("un motif est consigné");
    assert_eq!(
        motif,
        RefusNatif::SpecDifferente {
            attendue,
            recue: venue
        }
    );
    let texte = motif.to_string();
    assert!(
        texte.contains("44100") && texte.contains("Entier24"),
        "le motif nomme la spec attendue : {texte}"
    );
    assert!(
        texte.contains("48000") && texte.contains("Entier16"),
        "le motif nomme la spec venue : {texte}"
    );
    assert!(
        !puits.vivant(),
        "un refus tue le puits : false n'a qu'un seul sens"
    );
    assert!(
        !puits.ecrire(attendue.bloc(&octets)),
        "mort, il rend false même au bon format"
    );
}

/// (c) Un bloc non aligné est haché pour tout ce qu'il apporte, compté non
/// aligné, et son reste est gardé puis complété par le bloc suivant.
#[test]
fn le_reste_non_aligne_est_garde_et_complete_par_le_bloc_suivant() {
    let spec = stereo_24(96_000);
    assert_eq!(spec.octets_par_trame(), 6);
    // Deux trames et demie : 15 octets = 2 trames + 3 octets de reste.
    let premier: Vec<u8> = (1..=15).collect();
    // Les 3 octets qui complètent la trame, puis une trame entière : 9 octets.
    let second: Vec<u8> = (16..=24).collect();

    let mut puits = CaptureOutputNatif::ouvrir(spec);

    assert!(puits.ecrire(spec.bloc(&premier)));
    assert_eq!(
        puits.blocs_non_alignes(),
        1,
        "le premier bloc est non aligné"
    );
    assert_eq!(puits.trames(), 2, "deux trames complètes");
    assert_eq!(
        puits.octets(),
        15,
        "les 15 octets sont comptés, reste compris"
    );
    assert_eq!(
        puits.reste_en_attente(),
        &[13, 14, 15],
        "le reste est gardé, pas jeté"
    );
    assert_eq!(
        puits.empreinte(),
        fnv1a_de_reference(&premier),
        "le reste est haché avec le bloc"
    );

    assert!(puits.ecrire(spec.bloc(&second)));
    assert_eq!(
        puits.blocs_non_alignes(),
        2,
        "9 octets ne font pas un multiple de 6 : compté sur la partition du bloc"
    );
    assert_eq!(
        puits.trames(),
        4,
        "13..15 + 16..18 font une trame, 19..24 une autre"
    );
    assert!(
        puits.reste_en_attente().is_empty(),
        "15 + 9 = 24 octets = 4 trames, rien en attente"
    );
    let mut tout = premier.clone();
    tout.extend_from_slice(&second);
    assert_eq!(
        puits.empreinte(),
        fnv1a_de_reference(&tout),
        "l'empreinte est celle des 24 octets, dans l'ordre, sans perte"
    );
    assert_eq!(puits.octets(), 24);
    assert!(puits.vivant());
}

/// (c bis) Un bloc plus court qu'une trame s'accumule dans le reste, sans
/// jamais dépasser une trame ; la trame se compte quand elle est complète.
#[test]
fn des_blocs_plus_courts_qu_une_trame_s_accumulent_sans_perte() {
    let spec = stereo_24(48_000);
    let mut puits = CaptureOutputNatif::ouvrir(spec);
    let octets: Vec<u8> = (0..12).collect();
    for octet in &octets {
        assert!(puits.ecrire(spec.bloc(std::slice::from_ref(octet))));
        assert!(
            puits.reste_en_attente().len() < 6,
            "toujours sous une trame"
        );
    }
    assert_eq!(puits.trames(), 2);
    assert_eq!(
        puits.blocs_non_alignes(),
        12,
        "chaque bloc d'un octet est non aligné"
    );
    assert!(puits.reste_en_attente().is_empty());
    assert_eq!(puits.empreinte(), fnv1a_de_reference(&octets));
}

/// (d) Un bloc vide laisse le puits vivant et ne change rien à l'empreinte.
#[test]
fn un_bloc_vide_laisse_le_puits_vivant() {
    let spec = stereo_24(44_100);
    let mut puits = CaptureOutputNatif::ouvrir(spec);
    assert!(puits.ecrire(spec.bloc(&[])), "vivant sur un bloc vide");
    assert_eq!(puits.blocs(), 1);
    assert_eq!(puits.blocs_vides(), 1);
    assert_eq!(puits.blocs_non_alignes(), 0, "vide n'est pas non aligné");
    assert_eq!(puits.empreinte(), EMPREINTE_DU_VIDE);
    assert_eq!(puits.trames(), 0);
    assert!(puits.vivant());
}

/// Le contrat de `false` : un puits déclaré mort rend `false`, et seulement lui.
#[test]
fn un_puits_declare_mort_rend_false() {
    let spec = stereo_24(44_100);
    let mut puits = CaptureOutputNatif::ouvrir(spec);
    puits.declarer_mort();
    assert!(!puits.ecrire(spec.bloc(&[0u8; 6])));
    assert!(
        puits.dernier_refus().is_none(),
        "mort n'est pas un refus de spec"
    );
    assert_eq!(puits.blocs_refuses(), 0);
}
