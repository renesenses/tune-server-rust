-- 103_notes_personnelles.sql
--
-- #7849 : les notes personnelles en Markdown d'un profil sur un artiste, un
-- album ou une piste. Une note par (profil, type, objet), texte brut.
-- Jumelle de la migration SQLite 138.
--
-- Reprise des notes d'album existantes (`album_ratings.note`), sans rien
-- retirer de `album_ratings`. Idempotent : `CREATE ... IF NOT EXISTS` et
-- `ON CONFLICT DO NOTHING` sont sûrs à rejouer (une base de bascule rejoue
-- toutes les migrations, sentinelle 99).

BEGIN;

CREATE TABLE IF NOT EXISTS notes_personnelles (
    profile_id BIGINT NOT NULL,
    item_type TEXT NOT NULL CHECK (item_type IN ('artist', 'album', 'track')),
    item_id BIGINT NOT NULL,
    texte_md TEXT NOT NULL,
    maj_le TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (profile_id, item_type, item_id)
);
CREATE INDEX IF NOT EXISTS idx_notes_personnelles_objet
    ON notes_personnelles(item_type, item_id);

INSERT INTO notes_personnelles (profile_id, item_type, item_id, texte_md, maj_le)
-- CAST : la table d'une ancienne bascule a pu naître en TEXT (voir 012).
SELECT CAST(profile_id AS BIGINT), 'album', CAST(album_id AS BIGINT), note,
       COALESCE(CAST(created_at AS TEXT),
                to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'))
FROM album_ratings
WHERE note IS NOT NULL AND TRIM(note) <> ''
ON CONFLICT (profile_id, item_type, item_id) DO NOTHING;

INSERT INTO schema_version (version, name) VALUES (103, 'notes_personnelles')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
