SELECT EXISTS(
    SELECT 1
    FROM verified_tree_semantics
    WHERE tree_id = ?1 AND aggregate_id = ?2
)
