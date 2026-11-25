package local.godovoyplan.mobile;

import static local.godovoyplan.mobile.CalendarApplication.message;
import static local.godovoyplan.mobile.MainActivity.*;

import android.app.DatePickerDialog;
import android.text.InputType;
import android.widget.*;
import java.time.LocalDate;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.json.JSONArray;
import org.json.JSONObject;

final class EventForm {
    private final MainActivity activity;
    private final JSONObject previous;
    private final List<String> employeeIds = new ArrayList<>();
    private final List<TaskFields> tasks = new ArrayList<>();
    private EditText title, preparation, description;
    private Spinner category, assignee, scope;
    private CheckBox yearly;
    private Button dateButton;
    private LocalDate date;
    private LinearLayout taskList;

    private static final class TaskFields {
        String id;
        EditText title, offset;
    }

    EventForm(MainActivity activity, JSONObject previous) {
        this.activity = activity;
        this.previous = previous;
        date = previous == null ? LocalDate.now().withYear(activity.year)
                                : LocalDate.parse(str(previous, "date"));
    }

    void show() {
        LinearLayout form = activity.vertical();
        title = activity.field(form, "Название", str(previous, "title"), false);
        activity.label(form, "Дата события", 14);
        dateButton = activity.button(
                pretty(date.toString()), () -> new DatePickerDialog(activity, (picker, y, m, d) -> {
                    date = LocalDate.of(y, m + 1, d);
                    dateButton.setText(pretty(date.toString()));
                }, date.getYear(), date.getMonthValue() - 1, date.getDayOfMonth()).show());
        form.addView(dateButton);
        yearly = new CheckBox(activity);
        yearly.setText("Повторять каждый год");
        yearly.setChecked(previous != null && previous.optBoolean("yearly"));
        form.addView(yearly);
        if (previous != null && previous.optBoolean("yearly")) {
            scope = choices(form, "Изменить",
                    new String[] {"Только за " + date.getYear() + " год", "За этот и следующие годы"}, 0);
        }
        int categoryIndex = Arrays.asList(CATS).indexOf(str(previous, "category"));
        category = choices(form, "Категория", CAT_NAMES, categoryIndex < 0 ? 3 : categoryIndex);
        List<String> names = new ArrayList<>();
        names.add("Без ответственного");
        employeeIds.add("");
        String initialOwner = previous == null ? str(activity.state, "employeeId") : str(previous, "employeeId");
        activity.appendEmployeeChoices(employeeIds, names, null);
        if (!initialOwner.isEmpty() && !employeeIds.contains(initialOwner)) {
            employeeIds.add(initialOwner);
            names.add(activity.owner(initialOwner) + " · нет активных устройств");
        }
        assignee = choices(form, "Ответственный", names.toArray(new String[0]),
                Math.max(0, employeeIds.indexOf(initialOwner)));
        preparation = numberField(form, "За сколько дней начать подготовку",
                previous == null ? 30 : previous.optInt("preparationDays"));
        activity.label(form, "Чек-лист", 18);
        taskList = activity.vertical();
        form.addView(taskList);
        for (JSONObject task : items(array(previous, "checklist"))) addTask(task);
        form.addView(activity.button("Добавить пункт", () -> {
            if (tasks.size() < 100)
                addTask(null);
            else
                activity.feedback("До 100 пунктов в одном событии.");
        }));
        description = activity.field(form, "Описание", str(previous, "description"), true);
        activity.submit(previous == null ? "Новое событие" : "Редактировать событие", form,
                previous == null ? "Создать" : "Сохранить", this::command,
                ignored -> activity.year = date.getYear());
    }

    private Spinner choices(LinearLayout form, String label, String[] names, int initial) {
        activity.label(form, label, 14);
        Spinner spinner = new Spinner(activity);
        ArrayAdapter<String> adapter =
                new ArrayAdapter<>(activity, android.R.layout.simple_spinner_item, names);
        adapter.setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item);
        spinner.setAdapter(adapter);
        spinner.setSelection(initial);
        spinner.setContentDescription(label);
        spinner.setMinimumHeight(activity.dp(48));
        form.addView(spinner);
        return spinner;
    }

    private EditText numberField(LinearLayout form, String label, int value) {
        EditText input = activity.field(form, label, String.valueOf(value), false);
        input.setInputType(InputType.TYPE_CLASS_NUMBER);
        return input;
    }

    private void addTask(JSONObject old) {
        TaskFields task = new TaskFields();
        task.id = str(old, "id");
        LinearLayout group = activity.vertical();
        activity.line(group);
        task.title = activity.field(group, "Что сделать", str(old, "title"), false);
        task.offset =
                numberField(group, "За сколько дней до события", old == null ? 0 : old.optInt("daysBefore"));
        group.addView(activity.button("Убрать пункт", () -> {
            tasks.remove(task);
            taskList.removeView(group);
        }));
        tasks.add(task);
        taskList.addView(group);
    }

    private static int days(EditText field, int max) {
        try {
            int n = Integer.parseInt(value(field));
            if (n >= 0 && n <= max)
                return n;
        } catch (NumberFormatException ignored) {
        }
        field.requestFocus();
        throw new IllegalArgumentException("Укажите срок от 0 до " + max + " дней.");
    }

    private JSONObject command() throws Exception {
        if (value(title).isEmpty()) {
            title.requestFocus();
            throw new IllegalArgumentException("Введите название события.");
        }
        int lead = days(preparation, 366);
        JSONArray checklist = new JSONArray();
        for (TaskFields task : tasks) {
            if (value(task.title).isEmpty()) {
                task.title.requestFocus();
                throw new IllegalArgumentException("Введите название пункта чек-листа или уберите его.");
            }
            JSONObject item = new JSONObject()
                                      .put("title", value(task.title))
                                      .put("daysBefore", days(task.offset, lead));
            if (!task.id.isEmpty())
                item.put("id", task.id);
            checklist.put(item);
        }
        String owner = employeeIds.get(assignee.getSelectedItemPosition());
        if (previous != null && owner.equals(str(previous, "employeeId")))
            owner = str(previous, "ownerId");
        JSONObject command = message(previous == null ? "createEvent" : "editEvent", "title", value(title),
                "date", date.toString(), "description", description.getText().toString(), "yearly",
                yearly.isChecked(), "preparationDays", lead, "ownerId", owner.isEmpty() ? null : owner,
                "category", CATS[category.getSelectedItemPosition()], "checklist", checklist);
        if (previous != null) {
            command.put("eventId", str(previous, "id"))
                    .put("revision", str(previous, "revision"))
                    .put("year", previous.getInt("year"))
                    .put("scope", scope != null && scope.getSelectedItemPosition() == 0 ? "year" : "future");
        }
        return command;
    }
}
