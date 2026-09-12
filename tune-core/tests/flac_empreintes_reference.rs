//! FLAC : le PCM rendu par `decode_to_pcm` doit être celui de `flac -d`.
//!
//! # Tranche T1 du banc de conformité (#2218)
//!
//! C'est la table de référence sur laquelle les tranches suivantes s'adossent :
//! un tuple par fixture, et une garde qui le compare au décodeur de référence
//! du format. Le motif est celui du correctif WavPack (`4be4ba8d`, #3849) —
//! empreintes issues de l'outil officiel, jamais de ce module.
//!
//! # Pourquoi
//!
//! `tests/audio_integration.rs` porte 30 témoins verts qui **fabriquent
//! eux-mêmes** leurs fichiers dans un temporaire — six `create_test_*`, et pas
//! une seule référence à `tests/fixtures/`. C'est exactement le mécanisme qui a
//! laissé le décodeur WavPack rendre du bruit blanc pendant trois mois :
//! `parse_wavpack_header_only` et `decode_wavpack_truncated_graceful` étaient
//! verts contre un en-tête de 32 octets écrit à la main, pendant que 13 albums
//! de Marco Polo sortaient à -109 dB de SNR. **Un test qui se nourrit lui-même
//! ne garde rien.**
//!
//! État mesuré sur `batch/bugs-11` (a9a274be) avant cette tranche : sur les
//! onze branches d'extension d'`AudioFormat::from_extension`, **deux** formats
//! portaient une empreinte contre une référence externe — APE
//! (`tests/ape_fixture_i2505.rs`, un `.ape` appairé à son `.wav`) et WavPack
//! (`src/audio/wavpack.rs`, empreintes `wvunpack 5.6.0`). FLAC, le format le
//! plus lu du parc, n'en avait aucune.
//!
//! `tests/fixtures/test.flac` (20 641 o.) est cité 42 fois dans 21 fichiers —
//! scan, métadonnées, routes, chemin du signal. Un seul de ces sites le
//! décode : `decode.rs::decode_flac`, et ses quatre assertions sont
//! `!samples.is_empty()`, cadence, canaux et `duration_s > 0.9`. Pas une ne
//! porte sur la VALEUR des échantillons. Un décodeur qui rendrait du bruit à
//! la bonne cadence, sur le bon nombre de canaux, pendant la bonne durée,
//! passerait cette porte-là sans une rougeur — c'est mot pour mot ce qui est
//! arrivé à WavPack.
//!
//! # D'où viennent les empreintes, et comment les régénérer
//!
//! De `flac -d` (libFLAC 1.5.0, l'outil officiel du format), jamais de ce
//! module. ffmpeg est banni de toute chaîne de lecture de Tune, y compris comme
//! décodeur de référence dans un test (`tests/no_blind_ffmpeg.rs`).
//!
//! Empreinte d'une fixture — la commande exacte, telle quelle :
//!
//! ```text
//! f=tune-core/tests/fixtures/flac/ref_16_44100_stereo.flac ; bits=16
//! flac -s -d --force-raw-format --endian=little --sign=signed -f -o ref.raw "$f"
//! BPS=$bits perl -0777 -ne 'my $w=$ENV{BPS}/8; my @b=unpack("C*",$_); my @s;
//!   for (my $i=0; $i+$w<=@b; $i+=$w) { my $v=0; $v |= $b[$i+$_]<<(8*$_) for 0..$w-1;
//!   $v -= 1<<(8*$w) if $v >= 1<<(8*$w-1); push @s,$v } print pack("l<*",@s)' ref.raw \
//!   | md5sum          # `md5 -q` sous macOS
//! ```
//!
//! Le `perl` ne fait qu'une chose : élargir chaque échantillon de sa largeur
//! native (2 ou 3 octets, petit-boutiste signé) vers un `i32` petit-boutiste.
//! C'est la représentation que [`empreinte_i32`] hache, et celle que
//! `DecodedAudio::samples_i32` porte — `decode_symphonia` droitise sur la
//! largeur du conteneur avant de rendre.
//!
//! Le nombre d'échantillons se lit sur le même fichier brut :
//! `taille_de_ref.raw / (bits / 8)`.
//!
//! # D'où viennent les fixtures
//!
//! Contenu **100 % synthétique**, écrit pour cette tranche : aucune œuvre,
//! aucun droit à demander. Chaque piste est bâtie en quatre segments égaux, qui
//! visent les familles de sous-trames du format :
//!
//! 1. silence numérique → sous-trame `CONSTANT` ;
//! 2. deux sinus voisins (997 Hz / 1 003 Hz, 0,60 pleine échelle) → `LPC`, et
//!    un canal côté non nul, donc décorrélation stéréo réellement exercée ;
//! 3. bruit blanc (générateur congruentiel déterministe, 1/3 pleine échelle) →
//!    le pire cas de l'entropie : `VERBATIM` ou `LPC` d'ordre élevé ;
//! 4. sinus à 440 Hz dont toutes les valeurs sont multiples de 256 → **8 bits
//!    perdus**, et les deux canaux identiques, donc canal côté nul partout.
//!
//! Encodage (libFLAC 1.5.0), un réglage différent par fixture : un seul chemin
//! d'encodeur ne serait pas une couverture :
//!
//! ```text
//! flac -s -f -5       --no-padding -o ref_16_44100_stereo.flac src_16_44100_stereo.wav
//! flac -s -f -8 -e -p --no-padding -o ref_24_96000_stereo.flac src_24_96000_stereo.wav
//! flac -s -f -8       --no-padding -o ref_16_44100_mono.flac   src_16_44100_mono.wav
//! ```

use md5::{Digest, Md5};

/// L'empreinte d'un train d'échantillons : MD5 des `i32` petit-boutistes.
///
/// Même forme que celle du garde WavPack, pour que les deux tables se lisent
/// de la même façon.
fn empreinte_i32(samples: &[i32]) -> String {
    let mut h = Md5::new();
    for s in samples {
        h.update(s.to_le_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn chemin_fixture(nom: &str) -> String {
    format!("{}/tests/fixtures/flac/{nom}", env!("CARGO_MANIFEST_DIR"))
}

/// (fichier, canaux, cadence, profondeur, nb d'échantillons, empreinte md5)
///
/// Les empreintes sont celles de `flac -d` (libFLAC 1.5.0) — le décodeur de
/// RÉFÉRENCE — et non celles de ce module.
const FIXTURES: &[(&str, u32, u32, u16, usize, &str)] = &[
    // Le rip CD : 16 bits / 44,1 kHz stéréo, 0,4 s. Le cas le plus courant du
    // parc, et celui que `tests/fixtures/test.flac` n'a jamais gardé.
    (
        "ref_16_44100_stereo.flac",
        2,
        44100,
        16,
        35_280,
        "b702caf5d2257a84d3953c26526dd6dc",
    ),
    // Haute résolution 24 bits / 96 kHz, encodée `-8 -e -p` : recherche
    // exhaustive du modèle et prédicteur LPC exact, donc des ordres et des
    // précisions de coefficients que `-5` ne produit jamais. C'est aussi le
    // seul des trois dont la droitisation de `decode_symphonia` décale de
    // 8 rangs (`shift = 32 - 24`) : une erreur de largeur de conteneur s'y
    // voit, et nulle part ailleurs.
    (
        "ref_24_96000_stereo.flac",
        2,
        96000,
        24,
        19_200,
        "5647a1733e4ec46e7a1dd00e10feaf3c",
    ),
    // Mono : un seul canal, donc aucune décorrélation stéréo. Un décodeur qui
    // supposerait deux canaux rendrait ici la moitié de la durée, ou le
    // double — c'est la faute qu'aucun témoin stéréo ne peut voir.
    (
        "ref_16_44100_mono.flac",
        1,
        44100,
        16,
        8_820,
        "70c9300abe8f564171e38d869720fe1d",
    ),
];

/// Écrit dans `dossier` une copie de la fixture dont UN octet, pris au milieu
/// des trames audio, est inversé.
///
/// Le milieu du fichier : ni l'en-tête `fLaC`, ni `STREAMINFO`, ni la table de
/// recherche — ces trois-là sont en tête, et les abîmer éprouverait l'analyse
/// de conteneur, pas le décodage du signal.
fn copie_avec_un_octet_abime(dossier: &std::path::Path, nom: &str) -> (String, usize) {
    let mut octets = std::fs::read(chemin_fixture(nom)).expect("lire la fixture");
    let offset = octets.len() / 2;
    assert!(
        offset > 64,
        "{nom} : fixture trop courte pour viser les trames audio"
    );
    octets[offset] ^= 0xFF;

    let copie = dossier.join(nom);
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
/// de `flac -d`.
#[test]
fn le_pcm_flac_est_celui_du_decodeur_de_reference() {
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
            "{nom} : le PCM décodé ne correspond pas à celui de `flac -d` \
             (libFLAC 1.5.0) — le décodage FLAC n'est plus sans perte"
        );
    }
}

/// Contre-épreuve : la garde ci-dessus doit ROUGIR dès qu'un octet bouge.
///
/// Sans elle, une table d'empreintes ne prouve rien : un témoin qui ne sait pas
/// échouer est un témoin qui ne garde pas. On décode une copie jetable abîmée
/// d'un octet (voir [`copie_avec_un_octet_abime`]) et on exige que la mesure
/// diffère de la référence.
///
/// ⚠️ Ce qu'on exige ici, c'est la DIFFÉRENCE, pas le refus. Voir la note
/// `decodage_partiel` plus bas : `decode_symphonia` ne remonte pas l'erreur de
/// trame, il PERD un bloc et poursuit. La différence porte alors sur le nombre
/// d'échantillons, et c'est bien la garde qui rougit — mais pas pour la raison
/// qu'on croirait.
///
/// L'autre moitié de la contre-épreuve ne peut pas vivre dans le dépôt, car
/// elle demande une SECONDE fixture : le 12/09/2026, la source WAV de
/// `ref_16_44100_stereo.flac` a été réencodée avec **un seul échantillon
/// changé** (octet 30 044, trame 7 500, canal gauche). Nombre d'échantillons
/// identique — 35 280 — et empreinte `8b1f0693442dbb6609ec663c4655a803` au
/// lieu de `b702caf5d2257a84d3953c26526dd6dc` : la garde rougit sur
/// l'EMPREINTE seule, ce qui est la propriété demandée. Les deux empreintes
/// viennent de `flac -d`, pas de ce module.
#[test]
fn un_octet_abime_fait_rougir_la_garde() {
    let dossier = tune_core::test_scratch::scratch_dir("flac-contre-epreuve");

    for (nom, _, _, _, echantillons, md5) in FIXTURES {
        let (copie, offset) = copie_avec_un_octet_abime(&dossier, nom);
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

/// Ce que cette tranche a mesuré et ne corrige PAS.
///
/// Une trame FLAC dont le CRC ne tombe pas juste ne produit **aucune erreur**
/// pour l'appelant — elle produit un PCM plus COURT, et `decode_to_pcm` rend
/// `Ok`. Mesuré le 12/09/2026 sur `ref_16_44100_stereo.flac`, un octet inversé
/// au milieu des trames : **27 088 échantillons rendus sur 35 280**.
///
/// # 🔴 Le POURQUOI, remesuré — ce n'est PAS une troncature
///
/// La première rédaction de cette note désignait les deux `Err(_)` muets de
/// `decode_symphonia` (`continue` sur `decoder.decode`, `break` sur
/// `format.next_packet`) comme la cause, et concluait que « sur une piste de
/// quatre minutes, c'est une minute de musique qui disparaît ». **Les deux
/// affirmations sont fausses**, et la seconde a été reprise telle quelle dans
/// les notes de la v0.9.147.
///
/// Mesure du 12/09/2026, sur la même copie abîmée :
///
/// ```text
/// paquets_refuses = 0   trames_refusees = 0   premier_refus = None
/// préfixe commun avec la référence : 8 192 trames
/// suffixe commun avec la référence : 5 352 trames
/// préfixe + suffixe = 13 544 = TOUTE la sortie abîmée
/// ```
///
/// **Aucun des deux `Err(_)` ne se déclenche jamais** : le compteur d'`IntegriteFlux`
/// reste à zéro sur les deux. Et la queue de la piste est **présente et juste**,
/// bit pour bit. Ce qui manque est **un seul bloc FLAC de 4 096 trames**,
/// prélevé au milieu : le décodeur se resynchronise tout seul, dans
/// `symphonia-bundle-flac` — `PacketBuilder::try_build` vide sa file de
/// fragments (`self.frags.clear()`) dès que le fragment suivant porte un CRC-16
/// juste, et le fragment abîmé part avec elle, sans un mot.
///
/// Le seul contrôle qui voie quoi que ce soit est donc la comparaison de T4,
/// `STREAMINFO.total samples` face aux trames rendues.
///
/// **Le « 23,2 % » n'est pas une propriété du défaut, c'est une propriété de la
/// fixture** : elle dure 0,4 s, et un bloc EST 23,2 % de 0,4 s. Sur la piste de
/// quatre minutes de la note d'origine, le même octet abîmé coûte les mêmes
/// 4 096 trames, soit **92,9 ms — 0,04 %**, et non une minute. L'écart entre
/// les deux lectures est d'un facteur 600 ; il vient d'avoir extrapolé un
/// pourcentage mesuré sur une piste de 0,4 s.
///
/// Ce que le décodeur fait mal n'est donc pas de s'arrêter trop tôt — il ne
/// s'arrête pas. C'est de **jeter un bloc en silence**. Le chemin de LECTURE
/// le journalise depuis `tests/journal_perte_flac_2218.rs` ; le refus, lui,
/// reste un arbitrage ouvert et n'est tranché nulle part.
///
/// Ce témoin-ci **constate** le fait, pour qu'il ne se redécouvre pas. Le jour
/// où le décodeur remontera l'erreur, il rougira — et c'est la bonne nouvelle :
/// il faudra alors le retourner en exigeant le refus.
#[test]
fn decodage_partiel_une_trame_abimee_ne_remonte_aucune_erreur() {
    let dossier = tune_core::test_scratch::scratch_dir("flac-troncature");
    let (nom, _, _, _, echantillons, _) = FIXTURES[0];
    let (copie, _) = copie_avec_un_octet_abime(&dossier, nom);

    match mesure(&copie) {
        Ok((_, _, _, n, _)) => {
            assert!(
                n < echantillons,
                "{nom} : une trame abîmée a rendu AUTANT d'échantillons que la \
                 référence ({n}) — ce n'est ni un refus ni une troncature, donc \
                 c'est du PCM faux servi comme bon"
            );
            eprintln!(
                "T1 #2218 — trame abîmée : {n} échantillons rendus sur {echantillons} \
                 attendus ({:.1} % de la piste perdus), sans une seule erreur remontée \
                 à l'appelant.",
                100.0 * (1.0 - n as f64 / echantillons as f64)
            );
        }
        Err(e) => eprintln!(
            "T1 #2218 — le décodeur FLAC refuse désormais une trame abîmée : {e}. \
             Retourner ce témoin en exigeant le refus."
        ),
    }
}
