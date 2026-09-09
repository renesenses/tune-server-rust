//! #3618 — un DAC qui refuse le flottant ne doit plus faire taire la branche
//! compressée.
//!
//! Belkadi Yacine, v0.9.141, Linux/ALSA, DENAFRIPS Terminator II. Neuf lectures
//! `source=upnp` contre zéro contre-exemple : tout ce qui vient d'un serveur
//! multimédia arrive sans en-tête WAV — `orchestrator/commun.rs` envoie `upnp`
//! (comme `radio`, `podcast` et `bandcamp`) sur `resolve_direct`, qui rend
//! l'URL inchangée — donc sur la branche « flux compressé » de `local.rs`.
//! Cette branche n'offrait que du `f32`, et sur la première erreur
//! (`Sample format 'f32' is not supported by hardware in any endianness`) elle
//! abandonnait : `playing = false`, `open_failure` jamais renseigné, écran
//! muet.
//!
//! Le remède existait déjà 1 700 lignes plus bas, sur le chemin PCM. Ces
//! épreuves tiennent les deux bouts : la RÈGLE de repli (fonctions de
//! production, périphérique factice) et son BRANCHEMENT (le chemin compressé
//! l'appelle réellement, et renseigne le canal d'erreur).

use super::{FormatDeSortie, cascade_de_formats, ouvrir_premier_format_accepte};

fn cfg(channels: u16, sample_rate: u32) -> cpal::StreamConfig {
    cpal::StreamConfig {
        channels,
        sample_rate,
        buffer_size: cpal::BufferSize::Default,
    }
}

/// L'ordre exact : `f32` aux deux cadences d'abord — le chemin heureux reste
/// intact — puis la cascade entière `i32`/`i16`.
#[test]
fn la_cascade_tente_f32_puis_les_entiers_aux_deux_cadences() {
    let principal = cfg(2, 44_100);
    let source = cfg(2, 48_000);
    let ordre: Vec<(u32, &str)> = cascade_de_formats(&principal, &source)
        .iter()
        .map(|(c, f)| (c.sample_rate, f.nom()))
        .collect();
    assert_eq!(
        ordre,
        vec![
            (44_100, "f32"),
            (48_000, "f32"),
            (44_100, "i32"),
            (44_100, "i16"),
            (48_000, "i32"),
            (48_000, "i16"),
        ],
        "la branche compressée doit tenter exactement la même cascade que le \
         chemin PCM ; c'est elle, et elle seule, qui rattrape un DAC \
         bit-perfect qui refuse le flottant"
    );
}

/// Une cadence source identique à la principale n'ajoute pas de doublon.
#[test]
fn une_cadence_source_identique_ne_double_pas_les_tentatives() {
    let c = cfg(2, 48_000);
    let ordre: Vec<&str> = cascade_de_formats(&c, &c)
        .iter()
        .map(|(_, f)| f.nom())
        .collect();
    assert_eq!(ordre, vec!["f32", "i32", "i16"]);
}

/// LE cas du testeur : le périphérique refuse `f32` dans toutes les
/// endianness, et accepte `i32`. Avant le correctif, la lecture s'arrêtait à
/// la première ligne de cette liste.
#[test]
fn un_dac_qui_refuse_le_flottant_est_ouvert_en_entier() {
    let tentatives = cascade_de_formats(&cfg(2, 44_100), &cfg(2, 48_000));
    let mut essais: Vec<String> = Vec::new();
    let ouvert = ouvrir_premier_format_accepte(&tentatives, |c, f| {
        essais.push(format!("{}@{}", f.nom(), c.sample_rate));
        if f == FormatDeSortie::F32 {
            Err("Sample format 'f32' is not supported by hardware in any endianness")
        } else {
            Ok(format!("flux {}", f.nom()))
        }
    });
    let (flux, config, retenu) = ouvert.expect(
        "le DENAFRIPS accepte l'entier : sans repli, la zone s'arrête en \
         silence et rien n'apparaît à l'écran (#3618)",
    );
    assert_eq!(flux, "flux i32");
    assert_eq!(retenu, FormatDeSortie::I32);
    assert_eq!(config.sample_rate, 44_100);
    assert_eq!(
        essais,
        vec!["f32@44100", "f32@48000", "i32@44100"],
        "le flottant doit rester tenté EN PREMIER, aux deux cadences : le \
         chemin heureux ne doit rien changer pour les DAC qui l'acceptent"
    );
}

/// Le chemin heureux, intact : un périphérique qui accepte `f32` est ouvert du
/// premier coup et rien d'autre n'est tenté.
#[test]
fn un_peripherique_qui_accepte_le_flottant_est_ouvert_du_premier_coup() {
    let tentatives = cascade_de_formats(&cfg(2, 48_000), &cfg(2, 44_100));
    let mut essais = 0usize;
    let (_, _, retenu) = ouvrir_premier_format_accepte(&tentatives, |_, f| {
        essais += 1;
        Ok::<_, &str>(f)
    })
    .expect("le premier format doit suffire");
    assert_eq!(retenu, FormatDeSortie::F32);
    assert_eq!(
        essais, 1,
        "aucune tentative superflue sur le chemin heureux"
    );
}

/// Tout refusé : les erreurs sont CONSERVÉES, la première en tête. Sans elle,
/// impossible de classer la panne — et c'est cette classification qui remplit
/// `open_failure`, donc le message à l'écran.
#[test]
fn un_refus_total_rend_toutes_les_erreurs_la_premiere_en_tete() {
    let tentatives = cascade_de_formats(&cfg(2, 44_100), &cfg(2, 48_000));
    let echecs = ouvrir_premier_format_accepte(&tentatives, |_, f| match f {
        FormatDeSortie::F32 => Err::<(), _>("Sample format 'f32' is not supported"),
        FormatDeSortie::I32 => Err("i32 refusé"),
        FormatDeSortie::I16 => Err("i16 refusé"),
    })
    .expect_err("aucun format accepté");
    assert_eq!(echecs.len(), tentatives.len());
    assert_eq!(echecs[0], "Sample format 'f32' is not supported");
}

// ---------------------------------------------------------------------------
// Le BRANCHEMENT. Sans lui, tout ce qui précède serait « écrit mais pas
// branché » : la règle compilerait, serait verte, et la branche compressée
// continuerait d'abandonner à la première erreur.
// ---------------------------------------------------------------------------

/// ⚠️ `include_str!` rend le fichier ENTIER. On coupe à ce module pour que les
/// motifs cherchés ne puissent pas se trouver eux-mêmes dans les messages
/// d'assertion ni dans les épreuves qui suivent.
fn code_de_production() -> &'static str {
    const TOUT: &str = include_str!("../local.rs");
    const BORNE: &str = "mod repli_format_compresse_i3618";
    let fin = TOUT
        .find(BORNE)
        .unwrap_or_else(|| panic!("ce module a été renommé : la découpe ne protège plus rien"));
    &TOUT[..fin]
}

/// La branche compressée : de `local_audio_non_wav_stream_detected_decoding`
/// jusqu'à sa ligne d'échec d'ouverture.
fn branche_compressee() -> &'static str {
    let code = code_de_production();
    let debut = code
        .find("local_audio_non_wav_stream_detected_decoding")
        .expect("la branche « flux compressé » ne se journalise plus : elle a été renommée");
    let reste = &code[debut..];
    let fin = reste
        .find("audio_stream_build_failed_compressed")
        .expect("la branche compressée ne nomme plus son échec d'ouverture");
    &reste[..fin]
}

#[test]
fn la_branche_compressee_emprunte_reellement_la_cascade() {
    let bloc = branche_compressee();
    assert!(
        bloc.contains("ouvrir_premier_format_accepte(&tentatives"),
        "la branche compressée doit ouvrir PAR la cascade. Un
         `device.build_output_stream` isolé y rétablit le défaut de #3618 : \
         une seule tentative, en `f32`, et la zone se tait."
    );
    assert!(
        bloc.contains("cascade_de_formats(&output_config, &source_config)"),
        "la liste des tentatives doit venir de la règle partagée avec le \
         chemin PCM, pas d'une seconde liste recopiée ici"
    );
    for format in [
        "FormatDeSortie::I32 => build_int_stream::<i32>",
        "FormatDeSortie::I16 => build_int_stream::<i16>",
    ] {
        assert!(
            bloc.contains(format),
            "la cascade doit réellement construire le flux entier ({format}) : \
             sans cela elle énumère des formats qu'elle n'ouvre jamais"
        );
    }
}

#[test]
fn l_echec_d_ouverture_compressee_renseigne_le_canal_d_erreur() {
    let code = code_de_production();
    // La fenêtre couvre la branche d'échec de la cascade, du `match` sur son
    // résultat jusqu'à la ligne qui coupe la lecture.
    let debut = code
        .find("match ouverture {")
        .expect("la branche compressée n'arbitre plus le résultat de la cascade");
    let reste = &code[debut..];
    let fin = reste
        .find("playing.store(false, Ordering::SeqCst);")
        .expect("le bloc d'échec ne coupe plus la lecture");
    let bloc = &reste[..fin];
    assert!(
        bloc.contains("\"audio_stream_build_failed_compressed\""),
        "l'échec d'ouverture doit rester NOMMÉ sous ce marqueur : c'est lui \
         que les diagnostics de terrain cherchent (#3618)"
    );
    assert!(
        bloc.contains("open_failure.lock()"),
        "l'échec d'ouverture doit renseigner `open_failure` — le canal que le \
         sondeur draine à chaque tick pour émettre `zone.playback_error`. \
         Sans lui la zone s'arrête en silence, sans cause affichée : c'est la \
         moitié du défaut de #3618 que le testeur voit à l'écran."
    );
    assert!(
        bloc.contains("classify_open_failure("),
        "la cause doit être CLASSÉE comme sur le chemin PCM, sinon l'écran \
         affiche la chaîne ALSA brute, qui envoie chercher au mauvais endroit"
    );
}
