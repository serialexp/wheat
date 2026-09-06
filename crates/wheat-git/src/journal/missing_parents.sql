SELECT DISTINCT p.parent_oid
FROM work_parent p
LEFT JOIN work parent ON parent.oid = p.parent_oid
WHERE parent.oid IS NULL
ORDER BY p.parent_oid
LIMIT ?1
