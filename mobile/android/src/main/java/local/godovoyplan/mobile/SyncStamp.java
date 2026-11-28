package local.godovoyplan.mobile;

import org.json.JSONException;
import org.json.JSONObject;
import java.time.OffsetDateTime;
import java.time.format.DateTimeParseException;
import java.util.Iterator;
import java.util.UUID;

/** Applies a token-bound timestamp-only response without copying the calendar rows. */
final class SyncStamp {
    private SyncStamp() {}

    static JSONObject apply(JSONObject prior, JSONObject update, int year, String token) {
        Object nextToken = update.opt("stateToken");
        Object stamp = update.opt("lastSync");
        if (!Boolean.TRUE.equals(update.opt("syncStamp")) || update.has("unchanged") ||
                !(update.opt("year") instanceof Integer) || update.optInt("year") != year ||
                prior.optInt("year") != year || !token.equals(prior.opt("stateToken")) ||
                !token.equals(update.opt("baseToken")) || !(nextToken instanceof String) ||
                token.equals(nextToken) || !update.has("lastSync")) return null;
        try {
            if (!UUID.fromString((String)nextToken).toString().equals(nextToken)) return null;
            if (stamp != JSONObject.NULL) {
                if (!(stamp instanceof String)) return null;
                OffsetDateTime.parse((String)stamp);
            }
            JSONObject result = new JSONObject();
            Iterator<String> keys = prior.keys();
            while (keys.hasNext()) {
                String name = keys.next();
                result.put(name, prior.get(name));
            }
            result.put("lastSync", stamp);
            result.put("stateToken", nextToken);
            return result;
        } catch (IllegalArgumentException | DateTimeParseException | JSONException invalid) {
            return null;
        }
    }
}
