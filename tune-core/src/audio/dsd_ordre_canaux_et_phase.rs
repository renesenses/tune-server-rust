//! Les deux choses qu'un DSD mal lu casse en premier : l'ORDRE DES CANAUX et
//! la PHASE DE TRAME. (#2218, tranche T3)
//!
//! Ces deux témoins complètent la table de `tests/dsd_empreintes_reference.rs`.
//! La table garde le train ENTIER ; ces deux-ci nomment la faute quand elle
//! survient, et ils portent chacun leur propre contre-épreuve intégrée — une
//! garde qui ne sait pas montrer ce qu'elle refuse ne garde rien.
//!
//! Ce qui les distingue de [`crate::audio::dop_porteur_bout_en_bout`] : ce
//! module-là fabrique sa source (« un fichier DSD dont chaque octet est
//! identifiable ») et prouve la CONSERVATION du porteur DoP. Ici, la source est
//! un **fichier tiers** — trois fixtures que WavPack 5.8.1 lit et dont il donne
//! l'empreinte de référence (voir `tests/dsd_empreintes_reference.rs` pour la
//! provenance et les commandes de régénération).
//!
//! Les empreintes par canal ci-dessous sont celles du train de
//! `wvunpack --raw` (WavPack 5.8.1), découpé canal par canal :
//!
//! ```text
//! f=tune-core/tests/fixtures/dsd/ref_dsd64_stereo.dsf ; nch=2
//! wavpack  -y -q -h "$f" -o ref.wv
//! wvunpack -y -q --raw ref.wv -o ref.raw
//! for c in $(seq 0 $((nch-1))); do
//!   NCH=$nch C=$c perl -0777 -ne 'my ($n,$c)=($ENV{NCH},$ENV{C});
//!     my @b=unpack("C*",$_); print pack("C*", @b[grep { $_ % $n == $c } 0..$#b])' \
//!     ref.raw | md5sum
//! done
//! ```
//!
//! Aucun matériel n'est nécessaire : tout se mesure sur les octets. Ces témoins
//! vivent donc hors de la caractéristique `local-audio`, comme les trois
//! premiers de `dop_porteur_bout_en_bout.rs`, et tournent sur TOUTE PR Rust par
//! le job `test` de `ci.yml`.

use md5::{Digest, Md5};

fn empreinte(octets: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(octets);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Miroir des bits d'un octet — DSF range le DSD LSB d'abord, DSDIFF et
/// `wvunpack --raw` le rangent MSB d'abord. Copie volontaire du `reverse_bits`
/// privé de [`crate::audio::dsd_to_dop`] : une garde qui appellerait la
/// fonction testée pour calculer son attendu ne garderait rien.
fn miroir_des_bits(b: u8) -> u8 {
    let mut r = 0u8;
    for i in 0..8 {
        r |= ((b >> i) & 1) << (7 - i);
    }
    r
}

fn chemin_fixture(nom: &str) -> String {
    format!("{}/tests/fixtures/dsd/{nom}", env!("CARGO_MANIFEST_DIR"))
}

/// Le train DSD d'une fixture, sous la forme canonique de `wvunpack --raw`
/// (entrelacé par octet, MSB d'abord), lu d'un coup.
fn train_canonique(nom: &str) -> (usize, Vec<u8>) {
    let chemin = chemin_fixture(nom);
    if nom.ends_with(".dsf") {
        let info = crate::audio::dsf::parse_dsf(&chemin).expect("parse_dsf");
        let brut = crate::audio::dsf::read_dsf_blocks(&chemin, &info).expect("read_dsf_blocks");
        (
            info.channels as usize,
            brut.iter().copied().map(miroir_des_bits).collect(),
        )
    } else {
        let info = crate::audio::dff::parse_dff(&chemin).expect("parse_dff");
        let brut = crate::audio::dff::read_dff_data(&chemin, &info).expect("read_dff_data");
        (info.channels as usize, brut)
    }
}

/// Le même train, mais reconstitué par le lecteur en FLOT — celui que la
/// production emploie réellement (`decode.rs` ne passe que par là).
fn train_canonique_en_flot(nom: &str) -> (usize, Vec<u8>) {
    let chemin = chemin_fixture(nom);
    let mut train = Vec::new();
    if nom.ends_with(".dsf") {
        let info = crate::audio::dsf::parse_dsf(&chemin).expect("parse_dsf");
        let canaux = info.channels as usize;
        let mut lecteur =
            crate::audio::dsf::DsfStreamReader::open(&chemin, info).expect("open dsf");
        while let Some(tranche) = lecteur.next_chunk().expect("next_chunk dsf") {
            train.extend(tranche.iter().copied().map(miroir_des_bits));
        }
        (canaux, train)
    } else {
        let info = crate::audio::dff::parse_dff(&chemin).expect("parse_dff");
        let canaux = info.channels as usize;
        // La taille de tranche de la production : un multiple du nombre de
        // canaux, comme le documente `DffStreamReader::open`.
        let taille = 32768 / canaux * canaux;
        let mut lecteur =
            crate::audio::dff::DffStreamReader::open(&chemin, &info, taille).expect("open dff");
        while let Some(tranche) = lecteur.next_chunk().expect("next_chunk dff") {
            train.extend_from_slice(&tranche);
        }
        (canaux, train)
    }
}

/// Découpe un train entrelacé en un train par canal.
fn par_canal(train: &[u8], canaux: usize) -> Vec<Vec<u8>> {
    (0..canaux)
        .map(|c| train.iter().skip(c).step_by(canaux).copied().collect())
        .collect()
}

/// (fixture, empreintes par canal, empreinte du train entier)
///
/// Toutes issues de `wvunpack --raw` (WavPack 5.8.1) — voir l'en-tête.
const ATTENDU: &[(&str, &[&str], &str)] = &[
    (
        "ref_dsd64_stereo.dsf",
        &[
            "623e15e4fea1eeb3c95d0b3ff525f734",
            "84cc022fd2146d06dfa9e9a653f0d8c2",
        ],
        "3d777969a6530203851432534c95954b",
    ),
    (
        "ref_dsd64_stereo.dff",
        &[
            "623e15e4fea1eeb3c95d0b3ff525f734",
            "84cc022fd2146d06dfa9e9a653f0d8c2",
        ],
        "3d777969a6530203851432534c95954b",
    ),
    (
        "ref_dsd64_5v1.dff",
        &[
            "0da72289797d59109f5c88da69f95ca5",
            "a5ad7bea8f4704d903a909bfa55300b9",
            "1835a076c1f948112c92b795be03bef5",
            "48323415e592f00d1aba239203285a85",
            "5bbb455761a8f3b1a617abd313d66fc6",
            "b529bd524288418c319f85c41944d824",
        ],
        "415c685f552561781de8470ae9f67fe0",
    ),
];

/// TÉMOIN — chaque canal extrait est celui que le décodeur de référence place à
/// ce rang.
///
/// Deux canaux échangés, c'est l'image stéréo retournée sur toute une
/// discothèque, et en 5.1 c'est le centre dans un caisson. Aucun témoin de
/// longueur, de cadence ou de durée ne peut le voir : le train entier est
/// intact, seul son ORDRE a bougé. C'est pourquoi l'attendu est ici une
/// empreinte PAR CANAL et non une empreinte globale.
///
/// La fixture est bâtie pour que la faute soit visible : chaque canal porte une
/// sinusoïde de fréquence ET de phase différentes. Deux canaux identiques
/// octet pour octet rendraient ce témoin vert quoi qu'il arrive.
#[test]
fn l_ordre_des_canaux_du_train_dsd_est_celui_du_fichier() {
    for (nom, canaux_attendus, entier) in ATTENDU {
        let (canaux, train) = train_canonique(nom);
        assert_eq!(canaux, canaux_attendus.len(), "{nom} : nombre de canaux");

        // Le rang AVANT le tout : un échange de canaux laisse le train entier
        // de la même longueur et ne change que la répartition. C'est la
        // comparaison par canal qui NOMME la faute ; l'empreinte globale ne
        // dirait que « ce n'est pas le bon train ».
        let trains = par_canal(&train, canaux);
        for (rang, attendu) in canaux_attendus.iter().enumerate() {
            assert_eq!(
                empreinte(&trains[rang]),
                *attendu,
                "{nom} : le canal {rang} ne porte pas le train que `wvunpack --raw` \
                 (WavPack 5.8.1) place à ce rang — les canaux DSD sont dans le \
                 mauvais ordre"
            );
        }
        assert_eq!(empreinte(&train), *entier, "{nom} : train entier");

        // Contre-épreuve intégrée : l'échange de deux canaux doit faire bouger
        // ce que ce témoin compare. Sans elle, on ne saurait pas que les
        // empreintes par canal DISCRIMINENT réellement l'ordre.
        let mut echange = train.clone();
        for i in (0..echange.len()).step_by(canaux) {
            echange.swap(i, i + 1);
        }
        assert_ne!(
            empreinte(&par_canal(&echange, canaux)[0]),
            *canaux_attendus[0],
            "{nom} : intervertir les canaux 0 et 1 rend la MÊME empreinte pour le \
             canal 0 — ce témoin ne garde donc pas l'ordre des canaux, quoi qu'il \
             affirme"
        );
        assert_eq!(
            echange.len(),
            train.len(),
            "{nom} : l'échange de canaux doit conserver la longueur du train"
        );
    }
}

/// TÉMOIN — la phase de trame ne glisse pas d'un octet, ni à la lecture d'un
/// coup, ni en flot.
///
/// Un octet DSD perdu ou inséré décale TOUT l'aval d'un rang : le canal 0 du
/// décodeur devient le canal 1 du fichier, et — en DoP — le marqueur
/// `0x05`/`0xFA` quitte l'octet de poids fort. Le DAC ne verrouille plus en
/// DSD, lit le train comme du PCM, et joue du bruit blanc (#1894, #2369).
///
/// Deux sites peuvent faire glisser cette phase, et ce témoin les couvre tous
/// les deux :
///
/// 1. **Le remplissage de fin de super-bloc DSF.** `ref_dsd64_stereo.dsf` porte
///    9 000 octets par canal, hors multiple de 4 096 : le fichier contient donc
///    3 × 4 096 = 12 288 octets par canal, dont 3 288 de zéros à retrancher. Un
///    remplissage conservé ne serait pas du silence — c'est du DSD constant à
///    zéro, soit un continu pleine échelle.
/// 2. **Le reste de tranche en lecture par flot.** C'est le défaut qu'a corrigé
///    `un_dff_multicanal_ne_perd_plus_un_octet_par_bloc` : six octets — un par
///    canal — jetés toutes les 2,7 ms. Ici le fichier n'est plus fabriqué par
///    le test, et l'attendu vient d'un tiers.
#[test]
fn la_phase_de_trame_du_train_dsd_ne_glisse_pas() {
    for (nom, canaux_attendus, entier) in ATTENDU {
        let (canaux, dun_coup) = train_canonique(nom);
        let (canaux_flot, en_flot) = train_canonique_en_flot(nom);

        assert_eq!(canaux, canaux_flot, "{nom} : canaux, d'un coup vs en flot");
        assert_eq!(
            empreinte(&en_flot),
            *entier,
            "{nom} : le train reconstitué par le lecteur en FLOT — celui de la \
             production — n'est plus celui de `wvunpack --raw` (WavPack 5.8.1) : \
             un octet a été perdu, inséré, ou un remplissage de fin de bloc a \
             été conservé"
        );
        assert_eq!(
            empreinte(&dun_coup),
            empreinte(&en_flot),
            "{nom} : la lecture d'un coup et la lecture en flot ne rendent plus le \
             même train"
        );
        assert_eq!(
            en_flot.len() % canaux,
            0,
            "{nom} : le train ne contient pas un nombre entier de trames — la \
             phase de trame est rompue par construction"
        );

        // Contre-épreuve intégrée : un décalage d'UN octet, à longueur
        // CONSTANTE. C'est la seule forme qui prouve que l'empreinte mord, et
        // non la longueur : un témoin qui ne compterait que les octets
        // resterait vert ici.
        let mut decale = en_flot[1..].to_vec();
        decale.push(0);
        assert_eq!(
            decale.len(),
            en_flot.len(),
            "{nom} : décalage à longueur égale"
        );
        assert_ne!(
            empreinte(&decale),
            *entier,
            "{nom} : décaler la phase de trame d'un octet rend la MÊME empreinte — \
             ce témoin ne garde donc pas la phase de trame"
        );
        assert_ne!(
            empreinte(&par_canal(&decale, canaux)[0]),
            *canaux_attendus[0],
            "{nom} : un décalage d'un octet laisse le canal 0 identique — la \
             fixture ne distingue pas ses canaux, et ce témoin ne garde rien"
        );
    }
}
