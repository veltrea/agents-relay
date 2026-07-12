# agents-relay: AI コーディングエージェントのセッション記憶

**Read this in other languages:** [English](README.en.md)

Claude Code と Antigravity のセッション履歴を SQLite に取り込み、MCP ツールとして「過去の記憶」を提供するシングルバイナリです。旧名 claude-relay。マルチエージェント対応にあわせて改名しました。

## 経緯

[claude-mem](https://github.com/anthropics/claude-mem)（Claude Code のセッション記憶プラグイン）をローカル LLM で使えるようにしようとフォークしてソースコードを読んだところ、正直なところ使い物になりませんでした。

1件のツール使用ごとに AI 圧縮リクエストを飛ばす設計、タイムアウトなしの fetch、リトライ戦略の欠如、liveness と readiness の混同、圧縮後に生データを捨てる不可逆な処理 — コンピュータサイエンスの基礎が押さえられていない実装でした。詳しくは[別の記事](https://note.com/veltrea/n/n791d1defada0)に書いています。

で、考えてみたら取り込み時に AI で圧縮する必要なんてそもそもないんですよね。Claude Code は全セッションデータを `~/.claude/projects/` に JSONL で書き出しています。これを SQLite に**無損失で**突っ込んで、要約や圧縮は「あとから・何度でも作り直せる派生物」として扱えばいい。

そういうわけでゼロから作ったのがこれです。

## 設計原則

1. **原本は絶対に失わない。** 取り込みで切り詰めない。削除の前には必ず無損失エクスポート。要約アルゴリズムを将来差し替えても、原本から何度でも再生成できる。
2. **トークン節約は保存側でなく提示側で行う。** 検索結果には長大データ（思考過程・巨大本文）を含めず、ポインタだけ返す。エージェントが必要と判断したときだけ専用ツールで取りに行く。
3. **デーモン不要。** MCP ツール呼び出し時にバイトオフセットで差分取り込み。

## ストレージ構成（3 層 + 派生物）

| ファイル | 中身 | 消してよいか |
|---|---|---|
| `memory.db` | エントリ本体（inline）+ FTS 索引 + 要約 + 事実 | 不可（本体） |
| `fulltext.db` | 50KB 超の本文の**全文原本** | 消すと全文が失われる |
| `reasoning.db` | 生 chain-of-thought（thinking）の原本 | 消しても検索は無傷 |
| `archive/YYYY/MM/DD.jsonl` | アーカイブ済みエントリの無損失原本（追記専用） | 消すと復元不能 |
| `vectors.db` | 要約・事実の埋め込みベクトル（意味検索用） | `embed` で再生成可 |
| `memory-light.db` | `db export-light` の派生物（持ち出し用） | いつでも再生成可 |

## インストール

Rust のビルド環境が必要です。

```bash
git clone https://github.com/veltrea/agents-relay.git
cd agents-relay
cargo build --release

# Claude Code の MCP に登録
claude mcp add --transport stdio --scope user agents-relay -- $(pwd)/target/release/agents-relay serve
```

## 使い方

### Claude Code / Antigravity から使う

MCP ツールとして登録すれば、セッション内で普通に聞くだけです。取り込みはツール呼び出し時に自動で走ります。

- 「昨日の作業内容を教えて」
- 「OAuth 修正した時のやつ探して」
- 「このプロジェクトで何を決めたっけ？」（→ `memory_facts` が蒸留済みの決定・制約を返す）
- 「あのときなぜその設計にした？」（→ `memory_get_reasoning` が当時の思考過程を返す）

主な MCP ツール:

| ツール | 用途 |
|---|---|
| `memory_search` | FTS 全文検索（workspace 名・ブランチ名でも当たる） |
| `memory_list_sessions` | セッション一覧（蒸留済みなら要約スニペット付き） |
| `memory_get_session` | セッション全文（要約が先頭に付く） |
| `memory_get_entry` | 1 エントリの全文（fulltext 原本から） |
| `memory_get_reasoning` | 保存された思考過程（summary / full） |
| `memory_facts` | 蒸留済みの持続的事実（低トークン） |

### 睡眠時整理（要約と事実の蒸留）

終了したセッションをローカル LLM（LM Studio 等 OpenAI 互換 API）で蒸留します。LLM が落ちていれば抽出圧縮にフォールバックし、後から `--upgrade` で格上げできます。

```bash
# 直近の終了セッションを蒸留（要約 + 事実抽出）
agents-relay consolidate

# ヒューリスティック要約を LLM 要約に格上げ
agents-relay consolidate --upgrade

# LLM を有効化（既定はオフ）
agents-relay config set reasoning_llm_enabled true
agents-relay config set reasoning_llm_base_url http://localhost:1234/v1
```

### 意味検索（ハイブリッド検索）

FTS5 は日本語の分かち書きに弱く、言い回しが違うと当たりません。埋め込みモデル（bge-m3 等）を設定すると、`memory_search` が FTS の結果に加えて要約・事実の意味的近傍を返すようになります。

```bash
agents-relay config set embedding_model text-embedding-bge-m3
agents-relay embed   # 既存の要約・事実をベクトル化（以後は consolidate が自動で埋め込む）
```

### 管理 CLI

```bash
agents-relay db stats             # 件数・DB 別サイズ
agents-relay db backup            # 3 DB を backup_dir へ整合スナップショット（世代ローテーション）
agents-relay db backfill-fulltext # 過去に切り詰めた本文・CoT を元 JSONL から回収
agents-relay db export-light      # 軽量コピー生成（持ち出し用）
agents-relay db rebuild-fts       # FTS 索引の再構築
agents-relay archive              # 期限切れエントリを無損失 JSONL + Markdown にアーカイブして削除
agents-relay db restore-archive ~/.agents-relay/archive/2026/01/05.jsonl  # アーカイブから復元
agents-relay list --date 2026-07-12
agents-relay export <session_id>
agents-relay query "SELECT type, COUNT(*) FROM raw_entries GROUP BY type"
```

## 設定

`~/.agents-relay/config.json`:

```jsonc
{
  "retention_days": 30,                              // archive の対象になる日数
  "archive_dir": "~/.agents-relay/archive",
  "reasoning_llm_enabled": false,                    // consolidate / CoT 要約に LLM を使うか
  "reasoning_llm_base_url": "http://localhost:1234/v1",
  "reasoning_llm_model": "",                         // 空なら /v1/models から自動検出
  "reasoning_llm_timeout_secs": 120,
  "backup_dir": "~/.agents-relay/backups",           // db backup の出力先（別ディスク推奨）
  "embedding_model": ""                              // 意味検索用の埋め込みモデル（空 = 無効）
}
```

設計の詳細は [SPEC.md](SPEC.md) を参照してください。

## 注意事項

自分の環境（macOS）で動かすために作っているものです。他の環境は試していません。

バグを見つけた方、動かなかった方は [Issue](https://github.com/veltrea/agents-relay/issues) で教えてもらえると助かります。

PR は受け付けていません。思いついたら丸ごとコードを書き換えるタイプなので、PR をもらっても元のコードが残っていない可能性が高いです。気になった方はフォークして自由にやってください。

## ライセンス

MIT License
