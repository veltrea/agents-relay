# Changelog

## [0.2.0] - 2026-04-01

### Breaking Changes

- `agents-relay serve` は単体で DB を持たなくなりました。起動時に HTTP バックエンドサーバー (`agents-relay server`) を自動起動します。
- 環境変数が `CLAUDE_RELAY_WORKSPACE` → `AGENTS_RELAY_WORKSPACE` に変更されました。

### Architecture: Server/Client 分離

antigravity-relay の設計を移植し、単一プロセス方式から Server/Serve 分離方式に全面移行しました。

```
以前: serve(WS1), serve(WS2), serve(WS3) がそれぞれ SQLite に直接書き込み
         → 複数ワークスペース同時起動で BUSY エラーのリスク

以後: serve(WS1) ─┐
      serve(WS2) ─┼─→ HTTP:54322 → server(1プロセス) → SQLite
      serve(WS3) ─┘
         → 全書き込みが直列化され BUSY エラーを根絶
```

#### 新規ファイル

- **`src/server.rs`** — Axum HTTP サーバー (`127.0.0.1:54322`)
  - `/health` `/register` `/deregister` `/tool` エンドポイント
  - 全ツールロジックを DB と共に一手管理
  - 起動後500msで初回 sync、以後30秒ごとにバックグラウンド差分同期
  - serve プロセスが0かつ `pgrep` でも検出されない状態が約60秒続くと自動終了
  - バイナリ更新検知で古いサーバーを自動再起動

- **`src/client.rs`** — HTTP クライアントユーティリティ
  - `ensure_server_running()`: サーバーが落ちていれば自動起動・接続
  - `call_tool()`: ツール呼び出しを HTTP で転送、失敗時に自動リトライ
  - `register()` / `deregister()`: 接続数カウント管理
  - ポート番号は `~/.agents-relay/server.port` で管理（デフォルト 54322）

#### 変更ファイル

- **`src/mcp.rs`** — thin proxy に変更
  - DB への直接アクセスを完全撤廃
  - `ServerScope`・`WorkspaceConfig`・cross-scope ゲートロジックは serve 側に維持
  - `memory_unlock_cross_scope` はサーバー呼び出し不要のため serve 内で完結
  - スコープ解決（workspace/source の確定）を serve 側で行い、解決済み値をサーバーに送信

- **`src/main.rs`** — `server` サブコマンドを追加
- **`Cargo.toml`** — `axum 0.7`・`reqwest 0.12` を追加

---

## [0.1.1] - 2026-04-01

### Bug Fixes

- **BUG-001/002: クラッシュ時の重複挿入・FTS不整合を修正** (`src/ingest.rs`)
  - `ingest_file` 全体をトランザクション化（100件ごとにバッチコミット）
  - オフセット保存も同一トランザクション内で行い、クラッシュ時の重複取り込みを防止
  - `raw_entries` と `raw_entries_fts` の2つのINSERTが分断されないよう保護

- **BUG-003: Windows CRLF ファイルでオフセットがズレる問題を修正** (`src/ingest.rs`)
  - `lines()` イテレータ（改行を除去してしまう）を `read_line()` に置き換え
  - 実際に読み込んだバイト数でオフセットを計算するため、CRLF (`\r\n`) でも正確に差分追跡できる

- **BUG-004: archive コマンドの途中クラッシュで DB とファイルが不整合になる問題を修正** (`src/main.rs`)
  - `DELETE FROM raw_entries` をトランザクションで保護
  - アーカイブファイルの書き出しを冪等化（既存ファイルがあれば上書きしない）
  - `stmt` の借用を `Vec` に収集してから解放することで `conn.transaction()` を安全に呼び出せるよう修正

- **BUG-005: workspace フィルタの LIKE クエリで特殊文字が誤動作する問題を修正** (`src/db.rs`)
  - workspace パスに含まれる `%`・`_`・`\` を `ESCAPE '\\'` でエスケープ
  - `search()` と `list_sessions()` の両方に適用

- **BUG-006: `code` を含むプロセス名を VS Code と誤検知する問題を修正** (`src/detect.rs`)
  - `contains("code")` を完全一致・前方一致に変更（`recode`・`xcode` などの誤検知を解消）
  - 条件: `n == "code" || n.starts_with("code ") || n.starts_with("code-server") || n.contains("vscode") || n == "codium"`
  - リグレッション防止のためのユニットテストを追加

- **BUG-007: `memory_get_entry` の `id` パラメータが float で渡されると取得できない問題を修正** (`src/mcp.rs`)
  - AI クライアントが `3.0` のように float で送ってくる場合に `as_f64()` フォールバックで対応
  - `memory_search` / `memory_list_sessions` / `memory_get_session` では既に対応済みだったが `memory_get_entry` のみ漏れていた

- **BUG-009: workspace スコープ時に `cwd IS NULL` のエントリが除外される問題を修正** (`src/db.rs`)
  - `LIKE` 条件に `OR cwd IS NULL` を追加し、cwd を持たないエントリもスコープ内に含める
  - BUG-005 の修正と同時に `search()` と `list_sessions()` の両方に適用

### Internal Changes

- `ingest_file` / `ingest_dir` / `sync_all` のシグネチャを `&Connection` → `&mut Connection` に変更
- `ingest_antigravity::sync_all` / `ingest_brain` / `ingest_session` も同様に変更
- `mcp::serve` / `handle_request` / `handle_tool_call` も `&mut Connection` に対応

---

## [0.1.0] - 2026-03-xx

Initial release. Fork from [claude-relay](https://github.com/user/claude-relay) with multi-agent support.

### Features

- Claude Code (JSONL) と Antigravity (Markdown + JSON) の両ソースに対応
- ワークスペーススコーピング（`scope=self/cross/all`）
- PPID による呼び出し元エージェントの自動検出
- `memory_search` / `memory_get_entry` / `memory_list_sessions` / `memory_get_session` MCP ツール
- `memory_write_session` / `memory_record_summary` / `memory_unlock_cross_scope` MCP ツール（Antigravity 専用）
- SQLite WAL + FTS5 による全文検索
- `archive` / `export` / `list` / `sync` CLI コマンド
