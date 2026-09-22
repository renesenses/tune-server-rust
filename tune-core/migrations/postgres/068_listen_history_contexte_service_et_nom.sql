-- Rendre a l'objet demande son ESPACE DE NOMS et son NOM.
--
-- Jumelle de la migration SQLite 105. Les deux listes sont SEPAREES —
-- `run_migrations` ne prend qu'un `SqliteDb` — donc une colonne posee d'un
-- seul cote ne repare que la moitie du parc (#1612, #2111).
--
-- `context_source` : l'espace de noms de `context_id` (`local`, `qobuz`,
-- `tidal`…). La colonne `source`, elle, est celle de la PISTE et ne repond
-- pas a « chez qui ouvrir cet objet ». Mesure du 20/09/2026 sur le .18 : la
-- playlist Qobuz `66898771` porte 18 lignes `source = 'qobuz'` et 3 lignes
-- `source = 'local'`, trois titres de la bibliotheque glisses dans une
-- playlist de service. Grouper sur `source` couperait cette playlist en deux
-- vignettes ; y resoudre l'identifiant irait chercher `66898771` dans la
-- table `albums`.
--
-- `context_title` / `context_cover` : le nom et la pochette d'une playlist de
-- service, qui n'existent dans AUCUNE table de cette base. Sans eux,
-- « Continuer l'ecoute » rend `title: null` et le client replie sur l'album
-- de la derniere piste jouee — 16 vignettes de playlist sur 16 portaient un
-- libelle etranger a ce qui avait ete ecoute (Alex Campbell, 20/09/2026).
--
-- Idempotent : `IF NOT EXISTS` partout, sans danger a rejouer sur une base
-- deja migree depuis SQLite. Les lignes existantes gardent NULL — l'historique
-- d'avant ne l'a jamais su, et rien ici ne pretend le reconstituer.

BEGIN;

ALTER TABLE listen_history
    ADD COLUMN IF NOT EXISTS context_source TEXT;

ALTER TABLE listen_history
    ADD COLUMN IF NOT EXISTS context_title TEXT;

ALTER TABLE listen_history
    ADD COLUMN IF NOT EXISTS context_cover TEXT;

INSERT INTO schema_version (version, name) VALUES (68, 'listen_history_contexte_service_et_nom')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
