//! `GET|POST /library/composer-from-credits` — le compositeur des CRÉDITS
//! descend dans la colonne **et dans la balise du fichier**.
//!
//! Décision de Bertrand, 25/09/2026 : **les crédits font foi ; la colonne
//! suit, y compris en CORRIGEANT ; et la correction est écrite aussi dans le
//! fichier.**
//!
//! # Le constat qui commande la forme (mesuré sur le .18, en lecture seule)
//!
//! Le compositeur existe en **trois** porteurs qui s'ignorent :
//!
//! | porteur | état |
//! |---|---|
//! | `track_credits` à `role = 'composer'` | 114 lignes — exact et complet |
//! | `tracks.composer` | 19,4 % des pistes, depuis les balises — fautif quand il diverge |
//! | `track_metadata` clé `composer` | **0 ligne** sur 379 536 |
//!
//! Sur les 287 pistes des 29 albums dont les crédits ont été remplis le
//! 25/09 : 74 ont un crédit et **pas** de colonne, 36 ont une colonne et pas
//! de crédit (on n'y touche pas : sans crédit, rien ne fait foi), 21 sont
//! d'accord, et **9 divergent** — toujours au tort de la colonne :
//!
//! ```text
//! Wagon Wheels     colonne: "Billy Hills"    crédits: "Peter de Rose; Billy Hill"
//! Le Ballade       colonne: "Sheffield Lab"  crédits: "Robbie Buchanan"
//! Symphonie N° 3   colonne: "GORECKI"        crédits: "Henryk Mikołaj Górecki"
//! ```
//!
//! `Billy Hills` est une faute de frappe **avec un co-auteur manquant**.
//! `Sheffield Lab` est le **LABEL**, rangé dans la balise compositeur par
//! l'étiqueteur. `GORECKI` est un patronyme en capitales sans accents. Aucun
//! des trois n'est une variante défendable : ce sont des erreurs, et c'est
//! pourquoi la passe écrase au lieu de compléter.
//!
//! # 🔴 Pourquoi la BALISE, et pas seulement la base
//!
//! `update_du_scan` (lot `batch/scan-non-destructeur-20260924`) pose
//! `composer = COALESCE(NULLIF($19, ''), composer)` — « la balise gagne quand
//! elle parle ». Et `update_batch` tourne **sur tous les fichiers dès que
//! `force`/`full` est armé**, donc à chaque clic sur « Scan complet », en
//! construisant sa ligne **à partir du fichier seul**.
//!
//! Écrire seulement la base rendrait donc les **74 remplissages** durables
//! (balise vide ⇒ `NULLIF` rend `NULL` ⇒ la colonne est conservée) mais ferait
//! **effacer les 9 corrections à chaque scan complet** : la balise parle, elle
//! dit `Sheffield Lab`, elle gagne. Une passe qui se laisse défaire par le
//! bouton d'à côté n'a rien corrigé du tout.
//!
//! Bertrand a explicitement accepté que ses fichiers soient modifiés pour que
//! la correction tienne. C'est la seule raison pour laquelle cette passe
//! touche au disque.
//!
//! # 🔴 Le piège d'écriture de balises (#4238, commit `c252b7b4`)
//!
//! `write_tags` passait par `lofty::read_from_path` → `Tag` générique →
//! `save_to`. lofty 0.24 n'a pas d'`ItemKey::Unknown` :
//! `VorbisComments::split_tag` retient les champs inconnus dans un *reste* que
//! `From<VorbisComments> for Tag` **jette**. Tout champ Vorbis hors catalogue
//! disparaissait à la réécriture — `DYNAMIC RANGE`, `ALBUM DYNAMIC RANGE`,
//! `SOURCE`, et les champs maison que des testeurs posent avec Mp3tag. Ça a
//! coûté les DR de testeurs.
//!
//! Cette passe n'ouvre **aucun nouveau chemin d'écriture** : elle appelle
//! [`tune_core::metadata::tag_writer::write_tags`], qui passe par
//! `ecrire_par_tag_generique` — fichier CONCRET et couple `split_tag` /
//! `merge_tag` pour FLAC / Ogg / Opus. Le témoin
//! [`tests::un_champ_vorbis_hors_catalogue_survit_a_la_passe`] le prouve **au
//! niveau de la passe** et non du graveur : c'est ici qu'on décide d'écrire,
//! donc c'est ici qu'un futur raccourci se poserait.
//!
//! # La convention de séparateur, et sur quelle preuve
//!
//! [`SEPARATEUR`] vaut `"; "` — point-virgule + espace. Deux preuves dans ce
//! dépôt, pas un goût :
//!
//! 1. `tune_core::cloud::community_sync` écrit `values.join("; ")` dans
//!    `track_metadata` pour ses `EXTRA_KEYS`, dont **`composer`**, et relit en
//!    `value.split("; ")` sous le commentaire « *A single stored value may
//!    hold several names joined with `"; "`* ». C'est la seule convention
//!    ÉCRITE du dépôt pour un champ compositeur à plusieurs noms.
//! 2. `tune_core::metadata::artist_split::analyze_artist_credit` — le lecteur
//!    qui redécoupe une colonne de noms — ne tient `;` (`Separator::Semicolon`)
//!    pour un séparateur **fort** que parce qu'il « n'apparaît essentiellement
//!    jamais dans un nom d'artiste légitime ». La virgule et l'esperluette y
//!    sont classées *risquées* et restent sous condition (`split_risky`), et
//!    la **barre oblique n'est pas un séparateur du tout** : un
//!    `"Peter de Rose / Billy Hill"` serait relu comme **un seul** artiste
//!    portant une barre dans son nom.
//!
//! Écrire la barre oblique aurait donc fabriqué, au prochain découpage, un
//! artiste fantôme de plus — exactement le défaut que `artist_split` existe
//! pour réparer.
//!
//! # 🔴 La GARDE : une correction ne doit pas appauvrir (Bertrand, 25/09/2026)
//!
//! **Une correction n'a lieu que si les crédits ne portent pas MOINS de noms
//! que la colonne.** Sinon la piste n'est **pas touchée** — ni la colonne, ni
//! le fichier — et le cas est consigné avec ses deux valeurs.
//!
//! C'est un **comptage**, pas une comparaison de noms, et la nuance décide de
//! tout : une garde qui exigerait que chaque nom de la colonne se retrouve
//! dans les crédits bloquerait **sept des neuf bonnes corrections**
//! (`GORECKI` ne survit pas à `Henryk Mikołaj Górecki`). Voir
//! [`correction_appauvrissante`], qui porte la règle, ses six cas mesurés, et
//! surtout **pourquoi elle est un pis-aller assumé** : le comptage ne sait pas
//! distinguer « plusieurs auteurs » de « un auteur écrit deux fois »
//! (`Verdi Giuseppe (1813-1901)/Giuseppe Verdi`).
//!
//! # ⚠️ Ce que la décision coûte, mesuré et NON corrigé (25/09/2026)
//!
//! La sélection de ce module, rejouée en lecture seule contre le .18, rend
//! exactement le constat du brief : **104 candidats, 74 à remplir, 21 déjà
//! conformes, 9 à corriger**. Mais les neuf divergences ne se valent pas :
//!
//! ```text
//! Billy Hills                              ==>  Peter de Rose; Billy Hill
//! Sheffield Lab                            ==>  Robbie Buchanan
//! GORECKI  (×4)                            ==>  Henryk Mikołaj Górecki
//! R. Barrett                               ==>  Richard Barrett
//! George Gershwin/Ira Gershwin/D. Heyward  ==>  George Gershwin        ← 3 noms → 1
//! J. Joplin & G. Mekler                    ==>  Gabriel Mekler         ← 2 noms → 1
//! ```
//!
//! **Sur les 9, deux corrections feraient PERDRE des co-auteurs** : la colonne
//! y est plus riche que les crédits, incomplets pour ces deux pistes. Ira
//! Gershwin, DuBose Heyward et Janis Joplin y disparaîtraient.
//!
//! Ces deux-là sont **retenues par la garde** : rien n'est écrit pour elles.
//! Les sept autres sont corrigées. Les deux retenues sont consignées — `warn!`
//! au journal **et** liste dans l'état persisté (`retenues`), avec
//! l'identifiant, le titre, la valeur de la colonne et celle des crédits côte
//! à côte. La liste se relit par le `GET` **sans relancer la passe** : c'est
//! pour ça qu'elle vit dans `settings` et non seulement dans le journal.
//!
//! # 🔴 La mesure qui contredit la convention, et pourquoi elle ne l'emporte pas
//!
//! Les `tracks.composer` déjà en base sur le .18 se séparent ainsi : **35 avec
//! `/`, 26 avec `,`, 7 avec `&`, ZÉRO avec `;`**. Le terrain dit donc la barre
//! oblique — mais ce terrain, ce sont des **étiqueteurs tiers**, pas Tune : ces
//! valeurs viennent des balises, jamais d'une écriture de ce dépôt.
//!
//! Le départage est que **Tune ne sait pas relire la barre oblique** : elle
//! n'est pas un séparateur d'`analyze_artist_credit`, et écrire un
//! `"A / B"` fabriquerait un artiste fantôme de plus au prochain découpage.
//! Écrire ce que le lecteur du dépôt sait redécouper est le seul choix qui ne
//! crée pas de dette.
//!
//! # Ce que la passe NE touche pas
//!
//! * **`writer` et `lyricist`** (81 lignes de `writer` sur le .18). La
//!   décision de Bertrand porte sur le compositeur seul. `credits_release.rs`
//!   les range pourtant ensemble sous `ROLES_D_AUTEUR = ["composer",
//!   "writer"]` : un jour où l'on voudra les traiter, ce sera une décision à
//!   prendre, pas un `contains` à élargir ici.
//! * **Les pistes sans crédit compositeur** — les 36 qui n'ont que la colonne.
//!   Rien ne fait foi contre elle, donc rien ne la corrige.
//! * **Les pistes UPnP** : pas de fichier, rien à graver. Et les pistes de
//!   feuille CUE, dont `file_path` est NUL par construction — quinze pistes y
//!   partagent une image, et les réécrire l'une après l'autre laisserait
//!   l'image étiquetée avec la dernière tranche (même refus que
//!   `write_tags.rs`).
//!
//! # Déclenchement, arrêt, reprise
//!
//! Déclenchée **à la main** par un POST. Aucun appel depuis `scan_import` ni
//! depuis `background.rs` : une passe qui réécrit les fichiers de
//! l'utilisateur ne part pas toute seule.
//!
//! L'arrêt passe par [`tune_core::taches_de_fond`], sous
//! [`Tache::Enrichissement`] — et **pas** sous une huitième tâche à soi. C'est
//! le choix qu'a déjà fait la passe de crédits automatique
//! (`library/credits.rs`), qui remplit la table que celle-ci relit : les deux
//! bouts de la même chaîne s'arrêtent donc du même geste, et l'écran « État du
//! serveur » n'hérite pas d'une carte de plus sans libellé.
//!
//! La reprise est un **curseur** : `derniere_piste`, persisté avec
//! l'avancement, relu au lancement quand la passe précédente s'est arrêtée en
//! pause. Un curseur explicite et non « la sélection se vide d'elle-même » :
//! ici elle ne se vide pas — une piste traitée reste candidate, elle bascule
//! seulement en `inchange`. Sans curseur, une reprise relirait toute la
//! bibliothèque pour ne rien faire.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use tune_core::db::backend::{SqlValue, ToSqlValue};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::metadata::tag_writer::{TagUpdate, write_tags};
use tune_core::taches_de_fond::{Tache, est_en_pause};
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::state::AppState;

/// Identifiant au registre `background_tasks` (#2129) : c'est lui que le
/// bandeau global affiche, et lui que la route refuse de doubler.
pub(crate) const TACHE: &str = "compositeur_depuis_credits";

/// La clé de `settings` qui porte l'avancement. En base et non en mémoire :
/// l'écran doit retrouver les cinq comptes après un rechargement de page, et
/// le curseur doit survivre à une pause posée le soir.
const CLE_ETAT: &str = "compositeur_depuis_credits_status";

/// Le séparateur d'un `tracks.composer` à plusieurs noms. Voir l'en-tête du
/// module pour les deux preuves ; ce n'est pas un goût.
pub(crate) const SEPARATEUR: &str = "; ";

/// Tous les combien on republie l'avancement. Une écriture par piste ferait un
/// événement WebSocket par fichier.
const JALON_AVANCEMENT: usize = 25;

/// Une piste candidate, avec ce que les crédits disent d'elle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidat {
    pub(crate) id: i64,
    /// Le titre de la piste. Porté jusqu'ici uniquement pour que la liste des
    /// retenues soit lisible sans repasser par la base.
    pub(crate) titre: String,
    pub(crate) chemin: String,
    /// `tracks.composer` tel qu'il est, déjà rogné. `None` = colonne vide.
    pub(crate) colonne: Option<String>,
    /// Ce que les crédits disent, assemblé par [`chaine_des_compositeurs`].
    pub(crate) attendu: String,
}

/// Le verdict d'une piste — les cinq comptes que rend le `GET`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// La colonne était vide : on la remplit.
    Rempli,
    /// La colonne disait autre chose : on la corrige.
    Corrige,
    /// La colonne dit déjà exactement les crédits : on ne touche à rien.
    Inchange,
}

/// Ce qu'il y a à faire, mesuré maintenant. Sert au coût annoncé **avant** de
/// lancer comme au rapport de fin.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Inventaire {
    pub(crate) a_remplir: usize,
    pub(crate) a_corriger: usize,
    pub(crate) inchange: usize,
    /// Divergences que la garde [`correction_appauvrissante`] va RETENIR. Elles
    /// ne sont pas dans `a_corriger` : rien ne sera écrit pour elles, et le
    /// coût annoncé doit le dire avant de lancer, pas après.
    pub(crate) retenues: usize,
}

impl Inventaire {
    /// Le coût annoncé : combien de fichiers la passe va ouvrir et réécrire.
    pub(crate) fn pistes_concernees(self) -> usize {
        self.a_remplir + self.a_corriger
    }
}

/// La sélection.
///
/// `COALESCE(t.source, 'local') = 'local'` des deux côtés : la colonne est
/// `DEFAULT 'local'` mais reste nullable, et une ligne ancienne portant `NULL`
/// est une ligne locale. Un `source = 'local'` nu l'écarterait en silence.
///
/// ⛔ `file_path IS NOT NULL` **reste**. Ne pas retomber sur `cue_media_path` :
/// les pistes d'une feuille CUE partagent un fichier, et les réécrire l'une
/// après l'autre laisserait l'image étiquetée avec la dernière tranche. Même
/// refus, même raison, que `write_tags.rs` et `metadata::batch`.
///
/// Sortie de la fonction pour être **exécutable** par un témoin : c'est une
/// requête, pas un texte. Une garde qui n'en comparerait que la chaîne serait
/// satisfaite par sa propre cible et ne dirait rien des lignes rendues — or
/// c'est le périmètre décidé par Bertrand qui s'écrit ici.
pub(crate) fn sql_candidats(engine: tune_core::db::engine::Engine) -> String {
    let m = |i: usize| crate::routes::versions::marqueur(engine, i);
    format!(
        "SELECT t.id, t.file_path, t.composer, c.artist_name, c.position, c.id, t.title \
         FROM tracks t \
         JOIN track_credits c ON c.track_id = t.id \
         WHERE c.role = 'composer' \
           AND TRIM(COALESCE(c.artist_name, '')) <> '' \
           AND COALESCE(t.source, 'local') = 'local' \
           AND t.file_path IS NOT NULL \
           AND TRIM(t.file_path) <> '' \
           AND t.id > {} \
         ORDER BY t.id",
        m(1)
    )
}

/// La chaîne à écrire, depuis les noms crédités **dans l'ordre de
/// `position`**.
///
/// Dédoublonne à l'identique (après rognage) : un disque où le même auteur est
/// crédité deux fois — cas courant d'un import qui a tourné deux fois —
/// n'écrirait pas « Bach; Bach » dans le fichier de l'utilisateur. Le
/// dédoublonnage est EXACT et non replié sur la casse : « Bach » et « BACH »
/// sont deux graphies, et choisir laquelle survit serait une décision que
/// personne n'a prise.
pub(crate) fn chaine_des_compositeurs(noms: &[String]) -> String {
    let mut vus: Vec<&str> = Vec::new();
    for nom in noms {
        let n = nom.trim();
        if !n.is_empty() && !vus.contains(&n) {
            vus.push(n);
        }
    }
    vus.join(SEPARATEUR)
}

/// Les marques qui séparent à coup sûr DEUX noms dans une colonne écrite par
/// un étiqueteur tiers. La **virgule en est absente** volontairement : elle vit
/// à l'intérieur des noms (« Grover Washington, Jr. », « Earth, Wind & Fire »),
/// et `artist_split` la classe elle-même *risquée*.
const MARQUES_DE_PLURIEL: [char; 4] = [';', '/', '&', '+'];

/// Combien de noms une valeur porte, au plus sûr.
fn noms_distincts(valeur: &str) -> usize {
    valeur
        .split(MARQUES_DE_PLURIEL)
        .filter(|p| !p.trim().is_empty())
        .count()
        .max(1)
}

/// 🔴 **LA GARDE** — cette correction ferait-elle PERDRE des co-auteurs ?
///
/// Quand elle rend `true`, la piste n'est **pas** touchée : ni la colonne, ni
/// le fichier. Le cas est consigné avec ses deux valeurs, pour être repris à
/// la main.
///
/// # Un COMPTAGE, jamais une comparaison de noms
///
/// La règle de Bertrand (25/09/2026) : *une correction n'a lieu que si les
/// crédits ne portent pas MOINS de noms que la colonne.* Le comptage est
/// l'essentiel, et il n'est pas un détail d'implémentation : une garde qui
/// exigerait que chaque nom de la colonne « survive » dans les crédits
/// bloquerait **sept des neuf bonnes corrections** — `GORECKI` ne survit pas à
/// `Henryk Mikołaj Górecki`, ni `R. Barrett` à `Richard Barrett`, ni
/// `Billy Hills` à `Billy Hill`. Or ce sont exactement celles qu'on veut.
///
/// Sur les neuf divergences mesurées :
///
/// ```text
/// GORECKI (×4)                             1 → 1   corrigé
/// Sheffield Lab                            1 → 1   corrigé
/// R. Barrett                               1 → 1   corrigé
/// Billy Hills                              1 → 2   corrigé
/// George Gershwin/Ira Gershwin/D. Heyward  3 → 1   RETENU
/// J. Joplin & G. Mekler                    2 → 1   RETENU
/// ```
///
/// # ⚠️ Pourquoi c'est un pis-aller ASSUMÉ, et non une solution
///
/// Le comptage **ne distingue pas** « plusieurs auteurs » de « un auteur écrit
/// plusieurs fois ». Mesuré sur le .18 : **66 pistes locales sur 9 112**
/// (0,7 % — c'est toute l'exposition) ont une colonne à plusieurs noms, et
/// parmi elles :
///
/// ```text
/// Verdi Giuseppe (1813-1901)/Giuseppe Verdi
/// Verdi, Guiseppe (1813-1901)/Giuseppe Verdi     ← avec la faute, en prime
/// Primerose/ Mills
/// ```
///
/// C'est **le même homme deux fois**. Des crédits qui rendraient
/// `Giuseppe Verdi` seul **corrigeraient** au lieu d'appauvrir — et la garde
/// les retiendra quand même. Bertrand le sait et l'accepte : ces doublons
/// restent à traiter à la main.
///
/// 🔴 **Que personne ne prenne donc cette garde pour un jugement de qualité.**
/// Elle ne dit pas « les crédits sont moins bons » ; elle dit « les crédits
/// portent moins de noms, va voir ». La liste des retenues existe pour ça.
///
/// (Répartition par genre de ces 66 : Electro 16, Jazz 11, Classique 10,
/// Pop-Rock 8, Rock 7, Pop 7. Aucun genre ne domine — il n'y a aucune règle à
/// en tirer, et c'est pour éviter qu'on la cherche que le chiffre est ici.)
pub(crate) fn correction_appauvrissante(colonne: &str, attendu: &str) -> bool {
    noms_distincts(colonne) > noms_distincts(attendu)
}

/// Une divergence que la garde a RETENUE, telle que le `GET` la rend et que le
/// journal la nomme : les **deux valeurs côte à côte**, plus de quoi retrouver
/// la piste. Sans les deux, la liste n'est pas exploitable à la main.
fn retenue_en_json(candidat: &Candidat) -> Value {
    json!({
        "track_id": candidat.id,
        "titre": candidat.titre,
        "colonne": candidat.colonne.as_deref().unwrap_or(""),
        "credits": candidat.attendu,
        "noms_colonne": noms_distincts(candidat.colonne.as_deref().unwrap_or("")),
        "noms_credits": noms_distincts(&candidat.attendu),
    })
}

/// Plafond de la liste consignée dans l'état.
///
/// L'état vit dans une ligne de `settings` : une liste non bornée y ferait une
/// valeur de plusieurs mégaoctets sur une bibliothèque abîmée, relue à chaque
/// `GET`. Le **compte**, lui, n'est jamais tronqué — c'est la liste qui l'est,
/// et l'état le dit (`retenues_tronquees`). 200 tient très large : l'exposition
/// totale mesurée sur le .18 est de 66 pistes, et seules 2 sont retenues.
const RETENUES_LISTEES_MAX: usize = 200;

/// Le verdict d'une piste : remplir, corriger, ou ne rien faire.
///
/// La comparaison est faite sur les chaînes ROGNÉES. Elle n'est **pas** repliée
/// sur la casse ni sur les accents : `GORECKI` et `Henryk Mikołaj Górecki`
/// doivent diverger — c'est précisément le cas #3 du constat, et un repli les
/// déclarerait d'accord.
pub(crate) fn verdict(colonne: Option<&str>, attendu: &str) -> Verdict {
    match colonne.map(str::trim).filter(|c| !c.is_empty()) {
        None => Verdict::Rempli,
        Some(actuel) if actuel == attendu => Verdict::Inchange,
        Some(_) => Verdict::Corrige,
    }
}

/// Plie les lignes de la sélection — une par (piste × crédit) — en candidats.
///
/// Le tri des crédits se fait **en Rust** et non par un `ORDER BY c.position` :
/// `track_credits.position` est `INTEGER` sous SQLite mais **`TEXT`** sous
/// PostgreSQL (`pg_migrate.rs`), où `'10' < '2'` lexicalement. Un tri SQL
/// rendrait donc deux ordres différents selon le moteur, et la même
/// bibliothèque migrée verrait ses co-auteurs permutés. `SqlValue::as_i64`
/// sait lire les deux ; à position égale, l'`id` du crédit départage, pour que
/// l'ordre ne dépende pas de l'ordre de rendu du moteur.
/// Les crédits d'une piste pendant le pliage : `(chemin, colonne, [(position,
/// id du crédit, nom)])`. Nommé pour que la table de pliage reste lisible.
type GroupeDeCredits = (String, Option<String>, Vec<(i64, i64, String)>);

pub(crate) fn candidats_depuis_lignes(lignes: &[Vec<SqlValue>]) -> Vec<Candidat> {
    // (piste, chemin, colonne) → crédits [(position, id_credit, nom)]
    let mut ordre: Vec<i64> = Vec::new();
    let mut titres: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    let mut par_piste: std::collections::HashMap<i64, GroupeDeCredits> =
        std::collections::HashMap::new();

    for ligne in lignes {
        let (Some(id), Some(chemin), Some(nom)) = (
            ligne.first().and_then(SqlValue::as_i64),
            ligne.get(1).and_then(SqlValue::as_string),
            ligne.get(3).and_then(SqlValue::as_string),
        ) else {
            continue;
        };
        let colonne = ligne.get(2).and_then(SqlValue::as_string);
        titres.entry(id).or_insert_with(|| {
            ligne
                .get(6)
                .and_then(SqlValue::as_string)
                .unwrap_or_default()
        });
        let position = ligne.get(4).and_then(SqlValue::as_i64).unwrap_or(0);
        let id_credit = ligne.get(5).and_then(SqlValue::as_i64).unwrap_or(0);
        let entree = par_piste.entry(id).or_insert_with(|| {
            ordre.push(id);
            (chemin, colonne, Vec::new())
        });
        entree.2.push((position, id_credit, nom));
    }

    let mut sortie = Vec::with_capacity(ordre.len());
    for id in ordre {
        let Some((chemin, colonne, mut credits)) = par_piste.remove(&id) else {
            continue;
        };
        credits.sort_by_key(|c| (c.0, c.1));
        let noms: Vec<String> = credits.into_iter().map(|(_, _, n)| n).collect();
        let attendu = chaine_des_compositeurs(&noms);
        if attendu.is_empty() {
            continue;
        }
        sortie.push(Candidat {
            id,
            titre: titres.remove(&id).unwrap_or_default(),
            chemin,
            colonne,
            attendu,
        });
    }
    sortie
}

/// Les candidats après le curseur, et l'inventaire de ce qu'ils valent.
fn inventaire(state: &AppState, curseur: i64) -> (Inventaire, Vec<Candidat>) {
    let sql = sql_candidats(state.backend.engine());
    let params: [&dyn ToSqlValue; 1] = [&curseur];
    let lignes = state
        .backend
        .query_many(&sql, &params)
        .ou_defaut_journalise();
    let candidats = candidats_depuis_lignes(&lignes);
    let mut inv = Inventaire::default();
    for c in &candidats {
        match verdict(c.colonne.as_deref(), &c.attendu) {
            Verdict::Rempli => inv.a_remplir += 1,
            Verdict::Corrige => {
                if correction_appauvrissante(c.colonne.as_deref().unwrap_or(""), &c.attendu) {
                    inv.retenues += 1;
                } else {
                    inv.a_corriger += 1;
                }
            }
            Verdict::Inchange => inv.inchange += 1,
        }
    }
    (inv, candidats)
}

/// Les cinq comptes de la passe, plus le curseur de reprise.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Comptes {
    rempli: usize,
    corrige: usize,
    inchange: usize,
    sans_fichier: usize,
    echec_ecriture: usize,
    /// Les divergences que la GARDE a retenues : rien n'a été écrit pour
    /// elles, ni colonne ni fichier. Voir [`correction_appauvrissante`].
    retenu_moins_de_noms: usize,
    derniere_piste: i64,
}

impl Comptes {
    fn traitees(&self) -> usize {
        self.rempli
            + self.corrige
            + self.inchange
            + self.sans_fichier
            + self.echec_ecriture
            + self.retenu_moins_de_noms
    }
}

/// L'état au repos, servi tant qu'aucune passe n'a jamais tourné.
///
/// Toutes les clés y sont, y compris à zéro : rendre `{"status":"idle"}` seul
/// rendrait la réponse typée fausse et obligerait le client à combler les
/// manques (#1897).
fn etat_au_repos() -> Value {
    json!({
        "status": "idle",
        "task_id": Value::Null,
        "total": 0,
        "traitees": 0,
        "rempli": 0,
        "corrige": 0,
        "inchange": 0,
        "sans_fichier": 0,
        "echec_ecriture": 0,
        "retenu_moins_de_noms": 0,
        "retenues": [],
        "retenues_tronquees": false,
        "derniere_piste": 0,
        "raison": Value::Null,
    })
}

/// Une seule fabrique, pour que l'état servi pendant la passe et celui servi à
/// la fin ne puissent pas porter des clés différentes.
fn etat_en_json(
    task_id: &str,
    status: &str,
    total: usize,
    c: &Comptes,
    retenues: &[Value],
    raison: Option<&str>,
) -> Value {
    json!({
        "status": status,
        "task_id": task_id,
        "total": total,
        "traitees": c.traitees(),
        "rempli": c.rempli,
        "corrige": c.corrige,
        "inchange": c.inchange,
        "sans_fichier": c.sans_fichier,
        "echec_ecriture": c.echec_ecriture,
        "retenu_moins_de_noms": c.retenu_moins_de_noms,
        // 🔴 La LISTE, pas seulement le compte : l'exigence est qu'on puisse
        //    retrouver les cas après coup SANS relancer la passe. Elle vit donc
        //    dans l'état persisté, à côté des compteurs.
        "retenues": retenues,
        "retenues_tronquees": c.retenu_moins_de_noms > retenues.len(),
        "derniere_piste": c.derniere_piste,
        "raison": raison,
    })
}

fn ecrire_etat(backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>, valeur: &Value) {
    SettingsRepo::with_backend(backend.clone())
        .set(CLE_ETAT, &valeur.to_string())
        .ok();
}

fn lire_etat(state: &AppState) -> Value {
    SettingsRepo::with_backend(state.backend.clone())
        .get(CLE_ETAT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(etat_au_repos)
}

fn en_cours(state: &AppState) -> bool {
    state
        .background_tasks
        .snapshot()
        .iter()
        .any(|t| t.id == TACHE)
}

/// Le curseur de reprise, lu dans l'état précédent.
///
/// **Seul** un arrêt sur pause laisse un curseur à reprendre. Une passe
/// terminée (`done`) ou plantée repart de zéro : reprendre après un `done`
/// sauterait tout ce que l'utilisateur a pu ré-étiqueter entre-temps.
pub(crate) fn curseur_de_reprise(etat: &Value) -> i64 {
    if etat["status"].as_str() != Some("paused") {
        return 0;
    }
    etat["derniere_piste"]
        .as_i64()
        .filter(|c| *c > 0)
        .unwrap_or(0)
}

/// Corps optionnel de `POST /library/composer-from-credits`.
#[derive(Deserialize, Default)]
pub(crate) struct CorpsLancement {
    /// `true` : oublier le curseur et repasser toute la bibliothèque, même
    /// après une pause. Par défaut, une passe suspendue reprend où elle était.
    #[serde(default)]
    pub(crate) depuis_le_debut: bool,
}

/// `GET /library/composer-from-credits` — le coût AVANT, les comptes APRÈS.
///
/// L'inventaire est mesuré à l'instant (`a_remplir`, `a_corriger`,
/// `pistes_concernees`) et servi avec le dernier état de la passe : l'écran a
/// besoin des deux pour annoncer « 83 pistes concernées » avant, et « 74
/// remplies, 9 corrigées » après.
pub(crate) async fn statut(State(state): State<AppState>) -> Json<Value> {
    let mut etat = lire_etat(&state);
    // Le coût s'annonce toujours sur la bibliothèque ENTIÈRE (curseur 0) :
    // c'est ce que coûterait un lancement depuis le début, pas ce qu'il reste
    // d'une reprise. Les deux se lisent quand même, `reste_apres_curseur`
    // donnant la reprise.
    let (inv, _) = inventaire(&state, 0);
    let curseur = curseur_de_reprise(&etat);
    if let Some(o) = etat.as_object_mut() {
        o.insert("a_remplir".into(), json!(inv.a_remplir));
        o.insert("a_corriger".into(), json!(inv.a_corriger));
        o.insert("deja_conformes".into(), json!(inv.inchange));
        o.insert("retenues_prevues".into(), json!(inv.retenues));
        o.insert("pistes_concernees".into(), json!(inv.pistes_concernees()));
        o.insert("curseur_de_reprise".into(), json!(curseur));
        o.insert(
            "en_pause".into(),
            json!(est_en_pause(Tache::Enrichissement)),
        );
        if en_cours(&state) {
            o.insert("status".into(), json!("running"));
        }
    }
    Json(etat)
}

/// `POST /library/composer-from-credits` — lancer la passe.
///
/// 202 avec le coût annoncé ; 409 si elle tourne déjà — deux passes
/// concurrentes réécriraient les mêmes fichiers.
pub(crate) async fn lancer(
    State(state): State<AppState>,
    corps: Option<Json<CorpsLancement>>,
) -> impl IntoResponse {
    let depuis_le_debut = corps.map(|Json(c)| c.depuis_le_debut).unwrap_or(false);

    if en_cours(&state) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "code": "compositeur_deja_en_cours",
                "error": "compositeur_deja_en_cours",
                "message": "La passe « compositeur depuis les crédits » tourne déjà.",
            })),
        );
    }

    // Une passe en pause ne part pas en silence : sans ce refus, elle sortirait
    // à sa première piste et se déclarerait terminée — le repli muet qu'on
    // cherche à bannir.
    if est_en_pause(Tache::Enrichissement) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "code": "enrichissement_en_pause",
                "error": "enrichissement_en_pause",
                "message": "L'enrichissement est suspendu. Reprenez-le avant de lancer la passe.",
                "reprendre": "POST /system/taches-de-fond/enrichment/reprendre",
            })),
        );
    }

    let etat_precedent = lire_etat(&state);
    let curseur = if depuis_le_debut {
        0
    } else {
        curseur_de_reprise(&etat_precedent)
    };
    let (inv, candidats) = inventaire(&state, curseur);
    let total = candidats.len();
    let task_id = uuid::Uuid::new_v4().to_string();

    let comptes = Comptes {
        derniere_piste: curseur,
        ..Default::default()
    };
    // Écrit AVANT le spawn : un client qui sonde juste après son 202 doit lire
    // `running`, pas l'état de la passe précédente.
    ecrire_etat(
        &state.backend,
        &etat_en_json(&task_id, "running", total, &comptes, &[], None),
    );

    info!(
        task_id = %task_id,
        total,
        curseur,
        a_remplir = inv.a_remplir,
        a_corriger = inv.a_corriger,
        retenues_prevues = inv.retenues,
        "compositeur_depuis_credits_demarre"
    );

    // Garde RAII prise AVANT le spawn : entre ce point et le premier tour de
    // boucle, un second POST voit déjà « en cours ».
    let garde =
        state
            .background_tasks
            .begin(TACHE, "Compositeur depuis les crédits…", "enrichment");
    let etat_tache = state.clone();
    let id_tache = task_id.clone();
    tokio::spawn(async move {
        let _garde = garde;
        executer(etat_tache, id_tache, candidats, comptes).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "started",
            "task_id": task_id,
            "total": total,
            // Le coût annoncé AVANT : combien de fichiers vont être réécrits.
            "pistes_concernees": inv.pistes_concernees(),
            "a_remplir": inv.a_remplir,
            "a_corriger": inv.a_corriger,
            "deja_conformes": inv.inchange,
            // Ce que la GARDE va retenir : annoncé AVANT, pas découvert après.
            "retenues_prevues": inv.retenues,
            "curseur": curseur,
            "statut": "GET /library/composer-from-credits",
            "arreter": "POST /system/taches-de-fond/enrichment/pause",
        })),
    )
}

/// La boucle. Sortie de la route pour être lisible d'un bloc : une frontière
/// de pause, une comparaison, une écriture du fichier, une écriture de la
/// colonne.
async fn executer(state: AppState, task_id: String, candidats: Vec<Candidat>, mut c: Comptes) {
    let total = candidats.len();
    let taches = state.background_tasks.clone();
    // Les cas que la garde retient, consignés au fil de l'eau : ils partent
    // dans l'état à chaque publication, donc une passe interrompue laisse
    // déjà lisible ce qu'elle a retenu jusque-là.
    let mut retenues: Vec<Value> = Vec::new();

    for candidat in candidats {
        // La frontière de pause, en TÊTE de boucle : la piste précédente est
        // écrite, aucune écriture n'est en vol. On SORT, et le curseur — écrit
        // après CHAQUE piste — dit où reprendre.
        if est_en_pause(Tache::Enrichissement) {
            info!(
                task_id = %task_id,
                traitees = c.traitees(),
                restantes = total - c.traitees(),
                derniere_piste = c.derniere_piste,
                "compositeur_depuis_credits_en_pause"
            );
            ecrire_etat(
                &state.backend,
                &etat_en_json(
                    &task_id,
                    "paused",
                    total,
                    &c,
                    &retenues,
                    Some("pause_utilisateur"),
                ),
            );
            return;
        }

        let verdict = verdict(candidat.colonne.as_deref(), &candidat.attendu);
        if verdict == Verdict::Inchange {
            c.inchange += 1;
            c.derniere_piste = candidat.id;
            publier(&taches, &state, &task_id, total, &c, &retenues);
            continue;
        }

        // 🔴 LA GARDE, et elle est AVANT toute écriture.
        //
        //    Une correction n'a lieu que si les crédits ne portent pas MOINS de
        //    noms que la colonne. Quand ils en portent moins, on ne touche à
        //    RIEN — ni la colonne, ni le fichier — et le cas est consigné avec
        //    ses deux valeurs pour être repris à la main.
        //
        //    Sa place ici n'est pas cosmétique : posée après `write_tags`, elle
        //    aurait déjà gravé le fichier appauvri, et la « garde » n'aurait
        //    plus gardé que la base.
        if verdict == Verdict::Corrige {
            let avant = candidat.colonne.as_deref().unwrap_or("");
            if correction_appauvrissante(avant, &candidat.attendu) {
                c.retenu_moins_de_noms += 1;
                if retenues.len() < RETENUES_LISTEES_MAX {
                    retenues.push(retenue_en_json(&candidat));
                }
                warn!(
                    track_id = candidat.id,
                    titre = %candidat.titre,
                    colonne = avant,
                    credits = %candidat.attendu,
                    noms_colonne = noms_distincts(avant),
                    noms_credits = noms_distincts(&candidat.attendu),
                    "compositeur_retenu_les_credits_portent_moins_de_noms"
                );
                c.derniere_piste = candidat.id;
                publier(&taches, &state, &task_id, total, &c, &retenues);
                continue;
            }
        }

        // 🔴 Le FICHIER d'abord, la colonne ensuite.
        //
        //    C'est le fichier qui rend la correction durable : `update_batch`
        //    reconstruit sa ligne à partir de lui seul à chaque scan complet.
        //    Écrire la colonne d'abord puis échouer sur le fichier laisserait
        //    une base « corrigée » que le prochain scan complet défera — une
        //    correction qui se dit faite et qui ne l'est pas.
        let maj = TagUpdate {
            composer: Some(candidat.attendu.clone()),
            ..Default::default()
        };
        match write_tags(&candidat.chemin, &maj).await {
            Ok(_) => {}
            Err(e) if e == "file not found" => {
                c.sans_fichier += 1;
                c.derniere_piste = candidat.id;
                debug!(track_id = candidat.id, chemin = %candidat.chemin, "compositeur_fichier_introuvable");
                publier(&taches, &state, &task_id, total, &c, &retenues);
                continue;
            }
            Err(e) => {
                // Y compris « unsupported tag format » : un conteneur dans
                // lequel on ne sait pas graver (DSD, WavPack…) est un échec
                // d'écriture, pas un succès. Le journal le nomme.
                c.echec_ecriture += 1;
                c.derniere_piste = candidat.id;
                warn!(
                    track_id = candidat.id,
                    chemin = %candidat.chemin,
                    error = %e,
                    "compositeur_gravure_echouee"
                );
                publier(&taches, &state, &task_id, total, &c, &retenues);
                continue;
            }
        }

        if let Err(e) = ecrire_la_colonne(&state, candidat.id, &candidat.attendu) {
            // Le fichier porte la valeur, la base non : ce n'est pas perdu (le
            // prochain scan relira la balise), mais ça se dit.
            warn!(
                track_id = candidat.id,
                error = %e,
                "compositeur_colonne_non_ecrite"
            );
        }
        match verdict {
            Verdict::Rempli => c.rempli += 1,
            Verdict::Corrige => {
                // La garde est passée plus haut : arriver ici, c'est que les
                // crédits ne portent pas moins de noms que la colonne.
                c.corrige += 1;
                info!(
                    track_id = candidat.id,
                    avant = candidat.colonne.as_deref().unwrap_or(""),
                    apres = %candidat.attendu,
                    "compositeur_corrige"
                );
            }
            Verdict::Inchange => unreachable!("traité plus haut"),
        }
        c.derniere_piste = candidat.id;
        publier(&taches, &state, &task_id, total, &c, &retenues);
    }

    info!(
        task_id = %task_id,
        total,
        rempli = c.rempli,
        corrige = c.corrige,
        inchange = c.inchange,
        sans_fichier = c.sans_fichier,
        echec_ecriture = c.echec_ecriture,
        retenu_moins_de_noms = c.retenu_moins_de_noms,
        "compositeur_depuis_credits_termine"
    );
    taches.update_progress(TACHE, total as u64, total as u64, "Compositeur");
    // Fin de parcours : le curseur retombe à zéro, sans quoi une relance après
    // un `done` ne verrait plus rien.
    c.derniere_piste = 0;
    ecrire_etat(
        &state.backend,
        &etat_en_json(&task_id, "done", total, &c, &retenues, None),
    );
}

/// Publie l'avancement, au jalon seulement. Le curseur, lui, est dans l'état
/// publié : au jalon aussi, ce qui borne une reprise à au plus
/// [`JALON_AVANCEMENT`] pistes déjà faites — toutes en `inchange`, donc sans
/// une seule réécriture de fichier.
fn publier(
    taches: &crate::background_tasks::BackgroundTasks,
    state: &AppState,
    task_id: &str,
    total: usize,
    c: &Comptes,
    retenues: &[Value],
) {
    let traitees = c.traitees();
    if !traitees.is_multiple_of(JALON_AVANCEMENT) {
        return;
    }
    taches.update_progress(TACHE, traitees as u64, total as u64, "Compositeur");
    ecrire_etat(
        &state.backend,
        &etat_en_json(task_id, "running", total, c, retenues, None),
    );
}

/// L'écriture de `tracks.composer`.
///
/// 🔴 Volontairement écrite ici, en SQL nu, et **pas** ajoutée à
/// `tune_core::db::track_repo` : ce fichier est tenu par une autre session
/// (chasse PostgreSQL) et attendu par le lot `scan-non-destructeur` dans la
/// .165. Une méthode de plus y ferait un conflit de fusion sur un fichier de
/// 200 Kio, pour une requête d'une ligne.
fn ecrire_la_colonne(state: &AppState, track_id: i64, valeur: &str) -> Result<(), String> {
    let e = state.backend.engine();
    let m = |i: usize| crate::routes::versions::marqueur(e, i);
    let sql = format!("UPDATE tracks SET composer = {} WHERE id = {}", m(1), m(2));
    let params: [&dyn ToSqlValue; 2] = [&valeur, &track_id];
    state
        .backend
        .execute(&sql, &params)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::test_scratch::scratch_dir;

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    fn fixture(nom: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tune-core/tests/fixtures")
            .join(nom)
    }

    fn piste(
        s: &AppState,
        id: i64,
        chemin: Option<&str>,
        composer: Option<&str>,
        source: Option<&str>,
    ) {
        piste_titree(s, id, &format!("piste {id}"), chemin, composer, source);
    }

    fn piste_titree(
        s: &AppState,
        id: i64,
        titre: &str,
        chemin: Option<&str>,
        composer: Option<&str>,
        source: Option<&str>,
    ) {
        let titre = titre.to_string();
        let chemin = chemin.map(str::to_string);
        let composer = composer.map(str::to_string);
        let source = source.map(str::to_string);
        s.backend
            .execute(
                "INSERT INTO tracks (id, title, file_path, composer, source) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                &[&id as &dyn ToSqlValue, &titre, &chemin, &composer, &source],
            )
            .unwrap();
    }

    fn credit(s: &AppState, id: i64, track_id: i64, role: &str, nom: &str, position: i64) {
        let role = role.to_string();
        let nom = nom.to_string();
        s.backend
            .execute(
                "INSERT INTO track_credits (id, track_id, artist_name, role, position) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                &[&id as &dyn ToSqlValue, &track_id, &nom, &role, &position],
            )
            .unwrap();
    }

    /// Le compositeur réellement inscrit DANS le fichier, relu par le lecteur
    /// du scan — jamais par la base. Passer par le contenu du fichier, et pas
    /// par le code de retour, est délibéré : un `Ok(_)` ne prouverait pas dans
    /// quel fichier on a écrit, ni ce qu'on y a mis.
    fn compositeur_du_fichier(chemin: &std::path::Path) -> Option<String> {
        tune_core::metadata::read_extended_metadata(chemin)
            .get("composer")
            .cloned()
    }

    fn colonne(s: &AppState, id: i64) -> Option<String> {
        s.backend
            .query_one(
                "SELECT composer FROM tracks WHERE id = ?1",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap()
            .and_then(|r| r.first().and_then(SqlValue::as_string))
    }

    async fn attendre_la_fin(s: &AppState) {
        for _ in 0..400 {
            if !en_cours(s) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("la passe n'a pas fini");
    }

    // ------------------------------------------------------------------
    // La convention de séparateur, et l'ordre des co-auteurs.
    // ------------------------------------------------------------------

    /// Le cas réel `Wagon Wheels` : deux auteurs, dans l'ordre des crédits,
    /// joints par `"; "`.
    #[test]
    fn deux_auteurs_sont_joints_par_le_separateur_du_depot() {
        assert_eq!(
            chaine_des_compositeurs(&["Peter de Rose".into(), "Billy Hill".into()]),
            "Peter de Rose; Billy Hill"
        );
        assert_eq!(SEPARATEUR, "; ", "la convention a changé sans preuve");
    }

    /// 🔴 La preuve que `"; "` n'est pas un goût : c'est le SEUL séparateur que
    /// le relecteur du dépôt (`artist_split`) tient pour fort. Une barre
    /// oblique ne serait pas redécoupée du tout — les deux auteurs
    /// reviendraient comme UN artiste fantôme.
    #[test]
    fn le_separateur_retenu_est_celui_que_le_relecteur_sait_redecouper() {
        use tune_core::metadata::artist_split::analyze_artist_credit;

        let joint = chaine_des_compositeurs(&["Peter de Rose".into(), "Billy Hill".into()]);
        let relu = analyze_artist_credit(&joint, &[], false);
        assert_eq!(
            relu.tokens,
            vec!["Peter de Rose".to_string(), "Billy Hill".to_string()],
            "le séparateur écrit n'est pas redécoupé : {relu:?}"
        );

        // La contre-preuve, dans le même témoin : la barre oblique du constat
        // initial ne se redécoupe PAS, même avec les séparateurs risqués armés.
        let barre = analyze_artist_credit("Peter de Rose / Billy Hill", &[], true);
        assert_eq!(
            barre.tokens,
            vec!["Peter de Rose / Billy Hill".to_string()],
            "si la barre oblique se redécoupait, le choix du point-virgule \
             perdrait sa raison d'être"
        );
    }

    /// Un auteur crédité deux fois — un import qui a tourné deux fois — ne
    /// produit pas « Bach; Bach » dans le fichier de l'utilisateur.
    #[test]
    fn un_auteur_credite_deux_fois_n_est_ecrit_qu_une_fois() {
        assert_eq!(
            chaine_des_compositeurs(&["Bach".into(), " Bach ".into(), "Handel".into()]),
            "Bach; Handel"
        );
    }

    // ------------------------------------------------------------------
    // Le verdict : c'est lui qui distingue `rempli` de `corrige`.
    // ------------------------------------------------------------------

    #[test]
    fn le_verdict_separe_le_vide_du_divergent() {
        assert_eq!(verdict(None, "Górecki"), Verdict::Rempli);
        assert_eq!(verdict(Some(""), "Górecki"), Verdict::Rempli);
        assert_eq!(verdict(Some("   "), "Górecki"), Verdict::Rempli);
        assert_eq!(verdict(Some("Górecki"), "Górecki"), Verdict::Inchange);
        assert_eq!(verdict(Some(" Górecki "), "Górecki"), Verdict::Inchange);
        // 🔴 Les trois divergences réelles du constat. Un repli sur la casse ou
        // sur les accents déclarerait la troisième « d'accord » et laisserait
        // `GORECKI` en place.
        assert_eq!(
            verdict(Some("Billy Hills"), "Peter de Rose; Billy Hill"),
            Verdict::Corrige
        );
        assert_eq!(
            verdict(Some("Sheffield Lab"), "Robbie Buchanan"),
            Verdict::Corrige
        );
        assert_eq!(
            verdict(Some("GORECKI"), "Henryk Mikołaj Górecki"),
            Verdict::Corrige
        );
    }

    /// 🔴 La règle de la garde est un COMPTAGE, et ce témoin le prouve sur les
    /// neuf divergences réelles du .18 (relevées en lecture seule le
    /// 25/09/2026) : deux retenues, sept laissées passer.
    ///
    /// La deuxième moitié est la plus importante : une garde écrite comme une
    /// comparaison de NOMS — « chaque nom de la colonne doit se retrouver dans
    /// les crédits » — bloquerait sept des neuf bonnes corrections. Le témoin
    /// nomme donc chacune d'elles.
    #[test]
    fn la_garde_compte_les_noms_et_ne_compare_pas_les_noms() {
        // Les deux vraies : la colonne est plus riche que les crédits.
        assert!(correction_appauvrissante(
            "George Gershwin/Ira Gershwin/D. Heyward",
            "George Gershwin"
        ));
        assert!(correction_appauvrissante(
            "J. Joplin & G. Mekler",
            "Gabriel Mekler"
        ));
        // Les sept autres passent. AUCUN de ces quatre couples ne survivrait à
        // une garde qui comparerait les noms au lieu de les compter : `GORECKI`
        // n'est pas `Henryk Mikołaj Górecki`, `R. Barrett` n'est pas
        // `Richard Barrett`, `Billy Hills` n'est pas `Billy Hill`. C'est
        // exactement ce qu'on veut corriger.
        for (avant, apres) in [
            ("Billy Hills", "Peter de Rose; Billy Hill"), // 1 → 2
            ("Sheffield Lab", "Robbie Buchanan"),         // 1 → 1
            ("GORECKI", "Henryk Mikołaj Górecki"),        // 1 → 1
            ("R. Barrett", "Richard Barrett"),            // 1 → 1
        ] {
            assert!(
                !correction_appauvrissante(avant, apres),
                "{avant} → {apres} retenu à tort : la garde compare les noms au \
                 lieu de les compter, et bloque une bonne correction"
            );
        }

        // ⚠️ Le pis-aller assumé, écrit noir sur blanc : le comptage ne sait
        //    PAS qu'il s'agit du même homme deux fois. Ce témoin fige le fait
        //    que la garde retient ce cas — pour que personne ne la prenne pour
        //    un jugement de qualité, et pour que le jour où on saura replier
        //    les doublons, l'assertion change AVEC la règle.
        assert!(
            correction_appauvrissante(
                "Verdi Giuseppe (1813-1901)/Giuseppe Verdi",
                "Giuseppe Verdi"
            ),
            "si ce cas cesse d'être retenu, la garde a gagné un repli sur les \
             doublons : mettre la doc du module à jour avec"
        );
        // 🔴 La virgule n'est PAS une marque de pluriel : « Grover Washington,
        // Jr. » est un seul nom, et le compter pour deux ferait un faux
        // signalement à chaque suffixe générationnel.
        assert!(!correction_appauvrissante(
            "Grover Washington, Jr.",
            "Grover Washington Jr."
        ));
    }

    // ------------------------------------------------------------------
    // Le périmètre : ce que la sélection retient, et ce qu'elle écarte.
    // ------------------------------------------------------------------

    /// La sélection est passée à une VRAIE base : une garde de texte serait
    /// satisfaite par sa propre cible et ne dirait rien des lignes rendues.
    #[test]
    fn la_selection_ne_retient_que_le_local_avec_un_credit_compositeur() {
        let s = etat();
        // 1 — locale, avec fichier, un crédit compositeur : LE candidat.
        piste(&s, 1, Some("/m/1.flac"), None, Some("local"));
        credit(&s, 11, 1, "composer", "Górecki", 0);
        // 2 — locale, mais le crédit est un `writer` : hors décision (81 lignes
        //     sur le .18, laissées intactes exprès).
        piste(&s, 2, Some("/m/2.flac"), None, Some("local"));
        credit(&s, 21, 2, "writer", "Quelqu'un", 0);
        // 3 — locale, crédit `lyricist` : idem.
        piste(&s, 3, Some("/m/3.flac"), None, Some("local"));
        credit(&s, 31, 3, "lyricist", "Un parolier", 0);
        // 4 — UPnP : pas de fichier à graver, même avec un crédit.
        piste(&s, 4, Some("/m/4.flac"), None, Some("upnp"));
        credit(&s, 41, 4, "composer", "Un autre", 0);
        // 5 — `source` à NULL, ce qu'écrivent les lignes anciennes : LOCALE.
        piste(&s, 5, Some("/m/5.flac"), None, None);
        credit(&s, 51, 5, "composer", "Ancienne", 0);
        // 6 — piste de feuille CUE : `file_path` NUL, rien à graver.
        piste(&s, 6, None, None, Some("local"));
        credit(&s, 61, 6, "composer", "Cue", 0);
        // 7 — locale, sans aucun crédit : rien ne fait foi contre sa colonne.
        piste(
            &s,
            7,
            Some("/m/7.flac"),
            Some("Sheffield Lab"),
            Some("local"),
        );
        // 8 — crédit compositeur au nom VIDE : rien à écrire.
        piste(&s, 8, Some("/m/8.flac"), None, Some("local"));
        credit(&s, 81, 8, "composer", "   ", 0);

        let (_, candidats) = inventaire(&s, 0);
        assert_eq!(
            candidats.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![1, 5],
            "attendu : la piste locale créditée (1) et celle dont `source` est \
             NULL (5). Écartées : `writer` (2), `lyricist` (3), UPnP (4), CUE \
             (6), sans crédit (7), crédit vide (8)."
        );
    }

    /// L'ordre des co-auteurs suit `position`, pas l'ordre de rendu du moteur.
    /// Le tri est fait en Rust parce que `position` est `TEXT` sous PostgreSQL :
    /// un `ORDER BY` SQL y mettrait `'10'` avant `'2'`.
    #[test]
    fn les_co_auteurs_suivent_la_position_et_non_l_ordre_du_moteur() {
        let s = etat();
        piste(&s, 1, Some("/m/1.flac"), None, Some("local"));
        credit(&s, 103, 1, "composer", "Troisième", 10);
        credit(&s, 101, 1, "composer", "Deuxième", 2);
        credit(&s, 102, 1, "composer", "Premier", 1);

        let (_, candidats) = inventaire(&s, 0);
        assert_eq!(
            candidats[0].attendu, "Premier; Deuxième; Troisième",
            "l'ordre des co-auteurs ne suit pas `position`"
        );
    }

    /// L'inventaire ventile le coût AVANT de lancer : c'est ce que l'écran
    /// annonce, et c'est ce que la passe doit ensuite retrouver.
    #[test]
    fn l_inventaire_annonce_le_cout_avant_de_lancer() {
        let s = etat();
        piste(&s, 1, Some("/m/1.flac"), None, Some("local")); // à remplir
        credit(&s, 11, 1, "composer", "Górecki", 0);
        piste(&s, 2, Some("/m/2.flac"), Some("GORECKI"), Some("local")); // à corriger
        credit(&s, 21, 2, "composer", "Górecki", 0);
        piste(&s, 3, Some("/m/3.flac"), Some("Górecki"), Some("local")); // conforme
        credit(&s, 31, 3, "composer", "Górecki", 0);

        let (inv, _) = inventaire(&s, 0);
        assert_eq!(
            inv,
            Inventaire {
                a_remplir: 1,
                a_corriger: 1,
                inchange: 1,
                retenues: 0
            }
        );
        assert_eq!(inv.pistes_concernees(), 2, "deux fichiers seront réécrits");
    }

    /// Le curseur ne se reprend QUE derrière une pause. Après un `done`,
    /// reprendre sauterait ce que l'utilisateur a ré-étiqueté depuis.
    #[test]
    fn le_curseur_ne_se_reprend_que_derriere_une_pause() {
        assert_eq!(
            curseur_de_reprise(&json!({"status": "paused", "derniere_piste": 42})),
            42
        );
        assert_eq!(
            curseur_de_reprise(&json!({"status": "done", "derniere_piste": 42})),
            0
        );
        assert_eq!(
            curseur_de_reprise(&json!({"status": "running", "derniere_piste": 42})),
            0
        );
        assert_eq!(curseur_de_reprise(&etat_au_repos()), 0);
    }

    /// Et le curseur, une fois posé, écarte bien les pistes déjà vues.
    #[test]
    fn le_curseur_ecarte_les_pistes_deja_traitees() {
        let s = etat();
        for id in [1i64, 2, 3] {
            piste(&s, id, Some(&format!("/m/{id}.flac")), None, Some("local"));
            credit(&s, 100 + id, id, "composer", "Górecki", 0);
        }
        let (_, tous) = inventaire(&s, 0);
        assert_eq!(tous.iter().map(|c| c.id).collect::<Vec<_>>(), vec![1, 2, 3]);
        let (_, reste) = inventaire(&s, 2);
        assert_eq!(reste.iter().map(|c| c.id).collect::<Vec<_>>(), vec![3]);
    }

    // ------------------------------------------------------------------
    // 🟢 LE BANC — sur de VRAIS fichiers. Colonne vide ⇒ remplie ; colonne
    //    divergente ⇒ CORRIGÉE ; et la balise relue après écriture.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn la_passe_remplit_corrige_et_grave_la_balise() {
        let dir = scratch_dir("tune-compositeur-credits");
        // 1 — colonne VIDE, crédits à deux auteurs : à remplir.
        let vide = dir.join("vide.flac");
        std::fs::copy(fixture("test.flac"), &vide).unwrap();
        // 2 — colonne DIVERGENTE, et le cas non-ASCII : à corriger.
        let divergente = dir.join("divergente.flac");
        std::fs::copy(fixture("test.flac"), &divergente).unwrap();
        // 3 — colonne déjà conforme : on n'y touche pas.
        let conforme = dir.join("conforme.flac");
        std::fs::copy(fixture("test.flac"), &conforme).unwrap();

        let s = etat();
        piste(&s, 1, vide.to_str(), None, Some("local"));
        credit(&s, 11, 1, "composer", "Peter de Rose", 1);
        credit(&s, 12, 1, "composer", "Billy Hill", 2);
        piste(&s, 2, divergente.to_str(), Some("GORECKI"), Some("local"));
        credit(&s, 21, 2, "composer", "Henryk Mikołaj Górecki", 0);
        piste(
            &s,
            3,
            conforme.to_str(),
            Some("Robbie Buchanan"),
            Some("local"),
        );
        credit(&s, 31, 3, "composer", "Robbie Buchanan", 0);
        // 4 — la piste dont le fichier a disparu entre le scan et la passe.
        piste(&s, 4, Some("/nulle/part/absent.flac"), None, Some("local"));
        credit(&s, 41, 4, "composer", "Fantôme", 0);

        // Aucune des trois fixtures ne porte déjà un compositeur : sans cette
        // vérification, un témoin vert ne prouverait rien.
        for f in [&vide, &divergente, &conforme] {
            assert_eq!(compositeur_du_fichier(f), None, "{}", f.display());
        }

        let reponse = lancer(State(s.clone()), None).await.into_response();
        assert_eq!(reponse.status(), StatusCode::ACCEPTED);
        attendre_la_fin(&s).await;

        let etat = lire_etat(&s);
        assert_eq!(etat["status"], "done", "{etat}");
        assert_eq!(etat["rempli"], 1, "{etat}");
        assert_eq!(etat["corrige"], 1, "{etat}");
        assert_eq!(etat["inchange"], 1, "{etat}");
        assert_eq!(etat["sans_fichier"], 1, "{etat}");
        assert_eq!(etat["echec_ecriture"], 0, "{etat}");
        assert_eq!(etat["retenu_moins_de_noms"], 0, "{etat}");

        // 🔴 LA BALISE, relue sur le disque. C'est elle qui rend la correction
        //    durable : `update_batch` reconstruit la colonne à partir du
        //    fichier seul à chaque scan complet.
        assert_eq!(
            compositeur_du_fichier(&vide).as_deref(),
            Some("Peter de Rose; Billy Hill"),
            "la balise de la piste à colonne vide n'a pas été gravée"
        );
        assert_eq!(
            compositeur_du_fichier(&divergente).as_deref(),
            Some("Henryk Mikołaj Górecki"),
            "la CORRECTION n'a pas atteint la balise : le prochain scan \
             complet remettrait GORECKI"
        );
        // La conforme n'avait rien à recevoir, et n'a rien reçu.
        assert_eq!(compositeur_du_fichier(&conforme), None);

        // Et la colonne suit.
        assert_eq!(colonne(&s, 1).as_deref(), Some("Peter de Rose; Billy Hill"));
        assert_eq!(colonne(&s, 2).as_deref(), Some("Henryk Mikołaj Górecki"));
        assert_eq!(colonne(&s, 3).as_deref(), Some("Robbie Buchanan"));
        // La piste sans fichier n'a PAS vu sa colonne écrite : une colonne
        // écrite sans balise serait défaite au prochain scan complet.
        assert_eq!(colonne(&s, 4), None, "colonne écrite sans balise derrière");
    }

    /// Les accents et les caractères non-ASCII survivent à l'aller-retour
    /// disque. `Henryk Mikołaj Górecki` porte un `ł` (U+0142, hors Latin-1) et
    /// un `ó` : une écriture qui passerait par un encodage 8 bits les
    /// abîmerait sans rien rendre d'erroné.
    #[tokio::test]
    async fn les_caracteres_non_ascii_survivent_a_l_ecriture() {
        const NOM: &str = "Henryk Miko\u{0142}aj G\u{00f3}recki";
        assert!(NOM.contains('\u{0142}') && NOM.contains('\u{00f3}'));

        let dir = scratch_dir("tune-compositeur-accents");
        let cible = dir.join("accents.flac");
        std::fs::copy(fixture("test.flac"), &cible).unwrap();

        let s = etat();
        piste(&s, 1, cible.to_str(), Some("GORECKI"), Some("local"));
        credit(&s, 11, 1, "composer", NOM, 0);

        let _ = lancer(State(s.clone()), None).await.into_response();
        attendre_la_fin(&s).await;

        let relu = compositeur_du_fichier(&cible).unwrap();
        assert_eq!(relu, NOM, "octet pour octet : {:?}", relu.as_bytes());
        assert_eq!(colonne(&s, 1).as_deref(), Some(NOM));
    }

    /// 🔴 LA GARDE, SUR DE VRAIS FICHIERS, DES DEUX CÔTÉS.
    ///
    /// Un banc, deux pistes, une seule différence entre elles : le NOMBRE de
    /// noms. La réduction est retenue — **le fichier et la colonne sont
    /// vérifiés intacts**, pas seulement le compteur — et elle est consignée
    /// avec ses deux valeurs. La non-réduction, elle, passe et grave.
    ///
    /// Vérifier le fichier est le cœur du témoin : une garde posée après
    /// `write_tags` laisserait le compteur juste et le disque déjà gravé.
    #[tokio::test]
    async fn la_garde_retient_la_reduction_la_consigne_et_laisse_passer_le_reste() {
        let dir = scratch_dir("tune-compositeur-garde");
        // 1 — 3 noms dans la colonne, 1 dans les crédits : RETENUE.
        let appauvrie = dir.join("appauvrie.flac");
        std::fs::copy(fixture("test.flac"), &appauvrie).unwrap();
        // 2 — 1 nom dans la colonne, 1 dans les crédits, mais faux : CORRIGÉE.
        let corrigeable = dir.join("corrigeable.flac");
        std::fs::copy(fixture("test.flac"), &corrigeable).unwrap();
        // 3 — 1 nom dans la colonne, 2 dans les crédits : CORRIGÉE aussi. La
        //     garde ne bloque que la RÉDUCTION, pas l'enrichissement.
        let enrichie = dir.join("enrichie.flac");
        std::fs::copy(fixture("test.flac"), &enrichie).unwrap();

        let s = etat();
        piste_titree(
            &s,
            1,
            "Summertime",
            appauvrie.to_str(),
            Some("George Gershwin/Ira Gershwin/D. Heyward"),
            Some("local"),
        );
        credit(&s, 11, 1, "composer", "George Gershwin", 0);
        piste_titree(
            &s,
            2,
            "Symphonie N° 3",
            corrigeable.to_str(),
            Some("GORECKI"),
            Some("local"),
        );
        credit(&s, 21, 2, "composer", "Henryk Mikołaj Górecki", 0);
        piste_titree(
            &s,
            3,
            "Wagon Wheels",
            enrichie.to_str(),
            Some("Billy Hills"),
            Some("local"),
        );
        credit(&s, 31, 3, "composer", "Peter de Rose", 1);
        credit(&s, 32, 3, "composer", "Billy Hill", 2);

        // Le coût est annoncé AVANT : une retenue, deux corrections.
        let (inv, _) = inventaire(&s, 0);
        assert_eq!(inv.retenues, 1, "{inv:?}");
        assert_eq!(inv.a_corriger, 2, "{inv:?}");
        assert_eq!(
            inv.pistes_concernees(),
            2,
            "la piste retenue ne doit pas être comptée dans le coût : aucun \
             fichier ne sera ouvert pour elle"
        );

        let r = lancer(State(s.clone()), None).await.into_response();
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        attendre_la_fin(&s).await;

        let etat = lire_etat(&s);
        assert_eq!(etat["status"], "done", "{etat}");

        // 🔴 LE DISQUE D'ABORD. C'est l'assertion qui doit rougir quand la
        //    garde saute : un compteur juste sur un fichier déjà gravé serait
        //    le pire des verts. Le reste de l'état suit, en dessous.
        assert_eq!(
            compositeur_du_fichier(&appauvrie),
            None,
            "la garde a laissé GRAVER le fichier appauvri — état : {etat}"
        );
        assert_eq!(
            colonne(&s, 1).as_deref(),
            Some("George Gershwin/Ira Gershwin/D. Heyward"),
            "la garde a laissé écraser la colonne plus riche — état : {etat}"
        );
        assert_eq!(etat["retenu_moins_de_noms"], 1, "{etat}");
        assert_eq!(etat["corrige"], 2, "{etat}");

        // … et elle est CONSIGNÉE, avec les deux valeurs côte à côte, relisible
        // par le GET sans relancer la passe.
        let retenues = etat["retenues"].as_array().expect("liste absente");
        assert_eq!(retenues.len(), 1, "{etat}");
        let r0 = &retenues[0];
        assert_eq!(r0["track_id"], 1);
        assert_eq!(r0["titre"], "Summertime");
        assert_eq!(r0["colonne"], "George Gershwin/Ira Gershwin/D. Heyward");
        assert_eq!(r0["credits"], "George Gershwin");
        assert_eq!(r0["noms_colonne"], 3);
        assert_eq!(r0["noms_credits"], 1);
        assert_eq!(etat["retenues_tronquees"], false, "{etat}");

        // Les deux autres sont bien passées, disque compris.
        assert_eq!(
            compositeur_du_fichier(&corrigeable).as_deref(),
            Some("Henryk Mikołaj Górecki")
        );
        assert_eq!(
            compositeur_du_fichier(&enrichie).as_deref(),
            Some("Peter de Rose; Billy Hill"),
            "la garde bloque la RÉDUCTION, pas l'enrichissement"
        );

        // Et le GET sert la liste telle quelle, sans relancer quoi que ce soit.
        let Json(vu) = statut(State(s.clone())).await;
        assert_eq!(vu["retenues"].as_array().map(|a| a.len()), Some(1), "{vu}");
        assert_eq!(vu["retenues"][0]["titre"], "Summertime");
    }

    /// 🔴 LE TÉMOIN DU PIÈGE #4238 (commit `c252b7b4`).
    ///
    /// `lofty::read_from_path` → `Tag` → `save_to` JETTE tout champ Vorbis
    /// hors catalogue : `DYNAMIC RANGE`, `SOURCE`, et les champs maison que
    /// des testeurs posent avec Mp3tag. Ça a déjà coûté les DR de testeurs.
    ///
    /// Le témoin est posé **au niveau de la passe** et non du graveur : le
    /// graveur a le sien depuis #4238, mais c'est ICI qu'on décide d'écrire, et
    /// c'est ici qu'un futur raccourci vers `read_from_path` se poserait. La
    /// pochette est vérifiée avec, parce que `VorbisComments::save_to` seul la
    /// retirerait.
    #[tokio::test]
    async fn un_champ_vorbis_hors_catalogue_survit_a_la_passe() {
        use lofty::config::{ParseOptions, WriteOptions};
        use lofty::file::AudioFile;
        use lofty::flac::FlacFile;
        use lofty::ogg::OggPictureStorage;
        use lofty::picture::{MimeType, Picture, PictureType};

        let dir = scratch_dir("tune-compositeur-temoin-4238");
        let cible = dir.join("temoin.flac");
        std::fs::copy(fixture("test.flac"), &cible).unwrap();

        // Ce qu'un testeur a posé et que Tune ne connaît pas : un DR mesuré par
        // un analyseur externe, un `SOURCE`, un champ franchement maison — et
        // une pochette.
        {
            use std::io::Seek;
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&cible)
                .unwrap();
            let mut flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
            let png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
            flac.insert_picture(
                Picture::unchecked(png)
                    .pic_type(PictureType::CoverFront)
                    .mime_type(MimeType::Png)
                    .build(),
                Some(Default::default()),
            )
            .unwrap();
            if flac.vorbis_comments().is_none() {
                flac.set_vorbis_comments(Default::default());
            }
            let vc = flac.vorbis_comments_mut().unwrap();
            vc.insert("DYNAMIC RANGE".into(), "12".into());
            vc.insert("SOURCE".into(), "CD".into());
            vc.insert("MON CHAMP".into(), "valeur".into());
            f.rewind().unwrap();
            flac.save_to(&mut f, WriteOptions::default()).unwrap();
        }

        let s = etat();
        piste(&s, 1, cible.to_str(), Some("Sheffield Lab"), Some("local"));
        credit(&s, 11, 1, "composer", "Robbie Buchanan", 0);

        let _ = lancer(State(s.clone()), None).await.into_response();
        attendre_la_fin(&s).await;

        // L'écriture a bien eu lieu — sinon le témoin serait vert sans rien
        // prouver.
        assert_eq!(lire_etat(&s)["corrige"], 1);
        assert_eq!(
            compositeur_du_fichier(&cible).as_deref(),
            Some("Robbie Buchanan")
        );

        let mut f = std::fs::File::open(&cible).unwrap();
        let flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
        let vc = flac.vorbis_comments().unwrap();
        assert_eq!(
            vc.get("DYNAMIC RANGE"),
            Some("12"),
            "la passe a effacé le DYNAMIC RANGE : le piège #4238 est revenu"
        );
        assert_eq!(vc.get("SOURCE"), Some("CD"), "champ SOURCE effacé");
        assert_eq!(vc.get("MON CHAMP"), Some("valeur"), "champ maison effacé");
        assert_eq!(flac.pictures().len(), 1, "la pochette a sauté");
        // Et le lecteur du scan retrouve bien le DR, pas seulement lofty.
        let m = tune_core::metadata::read_extended_metadata(&cible);
        assert_eq!(m.get("dr_track").map(String::as_str), Some("12"));
    }

    /// La passe s'inscrit au registre (#2129) et ne se laisse pas doubler :
    /// deux passes concurrentes réécriraient les mêmes fichiers.
    #[tokio::test]
    async fn la_passe_s_inscrit_au_registre_et_refuse_un_doublon() {
        let dir = scratch_dir("tune-compositeur-doublon");
        let cible = dir.join("x.flac");
        std::fs::copy(fixture("test.flac"), &cible).unwrap();
        let s = etat();
        piste(&s, 1, cible.to_str(), None, Some("local"));
        credit(&s, 11, 1, "composer", "Górecki", 0);

        let r = lancer(State(s.clone()), None).await.into_response();
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        // La garde est prise avant le spawn : visible sans céder le fil.
        assert!(en_cours(&s));
        let r2 = lancer(State(s.clone()), None).await.into_response();
        assert_eq!(r2.status(), StatusCode::CONFLICT);
    }

    /// La pause de l'enrichissement arrête la passe à la frontière suivante,
    /// et l'état publié porte le curseur qui dit où reprendre.
    #[tokio::test]
    async fn une_pause_arrete_la_passe_et_laisse_un_curseur() {
        let s = etat();
        // Aucun fichier : les pistes défilent vite, seule la frontière compte.
        for id in [1i64, 2, 3] {
            piste(
                &s,
                id,
                Some(&format!("/nulle/part/{id}.flac")),
                None,
                Some("local"),
            );
            credit(&s, 100 + id, id, "composer", "Górecki", 0);
        }
        tune_core::taches_de_fond::oublier_pour_les_essais();
        tune_core::taches_de_fond::mettre_en_pause(&s.backend, Tache::Enrichissement).unwrap();

        // Lancée en pause : elle refuse de partir plutôt que de se déclarer
        // terminée sans rien faire.
        let r = lancer(State(s.clone()), None).await.into_response();
        assert_eq!(
            r.status(),
            StatusCode::CONFLICT,
            "une passe en pause est partie en silence"
        );

        tune_core::taches_de_fond::oublier_pour_les_essais();
    }

    /// Le `GET` rend les CINQ comptes, jamais une réponse partielle — même au
    /// repos, où ils sont tous à zéro (#1897).
    #[tokio::test]
    async fn le_get_rend_les_cinq_comptes_meme_au_repos() {
        let s = etat();
        let Json(v) = statut(State(s)).await;
        for cle in [
            "rempli",
            "corrige",
            "inchange",
            "sans_fichier",
            "echec_ecriture",
            "retenu_moins_de_noms",
        ] {
            assert_eq!(v[cle], 0, "{cle} absent ou non nul au repos : {v}");
        }
        assert_eq!(v["status"], "idle");
        assert_eq!(v["pistes_concernees"], 0);
    }
}
