//! Android platform services: the biometric app lock, notifications, speech input, the clipboard,
//! opening URLs, the photo and folder pickers, and foreground tracking.
//!
//! The implementations live in `java/dev/opencodemobile/Helper.java`, which build.rs compiles to dex.
//! The dex is embedded here and loaded through an `InMemoryDexClassLoader`. Every call goes through
//! one string-typed entry point, `Helper.call(activity, cmd, arg)`, which keeps the JNI surface to a
//! single method; asynchronous results (authentication, speech, pickers) are polled.
//!
//! On other targets every call fails, which keeps the crate buildable on the host for unit tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};


/// Whether the activity is between `Start` and `Stop`. Written from the Android event loop.
static VISIBLE: AtomicBool = AtomicBool::new(true);

pub fn set_visible(visible: bool) {
    VISIBLE.store(visible, Ordering::Relaxed);
}

pub fn is_foreground() -> bool {
    VISIBLE.load(Ordering::Relaxed)
}

#[cfg(target_os = "android")]
mod java {
    use jni::objects::{JClass, JObject, JString, JValue};
    use jni::refs::Global;
    use jni::{jni_sig, jni_str, JavaVM};
    use slint::android::AndroidApp;
    use std::sync::OnceLock;

    const DEX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/classes.dex"));
    static APP: OnceLock<AndroidApp> = OnceLock::new();
    static HELPER: OnceLock<Global<JClass<'static>>> = OnceLock::new();

    pub fn init(app: AndroidApp) {
        let _ = APP.set(app);
    }

    /// Invokes `Helper.call`. JNI failures are folded into the `"error:…"` convention.
    pub fn call(cmd: &str, arg: &str) -> String {
        try_call(cmd, arg).unwrap_or_else(|e| format!("error:JNI: {e}"))
    }

    fn try_call(cmd: &str, arg: &str) -> Result<String, jni::errors::Error> {
        let app = APP.get().ok_or(jni::errors::Error::NullPtr("platform::init not called"))?;
        // Slint's backend creates the singleton at startup; `from_raw` covers being called first.
        let vm = JavaVM::singleton().unwrap_or_else(|_| unsafe { JavaVM::from_raw(app.vm_as_ptr() as *mut _) });
        vm.attach_current_thread(|env| {
            // SAFETY: `activity_as_ptr` is a global reference that lives as long as the process, and
            // `JObject` has no `Drop`, so wrapping it releases nothing.
            let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr() as jni::sys::jobject) };
            if HELPER.get().is_none() {
                let parent = env.call_method(&activity, jni_str!("getClassLoader"), jni_sig!("()Ljava/lang/ClassLoader;"), &[])?.l()?;
                // SAFETY: `DEX` is 'static and the class loader only reads from the buffer.
                let dex = unsafe { env.new_direct_byte_buffer(DEX.as_ptr() as *mut u8, DEX.len())? };
                let loader = env.new_object(
                    jni_str!("dalvik/system/InMemoryDexClassLoader"),
                    jni_sig!("(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V"),
                    &[JValue::from(&dex), JValue::from(&parent)],
                )?;
                let name = env.new_string("dev.opencodemobile.Helper")?;
                let class = env
                    .call_method(&loader, jni_str!("loadClass"), jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"), &[JValue::from(&name)])?
                    .l()?;
                let class = env.cast_local::<JClass>(class)?;
                let _ = HELPER.set(env.new_global_ref(class)?);
            }
            let (cmd, arg) = (env.new_string(cmd)?, env.new_string(arg)?);
            let out = env
                .call_static_method(
                    HELPER.get().unwrap(),
                    jni_str!("call"),
                    jni_sig!("(Landroid/app/Activity;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
                    &[JValue::from(&activity), JValue::from(&cmd), JValue::from(&arg)],
                )?
                .l()?;
            env.cast_local::<JString>(out)?.try_to_string(env)
        })
    }
}

#[cfg(target_os = "android")]
pub use java::init;

#[cfg(target_os = "android")]
fn call(cmd: &str, arg: &str) -> String {
    java::call(cmd, arg)
}

#[cfg(not(target_os = "android"))]
fn call(_cmd: &str, _arg: &str) -> String {
    "error:only available on Android".into()
}

/// Starts `cmd`, then polls `poll_cmd` until it stops returning `"pending"` or `timeout` elapses.
fn call_and_wait(cmd: &str, arg: &str, poll_cmd: &str, timeout: Duration) -> String {
    let started = call(cmd, arg);
    if started.starts_with("error:") {
        return started;
    }
    let deadline = Instant::now() + timeout;
    loop {
        let r = call(poll_cmd, "");
        if r != "pending" {
            return r;
        }
        if Instant::now() > deadline {
            return "error:timed out".into();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Re-dispatches window insets. Slint derives the keyboard height from them, and a keyboard that
/// closed while another activity was on top never delivers its final update.
pub fn refresh_insets() {
    call("refresh_insets", "");
}

/// Creates the notification channel. Call once at startup.
pub fn setup() {
    let r = call("init", "");
    if r.starts_with("error:") {
        eprintln!("platform init: {r}");
    }
}

// App lock

/// Whether the app lock can be used, and the description to show for it in Settings.
pub fn lock_status() -> (bool, String) {
    let r = call("lock_status", "");
    match r.split_once('|') {
        Some((ok, text)) => (ok == "1", text.to_string()),
        None => (false, r.trim_start_matches("error:").to_string()),
    }
}

/// Shows the biometric / device-credential prompt and blocks until it resolves. Call off the UI
/// thread.
pub fn verify(title: &str) -> bool {
    call_and_wait("auth_start", title, "auth_poll", Duration::from_secs(300)) == "ok"
}

// Notifications

/// Requests POST_NOTIFICATIONS on Android 13+. The system only shows the prompt while the app is in
/// the foreground.
pub fn request_notification_permission() {
    call("request_notifications", "");
}

pub fn notify(title: &str, body: &str) {
    let r = call("notify", &format!("{title}\n{body}"));
    if r.starts_with("error:") {
        eprintln!("notify: {r}");
    }
}

// Speech input

pub fn speech_available() -> bool {
    cfg!(target_os = "android")
}

/// An in-flight speech recognition session.
pub struct Dictation;

impl Dictation {
    /// Starts recognition. `done` runs on a background thread with the transcript or an error.
    pub fn start(done: impl FnOnce(Result<String, String>) + Send + 'static) -> Dictation {
        std::thread::spawn(move || {
            let r = call_and_wait("speech_start", "", "speech_poll", Duration::from_secs(120));
            done(match r.split_once(':') {
                Some(("ok", text)) => Ok(text.to_string()),
                Some((_, msg)) => Err(msg.to_string()),
                None => Err(r),
            });
        });
        Dictation
    }

    /// Ends recognition early; whatever was recognized so far is still delivered to `done`.
    pub fn stop(&self) {
        call("speech_stop", "");
    }
}

// URLs and clipboard

fn result(r: String) -> Result<String, String> {
    match r.strip_prefix("error:") {
        Some(e) => Err(e.to_string()),
        None => Ok(r),
    }
}

pub fn open_url(url: &str) -> Result<(), String> {
    result(call("open_url", url)).map(|_| ())
}

pub fn copy_text(text: String) -> Result<(), String> {
    result(call("copy", &text)).map(|_| ())
}

pub enum Clip {
    /// An image, already copied out of its content provider into the cache directory.
    ImageFile(PathBuf),
    Text(String),
    Empty,
}

/// Reads the primary clip. Blocks while an image is copied out, so call off the UI thread.
pub fn read_clipboard() -> Result<Clip, String> {
    let r = result(call("paste", ""))?;
    Ok(if let Some(p) = r.strip_prefix("image:") {
        Clip::ImageFile(PathBuf::from(p))
    } else if let Some(t) = r.strip_prefix("text:") {
        Clip::Text(t.to_string())
    } else {
        Clip::Empty
    })
}

// Pickers

/// Set while a system picker covers the activity, so that backgrounding doesn't engage the app lock.
static PICKING: AtomicBool = AtomicBool::new(false);

pub fn picking() -> bool {
    PICKING.load(Ordering::Relaxed)
}

/// Opens a picker and waits for its `"ok:<payload>"` result; the payload is empty on cancel.
async fn pick(cmd: &'static str) -> Result<String, String> {
    PICKING.store(true, Ordering::Relaxed);
    let r = tokio::task::spawn_blocking(move || call_and_wait(cmd, "", "pick_poll", Duration::from_secs(30 * 60)))
        .await
        .unwrap_or_else(|e| format!("error:{e}"));
    PICKING.store(false, Ordering::Relaxed);
    let r = result(r)?;
    Ok(r.strip_prefix("ok:").unwrap_or(&r).to_string())
}

/// Picks a folder with the document-tree picker and maps it back to a filesystem path, which the
/// agent's tools require. Needs "All files access". Returns `Ok(None)` on cancel.
pub async fn pick_folder(_start: &Path) -> Result<Option<PathBuf>, String> {
    let p = pick("pick_folder").await?;
    Ok((!p.is_empty()).then(|| PathBuf::from(p)))
}

/// Picks up to ten images with the system photo picker and returns cache copies of them.
pub async fn pick_images() -> Result<Vec<PathBuf>, String> {
    Ok(pick("pick_images").await?.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())
}
