DELETE FROM tree_content_counts
WHERE content_id IN (
    SELECT content_id
    FROM tree_content_undo
    WHERE mark_id = ?1 AND prior_count = 0
)
