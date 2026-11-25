package local.godovoyplan.mobile;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

public final class ReminderReceiver extends BroadcastReceiver {
    @Override public void onReceive(Context context, Intent intent) {
        if (ReminderSchedule.ALARM.equals(intent.getAction())) ReminderSchedule.deliver(context);
        else ReminderSchedule.rearm(context);
    }
}
