# rust-whisper

Rust rewrite of `whisper-server`: drop in a video or audio file, get an English `.srt` back.
Three apps share one engine (whisper.cpp with Metal GPU on Apple Silicon):

| App | Crate | What it is |
|-----|-------|------------|
| Server | `server/` | HTTP API + web UI on port 8888, reachable from other devices on the LAN |
| Web | `web/` | The UI, written in Rust (Leptos) and compiled to WebAssembly; embedded in both binaries |
| Desktop | `desktop/` | Native macOS window (tao/wry) running the engine in-process on a private localhost port |
| Client | `client/` | Windows app (egui) that sends videos from a Windows PC to the server and plays them in VLC |

`core/` holds the engine and API, `common/` the types shared with the WASM UI.

## Run

```bash
./run.sh            # server + web UI: http://127.0.0.1:8888 (LAN address is printed too)
./run.sh desktop    # desktop app
./run.sh app        # build dist/Whisper.app (drag it to /Applications)
./run.sh test       # unit tests
./run.sh stop       # stop the running server / desktop app
./run.sh client     # build dist/whisper-client.exe for Windows (cross-compiled with zig)
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

## Windows client

`./run.sh client` builds `dist/whisper-client.exe` (single file, no installer). Copy it to the Windows PC and run it.

* **Header:** server IP and port, **Connect**. Once connected, the spoken-language and model lists appear (the model is remembered per language, shared with the web UI). The last server is reconnected automatically on the next start.
* **Folder:** pick a folder with **Browse…**. Sub-folders are scanned too.
* **Left panel:** videos that have no `.srt` yet, as a folder tree. Tick videos (or right-click a folder) and press **Generate subtitles**. Files are sent to the server one by one and the English `.srt` is saved next to each video, after which the video moves to the right panel.
* **Right panel:** videos that already have a subtitle (`name.srt`, `name.en.srt`, …), with their `.srt` files under them.
* **Double-click** a video to play it in VLC; on the right, the video plays with its subtitle loaded, and double-clicking a specific `.srt` plays the video with that one. Right-click for more (show in Explorer, regenerate).
* VLC is found automatically (Program Files or the registry); **Set VLC…** picks `vlc.exe` by hand.

The Mac running the server must accept connections on the port (allow it if macOS asks about incoming connections).
