# grace

macOS only for now.

You'll need:

- Deepgram API key: https://console.deepgram.com
    - Log into your Deepgram account, if you already have one, or create a new one using your GitHub account, and create a New API Key;
- OpenAI API key: https://platform.openai.com/api-keys
    - Go ahead and create a new API key on OpenAI to further use with grace;
        - Add some $ on your platform.openai.com account;
    

In Terminal:

```
curl -OL https://github.com/trevyn/grace/releases/latest/download/grace
chmod +x grace
xattr -dr com.apple.quarantine grace
OPENAI_API_KEY=sk-YOUR-OPENAI-API-KEY-HERE DEEPGRAM_API_KEY=YOUR-DEEPGRAM-API-KEY-HERE ./grace
```

1. once you're in, press the record button
2. have it listen in to your conversation for like 5-10 minutes loosely
3. name the speakers on the left by the numbers and see their names show up real time
4. click add window
5. ask questions about the transcript, grace has your conversation now as context eg: "summarize the above transcript and point out any blindspots, answer the sentence stem for each participant: what I really want but I'm not saying is.." (ask /AltonSun on Facebook if you have questions about how to use it)
7. hit ctrl+return to send it off and see.
8. feel free to edit the first prompt if you want to change it, or continue by asking follow up questions later
9. open a second window with 'add window' if you'd like to carry on a second or more conversation simultaneously
10. enjoy having every area of your life ambiently enhanced by an AI!
