UPDATE tree_content_counts
SET count = count + ?2
WHERE content_id = ?1
