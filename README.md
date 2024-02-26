# grace

macOS only for now.

You'll need:

- Deepgram API key: https://console.deepgram.com
    - Log into your Deepgram account, if you already have one, or create a new one using your GitHub account, and create a New API Key;
- OpenAI API key: https://platform.openai.com/api-keys
    - Go ahead and create a new API key on OpenAI to further use with grace;
    

In Terminal:

```
curl -OL https://github.com/trevyn/grace/releases/latest/download/grace
chmod +x grace
xattr -dr com.apple.quarantine grace
OPENAI_API_KEY=sk-YOUR-OPENAI-API-KEY-HERE 
DEEPGRAM_API_KEY=YOUR-DEEPGRAM-API-KEY-HERE ./grace
```
