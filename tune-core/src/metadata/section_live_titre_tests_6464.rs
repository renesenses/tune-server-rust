//! #6464 (FabienM, fil 2302 point 4) — la règle PROVISOIRE de la section
//! « Live » par le titre, éprouvée sur de VRAIS titres : des lives qu'elle
//! doit voir, des disques studio qu'elle ne doit pas prendre pour des lives
//! (faux positifs), et les lives qu'elle ne peut pas voir (faux négatifs
//! assumés, le titre ne le dit pas).

use super::release_type::titre_annonce_un_live;

/// Des albums live réels dont le titre le dit.
const LIVES: [&str; 14] = [
    "Live at Leeds",                 // The Who
    "MTV Unplugged in New York",     // Nirvana
    "Unplugged",                     // Eric Clapton
    "Live After Death",              // Iron Maiden
    "Live Rust",                     // Neil Young
    "Live/Dead",                     // Grateful Dead
    "Live and Dangerous",            // Thin Lizzy
    "Live Killers",                  // Queen
    "Live Evil",                     // Miles Davis
    "Live!",                         // Bob Marley & The Wailers
    "Live 1975–85",                  // Bruce Springsteen
    "Live at the Royal Albert Hall", // Adele
    "Live in Paris",                 // Diana Krall
    "Bashung (Live)",                // forme « (Live) » des services
];

/// Des disques studio ou des singles réels qui contiennent « live » comme
/// verbe, ou ces lettres dans un autre mot.
const PAS_DES_LIVES: [&str; 12] = [
    "Live Through This",       // Hole
    "Live and Let Die",        // Wings
    "Live Forever",            // Oasis
    "Long Live Rock 'n' Roll", // Rainbow
    "Live to Tell",            // Madonna
    "Live Your Life",          // T.I.
    "Live It Up",              // Crosby, Stills & Nash
    "I Want to Live",          // John Denver
    "Alive",                   // Pearl Jam
    "Lively Up Yourself",      // Bob Marley
    "Welcome to the Pleasuredome (Liverpool mix)",
    "Delivery Man", // Elvis Costello
];

/// Des lives réels que le titre ne nomme pas : la règle ne peut pas les voir,
/// seul le type secondaire `live` le peut.
const LIVES_MUETS: [&str; 3] = ["Waiting for Columbus", "Alive!", "Stop Making Sense"];

#[test]
fn les_lives_dont_le_titre_le_dit_sont_reconnus_6464() {
    for titre in LIVES {
        assert!(
            titre_annonce_un_live(titre),
            "« {titre} » est un live et son titre le dit : il doit aller dans « Live »"
        );
    }
}

#[test]
fn les_faux_positifs_ne_vont_pas_dans_live_6464() {
    for titre in PAS_DES_LIVES {
        assert!(
            !titre_annonce_un_live(titre),
            "« {titre} » n'est pas un live : « live » y est un verbe ou un \
             fragment de mot, il ne doit PAS aller dans « Live »"
        );
    }
}

#[test]
fn les_lives_muets_restent_hors_de_la_regle_6464() {
    for titre in LIVES_MUETS {
        assert!(
            !titre_annonce_un_live(titre),
            "« {titre} » ne dit pas « live » : la règle par le titre ne doit \
             pas prétendre le voir"
        );
    }
}

#[test]
fn la_casse_et_les_accents_ne_comptent_pas_6464() {
    assert!(titre_annonce_un_live("LIVE À L'OLYMPIA"));
    assert!(titre_annonce_un_live("mtv unplugged"));
    assert!(titre_annonce_un_live("Concert — live"));
}
