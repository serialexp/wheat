SELECT EXISTS(
    SELECT 1 FROM verified_map_pairs
    WHERE application_node = ?1 AND revision_node = ?2
      AND retained_root = ?3 AND conflict_root = ?4
);
