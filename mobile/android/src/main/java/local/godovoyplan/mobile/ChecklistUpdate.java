package local.godovoyplan.mobile;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.util.UUID;

/** One conditional update of the Android view, tied to the snapshot used to render the checkbox. */
final class ChecklistUpdate {
    private static final int MIN_VISIBLE_EVENTS = 600;

    private final JSONObject snapshot;
    final String token;
    private final int year;
    private final String eventId;
    private final String taskId;
    private final boolean done;

    private ChecklistUpdate(JSONObject snapshot, String token, int year, String eventId, String taskId, boolean done) {
        this.snapshot = snapshot;
        this.token = token;
        this.year = year;
        this.eventId = eventId;
        this.taskId = taskId;
        this.done = done;
    }

    static ChecklistUpdate forCommand(JSONObject snapshot, JSONObject command) {
        JSONArray events = snapshot.optJSONArray("events");
        Object year = snapshot.opt("year");
        if (events == null || events.length() < MIN_VISIBLE_EVENTS ||
                !Boolean.TRUE.equals(snapshot.opt("supportsCheckPatch")) ||
                !"check".equals(command.opt("op")) || !(year instanceof Integer) ||
                !year.equals(command.opt("year")) || !(command.opt("done") instanceof Boolean)) {
            return null;
        }
        String token = uuid(snapshot.opt("stateToken"));
        String event = uuid(command.opt("eventId"));
        String task = uuid(command.opt("taskId"));
        if (token == null || event == null || task == null) return null;
        return new ChecklistUpdate(snapshot, token, (Integer) year, event, task, (Boolean) command.opt("done"));
    }

    boolean matches(JSONObject current, int currentYear, String currentToken) {
        return snapshot == current && year == currentYear && token.equals(currentToken);
    }

    private static String uuid(Object value) {
        if (!(value instanceof String)) return null;
        String text = (String) value;
        try {
            return UUID.fromString(text).toString().equals(text) ? text : null;
        } catch (IllegalArgumentException invalid) {
            return null;
        }
    }

    // Android's getString/getInt coerce JSON values. Conditional merges require exact types.
    private static boolean sameString(JSONObject left, JSONObject right, String key) {
        Object value = left.opt(key);
        return value instanceof String && value.equals(right.opt(key));
    }

    private boolean confirmsTask(JSONObject row) throws JSONException {
        JSONArray checklist = row.getJSONArray("checklist");
        boolean found = false;
        for (int index = 0; index < checklist.length(); index++) {
            JSONObject task = checklist.getJSONObject(index);
            if (taskId.equals(task.opt("id"))) {
                if (found || !Boolean.valueOf(done).equals(task.opt("done"))) return false;
                found = true;
            }
        }
        return found;
    }

    private JSONArray replaceEvent(JSONObject patch) throws JSONException {
        JSONArray updates = patch.getJSONArray("events");
        if (updates.length() != 1) return null;
        JSONObject update = updates.getJSONObject(0);
        if (!eventId.equals(update.opt("id")) || !Integer.valueOf(year).equals(update.opt("year")) ||
                !confirmsTask(update)) return null;

        JSONArray events = snapshot.getJSONArray("events");
        JSONArray result = new JSONArray();
        boolean found = false;
        for (int index = 0; index < events.length(); index++) {
            JSONObject previous = events.getJSONObject(index);
            if (eventId.equals(previous.opt("id"))) {
                if (found || !Integer.valueOf(year).equals(previous.opt("year")) ||
                        !sameString(previous, update, "date")) return null;
                result.put(update);
                found = true;
            } else {
                result.put(previous);
            }
        }
        return found ? result : null;
    }

    private JSONArray replaceRelatedRows(JSONObject patch, String field, String positionKey,
                                         String countKey, String eventKey) throws JSONException {
        JSONArray previous = snapshot.getJSONArray(field);
        JSONArray replacements = patch.getJSONArray(field);
        JSONArray positions = patch.getJSONArray(positionKey);
        Object count = patch.get(countKey);
        if (!(count instanceof Integer) || positions.length() != replacements.length()) return null;
        int size = (Integer) count;
        if (size < 0 || size > (long) previous.length() + replacements.length()) return null;

        JSONArray result = new JSONArray();
        int original = 0;
        int replacement = 0;
        for (int destination = 0; destination < size; destination++) {
            if (replacement < replacements.length()) {
                Object position = positions.get(replacement);
                if (!(position instanceof Integer) || (Integer) position < destination) return null;
                if ((Integer) position == destination) {
                    JSONObject row = replacements.getJSONObject(replacement++);
                    if (!eventId.equals(row.opt(eventKey)) || !(row.opt("year") instanceof Integer)) return null;
                    result.put(row);
                    continue;
                }
            }
            while (original < previous.length() && eventId.equals(previous.getJSONObject(original).opt(eventKey))) {
                original++;
            }
            if (original == previous.length()) return null;
            result.put(previous.getJSONObject(original++));
        }
        while (original < previous.length() && eventId.equals(previous.getJSONObject(original).opt(eventKey))) {
            original++;
        }
        return original == previous.length() && replacement == replacements.length() ? result : null;
    }

    /** Runs on the core worker. Only the unshared incoming message is modified, after all checks. */
    JSONObject merge(JSONObject patch) {
        String nextToken = uuid(patch.opt("stateToken"));
        if (!Boolean.TRUE.equals(patch.opt("checkPatch")) || patch.has("syncStamp") || patch.has("unchanged") ||
                !token.equals(patch.opt("baseToken")) || nextToken == null || token.equals(nextToken) ||
                !Integer.valueOf(year).equals(patch.opt("year")) || !sameString(snapshot, patch, "deviceId") ||
                !sameString(snapshot, patch, "employeeId") || !sameString(snapshot, patch, "workspaceName") ||
                !Boolean.TRUE.equals(patch.opt("supportsCheckPatch")) ||
                !(patch.opt("canManage") instanceof Boolean) || !patch.opt("canManage").equals(snapshot.opt("canManage")) ||
                patch.optJSONArray("members") == null || patch.optJSONArray("employees") == null ||
                patch.optJSONArray("notificationDates") == null) return null;
        try {
            JSONArray events = replaceEvent(patch);
            JSONArray tasks = replaceRelatedRows(patch, "tasks", "taskPositions", "taskCount", "id");
            JSONArray reminders = replaceRelatedRows(patch, "reminders", "reminderPositions", "reminderCount", "eventId");
            if (events == null || tasks == null || reminders == null) return null;
            patch.put("events", events);
            patch.put("tasks", tasks);
            patch.put("reminders", reminders);
            patch.remove("baseToken");
            patch.remove("checkPatch");
            patch.remove("taskCount");
            patch.remove("taskPositions");
            patch.remove("reminderCount");
            patch.remove("reminderPositions");
            return patch;
        } catch (JSONException malformed) {
            return null;
        }
    }
}
