SELECT application.application, application.change, touch.position, touch.application
FROM touch JOIN application ON application.id = touch.application
WHERE touch.path = ?1 AND (touch.position, touch.application) < (?2, ?3)
ORDER BY touch.position DESC, touch.application DESC
LIMIT ?4
