SELECT EXISTS(
    SELECT 1 FROM verified_visible_sets
    WHERE node_id = ?1 AND aggregate_id = ?2
);
