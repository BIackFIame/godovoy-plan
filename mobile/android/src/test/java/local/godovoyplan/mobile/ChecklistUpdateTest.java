package local.godovoyplan.mobile;

import org.json.JSONArray;
import org.json.JSONObject;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

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

    private static JSONObject indexedView() throws Exception {
        JSONObject summary = new JSONObject().put("id",EVENT).put("year",YEAR).put("title","Проверка")
                .put("date","2027-01-01").put("ownerId",TASK).put("employeeId",TASK);
        return view(1).put("agendaIndex",true).put("supportsIndexCheckPatch",true)
                .put("events",new JSONArray().put(summary)).put("tasks",new JSONArray().put(summary))
                .put("reminders",new JSONArray()).put("reminderCount",12);
    }

    private static JSONObject indexedPatch() throws Exception {
        JSONObject patch = indexedView().put("checkPatch",true).put("baseToken",BASE).put("stateToken",NEXT)
                .put("taskCount",1).put("taskPositions",new JSONArray().put(0)).put("reminderCount",11);
        JSONObject card = new JSONObject(patch.getJSONArray("events").getJSONObject(0).toString());
        card.put("checklist",new JSONArray().put(new JSONObject().put("id",TASK).put("done",true)
                .put("dueDate","2027-01-01").put("daysBefore",0).put("title","Подписать")));
        patch.put("events",new JSONArray().put(card));
        return patch;
    }

    private static boolean sameJson(Object left, Object right) throws Exception {
        if (left instanceof JSONObject && right instanceof JSONObject) {
            JSONObject a = (JSONObject) left, b = (JSONObject) right;
            if (a.length() != b.length()) return false;
            for (java.util.Iterator<String> keys = a.keys(); keys.hasNext();) {
                String key = keys.next();
                if (!b.has(key) || !sameJson(a.get(key), b.get(key))) return false;
            }
            return true;
        }
        if (left instanceof JSONArray && right instanceof JSONArray) {
            JSONArray a = (JSONArray) left, b = (JSONArray) right;
            if (a.length() != b.length()) return false;
            for (int i = 0; i < a.length(); i++) if (!sameJson(a.get(i), b.get(i))) return false;
            return true;
        }
        return left.equals(right);
    }

    private static void compareWithCore(String path) throws Exception {
        byte[] bytes = java.nio.file.Files.readAllBytes(java.nio.file.Paths.get(path));
        JSONObject bundle = new JSONObject(new String(bytes, java.nio.charset.StandardCharsets.UTF_8));
        checkLimits(bundle.getJSONObject("limits"));
        syncStamp(bundle.getJSONObject("syncStamp"));
        JSONArray cases = bundle.getJSONArray("cases");
        require(cases.length() == 2);
        for (int i = 0; i < cases.length(); i++) {
            JSONObject test = cases.getJSONObject(i);
            ChecklistUpdate change = ChecklistUpdate.forCommand(test.getJSONObject("before"), test.getJSONObject("command"));
            require(change != null);
            JSONObject patch = test.getJSONObject("patch");
            require(sameJson(test.getJSONObject("expected"), change.merge(patch)));
            JSONArray invalid = test.getJSONArray("invalidPatches");
            require(invalid.length() >= 6);
            for (int j = 0; j < invalid.length(); j++) {
                require(change.merge(invalid.getJSONObject(j).getJSONObject("patch")) == null);
            }
            patch.put("baseToken", patch.getString("stateToken"));
            require(change.merge(patch) == null);
        }
    }

    private static void checkLimits(JSONObject limits) throws Exception {
        require(limits.getInt("maxTasks") == MobileLimits.MAX_TASKS);
        require(limits.getInt("maxTitleChars") == MobileLimits.MAX_TITLE_CHARS);
        require(limits.getInt("maxDescriptionChars") == MobileLimits.MAX_DESCRIPTION_CHARS);
        require(limits.getInt("maxNoteChars") == MobileLimits.MAX_NOTE_CHARS);
        require(limits.getInt("maxPreparationDays") == MobileLimits.MAX_PREPARATION_DAYS);
        require(limits.getLong("maxExchangeBytes") == MobileLimits.MAX_EXCHANGE_BYTES);
        require(MobileLimits.withinTitleLimit(repeat("🙂", 160)));
        require(!MobileLimits.withinTitleLimit(repeat("🙂", 161)));
        require(MobileLimits.withinDescriptionLimit(repeat("е́", 2000)));
        require(!MobileLimits.withinDescriptionLimit(repeat("е́", 2001)));
        require(MobileLimits.withinExchangeLimit(9L * 1024 * 1024));
        require(!MobileLimits.withinExchangeLimit(9L * 1024 * 1024 + 1));
        require(!MobileLimits.withinExchangeLimit(-1));
        try {
            CountingOutput accepted = new CountingOutput();
            MobileLimits.copyExchange(new RepeatedInput(MobileLimits.MAX_EXCHANGE_BYTES), accepted);
            require(accepted.count == MobileLimits.MAX_EXCHANGE_BYTES);
            try {
                MobileLimits.copyExchange(new RepeatedInput(MobileLimits.MAX_EXCHANGE_BYTES + 1), new CountingOutput());
                require(false);
            } catch (IOException expected) { }
        } catch (IOException failedAtLimit) {
            throw new AssertionError("The exchange reader rejected exactly 9 MiB", failedAtLimit);
        }
    }

    private static final class RepeatedInput extends InputStream {
        private int remaining;
        RepeatedInput(int bytes) { remaining = bytes; }
        @Override public int read() { if (remaining == 0) return -1; remaining--; return 0; }
        @Override public int read(byte[] bytes, int offset, int length) {
            if (remaining == 0) return -1;
            int count = Math.min(remaining, length);
            java.util.Arrays.fill(bytes, offset, offset + count, (byte) 0);
            remaining -= count;
            return count;
        }
    }

    private static final class CountingOutput extends OutputStream {
        long count;
        @Override public void write(int value) { count++; }
        @Override public void write(byte[] bytes, int offset, int length) { count += length; }
    }

    private static String repeat(String value, int count) {
        StringBuilder result = new StringBuilder(value.length() * count);
        for (int i = 0; i < count; i++) result.append(value);
        return result.toString();
    }

    private static JSONObject clone(JSONObject value) throws Exception {
        return new JSONObject(value.toString());
    }

    private static void syncStamp(JSONObject fixture) throws Exception {
        JSONObject prior = fixture.getJSONObject("prior");
        JSONObject update = fixture.getJSONObject("update");
        String token = prior.getString("stateToken");
        int year = prior.getInt("year");
        JSONObject result = SyncStamp.apply(prior, update, year, token);
        require(result != null && result.getString("stateToken").equals(update.getString("stateToken")));
        require(result.getString("lastSync").equals(update.getString("lastSync")));
        require(result.getJSONArray("events") == prior.getJSONArray("events"));
        require(result.getJSONArray("tasks") == prior.getJSONArray("tasks"));

        JSONObject nullStamp = clone(update).put("lastSync", JSONObject.NULL).put("stateToken", NEXT);
        require(SyncStamp.apply(prior, nullStamp, year, token) != null);
        require(SyncStamp.apply(prior, clone(update).put("year", year + 1), year, token) == null);
        require(SyncStamp.apply(prior, clone(update).put("baseToken", NEXT), year, token) == null);
        require(SyncStamp.apply(prior, clone(update).put("stateToken", token), year, token) == null);
        require(SyncStamp.apply(prior, clone(update).put("stateToken", "not-a-uuid"), year, token) == null);
        require(SyncStamp.apply(prior, clone(update).put("lastSync", "not-a-date"), year, token) == null);
        require(SyncStamp.apply(prior, clone(update).put("lastSync", 1), year, token) == null);
        JSONObject missingStamp = clone(update);
        missingStamp.remove("lastSync");
        require(SyncStamp.apply(prior, missingStamp, year, token) == null);
        require(SyncStamp.apply(prior, clone(update).put("unchanged", true), year, token) == null);
    }

    public static void main(String[] args) throws Exception {
        if (args.length > 0) compareWithCore(args[0]);
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
        require(ChecklistUpdate.forCommand(view(600).put("agendaIndex", true), command) == null);
        JSONObject compact = view(600).put("checklistFormat", "tuple-v1");
        ChecklistUpdate compactChange = ChecklistUpdate.forCommand(compact, command);
        JSONArray tuple = new JSONArray().put(TASK).put("Партнёры").put("2027-01-01").put(7).put(true);
        require(ChecklistRows.validTuple(tuple));
        JSONObject decoded = ChecklistRows.forCard(new JSONArray().put(tuple)).get(0);
        require(decoded.getString("title").equals("Партнёры") && decoded.getInt("daysBefore")==7 && decoded.getBoolean("done"));
        JSONObject packedPatch = patch().put("checklistFormat","tuple-v1");
        for (String group : new String[]{"events","tasks"}) {
            packedPatch.getJSONArray(group).getJSONObject(0).put("checklist",new JSONArray().put(tuple));
        }
        require(compactChange.merge(new JSONObject(packedPatch.toString()).put("checklistFormat","tuple-v2")) == null);
        require(compactChange.merge(patch()) == null);
        JSONObject wrong = new JSONObject(packedPatch.toString());
        wrong.getJSONArray("events").getJSONObject(0).getJSONArray("checklist").getJSONArray(0).put(4,"true");
        require(compactChange.merge(wrong) == null);
        require(compactChange.merge(packedPatch) != null);
        JSONObject index = indexedView();
        ChecklistUpdate indexChange = ChecklistUpdate.forCommand(index,command);
        require(indexChange != null && indexChange.indexed);
        require(indexChange.merge(patch()) == null);
        JSONObject differentOwner = indexedPatch();
        differentOwner.getJSONArray("events").getJSONObject(0).put("ownerId",NEXT);
        require(indexChange.merge(differentOwner) == null);
        JSONObject foreignTask = indexedPatch();
        foreignTask.getJSONArray("tasks").getJSONObject(0).put("employeeId",NEXT);
        require(indexChange.merge(foreignTask) == null);
        require(indexChange.merge(indexedPatch().put("reminders",new JSONArray().put(row(true)))) == null);
        require(indexChange.merge(indexedPatch().put("reminderCount",-1)) == null);
        require(indexChange.merge(indexedPatch().put("reminderPositions",new JSONArray())) == null);
        JSONObject duplicate = indexedPatch();
        duplicate.getJSONArray("tasks").put(duplicate.getJSONArray("tasks").get(0));
        duplicate.put("taskCount",2).getJSONArray("taskPositions").put(1);
        require(indexChange.merge(duplicate) == null);
        JSONObject confirmed = indexChange.merge(indexedPatch());
        require(confirmed != null && confirmed.getInt("reminderCount")==11 && !confirmed.has("taskPositions"));
        require(confirmed.getJSONArray("events").get(0)==index.getJSONArray("events").get(0));
        require(indexChange.cardFor(index,EVENT,YEAR)==null && indexChange.cardFor(confirmed,NEXT,YEAR)==null);
        require(indexChange.cardFor(confirmed,EVENT,YEAR+1)==null);
        require(indexChange.cardFor(confirmed,EVENT,YEAR).getJSONArray("checklist").getJSONObject(0).getBoolean("done"));
        System.out.println("Checklist snapshot, ordering, threshold and fallback contracts passed.");
    }
}
