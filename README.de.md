# claude-relay: das Ding, das Claude Codes Session-Speicher quick and dirty sichert

## Hintergrund

Ich habe [claude-mem](https://github.com/anthropics/claude-mem) (das Session-Memory-Plugin fuer Claude Code) geforkt, um es mit lokalen LLMs zum Laufen zu bringen, und als ich den Quellcode gelesen habe... ehrlich gesagt, war es unbrauchbar.

Ein Design, das bei jedem Tool-Aufruf eine KI-Komprimierungsanfrage abfeuert, Fetch ohne Timeout, keine Retry-Strategie, Verwechslung von Liveness und Readiness, irreversible Verarbeitung, die Rohdaten nach der Komprimierung wegwirft -- eine Implementierung, der die Grundlagen der Informatik fehlten. Details dazu habe ich in [einem separaten Artikel](https://note.com/veltrea/n/n791d1defada0) beschrieben.

Solange man die Claude-API nutzt, treten die Probleme nicht zutage, aber sobald man auf ein lokales LLM wechselt, wird alles kritisch. Ich habe versucht, den Fork zu patchen, aber es war ein grundsaetzliches Designproblem -- mit partiellen Patches nicht zu loesen.

Und wenn man mal darueber nachdenkt: KI-Komprimierung war von vornherein unnoetig. Claude Code schreibt alle Session-Daten bereits als JSONL nach `~/.claude/projects/`. Man muss sie nur in SQLite werfen und Claude selbst mit seinem 1M-Kontext die Rohdaten beim Lesen verstehen lassen. Keine KI-Komprimierung, kein Daemon noetig.

Also habe ich **claude-relay** von Grund auf neu geschrieben.

## Was ist das

- Ein einzelnes Binary in Rust (ca. 1.600 Zeilen)
- Verbindet sich als MCP-Server mit Claude Code und stellt Tools zur Suche vergangener Sessions bereit
- Kein Daemon. JSONL-Import erfolgt inkrementell beim Session-Start oder bei Tool-Aufrufen
- Alte Daten koennen nach Markdown archiviert und aus SQLite geloescht werden

## Installation

Eine Rust-Build-Umgebung wird benoetigt.

```bash
git clone https://github.com/veltrea/claude-relay.git
cd claude-relay
cargo build --release

# Register as MCP in Claude Code
claude mcp add --transport stdio --scope user claude-relay -- $(pwd)/target/release/claude-relay serve
```

Wer es im PATH haben will, kopiert `target/release/claude-relay` an den gewuenschten Ort.

## Benutzung

### Zuerst die JSONL importieren

```bash
# Ingest all sessions under ~/.claude/projects/
claude-relay ingest ~/.claude/projects/

# Or a specific file
claude-relay ingest path/to/session.jsonl

# Check how much was ingested
claude-relay db stats
```

Bei mir wurden etwa 48 Sessions mit rund 75.000 Eintraegen importiert.

### Aus Claude Code heraus nutzen

Da es als MCP-Tool registriert ist, kann man innerhalb einer Claude-Code-Session einfach fragen.

- "What did I work on yesterday?"
- "Find that OAuth fix"
- "What happened between March 20-23?"
- "Show me recent sessions"

Im Hintergrund werden MCP-Tools wie `memory_search`, `memory_list_sessions`, `memory_get_session` usw. aufgerufen.

### Geht auch ueber CLI

Es gibt auch Verwaltungskommandos fuer die direkte Nutzung. Da MCP-Tools Tokens verbrauchen, ist die Verwaltung fuer die CLI ausgelegt.

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

## Design

### Alles speichern, beim Lesen filtern

Anfangs wollte ich nur `user` und `assistant` speichern, aber dann dachte ich "warum nicht alles reinwerfen und beim Lesen mit WHERE filtern?". Also sind auch `system`, `progress`, `queue-operation` und der Rest drin. Wenn man spaeter doch mal Daten braucht, die man nicht erwartet hatte, sind sie da.

### Kein Daemon

Ich hatte ueberlegt, einen Dateiueberwachungs-Daemon (a la chokidar) laufen zu lassen, aber das habe ich verworfen. Stattdessen erfolgt der inkrementelle Import beim SessionStart-Hook und bei MCP-Tool-Aufrufen. Der Byte-Offset ("bis wohin wurde gelesen") wird pro JSONL gespeichert, und nur neue Zeilen werden verarbeitet.

### Archivierung

In der Konfigurationsdatei (`~/.claude-relay/config.json`) kann man `retention_days` setzen, um abgelaufene Daten als Markdown zu exportieren und aus der DB zu loeschen. Standard sind 30 Tage.

```jsonc
{
  "retention_days": 30,
  "archive_dir": "~/.claude-relay/archive"
}
```

## Hinweise

Ich habe das in etwa 30 Minuten gebaut. Es gibt so gut wie keine Tests. Auf meinem Rechner (macOS) laeuft es, aber andere Umgebungen habe ich nicht getestet.

Wer einen Bug findet oder bei dem es nicht laeuft, kann sich gerne per [Issue](https://github.com/veltrea/claude-relay/issues) melden.

PRs nehme ich nicht an. Ich bin der Typ, der den gesamten Code auf einmal umschreibt, wenn mir was einfaellt, daher ist es sehr wahrscheinlich, dass der urspruengliche Code nicht mehr existiert, wenn ein PR reinkommt. Wer Interesse hat, forkt einfach und macht sein eigenes Ding. Mit Vibe Coding kann das jeder bauen.

## Lizenz

MIT License
