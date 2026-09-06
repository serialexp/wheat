SELECT oid, change_id, revision_id, view_id, application_id
FROM work
WHERE status = 1 AND (?1 IS NULL OR oid > ?1)
ORDER BY oid
LIMIT ?2
