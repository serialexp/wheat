SELECT l.kind, l.offset, l.length, s.path
FROM locator AS l
JOIN physical_source AS s ON s.id = l.source_id
WHERE l.namespace=?1 AND l.digest=?2
