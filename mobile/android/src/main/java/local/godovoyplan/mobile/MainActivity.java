package local.godovoyplan.mobile;

import static local.godovoyplan.mobile.CalendarApplication.message;

import android.Manifest;
import android.app.Activity;
import android.app.AlertDialog;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.res.ColorStateList;
import android.graphics.Typeface;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.text.InputType;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowManager;
import android.widget.*;
import java.time.LocalDate;
import java.time.format.DateTimeFormatter;
import java.util.*;
import org.json.JSONArray;
import org.json.JSONObject;

public final class MainActivity extends Activity {
    static final int INK = 0xff232923, MUTED = 0xff546259, BLUE = 0xff2455aa, BG = 0xfff7f8f5;
    private static final int PICK = 10, WRITE = 11, ALERTS = 12;
    private static final String[] MONTHS = {"Январь", "Февраль", "Март", "Апрель", "Май", "Июнь", "Июль",
            "Август", "Сентябрь", "Октябрь", "Ноябрь", "Декабрь"};
    private static final String[] TABS = {"Год", "Задачи", "Напоминания", "Доступ"};
    static final String[] CATS = {"Campaign", "Partners", "Team", "Operations"},
                          CAT_NAMES = {"Акции", "Партнёры", "Команда", "Операции"};
    private static final DateTimeFormatter DISPLAY =
            DateTimeFormatter.ofPattern("d MMMM yyyy", new Locale("ru"));
    private final Handler clock = new Handler(Looper.getMainLooper());
    private CalendarApplication app;
    private LinearLayout root, body;
    JSONObject state = new JSONObject();
    private String section = "Год", selectedId = "", stateKey = "";
    int year = LocalDate.now().getYear();
    private int month, outstanding;
    private boolean active, loading;
    private AlertDialog dialog;
    private TextView status;
    private ListView list;
    private int firstVisible, topOffset;
    private final Runnable poll = new Runnable() {
        public void run() {
            if (!active)
                return;
            if (!loading && outstanding == 0)
                refresh(false);
            clock.postDelayed(this, 12000);
        }
    };

    @Override
    public void onCreate(Bundle saved) {
        super.onCreate(saved);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_SECURE);
        app = (CalendarApplication) getApplication();
        if (saved != null) {
            year = saved.getInt("year", year);
            month = saved.getInt("month");
            section = saved.getString("tab", "Год");
            selectedId = saved.getString("event", "");
        }
        root = vertical();
        root.setBackgroundColor(BG);
        setContentView(root);
        root.addView(text("Открываем календарь…", 17, false));
    }
    @Override
    protected void onSaveInstanceState(Bundle s) {
        super.onSaveInstanceState(s);
        s.putInt("year", year);
        s.putInt("month", month);
        s.putString("tab", section);
        s.putString("event", selectedId);
    }
    @Override
    public void onStart() {
        super.onStart();
        active = true;
        app.call(message("foreground"), null);
        refresh(true);
        clock.postDelayed(poll, 12000);
    }
    @Override
    public void onStop() {
        active = false;
        clock.removeCallbacks(poll);
        app.call(message("background"), null);
        super.onStop();
    }
    boolean alive() {
        return !isFinishing() && !isDestroyed();
    }
    void feedback(String msg) {
        if (alive())
            Toast.makeText(this, msg, Toast.LENGTH_LONG).show();
    }
    int dp(float d) {
        return Math.round(getResources().getDisplayMetrics().density * d);
    }
    LinearLayout vertical() {
        LinearLayout l = new LinearLayout(this);
        l.setOrientation(LinearLayout.VERTICAL);
        return l;
    }
    LinearLayout horizontal() {
        LinearLayout l = new LinearLayout(this);
        l.setGravity(Gravity.CENTER_VERTICAL);
        return l;
    }
    void pad(View v, int x, int y) {
        v.setPadding(dp(x), dp(y), dp(x), dp(y));
    }
    void fill(LinearLayout row, View v) {
        row.addView(v, new LinearLayout.LayoutParams(0, -2, 1));
    }
    TextView text(String label, int size, boolean bold) {
        TextView t = new TextView(this);
        t.setText(label);
        t.setTextSize(size);
        t.setTextColor(bold ? INK : MUTED);
        if (bold)
            t.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
        return t;
    }
    Button button(String label, Runnable onClick) {
        Button b = new Button(this, null, android.R.attr.borderlessButtonStyle);
        b.setText(label);
        b.setTextSize(15);
        b.setTextColor(BLUE);
        b.setAllCaps(false);
        b.setMinWidth(0);
        b.setMinimumWidth(0);
        b.setMinimumHeight(dp(48));
        b.setMinHeight(dp(48));
        b.setOnClickListener(v -> onClick.run());
        return b;
    }
    void line(LinearLayout parent) {
        View v = new View(this);
        v.setBackgroundColor(0xffd8ded8);
        parent.addView(v, new LinearLayout.LayoutParams(-1, dp(1)));
    }
    void label(LinearLayout parent, String title, int size) {
        TextView t = text(title, size, true);
        pad(t, 0, 7);
        parent.addView(t);
    }
    void hint(LinearLayout parent, String msg) {
        TextView t = text(msg, 15, false);
        pad(t, 0, 6);
        parent.addView(t);
    }
    EditText field(LinearLayout parent, String title, String value, boolean multi) {
        TextView caption = text(title, 14, true);
        parent.addView(caption);
        EditText f = new EditText(this);
        f.setTextSize(16);
        f.setSingleLine(!multi);
        f.setText(value);
        if (multi) {
            f.setMinLines(2);
            f.setGravity(Gravity.TOP);
        }
        f.setId(View.generateViewId());
        caption.setLabelFor(f.getId());
        parent.addView(f);
        return f;
    }
    static String value(EditText f) {
        return f.getText().toString().trim();
    }
    static String str(JSONObject o, String key) {
        return o == null || o.isNull(key) ? "" : o.optString(key, "");
    }
    static JSONArray array(JSONObject o, String key) {
        JSONArray a = o == null ? null : o.optJSONArray(key);
        return a == null ? new JSONArray() : a;
    }
    static String pretty(String iso) {
        try {
            return LocalDate.parse(iso).format(DISPLAY);
        } catch (Exception ignored) {
            return iso;
        }
    }
    static String catName(String code) {
        int i = Arrays.asList(CATS).indexOf(code);
        return i < 0 ? code : CAT_NAMES[i];
    }
    String owner(String id) {
        for (JSONObject m : items(array(state, "members")))
            if (id.equals(str(m, "id")))
                return str(m, "name");
        return "";
    }
    static List<JSONObject> items(JSONArray a) {
        List<JSONObject> out = new ArrayList<>();
        for (int i = 0; i < a.length(); i++) {
            JSONObject o = a.optJSONObject(i);
            if (o != null)
                out.add(o);
        }
        return out;
    }

    private void refresh(boolean force) {
        final int expected = year;
        loading = true;
        String token = str(state, "stateToken");
        JSONObject request = force || token.isEmpty() || state.optInt("year") != year
                ? message("state", "year", year) : message("state", "year", year, "ifToken", token);
        app.call(request, (data, error) -> {
            if (!alive())
                return;
            loading = false;
            if (error != null) {
                if (outstanding == 0)
                    showError(error);
                return;
            }
            if (expected != year || !(data instanceof JSONObject))
                return;
            JSONObject next = (JSONObject) data;
            if (next.optBoolean("unchanged")) return;
            String key = str(next, "stateToken");
            if (key.isEmpty()) key = next.toString();
            if (force || !key.equals(stateKey)) {
                state = next;
                stateKey = key;
                ReminderSchedule.refresh(this, state.optJSONArray("notificationDates"));
                if (dialog == null)
                    render();
            }
        });
    }
    void action(JSONObject command, CalendarApplication.Reply result) {
        if (outstanding != 0) {
            feedback("Дождитесь завершения операции.");
            return;
        }
        outstanding++;
        app.call(command, (data, error) -> {
            outstanding--;
            if (!alive())
                return;
            if (result != null)
                result.accept(data, error);
            else if (error != null)
                feedback(error);
            if (error == null)
                refresh(true);
        });
    }

    void showError(String error) {
        if (dialog != null)
            dialog.dismiss();
        state = new JSONObject();
        stateKey = "";
        selectedId = "";
        resetList();
        ReminderSchedule.refresh(this, null);
        root.removeAllViews();
        TextView reason = text(error, 16, false);
        pad(reason, 18, 18);
        root.addView(reason);
        root.addView(button("Повторить", () -> refresh(true)));
    }
    private void newPage(String title) {
        if (list != null) {
            firstVisible = list.getFirstVisiblePosition();
            View top = list.getChildAt(0);
            topOffset = top == null ? 0 : top.getTop();
        }
        list = null;
        root.removeAllViews();
        LinearLayout head = horizontal();
        pad(head, 16, 4);
        fill(head, text(title, 23, true));
        root.addView(head);
        line(root);
        status = text("", 14, false);
        status.setVisibility(View.GONE);
        pad(status, 16, 4);
        root.addView(status);
        body = vertical();
        root.addView(body, new LinearLayout.LayoutParams(-1, 0, 1));
    }
    private void resetList() {
        list = null;
        firstVisible = 0;
        topOffset = 0;
    }
    private void go(String to) {
        section = to;
        selectedId = "";
        resetList();
        render();
    }
    @Override
    public void onBackPressed() {
        if (!selectedId.isEmpty()) {
            selectedId = "";
            resetList();
            render();
        } else if (!"Год".equals(section))
            go("Год");
        else
            super.onBackPressed();
    }
    private void render() {
        if (!alive())
            return;
        boolean joined = !str(state, "workspaceName").isEmpty();
        if (!joined) {
            newPage("Годовой план");
            LinearLayout form = scroller();
            label(form, "Календарь команды", 23);
            hint(form, "Создайте календарь на этом устройстве или подключитесь к календарю коллег.");
            form.addView(button("Создать календарь", () -> connect(true, false)));
            form.addView(button("Присоединиться", () -> connect(false, false)));
            return;
        }
        JSONObject selected = selectedEvent();
        if (!selectedId.isEmpty() && selected == null)
            selectedId = "";
        newPage(selected != null ? "Событие" : "Год".equals(section) ? str(state, "workspaceName") : section);
        if (selected != null)
            detail(selected);
        else
            switch (section) {
                case "Доступ":
                    access();
                    break;
                case "Задачи":
                case "Напоминания":
                    agenda();
                    break;
                default:
                    calendar();
            }
        line(root);
        LinearLayout nav = horizontal();
        nav.setBackgroundColor(0xffffffff);
        for (String tab : TABS) {
            Button b = button(tab, () -> go(tab));
            b.setTextSize(13);
            pad(b, 0, 0);
            b.setTextColor(section.equals(tab) ? BLUE : MUTED);
            b.setSelected(section.equals(tab));
            fill(nav, b);
        }
        root.addView(nav);
    }
    private LinearLayout scroller() {
        ScrollView scroll = new ScrollView(this);
        LinearLayout inner = vertical();
        pad(inner, 16, 8);
        scroll.addView(inner);
        body.addView(scroll, new LinearLayout.LayoutParams(-1, -1));
        return inner;
    }
    private JSONObject selectedEvent() {
        if (selectedId.isEmpty())
            return null;
        for (String col : new String[] {"events", "tasks"})
            for (JSONObject e : items(array(state, col)))
                if (selectedId.equals(str(e, "id")) && year == e.optInt("year"))
                    return e;
        return null;
    }
    void open(String id, int yr) {
        selectedId = id;
        resetList();
        if (yr != year) {
            year = yr;
            refresh(true);
        } else
            render();
    }
    private void changeYear(int by) {
        int next = year + by;
        if (next < 1900 || next > 9999)
            return;
        year = next;
        resetList();
        refresh(true);
    }
    private void calendar() {
        LinearLayout bar = horizontal();
        pad(bar, 8, 0);
        Button prev = button("‹", () -> changeYear(-1));
        prev.setContentDescription("Предыдущий год");
        bar.addView(prev, new LinearLayout.LayoutParams(dp(48), -2));
        fill(bar,
                button(month > 0 ? MONTHS[month - 1] + " " + year : "Весь " + year + " год",
                        () -> chooseMonth()));
        Button next = button("›", () -> changeYear(1));
        next.setContentDescription("Следующий год");
        bar.addView(next, new LinearLayout.LayoutParams(dp(48), -2));
        bar.addView(button("Создать", () -> new EventForm(this, null).show()));
        body.addView(bar);
        List<Row> rows = new ArrayList<>();
        int[] counts = new int[12];
        int previous = 0;
        for (JSONObject e : items(array(state, "events"))) {
            String date = str(e, "date");
            if (date.length() != 10)
                continue;
            int m = LocalDate.parse(date).getMonthValue();
            counts[m - 1]++;
            if (month != 0 && month != m)
                continue;
            if (previous != m) {
                rows.add(new Row(MONTHS[m - 1], null, null, 0));
                previous = m;
            }
            rows.add(eventRow(e));
        }
        if (rows.isEmpty())
            rows.add(new Row("Нет событий", month == 0 ? "На выбранный год" : "За выбранный месяц", null, 0));
        YearDial dial = new YearDial(this, year, counts, month, m -> {
            month = m;
            resetList();
            render();
        });
        dial.setLayoutParams(new AbsListView.LayoutParams(-1, dp(158)));
        displayRows(rows, dial);
    }
    private void chooseMonth() {
        String[] values = new String[13];
        values[0] = "Весь год";
        System.arraycopy(MONTHS, 0, values, 1, 12);
        chooser("Месяц", values, month, n -> {
            month = n;
            resetList();
            render();
        });
    }
    private Row eventRow(JSONObject e) {
        String who = owner(str(e, "ownerId"));
        String date = pretty(str(e, "date"));
        return new Row(str(e, "title"), date + (who.isEmpty() ? "" : "  ·  " + who), str(e, "id"),
                e.optInt("year", year));
    }
    private void agenda() {
        List<Row> rows = new ArrayList<>();
        if (section.equals("Задачи"))
            for (JSONObject e : items(array(state, "tasks"))) rows.add(eventRow(e));
        else
            for (JSONObject r : items(array(state, "reminders")))
                rows.add(new Row(
                        str(r, "title"), pretty(str(r, "date")), str(r, "eventId"), r.optInt("year", year)));
        if (rows.isEmpty())
            rows.add(new Row(section.equals("Задачи") ? "Сейчас нет личных задач" : "Сейчас нет напоминаний",
                    null, null, 0));
        if (section.equals("Напоминания"))
            body.addView(
                    button(ReminderSchedule.enabled(this) ? "Выключить уведомления" : "Включить уведомления",
                            this::toggleAlerts));
        displayRows(rows, null);
    }
    private void toggleAlerts() {
        if (ReminderSchedule.enabled(this)) {
            ReminderSchedule.disable(this);
            render();
            return;
        }
        if (Build.VERSION.SDK_INT >= 33
                && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS)
                        != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[] {Manifest.permission.POST_NOTIFICATIONS}, ALERTS);
            return;
        }
        ReminderSchedule.enable(this);
        ReminderSchedule.refresh(this, state.optJSONArray("notificationDates"));
        render();
    }
    @Override
    public void onRequestPermissionsResult(int code, String[] permissions, int[] grants) {
        super.onRequestPermissionsResult(code, permissions, grants);
        if (code == ALERTS && grants.length > 0 && grants[0] == PackageManager.PERMISSION_GRANTED) {
            ReminderSchedule.enable(this);
            ReminderSchedule.refresh(this, state.optJSONArray("notificationDates"));
            render();
        }
    }
    static class Row {
        String title, sub, id;
        int year;
        Row(String t, String s, String i, int y) {
            title = t;
            sub = s;
            id = i;
            year = y;
        }
    }
    private void displayRows(List<Row> rows, View header) {
        list = new ListView(this);
        list.setDividerHeight(0);
        if (header != null)
            list.addHeaderView(header, null, false);
        list.setAdapter(new BaseAdapter() {
            public int getCount() {
                return rows.size();
            }
            public Object getItem(int pos) {
                return rows.get(pos);
            }
            public long getItemId(int pos) {
                return pos;
            }
            @Override
            public boolean areAllItemsEnabled() {
                return false;
            }
            @Override
            public boolean isEnabled(int pos) {
                return rows.get(pos).id != null;
            }
            public View getView(int pos, View recycled, ViewGroup parent) {
                Row row = rows.get(pos);
                LinearLayout cell;
                if (recycled == null) {
                    cell = vertical();
                    cell.setGravity(Gravity.CENTER_VERTICAL);
                    cell.addView(text("", 17, true));
                    cell.addView(text("", 14, false));
                } else
                    cell = (LinearLayout) recycled;
                pad(cell, 16, 8);
                cell.setMinimumHeight(dp(row.id == null ? 38 : 64));
                cell.setBackgroundColor(row.id == null ? BG : 0xffffffff);
                TextView title = (TextView) cell.getChildAt(0), sub = (TextView) cell.getChildAt(1);
                title.setText(row.title);
                sub.setText(row.sub);
                sub.setVisibility(row.sub == null ? View.GONE : View.VISIBLE);
                return cell;
            }
        });
        list.setOnItemClickListener((parent, v, pos, id) -> {
            Object value = parent.getItemAtPosition(pos);
            if (value instanceof Row) {
                Row r = (Row) value;
                if (r.id != null)
                    open(r.id, r.year);
            }
        });
        body.addView(list, new LinearLayout.LayoutParams(-1, 0, 1));
        list.setSelectionFromTop(firstVisible, topOffset);
    }
    private void detail(JSONObject e) {
        LinearLayout form = scroller();
        form.addView(button("‹ Назад", this::onBackPressed));
        label(form, str(e, "title"), 24);
        hint(form, pretty(str(e, "date")) + "  ·  " + catName(str(e, "category")));
        String who = owner(str(e, "ownerId"));
        if (!who.isEmpty())
            hint(form, "Ответственный · " + who);
        hint(form,
                "Подготовка с " + pretty(str(e, "preparationDate"))
                        + (e.optBoolean("yearly") ? "  ·  Ежегодно" : ""));
        if (!str(e, "description").isEmpty())
            hint(form, str(e, "description"));
        List<JSONObject> tasks = items(array(e, "checklist"));
        if (!tasks.isEmpty()) {
            label(form, "Чек-лист", 19);
            for (JSONObject task : tasks) {
                CheckBox check = new CheckBox(this);
                check.setText(str(task, "title") + "  ·  " + pretty(str(task, "dueDate")));
                check.setChecked(task.optBoolean("done"));
                check.setMinHeight(dp(48));
                form.addView(check);
                check.setOnClickListener(v -> {
                    if (outstanding != 0) {
                        check.setChecked(task.optBoolean("done"));
                        return;
                    }
                    check.setEnabled(false);
                    action(message("check", "eventId", str(e, "id"), "year", e.optInt("year"), "taskId",
                                   str(task, "id"), "done", check.isChecked()),
                            (data, error) -> {
                                if (error != null) {
                                    check.setChecked(task.optBoolean("done"));
                                    check.setEnabled(true);
                                    feedback(error);
                                }
                            });
                });
            }
        }
        String note = str(e, "note");
        label(form, "Дневник события", 19);
        if (!note.isEmpty())
            hint(form, note);
        form.addView(button(note.isEmpty() ? "Добавить заметку" : "Изменить заметку", () -> {
            LinearLayout panel = vertical();
            EditText editor = field(panel, "Заметка за " + e.optInt("year") + " год", note, true);
            submit("Дневник события", panel, "Сохранить",
                    ()
                            -> message("note", "eventId", str(e, "id"), "year", e.optInt("year"), "text",
                                    editor.getText().toString(), "base", note),
                    null);
        }));
        line(form);
        form.addView(button("Редактировать событие", () -> new EventForm(this, e).show()));
        form.addView(button("Удалить событие…", () -> {
            boolean yearly = e.optBoolean("yearly");
            if (yearly)
                chooser("Как удалить событие?",
                        new String[] {"Только за " + e.optInt("year") + " год", "За этот и следующие годы"},
                        -1, i -> delete(e, i == 0 ? "year" : "future"));
            else
                delete(e, "future");
        }));
    }
    private void delete(JSONObject e, String scope) {
        confirm("Удалить событие?",
                str(e, "title")
                        + (scope.equals("future") && e.optBoolean("yearly") ? " · этот и следующие годы"
                                                                            : " · " + e.optInt("year")),
                "Удалить", () -> {
                    action(message("deleteEvent", "eventId", str(e, "id"), "year", e.optInt("year"),
                                   "revision", str(e, "revision"), "scope", scope),
                            (unused, error) -> {
                                if (error != null)
                                    feedback(error);
                                else
                                    selectedId = "";
                            });
                });
    }

    private String nullable(String s) {
        return s.isEmpty() ? null : s;
    }
    private void connect(boolean create, boolean rejoin) {
        LinearLayout form = vertical();
        if (!create) {
            form.addView(button("Скопировать запрос устройства",
                    () -> copyOperation(message("pairingRequest"), "Запрос устройства скопирован")));
            hint(form, "Передайте запрос коллеге. Вставьте ответный код в это приложение.");
        }
        EditText name = rejoin ? null : field(form, "Ваше имя", "", false);
        EditText team = create ? field(form, "Название календаря", "", false) : null;
        EditText code = create ? null : field(form, "Код приглашения", "", false);
        if (code != null) {
            code.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_PASSWORD |
                    InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
            code.setImportantForAutofill(View.IMPORTANT_FOR_AUTOFILL_NO);
            code.setImeOptions(code.getImeOptions() |
                    android.view.inputmethod.EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING);
        }
        EditText addr = create ? null : field(form, "Адрес вручную (при необходимости)", "", false);
        submit(create            ? "Новый календарь"
                        : rejoin ? "Восстановить доступ"
                                 : "Подключиться",
                form, create ? "Создать" : "Подключиться", () -> {
                    if (create)
                        return message("createWorkspace", "name", value(team), "employee", value(name));
                    return rejoin ? message("rejoin", "code", value(code), "address", nullable(value(addr)))
                                  : message("join", "code", value(code), "employee", value(name), "address",
                                            nullable(value(addr)));
                }, null);
    }
    private void access() {
        LinearLayout form = scroller();
        label(form, str(state, "workspaceName"), 21);
        String addr = str(state, "address");
        if (!addr.isEmpty())
            hint(form, "Адрес устройства · " + addr);
        String error = str(state, "networkError");
        if (!error.isEmpty())
            hint(form, error);
        String last = str(state, "lastSync");
        if (!last.isEmpty()) {
            try {
                last = java.time.OffsetDateTime.parse(last)
                               .atZoneSameInstant(java.time.ZoneId.systemDefault())
                               .format(DateTimeFormatter.ofPattern("dd.MM.yyyy HH:mm"));
            } catch (Exception ignored) {
            }
            hint(form, "Последний обмен · " + last);
        }
        boolean paused = state.optBoolean("paused");
        form.addView(button(paused ? "Возобновить синхронизацию" : "Приостановить синхронизацию",
                () -> action(message("pause", "paused", !paused), null)));
        form.addView(button("Добавить адрес коллеги", () -> {
            LinearLayout panel = vertical();
            EditText field = field(panel, "IPv4-адрес и порт", "", false);
            field.setHint("192.168.1.20:45736");
            submit("Подключение по адресу", panel, "Добавить",
                    () -> message("peer", "address", value(field)), null);
        }));
        if (state.optBoolean("canManage"))
            form.addView(button("Пригласить устройство", () -> {
                LinearLayout p = vertical();
                EditText f = field(p, "Запрос устройства коллеги", "", false);
                submit("Приглашение", p, "Создать код",
                        ()
                                -> message("invite", "request", value(f)),
                        v -> copy(v.toString(), "Код приглашения скопирован"));
            }));
        form.addView(button("Подключиться по новому коду", () -> connect(false, true)));
        line(form);
        label(form, "Обмен через файл", 19);
        hint(form, "Зашифрованный файл открывается в том же календаре команды.");
        form.addView(button("Сохранить .yrplan", () -> pickDocument(true)));
        form.addView(button("Открыть .yrplan", () -> pickDocument(false)));
        line(form);
        label(form, "Устройства", 19);
        for (JSONObject m : items(array(state, "members"))) {
            String id = str(m, "id");
            hint(form,
                    str(m, "name") + (id.equals(str(state, "deviceId")) ? " · это устройство" : "")
                            + (m.optBoolean("revoked") ? " · доступ отозван" : "") + "\n" + id);
        }
        if (state.optBoolean("canManage")) {
            form.addView(button("Отозвать доступ…", this::revoke));
        }
    }
    private void revoke() {
        List<JSONObject> candidates = new ArrayList<>();
        for (JSONObject m : items(array(state, "members")))
            if (!m.optBoolean("revoked") && !str(m, "id").equals(str(state, "deviceId")))
                candidates.add(m);
        if (candidates.isEmpty()) {
            feedback("Нет других устройств с доступом.");
            return;
        }
        String[] names = new String[candidates.size()];
        boolean[] checked = new boolean[names.length];
        for (int i = 0; i < names.length; i++)
            names[i] = str(candidates.get(i), "name") + "\n" + str(candidates.get(i), "id");
        AlertDialog d = new AlertDialog.Builder(this)
                                .setTitle("Выберите устройства")
                                .setMultiChoiceItems(names, checked, (dlg, i, on) -> checked[i] = on)
                                .setNegativeButton("Отмена", null)
                                .setPositiveButton("Продолжить", null)
                                .create();
        track(d);
        d.getButton(-1).setOnClickListener(v -> {
            JSONArray ids = new JSONArray();
            StringBuilder selected = new StringBuilder();
            for (int i = 0; i < names.length; i++)
                if (checked[i]) {
                    ids.put(str(candidates.get(i), "id"));
                    selected.append(names[i]).append("\n");
                }
            if (ids.length() == 0) {
                feedback("Выберите хотя бы одно устройство.");
                return;
            }
            d.dismiss();
            confirm("Отозвать доступ?",
                    selected
                            + ("\nОбщий ключ изменится. Оставшимся сотрудникам понадобятся новые "
                               + "приглашения. Уже скачанные копии останутся у владельцев."),
                    "Отозвать", () -> action(message("revoke", "deviceIds", ids), null));
        });
    }
    private void pickDocument(boolean save) {
        Intent i = new Intent(save ? Intent.ACTION_CREATE_DOCUMENT : Intent.ACTION_OPEN_DOCUMENT);
        i.addCategory(Intent.CATEGORY_OPENABLE);
        i.setType(save ? "application/octet-stream" : "*/*");
        if (save)
            i.putExtra(Intent.EXTRA_TITLE, "Календарь-" + LocalDate.now() + ".yrplan");
        try {
            startActivityForResult(i, save ? WRITE : PICK);
        } catch (android.content.ActivityNotFoundException ex) {
            feedback("Системный выбор файлов недоступен.");
        }
    }
    @Override
    public void onActivityResult(int request, int result, Intent info) {
        super.onActivityResult(request, result, info);
        if (result != RESULT_OK || (request != PICK && request != WRITE) || info == null
                || info.getData() == null)
            return;
        Uri uri = info.getData();
        if (!"content".equals(uri.getScheme())) {
            feedback("Выберите файл через системное окно.");
            return;
        }
        CalendarApplication.Reply done = (value, error) -> {
            if (!alive())
                return;
            if (error != null)
                feedback(error);
            else {
                feedback(request == WRITE ? "Файл сохранён." : "Файл импортирован.");
                refresh(true);
            }
        };
        if (request == PICK)
            app.importDocument(uri, done);
        else
            app.exportDocument(uri, done);
    }
    private void copyOperation(JSONObject msg, String done) {
        action(msg, (result, error) -> {
            if (error != null)
                feedback(error);
            else
                copy(result.toString(), done);
        });
    }
    private void copy(String value, String done) {
        ClipboardManager clipboard = (ClipboardManager) getSystemService(CLIPBOARD_SERVICE);
        ClipData clip = ClipData.newPlainText("Годовой план", value);
        if (Build.VERSION.SDK_INT >= 33) {
            android.os.PersistableBundle extras = new android.os.PersistableBundle();
            extras.putBoolean(android.content.ClipDescription.EXTRA_IS_SENSITIVE, true);
            clip.getDescription().setExtras(extras);
        }
        clipboard.setPrimaryClip(clip);
        feedback(done);
    }
    interface Make {
        JSONObject get() throws Exception;
    }
    interface Accept {
        void run(Object value);
    }
    interface Choose {
        void chosen(int index);
    }
    void submit(String title, LinearLayout panel, String actionName, Make build, Accept success) {
        ScrollView scroll = new ScrollView(this);
        pad(panel, 20, 10);
        scroll.addView(panel);
        TextView errorLabel = text("", 14, false);
        errorLabel.setTextColor(0xff9b322b);
        panel.addView(errorLabel);
        AlertDialog d = new AlertDialog.Builder(this)
                                .setTitle(title)
                                .setView(scroll)
                                .setNegativeButton("Отмена", null)
                                .setPositiveButton(actionName, null)
                                .create();
        track(d);
        d.getButton(-1).setOnClickListener(v -> {
            JSONObject command;
            try {
                command = build.get();
            } catch (Exception ex) {
                errorLabel.setText(ex.getMessage());
                return;
            }
            if (outstanding != 0) {
                feedback("Дождитесь завершения операции.");
                return;
            }
            d.getButton(-1).setEnabled(false);
            action(command, (value, issue) -> {
                if (issue != null) {
                    if (d.isShowing()) {
                        d.getButton(-1).setEnabled(true);
                        errorLabel.setText(issue);
                        feedback(issue);
                    } else
                        feedback(issue);
                    return;
                }
                if (d.isShowing())
                    d.dismiss();
                if (success != null)
                    success.run(value);
            });
        });
    }
    void track(AlertDialog d) {
        dialog = d;
        d.setOnDismissListener(ignore -> {
            if (dialog == d) {
                dialog = null;
                if (active && !stateKey.isEmpty())
                    render();
            }
        });
        d.show();
    }
    void chooser(String title, String[] entries, int current, Choose pick) {
        AlertDialog d = new AlertDialog.Builder(this)
                                .setTitle(title)
                                .setSingleChoiceItems(entries, current,
                                        (dlg, n) -> {
                                            dlg.dismiss();
                                            pick.chosen(n);
                                        })
                                .setNegativeButton("Отмена", null)
                                .create();
        track(d);
    }
    void confirm(String title, String msg, String positive, Runnable run) {
        AlertDialog d = new AlertDialog.Builder(this)
                                .setTitle(title)
                                .setMessage(msg)
                                .setNegativeButton("Отмена", null)
                                .setPositiveButton(positive, (dlg, i) -> run.run())
                                .create();
        track(d);
    }
}
