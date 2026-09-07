INSERT OR IGNORE INTO verified_map_pairs
    (application_node, revision_node, retained_root, conflict_root)
VALUES (?1, ?2, ?3, ?4);
