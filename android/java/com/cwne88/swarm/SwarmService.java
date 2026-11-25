package com.cwne88.swarm;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;

/** Keeps swarm's process in the foreground, with a notification saying
 *  so, so Android doesn't stop or freeze it while it's out of sight:
 *  there's no server to wake it for a message or a call. Nothing runs
 *  here; swarm itself runs in the activity's library. */
public class SwarmService extends Service {
    private static final String CHANNEL = "connected";
    /** The service, while it runs (in swarm's own process). */
    private static SwarmService running;

    /** A call under way (true), or over, and its camera on or not:
     *  during one, the service says it uses the microphone too, and the
     *  camera while that's on, or Android silences the one and stops the
     *  other whenever swarm's out of sight (the screen off). Adding them
     *  is only allowed while swarm's in sight, as it is when a call's
     *  placed or answered, or the camera's turned on. */
    static void callUnderway(boolean call, boolean camera) {
        SwarmService service = running;
        if (service != null) {
            new Handler(Looper.getMainLooper()).post(() -> service.show(call, camera));
        }
    }

    @Override
    public void onCreate() {
        super.onCreate();
        running = this;
    }

    @Override
    public void onDestroy() {
        running = null;
        super.onDestroy();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        show(false, false);
        return START_NOT_STICKY;
    }

    /** In the foreground, with its notification: on a call or not, its
     *  camera on or not. */
    private void show(boolean call, boolean camera) {
        NotificationManager manager = getSystemService(NotificationManager.class);
        manager.createNotificationChannel(new NotificationChannel(CHANNEL, "Connected", NotificationManager.IMPORTANCE_LOW));
        PendingIntent open = PendingIntent.getActivity(this, 0,
                new Intent(this, SwarmActivity.class).setFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
                PendingIntent.FLAG_IMMUTABLE);
        Notification notification = new Notification.Builder(this, CHANNEL)
                .setSmallIcon(android.R.drawable.sym_action_chat)
                .setContentTitle("Swarm")
                .setContentText(call ? "On a call" : "Connected to your peers")
                .setContentIntent(open)
                .setOngoing(true)
                .build();
        try {
            if (Build.VERSION.SDK_INT >= 34) {
                int type = ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE;
                if (call) {
                    type |= ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE;
                }
                if (call && camera) {
                    type |= ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA;
                }
                startForeground(1, notification, type);
            } else if (Build.VERSION.SDK_INT >= 30 && call) {
                int type = ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE;
                if (camera) {
                    type |= ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA;
                }
                startForeground(1, notification, type);
            } else {
                startForeground(1, notification);
            }
        } catch (RuntimeException refused) {
            android.util.Log.w("swarm", "the service as on a call: " + refused);
        }
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
