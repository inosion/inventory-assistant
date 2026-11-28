//! Capture PWA. Two tabs:
//! - **Store**: pick a box once, then snap, drop, repeat.
//! - **Don't know**: snap first; the AI suggests a box from what the other boxes hold, you pick one and
//!   confirm once it's in there. Items wait in a "To store" list until then.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    time::Duration,
};

use gloo_net::http::{Request, Response};
use leptos::{prelude::*, task::spawn_local};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, CanvasRenderingContext2d, HtmlCanvasElement, HtmlInputElement, ImageBitmap};

/// Entries kept per log (Store captures; filed Don't-know captures). The server keeps the same number.
const KEEP: usize = 100;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Loc {
    pk: i64,
    name: String,
    #[serde(default)]
    items: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Sugg {
    location: i64,
    name: String,
    reason: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Job {
    id: String,
    status: String,
    name: Option<String>,
    part: Option<i64>,
    confidence: Option<String>,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    location_name: String,
    #[serde(default)]
    qty: f64,
    #[serde(default)]
    suggestions: Option<Vec<Sugg>>,
    #[serde(default)]
    filed: Option<String>,
    /// Set when the capture turned out to be a part already in InvenTree (stock was added to it).
    #[serde(default)]
    merged_into: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
enum Tab {
    Store,
    Sort,
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
    /// Taken in the Don't-know tab.
    sort: bool,
    state: ShotState,
}

impl Shot {
    fn render_key(&self) -> (u32, String, String) {
        (self.key, format!("{:?}", self.state), self.thumb.clone())
    }
    fn id(&self) -> Option<&str> {
        match &self.state {
            ShotState::Sent(id) => Some(id),
            _ => None,
        }
    }
}

/// A downscaled photo with its box (`None` = Don't-know capture) and qty.
type Pending = (Blob, Option<Loc>, u32);

thread_local! {
    /// Downscaled photos not yet accepted by the server, kept so a failed upload can be retried.
    static PENDING: RefCell<HashMap<u32, Pending>> = RefCell::new(HashMap::new());
    static NEXT_KEY: Cell<u32> = const { Cell::new(1) };
}

fn next_key() -> u32 {
    NEXT_KEY.with(|k| {
        let v = k.get();
        k.set(v + 1);
        v
    })
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

async fn upload(blob: &Blob, loc: Option<&Loc>, qty: u32) -> Result<String, ApiErr> {
    let fd = web_sys::FormData::new().map_err(|e| ApiErr::Other(js_err(e)))?;
    let add = |k: &str, v: &str| fd.append_with_str(k, v).map_err(|e| ApiErr::Other(js_err(e)));
    match loc {
        Some(loc) => {
            add("mode", "store")?;
            add("location", &loc.pk.to_string())?;
            add("location_name", &loc.name)?;
        }
        None => add("mode", "sort")?,
    }
    add("qty", &qty.to_string())?;
    fd.append_with_blob_and_filename("photo", blob, "photo.jpg").map_err(|e| ApiErr::Other(js_err(e)))?;
    let req = Request::post("api/capture").body(fd).map_err(|e| ApiErr::Other(e.to_string()))?;
    let job: Job = check(req.send().await).await?.json().await.map_err(|e| ApiErr::Other(e.to_string()))?;
    Ok(job.id)
}

/// App-wide state; every field is a signal, so this is `Copy` and can go into any closure.
#[derive(Clone, Copy)]
struct Ctx {
    locs: RwSignal<Vec<Loc>>,
    shots: RwSignal<Vec<Shot>>,
    jobs: RwSignal<HashMap<String, Job>>,
    auth: RwSignal<bool>,
    msg: RwSignal<String>,
    picker: RwSignal<bool>,
    /// The picker is choosing a box for this Don't-know capture (rather than the Store tab's box).
    target: RwSignal<Option<String>>,
    /// Box chosen per Don't-know capture, waiting for "stored" to be confirmed.
    picked: RwSignal<HashMap<String, Loc>>,
}

impl Ctx {
    fn job(&self, id: &str) -> Option<Job> {
        self.jobs.with(|m| m.get(id).cloned())
    }
}

fn is_filed(jobs: &HashMap<String, Job>, s: &Shot) -> bool {
    s.id().and_then(|id| jobs.get(id)).is_some_and(|j| j.filed.is_some())
}

/// Caps the Store log and the Stored log at `KEEP`; the To-store list is never cut.
fn trim(shots: &mut Vec<Shot>, jobs: &HashMap<String, Job>) {
    let (mut stores, mut filed) = (0, 0);
    shots.retain(|s| {
        if !s.sort {
            stores += 1;
            stores <= KEEP
        } else if is_filed(jobs, s) {
            filed += 1;
            filed <= KEEP
        } else {
            true
        }
    });
}

/// Uploads a pending shot and records the outcome on it.
fn send_shot(c: Ctx, key: u32) {
    let Some((blob, loc, qty)) = PENDING.with(|p| p.borrow().get(&key).cloned()) else { return };
    set_state(c, key, ShotState::Uploading);
    spawn_local(async move {
        let st = match upload(&blob, loc.as_ref(), qty).await {
            Ok(id) => {
                PENDING.with(|p| p.borrow_mut().remove(&key));
                ShotState::Sent(id)
            }
            Err(ApiErr::Auth) => {
                c.auth.set(true);
                ShotState::Failed("not logged in".into())
            }
            Err(e) => ShotState::Failed(e.to_string()),
        };
        set_state(c, key, st);
    });
}

fn set_state(c: Ctx, key: u32, st: ShotState) {
    c.shots.update(|v| {
        if let Some(s) = v.iter_mut().find(|s| s.key == key) {
            s.state = st;
        }
    });
}

fn refresh_locs(c: Ctx) {
    spawn_local(async move {
        match get_json::<Vec<Loc>>("api/locations").await {
            Ok(v) => {
                c.auth.set(false);
                c.locs.set(v)
            }
            Err(ApiErr::Auth) => c.auth.set(true),
            Err(e) => c.msg.set(format!("Couldn't load boxes: {e}")),
        }
    })
}

/// Fetches server-side progress. With `seed`, also adds the server's log to the lists (after a reload the
/// in-memory list is empty), using the server's thumbnails.
fn load_recent(c: Ctx, seed: bool) {
    spawn_local(async move {
        match get_json::<Vec<Job>>("api/recent").await {
            Ok(v) => {
                c.jobs.update(|m| m.extend(v.iter().map(|j| (j.id.clone(), j.clone()))));
                if seed {
                    let jobs = c.jobs.get_untracked();
                    c.shots.update(|shots| {
                        let have: HashSet<String> = shots.iter().filter_map(|s| s.id().map(String::from)).collect();
                        for j in v.into_iter().filter(|j| !have.contains(&j.id)) {
                            shots.push(Shot {
                                key: next_key(),
                                thumb: format!("api/thumb/{}", j.id),
                                loc: j.location_name.clone(),
                                qty: j.qty.max(1.0) as u32,
                                sort: j.mode == "sort",
                                state: ShotState::Sent(j.id),
                            });
                        }
                        trim(shots, &jobs);
                    });
                }
            }
            Err(ApiErr::Auth) => c.auth.set(true),
            Err(_) => {}
        }
    });
}

/// Moves a Don't-know capture from the Inbox into the chosen box.
fn file_job(c: Ctx, id: String, l: Loc) {
    c.msg.set(format!("Moving to {}…", l.name));
    spawn_local(async move {
        let body = serde_json::json!({ "location": l.pk, "location_name": l.name });
        match post_json::<Job>(&format!("api/jobs/{id}/file"), &body).await {
            Ok(j) => {
                c.msg.set(String::new());
                c.jobs.update(|m| {
                    m.insert(j.id.clone(), j);
                });
                c.picked.update(|p| {
                    p.remove(&id);
                });
                remember_box(&l.name);
                let jobs = c.jobs.get_untracked();
                c.shots.update(|v| trim(v, &jobs));
                refresh_locs(c);
            }
            Err(ApiErr::Auth) => c.auth.set(true),
            Err(e) => c.msg.set(format!("Couldn't move it: {e}")),
        }
    });
}

fn open_picker(c: Ctx, target: Option<String>) {
    c.target.set(target);
    c.picker.set(true);
    refresh_locs(c);
}

// ---------- UI ----------

/// One entry of a capture log. Don't-know captures still in the Inbox also get the box suggestions.
fn row(c: Ctx, s: Shot) -> impl IntoView {
    let key = s.key;
    let failed = matches!(s.state, ShotState::Failed(_));
    let id = StoredValue::new(s.id().map(String::from));
    let job = move || id.with_value(|id| id.as_deref().and_then(|i| c.job(i)));
    let sort = s.sort;
    let st = s.state.clone();
    let title = move || match &st {
        ShotState::Preparing => "Preparing…".to_string(),
        ShotState::Uploading => "Uploading…".to_string(),
        ShotState::Failed(e) => format!("Failed: {e} (tap to retry)"),
        ShotState::Sent(_) => match job() {
            None => "Saved, queued".into(),
            Some(j) => match j.status.as_str() {
                "queued" => "Saved, queued".into(),
                "stored" => "In InvenTree, identifying…".into(),
                "research" => format!("Searching the web… ({})", j.name.unwrap_or_default()),
                "named" => j.name.unwrap_or_default(),
                _ if sort => "Couldn't identify".into(),
                _ => "In Inbox (couldn't identify)".into(),
            },
        },
    };
    let (loc0, qty) = (s.loc.clone(), s.qty);
    let sub = move || {
        let j = job();
        let loc = j.as_ref().map(|j| j.location_name.clone()).filter(|l| !l.is_empty()).unwrap_or_else(|| loc0.clone());
        let mut parts = Vec::new();
        match (sort, j.as_ref().is_some_and(|j| j.filed.is_some())) {
            (false, _) => parts.push(loc),
            (true, true) => parts.push(format!("→ {loc}")),
            (true, false) => {}
        }
        parts.push(format!("qty {qty}"));
        if let Some(c) = j.as_ref().and_then(|j| j.confidence.clone()) {
            parts.push(c);
        }
        if j.as_ref().is_some_and(|j| j.merged_into.is_some()) {
            parts.push(format!("already had it: stock +{qty}"));
        }
        parts.join(" · ")
    };
    let link = move || job().and_then(|j| j.part).map(|p| format!("/web/part/{p}/details"));

    let suggest = move || {
        let j = job().filter(|j| sort && j.filed.is_none())?;
        let jid = j.id.clone();
        let chosen = c.picked.with(|p| p.get(&jid).cloned());
        let finding = j.suggestions.is_none() && j.status != "unnamed";
        let chips = j.suggestions.clone().unwrap_or_default();
        let none = !finding && chips.is_empty();
        let chips = chips
            .into_iter()
            .map(|g| {
                let cur = chosen.as_ref().is_some_and(|l| l.pk == g.location);
                let l = Loc { pk: g.location, name: g.name.clone(), items: 0 };
                let jid = jid.clone();
                view! {
                    <button class="tag sugg" class:cur=cur
                        on:click=move |_| c.picked.update(|p| { p.insert(jid.clone(), l.clone()); })>
                        <b>{g.name}</b> <small>{g.reason}</small>
                    </button>
                }
            })
            .collect_view();
        let other_for = jid.clone();
        let confirm = chosen.map(|l| {
            let label = format!("✓ It's in {}", l.name);
            let jid = jid.clone();
            view! { <button class="confirm" on:click=move |_| file_job(c, jid.clone(), l.clone())>{label}</button> }
        });
        Some(view! {
            <div class="suggest">
                {finding.then(|| view! { <span class="hint">"Finding a box…"</span> })}
                {none.then(|| view! { <span class="hint">"No suggestion, pick a box:"</span> })}
                {chips}
                <button class="tag other" on:click=move |_| open_picker(c, Some(other_for.clone()))>"Other box…"</button>
                {confirm}
            </div>
        })
    };

    view! {
        <li class:failed=failed on:click=move |_| if failed { send_shot(c, key) }>
            <div class="main">
                <img src=s.thumb.clone() alt=""
                    on:error=|ev| { let _ = event_target::<web_sys::Element>(&ev).remove_attribute("src"); } />
                <div class="txt">
                    <div class="t">{title}</div>
                    <div class="s">
                        {sub}
                        {move || link().map(|h| view! { " · " <a href=h target="_blank">"open"</a> })}
                    </div>
                </div>
            </div>
            {suggest}
        </li>
    }
}

#[component]
fn App() -> impl IntoView {
    let c = Ctx {
        locs: RwSignal::new(vec![]),
        shots: RwSignal::new(vec![]),
        jobs: RwSignal::new(HashMap::new()),
        auth: RwSignal::new(false),
        msg: RwSignal::new(String::new()),
        picker: RwSignal::new(false),
        target: RwSignal::new(None),
        picked: RwSignal::new(HashMap::new()),
    };
    let (tab, set_tab) = signal(load::<Tab>("qc.tab").unwrap_or(Tab::Store));
    let (loc, set_loc) = signal(load::<Loc>("qc.loc"));
    let (filter, set_filter) = signal(String::new());
    let (qty, set_qty) = signal(1u32);
    if tab.get_untracked() == Tab::Store && loc.get_untracked().is_none() {
        c.picker.set(true);
    }

    refresh_locs(c);
    load_recent(c, true);

    let switch = move |t: Tab| {
        store("qc.tab", &t);
        set_tab.set(t);
    };

    let close_picker = move || {
        c.picker.set(false);
        c.target.set(None);
        set_filter.set(String::new());
    };

    let choose = move |l: Loc| {
        match c.target.get_untracked() {
            Some(id) => c.picked.update(|p| {
                p.insert(id, l);
            }),
            None => {
                store("qc.loc", &l);
                remember_box(&l.name);
                set_loc.set(Some(l));
            }
        }
        close_picker();
    };

    let create_box = move |name: String| {
        let name = name.trim().to_uppercase();
        if name.is_empty() {
            return;
        }
        c.msg.set(format!("Creating box {name}…"));
        spawn_local(async move {
            match post_json::<Loc>("api/locations", &serde_json::json!({ "name": name })).await {
                Ok(l) => {
                    c.msg.set(String::new());
                    choose(l);
                    refresh_locs(c);
                }
                Err(ApiErr::Auth) => c.auth.set(true),
                Err(e) => c.msg.set(format!("Couldn't create box: {e}")),
            }
        });
    };

    // Boxes for the tag cloud: recently used first, then the rest; filtered by the typed text.
    let cloud = Memo::new(move |_| {
        let f = filter.get().trim().to_lowercase();
        let recent: Vec<String> = load("qc.recent").unwrap_or_default();
        let mut v: Vec<Loc> = c.locs.get().into_iter().filter(|l| f.is_empty() || l.name.to_lowercase().contains(&f)).collect();
        v.sort_by_key(|l| (recent.iter().position(|r| r == &l.name).unwrap_or(usize::MAX), l.name.to_lowercase()));
        v
    });
    let exact = Memo::new(move |_| {
        let f = filter.get().trim().to_lowercase();
        !f.is_empty() && c.locs.get().iter().any(|l| l.name.to_lowercase() == f)
    });

    let on_enter = move |ev: web_sys::KeyboardEvent| {
        if ev.key() != "Enter" {
            return;
        }
        let f = filter.get_untracked();
        let cl = cloud.get_untracked();
        if let Some(l) = cl.iter().find(|l| l.name.eq_ignore_ascii_case(f.trim())) {
            choose(l.clone());
        } else if cl.len() == 1 && !f.trim().is_empty() {
            choose(cl[0].clone());
        } else {
            create_box(f);
        }
    };

    let on_photo = move |ev: web_sys::Event| {
        let input: HtmlInputElement = event_target(&ev);
        let Some(files) = input.files() else { return };
        let sort = tab.get_untracked() == Tab::Sort;
        let l = if sort {
            None
        } else {
            let Some(l) = loc.get_untracked() else {
                open_picker(c, None);
                return;
            };
            Some(l)
        };
        let q = qty.get_untracked();
        for i in 0..files.length() {
            let Some(file) = files.get(i) else { continue };
            let key = next_key();
            let jobs = c.jobs.get_untracked();
            c.shots.update(|v| {
                let loc = l.as_ref().map(|l| l.name.clone()).unwrap_or_default();
                v.insert(0, Shot { key, thumb: String::new(), loc, qty: q, sort, state: ShotState::Preparing });
                trim(v, &jobs);
            });
            let l = l.clone();
            spawn_local(async move {
                let blob: Blob = file.into();
                if let Ok(t) = downscale(&blob, 160.0, 0.7).await {
                    let url = web_sys::Url::create_object_url_with_blob(&t).unwrap_or_default();
                    c.shots.update(|v| {
                        if let Some(s) = v.iter_mut().find(|s| s.key == key) {
                            s.thumb = url;
                        }
                    });
                }
                match downscale(&blob, 1600.0, 0.85).await {
                    Ok(small) => {
                        PENDING.with(|p| p.borrow_mut().insert(key, (small, l, q)));
                        send_shot(c, key);
                    }
                    Err(e) => set_state(c, key, ShotState::Failed(format!("couldn't read photo: {e}"))),
                }
            });
        }
        input.set_value("");
        set_qty.set(1);
    };

    // Poll server-side progress while anything is still being stored / identified / given suggestions.
    set_interval(
        move || {
            let waiting = c.shots.with_untracked(|v| {
                c.jobs.with_untracked(|m| {
                    v.iter().filter_map(|s| s.id().map(|id| (s, m.get(id)))).any(|(s, j)| match j {
                        None => true,
                        Some(j) => {
                            !matches!(j.status.as_str(), "named" | "unnamed") || (s.sort && j.filed.is_none() && j.suggestions.is_none())
                        }
                    })
                })
            });
            if waiting {
                load_recent(c, false);
            }
        },
        Duration::from_secs(4),
    );

    let store_list = move || c.shots.get().into_iter().filter(|s| !s.sort).take(KEEP).collect::<Vec<_>>();
    let inbox_list = move || {
        let jobs = c.jobs.get();
        c.shots.get().into_iter().filter(|s| s.sort && !is_filed(&jobs, s)).collect::<Vec<_>>()
    };
    let filed_list = move || {
        let jobs = c.jobs.get();
        let mut v: Vec<Shot> = c.shots.get().into_iter().filter(|s| s.sort && is_filed(&jobs, s)).collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.id().and_then(|id| jobs.get(id)).and_then(|j| j.filed.clone())));
        v.truncate(KEEP);
        v
    };

    let counts = move || {
        let sort = tab.get() == Tab::Sort;
        let v: Vec<Shot> = c.shots.get().into_iter().filter(|s| s.sort == sort).collect();
        let j = c.jobs.get();
        let busy = v.iter().filter(|s| matches!(s.state, ShotState::Preparing | ShotState::Uploading)).count();
        let failed = v.iter().filter(|s| matches!(s.state, ShotState::Failed(_))).count();
        let mut parts = Vec::new();
        if sort {
            let filed = v.iter().filter(|s| is_filed(&j, s)).count();
            let waiting = v.iter().filter(|s| s.id().is_some() && !is_filed(&j, s)).count();
            parts.push(format!("{waiting} to store"));
            parts.push(format!("{filed} stored"));
        } else {
            let sent = v.iter().filter(|s| s.id().is_some()).count();
            let named = v.iter().filter(|s| s.id().and_then(|id| j.get(id)).is_some_and(|x| x.status == "named")).count();
            parts.push(format!("{sent} saved"));
            parts.push(format!("{named} identified"));
        }
        if busy > 0 {
            parts.push(format!("{busy} uploading"));
        }
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        parts.join(" · ")
    };

    view! {
        <main class="app">
            <Show when=move || c.auth.get()>
                <div class="banner">
                    "Not logged in. " <a href="/web/" target="_blank">"Log in to InvenTree"</a>
                    ", then " <a href="" on:click=move |ev| { ev.prevent_default(); c.auth.set(false); refresh_locs(c); load_recent(c, true); }>"retry"</a>"."
                </div>
            </Show>
            <Show when=move || !c.msg.get().is_empty()>
                <div class="banner info" on:click=move |_| c.msg.set(String::new())>{move || c.msg.get()}</div>
            </Show>

            <nav class="tabs">
                <button class:on=move || tab.get() == Tab::Store on:click=move |_| switch(Tab::Store)>"Store"</button>
                <button class:on=move || tab.get() == Tab::Sort on:click=move |_| switch(Tab::Sort)>
                    "Don't know"
                    {move || {
                        let n = inbox_list().iter().filter(|s| s.id().is_some()).count();
                        (n > 0).then(|| view! { <span class="badge">{n}</span> })
                    }}
                </button>
            </nav>

            <Show when=move || tab.get() == Tab::Store fallback=|| view! {
                <div class="box hintbox">"Not sure where it goes? Snap it, the AI suggests a box."</div>
            }>
                <button class="box" on:click=move |_| open_picker(c, None)>
                    <span class="lbl">"Box"</span>
                    <span class="boxname">{move || loc.get().map(|l| l.name).unwrap_or_else(|| "tap to pick".into())}</span>
                    <span class="chg">"change"</span>
                </button>
            </Show>

            <label class="shutter" class:disabled=move || tab.get() == Tab::Store && loc.get().is_none()>
                <input type="file" accept="image/*" capture="environment" on:change=on_photo />
                <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M9 3 7.2 5H4a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2h-3.2L15 3H9zm3 5a5 5 0 1 1 0 10 5 5 0 0 1 0-10zm0 2a3 3 0 1 0 0 6 3 3 0 0 0 0-6z"/></svg>
                <span>{move || if tab.get() == Tab::Sort { "Snap & suggest" } else { "Snap" }}</span>
            </label>

            <div class="qty">
                <button on:click=move |_| set_qty.update(|q| if *q > 1 { *q -= 1 })>"−"</button>
                <span>{move || format!("qty {}", qty.get())}</span>
                <button on:click=move |_| set_qty.update(|q| *q += 1)>"+"</button>
                <button class="x10" on:click=move |_| set_qty.update(|q| *q = if *q == 1 { 10 } else { *q + 10 })>"+10"</button>
            </div>

            <div class="stats">{counts}</div>

            <Show when=move || tab.get() == Tab::Store fallback=move || view! {
                <h3>"To store"</h3>
                <ul class="shots">
                    <For each=inbox_list key=|s| s.render_key() let:s>{row(c, s)}</For>
                </ul>
                <Show when=move || !filed_list().is_empty()>
                    <h3>"Stored"</h3>
                    <ul class="shots">
                        <For each=filed_list key=|s| s.render_key() let:s>{row(c, s)}</For>
                    </ul>
                </Show>
            }>
                <ul class="shots">
                    <For each=store_list key=|s| s.render_key() let:s>{row(c, s)}</For>
                </ul>
            </Show>

            <Show when=move || c.picker.get()>
                <div class="picker">
                    <div class="sheet">
                        <Show when=move || c.target.get().is_some()>
                            <div class="for">"Which box is it going in?"</div>
                        </Show>
                        <div class="row">
                            <input
                                class="search" type="text" placeholder="Type a box, e.g. C3"
                                autocapitalize="characters" autocomplete="off" enterkeyhint="go"
                                prop:value=move || filter.get()
                                on:input=move |ev| set_filter.set(event_target_value(&ev))
                                on:keydown=on_enter
                            />
                            <Show when=move || loc.get().is_some() || c.target.get().is_some() || tab.get() == Tab::Sort>
                                <button class="close" on:click=move |_| close_picker()>"✕"</button>
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
