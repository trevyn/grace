# scarlett

macOS only for now.

You'll need:

- Deepgram API key: https://console.deepgram.com
- OpenAI API key: https://platform.openai.com/api-keys

In Terminal:

```
curl -OL https://github.com/trevyn/scarlett/releases/latest/download/scarlett
chmod +x scarlett
xattr -dr com.apple.quarantine scarlett
OPENAI_API_KEY=sk-YOUR-OPENAI-API-KEY-HERE DEEPGRAM_API_KEY=YOUR-DEEPGRAM-API-KEY-HERE ./scarlett
```
