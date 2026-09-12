//! DSD : le train extrait d'un `.dsf` ou d'un `.dff` doit être celui de
//! `wvunpack --raw`.
//!
//! # Tranche T3 du banc de conformité (#2218)
//!
//! Même motif que la tranche T1 (`tests/flac_empreintes_reference.rs`, #3946) :
//! un tuple par fixture, et une garde qui le compare au décodeur de RÉFÉRENCE
//! du format — jamais au module éprouvé. C'est le motif du correctif WavPack
//! (`4be4ba8d`, #3849).
//!
//! # Pourquoi le DSD, et pourquoi maintenant
//!
//! Le DSD est le chemin qui a le plus récemment produit du bruit blanc chez des
//! testeurs : le flux DoP partait avec DEUX en-têtes WAV depuis la v0.9.82,
//! soixante versions durant, sans qu'aucun témoin ne le voie (#1894, #2369).
//!
//! État mesuré sur `batch/bugs-11` (e4d691f6) avant cette tranche : **aucune
//! fixture DSD dans le dépôt** — `find` sur `.dsf` et `.dff` rendait zéro. Les
//! quatre modules du chemin DSD pèsent pourtant 121 583 octets :
//!
//! | module | taille |
//! |---|---|
//! | `src/audio/dsf.rs` | 18 538 o. |
//! | `src/audio/dff.rs` | 38 774 o. |
//! | `src/audio/dsd_to_pcm.rs` | 41 868 o. |
//! | `src/audio/dsd_to_dop.rs` | 22 403 o. |
//!
//! Et tout ce qui les éprouvait **fabriquait sa propre source** :
//! `tests/audio_integration.rs:113` et `:161` écrivent leurs `.dsf` et `.dff`
//! à la main, et `src/audio/dop_porteur_bout_en_bout.rs` — un excellent module
//! par ailleurs — « fabrique un fichier DSD dont chaque octet est
//! identifiable ». Il prouve la CONSERVATION du porteur, pas la LECTURE
//! correcte d'un fichier tiers. Un test qui se nourrit lui-même ne garde rien.
//!
//! # D'où viennent les empreintes, et comment les régénérer
//!
//! De **WavPack 5.8.1** (`wavpack`/`wvunpack`, David Bryant — l'implémentation
//! de référence du format, déjà la référence de `src/audio/wavpack.rs` dans ce
//! dépôt). WavPack 5 lit nativement le DSF et le DSDIFF : c'est donc un
//! analyseur de conteneur DSD tiers, complet et versionné. ffmpeg est banni de
//! toute chaîne de lecture de Tune, y compris comme décodeur de référence dans
//! un test (`tests/no_blind_ffmpeg.rs`).
//!
//! Empreinte d'une fixture — les commandes exactes, telles quelles :
//!
//! ```text
//! f=tune-core/tests/fixtures/dsd/ref_dsd64_stereo.dsf
//! wavpack  -y -q -h "$f" -o ref.wv        # WavPack 5.8.1
//! wvunpack -y -q --raw ref.wv -o ref.raw  # train DSD brut, sans en-tête
//! md5sum ref.raw                          # `md5 -q` sous macOS
//! wc -c < ref.raw                         # octets du train (canaux confondus)
//! ```
//!
//! `wvunpack --raw` rend le train DSD **entrelacé par octet**
//! (`ch0 ch1 ch0 ch1 …`) et **MSB d'abord**. Mesuré le 12/09/2026 : le même
//! `md5` sort du `.dsf` et du `.dff` d'un même contenu — WavPack normalise
//! l'ordre des bits sur la convention DSDIFF. C'est la forme CANONIQUE que
//! cette table hache.
//!
//! Côté Tune, les deux lectures ne rendent pas la même convention, et c'est
//! conforme aux spécifications :
//!
//! - [`dff::read_dff_data`] rend les octets tels quels — DSDIFF stocke MSB
//!   d'abord : forme canonique directe ;
//! - [`dsf::read_dsf_blocks`] désentrelace les blocs mais garde l'ordre des
//!   bits du fichier — DSF stocke LSB d'abord (`bits_per_sample = 1` au
//!   format chunk) : il faut donc **un miroir de bits par octet** pour
//!   retomber sur la forme canonique. Ce miroir est écrit ici
//!   ([`miroir_des_bits`]) et non emprunté à `dsd_to_dop` : une garde qui
//!   appellerait le code testé pour calculer son attendu ne garderait rien.
//!
//! # D'où viennent les fixtures
//!
//! Contenu **100 % synthétique**, écrit pour cette tranche : aucune œuvre,
//! aucun droit à demander. Un modulateur sigma-delta du 2ᵉ ordre module une
//! sinusoïde par canal, à une fréquence et une phase différentes par canal —
//! sans quoi deux canaux seraient identiques octet pour octet et un échange de
//! canaux ne se verrait pas. Le générateur est versionné à côté des fixtures :
//!
//! ```text
//! python3 tune-core/tests/fixtures/dsd/generer_fixtures_dsd.py \
//!         tune-core/tests/fixtures/dsd
//! ```
//!
//! Les conteneurs sont écrits d'après les spécifications publiées — « DSF File
//! Format Specification » v1.01 (Sony, 2005) et « DSD Interchange File Format »
//! v1.5 (Philips, 2004). Cette conformité n'est pas une affirmation, c'est une
//! mesure, et elle vient d'un tiers :
//!
//! ```text
//! # le .dsf : aller-retour par WavPack, bit pour bit
//! wavpack  -y -q -h ref_dsd64_stereo.dsf -o t.wv
//! wvunpack -y -q --dsf t.wv -o retour.dsf
//! md5sum ref_dsd64_stereo.dsf retour.dsf   # 86b25d13ac16d61a1b66652674c49aef ×2
//!
//! # les deux .dff : IDENTIQUES a ce que wvunpack ECRIT lui-meme
//! wavpack  -y -q -h ref_dsd64_stereo.dff -o t.wv
//! wvunpack -y -q --dsdiff t.wv -o ecrit_par_wavpack.dff
//! cmp ref_dsd64_stereo.dff ecrit_par_wavpack.dff   # identiques
//! ```
//!
//! Mesuré le 12/09/2026 : `ref_dsd64_stereo.dff`
//! (`b7cfe981e420dcea8fc59514ade430a2`) et `ref_dsd64_5v1.dff`
//! (`471aef205ecc789a0197f73ade520331`) sont **octet pour octet** ce qu'écrit
//! `wvunpack --dsdiff`. Ces fixtures ne sont donc pas « un DSDIFF écrit par
//! nous » : ce sont des DSDIFF de WavPack.
//!
//! `ref_dsd64_stereo.dsf` porte **9 000 octets par canal**, DÉLIBÉRÉMENT hors
//! multiple de 4 096 : le dernier super-bloc DSF est donc complété de zéros
//! dans le fichier, et l'extraction doit les retrancher. Un remplissage qui
//! passerait serait du DSD constant à zéro — un continu pleine échelle, pas du
//! silence.

use md5::{Digest, Md5};

/// L'empreinte d'un train DSD : MD5 des octets, tels quels.
fn empreinte(octets: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(octets);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Miroir des bits d'un octet — DSF range le DSD LSB d'abord, DSDIFF et
/// `wvunpack --raw` le rangent MSB d'abord.
///
/// Copie volontaire du `reverse_bits` privé de `crate::audio::dsd_to_dop` :
/// voir l'en-tête de module.
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

/// (fichier, canaux, cadence DSD, octets par canal, empreinte du train canonique)
///
/// Les empreintes sont celles de `wvunpack --raw` (WavPack 5.8.1) — le
/// décodeur de RÉFÉRENCE — et non celles des modules `dsf`/`dff`.
const FIXTURES: &[(&str, u32, u32, usize, &str)] = &[
    // DSD64 stéréo en conteneur Sony DSF : le cas le plus courant du parc.
    // 9 000 octets par canal, donc trois super-blocs dont le dernier est
    // complété de zéros — le remplissage doit disparaître à l'extraction.
    (
        "ref_dsd64_stereo.dsf",
        2,
        2_822_400,
        9_000,
        "3d777969a6530203851432534c95954b",
    ),
    // Le MÊME contenu en conteneur Philips DSDIFF. L'empreinte est identique à
    // celle du `.dsf` ci-dessus : c'est WavPack qui l'établit, pas nous, et
    // c'est la preuve que les deux chemins de lecture de Tune convergent sur
    // le même train.
    (
        "ref_dsd64_stereo.dff",
        2,
        2_822_400,
        9_000,
        "3d777969a6530203851432534c95954b",
    ),
    // DSDIFF MULTICANAL 5.1. `un_dff_multicanal_ne_perd_plus_un_octet_par_bloc`
    // (dans `src/audio/dop_porteur_bout_en_bout.rs`) garde déjà ce chemin —
    // contre une source SYNTHÉTIQUE. Celle-ci est un fichier que WavPack lit
    // aussi, donc la garde porte désormais sur la bonne chose.
    (
        "ref_dsd64_5v1.dff",
        6,
        2_822_400,
        3_000,
        "415c685f552561781de8470ae9f67fe0",
    ),
];

/// Extrait le train DSD d'une fixture et le rend sous la forme CANONIQUE
/// (entrelacé par octet, MSB d'abord) — celle de `wvunpack --raw`.
fn train_canonique(chemin: &str) -> Result<(u32, u32, Vec<u8>), String> {
    if chemin.ends_with(".dsf") {
        let info = tune_core::audio::dsf::parse_dsf(chemin)?;
        let brut = tune_core::audio::dsf::read_dsf_blocks(chemin, &info)?;
        // DSF stocke LSB d'abord : miroir pour retomber sur la convention de
        // `wvunpack --raw`.
        let canonique = brut.iter().copied().map(miroir_des_bits).collect();
        Ok((info.channels, info.sample_rate, canonique))
    } else {
        let info = tune_core::audio::dff::parse_dff(chemin)?;
        let brut = tune_core::audio::dff::read_dff_data(chemin, &info)?;
        // DSDIFF stocke MSB d'abord : rien à retourner.
        Ok((info.channels, info.sample_rate, brut))
    }
}

/// La garde : chaque fixture doit sortir des modules `dsf`/`dff` octet pour
/// octet comme de `wvunpack --raw`.
#[test]
fn le_train_dsd_est_celui_du_decodeur_de_reference() {
    for (nom, canaux, cadence, octets_par_canal, md5) in FIXTURES {
        let chemin = chemin_fixture(nom);
        assert!(
            std::path::Path::new(&chemin).exists(),
            "fixture absente : {chemin}"
        );

        let (c, r, train) = train_canonique(&chemin)
            .unwrap_or_else(|err| panic!("{nom} : lecture refusée : {err}"));

        assert_eq!(c, *canaux, "{nom} : canaux");
        assert_eq!(r, *cadence, "{nom} : cadence DSD");
        assert_eq!(
            train.len(),
            octets_par_canal * *canaux as usize,
            "{nom} : longueur du train DSD extrait — un remplissage de fin de \
             super-bloc conservé, ou un octet perdu par bloc, se voit ici"
        );
        assert_eq!(
            empreinte(&train),
            *md5,
            "{nom} : le train DSD extrait ne correspond pas à celui de \
             `wvunpack --raw` (WavPack 5.8.1) — la lecture DSD n'est plus \
             sans perte, et un train DSD faux part en bruit blanc vers le DAC \
             (#1894, #2369)"
        );
    }
}

/// Le `.dsf` et le `.dff` portent le MÊME contenu : les deux chemins de lecture
/// doivent converger.
///
/// Ce n'est pas une tautologie : les deux modules n'ont pas une ligne en
/// commun. `dsf.rs` désentrelace des blocs de 4 096 octets par canal et
/// retranche un remplissage ; `dff.rs` lit une tranche contiguë. Qu'ils
/// tombent sur le même train est un fait mesuré, et c'est WavPack qui a établi
/// d'abord que les deux fichiers portent bien le même contenu.
#[test]
fn les_deux_conteneurs_rendent_le_meme_train() {
    let (_, _, du_dsf) = train_canonique(&chemin_fixture("ref_dsd64_stereo.dsf"))
        .expect("lecture du .dsf de référence");
    let (_, _, du_dff) = train_canonique(&chemin_fixture("ref_dsd64_stereo.dff"))
        .expect("lecture du .dff de référence");
    assert_eq!(
        empreinte(&du_dsf),
        empreinte(&du_dff),
        "le .dsf et le .dff de même contenu ne rendent plus le même train DSD — \
         l'un des deux analyseurs de conteneur a dérivé"
    );
}

/// Écrit dans `dossier` une copie de la fixture dont UN octet de la zone audio
/// est inversé.
///
/// Le milieu du fichier : ni l'en-tête, ni le chunk de format — ceux-là sont en
/// tête, et les abîmer éprouverait l'analyse de conteneur, pas la lecture du
/// train.
fn copie_avec_un_octet_abime(dossier: &std::path::Path, nom: &str) -> (String, usize) {
    let mut octets = std::fs::read(chemin_fixture(nom)).expect("lire la fixture");
    let offset = octets.len() / 2;
    assert!(
        offset > 128,
        "{nom} : fixture trop courte pour viser la zone audio"
    );
    octets[offset] ^= 0xFF;

    let copie = dossier.join(nom);
    std::fs::write(&copie, &octets).expect("écrire la copie abîmée");
    (copie.to_str().expect("chemin utf-8").to_owned(), offset)
}

/// Contre-épreuve : la garde ci-dessus doit ROUGIR dès qu'un octet bouge.
///
/// Sans elle, une table d'empreintes ne prouve rien : un témoin qui ne sait pas
/// échouer est un témoin qui ne garde pas.
///
/// ⚠️ Ce qu'on exige ici est la DIFFÉRENCE à **longueur constante**. Un octet
/// inversé au milieu d'un train DSD ne change ni le nombre de canaux, ni la
/// taille du chunk, ni la durée annoncée : seule l'EMPREINTE bouge. C'est
/// exactement la propriété demandée — une garde qui ne mordrait que sur la
/// longueur resterait verte face à un décodeur qui rendrait du bruit à la
/// bonne cadence, sur le bon nombre de canaux, pendant la bonne durée. C'est
/// mot pour mot ce qui est arrivé à WavPack (#3849).
#[test]
fn un_octet_abime_fait_rougir_la_garde() {
    let dossier = tune_core::test_scratch::scratch_dir("dsd-contre-epreuve");

    for (nom, canaux, _, octets_par_canal, md5) in FIXTURES {
        let (copie, offset) = copie_avec_un_octet_abime(&dossier, nom);
        let Ok((_, _, train)) = train_canonique(&copie) else {
            // Un refus franc est la meilleure des issues : rien n'atteint le DAC.
            continue;
        };
        assert_eq!(
            train.len(),
            octets_par_canal * *canaux as usize,
            "{nom} : un octet inversé a changé la LONGUEUR du train — la \
             contre-épreuve doit porter sur l'empreinte seule"
        );
        assert_ne!(
            empreinte(&train),
            *md5,
            "{nom} : un octet abîmé à l'offset {offset} a rendu EXACTEMENT le \
             train DSD de référence — la garde ne rougirait donc pas sur une \
             régression de la lecture DSD"
        );
    }
}

/// Ce que T3 avait MESURÉ, et que T4 a corrigé.
///
/// `parse_dff` ne respectait pas le remplissage IFF à l'octet pair sur les
/// sous-chunks qu'il CONNAÎT. Les trois branches `FS  `, `CHNL` et `CMPR`
/// avançaient de `sub_size - n` octets ; seule la branche des sous-chunks
/// inconnus arrondissait, avec `(sub_size + 1) & !1`. Un sous-chunk de taille
/// impaire — que l'IFF autorise explicitement, à charge pour le lecteur de
/// sauter l'octet de remplissage — désalignait donc l'analyseur d'un octet, le
/// `sub_size` suivant était lu sur des octets qui n'en sont pas, et le `seek`
/// partait avec une valeur absurde :
///
/// ```text
/// parse_dff → Err("dff skip sub-chunk: Invalid argument (os error 22)")
/// ```
///
/// T3 avait retenu deux choses de ce refus : il était SÛR — rien n'atteignait
/// le DAC — mais son message était illisible pour qui le recevait. « Invalid
/// argument (os error 22) » sur un `.dff` ne dit ni ce qui manque, ni que le
/// fichier est en cause.
///
/// T4 arrondit les trois branches connues (`dff.rs`,
/// `taille_avec_remplissage`), et ce témoin est retourné comme T3 le
/// demandait : il exige désormais la LECTURE. La fixture reste hors de la
/// table `FIXTURES` — elle ne porte que 258 octets de charge utile, trop peu
/// pour une empreinte de train DSD, et son objet est l'analyseur, pas le
/// décodeur.
#[test]
fn un_chunk_cmpr_de_taille_impaire_se_lit_desormais() {
    let chemin = chemin_fixture("ref_dsd64_stereo_cmpr_impair.dff");
    assert!(
        std::path::Path::new(&chemin).exists(),
        "fixture absente : {chemin}"
    );

    let info = tune_core::audio::dff::parse_dff(&chemin).unwrap_or_else(|e| {
        panic!(
            "`parse_dff` refuse un DSDIFF dont le sous-chunk CMPR est de taille \
             impaire (19 o. au lieu de 20) : « {e} ». Le remplissage IFF n'est \
             plus appliqué aux sous-chunks connus — c'est la régression du \
             correctif T4 (#2218)."
        )
    });
    assert_eq!(info.channels, 2, "canaux relus après le remplissage");
    assert_eq!(info.sample_rate, 2_822_400, "cadence DSD relue");
    assert_eq!(
        info.compression.trim_end(),
        "DSD",
        "la compression relue n'est plus du DSD brut — l'analyseur est désaligné \
         d'un octet"
    );
}
