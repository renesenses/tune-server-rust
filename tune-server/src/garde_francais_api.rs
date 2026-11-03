//! Garde : aucune phrase française dans les champs d'erreur et de message de
//! l'API (#7077).
//!
//! ## Ce qu'elle interdit
//!
//! Dans le code de PRODUCTION de `tune-server/src` et `tune-core/src`, une
//! chaîne littérale française (accents français, ou au moins deux mots
//! français courants) placée :
//!
//! * comme valeur d'un champ exposé d'un `json!` — `error`, `message`,
//!   `detail`, `reason`, `warning`, `hint`, `raison`, `motif`, ou toute clé
//!   finissant par `_message`, `_error`, `_detail`, `_reason`… — directement
//!   ou dans un `format!` / un appel de méthode en position de valeur ;
//! * comme corps texte d'une réponse `(StatusCode::X, "…")` ou
//!   `(StatusCode::X, format!("…"))`.
//!
//! Ce sont les phrases que le client web recopiait à l'écran : un
//! anglophone lisait « réglage non enregistré » (testeur Alex, 26/10/2026).
//! La forme attendue est un code stable plus une clé de `i18n_server.json`,
//! via [`crate::message_api`].
//!
//! Un code machine (`zone_en_lecture`, ASCII sans espace) n'est pas une
//! phrase : il passe. Les journaux (`warn!`…), les commentaires et les
//! modules `#[cfg(test)]` ne sont pas lus.
//!
//! ## Ce qu'elle ne voit pas
//!
//! Une phrase française construite AILLEURS et qui arrive dans un corps par
//! `e.to_string()` (un `Display` d'erreur de `tune-core`, une aide `refus(…)`
//! qui reçoit sa phrase en argument). Ce chemin indirect est recensé dans
//! l'issue #7077, pas gardé ici.
//!
//! ## La liste d'exceptions
//!
//! `garde_francais_api_seconde_passe.txt` liste, une par ligne, les chaînes
//! qui restent à traiter (seconde passe de #7077), chacune avec son motif. La
//! garde rougit aussi sur une entrée qui ne correspond plus à rien : la liste
//! ne peut que diminuer, jamais servir de cache-misère à une chaîne disparue.
//!
//! ## Pourquoi elle ne se satisfait pas d'elle-même
//!
//! Elle INTERDIT (elle n'est pas satisfaite par la présence d'un motif) et
//! prouve qu'elle VOIT : elle compte les fichiers lus et les champs exposés
//! rencontrés, français ou non, et rougit sous un plancher. Son propre
//! fichier, qui cite des phrases françaises en exemple, est exclu par son
//! nom, ainsi que la liste d'exceptions ; les témoins de son détecteur
//! tournent sur des sources synthétiques.

use std::path::{Path, PathBuf};

/// Clés `json!` dont la valeur atteint l'écran.
const CLES_EXPOSEES: &[&str] = &[
    "error", "message", "detail", "reason", "warning", "hint", "raison", "motif",
];

/// Mots français courants. Deux suffisent à reconnaître une phrase sans
/// accent (« piste inconnue ou non MP3 »). Aucun n'est un mot anglais courant.
const MOTS_FRANCAIS: &[&str] = &[
    "le",
    "la",
    "les",
    "des",
    "du",
    "une",
    "est",
    "pas",
    "pour",
    "avec",
    "dans",
    "aucun",
    "aucune",
    "sur",
    "introuvable",
    "indisponible",
    "impossible",
    "echec",
    "delai",
    "depasse",
    "reessayez",
    "veuillez",
    "deja",
    "fichier",
    "dossier",
    "serveur",
    "lecture",
    "requis",
    "inconnu",
    "inconnue",
    "non",
    "cette",
    "ces",
    "sans",
    "ou",
    "et",
    "doit",
    "peut",
    "n'est",
    "n'a",
    "d'un",
    "d'une",
    "piste",
    "pistes",
];

/// Une chaîne exposée trouvée en français.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Trouvaille {
    pub ligne: usize,
    /// `json:<clé>` ou `corps-texte`.
    pub forme: String,
    pub texte: String,
}

/// Ce que l'analyse d'un fichier a vu.
#[derive(Debug, Default)]
pub(crate) struct Analyse {
    pub trouvailles: Vec<Trouvaille>,
    /// Champs exposés rencontrés, français ou non : la preuve que la garde voit.
    pub champs_exposes_vus: usize,
}

fn est_code_machine(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.:-".contains(c))
}

/// La chaîne est-elle une phrase française ?
pub(crate) fn est_francais(s: &str) -> bool {
    if est_code_machine(s) {
        return false;
    }
    // Les jetons `{…}` de format! ne comptent pas.
    let mut sans_jetons = String::with_capacity(s.len());
    let mut profondeur = 0usize;
    for c in s.chars() {
        match c {
            '{' => profondeur += 1,
            '}' => profondeur = profondeur.saturating_sub(1),
            _ if profondeur == 0 => sans_jetons.push(c),
            _ => sans_jetons.push(' '),
        }
    }
    if sans_jetons
        .chars()
        .any(|c| "éèêëàâùûçôîïœÉÈÊÀÂÇÔÎ".contains(c))
    {
        return true;
    }
    let minuscules = sans_jetons.to_lowercase();
    let mots = minuscules
        .split(|c: char| !(c.is_ascii_alphabetic() || c == '\''))
        .filter(|m| !m.is_empty());
    mots.filter(|m| MOTS_FRANCAIS.contains(m)).count() >= 2
}

fn cle_exposee(cle: &str) -> bool {
    CLES_EXPOSEES
        .iter()
        .any(|c| cle == *c || cle.ends_with(&format!("_{c}")))
}

#[derive(Debug, Clone, PartialEq)]
enum Jeton {
    Ident(String),
    Chaine,
    Ponct(char),
    Ouvre,
    Ferme,
}

struct Cadre {
    macro_: Option<String>,
    cle: Option<String>,
}

/// Analyse un source Rust (le texte d'un fichier de production).
pub(crate) fn analyser(source: &str) -> Analyse {
    let o: Vec<char> = source.chars().collect();
    let n = o.len();
    let mut i = 0usize;
    let mut ligne = 1usize;
    let mut cadres: Vec<Cadre> = Vec::new();
    let mut jetons: Vec<Jeton> = Vec::new();
    // Chaîne en attente : clé si un `:` suit, valeur sinon.
    let mut attente: Option<(usize, String, Option<String>, bool)> = None;
    let mut arme_test = false;
    let mut saut_profondeur: Option<usize> = None;
    let mut analyse = Analyse::default();

    let commence = |i: usize, motif: &str| -> bool {
        let m: Vec<char> = motif.chars().collect();
        i + m.len() <= n && o[i..i + m.len()] == m[..]
    };

    macro_rules! decider {
        ($deux_points:expr, $cadres:expr) => {
            if let Some((l, s, cle, corps_texte)) = attente.take() {
                if $deux_points {
                    if let Some(c) = $cadres.last_mut() {
                        c.cle = Some(s);
                    }
                } else if saut_profondeur.is_none() {
                    let exposee = cle.as_deref().is_some_and(cle_exposee);
                    if exposee || corps_texte {
                        analyse.champs_exposes_vus += 1;
                        if est_francais(&s) {
                            analyse.trouvailles.push(Trouvaille {
                                ligne: l,
                                forme: if exposee {
                                    format!("json:{}", cle.unwrap_or_default())
                                } else {
                                    "corps-texte".into()
                                },
                                texte: s,
                            });
                        }
                    }
                }
            }
        };
    }

    while i < n {
        let c = o[i];
        if c == '\n' {
            ligne += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if commence(i, "//") {
            while i < n && o[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if commence(i, "/*") {
            let mut prof = 0;
            while i < n {
                if commence(i, "/*") {
                    prof += 1;
                    i += 2;
                } else if commence(i, "*/") {
                    prof -= 1;
                    i += 2;
                    if prof == 0 {
                        break;
                    }
                } else {
                    if o[i] == '\n' {
                        ligne += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }
        if commence(i, "#[cfg(test)]") {
            decider!(false, cadres);
            arme_test = true;
            i += "#[cfg(test)]".len();
            continue;
        }
        // Chaînes : brutes r"…", r#"…"#, octets b"…", ordinaires "…".
        let brute = {
            let mut k = i;
            if k < n && o[k] == 'b' {
                k += 1;
            }
            if k < n && o[k] == 'r' {
                let mut d = k + 1;
                while d < n && o[d] == '#' {
                    d += 1;
                }
                (d < n && o[d] == '"').then_some((d - k - 1, d + 1))
            } else {
                None
            }
        };
        let ordinaire = if c == '"' {
            Some(i + 1)
        } else if c == 'b' && i + 1 < n && o[i + 1] == '"' {
            Some(i + 2)
        } else {
            None
        };
        if brute.is_some() || ordinaire.is_some() {
            let (texte, suite) = if let Some((dieses, debut)) = brute {
                let mut k = debut;
                loop {
                    if k >= n {
                        break;
                    }
                    if o[k] == '"' && (0..dieses).all(|j| k + 1 + j < n && o[k + 1 + j] == '#') {
                        break;
                    }
                    k += 1;
                }
                (
                    o[debut..k.min(n)].iter().collect::<String>(),
                    k + 1 + dieses,
                )
            } else {
                let debut = ordinaire.unwrap();
                let mut k = debut;
                while k < n && o[k] != '"' {
                    k += if o[k] == '\\' { 2 } else { 1 };
                }
                (o[debut..k.min(n)].iter().collect::<String>(), k + 1)
            };
            decider!(false, cadres);
            let dans_json = cadres.iter().any(|f| f.macro_.as_deref() == Some("json"));
            let mut cle = None;
            for f in cadres.iter().rev() {
                if f.cle.is_some() {
                    cle = f.cle.clone();
                    break;
                }
                if f.macro_.as_deref() == Some("json") {
                    break;
                }
            }
            let dans_assert = cadres
                .iter()
                .any(|f| f.macro_.as_deref().is_some_and(|m| m.contains("assert")));
            let t = &jetons;
            let l = t.len();
            let statut_avant = |d: usize| -> bool {
                // … StatusCode :: X ,  se terminant `d` jetons avant la fin.
                l >= d + 5
                    && t[l - d - 1] == Jeton::Ponct(',')
                    && matches!(t[l - d - 2], Jeton::Ident(_))
                    && t[l - d - 3] == Jeton::Ponct(':')
                    && t[l - d - 4] == Jeton::Ponct(':')
                    && t[l - d - 5] == Jeton::Ident("StatusCode".into())
            };
            let format_ouvert = l >= 3
                && t[l - 1] == Jeton::Ouvre
                && t[l - 2] == Jeton::Ponct('!')
                && t[l - 3] == Jeton::Ident("format".into());
            let corps_texte =
                !dans_assert && (statut_avant(0) || (format_ouvert && statut_avant(3)));
            attente = Some((
                ligne,
                texte.clone(),
                if dans_json { cle } else { None },
                corps_texte,
            ));
            ligne += texte.matches('\n').count();
            i = suite;
            jetons.push(Jeton::Chaine);
            continue;
        }
        if c == '\'' {
            // Caractère littéral ('a', '\n', '\u{..}') ; sinon durée de vie.
            let fin = if i + 2 < n && o[i + 1] != '\\' && o[i + 2] == '\'' {
                Some(i + 3)
            } else if i + 1 < n && o[i + 1] == '\\' {
                (i + 3..(i + 14).min(n))
                    .find(|&k| o[k] == '\'')
                    .map(|k| k + 1)
            } else {
                None
            };
            decider!(false, cadres);
            i = fin.unwrap_or(i + 1);
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let debut = i;
            while i < n && (o[i].is_alphanumeric() || o[i] == '_') {
                i += 1;
            }
            decider!(false, cadres);
            jetons.push(Jeton::Ident(o[debut..i].iter().collect()));
            continue;
        }
        if "([{".contains(c) {
            decider!(false, cadres);
            let l = jetons.len();
            let macro_ = match (l >= 2).then(|| (&jetons[l - 2], &jetons[l - 1])) {
                Some((Jeton::Ident(nom), Jeton::Ponct('!'))) => Some(nom.clone()),
                _ => None,
            };
            cadres.push(Cadre { macro_, cle: None });
            jetons.push(Jeton::Ouvre);
            if arme_test && c == '{' && saut_profondeur.is_none() {
                saut_profondeur = Some(cadres.len());
                arme_test = false;
            }
            i += 1;
            continue;
        }
        if ")]}".contains(c) {
            decider!(false, cadres);
            if saut_profondeur == Some(cadres.len()) {
                saut_profondeur = None;
            }
            cadres.pop();
            jetons.push(Jeton::Ferme);
            i += 1;
            continue;
        }
        if c == ':' {
            if commence(i, "::") {
                decider!(false, cadres);
                jetons.push(Jeton::Ponct(':'));
                jetons.push(Jeton::Ponct(':'));
                i += 2;
            } else {
                decider!(true, cadres);
                jetons.push(Jeton::Ponct(':'));
                i += 1;
            }
            continue;
        }
        decider!(false, cadres);
        if c == ',' {
            if let Some(f) = cadres.last_mut() {
                f.cle = None;
            }
        }
        if c == ';' && arme_test && saut_profondeur.is_none() {
            arme_test = false;
        }
        jetons.push(Jeton::Ponct(c));
        if jetons.len() > 400 {
            jetons.drain(..300);
        }
        i += 1;
    }
    decider!(false, cadres);
    analyse
}

/// Une entrée de la liste d'exceptions : fichier relatif au dépôt, début du
/// texte, motif.
#[derive(Debug)]
struct Exception {
    fichier: String,
    debut: String,
}

fn exceptions() -> Vec<Exception> {
    include_str!("garde_francais_api_seconde_passe.txt")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut p = l.splitn(3, " | ");
            let fichier = p.next().unwrap_or_default().trim().to_string();
            let debut = p.next().unwrap_or_default().trim().to_string();
            let motif = p.next().unwrap_or_default().trim();
            assert!(
                !fichier.is_empty() && !debut.is_empty() && !motif.is_empty(),
                "exception mal formée (fichier | début du texte | motif) : {l}"
            );
            Exception { fichier, debut }
        })
        .collect()
}

fn racine_du_depot() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tune-server a un parent")
        .to_path_buf()
}

fn fichiers_de_production(dossier: &Path, sortie: &mut Vec<PathBuf>) {
    let Ok(lecture) = std::fs::read_dir(dossier) else {
        return;
    };
    let mut entrees: Vec<_> = lecture.flatten().map(|e| e.path()).collect();
    entrees.sort();
    for p in entrees {
        let nom = p.file_name().and_then(|s| s.to_str()).unwrap_or_default();
        if p.is_dir() {
            if nom != "tests" {
                fichiers_de_production(&p, sortie);
            }
        } else if nom.ends_with(".rs") && !nom.contains("test") && nom != "garde_francais_api.rs" {
            sortie.push(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formes(src: &str) -> Vec<(String, String)> {
        analyser(src)
            .trouvailles
            .into_iter()
            .map(|t| (t.forme, t.texte))
            .collect()
    }

    #[test]
    fn le_detecteur_attrape_les_deux_formes_et_epargne_le_reste() {
        let src = r##"
fn a() -> Response {
    // "error": "réglage non enregistré" — un commentaire n'est pas lu.
    warn!(error = "délai dépassé", "journal_seulement");
    let code = json!({"error": "zone_en_lecture"});
    let x = json!({"error": format!("réglage non enregistré : {e}"), "zones": []});
    let y = json!({"ok": true, "message": "piste inconnue ou non MP3"});
    let z = json!({"valid": false, "validation_message": tr("k").replace("{e}", "token non configuré")});
    let w = json!({"title": "Écoute en cours", "error": "Track could not be decoded."});
    let t = (StatusCode::BAD_REQUEST, "La file d'attente est vide.").into_response();
    let u = (StatusCode::CONFLICT, format!("champ « {c} » refusé")).into_response();
    assert_eq!(s, StatusCode::OK, "le statut a changé");
    x
}
#[cfg(test)]
mod tests {
    fn b() { let _ = json!({"error": "réglage non enregistré"}); }
}
"##;
        let vus = formes(src);
        assert_eq!(
            vus,
            vec![
                ("json:error".into(), "réglage non enregistré : {e}".into()),
                ("json:message".into(), "piste inconnue ou non MP3".into()),
                (
                    "json:validation_message".into(),
                    "token non configuré".into()
                ),
                ("corps-texte".into(), "La file d'attente est vide.".into()),
                ("corps-texte".into(), "champ « {c} » refusé".into()),
            ],
            "trouvailles : {vus:#?}"
        );
    }

    #[test]
    fn un_code_machine_et_l_anglais_ne_sont_pas_du_francais() {
        assert!(!est_francais("zone_en_lecture"));
        assert!(!est_francais("artiste_inconnu_non_absorbable"));
        assert!(!est_francais("The track could not be decoded."));
        assert!(!est_francais("Server unreachable on port 445"));
        assert!(est_francais("délai dépassé"));
        assert!(est_francais("piste inconnue ou non MP3"));
    }

    /// LA garde : aucune chaîne française nouvelle dans un champ exposé.
    ///
    /// Contre-épreuve (#7077) : remettre
    /// `Json(json!({"error": format!("réglage non enregistré : {e}")}))` dans
    /// `routes/spotify_connect.rs` fait rougir ce test en nommant le fichier,
    /// la ligne et la phrase.
    #[test]
    fn aucune_phrase_francaise_dans_les_champs_exposes_de_l_api() {
        let racine = racine_du_depot();
        let mut fichiers = Vec::new();
        for d in ["tune-server/src", "tune-core/src"] {
            fichiers_de_production(&racine.join(d), &mut fichiers);
        }
        let exceptions = exceptions();
        let mut utilisees = vec![false; exceptions.len()];
        let mut fautes = Vec::new();
        let mut champs_vus = 0usize;
        for f in &fichiers {
            let rel = f
                .strip_prefix(&racine)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let source = std::fs::read_to_string(f).unwrap_or_default();
            let analyse = analyser(&source);
            champs_vus += analyse.champs_exposes_vus;
            for t in analyse.trouvailles {
                let premiere = t
                    .texte
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                match exceptions
                    .iter()
                    .position(|e| e.fichier == rel && premiere.starts_with(&e.debut))
                {
                    Some(k) => utilisees[k] = true,
                    None => fautes.push(format!("{rel}:{} [{}] « {premiere} »", t.ligne, t.forme)),
                }
            }
        }

        println!(
            "garde_francais_api : {} fichiers lus, {champs_vus} champs exposés croisés, {} exceptions",
            fichiers.len(),
            exceptions.len()
        );
        // La garde voit : elle a lu les deux caisses et croisé leurs champs.
        assert!(
            fichiers.len() > 400,
            "garde aveugle : {} fichiers de production lus seulement",
            fichiers.len()
        );
        assert!(
            champs_vus > 1000,
            "garde aveugle : {champs_vus} champs exposés croisés seulement"
        );

        let perimees: Vec<String> = exceptions
            .iter()
            .zip(&utilisees)
            .filter(|(_, u)| !**u)
            .map(|(e, _)| format!("{} | {}", e.fichier, e.debut))
            .collect();

        assert!(
            fautes.is_empty() && perimees.is_empty(),
            "\n{} phrase(s) française(s) dans un champ exposé de l'API — rendre un code \
             stable + une clé de i18n_server.json (crate::message_api), voir #7077 :\n  {}\n\
             {} exception(s) périmée(s) dans garde_francais_api_seconde_passe.txt — la \
             retirer de la liste :\n  {}\n",
            fautes.len(),
            fautes.join("\n  "),
            perimees.len(),
            perimees.join("\n  "),
        );
    }
}
