INSERT INTO tree_content_counts (content_id, count)
SELECT content_id, prior_count
FROM tree_content_undo
WHERE mark_id = ?1 AND prior_count > 0
ON CONFLICT(content_id) DO UPDATE SET count = excluded.count
