-- streaming_item_tags : etiqueter un album de STREAMING (#3699).
--
-- Jumelle PostgreSQL de la migration SQLite 97. Les deux listes sont SEPAREES
-- (`run_migrations` ne prend qu'un `SqliteDb`) : ecrite d'un seul cote, la
-- table manquerait a tout le parc PostgreSQL (.15, .18, Docker) et le premier
-- etiquetage d'un album Qobuz y rendrait une erreur SQL.
--
-- ## Pourquoi une table a part
--
-- `item_tags.item_id` est un ENTIER : la clef primaire d'un objet de la base
-- locale. Un album Qobuz, Tidal ou Bandcamp n'en a pas — il porte la paire
-- `source` + `source_id`. C'est exactement ce que les FAVORIS ont deja
-- constate : `favorites` est indexee sur un entier, donc les favoris de
-- streaming ont recu leur propre table `streaming_favorites`, indexee sur la
-- paire. Cette table-ci suit la meme forme, deliberement : une forme eprouvee
-- vaut mieux qu'un troisieme mecanisme qui divergera.
--
-- ## L'instantane d'affichage
--
-- `title`, `artist`, `album` et `cover_url` sont poses A L'ETIQUETAGE, comme
-- dans `streaming_favorites`. Ce n'est pas de la denormalisation gratuite :
-- c'est ce qui permet a la liste par etiquette de se rendre SANS interroger le
-- catalogue. Un album de streaming peut disparaitre — le service le retire, la
-- licence change, le compte est deconnecte. Un ecran qui hydraterait chaque
-- ligne aupres du service se bloquerait sur le premier `source_id` mort. Avec
-- l'instantane, la ligne s'affiche toujours et seule sa pochette degrade,
-- comme une pochette morte.
--
-- ## PAS de colonne `id`
--
-- La clef naturelle (tag_id, item_type, source, source_id) EST la clef
-- primaire. Meme choix que `favorite_facets` (migration 038) et `task_runs`
-- (040), et pour la meme raison : une colonne `id` impose la divergence
-- AUTOINCREMENT / BIGSERIAL que la bascule SQLite -> PostgreSQL a deja payee
-- cher (#1706). Elle rend aussi l'unicite structurelle : elle porte sur la
-- PAIRE, jamais sur un entier, donc le meme album Qobuz etiquete deux fois ne
-- peut pas creer deux lignes.
--
-- ## `tag_id` en BIGINT et non en TEXT
--
-- `tags.id` est BIGSERIAL sur une base PG neuve (script 005) et remis en
-- BIGINT par la migration 012 sur une base venue de la bascule. La lecture des
-- etiquettes d'un objet joint les deux tables : un `tag_id` TEXT rendrait
-- « operator does not exist: text = bigint ». Cette table est neuve, elle n'a
-- pas l'historique TEXT du schema de bascule, et la copie de donnees lie
-- NATIVEMENT les entiers (voir `insert_batch` dans `pg_migrate.rs`).
--
-- Pas de contrainte de clef etrangere vers `tags`, comme le schema de bascule
-- qui n'en porte aucune : `TagRepo::delete` retire explicitement les lignes de
-- cette table avant l'etiquette, sur les deux moteurs.
--
-- Idempotent : CREATE TABLE / CREATE INDEX IF NOT EXISTS.

CREATE TABLE IF NOT EXISTS streaming_item_tags (
    tag_id BIGINT NOT NULL,
    item_type TEXT NOT NULL,
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,
    title TEXT,
    artist TEXT,
    album TEXT,
    cover_url TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (tag_id, item_type, source, source_id)
);

CREATE INDEX IF NOT EXISTS idx_streaming_item_tags_item
    ON streaming_item_tags(item_type, source, source_id);
