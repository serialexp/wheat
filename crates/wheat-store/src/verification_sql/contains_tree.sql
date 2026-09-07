SELECT EXISTS(
    SELECT 1 FROM verified_trees
    WHERE tree_id = ?1 AND aggregate_id = ?2 AND visible_id = ?3
)
