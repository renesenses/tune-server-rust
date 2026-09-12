//! #3233 — la garde DoP est-elle encore APPELÉE, et toujours AVANT la
//! conversion ? Le témoin de site qui manquait depuis la v0.9.141.
//!
//! # Ce que ce fichier verrouille, et pourquoi il a fallu l'écrire
//!
//! `refuser_le_porteur_dop` a été livrée par la PR #3566 (v0.9.141), vérifiée
//! dans le dépôt le 08/09 puis **dans le binaire publié** le 09/09
//! (`local_audio_dop_carrier_destroyed_by_resample` : 0 en v0.9.140, 1 en
//! v0.9.144). À chaque ronde, le même verdict a été posé, et il était juste :
//!
//! > « La contre-épreuve est NÉGATIVE. Neutraliser la garde ne fait rougir
//! > aucun test. Les témoins de #3510 couvrent la *fonction pure*
//! > `rupture_du_porteur_dop`, pas ses QUATRE sites d'appel. On peut retirer
//! > les quatre `if refuser_le_porteur_dop(...)` et la suite reste verte. Une
//! > garde sans témoin de site est écrite, pas prouvée. »
//!
//! Mesure au tag publié, qui dit la même chose d'un mot :
//!
//! ```text
//! $ git grep -c refuser_le_porteur_dop v0.9.145
//! v0.9.145:tune-core/src/outputs/local.rs:5
//! ```
//!
//! **Une seule ligne de sortie : le fichier de production.** Aucun fichier de
//! test du dépôt ne nommait la garde. C'est ce trou-ci que ce fichier ferme,
//! et rien d'autre : il ne change aucun comportement audio.
//!
//! # Pourquoi une garde de SITE par `include_str!` et non un test d'exécution
//!
//! `tune-core/src/outputs/local.rs` vit derrière la feature `local-audio`, que
//! le job `Test` de `ci.yml` n'active pas
//! (`--no-default-features --features oaat,cloud-relay,bandcamp`) ; les deux
//! jobs qui l'activent sont conditionnés à `full` et ne sont donc jamais joués
//! sur une PR vers `batch/*`. Un test qui compilerait ce module serait **vert
//! contre rien** (#2816, « témoin endormi »). Lire le *texte* du fichier
//! échappe aux `cfg` : cette garde-ci s'exécute dans le job `Test`.
//!
//! Idiome du dépôt : `refus_de_peripherique_partage_dit_pourquoi.rs`,
//! `la_decision_de_cadence_est_branchee_sur_le_chemin_reel`
//! (`outputs/local/tests.rs`), `terminologie_eq.rs`.
//!
//! # Ce que la garde ne prouve PAS
//!
//! Elle prouve que le code appelle la garde avant la conversion. Elle ne
//! prouve **pas** le cas de Pierre M : WASAPI ne compile pas sur Shrek, la CI
//! compile Windows + ASIO mais **compiler n'est pas exécuter**, et son
//! `dsd_mode` de zone comme la cadence acceptée par son DAC manquent toujours.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

const LOCAL_RS: &str = include_str!("../../tune-core/src/outputs/local.rs");

/// La production seule : `local.rs` se termine par `#[cfg(test)] mod tests`,
/// dont le texte citerait nos propres motifs et rendrait la garde complaisante.
fn production() -> &'static str {
    let fin = LOCAL_RS
        .find("#[cfg(test)]\nmod tests")
        .expect("local.rs doit garder son `#[cfg(test)] mod tests` en fin de fichier");
    &LOCAL_RS[..fin]
}

/// Le texte sans commentaires ni blancs.
///
/// Retirer les blancs fait survivre la garde à un passage de `rustfmt` qui
/// recasserait les lignes — même idiome que
/// `les_quatre_charges_utiles_de_zone_appellent_le_contrat`. Retirer les
/// commentaires AVANT est ce qui permet aux quatre sites de garder la ligne
/// `// #3233 : …` qui les explique : sans ce passage, le commentaire se
/// collerait au code et l'aiguille ne se retrouverait plus.
///
/// `://` est épargné pour ne pas amputer une URL ou un chemin de greffon ALSA.
fn sans_commentaires_ni_blancs(source: &str) -> String {
    let mut assemble = String::with_capacity(source.len());
    for ligne in source.lines() {
        let mut garde = ligne;
        let mut depart = 0usize;
        while let Some(relatif) = ligne[depart..].find("//") {
            let absolu = depart + relatif;
            if absolu > 0 && ligne.as_bytes()[absolu - 1] == b':' {
                depart = absolu + 2;
                continue;
            }
            garde = &ligne[..absolu];
            break;
        }
        assemble.push_str(garde);
        assemble.push('\n');
    }
    assemble.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Le CORPS de la fermeture `refuser_le_porteur_dop`, et rien d'autre.
///
/// Chercher ses lignes dans le fichier entier ne prouverait rien :
/// `force_silent.store(true, …)`, `dop_active.store(false, …)` et le canal
/// `open_failure` vivent à plusieurs autres endroits de `local.rs`. Une
/// fermeture vidée resterait alors verte — c'est exactement ce qu'a montré la
/// contre-épreuve de ce fichier.
fn corps_de_la_fermeture() -> &'static str {
    let production = production();
    let debut = production
        .find("let refuser_le_porteur_dop = |")
        .expect("#3233 — la fermeture `refuser_le_porteur_dop` a disparu de `local.rs`");
    let corps = &production[debut..];
    // La fermeture est déclarée à 12 espaces d'indentation : elle se referme
    // sur `};` à la même colonne, et c'est la première occurrence de ce motif.
    let fin = corps
        .find("\n            };")
        .expect("la fermeture doit se refermer à son indentation de déclaration");
    &corps[..fin]
}

/// Les quatre sites, écrits comme un seul motif — et assemblés à l'exécution.
///
/// Écrit en clair, ce motif figurerait dans CE fichier ; il n'y serait pas lu
/// (la garde lit `local.rs`, pas sa propre source), mais l'idiome du dépôt
/// assemble par principe : un jour où quelqu'un déplacera la garde dans
/// `local.rs` lui-même, la précaution vaudra.
fn motif_du_site() -> String {
    [
        "ifrefuser_le_porteur_dop(processed.dop,sample_rate,channels){",
        "ifplay_generation.load(Ordering::SeqCst)==my_generation{",
        "playing.store(false,Ordering::SeqCst);",
        "}return;}",
        // Ce qui SUIT est la moitié qui compte : la garde doit précéder
        // l'adaptation de canaux et le rééchantillonnage, jamais les suivre.
        "ifneeds_channel_adapt{",
    ]
    .concat()
}

/// LE verrou : quatre sites, et chacun placé AVANT la conversion.
///
/// Compter les appels ne suffirait pas. Le défaut que #3233 décrit n'est pas
/// « la garde manque » mais « le porteur DoP est détruit avant qu'on le
/// refuse » : une garde déplacée SOUS `needs_resample` compterait pareil et ne
/// protégerait plus rien. Le motif enferme donc l'ordre.
#[test]
fn les_quatre_sites_refusent_le_porteur_dop_avant_toute_conversion() {
    let source = sans_commentaires_ni_blancs(production());
    let sites = source.matches(&motif_du_site()).count();
    assert_eq!(
        sites, 4,
        "#3233 — le chemin cpal PARTAGÉ compte quatre entrées PCM (piste \
         initiale WAV, piste initiale compressée, et les deux entrées de la \
         piste enchaînée sans blanc). Chacune doit refuser le porteur DoP \
         AVANT `adapt_channels` et `rubato_resample_chunk`, et rendre la main \
         en arrêtant la zone. J'en compte {sites}. Un site retiré, déplacé \
         sous la conversion, ou dont les arguments ont changé, rend le DAC \
         muet sans un mot : le sinc réécrit le marqueur 0x05/0xFA du porteur, \
         le DAC quitte le mode DSD et le temps défile (Pierre M, fil 1043)."
    );
}

/// La garde ne doit pas seulement être appelée : quand elle refuse, elle doit
/// DIRE pourquoi et couper le son plutôt que d'envoyer du bruit au DAC.
///
/// C'est la définition de la fermeture, celle que les quatre sites partagent.
/// Sans ce second verrou, une fermeture vidée de son corps laisserait les
/// quatre appels en place et le test ci-dessus vert.
///
/// ⚠️ La contre-épreuve de ce test a d'abord été NÉGATIVE, et pour la raison
/// la plus banale qui soit : cherchées dans le fichier ENTIER, les aiguilles
/// se trouvaient ailleurs. `force_silent.store(true, …)` existe à deux autres
/// endroits de `local.rs` — retirer la ligne de la fermeture ne changeait rien
/// au verdict. La recherche est donc bornée au CORPS de la fermeture, et rien
/// qu'à lui.
#[test]
fn la_fermeture_refusante_journalise_force_le_silence_et_retombe_le_dop() {
    let source = sans_commentaires_ni_blancs(corps_de_la_fermeture());
    for (fragment, pourquoi) in [
        (
            "rupture.journaliser(&device_name);",
            "sans la ligne de journal, un refus est indiscernable d'un DAC muet",
        ),
        (
            "force_silent.store(true,Ordering::SeqCst);",
            "le porteur DoP refusé doit taire la sortie, pas la laisser jouer le \
             flux DSD comme du PCM — c'est le bruit blanc à pleine échelle",
        ),
        (
            "dop_active.store(false,Ordering::SeqCst);",
            "l'écran doit cesser d'annoncer un mode DSD que le DAC ne tient plus",
        ),
        (
            "*slot=Some(rupture.message_utilisateur(&device_name));",
            "sans ce canal, la zone s'arrête sans cause à l'écran (#3618)",
        ),
    ] {
        assert!(
            source.contains(fragment),
            "#3233 — la fermeture `refuser_le_porteur_dop` a perdu `{fragment}` : {pourquoi}"
        );
    }
}

/// Le retour en arrière EXACT : refuser APRÈS avoir converti.
///
/// Ce n'est pas une redite du premier test. Celui-ci rougit sur une forme que
/// le comptage laisserait passer — quatre appels toujours là, mais rangés
/// derrière le rééchantillonneur, où le porteur est déjà détruit.
#[test]
fn aucun_site_ne_refuse_le_porteur_dop_apres_le_reechantillonnage() {
    let source = sans_commentaires_ni_blancs(production());
    // ⚠️ La contre-épreuve de CE test a d'abord été NÉGATIVE : écrit sans
    // l'accolade fermante, le motif ne trouvait rien alors que la garde venait
    // d'être déplacée sous le rééchantillonneur. Le `}` qui referme
    // `if needs_resample {` s'intercale, et sans lui ce témoin était une garde
    // qui ne pouvait pas refuser. Il a été corrigé, puis re-prouvé rouge.
    for retour_en_arriere in [
        "samples=rubato_resample_chunk(&mutresampler,&samples,output_ch,false,&mutresample_leftover,);}\
         ifrefuser_le_porteur_dop(",
        "smp=rubato_resample_chunk(&mutresampler,&smp,output_ch,false,&mutresample_leftover,);}\
         ifrefuser_le_porteur_dop(",
    ] {
        assert!(
            !source.contains(retour_en_arriere),
            "#3233 — la garde est passée SOUS `rubato_resample_chunk` : le sinc a \
             déjà réécrit le porteur DoP, le refus arrive trop tard et le DAC \
             est muet quoi qu'on journalise"
        );
    }
}

/// Le TITRE de #3233 : « le filtre de cadence est TAUTOLOGIQUE quand
/// l'énumération échoue ». Sa garde de site existe — et elle ne s'exécute
/// jamais sur une PR vers `batch/*`.
///
/// # Ce que ce test ajoute, et pourquoi il n'est pas une redite
///
/// Le défaut du titre a été corrigé par `170fe51f` (« la cadence d'ouverture ne
/// se fonde plus sur des capacités supposées », PR #3252), **première version
/// publiée : v0.9.132**. Le ticket a été ouvert le 02/09 à 20 h 26 UTC ; la PR
/// #3252 a été fusionnée le 03/09 à 01 h 19 UTC, soit **moins de cinq heures
/// plus tard**, et v0.9.132 a été taguée le matin même. Aucun des trois
/// commentaires de vérification (07, 08 et 09/09) ne le relève : ils suivent
/// tous `refuser_le_porteur_dop`, qui traite un AUTRE défaut du même ticket.
///
/// Ce correctif a bien sa garde de site,
/// `la_decision_de_cadence_est_branchee_sur_le_chemin_reel`
/// (`tune-core/src/outputs/local/tests.rs`). Mais elle vit dans un module
/// derrière la feature `local-audio`, et la mesure sur `ci.yml` est sans appel :
///
/// | job | ligne | `local-audio` | joué sur une PR vers `batch/*` |
/// |---|---|---|---|
/// | `Test` | `cargo test … --no-default-features --features oaat,cloud-relay,bandcamp` | **non** | oui |
/// | `Test (jeu de fonctionnalités livré)` | `--features …,local-audio,…` | oui | **non** (`if: … full == 'true'`) |
/// | `cargo test -p tune-core --features audio-embedding` | défauts conservés, donc `local-audio` | oui | **non** (`full`) |
/// | `clippy` | `--all-targets --features …,local-audio,…` | oui | oui, mais clippy **compile sans exécuter** |
///
/// Autrement dit : la seule garde du défaut qui donne son titre à cette P0 est
/// **compilée** à chaque PR et **exécutée** à aucune. C'est la définition du
/// témoin endormi (#2816). Ce test-ci relit le même site depuis
/// `tune-server/tests`, qui tourne, lui, dans le job `Test`.
#[test]
fn la_decision_de_cadence_reste_branchee_et_le_filtre_tautologique_n_est_pas_revenu() {
    let source = sans_commentaires_ni_blancs(production());

    let appel_reel = [
        "decide_local_rate_opening(sample_rate,default_sr,",
        "enumerated.is_some(),rate_evidence,)",
    ]
    .concat();
    assert!(
        source.contains(&appel_reel),
        "#3233 — le chemin cpal partagé doit passer par `decide_local_rate_opening` \
         en lui donnant la cadence de la source, celle du périphérique, la réponse \
         de l'énumération ET la preuve qui dit ce qu'elle vaut. Sans la preuve, la \
         décision redit oui à une liste fabriquée : sur WASAPI cpal retient les 21 \
         `COMMON_SAMPLE_RATES` sans rien demander à personne, un DSD64 décodé à \
         176 400 Hz est ouvert à 176 400 Hz quoi que sache faire l'endpoint, \
         `needs_resample` reste faux et rubato ne tourne jamais (Pierre M, fil 1043)"
    );

    let court_circuit = [
        "}elseifletSome(cfg)=find_matching_config(&device,channels,sample_rate)",
        ".filter(|c|c.sample_rate==sample_rate)",
    ]
    .concat();
    assert!(
        !source.contains(&court_circuit),
        "#3233 — le filtre TAUTOLOGIQUE est redevenu la CONDITION d'ouverture : \
         `find_matching_config` recopie la cadence demandée dans le `StreamConfig` \
         qu'il rend, donc l'égalité `c.sample_rate == sample_rate` est vraie par \
         CONSTRUCTION et la branche est prise quel que soit le matériel"
    );

    assert!(
        source.contains("note_rate_decision(ObservedRate{"),
        "#3233 — une décision qui change ce qui part au DAC doit atteindre le \
         client : sans `note_rate_decision`, il ne reste que le journal"
    );
}
