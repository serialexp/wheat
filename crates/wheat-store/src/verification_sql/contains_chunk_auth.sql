SELECT key_above, entries, height
FROM verified_chunk_auth
WHERE chunk_id = ?1
