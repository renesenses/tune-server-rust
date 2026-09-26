//! Crédits d'une piste Qobuz, lus dans la chaîne de rôles `performers`
//! (#4993, FabienM, fil forum 1921).
//!
//! Qobuz joint à une piste (`/track/get`, et chaque item de `/album/get`) une
//! chaîne « Nom, Rôle1, Rôle2 - Nom2, Rôle3 ». `qobuz.rs` la lisait déjà, mais
//! seulement pour choisir l'artiste à afficher (#1407) ; le reste n'était rendu
//! nulle part. Ce module la traduit en lignes de crédits dans le vocabulaire
//! de `track_credits` (celui de [`crate::metadata::credits_mb`]), pour que le
//! client affiche les crédits d'une piste de service avec le même tiroir que
//! ceux d'une piste de bibliothèque.
//!
//! Analyse HORS RÉSEAU : tout se teste sur des chaînes.
//!
//! ## Ce qui est prouvé, et ce qui ne l'est pas
//!
//! Les formes de chaîne que le dépôt connaît (tests de #1407) portent
//! `Composer`, `ComposerLyricist`, `MainArtist`, un instrument (`Piano`) et
//! `Orchestra`. Les autres rôles de la table (`Producer`, `MixingEngineer`…)
//! sont un vocabulaire ATTENDU, pas relevé sur une réponse enregistrée : un
//! rôle inconnu n'est pas perdu, il est écrit comme instrument d'un
//! interprète, sous son libellé normalisé.

use crate::metadata::credits_mb::LigneCredit;
use crate::metadata::instruments::{canoniser_instrument, est_un_instrument_connu, normaliser};

/// Ce que devient un rôle Qobuz.
enum Traduction {
    /// Un ou deux rôles canoniques, sans instrument (sauf la voix).
    Roles(&'static [&'static str]),
    /// Rôle sans intérêt sur une fiche de crédits (éditeur, label…).
    Ignore,
}

/// Rôle Qobuz → rôle(s) de `track_credits`. La clé est le rôle NORMALISÉ puis
/// débarrassé de ses espaces (« Mixing Engineer » et « MixingEngineer » se
/// rejoignent). `None` : le rôle n'est pas dans la table.
fn traduire(role: &str) -> Option<Traduction> {
    let cle = normaliser(role).replace(' ', "");
    Some(match cle.as_str() {
        "mainartist" => Traduction::Roles(&["artist"]),
        "featuredartist"
        | "associatedperformer"
        | "performer"
        | "soloist"
        | "orchestra"
        | "ensemble" => Traduction::Roles(&["performer"]),
        "vocals" | "vocal" | "vocalist" | "leadvocals" | "leadvocalist" | "backingvocals"
        | "backgroundvocalist" | "backgroundvocals" | "singer" | "choir" | "chorus" => {
            Traduction::Roles(&["vocal"])
        }
        "composer" => Traduction::Roles(&["composer"]),
        "composerlyricist" => Traduction::Roles(&["composer", "writer"]),
        "lyricist" | "author" | "writer" | "librettist" | "songwriter" => {
            Traduction::Roles(&["writer"])
        }
        "conductor" => Traduction::Roles(&["conductor"]),
        "producer" | "coproducer" | "executiveproducer" | "associateproducer" => {
            Traduction::Roles(&["producer"])
        }
        "mixer" | "mixingengineer" | "mixengineer" => Traduction::Roles(&["mixer"]),
        "masteringengineer" | "mastering" => Traduction::Roles(&["mastering"]),
        "engineer" | "recordingengineer" | "soundengineer" | "audioengineer"
        | "assistantengineer" | "studiopersonnel" => Traduction::Roles(&["engineer"]),
        "arranger" | "orchestrator" => Traduction::Roles(&["arranger"]),
        "remixer" => Traduction::Roles(&["remixer"]),
        "programmer" | "programming" => Traduction::Roles(&["programming"]),
        "musicpublisher" | "publisher" | "label" | "copyright" | "distributor" => {
            Traduction::Ignore
        }
        _ => return None,
    })
}

/// Un segment qui clôt le nom : rôle de la table ou instrument connu.
fn est_un_role(segment: &str) -> bool {
    traduire(segment).is_some() || est_un_instrument_connu(segment)
}

/// Sépare une entrée « Nom, Rôle1, Rôle2 » en nom et rôles.
///
/// Le nom s'arrête au premier segment reconnu comme rôle : « Blood, Sweat &
/// Tears, MainArtist » garde son nom entier. Sans aucun segment reconnu, le
/// nom est le premier segment et le reste est pris pour des rôles.
fn nom_et_roles(entree: &str) -> Option<(String, Vec<&str>)> {
    let segments: Vec<&str> = entree
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return None;
    }
    let fin_du_nom = (1..segments.len())
        .find(|&i| est_un_role(segments[i]))
        .unwrap_or(1);
    let nom = segments[..fin_du_nom].join(", ");
    Some((nom, segments[fin_du_nom..].to_vec()))
}

/// Les lignes de crédits d'une chaîne `performers`, dans l'ordre de la chaîne,
/// sans doublon (même nom, même rôle, même instrument).
///
/// Un intervenant sans aucun rôle est écrit `performer` : Qobuz l'a nommé, le
/// taire ferait disparaître quelqu'un de la fiche.
pub fn lignes_performers(performers: &str) -> Vec<LigneCredit> {
    let mut out: Vec<LigneCredit> = Vec::new();
    let mut pousser = |nom: &str, role: &str, instrument: Option<String>| {
        let ligne = LigneCredit {
            artist_name: nom.to_string(),
            role: role.to_string(),
            instrument,
            artist_mbid: None,
        };
        if !out.contains(&ligne) {
            out.push(ligne);
        }
    };
    for entree in performers.split(" - ") {
        let Some((nom, roles)) = nom_et_roles(entree) else {
            continue;
        };
        if roles.is_empty() {
            pousser(&nom, "performer", None);
            continue;
        }
        for role in roles {
            match traduire(role) {
                Some(Traduction::Ignore) => {}
                Some(Traduction::Roles(canons)) => {
                    for canon in canons {
                        // Comme `credits_mb` : un chant sans attribut porte
                        // l'instrument `vocals`.
                        let instrument = (*canon == "vocal").then(|| "vocals".to_string());
                        pousser(&nom, canon, instrument);
                    }
                }
                None => {
                    let instrument = canoniser_instrument(role);
                    if !instrument.is_empty() {
                        pousser(&nom, "performer", Some(instrument));
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triplets(performers: &str) -> Vec<(String, String, Option<String>)> {
        lignes_performers(performers)
            .into_iter()
            .map(|l| (l.artist_name, l.role, l.instrument))
            .collect()
    }

    fn t(nom: &str, role: &str, instrument: Option<&str>) -> (String, String, Option<String>) {
        (nom.into(), role.into(), instrument.map(Into::into))
    }

    /// La chaîne des tests de #1407 : compositeur, puis pianiste principale.
    #[test]
    fn compositeur_et_interprete_avec_instrument() {
        assert_eq!(
            triplets("Frédéric Chopin, Composer - Martha Argerich, Piano, MainArtist"),
            vec![
                t("Frédéric Chopin", "composer", None),
                t("Martha Argerich", "performer", Some("piano")),
                t("Martha Argerich", "artist", None),
            ]
        );
    }

    #[test]
    fn compositeur_parolier_et_orchestre() {
        assert_eq!(
            triplets(
                "John Williams, Composer, ComposerLyricist - Boston Pops Orchestra, Orchestra, MainArtist"
            ),
            vec![
                t("John Williams", "composer", None),
                t("John Williams", "writer", None),
                t("Boston Pops Orchestra", "performer", None),
                t("Boston Pops Orchestra", "artist", None),
            ]
        );
    }

    /// Un nom qui contient une virgule reste entier (cas de #1407).
    #[test]
    fn un_nom_avec_virgule_reste_entier() {
        assert_eq!(
            triplets("Blood, Sweat & Tears, MainArtist"),
            vec![t("Blood, Sweat & Tears", "artist", None)]
        );
    }

    /// Rôles de production : vocabulaire attendu, pas relevé sur une réponse
    /// enregistrée (voir l'en-tête du module).
    #[test]
    fn roles_de_production_et_editeur_ignore() {
        assert_eq!(
            triplets(
                "Agnes Obel, Producer, Mixing Engineer - Jane Doe, MasteringEngineer - Warner Chappell, MusicPublisher"
            ),
            vec![
                t("Agnes Obel", "producer", None),
                t("Agnes Obel", "mixer", None),
                t("Jane Doe", "mastering", None),
            ]
        );
    }

    #[test]
    fn un_role_inconnu_devient_un_instrument_normalise() {
        assert_eq!(
            triplets("Clara Rockmore, Theremin"),
            vec![t("Clara Rockmore", "performer", Some("theremin"))]
        );
    }

    #[test]
    fn chaine_vide_aucune_ligne() {
        assert!(lignes_performers("").is_empty());
        assert!(lignes_performers("  ").is_empty());
    }
}
