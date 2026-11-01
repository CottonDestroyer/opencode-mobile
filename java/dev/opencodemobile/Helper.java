package dev.opencodemobile;

import android.app.Activity;
import android.app.Fragment;
import android.app.KeyguardManager;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.ActivityNotFoundException;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.DialogInterface;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.database.Cursor;
import android.net.Uri;
import android.os.Environment;
import android.provider.DocumentsContract;
import android.provider.MediaStore;
import android.provider.OpenableColumns;
import android.provider.Settings;
import android.hardware.biometrics.BiometricManager;
import android.hardware.biometrics.BiometricPrompt;
import android.os.Build;
import android.os.Bundle;
import android.os.CancellationSignal;
import android.speech.RecognitionListener;
import android.speech.RecognizerIntent;
import android.speech.SpeechRecognizer;
import android.view.View;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.ArrayList;

/**
 * Platform services for the Rust app: the biometric app lock, notifications, speech input, the
 * clipboard, opening URLs, and the photo and folder pickers.
 *
 * Rust ({@code src/platform.rs}) talks to this class through the single entry point {@link #call},
 * passing a command and a string argument and getting a string back, which keeps the JNI surface to
 * one method. Errors come back as {@code "error:<message>"}. Asynchronous results land in static
 * fields that Rust polls; {@code null} means still pending.
 */
public final class Helper {
    private static final String CHANNEL = "agent";
    private static final String POST_NOTIFICATIONS = "android.permission.POST_NOTIFICATIONS";
    private static final String RECORD_AUDIO = "android.permission.RECORD_AUDIO";
    private static final String WRITE_EXTERNAL_STORAGE = "android.permission.WRITE_EXTERNAL_STORAGE";
    // Any biometric class, or the device PIN, pattern or password.
    private static final int AUTHENTICATORS =
            BiometricManager.Authenticators.BIOMETRIC_WEAK | BiometricManager.Authenticators.DEVICE_CREDENTIAL;

    private static volatile String authResult;   // "ok" or "fail:<message>"
    private static volatile String speechResult; // "ok:<text>" or "error:<message>"
    private static SpeechRecognizer recognizer;  // main thread only
    private static int notificationId = 1;

    private Helper() {}

    public static String call(Activity a, String cmd, String arg) {
        try {
            switch (cmd) {
                case "init": return init(a);
                case "request_notifications": return requestNotifications(a);
                case "lock_status": return lockStatus(a);
                case "auth_start": authStart(a, arg); return "";
                case "auth_poll": return authResult == null ? "pending" : authResult;
                case "notify": return notify(a, arg);
                case "speech_start": return speechStart(a);
                case "speech_poll": return speechResult == null ? "pending" : speechResult;
                case "speech_stop": speechStop(a); return "";
                case "copy": copy(a, arg); return "";
                case "paste": return paste(a);
                case "open_url": return openUrl(a, arg);
                case "pick_images": return pickImages(a);
                case "pick_folder": return pickFolder(a);
                case "pick_poll": return pickResult == null ? "pending" : pickResult;
                case "refresh_insets": refreshInsets(a); return "";
                default: return "error:unknown command " + cmd;
            }
        } catch (Throwable t) {
            return "error:" + t;
        }
    }

    // Setup and permissions

    private static String init(Activity a) {
        NotificationChannel ch = new NotificationChannel(CHANNEL, "Agent updates", NotificationManager.IMPORTANCE_HIGH);
        ch.setDescription("When the agent finishes, needs your permission, or hits an error");
        a.getSystemService(NotificationManager.class).createNotificationChannel(ch);
        return "";
    }

    /** Requests the notification permission on Android 13+ unless it's already granted. */
    private static String requestNotifications(Activity a) {
        if (Build.VERSION.SDK_INT >= 33 && a.checkSelfPermission(POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermission(a, POST_NOTIFICATIONS);
        }
        return "";
    }

    private static void requestPermission(final Activity a, final String permission) {
        a.runOnUiThread(new Runnable() {
            @Override public void run() {
                a.requestPermissions(new String[] { permission }, 1);
            }
        });
    }

    /**
     * Re-dispatches window insets. Slint derives the keyboard height from them, and a keyboard that
     * closes while another activity covers ours (photo picker, browser) never delivers its final
     * update, leaving a keyboard-sized gap in the UI. The delayed second pass catches a keyboard that
     * is still animating away.
     */
    private static void refreshInsets(final Activity a) {
        final View decor = a.getWindow().getDecorView();
        final Runnable dispatch = new Runnable() {
            @Override public void run() {
                decor.requestApplyInsets();
            }
        };
        a.runOnUiThread(dispatch);
        decor.postDelayed(dispatch, 500);
    }

    // App lock

    /** Returns {@code "1|<description>"} when the lock is usable, otherwise {@code "0|<reason>"}. */
    private static String lockStatus(Activity a) {
        if (Build.VERSION.SDK_INT < 28) return "0|App lock needs Android 9 or newer";
        if (Build.VERSION.SDK_INT >= 30) {
            int r = a.getSystemService(BiometricManager.class).canAuthenticate(AUTHENTICATORS);
            if (r == BiometricManager.BIOMETRIC_SUCCESS) return "1|Use your fingerprint, face or screen lock";
            if (r == BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED) return "0|Set up a screen lock in Android settings first";
            return "0|Biometric unlock isn't available on this device";
        }
        boolean secure = a.getSystemService(KeyguardManager.class).isDeviceSecure();
        return secure ? "1|Use your fingerprint or screen lock" : "0|Set up a screen lock in Android settings first";
    }

    private static void authStart(final Activity a, final String title) {
        authResult = null;
        if (Build.VERSION.SDK_INT < 28) {
            authResult = "fail:App lock needs Android 9 or newer";
            return;
        }
        a.runOnUiThread(new Runnable() {
            @Override public void run() {
                try {
                    BiometricPrompt.Builder b = new BiometricPrompt.Builder(a).setTitle(title);
                    if (Build.VERSION.SDK_INT >= 30) {
                        b.setAllowedAuthenticators(AUTHENTICATORS);
                    } else if (Build.VERSION.SDK_INT >= 29) {
                        b.setDeviceCredentialAllowed(true);
                    } else {
                        b.setNegativeButton("Cancel", a.getMainExecutor(), new DialogInterface.OnClickListener() {
                            @Override public void onClick(DialogInterface d, int which) {
                                authResult = "fail:Cancelled";
                            }
                        });
                    }
                    b.build().authenticate(new CancellationSignal(), a.getMainExecutor(), new BiometricPrompt.AuthenticationCallback() {
                        @Override public void onAuthenticationSucceeded(BiometricPrompt.AuthenticationResult r) {
                            authResult = "ok";
                        }
                        @Override public void onAuthenticationError(int code, CharSequence message) {
                            authResult = "fail:" + message;
                        }
                        // onAuthenticationFailed (an unrecognized finger) keeps the prompt open, so it
                        // produces no result.
                    });
                } catch (Throwable t) {
                    authResult = "fail:" + t;
                }
            }
        });
    }

    // Notifications

    /** {@code arg} is the title and body separated by a newline. Tapping the notification reopens the app. */
    private static String notify(Activity a, String arg) {
        if (Build.VERSION.SDK_INT >= 33 && a.checkSelfPermission(POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            return "error:notification permission not granted";
        }
        int nl = arg.indexOf('\n');
        String title = nl < 0 ? arg : arg.substring(0, nl);
        String body = nl < 0 ? "" : arg.substring(nl + 1);
        Intent open = a.getPackageManager().getLaunchIntentForPackage(a.getPackageName());
        open.setFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED);
        PendingIntent tap = PendingIntent.getActivity(a, 0, open, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        Notification n = new Notification.Builder(a, CHANNEL)
                .setSmallIcon(android.R.drawable.stat_notify_chat)
                .setContentTitle(title)
                .setContentText(body)
                .setStyle(new Notification.BigTextStyle().bigText(body))
                .setContentIntent(tap)
                .setAutoCancel(true)
                .build();
        a.getSystemService(NotificationManager.class).notify(notificationId++, n);
        return "";
    }

    // Speech input

    private static String speechStart(final Activity a) {
        if (a.checkSelfPermission(RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            requestPermission(a, RECORD_AUDIO);
            return "error:Allow microphone access, then tap the mic again";
        }
        if (!SpeechRecognizer.isRecognitionAvailable(a)) {
            return "error:No speech recognition service on this device";
        }
        speechResult = null;
        a.runOnUiThread(new Runnable() {
            @Override public void run() {
                try {
                    if (recognizer != null) recognizer.destroy();
                    recognizer = SpeechRecognizer.createSpeechRecognizer(a);
                    recognizer.setRecognitionListener(new RecognitionListener() {
                        @Override public void onResults(Bundle results) {
                            ArrayList<String> r = results.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION);
                            speechResult = "ok:" + (r == null || r.isEmpty() ? "" : r.get(0));
                        }
                        @Override public void onError(int error) {
                            speechResult = speechError(error);
                        }
                        @Override public void onReadyForSpeech(Bundle params) {}
                        @Override public void onBeginningOfSpeech() {}
                        @Override public void onRmsChanged(float rmsdB) {}
                        @Override public void onBufferReceived(byte[] buffer) {}
                        @Override public void onEndOfSpeech() {}
                        @Override public void onPartialResults(Bundle partialResults) {}
                        @Override public void onEvent(int eventType, Bundle params) {}
                    });
                    Intent i = new Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH);
                    i.putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM);
                    recognizer.startListening(i);
                } catch (Throwable t) {
                    speechResult = "error:" + t;
                }
            }
        });
        return "";
    }

    /** Stops listening early; whatever was recognized so far is still delivered as the result. */
    private static void speechStop(Activity a) {
        a.runOnUiThread(new Runnable() {
            @Override public void run() {
                if (recognizer != null) recognizer.stopListening();
            }
        });
    }

    // Clipboard

    private static void copy(final Activity a, final String text) {
        a.runOnUiThread(new Runnable() {
            @Override public void run() {
                a.getSystemService(ClipboardManager.class).setPrimaryClip(ClipData.newPlainText("OpenCode", text));
            }
        });
    }

    /**
     * Returns {@code "image:<cache file>"}, {@code "text:<text>"} or {@code "empty"}. Copies images out
     * of their content provider, so call it off the UI thread.
     */
    private static String paste(Activity a) {
        ClipboardManager cm = a.getSystemService(ClipboardManager.class);
        ClipData clip = cm.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) return "empty";
        ClipData.Item item = clip.getItemAt(0);
        Uri uri = item.getUri();
        if (uri != null) {
            String type = a.getContentResolver().getType(uri);
            if (type != null && type.startsWith("image/")) {
                return "image:" + copyToCache(a, uri, "pasted-image");
            }
        }
        CharSequence text = item.coerceToText(a);
        return text == null || text.length() == 0 ? "empty" : "text:" + text;
    }

    // URLs

    private static String openUrl(Activity a, String url) {
        try {
            Intent i = new Intent(Intent.ACTION_VIEW, Uri.parse(url));
            i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
            a.startActivity(i);
            return "";
        } catch (ActivityNotFoundException e) {
            return "error:No app can open " + url;
        }
    }

    // Pickers
    //
    // NativeActivity can't receive activity results, and cargo-apk allows only one activity, so pickers
    // are launched from a headless Fragment that receives onActivityResult. Results land in pickResult:
    // "ok:<payload>" (an empty payload on cancel) or "error:<message>".

    private static final int PICK_IMAGES = 1;
    private static final int PICK_FOLDER = 2;
    private static volatile String pickResult;

    /** Headless fragment that launches a picker and forwards its result. */
    public static class ResultFragment extends Fragment {
        Intent intent;
        int code;

        public ResultFragment() {}

        @Override public void onCreate(Bundle state) {
            super.onCreate(state);
            if (intent == null) { // restored by the system, with no picker to wait for
                done();
                return;
            }
            try {
                startActivityForResult(intent, code);
            } catch (ActivityNotFoundException e) {
                pickResult = "error:No app on this device can do that";
                done();
            }
        }

        @Override public void onActivityResult(int requestCode, int resultCode, final Intent data) {
            final Activity a = getActivity();
            final int req = requestCode;
            final boolean ok = resultCode == Activity.RESULT_OK && data != null;
            done();
            if (!ok) {
                pickResult = "ok:";
                return;
            }
            // Copying large images would stall the UI thread.
            new Thread(new Runnable() {
                @Override public void run() {
                    try {
                        pickResult = "ok:" + (req == PICK_IMAGES ? copyPickedImages(a, data) : folderPath(data.getData()));
                    } catch (Throwable t) {
                        pickResult = "error:" + t.getMessage();
                    }
                }
            }).start();
        }

        private void done() {
            getFragmentManager().beginTransaction().remove(this).commitAllowingStateLoss();
        }
    }

    private static String startPicker(final Activity a, final Intent intent, final int code) {
        pickResult = null;
        a.runOnUiThread(new Runnable() {
            @Override public void run() {
                try {
                    ResultFragment f = new ResultFragment();
                    f.intent = intent;
                    f.code = code;
                    a.getFragmentManager().beginTransaction().add(f, "opencode-picker").commitAllowingStateLoss();
                } catch (Throwable t) {
                    pickResult = "error:" + t;
                }
            }
        });
        return "";
    }

    private static String pickImages(Activity a) {
        Intent i;
        if (Build.VERSION.SDK_INT >= 33) {
            i = new Intent(MediaStore.ACTION_PICK_IMAGES); // the system photo picker
            i.putExtra(MediaStore.EXTRA_PICK_IMAGES_MAX, 10);
        } else {
            i = new Intent(Intent.ACTION_GET_CONTENT);
            i.setType("image/*");
            i.addCategory(Intent.CATEGORY_OPENABLE);
            i.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true);
        }
        return startPicker(a, i, PICK_IMAGES);
    }

    /** Copies the picked images into the cache directory and returns their paths, one per line. */
    private static String copyPickedImages(Activity a, Intent data) throws Exception {
        ArrayList<Uri> uris = new ArrayList<>();
        if (data.getClipData() != null) {
            for (int i = 0; i < data.getClipData().getItemCount(); i++) uris.add(data.getClipData().getItemAt(i).getUri());
        } else if (data.getData() != null) {
            uris.add(data.getData());
        }
        StringBuilder out = new StringBuilder();
        for (Uri u : uris) {
            if (out.length() > 0) out.append('\n');
            out.append(copyToCache(a, u, "image"));
        }
        return out.toString();
    }

    /**
     * Picks a project folder. The agent's tools need real filesystem paths, so this requires "All files
     * access", and the picked document tree is mapped back to its path on storage.
     */
    private static String pickFolder(Activity a) {
        if (Build.VERSION.SDK_INT >= 30) {
            if (!Environment.isExternalStorageManager()) {
                Intent s = new Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, Uri.parse("package:" + a.getPackageName()));
                s.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                a.startActivity(s);
                return "error:Allow \"All files access\" for OpenCode Mobile, then try again";
            }
        } else if (a.checkSelfPermission(WRITE_EXTERNAL_STORAGE) != PackageManager.PERMISSION_GRANTED) {
            requestPermission(a, WRITE_EXTERNAL_STORAGE);
            return "error:Allow storage access, then try again";
        }
        return startPicker(a, new Intent(Intent.ACTION_OPEN_DOCUMENT_TREE), PICK_FOLDER);
    }

    /** Maps {@code primary:Documents/x} to /storage/emulated/0/Documents/x and {@code ABCD-1234:x} to /storage/ABCD-1234/x. */
    private static String folderPath(Uri tree) {
        String id = DocumentsContract.getTreeDocumentId(tree);
        int colon = id.indexOf(':');
        String volume = colon < 0 ? id : id.substring(0, colon);
        String rel = colon < 0 ? "" : id.substring(colon + 1);
        String root = volume.equals("primary") ? Environment.getExternalStorageDirectory().getAbsolutePath() : "/storage/" + volume;
        if (volume.equals("home")) root = Environment.getExternalStorageDirectory().getAbsolutePath() + "/Documents";
        return rel.isEmpty() ? root : root + "/" + rel;
    }

    /** Copies a content URI to {@code <cache>/picked/<nanos>/<display name>} and returns the file path. */
    private static String copyToCache(Activity a, Uri uri, String fallbackName) {
        String name = fallbackName;
        Cursor c = a.getContentResolver().query(uri, new String[] { OpenableColumns.DISPLAY_NAME }, null, null, null);
        if (c != null) {
            try {
                if (c.moveToFirst() && c.getString(0) != null) name = c.getString(0);
            } finally {
                c.close();
            }
        }
        File dir = new File(new File(a.getCacheDir(), "picked"), String.valueOf(System.nanoTime()));
        dir.mkdirs();
        File out = new File(dir, name.replace('/', '_'));
        try (InputStream in = a.getContentResolver().openInputStream(uri); OutputStream os = new FileOutputStream(out)) {
            byte[] buf = new byte[64 * 1024];
            int n;
            while ((n = in.read(buf)) > 0) os.write(buf, 0, n);
        } catch (Exception e) {
            throw new RuntimeException("Couldn't read " + name + ": " + e.getMessage(), e);
        }
        return out.getAbsolutePath();
    }

    private static String speechError(int error) {
        switch (error) {
            case SpeechRecognizer.ERROR_NO_MATCH:
            case SpeechRecognizer.ERROR_SPEECH_TIMEOUT:
            case SpeechRecognizer.ERROR_CLIENT:
                return "ok:"; // silence or an explicit stop: no text, but not an error
            case SpeechRecognizer.ERROR_INSUFFICIENT_PERMISSIONS:
                return "error:Microphone permission denied";
            case SpeechRecognizer.ERROR_NETWORK:
            case SpeechRecognizer.ERROR_NETWORK_TIMEOUT:
                return "error:Voice input needs a network connection";
            case SpeechRecognizer.ERROR_RECOGNIZER_BUSY:
                return "error:Speech recognition is busy, try again";
            default:
                return "error:Voice input failed (code " + error + ")";
        }
    }
}
