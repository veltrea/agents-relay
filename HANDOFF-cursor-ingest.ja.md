# 引き継ぎ: Cursor ingest を Claude 木へ載せ替え

**日付:** 2026-11-22  
**理由:** このセッションのコンテキストが逼迫。実装の続きは次セッションで。

---

## いま何が分かっているか（最重要）

Cursor で触っていた木は **古い**。Claude Code で育てた木の方が先に進んでいる。

| パス | remote | 状態 | 版感 |
| --- | --- | --- | --- |
| `/Volumes/8TB_HDD/dev/cursor/agent-relay-private/agents-relay` | `veltrea/agents-relay`（公開） | tip `a959dd8` Publish v0.2.1 ＋ **未コミットの Cursor ingest** | **古いベース** |
| `/Volumes/8TB_HDD/dev/agents-relay` | 同上（公開） | tip `e7a3656`、origin より **ahead 12 / behind 3**、未コミットあり。`Cargo.toml` **0.2.4** | **正とする木** |
| GitHub `veltrea/agents-relay-private` | private | 最終 push 2026-06、claude-relay 表記のまま | **使わない（古い）** |

フォルダ名 `agent-relay-private` は紛らわしい。中身の `origin` は公開 `agents-relay`。

### Claude 木にあって古い木に無いもの（実用上の差）

- `src/compact.rs` / `compact.db`（min/mid 投影）
- conversation **surface** だけの FTS（tool 出力ノイズを索引から除外）
- `consolidate --watch`、`memory_search` の `deep` など 0.2.2–0.2.4
- `scripts/pull-remote-sessions.sh`、CHANGELOG 上の `extra_project_dirs` など

Claude Code 側の仕様変化に合わせた改修が入っているので、**載せ替え先は必ず `/Volumes/8TB_HDD/dev/agents-relay`**。

---

## このセッションで古い木に入れた Cursor ingest（資産）

設計書: `cursor-ingest-design.ja.md`（同ディレクトリ）

### 実装済み（古い 0.2.1 ベース上）

- `src/ingest_cursor.rs` — `~/.cursor/projects/*/agent-transcripts/*/*.jsonl`
- `fdb.tool_trace` + `raw_entries.tools_tokens`
- text → 提示用 `content`、`tool_use` → 分離保存
- `memory_get_tools`（summary / full）— MCP / server / CLI
- `sync_all` から Cursor を呼ぶ
- `client_to_source("cursor")`
- テスト: `tests/fixtures/cursor/sample.jsonl`, `tests/cursor_ingest_test.sh`
- ユニットテスト（ingest_cursor 内）通過済み（古い木で）

### 設計からの既知差分

1. ポインタ用に `raw_entries.tools_tokens` も追加（`reasoning_tokens` と同じ型）
2. cwd は project-id から復元を試すが、実在パスのときだけ
3. timestamp 無し行は mtime + 行番号の代理時刻
4. Composer `state.vscdb`・consolidate 連携・archive 往復は未着手

### 変更ファイル一覧（古い木・未コミット）

```
M  src/db.rs
M  src/ingest.rs
M  src/main.rs
M  src/mcp.rs
M  src/server.rs
?? src/ingest_cursor.rs
?? tests/cursor_ingest_test.sh
?? tests/fixtures/cursor/sample.jsonl
?? cursor-ingest-design.ja.md
?? cursor-env-setup-guide-ja.md
?? HANDOFF-cursor-ingest.ja.md  (本ファイル)
```

**まだ commit していない。** 古い木への commit は必須ではない（載せ替えが本命）。

---

## 次セッションでやること（推奨手順）

### ゴール

`/Volumes/8TB_HDD/dev/agents-relay` を正として、Cursor ingest を **その API（compact / surface FTS）に合わせて** 移植する。

### 手順案

1. **ワークスペースを Claude 木に切り替え**  
   `/Volumes/8TB_HDD/dev/agents-relay`
2. **現状を保全**  
   - `git status`（未コミットの CHANGELOG / config / ingest 等あり）  
   - 必要なら stash か、先に Claude 木の未コミットを整理・commit
3. **古い木から移植する核**  
   - `ingest_cursor.rs` をほぼそのままコピー  
   - `db.rs`: `tool_trace` / `tools_tokens` / `set_tool_trace` / `get_tool_trace` / `delete_cursor_session`  
     ※ Claude 木は insert 時に `compact::surface` で FTS を張る → **tool 原本を content に載せない方針はそのまま相性が良い**。`set_tool_trace` 後も surface 再計算が要るか確認  
   - `ingest.rs` の `sync_all` に Cursor を追加  
   - `mcp.rs` / `server.rs` / `main.rs` に `memory_get_tools` と `has_tools` ポインタ  
4. **食い違いチェック重点**  
   - `RawEntry` に既に min/mid / content_tokens 等がある → フィールド追加箇所を全更新  
   - `reset` / `stats` / backup 対象に `tool_trace`（と必要なら compact）  
   - search の `deep` と tool_trace の役割分担（二重にならないこと）  
5. **検証**  
   - `cargo test`  
   - `tests/cursor_ingest_test.sh` を Claude 木向けにコピーして実行  
   - 可能なら実在の `~/.cursor/projects/.../agent-transcripts` で `memory_search` + `memory_get_tools`
6. **ドキュメント**  
   - `cursor-ingest-design.ja.md` を Claude 木へコピーし、surface/compact との関係を1節追記  
   - 必要なら CHANGELOG 0.2.5 草案

### やらない方がよいこと

- 古い 0.2.1 木の上で Cursor ingest を「完成」させて公開する  
- `agents-relay-private`（GitHub）を最新だと思い込む  
- Composer vscdb まで一気にやる（設計どおり後回し）

---

## 次セッションへの一言プロンプト（コピペ用）

```text
agents-relay の作業を引き継いで。

正本は /Volumes/8TB_HDD/dev/agents-relay（Claude 木、0.2.4 相当、compact/surface あり）。
古い Cursor ワークスペース
/Volumes/8TB_HDD/dev/cursor/agent-relay-private/agents-relay
には未コミットの Cursor ingest 実装がある。

やってほしいこと:
1. Claude 木をワークスペースにする
2. そこの未コミットを確認・保全
3. 古い木の Cursor ingest（ingest_cursor / tool_trace / memory_get_tools）を
   Claude 木の compact・surface FTS に合わせて移植
4. cargo test と tests/cursor_ingest_test.sh 相当で検証

設計と詳細は古い木の
cursor-ingest-design.ja.md と HANDOFF-cursor-ingest.ja.md を読んで。
```

---

## 関連ドキュメント

- `cursor-ingest-design.ja.md` — 実装設計  
- `cursor-env-setup-guide-ja.md` — Cloud Agent 環境セットアップの初心者向け記録（別件）
