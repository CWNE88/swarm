//! swarm on Android phones: the library the app loads (see
//! tools/build-android.sh). Android starts the activity, which calls
//! `android_main` on a thread of its own; that runs the window (Slint, in
//! the activity) and its controller, as the desktop's does.
//!
//! The app's service (`java/com/cwne88/swarm/SwarmService.java`) keeps the
//! process in the foreground, with its notification, so swarm stays
//! connected with the screen off or another app open. The activity going
//! (swiped away) is swarm quitting: its connections are closed properly,
//! then the process ends, service and all.

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: slint::android::AndroidApp) {
    log_to_logcat();
    ACTIVITY.store(app.activity_as_ptr(), std::sync::atomic::Ordering::Relaxed);
    swarm_desktop::set_notifier(|notice| {
        if let Err(e) = notify(notice) {
            eprintln!("swarm: a notification: {e}");
        }
    });
    swarm_desktop::set_call_hook(|call| {
        use jni::objects::JValue;
        swarm_core::node::set_call_underway(call.underway);
        let flags = [JValue::Bool(call.underway.into()), JValue::Bool(call.camera.into()), JValue::Bool(call.video.into())];
        if let Err(e) = call_activity(|env, activity| env.call_method(activity, "callUnderway", "(ZZZ)V", &flags).map(|_| ())) {
            eprintln!("swarm: telling the service of a call: {e}");
        }
    });
    swarm_app::ringtone::set_phone(|what| {
        if let Err(e) = ring(what) {
            eprintln!("swarm: the ringtone: {e}");
        }
    });
    swarm_app::filedialog::set_phone(swarm_app::filedialog::Phone { pick: pick_file, downloads: downloads_folder() });
    swarm_app::audio::set_java_mic(swarm_app::audio::JavaMic { start: start_mic, stop: stop_mic });
    swarm_app::links::set_phone(open_link);
    swarm_app::discovery::set_multicast_hold(|hold| {
        if let Err(e) = call_activity(|env, activity| env.call_method(activity, "holdMulticast", "(Z)V", &[jni::objects::JValue::Bool(hold.into())]).map(|_| ())) {
            eprintln!("swarm: Wi-Fi's multicast lock: {e}");
        }
    });
    if let Err(e) = swarm_desktop::run_android(app) {
        eprintln!("swarm: {e:#}");
    }
    std::process::exit(0);
}

/// What swarm writes to stderr (its log, and panics) goes to Android's
/// log, read with `adb logcat -s swarm`: an app's stderr goes nowhere.
#[cfg(target_os = "android")]
fn log_to_logcat() {
    use std::ffi::CString;
    use std::io::{BufRead, BufReader};
    use std::os::fd::FromRawFd;

    #[link(name = "log")]
    extern "C" {
        fn __android_log_write(prio: libc::c_int, tag: *const libc::c_char, text: *const libc::c_char) -> libc::c_int;
    }
    const INFO: libc::c_int = 4;

    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 || unsafe { libc::dup2(fds[1], 2) } < 0 {
        return;
    }
    unsafe { libc::close(fds[1]) };
    let read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    std::thread::spawn(move || {
        let tag = CString::new("swarm").unwrap();
        for line in BufReader::new(read).split(b'\n').map_while(Result::ok) {
            if let Ok(text) = CString::new(line) {
                unsafe { __android_log_write(INFO, tag.as_ptr(), text.as_ptr()) };
            }
        }
    });
}

/// The activity's come into sight or gone out of it (`onResume`,
/// `onPause`): Java's `SwarmActivity.inSight`.
#[cfg(target_os = "android")]
#[no_mangle]
extern "system" fn Java_com_cwne88_swarm_SwarmActivity_inSight(_env: *mut std::ffi::c_void, _class: *mut std::ffi::c_void, yes: u8) {
    swarm_desktop::set_in_sight(yes != 0);
}

/// The screen's come on or gone off (Java's `SwarmActivity.screenOn`).
#[cfg(target_os = "android")]
#[no_mangle]
extern "system" fn Java_com_cwne88_swarm_SwarmActivity_screenOn(_env: *mut std::ffi::c_void, _class: *mut std::ffi::c_void, on: u8) {
    swarm_core::node::set_screen_on(on != 0);
}

/// The screen's turned (`Display.getRotation`, in degrees), as the
/// activity says when it comes into sight or the phone's turned: the
/// camera's pictures are turned to match. Java's `SwarmActivity.turned`.
#[cfg(target_os = "android")]
#[no_mangle]
extern "system" fn Java_com_cwne88_swarm_SwarmActivity_turned(_env: *mut std::ffi::c_void, _class: *mut std::ffi::c_void, degrees: i32) {
    swarm_app::camera::set_display_rotation(degrees.max(0) as u32);
}

/// What Android's picker gave, for `pick_file` waiting on it.
#[cfg(target_os = "android")]
type Picked = anyhow::Result<Option<(std::fs::File, String)>>;

#[cfg(target_os = "android")]
static PICKED: std::sync::Mutex<Option<std::sync::mpsc::Sender<Picked>>> = std::sync::Mutex::new(None);

/// A file to send, chosen in Android's own picker (the activity's
/// `chooseFile`): lent to swarm, with its name; `None` if none was. Waits
/// for the person, on the thread swarm asks on.
#[cfg(target_os = "android")]
fn pick_file() -> Picked {
    let (tx, rx) = std::sync::mpsc::channel();
    *PICKED.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
    call_activity(|env, activity| env.call_method(activity, "chooseFile", "()V", &[]).map(|_| ()))?;
    rx.recv().map_err(|_| anyhow::anyhow!("the picker went"))?
}

/// What the picker gave (the activity's `fileChosen`): a descriptor and
/// the file's name, -1 for nothing chosen, -2 and why it couldn't be had.
#[cfg(target_os = "android")]
#[no_mangle]
extern "system" fn Java_com_cwne88_swarm_SwarmActivity_fileChosen<'local>(mut env: jni::JNIEnv<'local>, _class: jni::objects::JClass<'local>, fd: i32, name: jni::objects::JString<'local>) {
    use std::os::fd::FromRawFd;
    let text = if name.is_null() { String::new() } else { env.get_string(&name).map(String::from).unwrap_or_default() };
    let picked = match fd {
        -1 => Ok(None),
        fd if fd < 0 => Err(anyhow::anyhow!("couldn't open it: {text}")),
        fd => Ok(Some((unsafe { std::fs::File::from_raw_fd(fd) }, text))),
    };
    if let Some(tx) = PICKED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
        let _ = tx.send(picked);
    }
}

/// A web address opened in the phone's browser (the activity's
/// `openLink`).
#[cfg(target_os = "android")]
fn open_link(url: &str) {
    let asked = call_activity(|env, activity| {
        let url = env.new_string(url)?;
        env.call_method(activity, "openLink", "(Ljava/lang/String;)V", &[jni::objects::JValue::Object(&url)]).map(|_| ())
    });
    if let Err(e) = asked {
        eprintln!("swarm: opening a link: {e}");
    }
}

/// The phone's Download folder (the activity's `downloadsFolder`).
#[cfg(target_os = "android")]
fn downloads_folder() -> std::path::PathBuf {
    let mut folder = None;
    let asked = call_activity(|env, activity| {
        let got = env.call_method(activity, "downloadsFolder", "()Ljava/lang/String;", &[])?.l()?;
        folder = Some(env.get_string(&jni::objects::JString::from(got))?.into());
        Ok(())
    });
    if let Err(e) = asked {
        eprintln!("swarm: the Download folder: {e}");
    }
    folder.map(|f: String| std::path::PathBuf::from(f)).unwrap_or_else(|| "/storage/emulated/0/Download".into())
}

/// The activity (a reference Android holds for as long as it lives,
/// which is as long as swarm does: see `android_main`).
#[cfg(target_os = "android")]
static ACTIVITY: std::sync::atomic::AtomicPtr<std::ffi::c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// A notification shown or taken away, by the activity's Java
/// (`notifyMessage`, `clearMessage`).
#[cfg(target_os = "android")]
fn notify(notice: swarm_desktop::Notice) -> jni::errors::Result<()> {
    use jni::objects::JValue;
    call_activity(|env, activity| match &notice {
        swarm_desktop::Notice::Message { id, from } => {
            let from = env.new_string(from)?;
            env.call_method(activity, "notifyMessage", "(Ljava/lang/String;I)V", &[JValue::Object(&from), JValue::Int(*id)]).map(|_| ())
        }
        swarm_desktop::Notice::Clear { id } => env.call_method(activity, "clearMessage", "(I)V", &[JValue::Int(*id)]).map(|_| ()),
    })
}

/// The ringtone started or stopped, by the activity's Java
/// (`startRinging`, `stopRinging`).
#[cfg(target_os = "android")]
fn ring(what: swarm_app::ringtone::Phone) -> jni::errors::Result<()> {
    use jni::objects::JValue;
    call_activity(|env, activity| match &what {
        swarm_app::ringtone::Phone::Start { samples, looped } => {
            let array = env.new_short_array(samples.len() as i32)?;
            env.set_short_array_region(&array, 0, samples)?;
            env.call_method(activity, "startRinging", "([SZ)V", &[JValue::Object(&array), JValue::Bool((*looped).into())]).map(|_| ())
        }
        swarm_app::ringtone::Phone::Stop => env.call_method(activity, "stopRinging", "()V", &[]).map(|_| ()),
    })
}

/// Starts the activity's Java recording (`startMic`): see
/// `swarm_app::audio::JavaMic`.
#[cfg(target_os = "android")]
fn start_mic() -> bool {
    let mut started = false;
    if let Err(e) = call_activity(|env, activity| {
        started = env.call_method(activity, "startMic", "()Z", &[])?.z()?;
        Ok(())
    }) {
        eprintln!("swarm: starting the microphone through Java: {e}");
    }
    started
}

/// Stops it (`stopMic`), once its thread has handed over the last of it.
#[cfg(target_os = "android")]
fn stop_mic() {
    if let Err(e) = call_activity(|env, activity| env.call_method(activity, "stopMic", "()V", &[]).map(|_| ())) {
        eprintln!("swarm: stopping the microphone through Java: {e}");
    }
}

/// The Java recording's audio: `count` 16-bit samples, `channels` to a
/// frame, at `rate`, from its own thread.
#[cfg(target_os = "android")]
#[no_mangle]
extern "system" fn Java_com_cwne88_swarm_SwarmActivity_micIn<'local>(
    env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    samples: jni::objects::JShortArray<'local>,
    count: jni::sys::jint,
    channels: jni::sys::jint,
    rate: jni::sys::jint,
) {
    let mut got = vec![0i16; usize::try_from(count).unwrap_or(0)];
    if env.get_short_array_region(&samples, 0, &mut got).is_ok() {
        swarm_app::audio::java_mic_in(&got, usize::try_from(channels).unwrap_or(1), u32::try_from(rate).unwrap_or(48_000));
    }
}

/// Calls the activity's Java with `call`, from this thread (attached to
/// Java for it); an exception it throws described in Android's log.
#[cfg(target_os = "android")]
fn call_activity(call: impl FnOnce(&mut jni::JNIEnv, &jni::objects::JObject) -> jni::errors::Result<()>) -> jni::errors::Result<()> {
    let activity = ACTIVITY.load(std::sync::atomic::Ordering::Relaxed);
    if activity.is_null() {
        return Ok(());
    }
    let vm = unsafe { jni::JavaVM::from_raw(ndk_context::android_context().vm().cast()) }?;
    let mut env = vm.attach_current_thread()?;
    let activity = unsafe { jni::objects::JObject::from_raw(activity.cast()) };
    let called = call(&mut env, &activity);
    if called.is_err() && env.exception_check()? {
        env.exception_describe()?;
        env.exception_clear()?;
    }
    called
}
