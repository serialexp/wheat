INSERT INTO stamp (id, lineage, generation) VALUES (0, ?1, ?2)
ON CONFLICT (id) DO UPDATE SET lineage = excluded.lineage, generation = excluded.generation
