SELECT EXISTS (
    SELECT 1 FROM work_parent p
    LEFT JOIN work parent ON parent.oid = p.parent_oid
    WHERE p.oid = ?1 AND (parent.oid IS NULL OR parent.status != 1)
)
