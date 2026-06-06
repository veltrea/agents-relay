# claude-relay: le truc qui sauvegarde la memoire de session de Claude Code a l'arrache

## Contexte

J'ai fork [claude-mem](https://github.com/anthropics/claude-mem) (le plugin de memoire de session de Claude Code) pour essayer de le faire tourner avec des LLMs locaux, et apres avoir lu le code source... franchement, c'etait inutilisable.

Un design qui envoie une requete de compression IA a chaque appel d'outil, des fetch sans timeout, aucune strategie de retry, confusion entre liveness et readiness, un traitement irreversible qui jette les donnees brutes apres compression -- une implementation qui ne maitrisait pas les bases de l'informatique. J'en parle en detail dans [un autre article](https://note.com/veltrea/n/n791d1defada0).

Tant qu'on utilise l'API Claude, les problemes ne se manifestent pas, mais des qu'on passe a un LLM local, tout devient critique. J'ai essaye de corriger le fork, mais c'etait un probleme de conception fondamental -- des patchs partiels ne suffisaient pas.

Et en y reflechissant, la compression par IA n'etait meme pas necessaire. Claude Code ecrit deja toutes les donnees de session en JSONL dans `~/.claude/projects/`. Il suffit de les mettre dans SQLite et de laisser Claude lui-meme, avec son contexte de 1M, comprendre les donnees brutes a la lecture. Pas besoin de compression IA, pas besoin de daemon.

C'est comme ca que j'ai cree **claude-relay** de zero.

## C'est quoi

- Un binaire unique en Rust (environ 1 600 lignes)
- Se connecte a Claude Code en tant que serveur MCP et fournit des outils pour rechercher les sessions passees
- Pas de daemon. L'import des JSONL se fait de maniere incrementale au demarrage de session ou lors des appels d'outils
- On peut aussi archiver les anciennes donnees en Markdown et les supprimer de SQLite

## Installation

Il faut un environnement de build Rust.

```bash
git clone https://github.com/veltrea/claude-relay.git
cd claude-relay
cargo build --release

# Register as MCP in Claude Code
claude mcp add --transport stdio --scope user claude-relay -- $(pwd)/target/release/claude-relay serve
```

Si vous voulez l'avoir dans le PATH, copiez `target/release/claude-relay` ou vous voulez.

## Utilisation

### D'abord, importer les JSONL

```bash
# Ingest all sessions under ~/.claude/projects/
claude-relay ingest ~/.claude/projects/

# Or a specific file
claude-relay ingest path/to/session.jsonl

# Check how much was ingested
claude-relay db stats
```

Sur mon poste, ca a importe environ 48 sessions et 75 000 entrees.

### Utiliser depuis Claude Code

Comme c'est enregistre en tant qu'outil MCP, on peut poser des questions directement dans une session Claude Code.

- "What did I work on yesterday?"
- "Find that OAuth fix"
- "What happened between March 20-23?"
- "Show me recent sessions"

En coulisses, ce sont les outils MCP `memory_search`, `memory_list_sessions`, `memory_get_session`, etc. qui sont appeles.

### Ca marche aussi en CLI

Il y a des commandes d'administration pour une utilisation directe. Comme les outils MCP consomment des tokens, la gestion se fait en CLI.

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

## Conception

### Tout sauvegarder, filtrer a la lecture

Au depart, je comptais ne sauvegarder que `user` et `assistant`, mais je me suis dit "autant tout mettre et filtrer avec WHERE a la lecture". Du coup, `system`, `progress`, `queue-operation` et le reste y sont aussi. Si plus tard on a besoin de voir des donnees qu'on n'attendait pas, elles sont la.

### Pas de daemon

J'ai envisage un daemon de surveillance de fichiers (genre chokidar), mais j'ai laisse tomber. A la place, l'import incremental se fait au hook SessionStart et lors des appels d'outils MCP. On enregistre l'offset en octets du "jusqu'ou on a lu" dans chaque JSONL et on ne traite que les nouvelles lignes.

### Archivage

Dans le fichier de configuration (`~/.claude-relay/config.json`), on peut definir `retention_days` pour exporter les donnees expirees en Markdown et les supprimer de la BD. Par defaut, c'est 30 jours.

```jsonc
{
  "retention_days": 30,
  "archive_dir": "~/.claude-relay/archive"
}
```

## Avertissements

J'ai fait ca en 30 minutes environ. Il n'y a quasiment pas de tests. Ca tourne sur mon poste (macOS), mais je n'ai pas teste ailleurs.

Si vous trouvez un bug ou que ca ne marche pas, faites-le moi savoir via les [Issues](https://github.com/veltrea/claude-relay/issues).

Je n'accepte pas les PRs. Je suis du genre a reecrire tout le code d'un coup quand j'ai une idee, donc il y a de fortes chances que le code original n'existe plus au moment ou je recevrais un PR. Si ca vous interesse, forkez et faites comme vous voulez. Avec du vibe coding, n'importe qui peut le faire.

## Licence

MIT License
