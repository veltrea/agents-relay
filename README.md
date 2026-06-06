# claude-relay: Claude Codeのセッション記憶を雑に保存するやつ

## 経緯

[claude-mem](https://github.com/anthropics/claude-mem)（Claude Code のセッション記憶プラグイン）をローカル LLM で使えるようにしようとフォークしてソースコードを読んだところ、正直なところ使い物になりませんでした。

1件のツール使用ごとに AI 圧縮リクエストを飛ばす設計、タイムアウトなしの fetch、リトライ戦略の欠如、liveness と readiness の混同、圧縮後に生データを捨てる不可逆な処理 — コンピュータサイエンスの基礎が押さえられていない実装でした。詳しくは[別の記事](https://note.com/veltrea/n/n791d1defada0)に書いています。

Claude API 前提なら問題が表面化しないだけで、ローカル LLM に切り替えた瞬間に全部が致命的になります。フォーク元の修正を試みましたが、設計思想の問題なので部分的なパッチでは直せません。

で、考えてみたら AI で圧縮する必要なんてそもそもないんですよね。Claude Code は全セッションデータを `~/.claude/projects/` に JSONL で書き出しています。これを SQLite に突っ込んで、検索時に Claude 自身の 1M コンテキストで生データを理解させればいい。AI 圧縮もデーモンも要らない。

そういうわけで **claude-relay** をゼロから作りました。

## どういうものか

- Rust 製のシングルバイナリ（約1,600行）
- MCP サーバーとして Claude Code に接続し、過去のセッションを検索するツールを提供します
- デーモン不要。セッション開始時やツール呼び出し時に JSONL を差分取り込みします
- 古いデータは Markdown にアーカイブして SQLite から消す、みたいな運用もできます

## インストール

Rust のビルド環境が必要です。

```bash
git clone https://github.com/veltrea/claude-relay.git
cd claude-relay
cargo build --release

# Claude Code の MCP に登録
claude mcp add --transport stdio --scope user claude-relay -- $(pwd)/target/release/claude-relay serve
```

パスを通したい人は `target/release/claude-relay` を好きな場所にコピーしてください。

## 使い方

### まず JSONL を取り込む

```bash
# ~/.claude/projects/ 配下の全セッションを取り込み
claude-relay ingest ~/.claude/projects/

# 特定のファイルだけ
claude-relay ingest path/to/session.jsonl

# どれくらい入ったか確認
claude-relay db stats
```

自分の環境では 48 セッション、75,000 エントリくらいが入りました。

### Claude Code から使う

MCP ツールとして登録してあるので、Claude Code のセッション内で普通に聞けます。

- 「昨日の作業内容を教えて」
- 「OAuth 修正した時のやつ探して」
- 「3月20日から23日の間に何やってた？」
- 「最近のセッション一覧を見せて」

裏では `memory_search`、`memory_list_sessions`、`memory_get_session` 等の MCP ツールが呼ばれています。

### CLI でも使える

人間が直接叩く管理コマンドもあります。MCP ツール経由だとトークンを食うので、管理系は CLI でやる設計です。

```bash
# セッション一覧
claude-relay list
claude-relay list --date 2026-03-23

# セッションの中身を Markdown で出力
claude-relay export <session_id>
claude-relay export --date 2026-03-23

# DB のリセット
claude-relay db reset

# 生 SQL も叩ける（開発時に便利）
claude-relay query "SELECT type, COUNT(*) FROM raw_entries GROUP BY type"

# テスト用に手動で書き込み
claude-relay write "テストメッセージ" --type user
```

## 設計の話

### 全部保存して、読む時に絞る

最初は `user` と `assistant` だけ保存しようとしていたんですが、「全部突っ込んで読み出し時に WHERE で絞ればよくない？」と思い直しました。`system`、`progress`、`queue-operation` も全部入れています。後から「やっぱりあのデータ見たい」となっても対応できます。

### デーモン不要

ファイル監視デーモン（chokidar 等）を常駐させる案もありましたが、やめました。SessionStart フックと MCP ツール呼び出し時に差分取り込みする方式にしています。JSONL の「前回どこまで読んだか」をバイトオフセットで記録しておいて、新しい行だけ処理します。

### アーカイブ

設定ファイル（`~/.claude-relay/config.json`）で `retention_days` を指定すると、期限切れのデータを Markdown に書き出して DB から消せます。デフォルトは 30 日です。

```jsonc
{
  "retention_days": 30,
  "archive_dir": "~/.claude-relay/archive"
}
```

## 注意事項

30 分くらいで作りました。テストはほとんどやっていません。自分の環境（macOS）では動いていますが、他の環境は試していません。

バグを見つけた方、動かなかった方は [Issue](https://github.com/veltrea/claude-relay/issues) で教えてもらえると助かります。

PR は受け付けていません。思いついたら丸ごとコードを書き換えるタイプなので、PR をもらっても元のコードが残っていない可能性が高いです。気になった方はフォークして自由にやってください。バイブコーディングすれば誰でも作れます。

## ライセンス

MIT License
