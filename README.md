# rust-whisper

Rust rewrite of `whisper-server`: drop in a video or audio file, get an English `.srt` back.
Three apps share one engine (whisper.cpp with Metal GPU on Apple Silicon):

| App | Crate | What it is |
|-----|-------|------------|
| Server | `server/` | HTTP API + web UI on port 8888, reachable from other devices on the LAN |
| Web | `web/` | The UI, written in Rust (Leptos) and compiled to WebAssembly; embedded in both binaries |
| Desktop | `desktop/` | Native macOS window (tao/wry) running the engine in-process on a private localhost port |

`core/` holds the engine and API, `common/` the types shared with the WASM UI.

## Run

```bash
./run.sh            # server + web UI: http://127.0.0.1:8888 (LAN address is printed too)
./run.sh desktop    # desktop app
./run.sh app        # build dist/Whisper.app (drag it to /Applications)
./run.sh test       # unit tests
```

First run installs what is missing (wasm32 target, trunk, a private CMake in `.tools/`) and builds everything.
Needs Rust (rustup) and `ffmpeg` (`brew install ffmpeg`).

Models are downloaded on first use to `~/.cache/rust-whisper/models` (override with `WHISPER_HOME`) and shared by the server and the desktop app.
Other settings: `PORT`, `HOST`, `THREADS`.

## Japanese / Chinese → English

* **Model per spoken language.** The model you pick is remembered for each language (stored in `settings.json`), so English can stay on Small while Japanese and Chinese use **Large v2**, which translates CJK speech to English better than Large v3 (v3 tends to leave words untranslated). `Large v2 (q5)` is a lighter alternative.
* **No duplicated lines.** Speech is found with Silero VAD and cut into pieces of at most ~24 s. Each piece is translated on its own, so a repetition loop cannot spread, and silence/music (where Whisper hallucinates) is never decoded. Repeated phrases, repeated lines (also A-B-A-B loops) and known hallucinations ("Thanks for watching", "Subtitles by…") are removed afterwards.
* **Untranslated words.** A piece whose English still contains kana, kanji, hangul, etc. is translated again with an English prompt and with temperature sampling; the attempt with the fewest untranslated characters wins.
* Long lines are split at sentence or comma boundaries into subtitle-sized lines.

English audio keeps the simple path: one whisper.cpp pass with built-in VAD.
