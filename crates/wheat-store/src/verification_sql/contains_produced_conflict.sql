SELECT EXISTS(
    SELECT 1 FROM produced_conflicts
    WHERE conflict_root = ?1 AND conflict_id = ?2
);
