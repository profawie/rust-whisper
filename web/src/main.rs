//! Whisper Server web UI, written in Rust (Leptos) and compiled to WebAssembly.
//! Same layout and behaviour as the original index.html.
use common::{Config, IpInfo, LanguageModelPref, LogLine, ModelInfo, State};
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{DragEvent, EventSource, File, MessageEvent, ProgressEvent, XmlHttpRequest};

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}

// ------------------------------------------------------------------ helpers
fn window() -> web_sys::Window {
    web_sys::window().unwrap()
}

fn fmt(n: f64) -> String {
    const GB: f64 = 1073741824.0;
    const MB: f64 = 1048576.0;
    if n >= GB {
        format!("{:.2} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else {
        format!("{:.0} KB", n / 1024.0)
    }
}

async fn fetch_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let resp: web_sys::Response = JsFuture::from(window().fetch_with_str(url))
        .await
        .map_err(|e| format!("{e:?}"))?
        .unchecked_into();
    let text = JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?).await.map_err(|e| format!("{e:?}"))?;
    serde_json::from_str(&text.as_string().unwrap_or_default()).map_err(|e| e.to_string())
}

/// POST; returns Ok(json) on 2xx, Err(detail) otherwise.
async fn post_json(url: &str, body: Option<String>) -> Result<serde_json::Value, String> {
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    if let Some(b) = body {
        let headers = web_sys::Headers::new().unwrap();
        headers.set("Content-Type", "application/json").unwrap();
        init.set_headers(&headers);
        init.set_body(&JsValue::from_str(&b));
    }
    let resp: web_sys::Response = JsFuture::from(window().fetch_with_str_and_init(url, &init))
        .await
        .map_err(|e| format!("{e:?}"))?
        .unchecked_into();
    let ok = resp.ok();
    let text = JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
        .await
        .ok()
        .and_then(|t| t.as_string())
        .unwrap_or_default();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    if ok {
        Ok(json)
    } else {
        Err(json.get("detail").and_then(|d| d.as_str()).unwrap_or("Request failed.").to_string())
    }
}

fn set_dark(dark: bool) {
    if let Some(el) = window().document().and_then(|d| d.document_element()) {
        let _ = el.class_list().toggle_with_force("dark", dark);
    }
    if let Ok(Some(s)) = window().local_storage() {
        let _ = s.set_item("theme", if dark { "dark" } else { "light" });
    }
}

fn is_dark() -> bool {
    window().document().and_then(|d| d.document_element()).map(|e| e.class_list().contains("dark")).unwrap_or(false)
}

fn copy_text(text: &str) {
    let _ = window().navigator().clipboard().write_text(text);
}

const DROP_TITLE: &str = "Drop a video or audio file here";
const DROP_SUB: &str = "or click to browse · one file at a time · max 10 GB";

// ------------------------------------------------------------------ app
#[component]
fn App() -> impl IntoView {
    let cfg = RwSignal::new(None::<Config>);
    let file = RwSignal::new_local(None::<File>);
    let language = RwSignal::new(String::new());
    let model = RwSignal::new(String::new());
    let server = RwSignal::new(State { status: "idle".into(), ..Default::default() });
    let uploading = RwSignal::new(false);
    let xhr_slot = StoredValue::new_local(None::<XmlHttpRequest>);
    let stage = RwSignal::new(("Idle".to_string(), Some(0.0f64)));
    let err = RwSignal::new(None::<String>);
    let file_err = RwSignal::new(None::<String>);
    let logs = RwSignal::new(Vec::<(u64, LogLine)>::new());
    let log_seq = StoredValue::new(0u64);
    let conn = RwSignal::new(("connecting…", "ml-2 text-xs font-normal text-slate-400"));
    let ip = RwSignal::new("…".to_string());
    let follow = RwSignal::new(true);
    let drag_over = RwSignal::new(false);
    let log_ref = NodeRef::<html::Div>::new();
    let file_input = NodeRef::<html::Input>::new();

    let busy = move || {
        let s = server.with(|s| s.status.clone());
        uploading.get() || s == "uploading" || s == "processing"
    };
    let current_model = move || -> Option<ModelInfo> {
        cfg.with(|c| c.as_ref().and_then(|c| c.models.iter().find(|m| m.id == model.get()).cloned()))
    };

    // ---- IP (refreshed so it follows network changes)
    let refresh_ip = move || {
        spawn_local(async move {
            match fetch_json::<IpInfo>("/api/ip").await {
                Ok(d) => ip.set(d.ip.map(|i| format!("{i}:{}", d.port)).unwrap_or_else(|| "not connected".into())),
                Err(_) => ip.set("unavailable".into()),
            }
        })
    };
    refresh_ip();
    set_interval(refresh_ip, std::time::Duration::from_secs(10));

    // ---- file selection
    let set_file = move |f: Option<File>| {
        file_err.set(None);
        let max = cfg.with_untracked(|c| c.as_ref().map(|c| c.max_bytes).unwrap_or(u64::MAX)) as f64;
        let f = match f {
            Some(f) if f.size() > max => {
                file_err.set(Some(format!("{} is {}; the limit is {}.", f.name(), fmt(f.size()), fmt(max))));
                None
            }
            f => f,
        };
        file.set(f);
    };

    // ---- per-language model choice
    let model_for = move |lang: &str| -> String {
        cfg.with_untracked(|c| {
            let c = c.as_ref().unwrap();
            let pick = c.language_models.get(lang).cloned().unwrap_or_else(|| c.default_model.clone());
            let ok = c.models.iter().any(|m| m.id == pick && (lang == "en" || m.multi.is_some()));
            if ok { pick } else { c.default_model.clone() }
        })
    };
    let on_language = move |lang: String| {
        model.set(model_for(&lang));
        language.set(lang);
    };
    let on_model = move |m: String| {
        let lang = language.get_untracked();
        model.set(m.clone());
        cfg.update(|c| {
            if let Some(c) = c {
                c.language_models.insert(lang.clone(), m.clone());
            }
        });
        let body = serde_json::to_string(&LanguageModelPref { language: lang, model: m }).unwrap();
        spawn_local(async move {
            let _ = post_json("/api/language-model", Some(body)).await;
        });
    };

    // ---- server state
    let on_state = move |s: State| {
        if !uploading.get_untracked() {
            let st = match s.status.as_str() {
                "idle" => ("Idle".to_string(), Some(0.0)),
                "uploading" => ("Uploading…".to_string(), None),
                _ => (
                    format!("{}{}", s.stage, if s.filename.is_empty() { String::new() } else { format!(" — {}", s.filename) }),
                    Some(s.progress),
                ),
            };
            stage.set(st);
        }
        if s.status == "error" {
            err.set(Some(s.error.clone()));
        }
        server.set(s);
    };

    // ---- SSE log viewer
    let connect = move || {
        let es = EventSource::new("/api/events").unwrap();
        let on_open = Closure::<dyn FnMut()>::new(move || conn.set(("● live", "ml-2 text-xs font-normal text-emerald-500")));
        let on_error = Closure::<dyn FnMut()>::new(move || conn.set(("○ reconnecting…", "ml-2 text-xs font-normal text-amber-500")));
        let on_reset = Closure::<dyn FnMut()>::new(move || logs.set(Vec::new()));
        let on_log = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Some(line) = e.data().as_string().and_then(|d| serde_json::from_str::<LogLine>(&d).ok()) {
                let id = log_seq.get_value();
                log_seq.set_value(id + 1);
                logs.update(|l| {
                    l.push((id, line));
                    if l.len() > 2000 {
                        let extra = l.len() - 2000;
                        l.drain(..extra);
                    }
                });
            }
        });
        let on_st = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Some(s) = e.data().as_string().and_then(|d| serde_json::from_str::<State>(&d).ok()) {
                on_state(s);
            }
        });
        es.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        es.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        let _ = es.add_event_listener_with_callback("reset", on_reset.as_ref().unchecked_ref());
        let _ = es.add_event_listener_with_callback("log", on_log.as_ref().unchecked_ref());
        let _ = es.add_event_listener_with_callback("state", on_st.as_ref().unchecked_ref());
        on_open.forget();
        on_error.forget();
        on_reset.forget();
        on_log.forget();
        on_st.forget();
    };

    // auto-scroll the log
    Effect::new(move |_| {
        logs.track();
        if follow.get_untracked() {
            request_animation_frame(move || {
                if let Some(el) = log_ref.get_untracked() {
                    el.set_scroll_top(el.scroll_height());
                }
            });
        }
    });

    // ---- init
    spawn_local(async move {
        if let Ok(c) = fetch_json::<Config>("/api/config").await {
            let lang = c.default_language.clone();
            cfg.set(Some(c));
            on_language(lang);
            connect();
        }
    });

    // ---- upload (raw body, so a 10 GB file streams to disk instead of being buffered)
    let go = move |_| {
        let Some(f) = file.get_untracked() else { return };
        if busy() {
            return;
        }
        let q = format!(
            "/api/transcribe?model={}&language={}",
            js_sys::encode_uri_component(&model.get_untracked()),
            js_sys::encode_uri_component(&language.get_untracked())
        );
        let xhr = XmlHttpRequest::new().unwrap();
        xhr.open("POST", &q).unwrap();
        xhr.set_request_header("X-Filename", &String::from(js_sys::encode_uri_component(&f.name()))).unwrap();

        let on_progress = Closure::<dyn FnMut(ProgressEvent)>::new(move |e: ProgressEvent| {
            if e.length_computable() {
                let p = e.loaded() / e.total() * 100.0;
                stage.set((format!("Uploading {} / {}", fmt(e.loaded()), fmt(e.total())), Some(p)));
            }
        });
        let x2 = xhr.clone();
        let on_load = Closure::<dyn FnMut()>::new(move || {
            uploading.set(false);
            xhr_slot.set_value(None);
            let status = x2.status().unwrap_or(0);
            if status >= 400 {
                let text = x2.response_text().ok().flatten().unwrap_or_default();
                let msg = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.get("detail").and_then(|d| d.as_str()).map(String::from))
                    .unwrap_or_else(|| x2.status_text().unwrap_or_default());
                err.set(Some(msg));
            } else {
                set_file(None);
            }
            // re-apply the server state now that our own upload is finished
            on_state(server.get_untracked());
        });
        let on_error = Closure::<dyn FnMut()>::new(move || {
            uploading.set(false);
            xhr_slot.set_value(None);
            err.set(Some("Upload failed (connection lost).".into()));
        });
        let on_abort = Closure::<dyn FnMut()>::new(move || {
            uploading.set(false);
            xhr_slot.set_value(None);
        });
        if let Ok(up) = xhr.upload() {
            up.set_onprogress(Some(on_progress.as_ref().unchecked_ref()));
        }
        xhr.set_onload(Some(on_load.as_ref().unchecked_ref()));
        xhr.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        xhr.set_onabort(Some(on_abort.as_ref().unchecked_ref()));
        on_progress.forget();
        on_load.forget();
        on_error.forget();
        on_abort.forget();

        uploading.set(true);
        err.set(None);
        server.update(|s| {
            if s.status == "done" {
                s.status = "idle".into();
            }
        });
        stage.set(("Uploading…".into(), Some(0.0)));
        xhr_slot.set_value(Some(xhr.clone()));
        let _ = xhr.send_with_opt_blob(Some(&f));
    };

    let clear = move |_| {
        if let Some(x) = xhr_slot.get_value() {
            let _ = x.abort();
        }
        spawn_local(async move {
            match post_json("/api/clear", None).await {
                Err(e) => err.set(Some(if e.is_empty() { "Could not clear.".into() } else { e })),
                Ok(_) => {
                    err.set(None);
                    set_file(None);
                }
            }
        });
    };

    // ---- dropzone handlers
    let on_drag_enter = move |e: DragEvent| {
        e.prevent_default();
        if !busy() {
            drag_over.set(true);
        }
    };
    let on_drag_leave = move |e: DragEvent| {
        e.prevent_default();
        drag_over.set(false);
    };
    let on_drop = move |e: DragEvent| {
        e.prevent_default();
        drag_over.set(false);
        if busy() {
            return;
        }
        let Some(files) = e.data_transfer().and_then(|d| d.files()) else { return };
        if files.length() > 1 {
            file_err.set(Some("Only one file at a time. Please drop a single file.".into()));
            return;
        }
        set_file(files.get(0));
    };

    // ---- derived view values
    let hint = move || {
        let Some(m) = current_model() else { return String::new() };
        let lang = language.get();
        if lang == "en" {
            return format!("Will use \"{}\" with voice detection. Output: English SRT.", m.en);
        }
        let lang_name = cfg.with(|c| {
            c.as_ref().and_then(|c| c.languages.iter().find(|l| l.code == lang).map(|l| l.name.clone())).unwrap_or_default()
        });
        let mut h = format!(
            "Will use multilingual \"{}\" and translate to English. Output: English SRT. This model is remembered for {lang_name}.",
            m.multi.clone().unwrap_or_default()
        );
        if cfg.with(|c| c.as_ref().map(|c| c.models.iter().any(|x| x.multi.is_none())).unwrap_or(false)) {
            h.push_str(" (Turbo is English-only, so it is disabled.)");
        }
        if common::is_cjk_language(&lang) && !m.id.starts_with("large-v2") {
            h.push_str(" Tip: Large v2 gives the best Japanese/Chinese → English translations.");
        }
        h
    };
    let bar_class = move || {
        let color = match server.with(|s| s.status.clone()).as_str() {
            "error" => "bg-red-500",
            "done" => "bg-emerald-500",
            _ => "bg-indigo-600",
        };
        format!("h-full rounded-full transition-all duration-300 {color}")
    };
    let done = move || server.with(|s| s.status == "done") && !uploading.get();
    let clear_disabled = move || {
        let s = server.with(|s| s.status.clone());
        (s == "uploading" && !uploading.get()) || s == "processing"
    };
    let is_desktop = move || cfg.with(|c| c.as_ref().map(|c| c.desktop).unwrap_or(false));

    let drop_class = move || {
        let base = "flex cursor-pointer flex-col items-center justify-center rounded-lg border-2 border-dashed border-slate-300 px-4 py-10 text-center transition hover:border-indigo-400 hover:bg-indigo-50/50 dark:border-slate-700 dark:hover:border-indigo-500 dark:hover:bg-indigo-500/5 aria-disabled:cursor-not-allowed aria-disabled:opacity-50";
        if drag_over.get() { format!("{base} border-indigo-500 bg-indigo-50/50") } else { base.to_string() }
    };

    view! {
      <div class="mx-auto max-w-3xl px-4 py-8">
        <header class="mb-6 flex items-center justify-between">
          <div>
            <h1 class="text-2xl font-semibold tracking-tight">"Whisper Server"</h1>
            <p class="text-sm text-slate-500 dark:text-slate-400">"Video / audio → English SRT, powered by whisper.cpp"</p>
            <Show when=move || !is_desktop()>
              <p class="mt-1 text-sm text-slate-500 dark:text-slate-400">"IP: "
                <button type="button" title="Click to copy" on:click=move |_| copy_text(&ip.get_untracked())
                  class="font-mono text-slate-700 hover:underline dark:text-slate-200">{move || ip.get()}</button>
              </p>
            </Show>
          </div>
          <button type="button" aria-label="Toggle dark mode" on:click=move |_| set_dark(!is_dark())
            class="rounded-lg border border-slate-300 bg-white p-2 text-slate-600 hover:bg-slate-100 dark:border-slate-700 dark:bg-slate-900 dark:text-slate-300 dark:hover:bg-slate-800">
            <svg class="hidden h-5 w-5 dark:block" fill="none" stroke="currentColor" stroke-width="1.8" viewBox="0 0 24 24"><circle cx="12" cy="12" r="4"/><path stroke-linecap="round" d="M12 2v2m0 16v2M4.9 4.9l1.4 1.4m11.4 11.4 1.4 1.4M2 12h2m16 0h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/></svg>
            <svg class="h-5 w-5 dark:hidden" fill="none" stroke="currentColor" stroke-width="1.8" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/></svg>
          </button>
        </header>

        <main class="space-y-5">
          // Dropzone
          <section class="rounded-xl border border-slate-200 bg-white p-5 shadow-sm dark:border-slate-800 dark:bg-slate-900">
            <label for="file" tabindex="0" class=drop_class aria-disabled=move || busy().to_string()
              on:dragenter=on_drag_enter on:dragover=on_drag_enter on:dragleave=on_drag_leave on:drop=on_drop
              on:click=move |e| if busy() { e.prevent_default() }
              on:keydown=move |e: web_sys::KeyboardEvent| {
                  if (e.key() == "Enter" || e.key() == " ") && !busy() {
                      e.prevent_default();
                      if let Some(i) = file_input.get_untracked() { i.click(); }
                  }
              }>
              <svg class="mb-3 h-10 w-10 text-slate-400" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M12 16V4m0 0L8 8m4-4 4 4M4 16v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2"/></svg>
              <div class="font-medium">{move || file.with(|f| f.as_ref().map(|f| f.name()).unwrap_or_else(|| DROP_TITLE.into()))}</div>
              <div class="mt-1 text-sm text-slate-500 dark:text-slate-400">
                {move || file.with(|f| f.as_ref().map(|f| format!("{} · click to change", fmt(f.size()))).unwrap_or_else(|| DROP_SUB.into()))}
              </div>
              <input id="file" node_ref=file_input type="file" class="sr-only" disabled=busy
                accept="video/*,audio/*,.mkv,.mov,.mp4,.m4a,.mp3,.wav,.flac,.webm,.ts,.avi"
                on:change=move |e| {
                    let input: web_sys::HtmlInputElement = event_target(&e);
                    set_file(input.files().and_then(|l| l.get(0)));
                    input.set_value("");
                }/>
            </label>
            <Show when=move || file_err.get().is_some()>
              <p class="mt-2 text-sm text-red-600 dark:text-red-400">{move || file_err.get().unwrap_or_default()}</p>
            </Show>

            <div class="mt-4 grid gap-4 sm:grid-cols-2">
              <div>
                <label for="language" class="mb-1 block text-sm font-medium">"Spoken language"</label>
                <select id="language" disabled=busy
                  class="w-full rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm dark:border-slate-700 dark:bg-slate-950"
                  on:change=move |e| on_language(event_target_value(&e))>
                  {move || cfg.with(|c| c.as_ref().map(|c| c.languages.clone()).unwrap_or_default()).into_iter().map(|l| {
                      let code = l.code.clone();
                      let selected = move || language.get() == code;
                      let suffix = if l.code == "en" || l.code == "auto" { "" } else { " → English" };
                      view! { <option value=l.code.clone() prop:selected=selected>{format!("{}{suffix}", l.name)}</option> }
                  }).collect_view()}
                </select>
              </div>
              <div>
                <label for="model" class="mb-1 block text-sm font-medium">"Model"</label>
                <select id="model" disabled=busy
                  class="w-full rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm dark:border-slate-700 dark:bg-slate-950"
                  on:change=move |e| on_model(event_target_value(&e))>
                  {move || cfg.with(|c| c.as_ref().map(|c| c.models.clone()).unwrap_or_default()).into_iter().map(|m| {
                      let id = m.id.clone();
                      let selected = move || model.get() == id;
                      let english_only = m.multi.is_none();
                      let disabled = move || english_only && language.get() != "en";
                      view! { <option value=m.id.clone() prop:selected=selected disabled=disabled>{format!("{} — {}, {}", m.label, m.size, m.note)}</option> }
                  }).collect_view()}
                </select>
              </div>
            </div>
            <p class="mt-2 text-xs text-slate-500 dark:text-slate-400">{hint}</p>

            <div class="mt-5 flex flex-wrap gap-3">
              <button type="button" on:click=go disabled=move || file.with(|f| f.is_none()) || busy()
                class="rounded-lg bg-indigo-600 px-5 py-2 text-sm font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-40">"Generate subtitle"</button>
              <button type="button" on:click=clear disabled=clear_disabled
                class="rounded-lg border border-slate-300 px-5 py-2 text-sm font-medium hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-40 dark:border-slate-700 dark:hover:bg-slate-800">"Clear all"</button>
            </div>
          </section>

          // Status / result
          <section class="rounded-xl border border-slate-200 bg-white p-5 shadow-sm dark:border-slate-800 dark:bg-slate-900">
            <div class="mb-2 flex items-center justify-between text-sm">
              <span class="font-medium">{move || stage.get().0}</span>
              <span class="tabular-nums text-slate-500 dark:text-slate-400">{move || stage.get().1.map(|p| format!("{p:.0}%")).unwrap_or_default()}</span>
            </div>
            <div class="h-2 overflow-hidden rounded-full bg-slate-200 dark:bg-slate-800">
              <div class=bar_class style:width=move || format!("{}%", stage.get().1.unwrap_or(0.0))></div>
            </div>
            <Show when=move || err.get().is_some()>
              <p class="mt-3 text-sm text-red-600 dark:text-red-400">{move || err.get().unwrap_or_default()}</p>
            </Show>
            <Show when=done>
              <a href="/api/download" class="mt-4 inline-flex items-center gap-2 rounded-lg bg-emerald-600 px-4 py-2 text-sm font-medium text-white hover:bg-emerald-500">
                <svg class="h-4 w-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M12 4v12m0 0 4-4m-4 4-4-4M4 20h16"/></svg>
                <span>{move || server.with(|s| format!("Download {} ({})", s.output_name, fmt(s.output_size as f64)))}</span>
              </a>
            </Show>
          </section>

          // Log viewer
          <section class="rounded-xl border border-slate-200 bg-white shadow-sm dark:border-slate-800 dark:bg-slate-900">
            <div class="flex items-center justify-between border-b border-slate-200 px-4 py-2 dark:border-slate-800">
              <h2 class="text-sm font-medium">"Log "<span class=move || conn.get().1>{move || conn.get().0}</span></h2>
              <label class="flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400">
                <input type="checkbox" class="rounded" prop:checked=move || follow.get()
                  on:change=move |e| follow.set(event_target_checked(&e))/>" Auto-scroll"
              </label>
            </div>
            <div node_ref=log_ref class="h-72 overflow-y-auto bg-slate-50 p-3 font-mono text-xs leading-relaxed dark:bg-slate-950" role="log" aria-live="off">
              <For each=move || logs.get() key=|(id, _)| *id let:item>
                {
                    let (_, l) = item;
                    let color = match l.level.as_str() {
                        "ok" => "text-emerald-600 dark:text-emerald-400",
                        "warn" => "text-amber-600 dark:text-amber-400",
                        "error" => "text-red-600 dark:text-red-400",
                        _ => "",
                    };
                    view! { <div class=color><span class="mr-2 text-slate-400">{l.t}</span>{l.msg}</div> }
                }
              </For>
            </div>
          </section>
        </main>
      </div>
    }
}
