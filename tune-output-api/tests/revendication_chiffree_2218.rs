//! #2218 — « Aucune revendication chiffrée sans protocole, matériel et
//! distribution publiés. »
//!
//! # Ce que cette garde défend
//!
//! L'épic #2218 porte, depuis son ouverture, une case qui n'avait jamais été
//! énoncée ailleurs que dans cette case : « Aucun chiffre marketing « < 1 ms »
//! sans protocole, matériel et distribution publiés ». Les essais qui
//! fonderaient un tel chiffre — charge 1 h / 8 h / 24 h, matrice
//! ASIO/WASAPI/CoreAudio/ALSA, boucle multiroom — exigent tous du matériel, et
//! aucun n'a jamais été exécuté. Le protocole qui les décrit est désormais
//! publié :
//!
//! ```text
//! docs/mesures/2218-banc-materiel-audio.md
//! ```
//!
//! Ce fichier tient deux choses :
//!
//! 1. **que le protocole existe et reste entier** — la phrase de la règle mot
//!    pour mot, l'instrument nommé, et les six rubriques de chacun des six
//!    essais. Un protocole dont on aurait retiré « Échec » ou « Matériel » ne
//!    protocole plus rien ;
//! 2. **qu'aucune revendication chiffrée de temps ne circule sans renvoi vers
//!    lui**, dans un périmètre de fichiers nommé un à un.
//!
//! # Pourquoi du TEXTE, et pourquoi ici
//!
//! `include_str!` ignore les `cfg` et ne demande aucune caractéristique de
//! compilation. `tune-output-api` figure dans le `-p` du job `Test` de
//! `ci.yml`, qui tourne sur **toutes** les PR Rust et n'active ni
//! `local-audio` ni `audio-embedding` : la garde y est donc exécutée, et pas
//! seulement compilée. C'est le même choix que
//! `tune-server/tests/famine_pilote_3205.rs`, pour la même raison.
//!
//! C'est aussi la caisse qui **possède l'instrument** : `RingStarvation` et
//! `OutputRingStarvation` vivent dans `src/lib.rs`, à côté. Le protocole ne
//! mesure avec rien d'autre.
//!
//! # 🔴 Ce que cette garde NE couvre PAS
//!
//! Écrit ici pour que la tranche suivante sache où reprendre — et tenu à jour
//! avec le périmètre, sans quoi ce paragraphe devient un mensonge :
//!
//! * **Le périmètre est de cinq fichiers**, listés dans [`PERIMETRE`]. Tout le
//!   reste de `docs/`, tout le code Rust, les notes de version, les messages du
//!   forum, le client web (autre dépôt) et les applications natives sont
//!   **hors de portée**. Une garde large sur du texte fabrique des faux rouges,
//!   et une garde désactivée ne garde rien : le périmètre s'élargit fichier par
//!   fichier, chacun vérifié vert avant d'entrer.
//! * **Les six traductions de `docs/getting-started/`** (de, es, it, ja, ko,
//!   zh) sont hors périmètre : les marqueurs de comparaison de cette garde sont
//!   français et anglais, et les inclure donnerait une couverture imaginaire.
//! * **Le document de protocole lui-même est hors périmètre** : il est le
//!   référent, c'est là que les seuils ont le droit de vivre. Sa structure est
//!   gardée par l'autre témoin de ce fichier.
//! * **Seule la forme « moins de N unités de temps » est reconnue.** Un chiffre
//!   sans comparaison (« la latence est de 3 ms »), une plage (« 1 à 2 ms »),
//!   un pourcentage, un taux par heure, un graphique ou une image passent.
//! * **Les unités reconnues sont `ms`, `µs`, `millisecond*` et `microsecond*`.**
//!   Ni `s`, ni `sec`, ni `secondes` : trop fréquents pour un premier
//!   périmètre, et ce n'est pas l'ordre de grandeur des revendications visées.
//! * **Le renvoi est vérifié au niveau de la SECTION** (le bloc entre deux
//!   titres Markdown). Une citation du protocole dans la section suffit ; la
//!   garde ne vérifie pas que la revendication découle vraiment d'un essai
//!   consigné. Elle force à citer, elle ne force pas à avoir mesuré.
//! * **Elle ne lit aucun chiffre produit par la CI** : elle ne sait rien des
//!   artefacts de mesure, seulement du texte publié.

// ---------------------------------------------------------------------------
// Le référent
// ---------------------------------------------------------------------------

/// Le chemin du protocole, tel qu'un renvoi doit l'écrire.
const PROTOCOLE: &str = "docs/mesures/2218-banc-materiel-audio.md";

const TEXTE_PROTOCOLE: &str = include_str!("../../docs/mesures/2218-banc-materiel-audio.md");

/// La phrase qui manquait partout, mot pour mot.
const PHRASE: &str =
    "Aucune revendication chiffrée sans protocole, matériel et distribution publiés.";

/// Les six essais du protocole, et les six rubriques que chacun doit porter.
const ESSAIS: &[&str] = &["E1", "E2", "E3", "E4", "E5", "E6"];
const RUBRIQUES: &[&str] = &[
    "**Matériel**",
    "**Durée**",
    "**Stimulus**",
    "**Relevé**",
    "**Échec**",
    "**Consignation**",
];

// ---------------------------------------------------------------------------
// Le périmètre — délibérément étroit, voir l'en-tête
// ---------------------------------------------------------------------------

/// Les fichiers relus, nommés un à un. Aucun glob : un périmètre qui s'élargit
/// tout seul est un périmètre que personne n'a vérifié.
const PERIMETRE: &[(&str, &str)] = &[
    ("README.md", include_str!("../../README.md")),
    (
        "docs/getting-started/fr.md",
        include_str!("../../docs/getting-started/fr.md"),
    ),
    (
        "docs/getting-started/en.md",
        include_str!("../../docs/getting-started/en.md"),
    ),
    (
        "docs/architecture-tune-server-rust.md",
        include_str!("../../docs/architecture-tune-server-rust.md"),
    ),
    (
        "docs/mesures/3205-noyau-rt-tune-os.md",
        include_str!("../../docs/mesures/3205-noyau-rt-tune-os.md"),
    ),
    // L'INTERFACE : le catalogue de chaînes que le serveur rend à l'écran.
    (
        "tune-server/src/i18n_server.json",
        include_str!("../../tune-server/src/i18n_server.json"),
    ),
];

// ---------------------------------------------------------------------------
// Le motif
// ---------------------------------------------------------------------------

/// Unités exigeant une borne de mot à droite : `ms` et `µs` sont des fragments
/// trop courts pour être reconnus au milieu d'un mot.
const UNITES_COURTES: &[&str] = &["ms", "µs"];

/// Unités écrites en toutes lettres. Pas de borne à droite : le pluriel et la
/// forme anglaise doivent être attrapés (`millisecondes`, `milliseconds`).
const UNITES_LONGUES: &[&str] = &["millisecond", "microsecond"];

/// Les marqueurs de comparaison, en symboles puis en mots.
const SYMBOLES: &[&str] = &["<=", "≤", "<"];
const MOTS: &[&str] = &[
    "moins de",
    "sous les",
    "inférieure à",
    "inférieur à",
    "au plus",
    "pas plus de",
    "less than",
    "under",
    "below",
];

/// Le vocabulaire qui fait d'un chiffre de temps une revendication AUDIO.
/// Hors de cette liste, un `< 200 ms` parle de pagination ou de HTTP, et cette
/// garde n'a rien à en dire.
const SUJETS: &[&str] = &[
    "latence",
    "latency",
    "gigue",
    "jitter",
    "famine",
    "starvation",
    "underrun",
    "xrun",
    "sous-alimentation",
    "sous-aliment",
    "multiroom",
    "synchronis",
    "synchroniz",
    "désynchron",
    "dérive d'horloge",
    "clock drift",
];

// ---------------------------------------------------------------------------
// Le scanner
// ---------------------------------------------------------------------------

fn char_de_mot(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Recule sur les blancs, en restant sur une frontière de caractère.
fn recule_sur_espaces(texte: &str, mut pos: usize) -> usize {
    while let Some(c) = texte[..pos].chars().next_back() {
        if c.is_whitespace() {
            pos -= c.len_utf8();
        } else {
            break;
        }
    }
    pos
}

/// Le début du nombre qui se termine en `fin`, s'il y en a un.
fn debut_du_nombre(texte: &str, fin: usize) -> Option<usize> {
    let mut pos = fin;
    let mut vu_un_chiffre = false;
    while let Some(c) = texte[..pos].chars().next_back() {
        if c.is_ascii_digit() {
            vu_un_chiffre = true;
            pos -= 1;
        } else if (c == '.' || c == ',') && vu_un_chiffre {
            pos -= 1;
        } else {
            break;
        }
    }
    vu_un_chiffre.then_some(pos)
}

/// Y a-t-il un marqueur de comparaison juste avant `debut_nombre` ?
fn comparaison_avant(texte: &str, debut_nombre: usize) -> bool {
    let pos = recule_sur_espaces(texte, debut_nombre);
    let avant = &texte[..pos];
    if SYMBOLES.iter().any(|s| avant.ends_with(s)) {
        return true;
    }
    // Les marqueurs en mots, insensibles à la casse. On ne minuscule qu'une
    // courte queue : inutile de recopier tout le fichier à chaque candidat.
    let debut_queue = avant
        .char_indices()
        .rev()
        .take(24)
        .last()
        .map_or(0, |(i, _)| i);
    let queue = avant[debut_queue..].to_lowercase();
    MOTS.iter().any(|m| queue.ends_with(m))
}

/// Tous les offsets (fin de l'unité) des revendications chiffrées de temps.
fn revendications_de_temps(texte: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    for unite in UNITES_COURTES {
        collecte(texte, unite, true, &mut offsets);
    }
    for unite in UNITES_LONGUES {
        collecte(texte, unite, false, &mut offsets);
    }
    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

fn collecte(texte: &str, unite: &str, borne_a_droite: bool, offsets: &mut Vec<usize>) {
    let mut depuis = 0usize;
    while let Some(rel) = texte[depuis..].find(unite) {
        let debut = depuis + rel;
        let fin = debut + unite.len();
        depuis = fin;

        // À gauche, une LETTRE interdit : `problems`, `items`. Un chiffre est
        // au contraire la forme attendue (`200ms`), et `_` laisse passer
        // `duration_ms`, que l'absence de nombre écartera juste après.
        if texte[..debut]
            .chars()
            .next_back()
            .is_some_and(char::is_alphabetic)
        {
            continue;
        }
        if borne_a_droite && texte[fin..].chars().next().is_some_and(char_de_mot) {
            continue;
        }

        let apres_nombre = recule_sur_espaces(texte, debut);
        let Some(debut_nombre) = debut_du_nombre(texte, apres_nombre) else {
            continue;
        };
        if comparaison_avant(texte, debut_nombre) {
            offsets.push(fin);
        }
    }
}

// ---------------------------------------------------------------------------
// Le contexte
// ---------------------------------------------------------------------------

/// La ligne qui contient `offset`, pour le message d'échec.
fn ligne_de(texte: &str, offset: usize) -> &str {
    let debut = texte[..offset].rfind('\n').map_or(0, |i| i + 1);
    let fin = texte[offset..]
        .find('\n')
        .map_or(texte.len(), |i| offset + i);
    texte[debut..fin].trim()
}

/// Le paragraphe qui contient `offset` : le bloc entre deux lignes vides.
/// C'est la fenêtre dans laquelle on cherche le SUJET — assez large pour
/// couvrir un tableau Markdown avec son en-tête, assez étroite pour qu'un mot
/// croisé trois sections plus loin ne réponde pas à sa place.
fn paragraphe_de(texte: &str, offset: usize) -> &str {
    let debut = texte[..offset].rfind("\n\n").map_or(0, |i| i + 2);
    let fin = texte[offset..]
        .find("\n\n")
        .map_or(texte.len(), |i| offset + i);
    &texte[debut..fin]
}

/// La section qui contient `offset` : le bloc entre deux titres Markdown, les
/// titres factices des blocs de code exclus. Un fichier sans titre est une
/// section unique.
fn section_de(texte: &str, offset: usize) -> &str {
    let titres = titres_de(texte);
    let debut = titres.iter().copied().rfind(|&t| t <= offset).unwrap_or(0);
    let fin = titres
        .iter()
        .copied()
        .find(|&t| t > offset)
        .unwrap_or(texte.len());
    &texte[debut..fin]
}

/// Les offsets de début de chaque ligne de titre Markdown, en ignorant ce qui
/// est enfermé dans une clôture ```. Sans cette exclusion, un commentaire
/// shell en début de ligne découperait une section en deux et couperait une
/// revendication de son renvoi — un faux rouge, et c'est exactement ce qui
/// fait désactiver une garde.
fn titres_de(texte: &str) -> Vec<usize> {
    let mut titres = Vec::new();
    let mut dans_un_bloc = false;
    let mut offset = 0usize;
    for ligne in texte.split_inclusive('\n') {
        if ligne.trim_start().starts_with("```") {
            dans_un_bloc = !dans_un_bloc;
        } else if !dans_un_bloc && ligne.starts_with('#') {
            titres.push(offset);
        }
        offset += ligne.len();
    }
    titres
}

// ---------------------------------------------------------------------------
// Les témoins
// ---------------------------------------------------------------------------

/// ⭐ Le protocole existe, porte la phrase, et reste entier.
///
/// Sans ce témoin, le second garderait un renvoi vers un document qu'on
/// pourrait vider de sa substance sans que rien ne rougisse.
#[test]
fn le_protocole_du_banc_materiel_reste_entier() {
    assert!(
        TEXTE_PROTOCOLE.contains(PHRASE),
        "`{PROTOCOLE}` ne porte plus la phrase de la règle, mot pour mot :\n  \
         « {PHRASE} »\nC'est la seule chose que #2218 demandait d'énoncer une \
         fois pour de bon. Sans elle, le renvoi que cette garde exige pointe \
         vers un document qui n'affirme plus rien"
    );

    assert!(
        TEXTE_PROTOCOLE.contains("RingStarvation::snapshot"),
        "`{PROTOCOLE}` ne nomme plus `RingStarvation::snapshot`. C'est \
         l'instrument, et il est déjà branché (poller/rappel_arrete_3814.rs) : \
         un protocole qui s'appuierait sur une instrumentation à inventer ne \
         serait exécutable par personne"
    );

    for essai in ESSAIS {
        let ancre = format!("### {essai} ");
        let debut = TEXTE_PROTOCOLE.find(&ancre).unwrap_or_else(|| {
            panic!(
                "`{PROTOCOLE}` ne contient plus l'essai `{essai}` (titre attendu : \
                 `{ancre}…`). Les six essais couvrent les quatre cases matérielles \
                 de #2218 ; en retirer un laisse une case sans protocole, et c'est \
                 l'état auquel cette tranche devait mettre fin"
            )
        });
        let reste = &TEXTE_PROTOCOLE[debut + ancre.len()..];
        let fin = reste.find("\n### ").or_else(|| reste.find("\n## "));
        let section = fin.map_or(reste, |i| &reste[..i]);

        for rubrique in RUBRIQUES {
            assert!(
                section.contains(rubrique),
                "l'essai `{essai}` de `{PROTOCOLE}` ne porte plus la rubrique \
                 {rubrique}. Les six rubriques sont ce qui distingue un \
                 protocole d'une intention : le matériel exact, la durée, le \
                 stimulus, ce qu'on relève, ce qui constitue un échec, et où le \
                 résultat se consigne. Il en manque une, donc l'essai n'est plus \
                 reproductible"
            );
        }
    }
}

/// ⭐⭐ Le témoin principal : aucune revendication chiffrée de temps portant
/// sur l'audio ne circule sans renvoi vers le protocole.
#[test]
fn aucune_revendication_chiffree_sans_renvoi_au_protocole() {
    assert!(
        !PERIMETRE.is_empty(),
        "le périmètre est vide : cette garde ne lirait plus aucun fichier et \
         resterait verte contre n'importe quoi"
    );

    let mut fautes = Vec::new();

    for (chemin, texte) in PERIMETRE {
        assert!(
            !texte.trim().is_empty(),
            "`{chemin}` est vide : un fichier vide passe cette garde sans rien \
             prouver. Si le fichier a été supprimé ou déplacé, retirer aussi son \
             entrée du périmètre — et le dire dans l'en-tête"
        );

        for offset in revendications_de_temps(texte) {
            if !SUJETS
                .iter()
                .any(|s| paragraphe_de(texte, offset).contains(s))
            {
                // Un chiffre de temps qui ne parle pas d'audio : pagination,
                // requête HTTP, délai de scrutation. Hors sujet, et cette
                // garde n'a pas à s'en mêler.
                continue;
            }
            if section_de(texte, offset).contains(PROTOCOLE) {
                continue;
            }
            fautes.push(format!("  {chemin} → {}", ligne_de(texte, offset)));
        }
    }

    assert!(
        fautes.is_empty(),
        "revendication(s) chiffrée(s) de latence, de gigue, de famine ou de \
         synchronisation publiée(s) sans renvoi au protocole :\n{}\n\n\
         #2218 : « {PHRASE} »\n\n\
         Le protocole est `{PROTOCOLE}`. Deux issues, et deux seulement :\n\
         — la revendication s'appuie sur un essai consigné : citer \
         `{PROTOCOLE}` dans la section qui la porte, et publier la fiche \
         d'essai et la distribution avec le chiffre ;\n\
         — elle ne s'appuie sur rien : la retirer. Aucun essai matériel de \
         #2218 n'a été exécuté à ce jour, et une machine de compilation sans \
         carte son ne peut pas en exécuter un.",
        fautes.join("\n")
    );
}
