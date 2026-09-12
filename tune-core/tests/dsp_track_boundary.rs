//! La frontière de piste doit rester BRANCHÉE, pas seulement exister.
//!
//! `reset_local_dsp` remet le convolveur à zéro entre deux pistes. Un test
//! unitaire qui appelle ce helper directement prouve qu'il fonctionne — il ne
//! prouve pas qu'on l'appelle. C'est précisément l'écart que JP Robbe a relevé
//! sur #2268 : onze tests verts sur le moteur isolé, et la chaîne réelle qui ne
//! drainait rien.
//!
//! `play_url` est async et pilote un périphérique : il ne se teste pas en
//! unitaire. On verrouille donc le point d'appel dans la source, comme le fait
//! déjà `no_blind_ffmpeg.rs` pour une autre invariante de ce dépôt.
//!
//! R6 bis (#2219) : les trois bras exclusifs de `play_url` vivent chacun dans
//! leur module (`local/bras_coreaudio.rs`, `local/bras_asio.rs`,
//! `local/bras_wasapi.rs`). Chaque bras est donc lu DEUX fois : son module,
//! pour ce qu'il fait ; et sa fenêtre dans `play_url`, entre les bannières,
//! pour prouver qu'il est APPELÉ — un module écrit mais pas branché serait
//! exactement le défaut que ce fichier existe pour attraper.

use std::path::Path;

fn source() -> String {
    std::fs::read_to_string(Path::new("src/outputs/local.rs"))
        .expect("src/outputs/local.rs doit être lisible depuis la racine du crate")
}

/// Le module d'un bras exclusif, entier : il n'a pas de `mod tests`. Un appel
/// par fichier, chemin en clair : `scripts/refonte/gardes.sh` inventorie les
/// lecteurs par ce littéral, un chemin composé lui échapperait.
fn bras(nom: &str) -> String {
    let lu = match nom {
        "coreaudio" => std::fs::read_to_string(Path::new("src/outputs/local/bras_coreaudio.rs")),
        "asio" => std::fs::read_to_string(Path::new("src/outputs/local/bras_asio.rs")),
        "wasapi" => std::fs::read_to_string(Path::new("src/outputs/local/bras_wasapi.rs")),
        autre => panic!("bras exclusif inconnu : {autre}"),
    };
    lu.unwrap_or_else(|e| {
        panic!("src/outputs/local/bras_{nom}.rs doit être lisible depuis la racine du crate : {e}")
    })
}

/// L'étage natif des bras Windows (REF-8, #2219) : `local/etage_natif.rs`,
/// coupé à son `mod tests`. C'est lui qui prépare (`prepare_windows_native_pcm`)
/// et qui draine (`flush_local_dsp`) pour WASAPI ; le bras ne fait plus que
/// l'appeler. Un appel par fichier, chemin en clair, comme `bras`.
fn etage_natif() -> String {
    let lu = std::fs::read_to_string(Path::new("src/outputs/local/etage_natif.rs"))
        .expect("src/outputs/local/etage_natif.rs doit être lisible depuis la racine du crate");
    lu.split("#[cfg(test)]\nmod tests")
        .next()
        .unwrap_or(&lu)
        .to_string()
}

/// La production seule : `mod tests` contient les mêmes appels et rendrait
/// toute assertion de comptage triviale. Ma première version de
/// `la_boucle_gapless_applique_le_dsp` allait jusqu'à la fin du fichier et
/// restait verte en débranchant un site.
fn production(src: &str) -> &str {
    src.split("mod tests").next().unwrap_or(src)
}

#[test]
fn play_url_remet_le_convolveur_a_zero() {
    let src = source();
    let debut = src
        .find("async fn play_url(")
        .expect("play_url doit exister — s'il a été renommé, ce test doit suivre");
    // Une fenêtre large : l'appel est en tête de fonction, juste après `stop()`.
    let fin = (debut + 4000).min(src.len());

    assert!(
        src[debut..fin].contains("reset_local_dsp(&self.convolver)"),
        "play_url n'appelle plus reset_local_dsp : la queue d'une piste \
         repartira dans la suivante (#2268, revue JP Robbe)"
    );
}

/// Le drainage doit rester BRANCHÉ sur les chemins qui TERMINENT une piste.
///
/// `flush_local_dsp` a existé pendant une PR entière sans un seul appel de
/// production — le compilateur le signalait, et je ne l'ai pas lu (JP Robbe,
/// revue de #2277). Un test qui appelle le helper directement ne peut pas voir
/// ça : il faut tenir les points d'appel réels.
///
/// ⚠️ L'invariant n'est PAS « autant de drainages que d'appels textuels à
/// `apply_local_dsp` ». La frontière PCM commune sert CoreAudio et cpal partagé,
/// et les deux sites de la boucle gapless passent eux aussi par elle sans
/// drainer : une transition gapless est un flux continu (#2296/#2232). Les
/// assertions doivent donc suivre les consommateurs de cette frontière, pas
/// recompter son implémentation.
///
/// Les transports Windows ont deux préparations exclusives : f32 quand le
/// format ASIO natif est incompatible, entière pour WASAPI et les formats
/// ASIO bit-perfect. Chaque chemin de fin possède son drainage ; les assertions
/// nommées empêchent le comptage global de masquer la perte d'un branchement.
#[test]
fn les_chemins_de_fin_de_piste_drainent_le_convolveur() {
    let src = source();
    let prod = production(&src);
    let bras_coreaudio = bras("coreaudio");
    let bras_asio = bras("asio");
    let bras_wasapi = bras("wasapi");
    let etage_natif = etage_natif();

    // R6 bis (#2219) : la définition et les deux drainages du chemin cpal
    // partagé (transition gapless, fin de chaîne) restent dans `local.rs` ;
    // chaque bras exclusif porte le sien dans son module. Le plancher ne
    // bouge pas : cinq chemins, cinq drainages, quel que soit le fichier.
    // REF-8 : le drainage de WASAPI vit dans l'étage natif (`rendre_la_queue`),
    // que le bras appelle ; il compte pour lui.
    let drainages = prod.matches("flush_local_dsp(").count() - 1 // moins la définition
        + bras_coreaudio.matches("flush_local_dsp(").count()
        + bras_asio.matches("flush_local_dsp(").count()
        + bras_wasapi.matches("flush_local_dsp(").count()
        + etage_natif.matches("flush_local_dsp(").count();
    assert!(
        drainages >= 5,
        "les cinq chemins de lecture locale doivent drainer, {drainages} trouvé(s)"
    );

    let preparation_locale = prod
        .split("impl LocalPcmProcessor<'_>")
        .nth(1)
        .and_then(|s| s.split("fn report_incomplete_local_pcm_probe(").next())
        .expect("la frontière PCM locale commune doit rester identifiable");
    assert!(
        preparation_locale.contains("apply_local_dsp("),
        "la frontière PCM commune ne passe plus par le DSP"
    );

    let coreaudio = prod
        .split("// ------- Exclusive mode path (macOS only) -------")
        .nth(1)
        .and_then(|s| {
            s.split("// ------- Exclusive mode path (Windows ASIO) -------")
                .next()
        })
        .expect("le chemin CoreAudio exclusif doit rester identifiable");
    assert!(
        coreaudio.contains("bras_coreaudio::jouer_via_coreaudio("),
        "play_url n'appelle plus le bras CoreAudio exclusif : un module écrit mais pas \
         branché ne draine rien (R6 bis, #2219)"
    );
    assert!(
        bras_coreaudio.contains("pcm_processor.process_pcm_chunk(")
            && bras_coreaudio.contains("flush_local_dsp("),
        "CoreAudio exclusif doit traverser la frontière PCM commune puis drainer sa fin de piste"
    );

    let preparation_windows = prod
        .split("fn prepare_windows_exclusive_pcm(")
        .nth(1)
        .and_then(|s| s.split("fn finish_windows_exclusive_probe(").next())
        .expect("la préparation Windows partagée doit rester identifiable");
    assert!(
        preparation_windows.contains("apply_local_dsp("),
        "la préparation Windows partagée ne passe plus par le DSP"
    );

    let preparation_windows_native = prod
        .split("fn prepare_windows_native_pcm(")
        .nth(1)
        .and_then(|s| s.split("impl OutputTarget for LocalOutput").next())
        .expect("la préparation Windows entière doit rester identifiable");
    assert!(
        preparation_windows_native.contains("apply_local_dsp("),
        "la préparation Windows entière ne traite plus le PCM non bit-perfect"
    );
    let asio = prod
        .split("// ------- Exclusive mode path (Windows ASIO) -------")
        .nth(1)
        .and_then(|s| {
            s.split("// ------- WASAPI Exclusive mode path (Windows, non-ASIO) -------")
                .next()
        })
        .expect("le chemin ASIO doit rester identifiable");
    assert!(
        asio.contains("bras_asio::jouer_via_asio("),
        "play_url n'appelle plus le bras ASIO exclusif : un module écrit mais pas branché \
         ne draine rien (R6 bis, #2219)"
    );
    // REF-8 (#2219) : ASIO a deux routes, et chacune draine. Route native :
    // l'étage natif (`EtageNatif`, `local/etage_natif.rs`, qui appelle
    // `prepare_windows_native_pcm` puis `flush_local_dsp` dans
    // `rendre_la_queue`) ; route traitée : l'étage de R1 (`EtageDeConversion`)
    // avec `flush_local_dsp` appelé dans le bras. Les deux doivent être
    // montées ET drainées : perdre l'une des deux, c'est perdre la fin de
    // piste sur la moitié des pilotes.
    assert!(
        bras_asio.contains("EtageNatif::monter(")
            && bras_asio.contains(".rendre_la_queue(")
            && bras_asio.contains("EtageDeConversion {")
            && bras_asio.contains("flush_local_dsp("),
        "ASIO doit monter l'étage conforme au pilote (natif ou R1) puis drainer sa fin de \
         piste sur les DEUX routes (REF-8, #2219)"
    );

    let wasapi = prod
        .split("// ------- WASAPI Exclusive mode path (Windows, non-ASIO) -------")
        .nth(1)
        .and_then(|s| {
            s.split("// ------- Open cpal device (shared mode) -------")
                .next()
        })
        .expect("le chemin WASAPI doit rester identifiable");
    assert!(
        wasapi.contains("bras_wasapi::jouer_via_wasapi("),
        "play_url n'appelle plus le bras WASAPI exclusif : un module écrit mais pas \
         branché ne draine rien (R6 bis, #2219)"
    );
    // REF-8 (#2219) : le bras WASAPI ne prépare ni ne draine en ligne — il
    // monte l'étage natif, lui fait décoder et pousser chaque lecture, puis
    // lui fait rendre la queue du DSP. La préparation entière et le drainage
    // vivent dans l'étage ; un bras qui n'appellerait plus l'un des deux, ou
    // un étage qui ne les contiendrait plus, rougit nommément.
    assert!(
        bras_wasapi.contains("etage.decoder_et_pousser(")
            && bras_wasapi.contains("etage.rendre_la_queue("),
        "WASAPI doit faire décoder et pousser par l'étage natif, puis lui faire rendre la \
         queue du DSP en fin de piste (REF-8, #2219)"
    );
    let decoder = etage_natif
        .split("fn decoder_et_pousser(")
        .nth(1)
        .and_then(|s| s.split("fn rendre_la_queue(").next())
        .expect("l'étage natif doit garder `decoder_et_pousser` avant `rendre_la_queue`");
    assert!(
        decoder.contains("prepare_windows_native_pcm("),
        "l'étage natif ne passe plus par la préparation entière : DoP, volume et DSP ne \
         sont plus résolus avant l'anneau (REF-8, #2219)"
    );
    let queue = etage_natif
        .split("fn rendre_la_queue(")
        .nth(1)
        .and_then(|s| s.split("fn vider(").next())
        .expect("l'étage natif doit garder `rendre_la_queue` avant `vider`");
    assert!(
        queue.contains("flush_local_dsp(") && queue.contains("f32_to_native_i32("),
        "l'étage natif ne draine plus le convolveur vers l'anneau entier en fin de piste \
         (#2209) : la queue de la convolution ne part jamais au DAC"
    );

    let partage = prod
        .split("// ------- Open cpal device (shared mode) -------")
        .nth(1)
        .expect("le chemin cpal partagé doit rester identifiable");
    // R1 (#2219) : le chemin partagé n'appelle plus `process_pcm_chunk` en
    // ligne — il MONTE la frontière PCM commune dans son étage de conversion,
    // et tout ce qui part au DAC traverse `etage.pousser`. L'exigence est la
    // même, et elle est même plus forte : il n'existe plus qu'UNE route.
    assert!(
        partage.contains("pcm: LocalPcmProcessor {")
            && partage.contains("etage.pousser(")
            && partage.contains("flush_local_dsp("),
        "cpal partagé doit traverser la frontière PCM commune puis drainer la fin de chaîne"
    );
}

/// Les pistes CHAÎNÉES en gapless doivent traverser le DSP elles aussi.
///
/// Les deux sites de la boucle gapless faisaient `adapt_channels` →
/// `rubato_resample_chunk` → `feed_ring` **sans** `apply_local_dsp` : seule la
/// première piste d'un album passait par l'EQ, la convolution et le crossfeed,
/// toutes les suivantes partaient sèches (JP Robbe, #2296).
///
/// Ce défaut est antérieur à #2290 — il ne venait pas du drainage, mais il ne
/// se voyait pas tant que personne ne regardait la chaîne complète.
///
/// ⚠️ R1 (#2219) a supprimé la duplication que ce test comptait. Les deux
/// sites existent toujours — amorce de la piste chaînée et boucle de lecture —
/// mais ils ne recopient plus la chaîne : ils passent tous deux par
/// `EtageDeConversion::pousser`, seule route vers le puits. On ne compte donc
/// plus des copies, on verrouille la route UNIQUE : c'est strictement plus
/// fort, parce qu'un troisième site ne pourrait pas la contourner.
#[test]
fn la_boucle_gapless_applique_le_dsp() {
    let src = source();
    let prod = production(&src);
    let debut = prod
        .find("local_audio_gapless_chaining_next_track")
        .expect("le point de chaînage gapless doit exister");
    let gapless = &prod[debut..];

    assert!(
        gapless.contains("etage.pousser("),
        "l'amorce de la piste chaînée doit traverser la frontière PCM qui \
         applique le DSP, sinon une correction de pièce cesse de s'appliquer \
         après la première piste d'un album (#2296/#2232)"
    );
    assert!(
        gapless.contains("producteur_enchaine.tourner(") && gapless.contains("&mut etage,"),
        "la boucle de lecture de la piste chaînée doit passer par le MÊME \
         étage de conversion que la piste initiale (#2296/#2232)"
    );

    // La route elle-même : `pousser` décode par la frontière PCM commune —
    // celle qui applique le DSP — avant d'écrire quoi que ce soit au puits.
    let pousser = prod
        .split("    fn pousser(")
        .nth(1)
        .and_then(|s| s.split("\n    }").next())
        .expect("EtageDeConversion::pousser doit rester identifiable");
    assert!(
        pousser.contains("self.decoder()") && pousser.contains("puits.ecrire("),
        "la seule route vers le puits doit décoder par la frontière PCM \
         commune AVANT d'écrire (#2296/#2232)"
    );
    let decoder = prod
        .split("    fn decoder(")
        .nth(1)
        .and_then(|s| s.split("\n    }").next())
        .expect("EtageDeConversion::decoder doit rester identifiable");
    assert!(
        decoder.contains("process_pcm_chunk("),
        "le décodage de l'étage doit rester la frontière PCM commune, celle \
         qui applique le DSP (#2296/#2232)"
    );
}

/// Le drainage appartient à la fin EFFECTIVE de la chaîne gapless, sauf quand
/// le format source change et impose de remplacer le moteur (#2210).
///
/// La présence d'un `next_media` ne prouve pas qu'une piste suivra : la requête
/// peut échouer, l'en-tête peut être vide ou non-WAV. Décider avant ces essais
/// faisait sauter le drainage sans qu'aucune piste soit finalement chaînée.
/// On verrouille donc les deux seuls cas légitimes :
///
/// - même cadence/layout : aucun drainage au milieu de la chaîne ;
/// - format différent : drainage conditionné, puis reconstruction.
///
/// Le drainage final reste après la boucle, avant le vidage du resampler, et
/// uniquement après EOF naturel.
#[test]
fn le_drainage_attend_la_fin_reelle_de_la_chaine() {
    let src = source();
    let prod = production(&src);
    let debut = prod
        .find("local_audio_gapless_chaining_next_track")
        .expect("la boucle gapless doit exister");
    let fin_chaine = prod[debut..]
        .find("End of gapless continuation")
        .map(|i| debut + i)
        .expect("la fin de la boucle gapless doit être identifiable");
    let draine = prod[fin_chaine..]
        .find("flush_local_dsp(")
        .map(|i| fin_chaine + i)
        .expect("la fin effective de chaîne doit drainer le DSP (#2295/#2296)");
    let vide_resampler = prod[draine..]
        .find("// Flush the resampler")
        .map(|i| draine + i)
        .expect("le resampler doit être vidé après la queue du DSP");
    let garde = &prod[fin_chaine..draine];

    let milieu = &prod[debut..fin_chaine];
    let garde_format = milieu
        .find("if convolver_format_changed {")
        .expect("un changement de format gapless doit être traité explicitement (#2210)");
    let draine_transition = milieu[garde_format..]
        .find("flush_local_dsp(")
        .map(|i| garde_format + i)
        .expect("l'ancien moteur doit rendre sa queue avant d'être remplacé");
    assert_eq!(
        milieu.matches("flush_local_dsp(").count(),
        1,
        "un seul drainage est permis dans la boucle : celui du changement de format"
    );
    assert!(
        !milieu[..garde_format].contains("flush_local_dsp(")
            && garde_format < draine_transition
            && milieu[draine_transition..].contains("rebuild_local_convolver("),
        "le drainage intermédiaire doit rester sous la garde de changement de format \
         et précéder la reconstruction ; à format identique il briserait le gapless (#2296)"
    );
    assert!(
        garde.contains("if http_eof")
            && garde.contains("!force_silent.load")
            && garde.contains("!device_gone.load"),
        "le drainage ne doit avoir lieu qu'après EOF naturel, jamais après \
         Stop, abort ou perte du périphérique"
    );
    assert!(
        draine < vide_resampler,
        "la queue du convolveur doit traverser le resampler AVANT son vidage ; \
         l'ordre inverse insère du silence ou jette la queue (#2295)"
    );
}

/// À cadence identique, le resampler doit conserver sa phase et son leftover
/// entre les pistes. Le vider puis le remettre à zéro ajoutait une frontière
/// artificielle précisément dans le chemin annoncé gapless.
#[test]
fn le_gapless_preserve_le_resampler_si_la_cadence_ne_change_pas() {
    let src = source();
    let prod = production(&src);
    // R1 (#2219) : le format source vit dans l'étage de conversion.
    let debut = prod
        .find("let prev_sr = etage.sample_rate")
        .expect("la transition doit mémoriser la cadence précédente");
    let fin = prod[debut..]
        .find("L'enchaînement est acquis")
        .map(|i| debut + i)
        .expect("la fin de la négociation gapless doit être identifiable");
    let transition = &prod[debut..fin];

    assert!(
        transition.contains("prev_needs_resample && (new_sr != prev_sr || !next_needs_resample)"),
        "le resampler ne doit être vidé que si la cadence change ou si la piste \
         suivante n'en a plus besoin"
    );
    assert!(
        !transition.contains(".reset()"),
        "remettre le resampler à zéro à cadence identique crée une discontinuité gapless"
    );
}

/// Aucun appel de production ne doit passer d'échantillons à un vidage.
///
/// `rubato_resample_chunk(.., flush = true, ..)` ne lit jamais son argument
/// `samples` : sa branche de vidage part de `resample_leftover`, ou de rien.
/// Le contrat est verrouillé côté moteur par
/// `audio::resample::tests::le_vidage_du_resampleur_ignore_ses_echantillons` ;
/// ici on vérifie que la chaîne locale le RESPECTE — c'est précisément ce que
/// #2290 avait enfreint, jetant la queue du convolveur sur tout chemin qui
/// rééchantillonne (#2295, JP Robbe).
#[test]
fn aucun_vidage_du_resampleur_ne_recoit_d_echantillons() {
    let src = source();
    let prod = production(&src);

    let mut sites = 0usize;
    let mut vidages = 0usize;
    let mut reste = prod;
    while let Some(pos) = reste.find("rubato_resample_chunk(") {
        let apres = &reste[pos + "rubato_resample_chunk(".len()..];
        // Refermer la parenthèse de l'appel pour isoler ses arguments.
        let mut profondeur = 1usize;
        let mut fin = 0usize;
        for (i, c) in apres.char_indices() {
            match c {
                '(' => profondeur += 1,
                ')' => {
                    profondeur -= 1;
                    if profondeur == 0 {
                        fin = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let appel = &apres[..fin];
        // Retirer les commentaires de ligne avant de découper les arguments :
        // ils contiennent des virgules.
        let nu: String = appel
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let args: Vec<&str> = nu
            .split(',')
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .collect();
        assert!(
            args.len() >= 4,
            "appel de rubato_resample_chunk à moins de 4 arguments : {appel}"
        );
        sites += 1;
        if args[3] == "true" {
            vidages += 1;
            assert_eq!(
                args[1], "&[]",
                "un vidage du resampler reçoit « {} » : ces échantillons ne \
                 seront JAMAIS lus, ils sont jetés en silence. Les traiter \
                 d'abord en flush = false, puis vider avec &[] (#2295)",
                args[1]
            );
        }
        reste = &apres[fin..];
    }

    // R1 (#2219) : les quatre appels `flush = false` de `play_url` — amorce et
    // boucle, pour la piste initiale comme pour la piste chaînée — étaient la
    // MÊME ligne recopiée. Ils n'en font plus qu'un, dans
    // `EtageDeConversion::convertir`. Le plancher descend donc de 6 à 3, et il
    // ne peut pas devenir un vert contre rien : l'assertion suivante exige que
    // l'unique appel non-vidage soit bien celui de l'étage, c'est-à-dire que
    // la baisse vienne de la centralisation et non d'une branche perdue.
    assert!(
        sites >= 3,
        "seulement {sites} appel(s) au resampler trouvé(s) : le test ne couvre \
         plus la chaîne locale"
    );
    let convertir = prod
        .split("    fn convertir(")
        .nth(1)
        .and_then(|s| s.split("\n    }").next())
        .expect("EtageDeConversion::convertir doit rester identifiable");
    assert!(
        convertir.contains("adapt_channels(") && convertir.contains("rubato_resample_chunk("),
        "l'unique conversion source → sortie doit adapter les canaux PUIS \
         rééchantillonner : c'est la ligne que les quatre sites recopiaient"
    );
    assert!(
        vidages >= 2,
        "seulement {vidages} vidage(s) trouvé(s) — les fins de piste et de \
         chaîne doivent vider le resampler ; une transition gapless à cadence \
         identique ne doit précisément PAS le vider"
    );
}
