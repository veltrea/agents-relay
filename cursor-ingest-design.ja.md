# Cursor セッション取り込み — 実装設計

**目的:** Claude Code / Antigravity と同様に、Cursor のローカル会話履歴を `agents-relay` に取り込み、検索・想起できるようにする。  
**原則:** 原本は捨てない。トークン節約は「保存を薄くする」のではなく「提示を薄くする」。大きい塊は別フィールドに分離し、必要時だけ読む。

---

## 1. 大枠の合意

Cursor の Agent 会話は `tool_use` だらけになりやすい。これを全部 `content` に載せると:

- FTS がノイズだらけになる
- `memory_search` / `memory_get_session` の応答がトークンを溶かす
- 「何を決めたか」が埋もれる

なので Claude 側の CoT 分離と同じ型で扱う。

| 層 | 何を入れるか | いつ読むか |
| --- | --- | --- |
| 提示面（`content` / 検索ヒット） | ユーザー文、assistant の text、tool の短い一行要約 | いつも |
| 分離原本（tool 軌道） | `tool_use` / 関連 input の JSON 原本 | 必要時だけ |
| 既存 fulltext | 巨大な text 本文 | 必要時だけ |
| 既存 reasoning | （Cursor JSONL に thinking 相当があれば）思考原本 | 必要時だけ |

「省略」＝削除ではない。**省略して分離し、オンデマンドで読む。**

---

## 2. 入力ソース（ローカル）

### 2.1 第1優先: Agent transcript JSONL

```text
~/.cursor/projects/<workspace-id>/agent-transcripts/<uuid>/<uuid>.jsonl
```

実測（このリポジトリの会話）:

- 1行1オブジェクト
- 主な形: `{"role":"user"|"assistant","message":{"content":[...parts]}}`
- assistant の part は `text` と `tool_use` が混在
- `tool_use` 例: `Shell`, `Read`, `Grep`, `Write`, `Glob`, `CallDynamicTool`, ...
- Claude JSONL と違い、`type` / `sessionId` / `timestamp` がトップレベルに無いことが多い

`session_id` はファイル名（UUID）を使う。  
`timestamp` は JSONL に無ければファイル mtime、または行オフセット由来の単調な代理値（後述の未決事項）。

### 2.2 第2優先（初期スコープ外）

`~/Library/Application Support/Cursor/User/**/state.vscdb` の Composer / チャット状態。

- 構造が不透明で変わりやすい
- Agent JSONL より取り込みコストが高い

**v1 では Agent JSONL のみ。** Composer UI 専用履歴は後続。

---

## 3. 既存アーキテクチャへの載せ方

いまの分離:

```text
memory.db   raw_entries.content     … 提示用（inline、長いものは先頭 + ポインタ）
reasoning.db rdb.cot                … 生 CoT + 要約
fulltext.db  fdb.fulltext           … content 全文（大きい本文）
```

Cursor の tool 軌道は **CoT とは別物** なので、reasoning に混ぜない。

### 推奨（v1）: 新テーブル `fdb.tool_trace`（または同趣旨の分離 DB）

```text
fdb.tool_trace (
  entry_id INTEGER PRIMARY KEY,   -- raw_entries.id
  tools_json TEXT NOT NULL,       -- tool_use 配列の原本 JSON
  tools_summary TEXT,             -- 短い一覧文（任意・再生成可）
  tools_tokens INTEGER            -- 概算サイズ（ポインタ用）
)
```

`raw_entries` 側:

| カラム | Cursor での使い方 |
| --- | --- |
| `source` | `"cursor"` |
| `type` | `user` / `assistant` / （必要なら）`system` |
| `content` | 提示用本文のみ（下の正規化ルール） |
| `tool_name` | その行の代表ツール名（先頭1つ、または `Shell+Read+…` の短縮） |
| `cwd` / `git_branch` | workspace パスから推定できれば入れる |
| `reasoning_*` | transcript に thinking 相当が無い限り空 |

`memory.db` を肥大化させず、tool 原本削除しても検索は生きる（Claude の rdb 方針と同じ）。

### 代替案（採用しない方向）

- **全部 `content` に連結:** トークンと FTS が壊れる
- **tool_use を捨てる:** ロスレス原則に反する
- **tool 1つごとに raw_entries 1行:** 行数が爆発し、セッション閲覧が読みにくい（v2 で「展開ビュー」が欲しくなったら再検討）

---

## 4. 行の正規化ルール

1 JSONL 行 → 原則 1 `raw_entries`。

### 4.1 `role == "user"`

- `content` = text part を結合
- 既存どおり巨大なら inline 頭 + `fdb.fulltext`
- Cursor 特有のラップ（`<cursor_commands>` 等）は **原本としては残す**。提示要約は後段の consolidate / 表示整形で削ってよい

### 4.2 `role == "assistant"`

parts を分割:

1. **text parts** → 提示用 `content` の本体
2. **tool_use parts** → `tools_json` に配列で保存。`content` には載せない（または下の一行要約だけ）

提示用 `content` の組み立て例:

```text
（あれば）assistant の text

[tools]
- Shell: cargo build --locked
- Read: src/ingest.rs
- Grep: tool_use
- CallDynamicTool: cursor-cloud / environment-info
```

ルール:

- 各 tool は **1行**（名前 + 短い引数要約）
- `Shell` はコマンド先頭 N 文字
- `Read` / `Write` / `Glob` はパス
- `Grep` は pattern（と任意で path）
- `CallDynamicTool` は `namespace/toolName`
- 引数の巨大 blob（ファイル全文、スクショ、長い JSON）は要約に入れない。原本は `tools_json` のみ

text が無く tool_use だけの行:

- `content` は `[tools]` 要約だけ
- 原本はすべて `tools_json`

### 4.3 検索インデックス（FTS）

索引対象は提示用 `content` + `tool_name` + session / context。  
`tools_json` は FTS に載せない（載せるとパスやコマンド断片でノイズが増える）。  
ツール経路を探したい需要が出たら、`tools_summary` だけを追加索引する（後続）。

---

## 5. 取得 API（提示を薄く保つ）

既存ツールの振る舞い:

| ツール | Cursor 取り込み後 |
| --- | --- |
| `memory_search` | 提示用 `content` のみ返す（いまどおりポインタ志向） |
| `memory_get_session` | 各エントリの提示用本文。tool がある行は `[has_tools ~Ntok]` のようなポインタ |
| `memory_get_entry` | 本文全文（fulltext 含む）。**tool 原本はまだ出さない** or 短い summary まで |
| **新規** `memory_get_tools` | `entry_id` を受け取り `tools_json` / `tools_summary` を返す |

新規ツールのモード案:

- `summary`（既定）: 一行一覧
- `full`: `tools_json` 全体

これで「必要に応じて読む」が MCP 上でも明示される。

CLI 対応:

```bash
agents-relay tool memory_get_tools --id 123
agents-relay tool memory_get_tools --id 123 --query full
```

---

## 6. コード変更の置き場

Antigravity と同じパターンでモジュールを分ける。

```text
src/ingest_cursor.rs   … 新規。探索・パース・正規化・insert
src/ingest.rs          … sync_all() から ingest_cursor::sync_all を呼ぶ
src/mcp.rs             … client_to_source("cursor") → Some("cursor")
                         tools/list に memory_get_tools
src/server.rs          … handle_tool に memory_get_tools
src/db.rs              … fdb.tool_trace の CREATE / get / set
src/main.rs            … mod ingest_cursor;
src/detect.rs          … 既存の "cursor" 認識を活かす（変更最小）
```

`sync_all` のイメージ:

```text
Claude projects JSONL
+ Antigravity
+ Cursor agent-transcripts JSONL   ← 追加
```

差分取り込みは既存の `sync_state`（パス → byte offset）を再利用。  
ファイルが短く書き換わった場合は Antigravity と同様に「当該 session の cursor 行を消して再 ingest」を検討（v1 で検知だけでも可）。

---

## 7. 実装フェーズ

### Phase 0 — 調査固定（半日以内）

- [ ] 複数プロジェクトの transcript でスキーマ差をサンプル
- [ ] `timestamp` / `session_id` / tool_result 相当の有無を確認
- [ ] workspace-id ディレクトリ名 → 実パス復元ルールを決める

### Phase 1 — 取り込み MVP

- [ ] `ingest_cursor.rs` + `source="cursor"`
- [ ] text → content、tool_use → `tool_trace`
- [ ] 提示用 `[tools]` 一行要約
- [ ] `sync_all` 接続
- [ ] 手動: 既知フレーズで `memory_search` が当たることを確認

### Phase 2 — 取得面

- [ ] `memory_get_tools`
- [ ] `memory_get_session` に has_tools ポインタ
- [ ] CLI `tool` サブコマンド対応

### Phase 3 — 品質

- [ ] consolidate / session-hook が cursor ソースも要約対象にする
- [ ] workspace スコープ（`.agents-relay.json`）との整合
- [ ] 回帰テスト（fixture JSONL → ingest → search → get_tools）

### Phase 4 — 任意

- [ ] Composer `state.vscdb` 対応の是非判断
- [ ] tool_result / 大きな添付の扱い強化
- [ ] `tools_summary` の FTS 追加

---

## 8. テスト方針

Claude の layer2 に近い、**Cursor CLI 不要**の fixture テスト:

1. 最小 JSONL（user text / assistant text+tool_use / tool_only）を `tests/fixtures/cursor/` に置く
2. 一時 HOME で ingest
3. 断言:
   - search で user フレーズが当たる
   - assistant `content` に巨大な `arguments` JSON が**含まれない**
   - `tool_trace` に原本がある
   - `memory_get_tools` で名前一覧が取れる

---

## 9. 明示的にやらないこと（v1）

- tool_use 原本の削除・非保存
- Composer vscdb の完全サポート
- Cloud Agent 側だけの履歴（ローカル JSONL が無いもの）の遠隔取得
- Cursor 公式 API への依存（ローカルファイルが正）

---

## 10. 未決事項

実装に入る前に決めるとよい点:

1. **タイムスタンプ**  
   JSONL に無い場合、ファイル mtime 一括か、行番号ベースの合成か。
2. **tool_trace の置き場**  
   `fulltext.db` 併設か、`tools.db` 新設か。（運用単純さなら fdb 併設が楽）
3. **user メッセージ内のシステム注入**（commands / rules 全文）  
   原本保持は必須。提示用に strip するかは Phase 1 では「そのまま」、Phase 3 で整形でも可。
4. **1 assistant 行に tool が何十個もある場合の要約上限**  
   例: 先頭 20 行 + `…and N more`。原本は全部残す。

---

## 11. 成功条件

次を満たせば「Claude 同様に対応した」と言える。

1. ローカルの Cursor Agent 会話が自動 sync される  
2. 「昨日 Cursor で何した？」が `memory_search` / `memory_list_sessions` で拾える  
3. 通常のツール応答に tool 原本が埋まらず、トークンが膨らまない  
4. 必要なら `memory_get_tools` で軌道を辿れる  
5. `source="cursor"` で Claude / Antigravity と混在検索・フィルタができる  

---

## 12. 実装時の着手順（最短）

1. `db.rs` に `tool_trace` を足す  
2. `ingest_cursor.rs` で1ファイル ingest  
3. fixture テストを赤→緑  
4. `sync_all` と MCP source マッピング  
5. `memory_get_tools`  

この順なら、設計の核（分離保存 + 薄い提示）を最初の PR で検証できる。

---

## 13. Claude 木 (0.2.4+) の surface / compact との関係

正本 `/Volumes/8TB_HDD/dev/agents-relay` には、古い 0.2.1 木に無い次の層がある。

| 層 | 役割 | Cursor ingest との関係 |
| --- | --- | --- |
| **surface** (`compact::surface`) | insert 時に FTS に載せる会話表層。成功 tool_result 本体は畳む | Cursor は tool 原本を `content` に載せないので、FTS には text + `[tools]` 一行要約だけが載る。相性が良い |
| **min / mid** (`cdb.entry_compact`) | `memory_get_session` の detail 投影。consolidate 後に生成 | 提示用 `content`（要約付き）を畳む。`tool_trace` は触らない |
| **deep search** | `memory_search(deep=true)` で全文 LIKE | ツール出力本体を探す逃げ道。Cursor の巨大 tool args は `tool_trace` 側なので deep では当たらない → `memory_get_tools` が担当 |

**方針の整理:**

- `set_tool_trace` は content / FTS を書き換えない → surface 再計算は不要
- `has_tools` / `tools_tokens` は CoT の `has_reasoning` と同型のポインタ経済
- deep と tool_trace は役割が違う（deep = content/fulltext 内の文字列、tool_trace = 分離した tool_use JSON）
