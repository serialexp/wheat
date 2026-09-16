SELECT EXISTS(
    SELECT 1 FROM verified_map_pairs
    WHERE application_node = ?1 AND revision_node = ?2 AND prefix = ?3
      AND application_skip = ?4 AND revision_skip = ?5
      AND retained_root = ?6 AND conflict_root = ?7
);
