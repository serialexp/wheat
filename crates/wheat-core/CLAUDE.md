# Wheat storage format rule

Durable system state is ONLY binary format generated from binschema, or a SQLite
database. If you happen to use *anything* else for it, it'll get thrown away and
rewritten, so DON'T FUCKING DO IT!

The only JSON allowed: files people edit (`config.json`), and temporary files that
never outlive the operation that wrote them.
