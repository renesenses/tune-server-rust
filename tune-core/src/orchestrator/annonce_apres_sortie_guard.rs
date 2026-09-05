/// Le fichier PRIVÉ de ce module de test.
///
/// ⚠️ La découpe n'est pas un détail. `include_str!` rend le fichier
/// ENTIER, module de test compris — et les motifs cherchés ci-dessous
/// figurent aussi, mot pour mot, dans les messages d'assertion. Un
/// `code_de_production().contains(...)` sur le fichier complet se trouve donc lui-même
/// et rend vrai quoi qu'il arrive.
///
/// C'est vécu : la première version de ce garde-fou a survécu au sabotage
/// de la condition qu'elle prétendait garder. Un contrôle qui ne peut pas
/// dire non ne contrôle rien (#2082).
fn code_de_production() -> &'static str {
    static PRODUCTION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PRODUCTION.get_or_init(|| {
        const TOUT: &str = include_str!("../orchestrator.rs");
        const BORNE: &str = "mod annonce_apres_sortie_guard";
        let fin = TOUT
            .find(BORNE)
            .unwrap_or_else(|| panic!("ce module a été renommé : la découpe ne protège plus rien"));
        // Le bloc `impl PlaybackOrchestrator` est réparti par familles (REF-2,
        // #2219) : le transport (`play_inner`, chemin de lecture), la
        // résolution locale (cache hit, passthrough) et le commun
        // (`confirmer_lecture_navigateur`) vivent dans leurs propres modules.
        // Le transport se lit EN PREMIER : dans le fichier d'origine
        // `play_inner` précédait `confirmer_lecture_navigateur`, et un test
        // compare des positions.
        format!(
            "{}{}{}{}",
            include_str!("../orchestrator/transport.rs"),
            &TOUT[..fin],
            include_str!("../orchestrator/resolve_local.rs"),
            include_str!("../orchestrator/commun.rs")
        )
    })
}

/// Position de la première occurrence, ou panique avec un message qui dit
/// quoi chercher — un garde-fou muet sur son propre désaccordage ne garde
/// rien.
fn position(motif: &str) -> usize {
    code_de_production().find(motif).unwrap_or_else(|| {
        panic!(
            "motif introuvable dans orchestrator.rs : « {motif} ».\n\
             Le code a été remanié ; ce garde-fou ne garde plus rien tant \
             qu'il n'a pas suivi. Voir #1998."
        )
    })
}

/// `output_sent` doit être CONNU avant qu'on annonce quoi que ce soit.
#[test]
fn l_annonce_vient_apres_le_resultat_de_la_sortie() {
    let resultat = position("let (output_sent, output_error) =");
    let annonce = position("self.dispatch_now_playing(");
    assert!(
        resultat < annonce,
        "`dispatch_now_playing` est appelé AVANT que `output_sent` soit \
         connu : une sortie en échec annoncera de nouveau une écoute qui \
         n'a pas eu lieu (#1998)."
    );
}

/// Et il doit être CONSULTÉ, pas seulement connu.
#[test]
fn l_annonce_est_conditionnee_a_output_sent() {
    assert!(
        code_de_production().contains("if output_sent {\n            self.dispatch_now_playing("),
        "`dispatch_now_playing` n'est plus gardé par `if output_sent` — \
         l'annonce « en écoute » repartirait sur un envoi refusé (#1998)."
    );
}

/// Un cache hit doit attacher des niveaux, comme le transcodage frais.
///
/// Le chemin du transcodage frais émet ses fenêtres depuis le `pcm_bytes`
/// que lui rend `transcode_source_to_file`. Un cache hit saute tout le
/// décodage : plus une seule fenêtre ne passe par là. Tant que sa branche
/// n'attachait pas son propre forwarder, les aiguilles tombaient à zéro et
/// le spectrogramme restait plat — dès la DEUXIÈME écoute d'une piste, la
/// première ayant rempli le cache.
///
/// Invisible à la lecture comme à la compilation : deux branches correctes
/// dont une seule alimente les niveaux, séparées par cent lignes. Et
/// invisible au testeur aussi, puisque la panne suit la mise en cache et
/// non le fichier. Journaux d'Yves Corbat du 01/09/2026 : 7 des 8 lectures
/// de « Topography of Mind » sont des cache hits, toutes sans niveaux.
///
/// Le garde tient sur la TRANCHE de la branche de cache — de son propre
/// journal jusqu'à celui du transcodage frais. Chercher
/// `spawn_local_file_levels_decode` dans tout le fichier serait satisfait
/// par les autres chemins qui l'appellent déjà.
///
/// Ce garde-fou exige la fonction BRIDÉE, pas un forwarder nu. Sa première
/// version demandait `spawn_paced_levels_forwarder` : elle était satisfaite
/// par le bloc en ligne qui recopiait la forme du décodage sans son frein,
/// et laissait donc passer la régression que ce garde prétendait couvrir.
/// La mesure qui va avec est
/// `la_rendition_en_cache_ne_retient_plus_toute_la_piste`.
#[test]
fn un_cache_hit_attache_aussi_les_niveaux() {
    let debut = position("\"transcode_cache_hit\"");
    let fin = position("\"transcode_to_temp_file_start\"");
    assert!(
        debut < fin,
        "les deux branches ont été réordonnées : la découpe ne délimite \
         plus la branche de cache, ce garde-fou ne garde plus rien."
    );
    let tranche = &code_de_production()[debut..fin];
    assert!(
        tranche.contains("spawn_local_file_levels_decode("),
        "la branche « cache hit » n'attache plus de niveaux par la fonction \
         bridée : soit les VU-mètres retombent morts dès la deuxième écoute, \
         soit — si le décodage a été réécrit en ligne — il repart sans frein \
         et la file du forwarder retient tout le PCM de la piste."
    );
    assert!(
        !tranche.contains("spawn_paced_levels_forwarder"),
        "la branche « cache hit » rebranche un forwarder à la main : c'est \
         la forme qui avait perdu le frein en route. Elle doit passer par \
         `spawn_local_file_levels_decode`, qui porte le bridage."
    );
}

/// #3145 : le décodage-pour-niveaux du PASSTHROUGH est bridé, ET il décode
/// toujours aux valeurs TAGUÉES de la piste.
///
/// Deux propriétés que la compilation ne peut pas tenir, sur la même
/// tranche, parce qu'elles se contredisent en apparence :
///
/// 1. **Le frein.** Le bloc d'origine (#1423) drainait son puits sans
///    condition. Le décodage courait alors à la vitesse du DISQUE pendant
///    que le forwarder ne publie qu'au temps réel, et sa file — non bornée,
///    chaque fenêtre portant son PCM — retenait la piste ENTIÈRE. La mesure
///    qui va avec est `le_passthrough_ne_retient_plus_toute_la_piste`.
/// 2. **Les valeurs de décodage.** La façon la plus courte de freiner
///    serait d'appeler `spawn_local_file_levels_decode`, la jumelle bridée
///    — mais elle décode au débit NATIF, pas au tag, et le passthrough est
///    le chemin des fichiers mal tagués. La mesure qui va avec est
///    `les_valeurs_taguees_ne_sont_pas_le_debit_natif`.
///
/// Sans la seconde assertion, ce garde serait satisfait par la « correction »
/// qui change en silence ce que les VU-mètres décrivent.
#[test]
fn le_passthrough_est_bride_sans_changer_ce_qu_il_decode() {
    let debut = position("let skip_passthrough_levels");
    let fin = position("\"passthrough_levels_decode_failed\"");
    assert!(
        debut < fin,
        "la découpe ne délimite plus le décodage-pour-niveaux du \
         passthrough : ce garde-fou ne garde plus rien."
    );
    let tranche = &code_de_production()[debut..fin];
    assert!(
        tranche.contains("spawn_braked_levels_sink("),
        "le décodage-pour-niveaux du passthrough n'est plus bridé : sa file \
         de fenêtres retient de nouveau tout le PCM de la piste, et la \
         rétention suit la DURÉE du morceau (#3145)."
    );
    assert!(
        !tranche.contains("while sink_rx.recv().await.is_some() {}"),
        "le puits du passthrough draine de nouveau SANS CONDITION : c'est \
         exactement la forme qui n'a jamais eu de frein depuis #1423."
    );
    assert!(
        tranche.contains("Some(sr),") && tranche.contains("Some(ch),"),
        "le passthrough ne décode plus aux valeurs TAGUÉES de la piste. \
         Freiner ne doit pas changer ce qui est MESURÉ : sur un fichier mal \
         tagué — la population même du passthrough — le débit natif et le \
         tag divergent (#3145)."
    );
}

/// L'historique local souffrait du même défaut. C'était la question laissée
/// ouverte par le ticket ; la réponse est oui, et elle est corrigée ici.
#[test]
fn l_historique_local_est_conditionne_lui_aussi() {
    assert!(
        code_de_production().contains("if output_sent && record_history"),
        "`record_listen` n'est plus gardé par `output_sent` : \
         `listen_history` se remplirait de titres jamais joués (#1998)."
    );
}

/// Le scrobble DÉFINITIF n'a jamais été concerné — il part du poller, une
/// fois le seuil des 50 % / 4 min franchi. Ce test épingle cette séparation
/// pour que personne ne la « répare » en le ramenant au démarrage : c'est
/// précisément ce que #1113 avait défait.
#[test]
fn le_scrobble_definitif_reste_hors_du_demarrage() {
    let play = position("async fn play_inner(");
    let src = code_de_production();
    let apres = &src[play..];
    // Depuis REF-4 (#2219) `play_inner` est découpé en six temps qui le
    // suivent immédiatement dans `transport.rs` : la fenêtre court jusqu'à la
    // méthode d'après, pour que le chemin de démarrage ENTIER reste couvert.
    let fin = apres
        .find("\n    /// Recreate a local (cpal) output on demand and play to it.")
        .unwrap_or_else(|| {
            panic!(
                "`recreate_local_and_play` a bougé : la fenêtre du scrobble ne \
                 se ferme plus, ce garde-fou ne garde rien tant qu'il n'a pas suivi."
            )
        });
    assert!(
        !apres[..fin].contains("dispatch_scrobble("),
        "le scrobble définitif est reparti dans le chemin de démarrage : \
         il scrobblerait un titre à la seconde où il commence, en ignorant \
         la règle des 50 % / 4 min de Last.fm (#1113)."
    );
}

/// Corps d'une méthode, de sa signature jusqu'à son accolade fermante au
/// même niveau d'indentation. Sert à vérifier une propriété DANS une
/// fonction sans que le reste du fichier puisse la satisfaire à sa place.
fn corps_de(signature: &str) -> &'static str {
    let debut = position(signature);
    let apres = &code_de_production()[debut..];
    let fin = apres
        .find("\n    }\n")
        .map(|i| i + 7)
        .unwrap_or(apres.len());
    &apres[..fin]
}

/// La zone navigateur n'a PAS de périphérique : `output_sent` y vaut
/// toujours faux. La garde ci-dessus lui avait donc supprimé toute annonce,
/// y compris quand elle joue — c'est la régression pour laquelle #1998 a
/// été rouvert. Son annonce doit être DIFFÉRÉE, pas supprimée.
#[test]
fn la_zone_navigateur_ne_perd_pas_son_annonce() {
    assert!(
        code_de_production().contains("if !output_sent && zone_navigateur {"),
        "le démarrage ne met plus rien en attente pour une zone navigateur : \
         elle ne scrobblerait plus RIEN, même en jouant (#1998, réouverture \
         du 22/08). La sortie d'une zone navigateur est l'onglet, pas un \
         appareil."
    );
}

/// Et cette annonce différée ne part que sur PREUVE : des octets réellement
/// tirés de la session de flux. Pas sur l'intention de jouer.
#[test]
fn l_annonce_navigateur_suit_la_preuve_de_lecture() {
    let corps = corps_de("pub async fn confirmer_lecture_navigateur(");
    let preuve = corps
        .find(".stream_bytes_sent(stream_id)")
        .unwrap_or_else(|| {
            panic!(
                "`confirmer_lecture_navigateur` n'interroge plus les octets tirés : \
             elle annoncerait une écoute de zone navigateur sans preuve, ce que \
             #1998 reproche au démarrage."
            )
        });
    let annonce = corps.find("self.dispatch_now_playing(").unwrap_or_else(|| {
        panic!("`confirmer_lecture_navigateur` n'annonce plus rien du tout (#1998)")
    });
    assert!(
        preuve < annonce,
        "l'annonce de zone navigateur part AVANT la preuve que l'onglet tire \
         le flux : c'est très exactement le défaut d'origine, déplacé (#1998)."
    );
}

/// L'historique local de zone navigateur suit la même preuve, et garde le
/// drapeau `record_history` du démarrage : une re-création de flux pour une
/// piste déjà en cours (recherche de position) ne doit pas doublonner.
#[test]
fn l_historique_navigateur_garde_record_history() {
    assert!(
        corps_de("pub async fn confirmer_lecture_navigateur(")
            .contains("if attente.record_history && attente.source != \"radio\" {"),
        "l'historique de zone navigateur ne consulte plus `record_history` : \
         déplacer le curseur ajouterait une ligne à chaque fois (#1998)."
    );
}
