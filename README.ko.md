# claude-relay: Claude Code 세션 기억을 대충 저장하는 도구

## 배경

[claude-mem](https://github.com/anthropics/claude-mem)(Claude Code의 세션 기억 플러그인)을 로컬 LLM에서 쓸 수 있게 하려고 fork해서 소스코드를 읽어봤는데, 솔직히 쓸 수 있는 상태가 아니었습니다.

도구 호출 한 번마다 AI 압축 요청을 보내는 설계, 타임아웃 없는 fetch, 리트라이 전략 부재, liveness와 readiness 혼동, 압축 후 원본 데이터를 버리는 비가역적 처리 — 컴퓨터 과학 기초가 안 잡혀있는 구현이었습니다. 자세한 내용은 [별도 글](https://note.com/veltrea/n/n791d1defada0)에 적어두었습니다.

Claude API 전제라면 문제가 드러나지 않을 뿐이고, 로컬 LLM으로 바꾸는 순간 전부 치명적이 됩니다. fork한 코드를 고쳐보려 했지만 설계 사상 자체의 문제라 부분적인 패치로는 해결이 안 됩니다.

그런데 생각해보면 AI로 압축할 필요가 애초에 없더라고요. Claude Code는 모든 세션 데이터를 `~/.claude/projects/`에 JSONL로 기록하고 있습니다. 이걸 SQLite에 넣어놓고, 검색할 때 Claude 자체의 1M 컨텍스트로 원본 데이터를 이해시키면 됩니다. AI 압축도 데몬도 필요 없습니다.

그래서 **claude-relay**를 처음부터 새로 만들었습니다.

## 이게 뭔가요

- Rust로 만든 싱글 바이너리 (약 1,600줄)
- MCP 서버로 Claude Code에 연결해서 과거 세션을 검색하는 도구를 제공합니다
- 데몬 불필요. 세션 시작 시와 도구 호출 시에 JSONL을 증분 수집합니다
- 오래된 데이터는 Markdown으로 아카이브하고 SQLite에서 지우는 운용도 가능합니다

## 설치

Rust 빌드 환경이 필요합니다.

```bash
git clone https://github.com/veltrea/claude-relay.git
cd claude-relay
cargo build --release

# Claude Code의 MCP에 등록
claude mcp add --transport stdio --scope user claude-relay -- $(pwd)/target/release/claude-relay serve
```

PATH에 넣고 싶으면 `target/release/claude-relay`를 원하는 위치에 복사하세요.

## 사용법

### 먼저 JSONL을 수집

```bash
# ~/.claude/projects/ 하위의 모든 세션을 수집
claude-relay ingest ~/.claude/projects/

# 특정 파일만
claude-relay ingest path/to/session.jsonl

# 얼마나 들어갔는지 확인
claude-relay db stats
```

제 환경에서는 48개 세션, 약 75,000개 엔트리가 들어갔습니다.

### Claude Code에서 사용

MCP 도구로 등록되어 있으니까 Claude Code 세션에서 그냥 물어보면 됩니다.

- "어제 작업한 내용 알려줘"
- "OAuth 수정했던 거 찾아봐"
- "3월 20일부터 23일 사이에 뭐 했어?"
- "최근 세션 목록 보여줘"

내부적으로는 `memory_search`, `memory_list_sessions`, `memory_get_session` 등의 MCP 도구가 호출됩니다.

### CLI로도 쓸 수 있음

사람이 직접 치는 관리 명령어도 있습니다. MCP 도구를 거치면 토큰을 먹으니까 관리 작업은 CLI로 하도록 설계했습니다.

```bash
# 세션 목록
claude-relay list
claude-relay list --date 2026-03-23

# 세션 내용을 Markdown으로 출력
claude-relay export <session_id>
claude-relay export --date 2026-03-23

# DB 초기화
claude-relay db reset

# 생 SQL도 실행 가능 (개발할 때 편리)
claude-relay query "SELECT type, COUNT(*) FROM raw_entries GROUP BY type"

# 테스트용으로 수동 기록
claude-relay write "테스트 메시지" --type user
```

## 설계 이야기

### 전부 저장하고, 읽을 때 골라내기

처음에는 `user`와 `assistant`만 저장하려고 했는데, "전부 넣어놓고 읽을 때 WHERE로 거르면 되지 않나?" 싶어서 다시 생각했습니다. `system`, `progress`, `queue-operation`도 전부 넣고 있습니다. 나중에 "역시 그 데이터 보고 싶다"가 되어도 대응할 수 있습니다.

### 데몬 불필요

파일 감시 데몬(chokidar 등)을 상주시키는 방법도 있었지만 그만뒀습니다. SessionStart 훅과 MCP 도구 호출 시에 증분 수집하는 방식으로 했습니다. JSONL의 "지난번에 어디까지 읽었는지"를 바이트 오프셋으로 기록해두고 새로운 줄만 처리합니다.

### 아카이브

설정 파일(`~/.claude-relay/config.json`)에서 `retention_days`를 지정하면 기한이 지난 데이터를 Markdown으로 내보내고 DB에서 삭제할 수 있습니다. 기본값은 30일입니다.

```jsonc
{
  "retention_days": 30,
  "archive_dir": "~/.claude-relay/archive"
}
```

## 주의사항

30분 정도 만에 만들었습니다. 테스트는 거의 안 했습니다. 제 환경(macOS)에서는 돌아가고 있지만 다른 환경은 시도해보지 않았습니다.

버그를 발견하셨거나 동작하지 않는 경우 [Issue](https://github.com/veltrea/claude-relay/issues)에서 알려주시면 감사하겠습니다.

PR은 받지 않습니다. 뭔가 떠오르면 코드를 통째로 갈아엎는 타입이라 PR을 받아도 원래 코드가 남아있지 않을 가능성이 높습니다. 관심 있으시면 fork해서 자유롭게 하세요. 바이브 코딩하면 누구나 만들 수 있습니다.

## 라이선스

MIT License
