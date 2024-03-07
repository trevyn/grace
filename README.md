# grace

macOS is tested, Windows is largely untested.

<img width="1114" alt="Screen Shot 2024-03-06 at 4 55 05 PM" src="https://github.com/trevyn/grace/assets/230691/1ff14230-d544-441c-b98f-6d4be6eec829">

## Getting started

You'll need:

- Deepgram API key: https://console.deepgram.com
  - Log into your Deepgram account, if you already have one, or create a new one using your GitHub account, and create a New API Key ("Member" permissions is fine);
- OpenAI API key: https://platform.openai.com/api-keys
  - Go ahead and create a new API key on OpenAI to further use with grace;
    - Add some $ on your platform.openai.com account;

In Terminal:

```
curl -OL https://github.com/trevyn/grace/releases/latest/download/grace
chmod +x grace
xattr -dr com.apple.quarantine grace
./grace
```

1. once you're in, enter your deepgram and openai api keys in the upper-left
2. press the record button
3. have it listen in to your conversation for like 5-10 minutes loosely
4. name the speakers on the left by the numbers and see their names show up real time
5. click add window
6. ask questions about the transcript, grace has your conversation now as context eg: "summarize the above transcript and point out any blindspots, answer the sentence stem for each participant: what I really want but I'm not saying is.." (ask /AltonSun on Facebook if you have questions about how to use it)
7. hit command+return to send it off and see.
8. feel free to edit the first prompt if you want to change it, or continue by asking follow up questions later
9. open a second window with 'add window' if you'd like to carry on a second or more conversation simultaneously
10. enjoy having every area of your life ambiently enhanced by an AI!

## Hacking on grace

If you want to hack on the code (pull requests welcome!), install Rust:

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

or on windows, download and run https://static.rust-lang.org/rustup/dist/i686-pc-windows-gnu/rustup-init.exe

clone the repo and:

```
cargo run
```
