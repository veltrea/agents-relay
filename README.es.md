# claude-relay: el que guarda la memoria de sesiones de Claude Code sin complicarse

## Contexto

Hice un fork de [claude-mem](https://github.com/anthropics/claude-mem) (el plugin de memoria de sesiones de Claude Code) para intentar usarlo con LLMs locales, y al leer el codigo fuente... sinceramente, no servia para nada.

Un diseno que lanza una peticion de compresion con IA por cada uso de herramienta, fetch sin timeout, sin estrategia de reintentos, confusion entre liveness y readiness, procesamiento irreversible que descarta los datos crudos despues de comprimir -- una implementacion que no cubria los fundamentos de ciencias de la computacion. Lo detallo en [otro articulo](https://note.com/veltrea/n/n791d1defada0).

Si solo usas la API de Claude, los problemas no se notan, pero en cuanto cambias a un LLM local todo se vuelve critico. Intente parchear el fork, pero era un problema de diseno de fondo, no se arregla con parches parciales.

Y pensandolo bien, la compresion con IA no era necesaria para empezar. Claude Code ya escribe todos los datos de sesion en JSONL en `~/.claude/projects/`. Solo hay que meterlos en SQLite y dejar que el propio Claude, con su contexto de 1M, entienda los datos crudos al buscar. Sin compresion con IA, sin daemons.

Asi que hice **claude-relay** desde cero.

## Que es

- Un binario unico hecho en Rust (unas 1,600 lineas)
- Se conecta a Claude Code como servidor MCP y ofrece herramientas para buscar sesiones pasadas
- Sin daemon. Importa los JSONL de forma incremental al iniciar sesion o al llamar herramientas
- Tambien puedes archivar datos antiguos en Markdown y borrarlos de SQLite

## Instalacion

Necesitas un entorno de compilacion de Rust.

```bash
git clone https://github.com/veltrea/claude-relay.git
cd claude-relay
cargo build --release

# Register as MCP in Claude Code
claude mcp add --transport stdio --scope user claude-relay -- $(pwd)/target/release/claude-relay serve
```

Si quieres tenerlo en el PATH, copia `target/release/claude-relay` donde prefieras.

## Uso

### Primero, importar los JSONL

```bash
# Ingest all sessions under ~/.claude/projects/
claude-relay ingest ~/.claude/projects/

# Or a specific file
claude-relay ingest path/to/session.jsonl

# Check how much was ingested
claude-relay db stats
```

En mi caso se importaron unas 48 sesiones con unos 75,000 registros.

### Usar desde Claude Code

Como esta registrado como herramienta MCP, puedes preguntar directamente dentro de una sesion de Claude Code.

- "What did I work on yesterday?"
- "Find that OAuth fix"
- "What happened between March 20-23?"
- "Show me recent sessions"

Por detras se llaman herramientas MCP como `memory_search`, `memory_list_sessions`, `memory_get_session`, etc.

### Tambien funciona por CLI

Hay comandos de administracion para usar directamente. Como las herramientas MCP consumen tokens, la gestion se hace por CLI.

```bash
# List sessions
claude-relay list
claude-relay list --date 2026-03-23

# Export session as Markdown
claude-relay export <session_id>
claude-relay export --date 2026-03-23

# Reset DB
claude-relay db reset

# Run raw SQL (handy for dev)
claude-relay query "SELECT type, COUNT(*) FROM raw_entries GROUP BY type"

# Manual test entry
claude-relay write "test message" --type user
```

## Diseno

### Guardar todo y filtrar al leer

Al principio iba a guardar solo `user` y `assistant`, pero pense "mejor meto todo y filtro con WHERE al leer". Asi que tambien estan `system`, `progress`, `queue-operation` y demas. Si luego necesitas ver algo que no esperabas, esta ahi.

### Sin daemon

Considere tener un daemon de vigilancia de archivos (tipo chokidar), pero lo descarte. En su lugar, la importacion incremental ocurre en el hook de SessionStart y en las llamadas a herramientas MCP. Se guarda el offset en bytes de "hasta donde lei" en cada JSONL y solo se procesan las lineas nuevas.

### Archivado

En el archivo de configuracion (`~/.claude-relay/config.json`) puedes definir `retention_days` para exportar datos vencidos a Markdown y borrarlos de la BD. Por defecto son 30 dias.

```jsonc
{
  "retention_days": 30,
  "archive_dir": "~/.claude-relay/archive"
}
```

## Advertencias

Lo hice en unos 30 minutos. Casi no tiene tests. Funciona en mi entorno (macOS), pero no lo he probado en otros.

Si encuentras un bug o no te funciona, avisame en los [Issues](https://github.com/veltrea/claude-relay/issues).

No acepto PRs. Soy del tipo que reescribe todo el codigo de golpe cuando se me ocurre algo, asi que es muy probable que el codigo original ya no exista para cuando reciba un PR. Si te interesa, haz un fork y hazlo a tu manera. Con vibe coding cualquiera puede hacerlo.

## Licencia

MIT License
