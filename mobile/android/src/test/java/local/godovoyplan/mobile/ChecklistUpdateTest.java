package local.godovoyplan.mobile;

import org.json.JSONArray;
import org.json.JSONObject;

/** Standalone contract checks; run with Android's implementation of org.json. */
public final class ChecklistUpdateTest {
    private static final String EVENT = "00000000-0000-4000-8000-000000000001";
    private static final String TASK = "00000000-0000-4000-8000-000000000002";
    private static final String BASE = "00000000-0000-4000-8000-000000000003";
    private static final String NEXT = "00000000-0000-4000-8000-000000000004";
    private static final int YEAR = 2027;

    private static JSONObject row(boolean done) throws Exception {
        JSONObject task = new JSONObject().put("id", TASK).put("done", done);
        return new JSONObject().put("id", EVENT).put("year", YEAR).put("date", "2027-01-01")
                .put("checklist", new JSONArray().put(task));
    }

    private static JSONObject view(int eventCount) throws Exception {
        JSONArray events = new JSONArray().put(row(false));
        for (int i = 1; i < eventCount; i++) events.put(new JSONObject().put("id", "other" + i));
        return new JSONObject().put("year", YEAR).put("stateToken", BASE).put("supportsCheckPatch", true)
                .put("deviceId", EVENT).put("employeeId", TASK).put("workspaceName", "Группа")
                .put("canManage", false).put("members", new JSONArray()).put("employees", new JSONArray())
                .put("notificationDates", new JSONArray()).put("events", events)
                .put("tasks", new JSONArray().put(row(false)).put(new JSONObject().put("id", NEXT)))
                .put("reminders", new JSONArray().put(new JSONObject().put("eventId", EVENT)));
    }

    private static JSONObject patch() throws Exception {
        return view(1).put("checkPatch", true).put("baseToken", BASE).put("stateToken", NEXT)
                .put("events", new JSONArray().put(row(true))).put("tasks", new JSONArray().put(row(true)))
                .put("taskPositions", new JSONArray().put(1)).put("taskCount", 2)
                .put("reminders", new JSONArray()).put("reminderPositions", new JSONArray()).put("reminderCount", 0);
    }

    private static void require(boolean condition) {
        if (!condition) throw new AssertionError("Unexpected checklist update result");
    }

    public static void main(String[] args) throws Exception {
        JSONObject command = new JSONObject().put("op", "check").put("year", YEAR).put("eventId", EVENT)
                .put("taskId", TASK).put("done", true);
        require(ChecklistUpdate.forCommand(view(599), command) == null);
        require(ChecklistUpdate.forCommand(view(600).put("supportsCheckPatch", false), command) == null);
        JSONObject before = view(600);
        ChecklistUpdate change = ChecklistUpdate.forCommand(before, command);
        require(change != null && change.matches(before, YEAR, BASE));
        require(!change.matches(view(600), YEAR, BASE));
        require(!change.matches(before, YEAR + 1, BASE) && !change.matches(before, YEAR, NEXT));
        require(change.merge(patch().put("year", "2027")) == null);
        require(change.merge(patch().put("checkPatch", "true")) == null);
        require(change.merge(patch().put("deviceId", NEXT)) == null);
        require(change.merge(patch().put("employeeId", NEXT)) == null);
        require(change.merge(patch().put("baseToken", NEXT)) == null);
        require(change.merge(patch().put("stateToken", BASE)) == null);
        require(change.merge(patch().put("events", new JSONArray().put(row(false)))) == null);
        require(change.merge(patch().put("events", new JSONArray().put(row(true).put("date", "2027-02-01")))) == null);
        require(change.merge(patch().put("events", new JSONArray().put(row(true).put("id", NEXT)))) == null);
        require(change.merge(patch().put("taskPositions", new JSONArray().put(2))) == null);
        require(change.merge(patch().put("taskPositions", new JSONArray().put("1"))) == null);
        require(change.merge(patch().put("reminderCount", -1)) == null);
        JSONObject merged = change.merge(patch());
        require(merged != null && !merged.has("baseToken") && !merged.has("checkPatch") && NEXT.equals(merged.get("stateToken")));
        require(merged.getJSONArray("events").length() == 600 && merged.getJSONArray("reminders").length() == 0);
        require(merged.getJSONArray("events").get(1) == before.getJSONArray("events").get(1));
        require(merged.getJSONArray("tasks").get(0) == before.getJSONArray("tasks").get(1));
        require(EVENT.equals(merged.getJSONArray("tasks").getJSONObject(1).get("id")));
        require(!before.getJSONArray("events").getJSONObject(0).getJSONArray("checklist").getJSONObject(0).getBoolean("done"));
        System.out.println("Checklist snapshot, ordering, threshold and fallback contracts passed.");
    }
}
