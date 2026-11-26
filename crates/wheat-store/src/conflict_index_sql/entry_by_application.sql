SELECT id, previous, jump, jump_ordinal, jump_position, ordinal, position, conflict, application, change
FROM entry WHERE application = ?1
