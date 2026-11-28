package local.godovoyplan.mobile;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;
import java.util.ArrayList;
import java.util.List;

/** Checklist fields carried in either the object API or the negotiated tuple-v1 layout. */
final class ChecklistRows {
    static final int ID = 0, TITLE = 1, DATE = 2, DAYS = 3, DONE = 4;
    private static final String[] KEYS = {"id", "title", "dueDate", "daysBefore", "done"};

    static Object field(Object task, int position) {
        if (task instanceof JSONObject) return ((JSONObject) task).opt(KEYS[position]);
        if (task instanceof JSONArray && ((JSONArray) task).length() == KEYS.length) {
            return ((JSONArray) task).opt(position);
        }
        return null;
    }

    static boolean validTuple(Object task) {
        return task instanceof JSONArray && field(task, ID) instanceof String &&
                field(task, TITLE) instanceof String && field(task, DATE) instanceof String &&
                field(task, DAYS) instanceof Integer && field(task, DONE) instanceof Boolean;
    }

    static JSONObject object(Object task) {
        if (task instanceof JSONObject) return (JSONObject) task;
        if (!validTuple(task)) return null;
        JSONObject row = new JSONObject();
        try {
            for (int i = 0; i < KEYS.length; i++) row.put(KEYS[i], field(task, i));
        } catch (JSONException malformed) { return null; }
        return row;
    }

    static List<JSONObject> forCard(JSONArray tasks) {
        List<JSONObject> rows = new ArrayList<>(tasks.length());
        for (int i = 0; i < tasks.length(); i++) {
            JSONObject row = object(tasks.opt(i));
            if (row != null) rows.add(row);
        }
        return rows;
    }
}
