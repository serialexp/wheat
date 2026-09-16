INSERT OR IGNORE INTO verified_map_pairs
    (
        application_node, revision_node, prefix, application_skip, revision_skip,
        retained_root, conflict_root
    )
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);
