//! T4 #2218 — les contrôles d'intégrité que les conteneurs offrent.
//!
//! # Ce que cette tranche garde
//!
//! Les tranches T1 à T3 ont posé un banc d'empreintes sur FLAC, ALAC, AIFF,
//! WAV et DSD, et il a trouvé trois défauts en quelques heures. Les trois ont
//! la même forme : **le conteneur porte l'information, le code ne la fait pas
//! remonter.** C'est le mécanisme du défaut WavPack — un CRC par bloc rangé
//! dans un champ `_crc: u32`, jamais comparé, et trois mois de bruit blanc.
//!
//! Ces témoins gardent la moitié qui n'exige AUCUN arbitrage : rendre la perte
//! LISIBLE. Aucun d'eux n'exige un refus, et le correctif qu'ils gardent n'en
//! introduit aucun. Le choix « refuser ou lire en signalant » (« D1 ») revient
//! à Bertrand et reste ouvert.
//!
//! # Les fixtures
//!
//! Toutes réutilisées telles quelles des tranches T1/T2/T3, sous
//! `tune-core/tests/fixtures/` — aucune n'est refabriquée ici. Les seuls
//! fichiers écrits par ce module sont des COPIES jetables, dans un
//! `ScratchDir` supprimé à la sortie de portée, dont un octet est inversé.
//! L'original n'est jamais touché.

use tune_core::audio::decode::decode_to_pcm;

fn chemin_fixture(sous_dossier: &str, nom: &str) -> String {
    format!(
        "{}/tests/fixtures/{sous_dossier}/{nom}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Une copie jetable dont UN octet, pris au milieu du fichier, est inversé.
///
/// Même geste que les tranches T1/T2/T3 (`copie_avec_un_octet_abime`), et pour
/// la même raison : le milieu du fichier tombe dans les trames audio, pas dans
/// l'en-tête — c'est le DÉCODAGE du signal qu'on éprouve, pas l'analyse du
/// conteneur.
fn copie_abimee(dossier: &std::path::Path, source: &str, nom: &str) -> String {
    let mut octets = std::fs::read(source).unwrap_or_else(|e| panic!("lire {source} : {e}"));
    let offset = octets.len() / 2;
    assert!(offset > 512, "{source} : fixture trop courte");
    octets[offset] ^= 0xFF;
    let copie = dossier.join(nom);
    std::fs::write(&copie, &octets).expect("écrire la copie abîmée");
    copie.to_str().expect("chemin utf-8").to_owned()
}

// ── FLAC ───────────────────────────────────────────────────────────────

/// Le cas fondateur de la tranche, en négatif : un fichier SAIN ne doit rien
/// signaler.
///
/// Sans ce témoin, la garde suivante serait verte contre un détecteur qui
/// crierait sur tout — donc contre rien.
#[test]
fn t4_un_flac_sain_ne_signale_aucune_perte() {
    let chemin = chemin_fixture("flac", "ref_16_44100_stereo.flac");
    let audio = decode_to_pcm(&chemin, None, None, 0.0, 0.0).expect("décodage de la fixture saine");
    assert_eq!(
        audio.integrite.trames_annoncees,
        Some(17_640),
        "le STREAMINFO annonce 17 640 trames (35 280 échantillons sur 2 canaux) — \
         si ce champ est `None`, la longueur annoncée n'est plus lue et la garde \
         suivante ne garde plus rien"
    );
    assert_eq!(audio.integrite.trames_rendues, 17_640);
    assert!(
        !audio.integrite.perte_detectee(),
        "une fixture SAINE est signalée comme abîmée : {:?}",
        audio.integrite
    );
    assert_eq!(audio.integrite.perte_pour_cent(), Some(0.0));
}

/// 🔴 Le bord le plus facile à casser, et le plus coûteux à casser.
///
/// `decode_to_pcm` journalise à `warn` dès que `perte_detectee()` est vrai.
/// Un détecteur trop bavard ne serait pas un détail : l'export de diagnostic
/// borne chaque module à un quart de sa fenêtre (`QUOTA_PAR_MODULE`, #1974),
/// donc un émetteur qui crierait sur chaque piste d'une bibliothèque
/// arracherait ses lignes à tous les autres — et noierait la panne qu'on
/// cherche. Une garde qui ne verrouille que le bord « ça crie quand il faut »
/// laisse passer exactement ça.
///
/// Le risque est réel et nommé : la longueur annoncée n'est pas exacte dans
/// tous les conteneurs. Un MP3 la tient d'un en-tête Xing facultatif, un Ogg
/// n'en annonce aucune. Ce témoin passe TOUTES les fixtures saines du dépôt,
/// un format par ligne, et exige le silence sur chacune.
#[test]
fn t4_aucune_fixture_saine_ne_declenche_de_signalement() {
    let base = env!("CARGO_MANIFEST_DIR");
    let fixtures = [
        "tests/fixtures/flac/ref_16_44100_stereo.flac",
        "tests/fixtures/flac/ref_24_96000_stereo.flac",
        "tests/fixtures/flac/ref_16_44100_mono.flac",
        "tests/fixtures/wav/ref_16_44100_stereo.wav",
        "tests/fixtures/wav/ref_24_96000_stereo.wav",
        "tests/fixtures/alac/ref_16_44100_stereo.m4a",
        "tests/fixtures/alac/ref_24_96000_stereo.m4a",
        "tests/fixtures/aiff/ref_16_44100_stereo.aiff",
        "tests/fixtures/aiff/ref_24_88200_stereo.aiff",
        "tests/fixtures/wavpack/rip_16_44100_stereo.wv",
        "tests/fixtures/wavpack/hires_24_96000_stereo.wv",
        "tests/fixtures/wavpack/mono_16_44100.wv",
        "tests/fixtures/ape/sine_16s_c3000.ape",
        "tests/fixtures/dsd/ref_dsd64_stereo.dsf",
        "tests/fixtures/dsd/ref_dsd64_stereo.dff",
        "tests/fixtures/test.flac",
        "tests/fixtures/test.wav",
        "tests/fixtures/test.aiff",
        "tests/fixtures/test.mp3",
        "tests/fixtures/test.m4a",
        "tests/fixtures/test.ogg",
        "tests/fixtures/test_vorbis.ogg",
        "tests/fixtures/test.opus",
    ];

    let mut examinees = 0usize;
    let mut bavardes: Vec<String> = Vec::new();
    for nom in fixtures {
        let chemin = format!("{base}/{nom}");
        assert!(
            std::path::Path::new(&chemin).exists(),
            "fixture absente : {chemin} — corriger la liste plutôt que la laisser \
             se vider en silence"
        );
        let audio = decode_to_pcm(&chemin, None, None, 0.0, 0.0)
            .unwrap_or_else(|e| panic!("{nom} : fixture saine refusée : {e}"));
        examinees += 1;
        if audio.integrite.perte_detectee() {
            bavardes.push(format!("{nom} → {:?}", audio.integrite));
        }
    }
    assert_eq!(
        examinees,
        fixtures.len(),
        "toutes les fixtures n'ont pas été décodées"
    );
    assert!(
        bavardes.is_empty(),
        "{} fixture(s) SAINE(s) déclenchent le signalement de perte — chaque \
         piste du parc écrirait une ligne WARN dans l'export de diagnostic, et \
         la vraie panne y serait noyée :\n{}",
        bavardes.len(),
        bavardes.join("\n")
    );
}

/// 🔴 La garde principale : un octet abîmé dans un FLAC doit être VISIBLE.
///
/// Mesuré le 12/09/2026 sur `batch/bugs-11` (762904de), AVANT ce correctif :
/// `decode_to_pcm` rendait `Ok(_)` avec 27 088 échantillons sur 35 280 —
/// 23,2 % de la piste évaporés —, sans une ligne de journal et sans aucun
/// moyen pour l'appelant de le savoir. Le `STREAMINFO` portait pourtant la
/// longueur exacte depuis toujours : elle était LUE par le démultiplexeur et
/// jamais COMPARÉE.
///
/// Ce témoin n'exige pas de refus. Il exige que la perte se voie.
#[test]
fn t4_un_octet_abime_dans_un_flac_remonte_une_perte_chiffree() {
    let dossier = tune_core::test_scratch::scratch_dir("t4-flac-perte");
    let source = chemin_fixture("flac", "ref_16_44100_stereo.flac");
    let copie = copie_abimee(dossier.path(), &source, "abime.flac");

    let audio = decode_to_pcm(&copie, None, None, 0.0, 0.0)
        .expect("le correctif ne refuse RIEN : un FLAC abîmé doit toujours se lire");

    assert!(
        audio.integrite.perte_detectee(),
        "un octet abîmé n'a laissé AUCUNE trace dans `integrite` — c'est très \
         exactement le défaut #2218 T1 : {} échantillons rendus, `Ok(_)`, et rien \
         à lire. État : {:?}",
        audio.samples_i32.len(),
        audio.integrite
    );
    let perte = audio
        .integrite
        .perte_pour_cent()
        .expect("le STREAMINFO annonce une longueur, donc la perte est chiffrable");
    assert!(
        perte > 1.0,
        "perte chiffrée à {perte:.2} % seulement — le compte de trames annoncées \
         n'est plus comparé à ce qui sort"
    );
    eprintln!(
        "T4 #2218 — FLAC, un octet abîmé : {} trames rendues sur {:?} annoncées \
         ({perte:.1} % perdus), contrôle nommé : {:?}",
        audio.integrite.trames_rendues,
        audio.integrite.trames_annoncees,
        audio.integrite.premier_refus
    );
}

// ── Ogg (Vorbis) : le CRC-32 de page ───────────────────────────────────

/// Le CRC-32 par page est le seul contrôle de la famille Ogg, et symphonia le
/// vérifie déjà (`symphonia-format-ogg-0.6.0/src/page.rs:241`).
///
/// Mesuré : selon l'endroit de la casse, le verdict arrive par DEUX chemins.
/// Sur une page lue à l'ouverture, c'est le sondage qui refuse — message
/// « probe: malformed stream: ogg: crc mismatch », déjà nommé. Sur une page
/// lue plus tard, c'est `format.next_packet()` qui rend l'erreur, et c'est
/// ce verdict-là que `decode_symphonia` jetait dans un `Err(_) => break`.
///
/// Ce témoin exige la même chose des deux : que le mot « crc » parvienne à
/// l'appelant, sur le modèle du correctif WavPack (`err.contains("CRC")`).
///
/// ⚠️ Ce que la MESURE dit du chemin emprunté, et qu'il faut lire avant de
/// croire ce témoin plus large qu'il n'est. Le 12/09/2026, vingt octets
/// répartis sur `test_vorbis.ogg` ont été inversés un par un : les treize
/// premiers (jusqu'à l'offset 6 260 sur 9 631) ont été refusés AU SONDAGE, et
/// les sept derniers n'ont rien changé au PCM rendu — le sondage de symphonia
/// lit assez de pages pour que la casse lui tombe dessus la première. Sur ces
/// fixtures-là, le compteur ajouté à `decode_symphonia` n'est donc jamais
/// atteint ; il garde le cas d'un fichier assez long pour que la casse tombe
/// après le sondage. Ce témoin verrouille ce qui est mesurable ici : que le
/// verdict du CRC parvienne à l'appelant, et qu'il le NOMME.
#[test]
fn t4_un_octet_abime_dans_un_ogg_nomme_le_crc_de_page() {
    let dossier = tune_core::test_scratch::scratch_dir("t4-ogg-crc");
    let source = format!(
        "{}/tests/fixtures/test_vorbis.ogg",
        env!("CARGO_MANIFEST_DIR")
    );
    let copie = copie_abimee(dossier.path(), &source, "abime.ogg");

    let nomme = match decode_to_pcm(&copie, None, None, 0.0, 0.0) {
        // Refus au sondage : le message porte déjà le nom du contrôle.
        Err(e) => e,
        // Lecture poursuivie : c'est `integrite` qui doit porter le nom.
        Ok(audio) => {
            assert!(
                audio.integrite.perte_detectee(),
                "un octet abîmé dans un Ogg n'a laissé aucune trace : {:?}",
                audio.integrite
            );
            audio.integrite.premier_refus.clone().unwrap_or_default()
        }
    };
    let bas = nomme.to_lowercase();
    assert!(
        bas.contains("crc"),
        "le verdict ne NOMME pas le contrôle : « {nomme} ». Un compteur sans nom \
         ne dit pas au testeur ce qui a lâché — c'est la leçon du CRC WavPack."
    );
    eprintln!("T4 #2218 — Ogg, un octet abîmé : contrôle nommé « {nomme} »");
}

// ── APE : le CRC par trame, déjà branché ───────────────────────────────

/// APE est le format le mieux gardé du parc, et rien de ce que T4 ajoute ne
/// l'améliore — ce témoin VERROUILLE l'acquis.
///
/// La caisse `ape-decoder` calcule le CRC de chaque trame et le compare
/// (`decoder.rs:828-831` → `ApeError::InvalidChecksum`), et les DEUX bras de
/// Tune propagent ce refus par `?` : `decode_ape_to_pcm` (chemin par lots) et
/// `decode_ape_streaming` (chemin progressif, qui journalise en plus
/// `ape_streaming_trame_refusee`).
///
/// ⚠️ Ce que la MESURE dit du CRC, et qu'il faut lire avant de croire ce
/// témoin plus fort qu'il n'est. Le 12/09/2026, quarante octets répartis sur
/// toute la longueur de `sine_16s_c3000.ape` ont été inversés un par un :
/// **les quarante ont été refusés**, mais aucun par le CRC — tous par le
/// décodeur entropique lui-même (« 16-bit sample overflow », « range coder:
/// overflow range_total out of bounds »), qui se désynchronise bien avant la
/// fin de la trame. Le CRC est le FILET derrière ce refus-là : il n'attrape
/// que la corruption qui se décode proprement. C'est pour cette raison que ce
/// témoin exige le refus, et non un message contenant « checksum » : exiger
/// le second serait exiger une chose que la mesure ne produit pas.
#[test]
fn t4_un_octet_abime_dans_un_ape_est_refuse_et_non_servi() {
    // La fixture du banc APE de #2505 : un `.ape` réel, appairé à son `.wav`.
    let source = chemin_fixture("ape", "sine_16s_c3000.ape");
    assert!(
        std::path::Path::new(&source).exists(),
        "fixture APE absente : {source}"
    );
    let dossier = tune_core::test_scratch::scratch_dir("t4-ape-crc");
    let copie = copie_abimee(dossier.path(), &source, "abime.ape");

    match decode_to_pcm(&copie, None, None, 0.0, 0.0) {
        Err(e) => {
            assert!(
                e.contains("ape"),
                "l'APE refuse, mais sans dire que c'est lui : « {e} »"
            );
            eprintln!("T4 #2218 — APE, un octet abîmé : refus « {e} »");
        }
        Ok(audio) => panic!(
            "un octet abîmé dans un `.ape` a rendu {} échantillons comme sains. \
             Ni le décodeur entropique ni le CRC par trame \
             (`ApeError::InvalidChecksum`) ne sont plus propagés.",
            audio.samples_i32.len()
        ),
    }
}

// ── DSDIFF : le remplissage IFF, et le message du refus ────────────────

// La lecture d'un `.dff` à sous-chunk impair est gardée là où vit la fixture :
// `tests/dsd_empreintes_reference.rs::un_chunk_cmpr_de_taille_impaire_se_lit_desormais`
// — le témoin de T3, retourné comme T3 le demandait. Le dupliquer ici ne
// garderait rien de plus. Ce qui suit garde l'AUTRE moitié du correctif.

/// L'autre moitié du correctif DSDIFF : une taille de chunk ABSURDE doit
/// produire un message qui nomme le chunk et le fichier, pas un code errno.
///
/// Avant : `seek(SeekFrom::Current(padded as i64))` recevait la valeur brute,
/// l'OS répondait EINVAL, et le testeur lisait « Invalid argument
/// (os error 22) » sur un `.dff` — impossible à diagnostiquer.
#[test]
fn t4_un_dsdiff_a_taille_de_chunk_absurde_refuse_par_un_message_lisible() {
    let source = chemin_fixture("dsd", "ref_dsd64_stereo.dff");
    let mut octets = std::fs::read(&source).expect("lire la fixture DFF");

    // Trouver le sous-chunk `FS  ` et gonfler sa taille annoncée (u64 BE).
    let pos = octets
        .windows(4)
        .position(|w| w == b"FS  ")
        .expect("la fixture doit porter un sous-chunk FS");
    octets[pos + 4..pos + 12].copy_from_slice(&u64::MAX.to_be_bytes());

    let dossier = tune_core::test_scratch::scratch_dir("t4-dff-absurde");
    let copie = dossier.join("taille_absurde.dff");
    std::fs::write(&copie, &octets).expect("écrire la copie");

    let e = tune_core::audio::dff::parse_dff(copie.to_str().expect("utf-8"))
        .expect_err("un sous-chunk de 2^64-1 octets ne peut pas être lu");
    assert!(
        e.contains("DSDIFF") && e.contains("FS"),
        "le refus ne nomme ni le format ni le chunk en cause : « {e} » — c'est le \
         message illisible que T3 a constaté (« Invalid argument (os error 22) »)"
    );
    assert!(
        !e.contains("os error"),
        "le refus retombe sur un code errno brut : « {e} »"
    );
    eprintln!("T4 #2218 — DSDIFF, taille absurde : refus lisible « {e} »");
}

// ── AIFF : COMM et SSND, deux longueurs jamais confrontées ─────────────

/// 🔴 L'AIFF ne porte AUCUNE somme de contrôle. Son seul recoupement possible
/// est structurel — et il ne se faisait pas.
///
/// `decode_aiff_to_pcm` borne sa lecture sur `COMM.numSampleFrames` et ignore
/// entièrement la taille du chunk `SSND` : `AiffInfo::data_size` n'avait AUCUN
/// lecteur dans tout le dépôt (mesuré le 12/09/2026 — le champ, une écriture,
/// et une assertion `> 0` dans un test unitaire ; pas un seul consommateur).
///
/// La conséquence n'est pas théorique. Quand `SSND` porte moins d'octets que
/// `COMM` n'annonce de trames, la lecture continue **au-delà du SSND**, dans
/// ce qui suit — des octets qui ne sont pas de l'audio, servis à la bonne
/// longueur, sans une erreur. C'est la forme ALAC du défaut : celle que ni la
/// durée, ni le compte, ni le code de retour ne distinguent d'un flux sain.
///
/// ⛔ Ce témoin n'exige PAS que la lecture soit bornée au `SSND` : borner
/// changerait le PCM rendu à des fichiers qui se lisent aujourd'hui, et ce
/// choix revient à Bertrand. Il exige que l'incohérence soit DITE.
#[test]
fn t4_un_aiff_dont_le_ssnd_sous_annonce_le_signale() {
    let source = chemin_fixture("aiff", "ref_16_44100_stereo.aiff");
    let mut octets = std::fs::read(&source).expect("lire la fixture AIFF");

    // Réduire la taille annoncée du SSND de 4 096 octets — le fichier, lui, ne
    // change pas d'un octet : c'est bien l'ANNONCE qu'on met en défaut, pas la
    // longueur réelle (un fichier tronqué, lui, échoue déjà sur `read_exact`).
    let ssnd = octets
        .windows(4)
        .position(|w| w == b"SSND")
        .expect("la fixture doit porter un chunk SSND");
    let avant = u32::from_be_bytes(octets[ssnd + 4..ssnd + 8].try_into().expect("4 octets"));
    assert!(
        avant > 8_192,
        "SSND trop court pour cette épreuve : {avant}"
    );
    octets[ssnd + 4..ssnd + 8].copy_from_slice(&(avant - 4_096).to_be_bytes());

    let dossier = tune_core::test_scratch::scratch_dir("t4-aiff-ssnd-court");
    let copie = dossier.join("ssnd_sous_annonce.aiff");
    std::fs::write(&copie, &octets).expect("écrire la copie");
    let copie = copie.to_str().expect("utf-8");

    // La lecture n'est PAS refusée — c'est le contrat de cette tranche.
    let audio = match decode_to_pcm(copie, None, None, 0.0, 0.0) {
        Ok(a) => a,
        Err(e) => panic!(
            "l'AIFF truqué est désormais REFUSÉ (« {e} ») — T4 ne devait rien \
             refuser ; c'est un arbitrage que Bertrand n'a pas rendu"
        ),
    };

    let nomme = audio.integrite.premier_refus.clone().unwrap_or_default();
    assert!(
        nomme.contains("SSND") && nomme.contains("COMM"),
        "le SSND porte 1 024 trames de moins que COMM n'en annonce, et rien ne le \
         dit : « {nomme} ». État complet : {:?}",
        audio.integrite
    );
    assert!(
        audio.integrite.perte_detectee(),
        "l'incohérence est décrite mais `perte_detectee()` reste faux — le \
         consommateur de `decode_to_pcm` ne journalisera rien"
    );
    eprintln!("T4 #2218 — AIFF, SSND sous-annoncé : « {nomme} »");
}

/// Le débordement par le bas de `parse_aiff`, corrigé : un `SSND` dont la
/// taille annoncée est plus petite que son propre sous-en-tête.
///
/// Avant : `chunk_size as u64 - 8 - offset_field as u64` était NU — panique en
/// debug, et en release un `data_size` d'environ 2^64 rendu comme valide.
#[test]
fn t4_un_ssnd_plus_petit_que_son_entete_refuse_sans_deborder() {
    let source = chemin_fixture("aiff", "ref_16_44100_stereo.aiff");
    let mut octets = std::fs::read(&source).expect("lire la fixture AIFF");

    let ssnd = octets
        .windows(4)
        .position(|w| w == b"SSND")
        .expect("la fixture doit porter un chunk SSND");
    // Annoncer 4 octets : moins que les 8 octets d'offset/blockSize.
    octets[ssnd + 4..ssnd + 8].copy_from_slice(&4u32.to_be_bytes());

    let dossier = tune_core::test_scratch::scratch_dir("t4-aiff-ssnd");
    let copie = dossier.join("ssnd_trop_court.aiff");
    std::fs::write(&copie, &octets).expect("écrire la copie");

    let e = match tune_core::audio::aiff::parse_aiff(copie.to_str().expect("utf-8")) {
        Err(e) => e,
        Ok(info) => panic!(
            "un SSND de 4 octets a été accepté : data_size = {} — la soustraction \
             `chunk_size - 8 - offset` a débordé par le bas",
            info.data_size
        ),
    };
    assert!(
        e.contains("SSND"),
        "le refus ne nomme pas le chunk en cause : « {e} »"
    );
    eprintln!("T4 #2218 — AIFF, SSND tronqué : refus nommé « {e} »");
}

// ── DSF : deux longueurs annoncées, jamais confrontées ─────────────────

/// Sur un DSF SAIN, les deux annonces concordent : aucun signalement.
///
/// Le bord facile à casser. Sans lui, un détecteur qui crierait sur tous les
/// DSF du parc serait vert — et noierait l'export de diagnostic, dont chaque
/// module n'a qu'un quart de la fenêtre (`QUOTA_PAR_MODULE`, #1974).
#[test]
fn t4_un_dsf_sain_ne_signale_aucune_incoherence() {
    let chemin = chemin_fixture("dsd", "ref_dsd64_stereo.dsf");
    let info = tune_core::audio::dsf::parse_dsf(&chemin).expect("fixture DSF saine");
    assert_eq!(
        info.incoherence_de_longueur(),
        None,
        "une fixture DSF saine est signalée comme incohérente : data_size = {}, \
         octets attendus = {}",
        info.data_size,
        info.octets_attendus()
    );
}

/// 🔴 Le pire des trois cas de la tranche, et le seul que rien ne disait.
///
/// Quand le chunk `data` porte moins d'octets que `fmt ` n'en implique,
/// `read_dsf_blocks` rend la queue à ZÉRO — et en DSD, zéro n'est pas du
/// silence : c'est un `-1.0` constant, du continu à PLEINE ÉCHELLE.
///
/// Le DSF ne porte aucune somme de contrôle ; ce recoupement de deux
/// longueurs annoncées est tout ce dont on dispose, et il ne se faisait pas.
#[test]
fn t4_un_dsf_dont_le_chunk_data_sous_annonce_le_signale() {
    let source = chemin_fixture("dsd", "ref_dsd64_stereo.dsf");
    let mut octets = std::fs::read(&source).expect("lire la fixture DSF");

    // Rétrécir la taille annoncée du chunk `data` sous ce que `fmt ` implique.
    //
    // ⚠️ Le viser en RELATIF ne marcherait pas : un chunk `data` de DSF est
    // découpé en blocs de `block_size` octets par canal, et le dernier bloc
    // est complété de zéros. La fixture annonce donc 24 576 octets là où
    // `fmt ` n'en implique que 18 000 — un écart de 6 576 octets parfaitement
    // légitime, et c'est très exactement ce qui rend ce recoupement subtil :
    // seul le défaut PAR LE BAS est une perte.
    let sain = tune_core::audio::dsf::parse_dsf(&source).expect("fixture DSF saine");
    let vise = sain.octets_attendus() - 4_096;
    let data = octets
        .windows(4)
        .position(|w| w == b"data")
        .expect("la fixture doit porter un chunk data");
    octets[data + 4..data + 12].copy_from_slice(&(vise + 12).to_le_bytes());

    let dossier = tune_core::test_scratch::scratch_dir("t4-dsf-data-court");
    let copie = dossier.join("data_sous_annonce.dsf");
    std::fs::write(&copie, &octets).expect("écrire la copie");

    let info = tune_core::audio::dsf::parse_dsf(copie.to_str().expect("utf-8"))
        .expect("T4 ne refuse rien : un DSF incohérent doit toujours s'analyser");
    let motif = info.incoherence_de_longueur().unwrap_or_else(|| {
        panic!(
            "le chunk `data` porte 4 096 octets de MOINS que `fmt ` n'en implique, \
             et rien ne le dit : data_size = {}, attendus = {}. La queue partira à \
             zéro — du continu à pleine échelle — en silence.",
            info.data_size,
            info.octets_attendus()
        )
    });
    assert!(
        motif.contains("pleine échelle"),
        "le motif ne dit pas ce que la perte PRODUIT : « {motif} »"
    );
    eprintln!("T4 #2218 — DSF, chunk data sous-annoncé : « {motif} »");
}
