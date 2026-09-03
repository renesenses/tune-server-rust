//! #3209 — tout site qui ouvre un peripherique local doit dire QUEL PCM.
//!
//! Sous Linux il n'y a pas de mode exclusif : ce qui decide si Tune parle au
//! DAC ou a un reechantillonneur logiciel, c'est le NOM ALSA ouvert. `hw:CARD=…`
//! atteint le pilote ; `default`, `sysdefault:`, `dmix:`, `plughw:` passent par
//! un greffon qui accepte toutes les cadences et convertit en silence.
//!
//! #1655 (v0.9.131) a fait journaliser ce nom par le chemin PCM/WAV. Le second
//! site d'ouverture — le chemin « flux compresse », emprunte quand le flux ne
//! porte pas d'en-tete WAV — ouvrait le meme peripherique sans le nommer : un
//! releve de terrain y etait aveugle. C'est ce que #3209 demandait de mesurer.
//!
//! **Ce que cette garde peut et ne peut pas faire.** Elle lit le TEXTE de
//! `tune-core/src/outputs/local.rs` (`include_str!`), elle n'execute pas une
//! ouverture ALSA : un runner GitHub n'a pas de carte son, et le code vise est
//! d'ailleurs derriere `local-audio`, que le job `Test` de `ci.yml` n'active
//! pas. `include_str!` ignore les `cfg` — c'est precisement pourquoi la garde
//! est ecrite ainsi, et c'est ce qui la rend executee par le job qui tourne sur
//! toutes les PR.
//!
//! Ce qu'elle attrape : un site d'ouverture — celui-ci ou un NOUVEAU — qui
//! resout un peripherique puis ouvre le flux sans avoir journalise le PCM.

const SOURCE: &str = include_str!("../../tune-core/src/outputs/local.rs");

/// Les offsets des APPELS a `find_device_with_fallback` — pas sa definition,
/// ni les aiguilles que les tests de `local.rs` en font.
///
/// Le filtre est le prefixe `fn ` : il ecarte d'un coup `fn
/// find_device_with_fallback(` (la definition) et `.find("fn
/// find_device_with_fallback(")` (les gardes internes, ou le nom est precede de
/// `"fn `). Ce qui reste est la liste des sites qui ouvrent reellement.
fn sites_d_ouverture() -> Vec<usize> {
    let appel = "find_device_with_fallback(";
    let mut sites = Vec::new();
    let mut depuis = 0usize;
    while let Some(rel) = SOURCE[depuis..].find(appel) {
        let pos = depuis + rel;
        let precede_de_fn = pos >= 3 && &SOURCE[pos - 3..pos] == "fn ";
        if !precede_de_fn {
            sites.push(pos);
        }
        depuis = pos + appel.len();
    }
    sites
}

/// Entre la resolution du peripherique et l'ouverture du flux, le nom de PCM
/// doit etre journalise.
///
/// La fenetre n'est pas un nombre d'octets choisi au doigt mouille : elle va du
/// site de resolution au `.build_output_stream(` qui le suit. C'est exactement
/// le segment pendant lequel le code sait QUEL peripherique il tient et ne l'a
/// pas encore ouvert. Un `endpoint_id = %` situe apres l'ouverture, ou dans un
/// tout autre chemin, ne compte donc pas.
#[test]
fn chaque_site_d_ouverture_locale_journalise_le_pcm() {
    let sites = sites_d_ouverture();
    assert!(
        sites.len() >= 2,
        "moins de deux sites d'ouverture trouves ({}) : soit `find_device_with_fallback` \
         a ete renomme, soit un chemin de lecture a disparu. Dans les deux cas cette \
         garde ne mesure plus rien — la reaccorder fait partie du changement",
        sites.len()
    );

    let ouverture = ".build_output_stream(";
    let trace = ["endpoint_id", " = %"].concat();

    for (rang, &site) in sites.iter().enumerate() {
        let reste = &SOURCE[site..];
        let fin = reste.find(ouverture).unwrap_or_else(|| {
            panic!(
                "site d'ouverture #{rang} : aucun `{ouverture}` apres la resolution du \
                 peripherique. Le flux est ouvert autrement, et cette garde ne sait plus \
                 ou s'arreter"
            )
        });
        let ligne = SOURCE[..site].lines().count();
        assert!(
            reste[..fin].contains(trace.as_str()),
            "site d'ouverture #{rang} (tune-core/src/outputs/local.rs, vers la ligne \
             {ligne}) : le peripherique est resolu puis ouvert sans que le journal dise \
             QUEL PCM. Sous Linux c'est toute la difference entre `hw:CARD=…` — le DAC — \
             et `dmix:`/`plughw:`/`default`, qui reechantillonnent en silence. Sans cette \
             ligne, aucun releve de terrain ne peut trancher (#3209, #1655)"
        );
    }
}

/// L'evenement du chemin « flux compresse » existe et porte les deux champs qui
/// le rendent exploitable : l'hote et le PCM.
///
/// La garde ci-dessus verifie la POSITION de la trace ; celle-ci verifie qu'elle
/// est nommee de facon stable, pour qu'un releve puisse la chercher.
#[test]
fn l_ouverture_du_chemin_compresse_nomme_l_hote_et_le_pcm() {
    let evenement = ["local_audio_compressed", "_open_endpoint"].concat();
    let pos = SOURCE.find(evenement.as_str()).unwrap_or_else(|| {
        panic!(
            "l'evenement `{evenement}` a disparu : le chemin « flux compresse » ouvre a \
             nouveau un peripherique ALSA sans dire lequel (#3209)"
        )
    });
    // Les champs precedent le nom de l'evenement dans une macro `info!`.
    let debut = pos.saturating_sub(400);
    let entete = &SOURCE[debut..pos];
    for champ in [
        ["backend", " = %"].concat(),
        ["endpoint_id", " = %"].concat(),
    ] {
        assert!(
            entete.contains(champ.as_str()),
            "`{evenement}` sans `{champ}` : un releve qui ne sait ni l'hote ni le PCM ne \
             dit rien de plus que « ca a joue » (#3209)"
        );
    }
}

/// Le bras NOMINAL de la decision d'ouverture journalise le PCM, lui aussi.
///
/// La garde de position ci-dessus est un test de TEXTE : elle voit bien un
/// `endpoint_id = %` entre la resolution et l'ouverture, mais elle ne peut pas
/// savoir QUEL bras du `match` s'execute. Or la decision a quatre bras, et le
/// plus frequent — `DeviceAlreadyAtSourceRate`, « le DAC est deja a la cadence
/// de la source » — n'emettait rien. Un releve de terrain n'aurait donc vu que
/// les cas anormaux : le rapport aurait sur-represente les `dmix:`/`default` et
/// sous-estime les `hw:`, c'est-a-dire exactement la mesure que #3209 demande.
///
/// La fenetre s'arrete au bras suivant : `LocalRateOpening::` reapparait a
/// chaque motif du `match`, donc un `endpoint_id` appartenant a un autre bras
/// ne peut pas rendre cette garde verte a la place du bon.
#[test]
fn le_bras_nominal_de_la_decision_journalise_aussi_le_pcm() {
    let bras = [
        "(LocalRateOpening::DeviceAlreadyAtSourceRate,",
        " Some(cfg), _) => {",
    ]
    .concat();
    let debut = SOURCE.find(bras.as_str()).unwrap_or_else(|| {
        panic!(
            "le bras `{bras}` a disparu du `match` de la decision d'ouverture : la garde \
             ne mesure plus rien, la reaccorder fait partie du changement"
        )
    });
    let apres = &SOURCE[debut + bras.len()..];
    let fin = apres
        .find("LocalRateOpening::")
        .expect("aucun bras apres le bras nominal : la forme du `match` a change");
    let trace = ["endpoint_id", " = %"].concat();
    assert!(
        apres[..fin].contains(trace.as_str()),
        "le bras NOMINAL de la decision d'ouverture (le DAC est deja a la cadence de la \
         source — le cas le plus frequent) n'ecrit pas le PCM ouvert. Les trois autres \
         bras le font depuis #1655 : un releve n'aurait donc vu que les cas anormaux, et \
         aurait sous-estime la part de `hw:` dans le parc (#3209)"
    );
}
