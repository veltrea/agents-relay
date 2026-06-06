# claude-relay: 简单粗暴地保存 Claude Code 会话记忆

## 背景

我 fork 了 [claude-mem](https://github.com/anthropics/claude-mem)（Claude Code 的会话记忆插件），想让它能用本地 LLM 跑起来，结果读完源码发现——说实话，根本没法用。

每次工具调用都要发一个 AI 压缩请求、fetch 没有超时、没有重试策略、liveness 和 readiness 搞混了、压缩完就把原始数据丢掉的不可逆操作——基本的计算机科学素养没有体现出来。详细内容写在[另一篇文章](https://note.com/veltrea/n/n791d1defada0)里了。

如果只用 Claude API 的话问题不会暴露，但一换成本地 LLM，所有问题立刻变成致命的。我试过在 fork 的基础上修，但这是设计思路的问题，打补丁解决不了。

后来仔细想想，其实根本不需要用 AI 来压缩。Claude Code 本来就会把所有会话数据以 JSONL 格式写到 `~/.claude/projects/` 下面。直接塞进 SQLite，查询的时候让 Claude 自己用 1M 上下文去理解原始数据就行了。不需要 AI 压缩，也不需要守护进程。

所以我从零开始写了 **claude-relay**。

## 这是什么

- Rust 写的单一二进制文件（大约 1,600 行）
- 作为 MCP 服务器连接 Claude Code，提供搜索历史会话的工具
- 不需要守护进程。会话启动时和工具调用时增量导入 JSONL
- 也可以把旧数据归档成 Markdown 然后从 SQLite 里删掉

## 安装

需要 Rust 构建环境。

```bash
git clone https://github.com/veltrea/claude-relay.git
cd claude-relay
cargo build --release

# 注册到 Claude Code 的 MCP
claude mcp add --transport stdio --scope user claude-relay -- $(pwd)/target/release/claude-relay serve
```

想加到 PATH 里的话，把 `target/release/claude-relay` 复制到你喜欢的位置就行。

## 使用方法

### 先导入 JSONL

```bash
# 导入 ~/.claude/projects/ 下的所有会话
claude-relay ingest ~/.claude/projects/

# 只导入特定文件
claude-relay ingest path/to/session.jsonl

# 看看导入了多少
claude-relay db stats
```

我自己的环境大概导入了 48 个会话、75,000 条记录。

### 在 Claude Code 里用

已经注册为 MCP 工具了，在 Claude Code 会话里直接问就行。

- "告诉我昨天干了什么"
- "帮我找一下修 OAuth 那次的记录"
- "3月20号到23号之间我做了什么？"
- "给我看看最近的会话列表"

底层调用的是 `memory_search`、`memory_list_sessions`、`memory_get_session` 等 MCP 工具。

### 也能用 CLI

也有给人直接敲的管理命令。通过 MCP 工具会消耗 token，所以管理类操作设计成用 CLI 来做。

```bash
# 会话列表
claude-relay list
claude-relay list --date 2026-03-23

# 把会话内容导出为 Markdown
claude-relay export <session_id>
claude-relay export --date 2026-03-23

# 重置数据库
claude-relay db reset

# 也能直接跑 SQL（开发调试时很方便）
claude-relay query "SELECT type, COUNT(*) FROM raw_entries GROUP BY type"

# 手动写入测试数据
claude-relay write "测试消息" --type user
```

## 设计思路

### 全部存下来，读的时候再筛选

一开始我只打算存 `user` 和 `assistant`，后来一想——"全部塞进去、读的时候用 WHERE 筛不就行了？" 于是 `system`、`progress`、`queue-operation` 也全部存了。这样以后突然想看某个数据也不会抓瞎。

### 不需要守护进程

也考虑过用文件监听守护进程（chokidar 之类的）常驻，但最后放弃了。改成在 SessionStart 钩子和 MCP 工具调用时做增量导入。用字节偏移量记录 JSONL "上次读到哪了"，只处理新增的行。

### 归档

在配置文件（`~/.claude-relay/config.json`）里设置 `retention_days`，就可以把过期数据导出为 Markdown 并从数据库中删除。默认是 30 天。

```jsonc
{
  "retention_days": 30,
  "archive_dir": "~/.claude-relay/archive"
}
```

## 注意事项

大概花了 30 分钟写的。几乎没做测试。在我自己的环境（macOS）上能跑，但没试过其他环境。

发现 bug 或者跑不起来的话，麻烦在 [Issue](https://github.com/veltrea/claude-relay/issues) 里告诉我。

不接受 PR。我是那种想到什么就把代码整个重写的人，收到 PR 的时候原来的代码大概率已经不存在了。感兴趣的话请 fork 自由发挥。vibe coding 一下谁都能写出来。

## 许可证

MIT License
