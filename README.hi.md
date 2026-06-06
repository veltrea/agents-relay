# claude-relay: Claude Code की सेशन मेमोरी को आसानी से सेव करने वाला टूल

## पृष्ठभूमि

[claude-mem](https://github.com/anthropics/claude-mem) (Claude Code का सेशन मेमोरी प्लगइन) को लोकल LLM के साथ इस्तेमाल करने के लिए मैंने इसे फोर्क किया और सोर्स कोड पढ़ा। सच कहूँ तो, यह काम का नहीं था।

हर टूल कॉल पर AI कम्प्रेशन रिक्वेस्ट भेजने वाला डिज़ाइन, बिना timeout के fetch, retry स्ट्रेटजी की कमी, liveness और readiness का घालमेल, कम्प्रेशन के बाद रॉ डेटा हटा देने वाली irreversible प्रोसेसिंग -- कंप्यूटर साइंस की बेसिक्स भी कवर नहीं थीं। डिटेल्स मैंने [एक अलग आर्टिकल](https://note.com/veltrea/n/n791d1defada0) में लिखी हैं।

Claude API इस्तेमाल करो तो प्रॉब्लम सरफेस पर नहीं आतीं। लेकिन जैसे ही लोकल LLM पर स्विच करो, सब कुछ क्रिटिकल हो जाता है। मैंने फोर्क को ठीक करने की कोशिश की, लेकिन प्रॉब्लम डिज़ाइन फिलॉसफी में है -- पार्शियल पैच से नहीं बनेगा काम।

फिर सोचा: AI से कम्प्रेस करने की ज़रूरत ही क्या है? Claude Code सारा सेशन डेटा `~/.claude/projects/` में JSONL फॉर्मेट में लिखता है। बस इसे SQLite में डाल दो, और सर्च करते वक्त Claude को खुद ही अपने 1M टोकन कॉन्टेक्स्ट से रॉ डेटा समझने दो। न AI कम्प्रेशन चाहिए, न डेमन।

इसलिए **claude-relay** को स्क्रैच से बनाया।

## यह क्या है

- Rust में बना सिंगल बाइनरी (लगभग 1,600 लाइन)
- Claude Code से MCP सर्वर के रूप में कनेक्ट होता है और पुराने सेशन सर्च करने के टूल देता है
- डेमन की ज़रूरत नहीं। सेशन शुरू होने पर और टूल कॉल के समय JSONL को इंक्रीमेंटली इम्पोर्ट करता है
- पुराने डेटा को Markdown में आर्काइव करके SQLite से हटाने का ऑप्शन भी है

## इंस्टालेशन

Rust का बिल्ड एनवायरनमेंट चाहिए।

```bash
git clone https://github.com/veltrea/claude-relay.git
cd claude-relay
cargo build --release

# Register as MCP in Claude Code
claude mcp add --transport stdio --scope user claude-relay -- $(pwd)/target/release/claude-relay serve
```

अगर PATH में रखना चाहते हो तो `target/release/claude-relay` को अपनी पसंद की जगह कॉपी कर लो।

## इस्तेमाल कैसे करें

### पहले JSONL इम्पोर्ट करो

```bash
claude-relay ingest ~/.claude/projects/
claude-relay ingest path/to/session.jsonl
claude-relay db stats
```

मेरे एनवायरनमेंट में लगभग 48 सेशन और 75,000 एंट्रीज़ आईं।

### Claude Code से इस्तेमाल

MCP टूल के रूप में रजिस्टर है, तो Claude Code के सेशन में बस नॉर्मली पूछ सकते हो।

- "What did I work on yesterday?"
- "Find that OAuth fix"
- "What happened between March 20-23?"
- "Show me recent sessions"

बैकग्राउंड में `memory_search`, `memory_list_sessions`, `memory_get_session` जैसे MCP टूल कॉल होते हैं।

### CLI से भी चलता है

डायरेक्ट टर्मिनल से चलाने के लिए एडमिन कमांड भी हैं। MCP टूल से चलाने पर टोकन खर्च होते हैं, इसलिए एडमिन काम CLI से करने के लिए डिज़ाइन किया है।

```bash
claude-relay list
claude-relay list --date 2026-03-23
claude-relay export <session_id>
claude-relay export --date 2026-03-23
claude-relay db reset
claude-relay query "SELECT type, COUNT(*) FROM raw_entries GROUP BY type"
claude-relay write "test message" --type user
```

## डिज़ाइन के बारे में

### सब कुछ सेव करो, पढ़ते वक्त फिल्टर करो

शुरू में सिर्फ `user` और `assistant` सेव करने का प्लान था, लेकिन फिर सोचा: "सब कुछ डाल दो और पढ़ते वक्त WHERE से फिल्टर कर लो, क्या प्रॉब्लम है?" तो `system`, `progress`, `queue-operation` -- सब डालता हूँ। बाद में "वो डेटा देखना था" ऐसा हो तो भी मिल जाएगा।

### डेमन नहीं चाहिए

फाइल वॉचिंग डेमन (जैसे chokidar) चलाने का सोचा था, लेकिन छोड़ दिया। SessionStart हुक और MCP टूल कॉल के समय इंक्रीमेंटल इम्पोर्ट होता है। हर JSONL का "कहाँ तक पढ़ा" byte offset रिकॉर्ड रहता है, और सिर्फ नई लाइनें प्रोसेस होती हैं।

### आर्काइविंग

कॉन्फिग फाइल (`~/.claude-relay/config.json`) में `retention_days` सेट कर सकते हो। एक्सपायर्ड डेटा Markdown में एक्सपोर्ट होकर DB से डिलीट हो जाता है। डिफॉल्ट 30 दिन है।

```jsonc
{
  "retention_days": 30,
  "archive_dir": "~/.claude-relay/archive"
}
```

## ध्यान रखने वाली बातें

करीब 30 मिनट में बनाया है। टेस्टिंग लगभग नहीं की। मेरे एनवायरनमेंट (macOS) पर चल रहा है, दूसरे एनवायरनमेंट पर ट्राई नहीं किया।

अगर बग मिले या काम न करे तो [Issue](https://github.com/veltrea/claude-relay/issues) में बता दो।

PR accept नहीं करता। मैं उस टाइप का हूँ जो आइडिया आने पर पूरा कोड फिर से लिख देता है, तो PR मिलने तक ओरिजिनल कोड बचा हो इसकी गारंटी नहीं। इंटरेस्ट हो तो फोर्क करो और जो मन करे करो। Vibe coding से कोई भी बना सकता है।

## लाइसेंस

MIT License
