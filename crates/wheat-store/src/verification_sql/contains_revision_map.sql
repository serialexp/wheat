SELECT EXISTS(
    SELECT 1 FROM verified_revision_maps
    WHERE node_id = ?1 AND retained_root = ?2
);
