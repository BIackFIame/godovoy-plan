package local.godovoyplan.mobile;

import android.app.AlarmManager;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.ComponentName;
import android.content.Context;
import android.content.pm.PackageManager;
import android.content.Intent;
import android.content.SharedPreferences;
import org.json.JSONArray;
import java.time.LocalDate;
import java.time.ZoneId;
import java.time.format.DateTimeParseException;
import java.util.HashSet;
import java.util.Set;

public final class ReminderSchedule {
    private static final String CHANNEL = "calendar-reminders";
    static final String ALARM = "local.godovoyplan.mobile.REMINDER";
    private static SharedPreferences preferences(Context context) {
        return context.getSharedPreferences("reminder-schedule", Context.MODE_PRIVATE);
    }
    public static boolean enabled(Context context) {
        return preferences(context).getBoolean("enabled", false);
    }
    // Keep the manifest default for upgrades from versions with active reminders.
    // A fresh or opted-out profile should not start on boot or clock changes.
    static void configureReceiver(Context context, boolean enabled) {
        PackageManager packages = context.getPackageManager();
        ComponentName receiver = new ComponentName(context, ReminderReceiver.class);
        int wanted = enabled ? PackageManager.COMPONENT_ENABLED_STATE_DEFAULT
                             : PackageManager.COMPONENT_ENABLED_STATE_DISABLED;
        if (packages.getComponentEnabledSetting(receiver) != wanted) {
            packages.setComponentEnabledSetting(receiver, wanted, PackageManager.DONT_KILL_APP);
        }
    }
    public static synchronized void enable(Context context) {
        configureReceiver(context, true);
        preferences(context).edit().putBoolean("enabled", true).apply();
        context.getSystemService(NotificationManager.class).createNotificationChannel(
                new NotificationChannel(CHANNEL, "Напоминания календаря", NotificationManager.IMPORTANCE_DEFAULT));
        rearm(context);
    }
    public static synchronized void disable(Context context) {
        preferences(context).edit().putBoolean("enabled", false).apply();
        cancelAlarm(context);
        configureReceiver(context, false);
        context.getSystemService(NotificationManager.class).cancel(1);
    }
    public static synchronized void refresh(Context context, JSONArray dates) {
        Set<String> next = new HashSet<>();
        if (dates != null) {
            for (int i = 0; i < dates.length(); i++) {
                try {next.add(LocalDate.parse(dates.optString(i)).toString());}
                catch (DateTimeParseException ignored) { /* Only accept ISO calendar dates. */ }
            }
        }
        SharedPreferences pref = preferences(context);
        if (next.equals(pref.getStringSet("dates", null))) return;
        pref.edit().putStringSet("dates", next).apply();
        rearm(context);
    }
    private static PendingIntent alarmIntent(Context context, int flags) {
        Intent intent = new Intent(context, ReminderReceiver.class).setAction(ALARM);
        return PendingIntent.getBroadcast(context, 1, intent, PendingIntent.FLAG_IMMUTABLE | flags);
    }
    private static void cancelAlarm(Context context) {
        PendingIntent existing = alarmIntent(context, PendingIntent.FLAG_NO_CREATE);
        if (existing != null) {
            context.getSystemService(AlarmManager.class).cancel(existing);
            existing.cancel();
        }
    }
    static synchronized void rearm(Context context) {
        if (!enabled(context)) return;
        long now = System.currentTimeMillis(), earliest = Long.MAX_VALUE;
        String previous = preferences(context).getString("delivered", "");
        for (String date : preferences(context).getStringSet("dates", java.util.Collections.emptySet())) {
            if (date.equals(previous)) continue;
            long time = LocalDate.parse(date).atTime(9, 0).atZone(ZoneId.systemDefault()).toInstant().toEpochMilli();
            if (time > now && time < earliest) earliest = time;
        }
        if (earliest == Long.MAX_VALUE) {
            cancelAlarm(context);
        } else {
            context.getSystemService(AlarmManager.class).setAndAllowWhileIdle(
                    AlarmManager.RTC_WAKEUP, earliest, alarmIntent(context, 0));
        }
    }
    static synchronized void deliver(Context context) {
        String day = LocalDate.now().toString();
        SharedPreferences pref = preferences(context);
        NotificationManager notifications = context.getSystemService(NotificationManager.class);
        if (enabled(context) && notifications.areNotificationsEnabled() &&
                !day.equals(pref.getString("delivered", "")) &&
                pref.getStringSet("dates", java.util.Collections.emptySet()).contains(day)) {
            Intent open = context.getPackageManager().getLaunchIntentForPackage(context.getPackageName());
            if (open != null) {
                PendingIntent tap = PendingIntent.getActivity(context, 2, open, PendingIntent.FLAG_IMMUTABLE);
                Notification notification = new Notification.Builder(context, CHANNEL)
                    .setSmallIcon(android.R.drawable.ic_popup_reminder).setContentTitle("Годовой план")
                    .setContentText("Откройте календарь, чтобы проверить свои задачи.")
                    .setContentIntent(tap).setAutoCancel(true).setVisibility(Notification.VISIBILITY_PRIVATE).build();
                try {
                    notifications.notify(1, notification);
                    pref.edit().putString("delivered", day).apply();
                } catch (SecurityException revokedWhilePosting) { /* Permission can change while the receiver runs. */ }
            }
        }
        rearm(context);
    }
    private ReminderSchedule() {}
}
