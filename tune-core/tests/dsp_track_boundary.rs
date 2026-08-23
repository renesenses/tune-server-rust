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

use std::path::Path;

fn source() -> String {
    std::fs::read_to_string(Path::new("src/outputs/local.rs"))
        .expect("src/outputs/local.rs doit être lisible depuis la racine du crate")
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
/// ça : c'est le NOMBRE de points d'appel qu'il faut tenir.
///
/// ⚠️ L'invariant n'est PAS « autant de drainages que d'applications du DSP ».
/// Les deux sites de la boucle gapless appliquent le DSP et ne doivent
/// SURTOUT PAS drainer : une transition gapless est un flux continu, et vider
/// le convolveur y insérerait la queue de la piste précédente (#2296). On
/// compte donc les applications qui précèdent le point de chaînage.
#[test]
fn les_chemins_de_fin_de_piste_drainent_le_convolveur() {
    let src = source();
    let prod = production(&src);
    let chainage = prod
        .find("local_audio_gapless_chaining_next_track")
        .unwrap_or(prod.len());

    let drainages = prod.matches("flush_local_dsp(").count() - 1; // moins la définition
    let applications = prod[..chainage].matches("apply_local_dsp(").count() - 1; // idem

    assert_eq!(
        drainages, applications,
        "{drainages} drainage(s) pour {applications} chemin(s) qui terminent une \
         piste : un chemin applique le convolveur sans jamais rendre ce qu'il \
         retient, donc tronque la fin de sa piste (#2209)"
    );
    assert!(
        drainages >= 4,
        "les quatre chemins de lecture locale doivent drainer, {drainages} trouvé(s)"
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
#[test]
fn la_boucle_gapless_applique_le_dsp() {
    let src = source();
    let prod = production(&src);
    let debut = prod
        .find("local_audio_gapless_chaining_next_track")
        .expect("le point de chaînage gapless doit exister");

    assert_eq!(
        prod[debut..].matches("apply_local_dsp(").count(),
        2,
        "les deux sites de la boucle gapless — premier bloc et boucle \
         principale — doivent appliquer le DSP, sinon une correction de pièce \
         cesse de s'appliquer après la première piste d'un album (#2296)"
    );
}

/// Le drainage appartient à la fin EFFECTIVE de la chaîne gapless.
///
/// La présence d'un `next_media` ne prouve pas qu'une piste suivra : la requête
/// peut échouer, l'en-tête peut être vide ou non-WAV. Décider avant ces essais
/// faisait sauter le drainage sans qu'aucune piste soit finalement chaînée.
/// On verrouille donc l'ordre réel : boucle de chaînage terminée, puis drainage,
/// puis vidage du resampler — uniquement après EOF naturel.
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

    assert!(
        !prod[debut..fin_chaine].contains("flush_local_dsp("),
        "le convolveur est drainé au milieu d'une chaîne gapless (#2296)"
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
    let debut = prod
        .find("let prev_sr = sample_rate")
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

    assert!(
        sites >= 6,
        "seulement {sites} appel(s) au resampler trouvé(s) : le test ne couvre \
         plus la chaîne locale"
    );
    assert!(
        vidages >= 2,
        "seulement {vidages} vidage(s) trouvé(s) — les fins de piste et de \
         chaîne doivent vider le resampler ; une transition gapless à cadence \
         identique ne doit précisément PAS le vider"
    );
}
