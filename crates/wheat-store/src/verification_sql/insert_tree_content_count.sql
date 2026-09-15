INSERT INTO tree_content_counts (content_id, count)
VALUES (?1, ?2)
ON CONFLICT(content_id) DO UPDATE SET count = count + excluded.count
