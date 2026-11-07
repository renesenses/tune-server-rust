-- #8077 — les apports d'un service de streaming (Qobuz) à un album LOCAL.
--
-- Rien n'est écrit dans les fichiers audio ni dans les colonnes de l'album :
-- un apport vit ICI, avec sa source, et se retire en effaçant ses lignes
-- (`DELETE /library/albums/{id}/apports-qobuz`). Les crédits de piste repris
-- vont dans `track_credits` avec `source = 'qobuz'` (#7765).
--
-- `apports_service_albums` : le rapprochement d'un album local avec un album
-- du service, une ligne par (album, source). `statut` : `applique`,
-- `a_confirmer` (rapprochement douteux, rien n'est appliqué), `aucun` (rien
-- de retrouvé) ou `retire` (apports retirés par l'utilisateur : la passe de
-- fond ne les remet pas). `methode` : `upc`, `mbid`, `isrc`, `titre` ou
-- `confirme`.
--
-- `apports_service` : un champ repris, pour l'album (`track_id = 0`) ou pour
-- une piste. `valeur` est du JSON.
--
-- Même texte pour SQLite (142) et PostgreSQL (108).
CREATE TABLE IF NOT EXISTS apports_service_albums (
 album_id BIGINT NOT NULL,
 source TEXT NOT NULL,
 service_album_id TEXT,
 methode TEXT,
 statut TEXT NOT NULL,
 motif TEXT,
 pistes_locales BIGINT NOT NULL DEFAULT 0,
 pistes_rapprochees BIGINT NOT NULL DEFAULT 0,
 verifie_le TEXT NOT NULL,
 PRIMARY KEY (album_id, source)
);
CREATE TABLE IF NOT EXISTS apports_service (
 album_id BIGINT NOT NULL,
 track_id BIGINT NOT NULL DEFAULT 0,
 champ TEXT NOT NULL,
 source TEXT NOT NULL,
 valeur TEXT NOT NULL,
 cree_le TEXT NOT NULL,
 PRIMARY KEY (album_id, track_id, champ, source)
);
CREATE INDEX IF NOT EXISTS idx_apports_service_piste ON apports_service(track_id)
