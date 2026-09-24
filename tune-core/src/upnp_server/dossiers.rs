//! Parcours par dossiers du serveur média (#4318).
//!
//! Tades (fil forum 1818, « Jplay ») : « Toujours pas de vue par
//! répertoire ». Bertrand, recette v0.9.161 : « Serveur UPnP de Tune → ajout
//! répertoires ou dossiers pour être accessibles dans JPlay ». Les sept
//! rayons de la racine rangeaient la bibliothèque par tags ; un point de
//! contrôle qui navigue par dossiers n'y trouvait pas l'arborescence du
//! disque. La vue Répertoires de l'interface web (`GET /library/browse/dir`)
//! existait déjà : c'est l'exposition UPnP qui manquait.
//!
//! # D'où vient l'arborescence : de la BASE, pas du disque
//!
//! Chaque dossier publié est déduit des `tracks.file_path` rangés sous les
//! dossiers musicaux configurés (`music_dirs`) — exactement le découpage que
//! la vue Répertoires fait pour ses compteurs
//! ([`crate::db::track_repo::compter_pistes_par_sous_dossier`]). Trois
//! conséquences, voulues :
//!
//! * **Aucun dossier vide.** Un dossier n'existe ici que s'il contient au
//!   moins une piste à une profondeur quelconque — la règle de
//!   `ROOT_CONTAINERS` : un dossier visible et vide se lit comme une
//!   bibliothèque cassée.
//! * **Chaque item est une piste de la bibliothèque**, émise par le MÊME
//!   `didl_track_item` que les autres rayons : même identifiant `track/N`,
//!   même URL de flux (`/api/v1/library/tracks/N/audio`), mêmes métadonnées.
//!   Un fichier que le scanner n'a pas retenu ne serait pas jouable — aucune
//!   route ne sert un chemin arbitraire — et n'est donc pas publié.
//! * **Rien ne sort des racines configurées.** Le serveur ne lit jamais le
//!   disque pour répondre : il ne publie que des chemins déjà en base ET
//!   sous une racine. Un lien symbolique qui pointe hors racine n'est jamais
//!   suivi par cette vue, et un identifiant forgé (`..`, séparateur,
//!   segment vide) est refusé au décodage — il rend un DIDL vide, comme tout
//!   identifiant inconnu. La vue n'expose aucun fichier que
//!   `/library/tracks/{id}/audio` ne servait déjà.
//!
//! # Identifiants
//!
//! * `folders` — le rayon racine ;
//! * `folder/<clef>` — une racine configurée, où `<clef>` est un condensat
//!   court de son chemin : stable entre les démarrages ET si l'ordre des
//!   racines change dans les réglages (un index ne l'aurait pas été) ;
//! * `folder/<clef>/<seg>/<seg>…` — un sous-dossier, chaque segment encodé
//!   par `urlencoding` (qui échappe aussi `/`), donc découpable sans
//!   ambiguïté.
//!
//! Les pistes CUE virtuelles n'ont pas de `file_path` : elles n'apparaissent
//! pas ici, comme dans la vue Répertoires.

use std::cmp::Ordering;

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization as _;

use super::{
    DidlResult, UpnpState, didl_container, didl_track_item, didl_wrap, empty_didl, paginer,
    sans_accents_minuscule,
};
use crate::db::models::Track;
use crate::db::track_repo::TrackRepo;

/// Identifiant du rayon racine.
pub(super) const ID_RAYON: &str = "folders";
/// Titre du rayon racine — en anglais comme ses sept voisins.
pub(super) const TITRE_RAYON: &str = "Folders";
/// Classe du rayon racine, la même que ses voisins.
pub(super) const CLASSE_RAYON: &str = "object.container";
/// Préfixe des identifiants de dossier.
const PREFIXE: &str = "folder/";
/// Classe d'un dossier : `storageFolder` est ce que les points de contrôle
/// qui naviguent « par dossiers » reconnaissent comme un répertoire.
const CLASSE_DOSSIER: &str = "object.container.storageFolder";

/// Un identifiant de ce module ?
pub(super) fn est_a_nous(id: &str) -> bool {
    id == ID_RAYON || id.starts_with(PREFIXE)
}

/// Une racine configurée, telle qu'elle est publiée.
struct Racine {
    /// Condensat court du chemin : la partie stable de l'identifiant.
    clef: String,
    /// Chemin normalisé (NFC, sans séparateur final) — la forme de
    /// `tracks.file_path`.
    chemin: String,
    /// Nom affiché : le dernier segment, ou le chemin entier s'il n'en a pas
    /// ou si deux racines portent le même nom.
    titre: String,
}

/// Un dossier décodé depuis son identifiant.
struct Dossier {
    racine: Racine,
    segments: Vec<String>,
}

impl Dossier {
    /// Le chemin tel qu'il figure en tête de `tracks.file_path`.
    fn chemin(&self) -> String {
        let sep = std::path::MAIN_SEPARATOR.to_string();
        let mut c = self.racine.chemin.clone();
        for s in &self.segments {
            c.push_str(&sep);
            c.push_str(s);
        }
        c
    }

    fn id(&self) -> String {
        id_dossier(&self.racine.clef, &self.segments)
    }

    /// Le parent : la racine remonte au rayon `folders`.
    fn id_parent(&self) -> String {
        match self.segments.split_last() {
            None => ID_RAYON.to_string(),
            Some((_, avant)) => id_dossier(&self.racine.clef, avant),
        }
    }

    fn titre(&self) -> &str {
        self.segments
            .last()
            .map(String::as_str)
            .unwrap_or(&self.racine.titre)
    }
}

fn id_dossier(clef: &str, segments: &[String]) -> String {
    let mut id = format!("{PREFIXE}{clef}");
    for s in segments {
        id.push('/');
        id.push_str(&urlencoding::encode(s));
    }
    id
}

fn clef_de_racine(chemin: &str) -> String {
    let condensat = Sha256::digest(chemin.as_bytes());
    condensat[..8].iter().map(|o| format!("{o:02x}")).collect()
}

/// Les racines configurées (`music_dirs`), dans l'ordre des réglages, sans
/// doublon. Le réglage est écrit au démarrage depuis `TUNE_MUSIC_DIRS` /
/// la configuration quand il manque (`startup.rs`) : c'est la seule source.
fn racines_configurees(state: &UpnpState) -> Vec<Racine> {
    let brutes: Vec<String> =
        crate::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .get("music_dirs")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
    let mut chemins: Vec<String> = Vec::new();
    for b in &brutes {
        let norm = crate::scanner::walker::normalize_path(b);
        if norm.is_empty() {
            continue;
        }
        let nfc: String = norm.nfc().collect();
        let chemin = nfc.trim_end_matches(['/', '\\']).to_string();
        // Une racine de lecteur (« / », « D:\ ») rogne à vide ou à « D: » :
        // c'est bien le préfixe de ses fichiers une fois le séparateur remis.
        if !chemins.contains(&chemin) {
            chemins.push(chemin);
        }
    }
    let noms: Vec<String> = chemins
        .iter()
        .map(|c| {
            std::path::Path::new(c)
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| !n.is_empty())
                .unwrap_or(c)
                .to_string()
        })
        .collect();
    chemins
        .iter()
        .zip(&noms)
        .map(|(chemin, nom)| {
            let homonyme = noms.iter().filter(|n| *n == nom).count() > 1;
            Racine {
                clef: clef_de_racine(chemin),
                chemin: chemin.clone(),
                titre: if homonyme || nom.is_empty() {
                    chemin.clone()
                } else {
                    nom.clone()
                },
            }
        })
        .collect()
}

/// Un segment d'identifiant acceptable : un nom de dossier, rien d'autre.
/// `..` remonterait hors de la racine, `.` et le vide désignent le même
/// dossier sous un second identifiant, et un séparateur glisserait deux
/// segments dans un seul.
fn segment_valide(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0'])
}

/// Décode `folder/<clef>/<seg>…`. `None` pour tout identifiant qui ne
/// désigne pas un dossier sous une racine configurée.
fn decoder(state: &UpnpState, id: &str) -> Option<Dossier> {
    let reste = id.strip_prefix(PREFIXE)?;
    let mut morceaux = reste.split('/');
    let clef = morceaux.next()?;
    let racine = racines_configurees(state)
        .into_iter()
        .find(|r| r.clef == clef)?;
    let mut segments = Vec::new();
    for m in morceaux {
        let seg = urlencoding::decode(m).ok()?.into_owned();
        if !segment_valide(&seg) {
            return None;
        }
        segments.push(seg);
    }
    let dossier = Dossier { racine, segments };
    // Seconde garde, redondante par construction : le chemin reconstruit
    // reste sous sa racine, composant par composant.
    if !std::path::Path::new(&dossier.chemin()).starts_with(dossier_racine_ou_sep(&dossier.racine))
    {
        return None;
    }
    Some(dossier)
}

/// La racine comme `Path` — une racine de système (`/`) a été rognée à vide.
fn dossier_racine_ou_sep(r: &Racine) -> String {
    if r.chemin.is_empty() {
        std::path::MAIN_SEPARATOR.to_string()
    } else {
        r.chemin.clone()
    }
}

/// Un enfant d'un dossier.
enum Enfant {
    Dossier { nom: String, nb_enfants: u64 },
    Piste(Track),
}

/// Le nom de fichier d'une piste — sa clef de tri dans le dossier.
fn nom_de_fichier(t: &Track) -> String {
    t.file_path
        .as_deref()
        .and_then(|p| std::path::Path::new(p).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Les enfants d'un dossier : les sous-dossiers peuplés d'abord, puis les
/// pistes, chacun en tri naturel — « CD2 » avant « CD10 », « 02 - » avant
/// « 10 - », comme un explorateur de fichiers.
fn enfants(state: &UpnpState, chemin: &str) -> Vec<Enfant> {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut dossiers = repo.sous_dossiers_peuples(chemin).unwrap_or_default();
    dossiers.sort_by(|a, b| comparer_naturel(&a.0, &b.0));
    let mut pistes: Vec<(String, Track)> = repo
        .pistes_du_dossier(chemin)
        .unwrap_or_default()
        .into_iter()
        .map(|t| (nom_de_fichier(&t), t))
        .collect();
    pistes.sort_by(|a, b| comparer_naturel(&a.0, &b.0));
    dossiers
        .into_iter()
        .filter(|(nom, _)| segment_valide(nom))
        .map(|(nom, nb_enfants)| Enfant::Dossier { nom, nb_enfants })
        .chain(pistes.into_iter().map(|(_, t)| Enfant::Piste(t)))
        .collect()
}

/// Nombre d'enfants d'un dossier, pour `BrowseMetadata` et pour le
/// `childCount` d'une racine. Compté par la MÊME fonction que le Browse :
/// un compteur qui diverge de ce que le dossier ouvre serait pire que rien.
fn nb_enfants(state: &UpnpState, chemin: &str) -> u64 {
    enfants(state, chemin).len() as u64
}

/// Les racines qui contiennent au moins une piste.
fn racines_peuplees(state: &UpnpState) -> Vec<Racine> {
    let repo = TrackRepo::with_backend(state.backend.clone());
    racines_configurees(state)
        .into_iter()
        .filter(|r| repo.dossier_peuple(&r.chemin).unwrap_or(false))
        .collect()
}

/// Le rayon `folders` a-t-il quelque chose à ouvrir ? S'il n'a rien, il
/// n'est pas publié à la racine.
pub(super) fn publie(state: &UpnpState) -> bool {
    !racines_peuplees(state).is_empty()
}

/// Le `childCount` du rayon : le nombre de racines que son Browse rendra.
pub(super) fn nb_racines(state: &UpnpState) -> u64 {
    racines_peuplees(state).len() as u64
}

/// `BrowseMetadata` d'un identifiant de ce module. `None` pour un dossier
/// inconnu, forgé ou vide.
pub(super) fn decrire(state: &UpnpState, id: &str) -> Option<String> {
    if id == ID_RAYON {
        let n = nb_racines(state);
        return (n > 0).then(|| didl_container(ID_RAYON, "0", TITRE_RAYON, CLASSE_RAYON, Some(n)));
    }
    let dossier = decoder(state, id)?;
    let n = nb_enfants(state, &dossier.chemin());
    (n > 0).then(|| {
        didl_container(
            &dossier.id(),
            &dossier.id_parent(),
            dossier.titre(),
            CLASSE_DOSSIER,
            Some(n),
        )
    })
}

/// `BrowseDirectChildren` d'un identifiant de ce module, paginé.
pub(super) fn parcourir(state: &UpnpState, id: &str, start: u64, count: u64) -> DidlResult {
    if id == ID_RAYON {
        let racines: Vec<(Racine, u64)> = racines_peuplees(state)
            .into_iter()
            .map(|r| {
                let n = nb_enfants(state, &r.chemin);
                (r, n)
            })
            .collect();
        let (page, total) = paginer(racines, start, count);
        let mut inner = String::new();
        for (r, n) in &page {
            inner.push_str(&didl_container(
                &id_dossier(&r.clef, &[]),
                ID_RAYON,
                &r.titre,
                CLASSE_DOSSIER,
                Some(*n),
            ));
        }
        return DidlResult {
            xml: didl_wrap(&inner),
            total,
            returned: page.len() as u64,
        };
    }
    let Some(dossier) = decoder(state, id) else {
        return empty_didl();
    };
    let (page, total) = paginer(enfants(state, &dossier.chemin()), start, count);
    let base_url = state.base_url();
    let mut inner = String::new();
    for e in &page {
        match e {
            Enfant::Dossier { nom, nb_enfants } => {
                let mut segments = dossier.segments.clone();
                segments.push(nom.clone());
                inner.push_str(&didl_container(
                    &id_dossier(&dossier.racine.clef, &segments),
                    &dossier.id(),
                    nom,
                    CLASSE_DOSSIER,
                    Some(*nb_enfants),
                ));
            }
            Enfant::Piste(t) => inner.push_str(&didl_track_item(t, &dossier.id(), &base_url)),
        }
    }
    DidlResult {
        xml: didl_wrap(&inner),
        total,
        returned: page.len() as u64,
    }
}

/// Tri naturel : les suites de chiffres se comparent par leur VALEUR, le
/// reste sans casse ni accents. Égalité départagée par le texte brut, pour
/// un ordre total et stable entre deux requêtes — la pagination en dépend.
fn comparer_naturel(a: &str, b: &str) -> Ordering {
    let (ka, kb) = (cle_naturelle(a), cle_naturelle(b));
    ka.cmp(&kb).then_with(|| a.cmp(b))
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Morceau {
    // Les nombres passent avant le texte, comme dans un explorateur.
    Nombre { longueur: usize, chiffres: String },
    Texte(String),
}

fn cle_naturelle(s: &str) -> Vec<Morceau> {
    let replie = sans_accents_minuscule(s);
    let mut morceaux = Vec::new();
    let mut courant = String::new();
    let mut en_chiffres = false;
    let pousser = |courant: &mut String, en_chiffres: bool, morceaux: &mut Vec<Morceau>| {
        if courant.is_empty() {
            return;
        }
        let m = std::mem::take(courant);
        if en_chiffres {
            // Les zéros de tête ne comptent pas : « 007 » vaut « 7 ». La
            // longueur passe avant les chiffres, donc 9 < 10.
            let chiffres = m.trim_start_matches('0').to_string();
            morceaux.push(Morceau::Nombre {
                longueur: chiffres.len(),
                chiffres,
            });
        } else {
            morceaux.push(Morceau::Texte(m));
        }
    };
    for c in replie.chars() {
        let chiffre = c.is_ascii_digit();
        if chiffre != en_chiffres {
            pousser(&mut courant, en_chiffres, &mut morceaux);
            en_chiffres = chiffre;
        }
        courant.push(c);
    }
    pousser(&mut courant, en_chiffres, &mut morceaux);
    morceaux
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::backend::DbBackend;
    use std::sync::Arc;

    /// « Tout » : ce que le trajet SOAP met à la place de `RequestedCount = 0`
    /// (`UNLIMITED_BROWSE_COUNT`). Appelé directement, `parcourir(…, 0, 0)`
    /// demanderait une page vide.
    const TOUT: u64 = u64::MAX;

    /// Une vraie arborescence temporaire, et les pistes que le scanner y
    /// aurait trouvées. Les fichiers existent sur le disque ; la vue, elle,
    /// ne lit que la base — c'est ce que le test vérifie aussi.
    struct Banc {
        state: UpnpState,
        racine: String,
        _tmp: tempfile::TempDir,
    }

    fn sep() -> String {
        std::path::MAIN_SEPARATOR.to_string()
    }

    fn banc(fichiers: &[&str]) -> Banc {
        use crate::db::sqlite::SqliteDb;
        let tmp = tempfile::tempdir().unwrap();
        let racine: String = tmp.path().join("Musique").to_string_lossy().nfc().collect();
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        crate::db::settings_repo::SettingsRepo::with_backend(backend.clone())
            .set("music_dirs", &serde_json::to_string(&[&racine]).unwrap())
            .unwrap();
        let repo = TrackRepo::with_backend(backend.clone());
        for rel in fichiers {
            let chemin = format!("{racine}{}{}", sep(), rel.replace('/', &sep()));
            std::fs::create_dir_all(std::path::Path::new(&chemin).parent().unwrap()).unwrap();
            std::fs::write(&chemin, b"").unwrap();
            let titre = std::path::Path::new(rel)
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let mut t = Track::new(titre);
            t.file_path = Some(chemin);
            t.format = Some("flac".into());
            repo.create(&t).unwrap();
        }
        Banc {
            state: UpnpState::new(backend, 8888, None),
            racine,
            _tmp: tmp,
        }
    }

    /// Les `(balise, id, titre)` des enfants d'un DIDL. Le document passe
    /// d'abord par un vrai parseur XML — un DIDL mal formé fait échouer le
    /// test ici — puis les champs sont relevés et déséchappés.
    fn lire(didl: &str) -> Vec<(String, String, String)> {
        let mut r = quick_xml::Reader::from_str(didl);
        r.config_mut().check_end_names = true;
        loop {
            match r.read_event() {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("DIDL-Lite mal formé : {e} — {didl}"),
            }
        }
        let deseschapper = |s: &str| {
            quick_xml::escape::unescape(s)
                .expect("échappement")
                .into_owned()
        };
        let mut out = Vec::new();
        let mut reste = didl;
        loop {
            let (debut, balise) = match (reste.find("<container "), reste.find("<item ")) {
                (Some(c), Some(i)) if c < i => (c, "container"),
                (_, Some(i)) => (i, "item"),
                (Some(c), None) => (c, "container"),
                (None, None) => break,
            };
            reste = &reste[debut..];
            let a = reste.find(" id=\"").unwrap() + 5;
            let b = a + reste[a..].find('"').unwrap();
            let id = deseschapper(&reste[a..b]);
            let t = reste.find("<dc:title>").unwrap() + "<dc:title>".len();
            let u = t + reste[t..].find("</dc:title>").unwrap();
            let titre = deseschapper(&reste[t..u]);
            out.push((balise.to_string(), id, titre));
            reste = &reste[u..];
        }
        out
    }

    fn arbre() -> Banc {
        banc(&[
            "Jazz/Miles Davis/Kind of Blue/01 So What.flac",
            "Jazz/Miles Davis/Kind of Blue/02 Freddie Freeloader.flac",
            "Jazz/Miles Davis/Kind of Blue/10 Bonus.flac",
            "Jazz/Miles Davis/Kind of Blue/9 Interlude.flac",
            "Classique/Coffret/CD10/01 Final.flac",
            "Classique/Coffret/CD2/01 Adagio.flac",
            "Classique/Coffret/CD1/01 Ouverture.flac",
            "Seul.flac",
        ])
    }

    #[test]
    fn le_rayon_liste_la_racine_configuree_et_s_y_publie() {
        let b = arbre();
        assert!(publie(&b.state));
        let r = parcourir(&b.state, ID_RAYON, 0, TOUT);
        let enfants = lire(&r.xml);
        assert_eq!(r.total, 1);
        assert_eq!(enfants.len(), 1);
        assert_eq!(enfants[0].0, "container");
        assert_eq!(enfants[0].2, "Musique");
        assert!(r.xml.contains("object.container.storageFolder"));
        // Classique, Jazz et le fichier posé à la racine.
        assert!(r.xml.contains("childCount=\"3\""), "{}", r.xml);
    }

    #[test]
    fn la_racine_liste_dossiers_puis_pistes_en_tri_naturel() {
        let b = arbre();
        let id_racine = lire(&parcourir(&b.state, ID_RAYON, 0, TOUT).xml)[0]
            .1
            .clone();
        let r = parcourir(&b.state, &id_racine, 0, TOUT);
        let e = lire(&r.xml);
        let titres: Vec<&str> = e.iter().map(|x| x.2.as_str()).collect();
        assert_eq!(titres, ["Classique", "Jazz", "Seul"]);
        assert_eq!(e[2].0, "item");
        assert!(
            e[2].1.starts_with("track/"),
            "item = piste de la bibliothèque"
        );
        assert_eq!(r.total, 3);
        // L'item porte la même URL de flux que les autres rayons.
        assert!(r.xml.contains("/api/v1/library/tracks/"));
    }

    #[test]
    fn un_sous_dossier_s_ouvre_et_annonce_ce_qu_il_ouvre() {
        let b = arbre();
        let id_racine = lire(&parcourir(&b.state, ID_RAYON, 0, TOUT).xml)[0]
            .1
            .clone();
        let classique = lire(&parcourir(&b.state, &id_racine, 0, TOUT).xml)[0]
            .1
            .clone();
        let coffret = lire(&parcourir(&b.state, &classique, 0, TOUT).xml);
        assert_eq!(coffret.len(), 1);
        let cds = parcourir(&b.state, &coffret[0].1, 0, TOUT);
        let titres: Vec<String> = lire(&cds.xml).into_iter().map(|x| x.2).collect();
        assert_eq!(titres, ["CD1", "CD2", "CD10"], "tri naturel");

        // Chaque conteneur annonce le nombre d'enfants que son Browse rend,
        // et se décrit par BrowseMetadata avec le bon parent.
        for (_, id, _) in lire(&cds.xml) {
            let ouvert = parcourir(&b.state, &id, 0, TOUT);
            let meta = decrire(&b.state, &id).expect("dossier publié mais indescriptible");
            assert!(meta.contains(&format!("childCount=\"{}\"", ouvert.total)));
            assert!(meta.contains(&format!("parentID=\"{}\"", coffret[0].1)));
        }
        let kob = format!("{id_racine}/Jazz/Miles%20Davis/Kind%20of%20Blue");
        let pistes: Vec<String> = lire(&parcourir(&b.state, &kob, 0, TOUT).xml)
            .into_iter()
            .map(|x| x.2)
            .collect();
        assert_eq!(
            pistes,
            [
                "01 So What",
                "02 Freddie Freeloader",
                "9 Interlude",
                "10 Bonus"
            ]
        );
    }

    #[test]
    fn la_pagination_respecte_starting_index_et_requested_count() {
        let b = arbre();
        let id_racine = lire(&parcourir(&b.state, ID_RAYON, 0, TOUT).xml)[0]
            .1
            .clone();
        let kob = format!("{id_racine}/Jazz/Miles%20Davis/Kind%20of%20Blue");
        let p1 = parcourir(&b.state, &kob, 0, 3);
        let p2 = parcourir(&b.state, &kob, 3, 3);
        assert_eq!((p1.total, p1.returned), (4, 3));
        assert_eq!((p2.total, p2.returned), (4, 1));
        assert_eq!(lire(&p2.xml)[0].2, "10 Bonus");
        let hors = parcourir(&b.state, &kob, 10, 5);
        assert_eq!((hors.total, hors.returned), (4, 0));
    }

    #[test]
    fn aucune_sortie_de_racine_par_identifiant_forge() {
        // Une piste HORS racine, en base : elle ne doit jamais apparaître.
        let b = arbre();
        let repo = TrackRepo::with_backend(b.state.backend.clone());
        let parent = std::path::Path::new(&b.racine)
            .parent()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let mut dehors = Track::new("Secret".into());
        dehors.file_path = Some(format!("{parent}{}secret.flac", sep()));
        repo.create(&dehors).unwrap();
        // Et une ligne dont le chemin porte un `..` littéral sous la racine :
        // c'est elle qui fait mordre la garde de décodage. Sans le refus de
        // `..`, `folder/<clef>/..` ouvrirait « <racine>/.. » et la montrerait.
        let mut remonte = Track::new("Secret".into());
        remonte.file_path = Some(format!("{}{s}..{s}secret2.flac", b.racine, s = sep()));
        repo.create(&remonte).unwrap();

        let id_racine = lire(&parcourir(&b.state, ID_RAYON, 0, TOUT).xml)[0]
            .1
            .clone();
        for forge in [
            format!("{id_racine}/.."),
            format!("{id_racine}/%2E%2E"),
            format!("{id_racine}/Jazz/../.."),
            format!("{id_racine}/Jazz%2F..%2F.."),
            format!("{id_racine}/"),
            format!("{id_racine}/."),
            "folder/0000000000000000".to_string(),
            "folder/".to_string(),
        ] {
            let r = parcourir(&b.state, &forge, 0, TOUT);
            assert_eq!(r.total, 0, "{forge} ne doit rien ouvrir");
            assert!(!r.xml.contains("Secret"));
            assert!(decrire(&b.state, &forge).is_none(), "{forge}");
        }
        // Et le parcours honnête ne la montre pas non plus.
        let racine = parcourir(&b.state, &id_racine, 0, TOUT);
        assert!(!racine.xml.contains("Secret"));
    }

    #[test]
    fn un_lien_symbolique_hors_racine_n_est_pas_suivi() {
        #[cfg(unix)]
        {
            let b = arbre();
            let dehors = tempfile::tempdir().unwrap();
            std::fs::write(dehors.path().join("fuite.flac"), b"").unwrap();
            std::os::unix::fs::symlink(dehors.path(), format!("{}/Lien", b.racine)).unwrap();
            let id_racine = lire(&parcourir(&b.state, ID_RAYON, 0, TOUT).xml)[0]
                .1
                .clone();
            // Le lien existe sur le disque ; la vue ne lit pas le disque.
            let racine = parcourir(&b.state, &id_racine, 0, TOUT);
            assert!(!racine.xml.contains("Lien"));
            let r = parcourir(&b.state, &format!("{id_racine}/Lien"), 0, TOUT);
            assert_eq!(r.total, 0);
        }
    }

    #[test]
    fn sans_racine_peuplee_le_rayon_n_est_pas_publie() {
        let b = banc(&[]);
        assert!(!publie(&b.state));
        assert!(decrire(&b.state, ID_RAYON).is_none());
        assert_eq!(parcourir(&b.state, ID_RAYON, 0, TOUT).total, 0);
    }

    #[test]
    fn la_clef_de_racine_est_stable() {
        let b = arbre();
        let a = lire(&parcourir(&b.state, ID_RAYON, 0, TOUT).xml)[0]
            .1
            .clone();
        let c = lire(&parcourir(&b.state, ID_RAYON, 0, TOUT).xml)[0]
            .1
            .clone();
        assert_eq!(a, c);
        assert_eq!(a, format!("folder/{}", clef_de_racine(&b.racine)));
    }

    #[test]
    fn un_nom_de_dossier_exotique_fait_l_aller_retour() {
        let b = banc(&["AC_DC & 100% Live/Back in Black #1/01 Hells Bells.flac"]);
        let id_racine = lire(&parcourir(&b.state, ID_RAYON, 0, TOUT).xml)[0]
            .1
            .clone();
        let d1 = lire(&parcourir(&b.state, &id_racine, 0, TOUT).xml);
        assert_eq!(d1[0].2, "AC_DC & 100% Live");
        let d2 = lire(&parcourir(&b.state, &d1[0].1, 0, TOUT).xml);
        assert_eq!(d2[0].2, "Back in Black #1");
        let p = lire(&parcourir(&b.state, &d2[0].1, 0, TOUT).xml);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].0, "item");
    }

    /// La réponse SOAP complète d'un `Browse` — le trajet réel du point de
    /// contrôle, échappement de `<Result>` compris.
    fn soap(state: &UpnpState, object_id: &str, drapeau: &str, debut: u64, nombre: u64) -> String {
        let corps = format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
    <ObjectID>{}</ObjectID>
    <BrowseFlag>{drapeau}</BrowseFlag>
    <Filter>*</Filter><StartingIndex>{debut}</StartingIndex><RequestedCount>{nombre}</RequestedCount>
    <SortCriteria></SortCriteria>
  </u:Browse></s:Body>
</s:Envelope>"#,
            quick_xml::escape::escape(object_id)
        );
        super::super::browse_action_response(state, &corps)
    }

    fn champ(reponse: &str, balise: &str) -> String {
        let o = format!("<{balise}>");
        let a = reponse.find(&o).unwrap() + o.len();
        let b = a + reponse[a..].find(&format!("</{balise}>")).unwrap();
        reponse[a..b].to_string()
    }

    fn didl(reponse: &str) -> String {
        quick_xml::escape::unescape(&champ(reponse, "Result"))
            .unwrap()
            .into_owned()
    }

    #[test]
    fn par_soap_la_racine_annonce_folders_et_il_s_ouvre() {
        let b = arbre();
        let racine = soap(&b.state, "0", "BrowseDirectChildren", 0, 0);
        // Huit rayons (dont « All Tracks (Shuffle) », fil 1916) + « Folders ».
        assert_eq!(champ(&racine, "TotalMatches"), "9");
        let rayons = lire(&didl(&racine));
        assert_eq!(rayons.last().unwrap().1, "folders");
        assert_eq!(rayons.last().unwrap().2, "Folders");
        // BrowseMetadata("0") annonce ce que la racine ouvre.
        let meta0 = didl(&soap(&b.state, "0", "BrowseMetadata", 0, 0));
        assert!(meta0.contains("childCount=\"9\""), "{meta0}");

        let meta = soap(&b.state, "folders", "BrowseMetadata", 0, 0);
        assert_eq!(champ(&meta, "NumberReturned"), "1");
        let racines = soap(&b.state, "folders", "BrowseDirectChildren", 0, 0);
        let id_racine = lire(&didl(&racines))[0].1.clone();
        let page = soap(&b.state, &id_racine, "BrowseDirectChildren", 1, 1);
        assert_eq!(champ(&page, "NumberReturned"), "1");
        assert_eq!(champ(&page, "TotalMatches"), "3");
        assert_eq!(lire(&didl(&page))[0].2, "Jazz");
        let meta_racine = didl(&soap(&b.state, &id_racine, "BrowseMetadata", 0, 0));
        assert!(meta_racine.contains("parentID=\"folders\""));

        // L'item d'un dossier se décrit comme toute piste publiée.
        let items = lire(&didl(&soap(
            &b.state,
            &id_racine,
            "BrowseDirectChildren",
            2,
            1,
        )));
        assert_eq!(items[0].0, "item");
        let meta_item = soap(&b.state, &items[0].1, "BrowseMetadata", 0, 0);
        assert_eq!(champ(&meta_item, "NumberReturned"), "1");
    }

    #[test]
    fn sans_music_dirs_la_racine_garde_ses_huit_rayons() {
        let b = banc(&[]);
        let racine = soap(&b.state, "0", "BrowseDirectChildren", 0, 0);
        // Sept rayons historiques + « All Tracks (Shuffle) » (fil 1916).
        assert_eq!(champ(&racine, "TotalMatches"), "8");
        assert!(!didl(&racine).contains("\"folders\""));
    }

    #[test]
    fn le_tri_naturel_compare_les_nombres_par_valeur() {
        let mut v = vec!["CD10", "cd2", "CD1", "Émile", "Eric", "007", "8"];
        v.sort_by(|a, b| comparer_naturel(a, b));
        assert_eq!(v, ["007", "8", "CD1", "cd2", "CD10", "Émile", "Eric"]);
    }
}
