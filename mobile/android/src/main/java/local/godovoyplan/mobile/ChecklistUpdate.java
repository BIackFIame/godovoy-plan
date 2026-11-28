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
    final boolean indexed;
    private JSONObject confirmedView;
    private JSONObject confirmedCard;

    private ChecklistUpdate(JSONObject snapshot, String token, int year, String eventId, String taskId, boolean done) {
        this.snapshot = snapshot;
        this.token = token;
        this.year = year;
        this.eventId = eventId;
        this.taskId = taskId;
        this.done = done;
        this.indexed = Boolean.TRUE.equals(snapshot.opt("agendaIndex"));
    }

    static ChecklistUpdate forCommand(JSONObject snapshot, JSONObject command) {
        JSONArray events = snapshot.optJSONArray("events");
        Object year = snapshot.opt("year");
        boolean indexed = Boolean.TRUE.equals(snapshot.opt("agendaIndex"));
        boolean eligible = indexed ? Boolean.TRUE.equals(snapshot.opt("supportsIndexCheckPatch")) :
                events != null && events.length() >= MIN_VISIBLE_EVENTS;
        if (!eligible || events == null ||
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

    JSONObject cardFor(JSONObject view, String selectedId, int selectedYear) {
        return confirmedView == view && eventId.equals(selectedId) && year == selectedYear ? confirmedCard : null;
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
        boolean tuples = !indexed && "tuple-v1".equals(snapshot.opt("checklistFormat"));
        for (int index = 0; index < checklist.length(); index++) {
            Object task = checklist.get(index);
            if (tuples ? !ChecklistRows.validTuple(task) : !(task instanceof JSONObject)) return false;
            if (indexed && (uuid(ChecklistRows.field(task, ChecklistRows.ID)) == null ||
                    !(ChecklistRows.field(task, ChecklistRows.TITLE) instanceof String) ||
                    !(ChecklistRows.field(task, ChecklistRows.DATE) instanceof String) ||
                    !(ChecklistRows.field(task, ChecklistRows.DAYS) instanceof Integer) ||
                    !(ChecklistRows.field(task, ChecklistRows.DONE) instanceof Boolean))) return false;
            if (taskId.equals(ChecklistRows.field(task, ChecklistRows.ID))) {
                if (found || !Boolean.valueOf(done).equals(ChecklistRows.field(task, ChecklistRows.DONE))) return false;
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
                if (indexed && (!sameString(previous, update, "title") ||
                        !sameNullableId(previous, update, "ownerId") || !sameNullableId(previous, update, "employeeId"))) return null;
                result.put(indexed ? previous : update);
                found = true;
            } else {
                result.put(previous);
            }
        }
        return found ? result : null;
    }

    private static boolean sameNullableId(JSONObject a, JSONObject b, String name) {
        Object id = a.opt(name);
        return (id == JSONObject.NULL || uuid(id) != null) && id.equals(b.opt(name));
    }

    private boolean validPersonalSummary(JSONObject row) {
        if (!sameString(snapshot,row,"employeeId") || uuid(row.opt("ownerId")) == null ||
                !(row.opt("title") instanceof String) || !(row.opt("date") instanceof String) ||
                row.has("checklist")) return false;
        if (!row.has("nextOpenTask")) return true;
        JSONObject task = row.optJSONObject("nextOpenTask");
        return task != null && uuid(task.opt("id")) != null && task.opt("title") instanceof String &&
                task.opt("dueDate") instanceof String;
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
        java.util.HashSet<Integer> updatedYears = indexed ? new java.util.HashSet<>() : null;
        int original = 0;
        int replacement = 0;
        for (int destination = 0; destination < size; destination++) {
            if (replacement < replacements.length()) {
                Object position = positions.get(replacement);
                if (!(position instanceof Integer) || (Integer) position < destination) return null;
                if ((Integer) position == destination) {
                    JSONObject row = replacements.getJSONObject(replacement++);
                    if (!eventId.equals(row.opt(eventKey)) || !(row.opt("year") instanceof Integer)) return null;
                    if (indexed && (!validPersonalSummary(row) || !updatedYears.add(row.getInt("year")))) return null;
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
        Object format = snapshot.opt("checklistFormat");
        if ((format != null && !"tuple-v1".equals(format)) || !java.util.Objects.equals(format, patch.opt("checklistFormat")) ||
                !Boolean.TRUE.equals(patch.opt("checkPatch")) || patch.has("syncStamp") || patch.has("unchanged") ||
                !token.equals(patch.opt("baseToken")) || nextToken == null || token.equals(nextToken) ||
                !Integer.valueOf(year).equals(patch.opt("year")) || !sameString(snapshot, patch, "deviceId") ||
                !sameString(snapshot, patch, "employeeId") || !sameString(snapshot, patch, "workspaceName") ||
                !Boolean.TRUE.equals(patch.opt("supportsCheckPatch")) ||
                !(patch.opt("canManage") instanceof Boolean) || !patch.opt("canManage").equals(snapshot.opt("canManage")) ||
                patch.optJSONArray("members") == null || patch.optJSONArray("employees") == null ||
                patch.optJSONArray("notificationDates") == null) return null;
        if (indexed != Boolean.TRUE.equals(patch.opt("agendaIndex"))) return null;
        try {
            JSONArray reminders;
            JSONObject card = null;
            if (indexed) {
                reminders = patch.getJSONArray("reminders");
                Object count = patch.opt("reminderCount");
                if (!Boolean.TRUE.equals(patch.opt("supportsIndexCheckPatch")) ||
                        !(count instanceof Integer) || (Integer) count < 0 ||
                        reminders.length() != 0 || patch.has("reminderPositions")) return null;
                card = patch.getJSONArray("events").getJSONObject(0);
            } else {
                reminders = replaceRelatedRows(patch, "reminders", "reminderPositions", "reminderCount", "eventId");
            }
            JSONArray events = replaceEvent(patch);
            JSONArray tasks = replaceRelatedRows(patch, "tasks", "taskPositions", "taskCount", "id");
            if (events == null || tasks == null || reminders == null) return null;
            patch.put("events", events);
            patch.put("tasks", tasks);
            patch.put("reminders", reminders);
            patch.remove("baseToken");
            patch.remove("checkPatch");
            patch.remove("taskCount");
            patch.remove("taskPositions");
            if (!indexed) {
                patch.remove("reminderCount");
                patch.remove("reminderPositions");
            }
            confirmedCard = card;
            confirmedView = patch;
            return patch;
        } catch (JSONException malformed) {
            return null;
        }
    }
}
