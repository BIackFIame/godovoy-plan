package local.godovoyplan.mobile;

import android.app.AlarmManager;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Context;
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
    public static synchronized void enable(Context context) {
        preferences(context).edit().putBoolean("enabled", true).apply();
        context.getSystemService(NotificationManager.class).createNotificationChannel(
                new NotificationChannel(CHANNEL, "Напоминания календаря", NotificationManager.IMPORTANCE_DEFAULT));
        rearm(context);
    }
    public static synchronized void disable(Context context) {
        preferences(context).edit().putBoolean("enabled", false).apply();
        context.getSystemService(AlarmManager.class).cancel(alarmIntent(context));
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
    private static PendingIntent alarmIntent(Context context) {
        Intent intent = new Intent(context, ReminderReceiver.class).setAction(ALARM);
        return PendingIntent.getBroadcast(context, 1, intent, PendingIntent.FLAG_IMMUTABLE);
    }
    static synchronized void rearm(Context context) {
        AlarmManager alarms = context.getSystemService(AlarmManager.class);
        PendingIntent intent = alarmIntent(context);
        alarms.cancel(intent);
        if (!enabled(context)) return;
        long now = System.currentTimeMillis(), earliest = Long.MAX_VALUE;
        String previous = preferences(context).getString("delivered", "");
        for (String date : preferences(context).getStringSet("dates", java.util.Collections.emptySet())) {
            if (date.equals(previous)) continue;
            long time = LocalDate.parse(date).atTime(9, 0).atZone(ZoneId.systemDefault()).toInstant().toEpochMilli();
            if (time > now && time < earliest) earliest = time;
        }
        if (earliest != Long.MAX_VALUE) alarms.setAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, earliest, intent);
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
