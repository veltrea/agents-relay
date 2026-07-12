# agents-relay 設計仕様

**他の言語で読む:** [English](SPEC.md)

対象バージョン: 0.2.1

## 1. 目的と設計原則

AI コーディングエージェント（Claude Code / Antigravity）のセッション履歴を横断検索可能な「記憶」として提供する。

- **無損失原則**: 原本（本文全文・生 chain-of-thought）は取り込み・更新・削除・アーカイブのどの経路でも失われない。切り詰めは表示・索引・転送のためだけに行い、必ず原本への参照を残す。
- **派生物は再生成可能**: 要約・事実・軽量コピー・FTS 索引はすべて原本から再構築できる。圧縮・要約アルゴリズムの差し替えが後から可能。
- **トークン経済はポインタで**: 検索・一覧のレスポンスには重いデータを含めず、`has_reasoning` / トークン数などのポインタだけを返す。取得は専用ツールで明示的に行う。

## 2. プロセス構成

```
serve (MCP stdio, ワークスペースごと) ──┐
serve (MCP stdio)                       ├─→ HTTP 127.0.0.1:54322 → server (単一プロセス) → SQLite
serve (MCP stdio)                       ──┘
```

- `agents-relay serve` — MCP stdio サーバー。thin proxy。スコープ解決（workspace / source）だけを行い、ツール呼び出しを HTTP で `server` に転送する。起動時に `ensure_server_running()` で `server` を自動起動。
- `agents-relay server` — Axum HTTP サーバー。全 DB アクセスを直列化する唯一のプロセス。`/health` `/register` `/deregister` `/tool`。ポートは `~/.agents-relay/server.port`（既定 54322）。バイナリ更新を mtime で検知して自動再起動。クライアント 0 が続くと自動終了。

## 3. ストレージ層

すべて `~/.agents-relay/` 配下。`db::open()` が 3 つの DB を ATTACH して単一コネクションで扱う。

### 3.1 memory.db（本体）

- `raw_entries` — エントリ本体。`content` は **inline 表現**（後述）。`reasoning_tokens` は CoT ポインタ。
- `raw_entries_fts` — FTS5 索引（`content`, `tool_name`, `session_id`, `context`）。`context` は「workspace 名（cwd の basename）+ git branch + source」で、プロジェクト名検索を可能にする。索引対象は inline のみ。
- `summaries` — セッション要約（consolidate が生成）。`summary_md` 冒頭に `<!-- summarizer: llm|heuristic -->` マーカー。
- `facts` — 持続的事実。`category`（DECISION / CONSTRAINT / PREFERENCE / ENVIRONMENT）、`superseded_by`（削除せず失効）。`facts_fts` で検索。
- `sync_state` — 取り込み済みバイトオフセット / 変更検知スタンプ（`<path>` と `<path>#mtime` の 2 キー）。

### 3.2 fulltext.db（全文原本・ATTACH 名 `fdb`）

- `fulltext(entry_id, content)` — `CONTENT_INLINE_MAX`（50,000 bytes）超の本文の全文。
- inline 表現 = 先頭 1,000 bytes + `... [truncated, N bytes]` マーカー。切り詰めポリシーは `db::insert_entry_full` に集約され、呼び出し側は常に全文を渡す。
- `memory_get_entry` は `COALESCE(fdb.content, inline)` で全文を返す。

### 3.3 reasoning.db（生 CoT・ATTACH 名 `rdb`）

- `cot(entry_id, reasoning, reasoning_summary)` — thinking の原本（無切り詰め）と要約キャッシュ。
- FTS には載せない。検索・セッション結果は `has_reasoning=true, reasoning_tokens≈N` のポインタのみ。
- 削除しても検索・セッション取得は壊れない（`memory_get_reasoning` が「なし」を返すだけ）。

### 3.4 アーカイブ（無損失・追記専用）

`archive` コマンドは `retention_days` 超の日付ごとに:

1. `archive_dir/YYYY/MM/DD.jsonl` に全カラム + 生 CoT + 全文原本を **追記**し fsync
2. 人間用 Markdown ダイジェスト `DD.md` を書き出し（既存なら再利用）
3. その後に初めて raw_entries / cot / fulltext から削除

追記専用のため、再実行・クラッシュリトライで重複行は生じ得るが損失は構造的に生じない。`db restore-archive <file.jsonl>` で冪等復元（既存エントリはスキップ、content は inline/fulltext に再分配）。

### 3.5 vectors.db（埋め込み・ATTACH 名 `vdb`）

- `vec_summaries` / `vec_facts` — sqlite-vec (vec0) による要約・事実の埋め込み。`vec_meta` にモデル名と次元を記録し、モデル変更時は索引を破棄して再生成する。
- 埋め込みは OpenAI 互換 `/embeddings`（`embedding_model` 設定時のみ）。`consolidate` が新規分を自動埋め込み、`embed` コマンドで一括バックフィル。
- `memory_search` は FTS 結果に加えて `match: "semantic"` の近傍（要約 3 件 + 事実 3 件）を付加する。埋め込みサービス不達時は静かに FTS のみになる。
- 完全な派生物。ファイルごと削除しても `embed` で再生成できる。

### 3.6 転送キャップ

MCP レスポンスの `content` / `reasoning` は 200,000 bytes で切る（`[transport limit: full N bytes stored losslessly in DB]` を付記）。DB 内は無損失のまま。

## 4. 取り込み

### 4.1 Claude Code

- `~/.claude/projects/**/*.jsonl` をバイトオフセットで増分取り込み（100 件ごとにトランザクションコミット、オフセットも同一 tx）。
- assistant の `thinking` ブロックは content から分離して reasoning へ。`data:image/` のみの小エントリはスキップ。
- `db backfill-fulltext` — 旧版が切り詰めた本文と捨てた thinking を、元 JSONL と `.jsonl.bak` から回収する。照合キー:
  - 本文: session + type + 先頭 1,000 bytes + 正確なバイト数（Claude Code のファイル書き直しに耐えるため timestamp は使わない）
  - CoT: session + timestamp + type + content 先頭比較（誤付与の害が大きいため厳密側）

### 4.2 Antigravity

- `agyhub_summaries_proto.pb` — 平文 protobuf（手書き wire format リーダー）。全会話の UUID・タイトル・workspace（file:// URI をパーセントデコード）・時刻。マーカー `ag:conv-summary` で upsert。
- `brain/<uuid>/*.metadata.json` + 対応 `.md` + 最新 `.resolved.N` — アーティファクト。サイズ + mtime で変更検知、マーカー付き delete-insert で更新。
- `brain/<uuid>/.system_generated/logs/transcript.jsonl` — 実トランスクリプト。offset 増分。USER_INPUT→user / PLANNER_RESPONSE→assistant / その他→system。ファイル縮小（書き直し）検知で `ag:step:*` を削除して再取り込み。
- `knowledge/<topic>/` — metadata.json + `artifacts/**/*.md`。参照 conversation_id から workspace を逆引き。
- `conversations/*.pb` と `implicit/*.pb` は暗号化されており取り込み不可（確認済み）。

## 5. 睡眠時整理（consolidate）

`agents-relay consolidate [--limit N] [--min-entries N] [--idle-mins N] [--no-facts] [--upgrade]`

1. 対象選定: user/assistant が `min_entries` 件以上、最終エントリが `idle_mins` 分より古く、要約が無いか古いセッション。`--upgrade` はヒューリスティック要約も対象に含める。
2. ダイジェスト生成: 各メッセージ 400 字、全体 14,000 字。超過時は先頭 60% + 末尾 40%（中間は省略マーカー）。
3. 要約: LLM（OpenAI 互換 `/chat/completions`）優先、不達・暴走時は抽出圧縮にフォールバック。経路をマーカーで記録。
4. 事実抽出（LLM 必須）: `CATEGORY: fact` 形式で抽出し、**新規性ゲート**（既存有効事実との文字バイグラム Jaccard >= 0.6 で重複と判定）を通過したものだけ `facts` へ。

原本は一切変更しない。要約・事実は再生成可能な派生物。

## 6. MCP ツール

| ツール | 概要 |
|---|---|
| `memory_search` | FTS 検索。scope=self/cross/all（cross/all はユーザー承認ゲート付き） |
| `memory_list_sessions` | セッション一覧 + 要約スニペット |
| `memory_get_session` | セッション履歴（先頭に要約）。type フィルタ既定 user,assistant |
| `memory_get_entry` | 1 エントリ全文（fulltext 原本、転送キャップあり） |
| `memory_get_reasoning` | CoT。mode=summary（遅延生成・キャッシュ）/ full |
| `memory_facts` | 蒸留済み事実の検索（workspace フィルタ対応） |
| `memory_set_enabled` | ワークスペース / グローバルの有効・無効 |
| `memory_find_workspace` ほか | Antigravity 専用（workspace 登録・cross-scope 解錠） |

## 7. CLI

`serve` / `server` / `ingest` / `sync` / `list` / `export` / `write` / `query` / `summarize` / `consolidate` / `embed` / `archive` / `workspace` / `config` / `db {stats, vacuum, reset, rebuild-fts, export-light, backfill-fulltext, restore-archive, backup}` / `tool`（デバッグ用直接呼び出し）

サーバーポートは `AGENTS_RELAY_PORT` 環境変数で上書き可能（テスト・並行環境用。既定 54322）。

## 8. 設定（~/.agents-relay/config.json）

| キー | 既定 | 意味 |
|---|---|---|
| `retention_days` | 30 | archive の対象日数 |
| `archive_dir` | `~/.agents-relay/archive` | アーカイブ出力先 |
| `enabled` | true | グローバル有効フラグ |
| `reasoning_llm_enabled` | false | consolidate / CoT 要約に LLM を使う |
| `reasoning_llm_base_url` | `http://localhost:1234/v1` | OpenAI 互換 API |
| `reasoning_llm_model` | （空） | 空なら `/v1/models` から自動検出 |
| `reasoning_llm_timeout_secs` | 120 | LLM 呼び出しタイムアウト |
| `backup_dir` | `~/.agents-relay/backups` | `db backup` の出力先（別物理ディスク推奨） |
| `embedding_model` | （空） | 意味検索用の埋め込みモデル。空なら無効 |

## 9. バックアップと復元

- **フルバックアップ** = `memory.db` + `fulltext.db` + `reasoning.db` の 3 点（+ 必要なら `archive/`）。`db backup` が `VACUUM INTO` で整合スナップショットを `backup_dir` に世代管理で作成する（`--keep N`、既定 3 世代）。
- **軽量コピー** = `db export-light`（CoT・FTS 実体なし、sync_state 込み）。復元先で `db rebuild-fts` すれば検索まで完全復元。
- FTS が壊れた場合は `db rebuild-fts` で常に再構築可能（索引は派生物）。
