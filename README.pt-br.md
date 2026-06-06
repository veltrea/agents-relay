# claude-relay: salvando a memoria de sessao do Claude Code de um jeito tosco

## Como surgiu

Eu forkei o [claude-mem](https://github.com/anthropics/claude-mem) (plugin de memoria de sessao do Claude Code) pra tentar usar com LLMs locais, li o codigo-fonte e, sinceramente, nao dava pra usar.

Uma arquitetura que dispara uma requisicao de compressao por IA a cada uso de ferramenta, fetch sem timeout, ausencia de estrategia de retry, confusao entre liveness e readiness, processamento irreversivel que descarta os dados brutos apos compressao -- uma implementacao que nao cobre o basico de ciencia da computacao. Escrevi sobre isso em detalhes [neste artigo](https://note.com/veltrea/n/n791d1defada0).

Se voce usa a API do Claude, os problemas nao aparecem na superficie. Mas no momento que troca pra uma LLM local, tudo vira critico. Tentei corrigir o fork, mas o problema e de concepcao -- patches pontuais nao resolvem.

Ai parei pra pensar e percebi: nao tem necessidade nenhuma de comprimir com IA. O Claude Code ja grava todos os dados de sessao em JSONL em `~/.claude/projects/`. Basta jogar isso num SQLite e, na hora da consulta, deixar o proprio Claude entender os dados brutos com o contexto de 1M tokens. Sem compressao por IA, sem daemon.

Entao criei o **claude-relay** do zero.

## O que e isso

- Binario unico em Rust (cerca de 1.600 linhas)
- Conecta ao Claude Code como servidor MCP e fornece ferramentas para buscar sessoes anteriores
- Sem daemon. Importa o JSONL incrementalmente no inicio da sessao e nas chamadas de ferramentas
- Da pra arquivar dados antigos em Markdown e limpar do SQLite, se quiser

## Instalacao

Voce precisa de um ambiente de build Rust.

```bash
git clone https://github.com/veltrea/claude-relay.git
cd claude-relay
cargo build --release

# Register as MCP in Claude Code
claude mcp add --transport stdio --scope user claude-relay -- $(pwd)/target/release/claude-relay serve
```

Se quiser colocar no PATH, copie `target/release/claude-relay` pra onde preferir.

## Como usar

### Primeiro, importe os JSONL

```bash
claude-relay ingest ~/.claude/projects/
claude-relay ingest path/to/session.jsonl
claude-relay db stats
```

No meu ambiente deu cerca de 48 sessoes e 75.000 entradas.

### Usando pelo Claude Code

Como ja esta registrado como ferramenta MCP, basta perguntar normalmente dentro de uma sessao do Claude Code.

- "What did I work on yesterday?"
- "Find that OAuth fix"
- "What happened between March 20-23?"
- "Show me recent sessions"

Por baixo dos panos, ferramentas MCP como `memory_search`, `memory_list_sessions`, `memory_get_session` etc. sao chamadas.

### Tambem funciona pela CLI

Tem comandos de gerenciamento pra usar direto no terminal. Como usar via ferramenta MCP consome tokens, a ideia e que tarefas administrativas sejam feitas pela CLI.

```bash
claude-relay list
claude-relay list --date 2026-03-23
claude-relay export <session_id>
claude-relay export --date 2026-03-23
claude-relay db reset
claude-relay query "SELECT type, COUNT(*) FROM raw_entries GROUP BY type"
claude-relay write "test message" --type user
```

## Sobre o design

### Salva tudo, filtra na leitura

No comeco eu ia salvar so `user` e `assistant`, mas pensei melhor: "por que nao enfiar tudo e filtrar com WHERE na hora de ler?" Entao `system`, `progress`, `queue-operation` -- tudo entra. Se depois voce quiser ver algum dado que antes parecia inutil, ele ta la.

### Sem daemon

Cogitei rodar um daemon de monitoramento de arquivos (tipo chokidar), mas desisti. A importacao incremental acontece no hook de SessionStart e nas chamadas de ferramentas MCP. O byte offset de "ate onde ja li" de cada JSONL fica registrado, e so as linhas novas sao processadas.

### Arquivamento

No arquivo de configuracao (`~/.claude-relay/config.json`), voce pode definir `retention_days` pra exportar dados vencidos em Markdown e apagar do banco. O padrao e 30 dias.

```jsonc
{
  "retention_days": 30,
  "archive_dir": "~/.claude-relay/archive"
}
```

## Observacoes

Fiz em uns 30 minutos. Quase nao testei. Funciona no meu ambiente (macOS), mas nao tentei em outros.

Se encontrar um bug ou nao conseguir rodar, me avise por [Issue](https://github.com/veltrea/claude-relay/issues).

Nao aceito PRs. Sou do tipo que reescreve o codigo inteiro quando tenho uma ideia, entao se receber um PR, provavelmente o codigo original ja nem vai existir mais. Se te interessou, faz um fork e vai fundo. Com vibe coding qualquer um consegue fazer.

## Licenca

MIT License
