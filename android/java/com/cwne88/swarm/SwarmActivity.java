package com.cwne88.swarm;

import android.Manifest;
import android.app.NativeActivity;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.content.res.Configuration;
import android.database.Cursor;
import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioManager;
import android.media.AudioRecord;
import android.media.AudioTrack;
import android.media.MediaRecorder;
import android.net.Uri;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.os.ParcelFileDescriptor;
import android.os.PowerManager;
import android.os.VibrationEffect;
import android.os.Vibrator;
import android.os.Process;
import android.provider.OpenableColumns;
import android.util.Log;
import android.view.Surface;
import android.view.WindowManager;
import java.util.concurrent.atomic.AtomicBoolean;

/** swarm's screen: Android's NativeActivity, which runs the Rust library
 *  (libswarm.so, its android_main), plus starting the service that keeps
 *  swarm connected while it's out of sight. */
public class SwarmActivity extends NativeActivity {
    private static final String MESSAGES = "messages";
    private static final int CHOOSE_FILE = 2;

    static {
        System.loadLibrary("swarm");
    }

    /** swarm in sight or not (see android/src/lib.rs). */
    private static native void inSight(boolean yes);

    /** A file chosen to send: its descriptor (swarm's to close) and
     *  name; -1 if none was, -2 if it couldn't be opened, and why (see
     *  android/src/lib.rs). */
    private static native void fileChosen(int fd, String nameOrWhy);

    /** The screen's turned this far (degrees), so the camera's pictures
     *  are turned to match (see android/src/lib.rs). */
    private static native void turned(int degrees);

    /** The screen's come on (true) or gone off: with it off, those who
     *  can't be reached are dialled less and less often, and with it on
     *  as before (swarm_core::node::set_screen_on). Said once at the
     *  start, then on each change, for as long as swarm runs. */
    private static native void screenOn(boolean on);

    /** The microphone's audio, recorded here (startMic): count 16-bit
     *  samples, channels to a frame, at rate (see android/src/lib.rs). */
    private static native void micIn(short[] samples, int count, int channels, int rate);

    private static final int MIC_RATE = 48000;
    /** 20ms, as swarm's other microphones hand it over. */
    private static final int MIC_CHUNK = MIC_RATE / 50;
    private volatile boolean micOn;
    private Thread micThread;

    /** Records the microphone the way calling apps do (the voice
     *  communication source, mono, 16-bit, 48kHz), for a device whose
     *  native audio misbehaves (see swarm_app::audio::JavaMic), handing
     *  it to swarm 20ms at a time from a thread of its own. False if it
     *  couldn't be started. */
    public synchronized boolean startMic() {
        stopMic();
        int least = AudioRecord.getMinBufferSize(MIC_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT);
        if (least <= 0) {
            Log.w("swarm", "the microphone: no buffer size for 48kHz mono 16-bit (" + least + ")");
            return false;
        }
        final AudioRecord recording;
        try {
            recording = new AudioRecord(MediaRecorder.AudioSource.VOICE_COMMUNICATION, MIC_RATE,
                    AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT, Math.max(least, MIC_CHUNK * 2 * 4));
        } catch (RuntimeException e) {
            Log.w("swarm", "the microphone: " + e);
            return false;
        }
        if (recording.getState() != AudioRecord.STATE_INITIALIZED) {
            recording.release();
            Log.w("swarm", "the microphone: AudioRecord didn't start up");
            return false;
        }
        try {
            recording.startRecording();
        } catch (RuntimeException e) {
            recording.release();
            Log.w("swarm", "the microphone: " + e);
            return false;
        }
        final int channels = Math.max(1, recording.getChannelCount());
        final int rate = recording.getSampleRate();
        Log.i("swarm", "the microphone through Java: " + channels + " channel(s) at " + rate + " Hz");
        micOn = true;
        micThread = new Thread(() -> {
            Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO);
            short[] chunk = new short[MIC_CHUNK * channels];
            while (micOn) {
                int got = recording.read(chunk, 0, chunk.length);
                if (got > 0) {
                    micIn(chunk, got - got % channels, channels, rate);
                } else if (got < 0) {
                    Log.w("swarm", "the microphone: reading it failed (" + got + ")");
                    break;
                }
            }
            try {
                recording.stop();
            } catch (RuntimeException e) {
            }
            recording.release();
        }, "swarm microphone");
        micThread.start();
        return true;
    }

    /** Stops startMic's recording, once its thread has handed over the
     *  last of it. */
    public synchronized void stopMic() {
        micOn = false;
        Thread thread = micThread;
        micThread = null;
        if (thread != null) {
            try {
                thread.join(2000);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        inSight(true);
        sayTurned();
    }

    @Override
    public void onConfigurationChanged(Configuration changed) {
        super.onConfigurationChanged(changed);
        sayTurned();
    }

    @SuppressWarnings("deprecation")
    private void sayTurned() {
        int rotation = Build.VERSION.SDK_INT >= 30 ? getDisplay().getRotation() : getWindowManager().getDefaultDisplay().getRotation();
        switch (rotation) {
            case Surface.ROTATION_90: turned(90); break;
            case Surface.ROTATION_180: turned(180); break;
            case Surface.ROTATION_270: turned(270); break;
            default: turned(0);
        }
    }

    @Override
    protected void onPause() {
        inSight(false);
        super.onPause();
    }

    /** A message from `from`, notified: who it's from, never what it
     *  says, and on the lock screen not even that. Tapped, swarm opens.
     *  Called from swarm (android/src/lib.rs), on its own thread. */
    public void notifyMessage(String from, int id) {
        NotificationManager manager = getSystemService(NotificationManager.class);
        manager.createNotificationChannel(new NotificationChannel(MESSAGES, "Messages", NotificationManager.IMPORTANCE_DEFAULT));
        PendingIntent open = PendingIntent.getActivity(this, 0,
                new Intent(this, SwarmActivity.class).setFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
                PendingIntent.FLAG_IMMUTABLE);
        Notification hidden = new Notification.Builder(this, MESSAGES)
                .setSmallIcon(android.R.drawable.sym_action_chat)
                .setContentTitle("Swarm")
                .setContentText("New message")
                .build();
        Notification notification = new Notification.Builder(this, MESSAGES)
                .setSmallIcon(android.R.drawable.sym_action_chat)
                .setContentTitle("New message from " + from)
                .setContentIntent(open)
                .setAutoCancel(true)
                .setVisibility(Notification.VISIBILITY_PRIVATE)
                .setPublicVersion(hidden)
                .build();
        manager.notify(id, notification);
    }

    /** A call under way, or over, and its camera on or not (see
     *  SwarmService.callUnderway); with video, the screen's kept on while
     *  it lasts, its pictures being watched, not touched. Called from
     *  swarm, on its own thread. */
    public void callUnderway(boolean call, boolean camera, boolean video) {
        SwarmService.callUnderway(call, camera);
        runOnUiThread(() -> {
            if (call && video) {
                getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            } else {
                getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            }
        });
    }

    /** Android's own picker, for a file to send; what's chosen goes to
     *  fileChosen. Called from swarm, on its own thread. */
    public void openLink(String url) {
        runOnUiThread(() -> {
            try {
                startActivity(new Intent(Intent.ACTION_VIEW, Uri.parse(url)).addCategory(Intent.CATEGORY_BROWSABLE));
            } catch (Exception e) {
                Log.w("swarm", "opening " + url + ": " + e);
            }
        });
    }

    public void chooseFile() {
        runOnUiThread(() -> {
            Intent pick = new Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE).setType("*/*");
            try {
                startActivityForResult(pick, CHOOSE_FILE);
            } catch (Exception e) {
                fileChosen(-2, String.valueOf(e.getMessage()));
            }
        });
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request != CHOOSE_FILE) {
            return;
        }
        Uri uri = result == RESULT_OK && data != null ? data.getData() : null;
        if (uri == null) {
            fileChosen(-1, null);
            return;
        }
        String name = "";
        try (Cursor found = getContentResolver().query(uri, new String[] { OpenableColumns.DISPLAY_NAME }, null, null, null)) {
            if (found != null && found.moveToFirst() && !found.isNull(0)) {
                name = found.getString(0);
            }
        } catch (Exception e) {
        }
        try {
            ParcelFileDescriptor file = getContentResolver().openFileDescriptor(uri, "r");
            if (file == null) {
                fileChosen(-2, "it couldn't be opened");
            } else {
                fileChosen(file.detachFd(), name);
            }
        } catch (Exception e) {
            fileChosen(-2, String.valueOf(e.getMessage()));
        }
    }

    /** The phone's Download folder, where accepted files go. */
    @SuppressWarnings("deprecation")
    public String downloadsFolder() {
        return Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS).getPath();
    }

    /** Wi-Fi's multicast lock, while swarm's being found on the network
     *  or finding others there (app/src/discovery.rs); null if not held. */
    private WifiManager.MulticastLock multicast;

    /** Held or let go: without it, Wi-Fi drops mDNS's packets. */
    public synchronized void holdMulticast(boolean hold) {
        if (hold && multicast == null) {
            WifiManager wifi = getApplicationContext().getSystemService(WifiManager.class);
            if (wifi == null) {
                return;
            }
            multicast = wifi.createMulticastLock("swarm:mdns");
            multicast.setReferenceCounted(false);
            multicast.acquire();
        } else if (!hold && multicast != null) {
            multicast.release();
            multicast = null;
        }
    }

    /** The ringtone's rate: swarm's, as a call's audio is. */
    private static final int RING_RATE = 48000;
    /** The ringtone playing (or vibrating): set to stop it; null if none. */
    private AtomicBoolean ringStop;
    /** Kept awake while a call rings, the screen off or not. */
    private PowerManager.WakeLock ringAwake;

    /** How the ringtone's played: as a ringtone, so the phone's ring
     *  volume goes for it, as its own calls' do. */
    private static AudioAttributes ringing() {
        return new AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE)
                .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                .build();
    }

    /** A call ringing (looped, until stopRinging) or the ringtone heard
     *  in Settings (once): 16-bit samples, mono, at 48kHz (see
     *  app/src/ringtone.rs). A call rings as the phone's own do: not in
     *  silent mode, vibrating in vibrate mode, and kept awake meanwhile. */
    @SuppressWarnings("deprecation")
    public synchronized void startRinging(short[] samples, boolean looped) {
        stopRinging();
        int mode = getSystemService(AudioManager.class).getRingerMode();
        if (looped) {
            if (mode == AudioManager.RINGER_MODE_SILENT) {
                return;
            }
            ringAwake = getSystemService(PowerManager.class).newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "swarm:ringing");
            ringAwake.acquire(2 * 60 * 1000L);
            if (mode == AudioManager.RINGER_MODE_VIBRATE) {
                ringStop = new AtomicBoolean(false);
                getSystemService(Vibrator.class).vibrate(VibrationEffect.createWaveform(new long[] { 0, 800, 1200 }, 0), ringing());
                return;
            }
        }
        final AtomicBoolean stop = new AtomicBoolean(false);
        ringStop = stop;
        new Thread(() -> play(samples, looped, stop), "swarm ringtone").start();
    }

    /** Plays `samples`, over and over while `looped`, until `stop`. */
    private static void play(short[] samples, boolean looped, AtomicBoolean stop) {
        int least = AudioTrack.getMinBufferSize(RING_RATE, AudioFormat.CHANNEL_OUT_MONO, AudioFormat.ENCODING_PCM_16BIT);
        AudioTrack track;
        try {
            track = new AudioTrack.Builder()
                    .setAudioAttributes(ringing())
                    .setAudioFormat(new AudioFormat.Builder()
                            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                            .setSampleRate(RING_RATE)
                            .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                            .build())
                    .setTransferMode(AudioTrack.MODE_STREAM)
                    .setBufferSizeInBytes(Math.max(least, RING_RATE / 5 * 2))
                    .build();
            track.play();
        } catch (RuntimeException e) {
            System.err.println("swarm: the ringtone couldn't be played: " + e);
            return;
        }
        int piece = RING_RATE / 10;
        do {
            for (int at = 0; at < samples.length && !stop.get(); ) {
                int written = track.write(samples, at, Math.min(piece, samples.length - at));
                if (written < 0) {
                    stop.set(true);
                    break;
                }
                at += written;
            }
        } while (looped && !stop.get());
        if (!stop.get()) {
            track.stop();
            long until = System.currentTimeMillis() + 1000;
            while (track.getPlayState() == AudioTrack.PLAYSTATE_PLAYING && System.currentTimeMillis() < until && !stop.get()) {
                try { Thread.sleep(20); } catch (InterruptedException e) { break; }
            }
        } else {
            track.pause();
            track.flush();
        }
        track.release();
    }

    /** The ringtone stopped: the call's answered, here or elsewhere,
     *  rejected, or rung out. */
    public synchronized void stopRinging() {
        if (ringStop != null) {
            ringStop.set(true);
            ringStop = null;
            getSystemService(Vibrator.class).cancel();
        }
        if (ringAwake != null) {
            if (ringAwake.isHeld()) {
                ringAwake.release();
            }
            ringAwake = null;
        }
    }

    /** A chat's notification taken away: it's been read. */
    public void clearMessage(int id) {
        getSystemService(NotificationManager.class).cancel(id);
    }

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        java.util.ArrayList<String> ask = new java.util.ArrayList<>();
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            ask.add(Manifest.permission.RECORD_AUDIO);
        }
        if (checkSelfPermission(Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED) {
            ask.add(Manifest.permission.CAMERA);
        }
        if (Build.VERSION.SDK_INT >= 33
                && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            ask.add(Manifest.permission.POST_NOTIFICATIONS);
        }
        if (!ask.isEmpty()) {
            requestPermissions(ask.toArray(new String[0]), 1);
        }
        startForegroundService(new Intent(this, SwarmService.class));
        IntentFilter screen = new IntentFilter(Intent.ACTION_SCREEN_ON);
        screen.addAction(Intent.ACTION_SCREEN_OFF);
        registerReceiver(new BroadcastReceiver() {
            @Override
            public void onReceive(Context context, Intent intent) {
                screenOn(Intent.ACTION_SCREEN_ON.equals(intent.getAction()));
            }
        }, screen);
        screenOn(getSystemService(PowerManager.class).isInteractive());
    }

    /** Back leaves swarm running, out of sight, as Home does: it's
     *  swiping it away that quits. The window takes Back while it has
     *  something to close; when it hasn't, Slint finishes the activity
     *  (Android 14 and later), or Android calls onBackPressed. */
    @Override
    public void finish() {
        moveTaskToBack(true);
    }

    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() {
        moveTaskToBack(true);
    }
}
