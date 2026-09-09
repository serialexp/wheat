SELECT EXISTS (
    SELECT 1 FROM work_parent p
    LEFT JOIN work parent ON parent.oid = p.parent_oid
    LEFT JOIN prepared_work prepared ON prepared.oid = p.parent_oid
    WHERE p.oid = ?1
      AND (parent.oid IS NULL OR (parent.status != 1 AND prepared.oid IS NULL))
)
