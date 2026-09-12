//! ALAC, AIFF, WAV : le PCM rendu par `decode_to_pcm` doit être celui du
//! décodeur de référence de chaque format.
//!
//! # Tranche T2 du banc de conformité (#2218)
//!
//! Même table, même `empreinte_i32`, même mode d'emploi que la tranche T1
//! (`tests/flac_empreintes_reference.rs`) : un tuple par fixture, et une garde
//! qui le compare au décodeur de référence du format — jamais à ce module.
//! T1 posait FLAC ; T2 ajoute les trois conteneurs **sans perte** restants.
//!
//! # Le défaut, remesuré sur `batch/bugs-11` (ef149b11) le 12/09/2026
//!
//! `AudioFormat::from_extension` (`src/audio/formats.rs`) reconnaît onze
//! formats. Avant cette tranche, **trois** portaient une empreinte contre une
//! référence externe : APE (`tests/ape_fixture_i2505.rs`), WavPack
//! (`src/audio/wavpack.rs`, empreintes `wvunpack 5.6.0`) et FLAC depuis T1.
//!
//! Les trois fixtures sans perte restantes existaient, servaient partout, et
//! n'étaient décodées **sous aucune assertion de valeur** :
//!
//! - `tests/fixtures/test.m4a` : lu en **octets bruts** par
//!   `src/audio/http_range.rs:325` et `:334` (découpe des requêtes HTTP par
//!   plage). Le seul site qui le décode est `decode.rs::decode_m4a`, et ses
//!   deux assertions sont `!samples.is_empty()` et `sample_rate == 44100`.
//! - `tests/fixtures/test.aiff` : `decode.rs::decode_aiff_native` et les cinq
//!   témoins d'`src/audio/aiff.rs` n'assertent que cadence, canaux, durée et
//!   « pas vide ». Et `tests/audio_integration.rs:58` **fabrique son propre
//!   AIFF** (`create_test_aiff`) au lieu d'utiliser la fixture.
//! - `tests/fixtures/test.wav` : idem — `decode.rs::decode_wav` s'arrête à
//!   cadence, canaux et durée ; `audio_integration.rs:16` fabrique le sien
//!   (`create_test_wav`).
//!
//! Pas une de ces assertions ne porte sur la VALEUR des échantillons. Un
//! décodeur qui rendrait du bruit à la bonne cadence, sur le bon nombre de
//! canaux, pendant la bonne durée, passerait toutes ces portes sans une
//! rougeur. **Un test qui se nourrit lui-même ne garde rien** : c'est ce qui a
//! laissé le décodeur WavPack rendre du bruit blanc pendant trois mois (#3849),
//! vert contre un en-tête de 32 octets écrit à la main.
//!
//! # D'où viennent les empreintes, et comment les régénérer
//!
//! ffmpeg est banni de toute chaîne de lecture de Tune, **y compris comme
//! décodeur de référence dans un test** (`tests/no_blind_ffmpeg.rs`). Aucune
//! commande ci-dessous ne l'appelle, ni en premier choix ni en repli.
//!
//! L'outil commun qui transforme du PCM brut en empreinte est le même qu'en
//! T1 — il élargit chaque échantillon de sa largeur native (2 ou 3 octets,
//! petit-boutiste signé) vers un `i32` petit-boutiste, la représentation que
//! [`empreinte_i32`] hache et que `DecodedAudio::samples_i32` porte :
//!
//! ```text
//! empreinte() {            # $1 = PCM brut petit-boutiste signé, $2 = bits
//!   BPS=$2 perl -0777 -ne 'my $w=$ENV{BPS}/8; my @b=unpack("C*",$_); my @s;
//!     for (my $i=0; $i+$w<=@b; $i+=$w) { my $v=0; $v |= $b[$i+$_]<<(8*$_) for 0..$w-1;
//!     $v -= 1<<(8*$w) if $v >= 1<<(8*$w-1); push @s,$v } print pack("l<*",@s)' "$1" \
//!     | md5sum         # `md5 -q` sous macOS
//! }
//! ```
//!
//! Le nombre d'échantillons se lit sur le même fichier brut :
//! `taille / (bits / 8)`.
//!
//! ## WAV et AIFF — libFLAC 1.5.0, `flac(1)`
//!
//! `flac` lit nativement le WAV et l'AIFF, et son aller-retour est sans perte :
//! le PCM qui ressort est la lecture que libFLAC fait du conteneur, sans une
//! ligne de Tune dans la chaîne. Pour `wav/ref_16_44100_stereo.wav` (bits=16),
//! `wav/ref_24_96000_stereo.wav` (24), `aiff/ref_16_44100_stereo.aiff` (16) et
//! `aiff/ref_24_88200_stereo.aiff` (24) :
//!
//! ```text
//! f=tune-core/tests/fixtures/wav/ref_16_44100_stereo.wav ; bits=16
//! flac -s -f --no-padding -o ref.flac "$f"
//! flac -s -d --force-raw-format --endian=little --sign=signed -f -o ref.raw ref.flac
//! empreinte ref.raw $bits
//! ```
//!
//! Contre-vérifié, valeur par valeur, par un second lecteur indépendant :
//! `afconvert` (Apple Core Audio, « Audio File Convert Version 2.0 »,
//! macOS 26.6.2 build 25G83), `afconvert -f WAVE -d LEI$bits "$f" x.wav`. Les
//! quatre empreintes sont identiques entre les deux outils.
//!
//! ## ALAC — le décodeur de RÉFÉRENCE d'Apple
//!
//! `alacconvert`, bâti tel quel depuis `macosforge/alac`, commit
//! `c38887c5c5e64a4b31108733bd79ca9b2496d987` (2016-05-11), le dépôt de
//! référence d'Apple dont ce dépôt-ci vendorise déjà la moitié encodeur sous
//! `tune-core/vendor/alac`. Il ne lit que le conteneur CAF ; les paquets ALAC
//! de la fixture lui sont donc passés **tels quels**, sans réencodage, par
//! `tests/fixtures/alac/m4a_vers_caf.py` (remultiplexage pur : il recopie les
//! octets ALAC du `.m4a` dans un `.caf`, et ne touche à aucun échantillon).
//!
//! ```text
//! (cd alac/codec && make) && (cd alac/convert-utility && make)   # -> alacconvert
//! f=tune-core/tests/fixtures/alac/ref_16_44100_stereo.m4a ; bits=16
//! python3 tune-core/tests/fixtures/alac/m4a_vers_caf.py "$f" ref.caf
//! alacconvert ref.caf ref.wav
//! # ref.wav est un WAV PCM natif : son bloc `data` est déjà du petit-boutiste signé
//! python3 -c 'import wave; w=wave.open("ref.wav","rb");
//!   open("ref.raw","wb").write(w.readframes(w.getnframes()))'
//! empreinte ref.raw $bits
//! ```
//!
//! Contre-vérifié par `afconvert -f WAVE -d LEI$bits "$f" x.wav`, c'est-à-dire
//! par le décodeur ALAC de Core Audio appliqué **directement** au `.m4a` : les
//! deux empreintes sont identiques, sur les deux fixtures.
//!
//! ⚠️ Le raccourci `afconvert -f caff -d alac "$f" ref.caf` ne convient PAS :
//! Core Audio réencode alors en ALAC « 32-bit source » (drapeau 0x4), et
//! `alacconvert` rend un WAV 32 bits dont l'empreinte est décalée de 16 rangs.
//! Mesuré le 12/09/2026 : `fe0c2789ca2b7552043afb604f7bcb65` au lieu de
//! `d84d71bbf68e93eaa8f91170de7f1ba3`. Le PCM reste sans perte, mais la largeur
//! de conteneur change — c'est exactement le genre d'écart qu'une table
//! d'empreintes doit attraper, et la raison du remultiplexage pur ci-dessus.
//!
//! # D'où viennent les fixtures
//!
//! Contenu **100 % synthétique**, bâti sur le même dessin qu'en T1 : aucune
//! œuvre, aucun droit à demander. Quatre segments égaux :
//!
//! 1. silence numérique ;
//! 2. deux sinus voisins (997 Hz à gauche / 1 003 Hz à droite, 0,60 pleine
//!    échelle) — canal côté non nul, donc décorrélation stéréo réellement
//!    exercée par l'encodeur ALAC ;
//! 3. bruit blanc (générateur congruentiel déterministe, 1/3 pleine échelle) —
//!    le pire cas de l'entropie, celui qui pousse ALAC en mode brut ;
//! 4. sinus à 440 Hz dont toutes les valeurs sont multiples de 256 — **8 bits
//!    perdus**, les deux canaux identiques, donc canal côté nul partout.
//!
//! Conteneurs écrits par `afconvert` (Apple Core Audio, Version 2.0,
//! macOS 26.6.2), l'outil officiel d'Apple pour AIFF et ALAC :
//!
//! ```text
//! afconvert -f WAVE -d LEI16@44100 src_16_44100_stereo.wav wav/ref_16_44100_stereo.wav
//! afconvert -f WAVE -d LEI24@96000 src_24_96000_stereo.wav wav/ref_24_96000_stereo.wav
//! afconvert -f AIFF -d BEI16@44100 src_16_44100_stereo.wav aiff/ref_16_44100_stereo.aiff
//! afconvert -f AIFF -d BEI24@88200 src_24_88200_stereo.wav aiff/ref_24_88200_stereo.aiff
//! afconvert -f m4af -d alac@44100  src_16_44100_stereo.wav alac/ref_16_44100_stereo.m4a
//! afconvert -f m4af -d alac@96000  src_24_96000_stereo.wav alac/ref_24_96000_stereo.m4a
//! ```
//!
//! Ces fixtures portent un détail qu'aucun fichier fabriqué à la main n'aurait :
//! `afconvert` insère un bloc de bourrage **`FLLR`** avant `data` / `SSND`.
//! C'est un bloc inconnu de Tune, posé au milieu de l'en-tête, tel qu'en
//! produisent les vrais outils Apple. Un analyseur qui supposerait `data`
//! immédiatement après `fmt ` ne lirait ici que du bourrage.
//!
//! # Trois conteneurs, un seul PCM — et c'est une garde de plus
//!
//! Les six fixtures naissent de deux sources seulement. `ref_16_44100_stereo`
//! en `.wav`, en `.aiff` et en `.m4a` porte donc **la même empreinte**,
//! `d84d71bb…`, et `ref_24_96000_stereo` en `.wav` et en `.m4a` porte
//! `f3d45a3d…`. Ce n'est pas une redondance : c'est une garde transversale.
//! L'AIFF est **gros-boutiste** et le WAV petit-boutiste ; un décodeur qui se
//! tromperait d'ordre d'octets rendrait un PCM valide, à la bonne cadence et
//! au bon nombre d'échantillons — et l'empreinte partagée le dit sur-le-champ.
//! De même entre le WAV 24 bits et l'ALAC 24 bits : une erreur de largeur de
//! conteneur dans un seul des deux chemins casse l'égalité.

use md5::{Digest, Md5};

/// L'empreinte d'un train d'échantillons : MD5 des `i32` petit-boutistes.
///
/// Identique à celle de T1 et du garde WavPack, pour que les trois tables se
/// lisent de la même façon.
fn empreinte_i32(samples: &[i32]) -> String {
    let mut h = Md5::new();
    for s in samples {
        h.update(s.to_le_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn chemin_fixture(nom: &str) -> String {
    format!("{}/tests/fixtures/{nom}", env!("CARGO_MANIFEST_DIR"))
}

/// (fichier, canaux, cadence, profondeur, nb d'échantillons, empreinte md5)
///
/// Les empreintes viennent du décodeur de RÉFÉRENCE de chaque format —
/// `alacconvert` (macosforge/alac `c38887c5`) pour ALAC, `flac` (libFLAC
/// 1.5.0) pour AIFF et WAV, chacune contre-vérifiée par `afconvert` (Apple
/// Core Audio 2.0) — et non de ce module.
const FIXTURES: &[(&str, u32, u32, u16, usize, &str)] = &[
    // ALAC 16 bits / 44,1 kHz : le rip CD Apple, le cas le plus courant du
    // parc iTunes. `test.m4a` n'est lu qu'en octets bruts par http_range.rs ;
    // rien ne gardait jusqu'ici la VALEUR de ce que le décodeur ALAC rend.
    (
        "alac/ref_16_44100_stereo.m4a",
        2,
        44100,
        16,
        35_280,
        "d84d71bbf68e93eaa8f91170de7f1ba3",
    ),
    // ALAC 24 bits / 96 kHz : `alac` avec le drapeau de format 0x3
    // (« 24-bit source »). C'est le seul chemin où la profondeur ne se lit
    // NI dans le conteneur NI dans l'en-tête de paquet, mais dans le magic
    // cookie (`decode.rs::resolve_bit_depth_from_alac_magic_cookie`) — une
    // erreur de largeur s'y voit, et nulle part ailleurs.
    (
        "alac/ref_24_96000_stereo.m4a",
        2,
        96000,
        24,
        19_200,
        "f3d45a3d47f4692f2ff95ef8cfd560c6",
    ),
    // AIFF 16 bits / 44,1 kHz : le format d'Apple, GROS-BOUTISTE. Même
    // empreinte que le WAV 16 bits : l'inversion d'ordre d'octets est donc
    // détectée, et c'est la faute la plus probable d'un analyseur AIFF.
    (
        "aiff/ref_16_44100_stereo.aiff",
        2,
        44100,
        16,
        35_280,
        "d84d71bbf68e93eaa8f91170de7f1ba3",
    ),
    // AIFF 24 bits / 88,2 kHz. Ajouté au-delà du minimum demandé, et pour une
    // raison précise : c'est le SEUL cas du banc où trois octets gros-boutistes
    // doivent être assemblés en un entier signé. Le 16 bits gros-boutiste se
    // rattrape par un `swap_bytes` ; le 24 bits, non — il faut poser le signe à
    // la main. Et 88,2 kHz est la cadence des masters DXD décimés, celle que
    // les testeurs apportent le plus souvent en AIFF.
    (
        "aiff/ref_24_88200_stereo.aiff",
        2,
        88200,
        24,
        17_640,
        "ea88d70a6609243468d4c9ff47f14587",
    ),
    // WAV 16 bits / 44,1 kHz : `fmt ` hérité (format 1), bloc `FLLR` de
    // bourrage avant `data`.
    (
        "wav/ref_16_44100_stereo.wav",
        2,
        44100,
        16,
        35_280,
        "d84d71bbf68e93eaa8f91170de7f1ba3",
    ),
    // WAV 24 bits / 96 kHz. `afconvert` l'écrit en `fmt ` de type 1 avec
    // `bits-per-sample=24` — ce que libFLAC signale comme « legacy WAVE file »
    // et que beaucoup d'analyseurs refusent. C'est pourtant ce qu'écrivent les
    // enregistreurs du parc, donc ce qu'il faut savoir lire.
    (
        "wav/ref_24_96000_stereo.wav",
        2,
        96000,
        24,
        19_200,
        "f3d45a3d47f4692f2ff95ef8cfd560c6",
    ),
];

/// Écrit dans `dossier` une copie de la fixture dont UN octet, pris au milieu
/// du fichier, est inversé.
///
/// Le milieu : pour les six fixtures, c'est au cœur des données audio — les
/// en-têtes (`RIFF`/`fmt `/`FLLR`, `FORM`/`COMM`, `ftyp`/`moov`) tiennent en
/// quelques centaines d'octets en tête, et le plus petit de ces fichiers pèse
/// 31 ko. Abîmer l'en-tête éprouverait l'analyse de conteneur ; ici on éprouve
/// le DÉCODAGE DU SIGNAL.
///
/// La longueur du fichier ne bouge pas, et pour les quatre fixtures PCM
/// (WAV, AIFF) le nombre d'échantillons ne bouge pas non plus : ce qui rougit
/// est alors l'EMPREINTE SEULE, ce qui est la propriété qu'on veut prouver.
fn copie_avec_un_octet_abime(dossier: &std::path::Path, nom: &str) -> (String, usize) {
    let mut octets = std::fs::read(chemin_fixture(nom)).expect("lire la fixture");
    let offset = octets.len() / 2;
    assert!(
        offset > 512,
        "{nom} : fixture trop courte pour viser les données audio"
    );
    octets[offset] ^= 0xFF;

    let copie = dossier.join(nom.rsplit('/').next().expect("nom de fichier"));
    std::fs::write(&copie, &octets).expect("écrire la copie abîmée");
    (copie.to_str().expect("chemin utf-8").to_owned(), offset)
}

/// Décode la fixture et rend ce qui est comparable à la table.
fn mesure(chemin: &str) -> Result<(u32, u32, u16, usize, String), String> {
    let audio = tune_core::audio::decode::decode_to_pcm(chemin, None, None, 0.0, 0.0)?;
    Ok((
        audio.channels,
        audio.sample_rate,
        audio.bit_depth,
        audio.samples_i32.len(),
        empreinte_i32(&audio.samples_i32),
    ))
}

/// La garde : chaque fixture doit sortir de `decode_to_pcm` bit pour bit comme
/// du décodeur de référence de son format.
#[test]
fn le_pcm_alac_aiff_wav_est_celui_des_decodeurs_de_reference() {
    for (nom, canaux, cadence, profondeur, echantillons, md5) in FIXTURES {
        let chemin = chemin_fixture(nom);
        assert!(
            std::path::Path::new(&chemin).exists(),
            "fixture absente : {chemin}"
        );

        let (c, r, b, n, e) =
            mesure(&chemin).unwrap_or_else(|err| panic!("{nom} : décodage refusé : {err}"));

        assert_eq!(c, *canaux, "{nom} : canaux");
        assert_eq!(r, *cadence, "{nom} : cadence");
        assert_eq!(b, *profondeur, "{nom} : profondeur");
        assert_eq!(n, *echantillons, "{nom} : nombre d'échantillons");
        assert_eq!(
            e, *md5,
            "{nom} : le PCM décodé ne correspond pas à celui du décodeur de \
             référence du format (alacconvert macosforge/alac c38887c5 pour \
             ALAC, flac 1.5.0 pour AIFF et WAV) — le décodage n'est plus sans \
             perte"
        );
    }
}

/// Contre-épreuve : la garde ci-dessus doit ROUGIR dès qu'un octet bouge.
///
/// Sans elle, une table d'empreintes ne prouve rien : un témoin qui ne sait pas
/// échouer est un témoin qui ne garde pas. On décode une copie jetable, dans un
/// dossier temporaire nettoyé par `Drop`, dont un octet a été inversé au milieu
/// des données audio.
///
/// Ce qu'on exige est la DIFFÉRENCE, pas le refus : `decode_symphonia` avale
/// l'erreur de paquet (voir le témoin `decodage_partiel_*` plus bas). Pour les
/// quatre fixtures PCM, la différence porte sur l'empreinte seule — le nombre
/// d'échantillons est rigoureusement identique, puisqu'aucun octet n'a été
/// ajouté ni retiré.
#[test]
fn un_octet_abime_fait_rougir_la_garde() {
    let dossier = tune_core::test_scratch::scratch_dir("t2-contre-epreuve");

    for (nom, _, _, _, echantillons, md5) in FIXTURES {
        let (copie, offset) = copie_avec_un_octet_abime(dossier.path(), nom);
        let Ok((_, _, _, n, e)) = mesure(&copie) else {
            // Un refus franc est la meilleure des issues : rien n'atteint le DAC.
            continue;
        };
        assert!(
            n != *echantillons || e != *md5,
            "{nom} : un octet abîmé à l'offset {offset} a rendu EXACTEMENT le PCM \
             de référence ({n} échantillons, {e}) — la garde ne rougirait donc pas \
             sur une régression du décodeur, et un flux désynchronisé repartirait \
             en bruit sans une ligne de journal (#3849)"
        );
    }
}

/// La contre-épreuve la plus étroite : UN SEUL échantillon changé.
///
/// Un octet inversé peut déplacer plusieurs échantillons à la fois, ou tronquer
/// le flux. Ici on change **un échantillon et un seul**, de la plus petite
/// quantité possible (± 1 LSB), au milieu des données audio d'une fixture PCM.
/// La longueur du fichier, le nombre d'échantillons, la cadence, les canaux et
/// la profondeur restent tous identiques : **seule l'empreinte bouge**. C'est la
/// seule façon de prouver que c'est bien l'EMPREINTE qui mord, et non la taille.
///
/// Ne s'applique qu'aux conteneurs PCM (WAV, AIFF), où l'on sait où poser le
/// doigt sans réencoder. Pour ALAC, changer un échantillon demanderait de
/// réencoder le paquet — ce que le témoin précédent couvre autrement.
#[test]
fn un_seul_echantillon_change_fait_rougir_la_garde() {
    let dossier = tune_core::test_scratch::scratch_dir("t2-un-echantillon");
    let mut eprouvees = 0usize;

    for (nom, _, _, profondeur, echantillons, md5) in FIXTURES {
        if !(nom.ends_with(".wav") || nom.ends_with(".aiff")) {
            continue;
        }
        let mut octets = std::fs::read(chemin_fixture(nom)).expect("lire la fixture");
        let largeur = (*profondeur / 8) as usize;
        // Un octet de poids FORT de l'échantillon : petit-boutiste en WAV
        // (dernier octet du groupe), gros-boutiste en AIFF (premier).
        let groupe = (octets.len() / 2 / largeur) * largeur;
        let cible = if nom.ends_with(".wav") {
            groupe + largeur - 1
        } else {
            groupe
        };
        // ± 1 LSB sur cet octet : le plus petit écart mesurable.
        octets[cible] = octets[cible].wrapping_add(1);

        let copie = dossier
            .path()
            .join(format!("un-{}", nom.rsplit('/').next().expect("nom")));
        std::fs::write(&copie, &octets).expect("écrire la copie");

        let (_, _, _, n, e) = mesure(copie.to_str().expect("chemin utf-8"))
            .unwrap_or_else(|err| panic!("{nom} : décodage refusé : {err}"));

        assert_eq!(
            n, *echantillons,
            "{nom} : changer un seul échantillon ne doit PAS changer leur nombre — \
             sinon la contre-épreuve prouverait la taille, pas l'empreinte"
        );
        assert_ne!(
            e, *md5,
            "{nom} : un échantillon changé de 1 LSB à l'octet {cible} rend la MÊME \
             empreinte que la référence — la table ne garde donc pas la valeur des \
             échantillons, seulement leur nombre"
        );
        eprouvees += 1;
    }

    assert_eq!(
        eprouvees, 4,
        "les quatre fixtures PCM (2 WAV, 2 AIFF) doivent toutes passer par cette \
         contre-épreuve ; {eprouvees} l'ont fait"
    );
}

/// Ce que cette tranche a mesuré et ne corrige PAS.
///
/// Prolonge le constat de T1 (`decodage_partiel_une_trame_abimee_ne_remonte_aucune_erreur`)
/// sur les conteneurs de T2 : `decode_symphonia` avale l'erreur de paquet
/// (`Err(_) => continue` sur `decoder.decode`, `Err(_) => break` sur
/// `format.next_packet`). L'appelant reçoit `Ok(_)` et un PCM éventuellement
/// plus court, sans une ligne de journal.
///
/// Ce témoin ne fait que **chiffrer** ce qui se passe, format par format, pour
/// qu'une tranche ultérieure ait un point de départ. Il n'exige rien d'autre que
/// la différence déjà exigée plus haut.
#[test]
fn decodage_partiel_un_octet_abime_ne_remonte_aucune_erreur() {
    let dossier = tune_core::test_scratch::scratch_dir("t2-troncature");

    for (nom, _, _, _, echantillons, _) in FIXTURES {
        let (copie, offset) = copie_avec_un_octet_abime(dossier.path(), nom);
        match mesure(&copie) {
            Ok((_, _, _, n, _)) if n < *echantillons => eprintln!(
                "T2 #2218 — {nom} : octet {offset} abîmé, {n} échantillons rendus sur \
                 {echantillons} attendus ({:.1} % de la piste perdus), Ok(_) en retour \
                 et pas une erreur remontée.",
                100.0 * (1.0 - n as f64 / *echantillons as f64)
            ),
            Ok((_, _, _, n, _)) => eprintln!(
                "T2 #2218 — {nom} : octet {offset} abîmé, {n} échantillons rendus \
                 (autant qu'attendu) — la faute ne se voit QUE sur l'empreinte."
            ),
            Err(e) => eprintln!(
                "T2 #2218 — {nom} : le décodeur refuse un octet abîmé : {e}. \
                 C'est la bonne issue ; retourner ce témoin en exigeant le refus."
            ),
        }
    }
}
