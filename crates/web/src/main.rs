//! Capture PWA: pick a box once, then snap, drop, repeat.

use std::{cell::RefCell, collections::HashMap, time::Duration};

use gloo_net::http::{Request, Response};
use leptos::{prelude::*, task::spawn_local};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, CanvasRenderingContext2d, HtmlCanvasElement, HtmlInputElement, ImageBitmap};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Loc {
    pk: i64,
    name: String,
    #[serde(default)]
    items: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Job {
    id: String,
    status: String,
    name: Option<String>,
    part: Option<i64>,
    confidence: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum ShotState {
    Preparing,
    Uploading,
    Sent(String),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
struct Shot {
    key: u32,
    thumb: String,
    loc: String,
    qty: u32,
    state: ShotState,
}

impl Shot {
    fn render_key(&self) -> (u32, String, String) {
        (self.key, format!("{:?}", self.state), self.thumb.clone())
    }
}

thread_local! {
    /// Downscaled photos not yet accepted by the server, kept so a failed upload can be retried.
    static PENDING: RefCell<HashMap<u32, (Blob, Loc, u32)>> = RefCell::new(HashMap::new());
}

// ---------- small helpers ----------

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}
fn load<T: DeserializeOwned>(k: &str) -> Option<T> {
    serde_json::from_str(&storage()?.get_item(k).ok()??).ok()
}
fn store<T: Serialize>(k: &str, v: &T) {
    if let (Some(s), Ok(j)) = (storage(), serde_json::to_string(v)) {
        let _ = s.set_item(k, &j);
    }
}
fn remember_box(name: &str) {
    let mut r: Vec<String> = load("qc.recent").unwrap_or_default();
    r.retain(|x| x != name);
    r.insert(0, name.to_string());
    r.truncate(12);
    store("qc.recent", &r);
}

#[derive(Debug, Clone)]
enum ApiErr {
    Auth,
    Other(String),
}
impl std::fmt::Display for ApiErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiErr::Auth => write!(f, "not logged in to InvenTree"),
            ApiErr::Other(s) => write!(f, "{s}"),
        }
    }
}

async fn check(r: Result<Response, gloo_net::Error>) -> Result<Response, ApiErr> {
    let r = r.map_err(|e| ApiErr::Other(e.to_string()))?;
    match r.status() {
        200..=299 => Ok(r),
        401 | 403 => Err(ApiErr::Auth),
        s => Err(ApiErr::Other(format!("{s}: {}", r.text().await.unwrap_or_default()))),
    }
}

async fn get_json<T: DeserializeOwned>(url: &str) -> Result<T, ApiErr> {
    check(Request::get(url).send().await).await?.json().await.map_err(|e| ApiErr::Other(e.to_string()))
}

async fn post_json<T: DeserializeOwned>(url: &str, body: &serde_json::Value) -> Result<T, ApiErr> {
    let req = Request::post(url).json(body).map_err(|e| ApiErr::Other(e.to_string()))?;
    check(req.send().await).await?.json().await.map_err(|e| ApiErr::Other(e.to_string()))
}

fn js_err(e: JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}

/// Decodes (honouring EXIF orientation) and re-encodes as a JPEG no larger than `max` px.
async fn downscale(src: &Blob, max: f64, quality: f64) -> Result<Blob, String> {
    let win = web_sys::window().ok_or("no window")?;
    let bmp: ImageBitmap =
        JsFuture::from(win.create_image_bitmap_with_blob(src).map_err(js_err)?).await.map_err(js_err)?.dyn_into().map_err(js_err)?;
    let (w, h) = (bmp.width() as f64, bmp.height() as f64);
    let s = (max / w.max(h)).min(1.0);
    let (cw, ch) = ((w * s).round(), (h * s).round());
    let canvas: HtmlCanvasElement =
        win.document().ok_or("no document")?.create_element("canvas").map_err(js_err)?.dyn_into().map_err(|_| "not a canvas")?;
    canvas.set_width(cw as u32);
    canvas.set_height(ch as u32);
    let ctx: CanvasRenderingContext2d =
        canvas.get_context("2d").map_err(js_err)?.ok_or("no 2d context")?.dyn_into().map_err(|_| "not a 2d context")?;
    ctx.draw_image_with_image_bitmap_and_dw_and_dh(&bmp, 0.0, 0.0, cw, ch).map_err(js_err)?;
    bmp.close();
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        let cb = Closure::once_into_js(move |b: JsValue| {
            let _ = resolve.call1(&JsValue::NULL, &b);
        });
        if let Err(e) = canvas.to_blob_with_type_and_encoder_options(cb.unchecked_ref(), "image/jpeg", &JsValue::from_f64(quality)) {
            let _ = reject.call1(&JsValue::NULL, &e);
        }
    });
    let b = JsFuture::from(promise).await.map_err(js_err)?;
    b.dyn_into::<Blob>().map_err(|_| "canvas produced no image".to_string())
}

async fn upload(blob: &Blob, loc: &Loc, qty: u32) -> Result<String, ApiErr> {
    let fd = web_sys::FormData::new().map_err(|e| ApiErr::Other(js_err(e)))?;
    let add = |k: &str, v: &str| fd.append_with_str(k, v).map_err(|e| ApiErr::Other(js_err(e)));
    add("location", &loc.pk.to_string())?;
    add("location_name", &loc.name)?;
    add("qty", &qty.to_string())?;
    fd.append_with_blob_and_filename("photo", blob, "photo.jpg").map_err(|e| ApiErr::Other(js_err(e)))?;
    let req = Request::post("api/capture").body(fd).map_err(|e| ApiErr::Other(e.to_string()))?;
    let job: Job = check(req.send().await).await?.json().await.map_err(|e| ApiErr::Other(e.to_string()))?;
    Ok(job.id)
}

/// Uploads a pending shot and records the outcome on it.
fn send_shot(key: u32, set_shots: WriteSignal<Vec<Shot>>, set_auth: WriteSignal<bool>) {
    let Some((blob, loc, qty)) = PENDING.with(|p| p.borrow().get(&key).cloned()) else { return };
    set_state(set_shots, key, ShotState::Uploading);
    spawn_local(async move {
        let st = match upload(&blob, &loc, qty).await {
            Ok(id) => {
                PENDING.with(|p| p.borrow_mut().remove(&key));
                ShotState::Sent(id)
            }
            Err(ApiErr::Auth) => {
                set_auth.set(true);
                ShotState::Failed("not logged in".into())
            }
            Err(e) => ShotState::Failed(e.to_string()),
        };
        set_state(set_shots, key, st);
    });
}

fn set_state(set_shots: WriteSignal<Vec<Shot>>, key: u32, st: ShotState) {
    set_shots.update(|v| {
        if let Some(s) = v.iter_mut().find(|s| s.key == key) {
            s.state = st;
        }
    });
}

// ---------- UI ----------

#[component]
fn App() -> impl IntoView {
    let (loc, set_loc) = signal(load::<Loc>("qc.loc"));
    let (locs, set_locs) = signal(Vec::<Loc>::new());
    let (picker, set_picker) = signal(loc.get_untracked().is_none());
    let (filter, set_filter) = signal(String::new());
    let (qty, set_qty) = signal(1u32);
    let (shots, set_shots) = signal(Vec::<Shot>::new());
    let (jobs, set_jobs) = signal(HashMap::<String, Job>::new());
    let (auth, set_auth) = signal(false);
    let (msg, set_msg) = signal(String::new());
    let next_key = StoredValue::new(1u32);

    let refresh_locs = move || {
        spawn_local(async move {
            match get_json::<Vec<Loc>>("api/locations").await {
                Ok(v) => {
                    set_auth.set(false);
                    set_locs.set(v)
                }
                Err(ApiErr::Auth) => set_auth.set(true),
                Err(e) => set_msg.set(format!("Couldn't load boxes: {e}")),
            }
        })
    };
    refresh_locs();

    let choose = move |l: Loc| {
        store("qc.loc", &l);
        remember_box(&l.name);
        set_loc.set(Some(l));
        set_picker.set(false);
        set_filter.set(String::new());
    };

    let create_box = move |name: String| {
        let name = name.trim().to_uppercase();
        if name.is_empty() {
            return;
        }
        set_msg.set(format!("Creating box {name}…"));
        spawn_local(async move {
            match post_json::<Loc>("api/locations", &serde_json::json!({ "name": name })).await {
                Ok(l) => {
                    set_msg.set(String::new());
                    choose(l);
                    refresh_locs();
                }
                Err(ApiErr::Auth) => set_auth.set(true),
                Err(e) => set_msg.set(format!("Couldn't create box: {e}")),
            }
        });
    };

    // Boxes for the tag cloud: recently used first, then the rest; filtered by the typed text.
    let cloud = Memo::new(move |_| {
        let f = filter.get().trim().to_lowercase();
        let recent: Vec<String> = load("qc.recent").unwrap_or_default();
        let mut v: Vec<Loc> = locs.get().into_iter().filter(|l| f.is_empty() || l.name.to_lowercase().contains(&f)).collect();
        v.sort_by_key(|l| (recent.iter().position(|r| r == &l.name).unwrap_or(usize::MAX), l.name.to_lowercase()));
        v
    });
    let exact = Memo::new(move |_| {
        let f = filter.get().trim().to_lowercase();
        !f.is_empty() && locs.get().iter().any(|l| l.name.to_lowercase() == f)
    });

    let on_enter = move |ev: web_sys::KeyboardEvent| {
        if ev.key() != "Enter" {
            return;
        }
        let f = filter.get_untracked();
        let c = cloud.get_untracked();
        if let Some(l) = c.iter().find(|l| l.name.eq_ignore_ascii_case(f.trim())) {
            choose(l.clone());
        } else if c.len() == 1 && !f.trim().is_empty() {
            choose(c[0].clone());
        } else {
            create_box(f);
        }
    };

    let on_photo = move |ev: web_sys::Event| {
        let input: HtmlInputElement = event_target(&ev);
        let Some(files) = input.files() else { return };
        let Some(l) = loc.get_untracked() else {
            set_picker.set(true);
            return;
        };
        let q = qty.get_untracked();
        for i in 0..files.length() {
            let Some(file) = files.get(i) else { continue };
            let key = next_key.get_value();
            next_key.set_value(key + 1);
            set_shots.update(|v| {
                v.insert(0, Shot { key, thumb: String::new(), loc: l.name.clone(), qty: q, state: ShotState::Preparing });
                v.truncate(60);
            });
            let l = l.clone();
            spawn_local(async move {
                let blob: Blob = file.into();
                if let Ok(t) = downscale(&blob, 160.0, 0.7).await {
                    let url = web_sys::Url::create_object_url_with_blob(&t).unwrap_or_default();
                    set_shots.update(|v| {
                        if let Some(s) = v.iter_mut().find(|s| s.key == key) {
                            s.thumb = url;
                        }
                    });
                }
                match downscale(&blob, 1600.0, 0.85).await {
                    Ok(small) => {
                        PENDING.with(|p| p.borrow_mut().insert(key, (small, l, q)));
                        send_shot(key, set_shots, set_auth);
                    }
                    Err(e) => set_state(set_shots, key, ShotState::Failed(format!("couldn't read photo: {e}"))),
                }
            });
        }
        input.set_value("");
        set_qty.set(1);
    };

    // Poll server-side progress while anything is still being stored / identified.
    set_interval(
        move || {
            let waiting = shots.with_untracked(|v| {
                jobs.with_untracked(|j| {
                    v.iter().any(|s| match &s.state {
                        ShotState::Sent(id) => !matches!(j.get(id).map(|x| x.status.as_str()), Some("named" | "unnamed")),
                        _ => false,
                    })
                })
            });
            if waiting {
                spawn_local(async move {
                    if let Ok(v) = get_json::<Vec<Job>>("api/recent").await {
                        set_jobs.update(|m| m.extend(v.into_iter().map(|j| (j.id.clone(), j))));
                    }
                });
            }
        },
        Duration::from_secs(4),
    );

    let counts = move || {
        let v = shots.get();
        let j = jobs.get();
        let sent = v.iter().filter(|s| matches!(s.state, ShotState::Sent(_))).count();
        let busy = v.iter().filter(|s| matches!(s.state, ShotState::Preparing | ShotState::Uploading)).count();
        let failed = v.iter().filter(|s| matches!(s.state, ShotState::Failed(_))).count();
        let named = v.iter().filter(|s| matches!(&s.state, ShotState::Sent(id) if j.get(id).is_some_and(|x| x.status == "named"))).count();
        let mut parts = vec![format!("{sent} saved")];
        if busy > 0 {
            parts.push(format!("{busy} uploading"));
        }
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        parts.push(format!("{named} identified"));
        parts.join(" · ")
    };

    view! {
        <main class="app">
            <Show when=move || auth.get()>
                <div class="banner">
                    "Not logged in. " <a href="/web/" target="_blank">"Log in to InvenTree"</a>
                    ", then " <a href="" on:click=move |ev| { ev.prevent_default(); set_auth.set(false); refresh_locs(); }>"retry"</a>"."
                </div>
            </Show>
            <Show when=move || !msg.get().is_empty()>
                <div class="banner info" on:click=move |_| set_msg.set(String::new())>{move || msg.get()}</div>
            </Show>

            <button class="box" on:click=move |_| { set_picker.set(true); refresh_locs(); }>
                <span class="lbl">"Box"</span>
                <span class="boxname">{move || loc.get().map(|l| l.name).unwrap_or_else(|| "tap to pick".into())}</span>
                <span class="chg">"change"</span>
            </button>

            <label class="shutter" class:disabled=move || loc.get().is_none()>
                <input type="file" accept="image/*" capture="environment" on:change=on_photo />
                <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M9 3 7.2 5H4a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2h-3.2L15 3H9zm3 5a5 5 0 1 1 0 10 5 5 0 0 1 0-10zm0 2a3 3 0 1 0 0 6 3 3 0 0 0 0-6z"/></svg>
                <span>"Snap"</span>
            </label>

            <div class="qty">
                <button on:click=move |_| set_qty.update(|q| if *q > 1 { *q -= 1 })>"−"</button>
                <span>{move || format!("qty {}", qty.get())}</span>
                <button on:click=move |_| set_qty.update(|q| *q += 1)>"+"</button>
                <button class="x10" on:click=move |_| set_qty.update(|q| *q = if *q == 1 { 10 } else { *q + 10 })>"+10"</button>
            </div>

            <div class="stats">{counts}</div>

            <ul class="shots">
                <For each=move || shots.get() key=|s| s.render_key() let:s>
                    {
                        let key = s.key;
                        let failed = matches!(s.state, ShotState::Failed(_));
                        let st = s.state.clone();
                        let title = move || match &st {
                            ShotState::Preparing => "Preparing…".to_string(),
                            ShotState::Uploading => "Uploading…".to_string(),
                            ShotState::Failed(e) => format!("Failed: {e} (tap to retry)"),
                            ShotState::Sent(id) => match jobs.get().get(id) {
                                None => "Saved, queued".into(),
                                Some(j) => match j.status.as_str() {
                                    "queued" => "Saved, queued".into(),
                                    "stored" => "In InvenTree, identifying…".into(),
                                    "research" => format!("Searching the web… ({})", j.name.clone().unwrap_or_default()),
                                    "named" => j.name.clone().unwrap_or_default(),
                                    _ => "In Inbox (couldn't identify)".into(),
                                },
                            },
                        };
                        let st2 = s.state.clone();
                        let link = move || match &st2 {
                            ShotState::Sent(id) => jobs.get().get(id).and_then(|j| j.part).map(|p| format!("/web/part/{p}/details")),
                            _ => None,
                        };
                        let conf = {
                            let st3 = s.state.clone();
                            move || match &st3 {
                                ShotState::Sent(id) => jobs.get().get(id).and_then(|j| j.confidence.clone()).map(|c| format!(" · {c}")).unwrap_or_default(),
                                _ => String::new(),
                            }
                        };
                        view! {
                            <li class:failed=failed on:click=move |_| if failed { send_shot(key, set_shots, set_auth) }>
                                <img src=s.thumb.clone() alt="" />
                                <div class="txt">
                                    <div class="t">{title}</div>
                                    <div class="s">
                                        {format!("{} · qty {}", s.loc, s.qty)}{conf}
                                        {move || link().map(|h| view! { " · " <a href=h target="_blank">"open"</a> })}
                                    </div>
                                </div>
                            </li>
                        }
                    }
                </For>
            </ul>

            <Show when=move || picker.get()>
                <div class="picker">
                    <div class="sheet">
                        <div class="row">
                            <input
                                class="search" type="text" placeholder="Type a box, e.g. C3"
                                autocapitalize="characters" autocomplete="off" enterkeyhint="go"
                                prop:value=move || filter.get()
                                on:input=move |ev| set_filter.set(event_target_value(&ev))
                                on:keydown=on_enter
                            />
                            <Show when=move || loc.get().is_some()>
                                <button class="close" on:click=move |_| set_picker.set(false)>"✕"</button>
                            </Show>
                        </div>
                        <Show when=move || !filter.get().trim().is_empty() && !exact.get()>
                            <button class="create" on:click=move |_| create_box(filter.get_untracked())>
                                {move || format!("+ New box “{}”", filter.get().trim().to_uppercase())}
                            </button>
                        </Show>
                        <div class="cloud">
                            <For each=move || cloud.get() key=|l| (l.pk, l.items) let:l>
                                {
                                    let l2 = l.clone();
                                    let cur = loc.get_untracked().is_some_and(|c| c.pk == l.pk);
                                    view! {
                                        <button class="tag" class:cur=cur on:click=move |_| choose(l2.clone())>
                                            {l.name.clone()} <small>{l.items}</small>
                                        </button>
                                    }
                                }
                            </For>
                        </div>
                    </div>
                </div>
            </Show>
        </main>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}
