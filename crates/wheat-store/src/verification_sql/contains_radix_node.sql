SELECT EXISTS(
    SELECT 1
    FROM verified_radix_nodes
    WHERE kind = ?1
      AND context_id = ?2
      AND node_id = ?3
      AND prefix = ?4
)
