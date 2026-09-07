SELECT EXISTS(SELECT 1 FROM retained_objects WHERE family = ?1 AND object_id = ?2)
