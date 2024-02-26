# grace

macOS only for now.

You'll need:

- Deepgram API key: https://console.deepgram.com
- OpenAI API key: https://platform.openai.com/api-keys

In Terminal:

```
curl -OL https://github.com/trevyn/grace/releases/latest/download/grace
chmod +x grace
xattr -dr com.apple.quarantine grace
OPENAI_API_KEY=sk-YOUR-OPENAI-API-KEY-HERE DEEPGRAM_API_KEY=YOUR-DEEPGRAM-API-KEY-HERE ./grace
```
