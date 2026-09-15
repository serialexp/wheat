INSERT OR IGNORE INTO tree_content_undo (mark_id, content_id, prior_count)
SELECT ?1, content_id, count
FROM tree_content_counts
