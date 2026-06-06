# claude-relay: حفظ ذاكرة جلسات Claude Code بطريقة بسيطة

## القصة

عملت فورك لـ [claude-mem](https://github.com/anthropics/claude-mem) (إضافة ذاكرة الجلسات في Claude Code) عشان أشغّله مع نماذج LLM محلية، وقرأت الكود المصدري. بصراحة، ما كان ينفع للاستخدام.

تصميم يرسل طلب ضغط بالذكاء الاصطناعي مع كل استدعاء أداة، fetch بدون timeout، ما فيه استراتيجية إعادة محاولة، خلط بين liveness و readiness، معالجة لا رجعة فيها تحذف البيانات الخام بعد الضغط -- تطبيق ما يغطي أساسيات علوم الحاسب. كتبت عن التفاصيل في [مقال منفصل](https://note.com/veltrea/n/n791d1defada0).

لو تستخدم Claude API، المشاكل ما تظهر على السطح. لكن لحظة ما تنتقل لنموذج LLM محلي، كل شيء يصير حرج. حاولت أصلح الفورك، لكن المشكلة في فلسفة التصميم نفسها -- الرقع الجزئية ما تحلها.

وبعدين فكرت: أصلاً ليش نحتاج ضغط بالذكاء الاصطناعي؟ Claude Code يكتب كل بيانات الجلسات بصيغة JSONL في `~/.claude/projects/`. يكفي نحطها في SQLite، ووقت البحث نخلي Claude نفسه يفهم البيانات الخام بسياقه اللي يوصل لمليون توكن. لا ضغط بذكاء اصطناعي ولا خدمة خلفية.

وهكذا بنيت **claude-relay** من الصفر.

## إيش هذا الشيء

- ملف تنفيذي واحد بلغة Rust (حوالي 1,600 سطر)
- يتصل بـ Claude Code كسيرفر MCP ويوفر أدوات للبحث في الجلسات السابقة
- بدون خدمة خلفية. يستورد JSONL بشكل تدريجي عند بداية الجلسة وعند استدعاء الأدوات
- تقدر تأرشف البيانات القديمة بصيغة Markdown وتحذفها من SQLite

## التثبيت

تحتاج بيئة بناء Rust.

```bash
git clone https://github.com/veltrea/claude-relay.git
cd claude-relay
cargo build --release

# Register as MCP in Claude Code
claude mcp add --transport stdio --scope user claude-relay -- $(pwd)/target/release/claude-relay serve
```

لو تبي تضيفه للـ PATH، انسخ `target/release/claude-relay` للمكان اللي تبيه.

## طريقة الاستخدام

### أولاً، استورد ملفات JSONL

```bash
claude-relay ingest ~/.claude/projects/
claude-relay ingest path/to/session.jsonl
claude-relay db stats
```

في بيئتي طلع تقريباً 48 جلسة و 75,000 مُدخل.

### الاستخدام من Claude Code

مسجّل كأداة MCP، فتقدر تسأل بشكل عادي داخل جلسة Claude Code.

- "What did I work on yesterday?"
- "Find that OAuth fix"
- "What happened between March 20-23?"
- "Show me recent sessions"

في الخلفية، تُستدعى أدوات MCP مثل `memory_search` و `memory_list_sessions` و `memory_get_session` وغيرها.

### يشتغل من سطر الأوامر بعد

فيه أوامر إدارية تقدر تستخدمها مباشرة من الترمنال. لأن الاستخدام عبر أدوات MCP يستهلك توكنات، المهام الإدارية مصممة تنفذ من CLI.

```bash
claude-relay list
claude-relay list --date 2026-03-23
claude-relay export <session_id>
claude-relay export --date 2026-03-23
claude-relay db reset
claude-relay query "SELECT type, COUNT(*) FROM raw_entries GROUP BY type"
claude-relay write "test message" --type user
```

## عن التصميم

### احفظ كل شيء، وفلتر وقت القراءة

في البداية كنت بحفظ `user` و `assistant` بس، لكن فكرت: "ليش ما نحط كل شيء ونفلتر بـ WHERE وقت القراءة؟" فصرت أحفظ `system` و `progress` و `queue-operation` كلها. لو بعدين قلت "يا ليتني شفت هالبيانات"، بتكون موجودة.

### بدون خدمة خلفية

فكرت أشغّل daemon لمراقبة الملفات (مثل chokidar)، لكن تراجعت. الاستيراد التدريجي يصير في هوك SessionStart وعند استدعاء أدوات MCP. يتم تسجيل byte offset لـ "وين وقفنا" في كل ملف JSONL، والأسطر الجديدة بس هي اللي تنعالج.

### الأرشفة

في ملف الإعدادات (`~/.claude-relay/config.json`) تقدر تحدد `retention_days` عشان تصدّر البيانات المنتهية الصلاحية لـ Markdown وتحذفها من قاعدة البيانات. الافتراضي 30 يوم.

```jsonc
{
  "retention_days": 30,
  "archive_dir": "~/.claude-relay/archive"
}
```

## ملاحظات

بنيته في حوالي 30 دقيقة. ما سويت اختبارات تقريباً. يشتغل على بيئتي (macOS)، لكن ما جربته على بيئات ثانية.

لو لقيت باق أو ما اشتغل عندك، بلّغني عبر [Issue](https://github.com/veltrea/claude-relay/issues).

ما أقبل PR. أنا من النوع اللي يعيد كتابة الكود كامل لما تجيني فكرة، فلو استلمت PR، غالباً الكود الأصلي ما بيكون موجود. لو عجبك المشروع، سوّ فورك وسوّ اللي تبيه. مع الـ vibe coding أي أحد يقدر يسويه.

## الرخصة

MIT License
