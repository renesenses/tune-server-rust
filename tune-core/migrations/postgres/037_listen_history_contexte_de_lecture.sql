-- #2441 — `listen_history` ne conservait AUCUNE trace de ce que l'auditeur
-- avait demande.
--
-- Une piste jouee seule, la meme piste jouee depuis une playlist, et la meme
-- piste jouee dans un album complet produisaient trois lignes rigoureusement
-- identiques. Toute rubrique voulant « refleter la realite de ce qu'a voulu
-- faire l'auditeur » (FabienM, fil forum 1557) devait donc repartir de la
-- table `albums` : c'est ce que fait `fetch_continue_listening`, et c'est
-- pourquoi « Continuer l'ecoute » ne peut structurellement montrer qu'un
-- album.
--
-- `context_type` : la nature de l'objet sur lequel « Lire » a ete clique —
-- `track`, `album`, `playlist`, `artist`, `label`. `context_id` : son
-- identifiant. TEXT et non INTEGER pour les deux : une playlist locale a un
-- identifiant numerique, un album Qobuz une chaine, et la colonne doit
-- accueillir les deux sans convertir.
--
-- Jumelle de la migration SQLite 84. Les deux listes sont SEPAREES —
-- `run_migrations` ne prend qu'un `SqliteDb` — donc une colonne posee d'un
-- seul cote ne repare que la moitie du parc (#1612, #2111).
--
-- Idempotent : `IF NOT EXISTS` des deux cotes, sans danger a rejouer sur une
-- base deja migree depuis SQLite ou la colonne est deja la. Les lignes
-- existantes gardent NULL — l'historique d'avant n'a jamais su d'ou venaient
-- ses ecoutes, et rien ici ne pretend le reconstituer.
ALTER TABLE listen_history
    ADD COLUMN IF NOT EXISTS context_type TEXT;

ALTER TABLE listen_history
    ADD COLUMN IF NOT EXISTS context_id TEXT;
