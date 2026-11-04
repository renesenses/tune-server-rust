-- Migration SQLite 136 (#7849) : les notes personnelles en Markdown d'un
-- profil sur un artiste, un album ou une piste.
--
-- Une note par (profil, type, objet). Le texte est gardé brut : le serveur ne
-- l'assainit pas, le client web le fait au rendu.
--
-- Reprise des notes d'album existantes (`album_ratings.note`) : recopiées une
-- fois, sans rien retirer de `album_ratings`, dont la colonne reste lue tant
-- que le client web n'a pas basculé. `INSERT OR IGNORE` : rejouée, la reprise
-- n'écrase jamais une note déjà présente dans la nouvelle table.
-- Jumelle PG : 101.
CREATE TABLE IF NOT EXISTS notes_personnelles (
    profile_id INTEGER NOT NULL,
    item_type TEXT NOT NULL CHECK (item_type IN ('artist', 'album', 'track')),
    item_id INTEGER NOT NULL,
    texte_md TEXT NOT NULL,
    maj_le TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    PRIMARY KEY (profile_id, item_type, item_id)
);
CREATE INDEX IF NOT EXISTS idx_notes_personnelles_objet
    ON notes_personnelles(item_type, item_id);

INSERT OR IGNORE INTO notes_personnelles (profile_id, item_type, item_id, texte_md, maj_le)
SELECT profile_id, 'album', album_id, note,
       COALESCE(created_at, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
FROM album_ratings
WHERE note IS NOT NULL AND TRIM(note) <> '';
