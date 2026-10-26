SELECT EXISTS(
    SELECT 1
    FROM verified_chunk_semantics
    WHERE chunk_id = ?1 AND aggregate_id = ?2
)
