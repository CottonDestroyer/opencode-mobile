//! OpenCode Mobile for Android: a Slint UI over an embedded opencode agent engine, so no server is
//! needed.
//!
//! The activity loads this crate as a `cdylib` and enters through `android_main`.

mod controller;
mod demo;
mod markdown;
mod platform;
mod prefs;
mod render;

slint::include_modules!();

/// Builds the window and runs the event loop until it closes.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn run(start_dir: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("opencode-engine").build()?;
    let _guard = rt.enter();
    let ui = AppWindow::new()?;
    controller::init(&ui, rt.handle().clone(), start_dir);
    ui.run()?;
    // Bound how long shutdown waits for in-flight agent turns and background tasks.
    rt.shutdown_timeout(std::time::Duration::from_millis(500));
    Ok(())
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    use slint::android::android_activity::{MainEvent, PollEvent};

    // Android provides no HOME and /data/local/tmp isn't writable by apps, so point the engine's
    // XDG paths and temp dir at app-private storage. New sessions default to `<files>/workspace`.
    let home = app.internal_data_path().unwrap_or_else(|| std::path::PathBuf::from("/data/local/tmp"));
    let tmp = home.join("tmp");
    let workspace = home.join("workspace");
    let _ = std::fs::create_dir_all(&tmp);
    let _ = std::fs::create_dir_all(&workspace);
    // SAFETY: none of our threads exist yet, and the JVM's threads don't read these variables.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("TMPDIR", &tmp);
    }
    platform::init(app.clone());
    // Visibility follows Start/Stop rather than Resume/Pause: the biometric prompt and permission
    // dialogs pause the activity without hiding it.
    slint::android::init_with_event_listener(app, |event| {
        if let PollEvent::Main(e) = event {
            match e {
                MainEvent::Start => controller::app_visible(true),
                MainEvent::Stop => controller::app_visible(false),
                _ => {}
            }
        }
    })
    .expect("Slint Android backend failed to start");
    if let Err(e) = run(Some(workspace)) {
        eprintln!("opencode-mobile: {e:#}");
    }
}
