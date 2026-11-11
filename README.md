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

* **Model per spoken language.** The model you pick is remembered for each language (stored in `settings.json`), so English can stay on Small while Japanese and Chinese use **Large v2** (the usual choice for translating to English). **Large v3** is worth comparing on the same film: with the clean-up below it did as well in tests. `Large v2 (q5)` is a lighter alternative.
* **Kotoba Bilingual** (Japanese only): [kotoba-whisper-bilingual](https://huggingface.co/kotoba-tech/kotoba-whisper-bilingual-v1.0), distilled from Large v3 and trained specifically for Japanese → English speech translation. About 1.5 GB and much faster than Large v2.
* **Film audio.** The track in the spoken language is picked when a file has several, and 5.1/7.1 tracks are folded to mono from the centre (dialogue) channel instead of mixing all music and effects channels in at the same level.
* **No missing lines.** Silero VAD (tuned to keep speech under music) cuts the audio into pieces of at most ~15 s. A piece that comes back empty, much shorter than its speech, or still in Japanese/Chinese is translated again with an English prompt and with temperature sampling, and the best attempt wins. Whisper's own "no speech" skipping is relaxed because VAD already removed the silence.
* **No duplicated lines.** Each piece is translated on its own, so a repetition loop cannot spread. Repeated phrases, repeated lines (also A-B-A-B loops) and YouTube-style hallucinations ("Thanks for watching", "Please subscribe", "Subtitles by…") are removed.
* Long lines are split at sentence or comma boundaries into subtitle-sized lines.
* **TV-style timing.** Every line is fitted to the voice that VAD heard: it appears when the speaker starts and disappears ~0.35 s after they stop, instead of hanging over the action that follows. A line that covers two separate stretches of speech is split at a sentence boundary, short lines stay at least ~0.85 s (long enough to read), nothing stays longer than 7 s, and lines never overlap. This applies to English audio too.

English audio keeps the simple path: one whisper.cpp pass with built-in VAD.

## Windows client

`./run.sh client` builds `dist/whisper-client.exe` (single file, no installer). Copy it to the Windows PC and run it.

* **Header:** server IP and port, **Connect**. Once connected, the spoken-language and model lists appear (the model is remembered per language, shared with the web UI). The last server is reconnected automatically on the next start.
* **Folder:** pick a folder with **Browse…**. Sub-folders are scanned too.
* **Left panel:** videos that have no `.srt` yet, as a folder tree. Tick videos (or right-click a folder) and press **Generate subtitles**. Files are sent to the server one by one and the English `.srt` is saved next to each video, after which the video moves to the right panel.
* **Right panel:** videos that already have a subtitle (`name.srt`, `name.en.srt`, …), with their `.srt` files under them.
* **Double-click** a video to play it in VLC; on the right, the video plays with its subtitle loaded, and double-clicking a specific `.srt` plays the video with that one. Right-click for more (show in Explorer, regenerate).
* VLC is found automatically (Program Files or the registry); **Set VLC…** picks `vlc.exe` by hand.
* The app opens on the folder it was started from (put `whisper-client.exe` in your video folder and double-click it); **Browse…** picks another.
* **Progress list** at the bottom: every queued file with its state (waiting, uploading, translating with a progress bar, saved, failed with the reason) and how long it took. Double-click a row to play it.
* If the server can't be reached for a while (Mac asleep, Wi-Fi reconnecting), the client keeps retrying for up to 20 minutes instead of failing. The server itself keeps the Mac from idle-sleeping while it uploads or processes a file (`caffeinate`), until 3 minutes after the job. Closing the lid still sleeps the Mac.

The Mac running the server must accept connections on the port (allow it if macOS asks about incoming connections).
