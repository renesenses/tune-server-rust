# Réparer une playlist

Reconnaissance du 14/09/2026, sur `origin/main` des deux dépôts, **et sur les
serveurs .18 et .15**. Aucun code modifié. Tout ce qui suit est lu dans le
code ou mesuré sur un serveur vivant.

---

## Ce que la mesure dit, avant toute chose

Le chantier demandé était « finir `recoverPlaylist` », la fonction qui répare
une playlist dont des fichiers ont disparu.

**Cette fonction ne répare rien de ce qui casse réellement les playlists.**

Mesuré sur le **.15**, en appelant `POST /playlists/{id}/recover` sur les 43
playlists :

| | |
|---|---|
| playlists | **43** |
| pistes référencées | **508** |
| pistes présentes | **80** |
| **références orphelines** | **428 — soit 84 %** |
| pistes dont seul le FICHIER manque | **0** |

Sur le **.18** : 13 playlists, 135 pistes, **0 absente**.

Le cas que `recoverPlaylist` sait diagnostiquer — la piste existe, son fichier
a bougé — **ne se produit sur aucun des deux serveurs**. Celui qui détruit les
playlists en production en représente 84 %, et `recoverPlaylist` ne le voit
même pas correctement.

---

## Ce que « orpheline » veut dire, et comment on le sait

`recover_playlist` (`tune-server/src/routes/playlists.rs:1376`) lit chaque
piste ainsi :

```rust
let (title, artist, present) = match trepo.get(*tid) {
    Ok(Some(t)) => { /* … teste le fichier … */ }
    _ => (String::new(), String::new(), false),
};
```

Les 428 pistes « indisponibles » du .15 reviennent **toutes avec un titre vide
et un artiste vide**. C'est la branche `_` : `trepo.get(track_id)` **ne rend
rien**. La piste n'existe plus du tout dans la base — ce n'est pas son fichier
qui manque, c'est sa ligne.

Conséquence directe et lourde : **on ne peut pas réparer par le titre, puisque
le titre est parti avec la ligne.** Toute la stratégie « proposer un candidat
par titre et artiste » s'effondre sur ce cas.

---

## La cause : les deux moteurs ne se comportent pas pareil

C'est le cœur du problème, et il est net.

**SQLite** (`tune-core/src/db/sqlite.rs:486`) :

```sql
track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE
```

et `PRAGMA foreign_keys=ON` est bien posé (`sqlite.rs:96`). Quand le prune
post-scan supprime une piste, la ligne de playlist **disparaît avec elle**. La
playlist rétrécit, mais elle reste cohérente.

**PostgreSQL** (`tune-core/src/db/pg_migrate.rs:354`) :

```sql
CREATE TABLE IF NOT EXISTS playlist_tracks (
    id TEXT PRIMARY KEY,
    playlist_id TEXT NOT NULL,
    track_id TEXT NOT NULL,
    position TEXT NOT NULL DEFAULT 0
);
```

**Aucune clé étrangère. Aucune cascade.** La ligne reste, et pointe dans le
vide.

Le .15 est sur PostgreSQL. D'où 84 %.

⚠️ Au passage, et sans rapport avec ce chantier : les quatre colonnes sont
`TEXT`, y compris `position`. Trier des positions en texte donne
`1, 10, 11, 2` — à vérifier séparément.

---

## Pourquoi les pistes disparaissent, et pourquoi c'est normal

Le code le documente déjà, pour un autre objet.
`tune-core/src/db/favorites_reconcile.rs:1-22` :

> *Les favoris par profil référencent des rowids de `albums`/`tracks`/
> `artists`. Ces ids ne survivent PAS à tous les scans : une racine music
> déplacée/remplacée fait passer chaque fichier par le prune post-scan
> (ancien chemin absent → piste supprimée, album orphelin nettoyé) puis par
> une ré-insertion sous un nouvel id ; un « library clear » + rescan
> réattribue tout. Résultat (bug .18, v0.9.50) : cœurs éteints partout et
> filtre « Favoris » vide — les favoris pointent des ids morts.*

**C'est mot pour mot ce qui arrive aujourd'hui aux playlists du .15.** Le
diagnostic a déjà été fait, et la solution déjà écrite — pour les favoris
seulement.

---

## La solution existe déjà, pour un autre objet

`favorites_reconcile` répare les favoris en deux temps :

1. **Instantané d'identité** — à l'ajout, on fige `item_name`, `item_artist`,
   `item_path` (migration v66 SQLite, 017 PG). L'identité stable survit au
   renouvellement d'identifiant.
2. **Réconciliation** — au démarrage et après chaque scan, tout favori dont
   l'item n'existe plus est re-rattaché à l'item vivant retrouvé par identité
   (piste : chemin, puis titre + artiste).

`playlist_tracks` **n'a aucune des deux**. Ni instantané, ni réconciliation.

Le chantier n'est donc pas « finir `recoverPlaylist` ». C'est **faire pour les
playlists ce qui a déjà été fait pour les favoris** — avec un modèle éprouvé
sous les yeux.

🔴 Et il y a une urgence que `favorites_reconcile` n'avait pas : **tant que
l'instantané n'existe pas, chaque scan détruit un peu plus d'information sans
retour.** Les 428 lignes du .15 sont probablement irrécupérables — on ne sait
même plus quel morceau elles désignaient.

---

## Ce qui revient à Bertrand

**1. Confirmez-vous que les playlists du .15 sont bien cassées à l'écran ?**
La mesure dit 84 %. Une vérification à l'œil — ouvrir une playlist du .15 et
compter les pistes affichées — confirmerait que le diagnostic ne ment pas.
C'est cinq minutes, et ça vaut mieux que ma seule lecture d'API.

**2. Que fait-on des 428 lignes déjà orphelines ?** Trois options, et aucune
n'est bonne :
- les **laisser** : les playlists restent trouées, à jamais ;
- les **effacer** : les playlists rétrécissent mais redeviennent honnêtes —
  c'est ce que SQLite fait déjà tout seul ;
- tenter de les **retrouver** par l'historique d'écoute, comme
  `favorites_reconcile` le fait en dernier recours pour les bibliothèques déjà
  cassées. Coûteux, et sans garantie.

**3. `recoverPlaylist` : on le finit, ou on le retire ?** Il diagnostique un
cas réel (fichier déplacé, piste vivante) qui ne se produit sur aucun de vos
deux serveurs. Sa moitié « réparation » est du théâtre — voir l'annexe. Le
finir coûte plus cher que ce qu'il rapporte tant que le vrai défaut n'est pas
traité.

---

## Découpe proposée

### Phase 0 — Arrêter l'hémorragie *(le plus urgent)*

Poser la clé étrangère manquante sur PostgreSQL, ou à défaut un nettoyage
post-scan explicite. Après cette phase, une playlist rétrécit au lieu de se
trouer — le comportement que SQLite a déjà.

**Témoin** : sur une base PG, supprimer une piste référencée par une playlist ;
`get_track_ids` ne doit plus la rendre.
**Contre-épreuve** : retirer la contrainte doit faire réapparaître la ligne
orpheline — et rougir en la nommant.

### Phase 1 — L'instantané d'identité

Ajouter à `playlist_tracks` les colonnes d'identité, sur le modèle de
`favorites` : titre, artiste, chemin, figés à l'ajout. Migration SQLite **et**
PostgreSQL, et rattrapage des lignes existantes encore vivantes.

**Témoin** : ajouter une piste à une playlist fige son identité.
**Contre-épreuve** : sans le rattrapage, les 80 pistes encore vivantes du .15
resteraient sans identité — c'est mesurable.

### Phase 2 — La réconciliation

Au démarrage et après chaque scan, re-rattacher les lignes dont la piste a
disparu, par chemin puis par titre + artiste. C'est l'exact pendant de
`favorites_reconcile`, dont une bonne part doit pouvoir se partager.

**Témoin** : une playlist dont les pistes ont été supprimées puis ré-insérées
sous de nouveaux identifiants retrouve ses pistes après réconciliation.
**Contre-épreuve** : retirer l'appel post-scan doit laisser la playlist trouée.

### Phase 3 — `recoverPlaylist`, si vous le voulez encore

Voir l'annexe. À juger **après** les phases 0 à 2, quand on saura s'il reste
quelque chose à réparer.

---

## Ce que je déconseille

**Commencer par `recoverPlaylist`.** Il répare un cas qui ne se produit pas,
pendant que le cas qui se produit détruit 84 % des playlists de production.

**Livrer la phase 1 sans la phase 0.** Figer l'identité pendant que les lignes
continuent de s'orpheliner, c'est courir derrière la fuite.

**Effacer les 428 lignes avant d'avoir tenté de les lire.** Elles sont peut-être
retrouvables par l'historique d'écoute. Une fois effacées, non.

---

## Annexe — pourquoi `recoverPlaylist` est du théâtre

Trois faits, pour mémoire, si le sujet revient.

**Le serveur ne propose jamais rien.** `recover_playlist` pousse
`"alternatives": []` dans chaque piste, littéralement (`playlists.rs:1418`).
Aucune recherche n'est faite.

**`apply_recovery` ne remplace rien.** Sa signature (`playlists.rs:1617`) ne
prend **aucun corps** : il recompte les fichiers présents. Le client lui envoie
pourtant `[{track_id, new_source, new_source_id}]` — ignoré faute de
paramètre pour le lire.

**L'écran est prêt à mentir.** `applyOneRecovery`
(`PlaylistManagerView:1080`) marque la piste `status: 'available'` **dans
l'écran**, sans que rien n'ait changé côté serveur, et avale l'échec par
`console.error`. Elle n'est aujourd'hui pas atteignable — le bouton se
construit depuis `alternatives`, toujours vide. **Mais le jour où le serveur
en renverrait une, cet écran afficherait « réparé » sur des pistes toujours
manquantes.**

C'est le quatrième « écrit mais pas branché » de ce client, et le plus
dangereux : les trois autres étaient inertes.

**Le moteur qui manque existe pourtant** : `match_tracks`
(`playlists.rs:1807`) rapproche titre/artiste vers des candidats locaux via
`track_repo.search`. Si `recoverPlaylist` devait être fini un jour, c'est là
qu'il faudrait le brancher — et corriger l'écran **avant**, pas après.
