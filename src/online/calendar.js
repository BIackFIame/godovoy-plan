"use strict";

(() => {
    const el = (id) => document.getElementById(id);
    const categories = {Campaign: "Акции", Partners: "Партнёры", Team: "Команда", Operations: "Операции"};
    const monthLabel = new Intl.DateTimeFormat("ru", {month: "long", year: "numeric", timeZone: "UTC"});
    const dateLabel = new Intl.DateTimeFormat("ru", {day: "numeric", month: "short", timeZone: "UTC"});
    const fullDate = new Intl.DateTimeFormat("ru", {day: "numeric", month: "long", year: "numeric", timeZone: "UTC"});
    const deadlineLabel = new Intl.DateTimeFormat("ru", {day: "numeric", month: "short", hour: "2-digit", minute: "2-digit", timeZoneName: "short"});
    const isoDate = (value) => new Date(value + "T12:00:00Z");
    const prompts = {
        login: "Введите личный ключ, который вам выдал администратор календаря.",
        disconnected: "Связь с компьютером компании прервана. Для продолжения подключитесь заново.",
        uncertain: "Связь прервана во время сохранения. Подключитесь и проверьте, появилось ли изменение в календаре.",
        denied: "Доступ не подтверждён. Проверьте ключ или обратитесь к администратору.",
        hidden: "Для просмотра календаря требуется соединение с компьютером компании.",
    };
    const leaseMs = 9000;
    const controllers = new Set();
    let credential = "";
    let generation = 0;
    let lastVerified = 0;
    let lastLoaded = 0;
    let requestNumber = 0;
    let heartbeatRunning = false;
    let currentYear = new Date().getFullYear();
    let mode = "all";
    let limit = 80;
    let state = null;
    let writing = false;
    let loginPending = false;
    let secure = false;
    let fieldNumber = 0;

    function element(tag, text, className) {
        const node = document.createElement(tag);
        if (text !== undefined) node.textContent = text;
        if (className) node.className = className;
        return node;
    }
    function button(text, onClick, className) {
        const node = element("button", text, className);
        node.type = "button";
        node.addEventListener("click", onClick);
        return node;
    }
    function notice(message) { el("feedback").textContent = message; }
    function standbyNotice(until) {
        const node = el("standby-status");
        if (until === (node.dataset.until || "")) return;
        node.dataset.until = until;
        node.hidden = !until;
        node.textContent = until
            ? "Работа на резервном сервере до " + deadlineLabel.format(new Date(Number(until) * 1000)) + "."
            : "";
    }
    function closeSheet() {
        if (el("sheet").open) el("sheet").close();
        el("sheet-body").replaceChildren();
        el("sheet-error").textContent = "";
        el("sheet-title").textContent = "";
    }
    function sheet(title) {
        el("sheet-title").textContent = title;
        el("sheet-error").textContent = "";
        const body = el("sheet-body");
        body.replaceChildren();
        if (!el("sheet").open) el("sheet").showModal();
        return body;
    }
    // Drop all workspace DOM, including modal form values. Late responses are
    // fenced by generation even if a browser completes an aborted fetch.
    function clearWorkspace(message, retainCredential = false) {
        generation++;
        requestNumber++;
        for (const controller of controllers) controller.abort();
        controllers.clear();
        if (!retainCredential) credential = "";
        writing = false;
        heartbeatRunning = false;
        loginPending = false;
        lastVerified = 0;
        lastLoaded = 0;
        state = null;
        standbyNotice("");
        el("workspace").hidden = true;
        closeSheet();
        el("sheet-body").inert = false;
        el("close").disabled = false;
        el("list").replaceChildren();
        el("calendar-name").textContent = "";
        el("account").textContent = "";
        el("year").textContent = "";
        notice("");
        el("logout").hidden = !credential;
        el("gate").hidden = false;
        el("gate-status").textContent = message;
        el("login").hidden = !!credential;
        el("token").value = "";
        el("connect").disabled = !secure;
        mode = "all";
        limit = 80;
    }

    class Problem extends Error {}
    async function api(path, {body, data = false} = {}) {
        const epoch = generation;
        if (!credential || document.hidden) throw new Problem();
        const controller = new AbortController();
        controllers.add(controller);
        const timeout = setTimeout(() => controller.abort(), 4500);
        try {
            const headers = {Authorization: "Bearer " + credential};
            if (body !== undefined) headers["Content-Type"] = "application/json";
            const response = await fetch(path, {
                method: body === undefined ? "GET" : "POST",
                body: body === undefined ? undefined : JSON.stringify(body),
                headers, cache: "no-store", credentials: "omit", referrerPolicy: "no-referrer",
                mode: "same-origin", redirect: "error", signal: controller.signal,
            });
            if (epoch !== generation || document.hidden) throw new Problem();
            const restriction = response.headers.get("x-godovoy-access");
            const textPolicy = response.headers.get("x-godovoy-allow-free-text");
            const scope = response.headers.get("x-godovoy-assigned-only");
            const scopeChanged = response.ok && state && (scope === "true" || scope === "false") &&
                (state.assignedOnly === true) !== (scope === "true");
            const textChanged = response.ok && state && (textPolicy === "true" || textPolicy === "false") &&
                (state.allowFreeText !== false) !== (textPolicy === "true");
            if (scopeChanged || textChanged || (response.status === 403 &&
                (restriction === "free-text-disabled" || restriction === "assigned-only"))) {
                let message = "Администратор закрыл описания и дневник. Названия событий и чек-листы доступны.";
                if (textPolicy === "true") message = "Администратор разрешил описания и дневник.";
                if (scopeChanged || restriction === "assigned-only") message = scope === "false"
                    ? "Теперь вам доступен весь календарь." : "Теперь вам доступны только назначенные вам события.";
                clearWorkspace(message, true);
                const updated = generation;
                load(currentYear).then(() => { if (updated === generation) notice(message); }).catch(() => {});
                throw new Problem();
            }
            if (response.status === 429) {
                clearWorkspace("Проверка ключей занята. Подождите секунду и войдите снова.");
                throw new Problem();
            }
            if (response.status === 401 || response.status === 403) {
                const messages = {
                    "office-only": "Этот ключ действует только в офисе. Откройте внутренний адрес календаря или запросите удалённый доступ.",
                    "remote-disabled": "Организация отключила удалённый вход. Подключитесь к офисной сети и откройте внутренний адрес календаря."
                };
                clearWorkspace(messages[restriction] || prompts.denied);
                throw new Problem();
            }
            if (response.status === 503 && response.headers.get("x-godovoy-standby") === "unavailable") {
                clearWorkspace(writing ? prompts.uncertain : "Календарь временно недоступен. Дождитесь возврата основного сервера и войдите снова.");
                throw new Problem();
            }
            if (!response.ok) {
                if (response.status >= 500 || body === undefined) throw new Error("Connection unavailable");
                const problem = await response.json();
                if (epoch !== generation || document.hidden) throw new Problem();
                throw new Problem(typeof problem.error === "string" ? problem.error : "Компьютер компании отклонил изменение.");
            }
            if (data ? response.status !== 200 || !/^application\/json(?:;|$)/i.test(response.headers.get("Content-Type") || "")
                     : response.status !== 204) {
                throw new Error("Unexpected response from calendar host");
            }
            const result = data ? await response.json() : null;
            if (epoch !== generation || document.hidden) throw new Problem();
            const until = response.headers.get("x-godovoy-standby-until") || "";
            standbyNotice(/^\d{1,11}$/.test(until) ? until : "");
            lastVerified = performance.now();
            return result;
        } catch (error) {
            if (!(error instanceof Problem) && epoch === generation) {
                clearWorkspace(writing ? prompts.uncertain : prompts.disconnected);
            }
            throw error;
        } finally {
            clearTimeout(timeout);
            controllers.delete(controller);
        }
    }

    async function load(year = currentYear) {
        const sequence = ++requestNumber;
        const next = await api("/v1/state?year=" + year, {data: true});
        if (sequence !== requestNumber) throw new Problem();
        if (!Array.isArray(next.events) || !Array.isArray(next.employees) || !Array.isArray(next.tasks)) {
            clearWorkspace("Компьютер компании вернул несовместимую версию календаря.");
            throw new Problem();
        }
        state = next;
        currentYear = year;
        lastLoaded = performance.now();
        el("gate").hidden = true;
        el("logout").hidden = false;
        el("workspace").hidden = false;
        el("account").textContent = (ownerName(state.employeeId) || "Сотрудник") +
            (state.assignedOnly ? " · Только назначенные события" : "") +
            (state.canEdit ? "" : " · Только чтение");
        el("calendar-name").textContent = state.workspaceName;
        el("create").hidden = !state.canEdit || state.assignedOnly === true;
        renderList();
    }
    function ownerName(id) {
        return state?.employees.find((person) => person.id === id)?.name || "";
    }
    function visibleEvents() { return mode === "all" ? state.events : state.tasks; }
    function renderList() {
        if (!state) return;
        el("all").setAttribute("aria-pressed", String(mode === "all"));
        el("mine").setAttribute("aria-pressed", String(mode === "mine"));
        el("year-tools").hidden = mode !== "all";
        el("year").textContent = String(currentYear);
        el("previous").disabled = currentYear <= 1900;
        el("next").disabled = currentYear >= 9999;
        const list = el("list");
        list.replaceChildren();
        const events = visibleEvents();
        if (!events.length) {
            list.append(element("p", mode === "mine" ? "Сейчас у вас нет задач, требующих внимания." : "В этом году событий пока нет.", "empty"));
            return;
        }
        let lastMonth = "";
        let rows;
        for (const event of events.slice(0, limit)) {
            const month = event.date.slice(0, 7);
            const date = isoDate(event.date);
            if (month !== lastMonth) {
                const group = element("section", undefined, "month");
                const label = monthLabel.format(date);
                group.append(element("h2", label[0].toLocaleUpperCase("ru") + label.slice(1)));
                rows = element("ul", undefined, "items");
                group.append(rows);
                list.append(group);
                lastMonth = month;
            }
            const item = element("li");
            const open = button("", () => detail(event), "event");
            open.append(element("span", dateLabel.format(date), "date"));
            const summary = element("span");
            const owner = ownerName(event.ownerId);
            summary.append(element("strong", event.title), element("span",
                [categories[event.category], owner].filter(Boolean).join(" · "), "sub"));
            open.append(summary);
            if (event.checklist.length) {
                const done = event.checklist.filter((task) => task.done).length;
                const counter = element("span", done + "/" + event.checklist.length, "count");
                counter.setAttribute("aria-label", "Выполнено " + done + " из " + event.checklist.length);
                open.append(counter);
            }
            item.append(open);
            rows.append(item);
        }
        if (events.length > limit) list.append(button("Показать ещё", () => { limit += 80; renderList(); }, "more"));
    }

    function section(title) {
        const block = element("section", undefined, "section");
        block.append(element("h3", title));
        return block;
    }
    function target(event) { return {eventId: event.id, year: event.year}; }
    function detail(event) {
        const body = sheet(event.title);
        body.append(element("p", [fullDate.format(isoDate(event.date)), categories[event.category],
            event.yearly ? "Каждый год" : "", ownerName(event.ownerId)].filter(Boolean).join(" · "), "sub"));
        body.append(element("p", "Начало подготовки — " + fullDate.format(isoDate(event.preparationDate))));
        const freeText = state.allowFreeText !== false;
        if (freeText && event.description) body.append(element("p", event.description, "description"));
        const tasks = section("Чек-лист");
        if (!event.checklist.length) tasks.append(element("p", "Пунктов пока нет.", "sub"));
        for (const task of event.checklist) {
            const row = element("label", undefined, "task");
            const box = element("input");
            box.type = "checkbox";
            box.checked = task.done;
            box.disabled = !state.canEdit;
            box.addEventListener("change", () => {
                const done = box.checked;
                box.checked = task.done;
                save({op: "check", ...target(event), taskId: task.id, done}, event);
            });
            const label = element("span", task.title);
            label.append(element("small", "До " + fullDate.format(isoDate(task.dueDate)), "sub"));
            row.append(box, label);
            tasks.append(row);
        }
        body.append(tasks);
        if (freeText) {
            const notes = section("Дневник");
            notes.append(element("p", event.note || "Записей пока нет.", event.note ? "description" : "sub"));
            body.append(notes);
        } else {
            body.append(element("p", "Организация отключила описания и дневник.", "sub"));
        }
        if (state.canEdit && (freeText || !state.assignedOnly)) {
            const actions = element("div", undefined, "actions");
            if (freeText) actions.append(button("Запись в дневнике", () => noteEditor(event)));
            if (!state.assignedOnly) actions.append(button("Изменить событие", () => editor(event)));
            body.append(actions);
        }
    }
    function control(parent, label, input) {
        const wrapper = element("div");
        const caption = element("label", label);
        input.id = "field-" + (++fieldNumber);
        caption.htmlFor = input.id;
        wrapper.append(caption, input);
        parent.append(wrapper);
        return input;
    }
    function input(type, value = "") {
        const item = element("input");
        item.type = type;
        item.value = value;
        return item;
    }
    function options(choices, value) {
        const select = element("select");
        for (const [id, name] of choices) select.add(new Option(name, id));
        select.value = value;
        return select;
    }
    function textArea(value) {
        const field = element("textarea");
        field.value = value;
        field.maxLength = 4000;
        return field;
    }
    function days(value) {
        const field = input("number", value);
        field.min = "0";
        field.max = "366";
        field.step = "1";
        field.required = true;
        return field;
    }
    function submit(form, text) {
        const action = element("button", text, "primary");
        action.type = "submit";
        form.append(action);
    }
    function formFor(title) {
        const form = element("form");
        form.autocomplete = "off";
        sheet(title).append(form);
        return form;
    }
    function noteEditor(event) {
        const form = formFor("Дневник события");
        const text = control(form, event.title, textArea(event.note));
        submit(form, "Сохранить запись");
        form.addEventListener("submit", (e) => {
            e.preventDefault();
            save({op: "note", ...target(event), text: text.value, base: event.note}, event);
        });
    }
    function editor(event) {
        if (!state?.canEdit || state.assignedOnly) return;
        const form = formFor(event ? "Изменить событие" : "Новое событие");
        const title = control(form, "Название", input("text", event?.title || ""));
        title.maxLength = 160;
        title.required = true;
        const today = new Date();
        const defaultDate = currentYear === today.getFullYear()
            ? [currentYear, String(today.getMonth() + 1).padStart(2, "0"), String(today.getDate()).padStart(2, "0")].join("-")
            : currentYear + "-01-01";
        const date = control(form, "Дата события", input("date", event?.date || defaultDate));
        date.min = event ? event.year + "-01-01" : "1900-01-01";
        date.max = event ? event.year + "-12-31" : "9999-12-31";
        date.required = true;
        const category = control(form, "Категория", options(Object.entries(categories), event?.category || "Team"));
        const employees = state.employees.map((person) => [person.id, person.name]);
        if (event?.ownerId && !employees.some(([id]) => id === event.ownerId)) employees.push([event.ownerId, "Прежний ответственный"]);
        const owner = control(form, "Ответственный", options([["", "Без ответственного"], ...employees], event?.ownerId || ""));
        const prep = control(form, "Начать подготовку за столько дней", days(event?.preparationDays ?? 14));
        const yearly = input("checkbox");
        yearly.checked = event?.yearly || false;
        const repeat = element("label", undefined, "task");
        repeat.append(yearly, element("span", "Повторять каждый год"));
        form.append(repeat);
        const description = state.allowFreeText === false ? null : control(form, "Описание", textArea(event?.description || ""));
        if (!description) form.append(element("p", "Организация отключила описания и дневник.", "sub"));
        const checklist = section("Чек-лист");
        const rows = element("div", undefined, "section");
        const drafted = new Map();
        const add = button("Добавить пункт", () => addTask());
        function addTask(task) {
            if (drafted.size >= 100) return;
            const row = element("div", undefined, "edit-task");
            const name = control(row, "Пункт", input("text", task?.title || ""));
            name.required = true;
            name.maxLength = 160;
            const lead = control(row, "Дней до", days(task?.daysBefore || 0));
            const remove = button("×", () => { drafted.delete(row); row.remove(); add.disabled = false; });
            remove.setAttribute("aria-label", "Убрать пункт");
            row.append(remove);
            rows.append(row);
            drafted.set(row, {name, lead, id: task?.id});
            add.disabled = drafted.size >= 100;
            if (!task) name.focus();
        }
        checklist.append(rows, add);
        form.append(checklist);
        for (const task of event?.checklist || []) addTask(task);
        const scope = event?.yearly ? control(form, "Какие годы изменить", options([
            ["year", "Только " + event.year + " год"], ["future", "Этот и следующие годы"],
        ], "year")) : null;
        submit(form, "Сохранить событие");
        form.addEventListener("submit", (e) => {
            e.preventDefault();
            const tasks = [];
            for (const item of drafted.values()) {
                if (item.lead.valueAsNumber > prep.valueAsNumber) {
                    el("sheet-error").textContent = "Срок пункта должен входить в период подготовки.";
                    item.lead.focus();
                    return;
                }
                const task = {title: item.name.value, daysBefore: item.lead.valueAsNumber};
                if (item.id) task.id = item.id;
                tasks.push(task);
            }
            const command = {op: event ? "editEvent" : "createEvent", title: title.value.trim(), date: date.value,
                yearly: yearly.checked, category: category.value, ownerId: owner.value || null,
                preparationDays: prep.valueAsNumber, description: description?.value || "", checklist: tasks};
            if (event) Object.assign(command, target(event), {scope: scope?.value || "future", revision: event.revision});
            save(command, null, Number(date.value.slice(0, 4)));
        });
        if (event) form.append(button("Удалить событие…", () => confirmDelete(event), "danger"));
    }
    function confirmDelete(event) {
        const form = formFor("Удалить событие?");
        form.append(element("p", event.title));
        const scope = event.yearly ? control(form, "Какие годы удалить", options([
            ["year", "Только " + event.year + " год"], ["future", "Этот и следующие годы"],
        ], "year")) : null;
        submit(form, "Подтвердить удаление");
        form.append(button("Оставить событие", () => detail(event)));
        form.addEventListener("submit", (e) => {
            e.preventDefault();
            save({op: "deleteEvent", ...target(event), revision: event.revision, scope: scope?.value || "future"});
        });
    }

    async function save(command, reopen = null, year = currentYear) {
        if (writing || !state || !state.canEdit) return;
        const epoch = generation;
        writing = true;
        el("sheet-body").inert = true;
        el("close").disabled = true;
        el("sheet-error").textContent = "";
        try {
            await api("/v1/command", {body: command});
            writing = false;
            await load(year);
            if (epoch !== generation) return;
            const fresh = reopen && [...state.events, ...state.tasks].find((e) => e.id === reopen.id && e.year === reopen.year);
            if (fresh) detail(fresh);
            else closeSheet();
            notice("Изменение сохранено на компьютере компании.");
        } catch (error) {
            if (epoch === generation && error instanceof Problem && error.message) el("sheet-error").textContent = error.message;
        } finally {
            if (epoch === generation) {
                writing = false;
                el("sheet-body").inert = false;
                el("close").disabled = false;
            }
        }
    }
    function changeMode(next) { mode = next; limit = 80; notice(""); renderList(); }
    async function refresh(year = currentYear) {
        if (writing || document.hidden) return;
        try { await load(year); }
        catch { /* api clears the workspace on lost access or connectivity. */ }
    }
    el("login").addEventListener("submit", async (evt) => {
        evt.preventDefault();
        if (!secure || loginPending || document.hidden) return;
        const token = el("token").value.trim();
        if (!token || !/^[\x21-\x7e]+$/.test(token)) {
            el("gate-status").textContent = "Введите ключ целиком, без пробелов.";
            return;
        }
        credential = token;
        loginPending = true;
        el("token").value = "";
        el("connect").disabled = true;
        el("gate-status").textContent = "Подключение…";
        const epoch = generation;
        try { await load(); }
        catch { /* api shows the reason when it locks the view. */ }
        finally { if (epoch === generation) { loginPending = false; el("connect").disabled = !secure; } }
    });
    el("logout").addEventListener("click", () => clearWorkspace(prompts.login));
    el("all").addEventListener("click", () => changeMode("all"));
    el("mine").addEventListener("click", () => changeMode("mine"));
    el("previous").addEventListener("click", () => { limit = 80; refresh(Math.max(1900, currentYear - 1)); });
    el("next").addEventListener("click", () => { limit = 80; refresh(Math.min(9999, currentYear + 1)); });
    el("refresh").addEventListener("click", () => { refresh(); });
    el("create").addEventListener("click", () => editor(null));
    el("close").addEventListener("click", closeSheet);
    el("sheet").addEventListener("cancel", (evt) => { evt.preventDefault(); if (!writing) closeSheet(); });

    window.addEventListener("offline", () => clearWorkspace(writing ? prompts.uncertain : prompts.disconnected));
    window.addEventListener("pagehide", () => clearWorkspace(prompts.login));
    document.addEventListener("visibilitychange", () => {
        if (document.hidden) { clearWorkspace(writing ? prompts.uncertain : prompts.hidden, !writing); return; }
        if (credential) refresh();
    });
    setInterval(async () => {
        if (!state || document.hidden) return;
        if (performance.now() - lastVerified >= leaseMs) {
            clearWorkspace(writing ? prompts.uncertain : prompts.disconnected);
            return;
        }
        if (heartbeatRunning) return;
        const epoch = generation;
        heartbeatRunning = true;
        try {
            await api("/v1/session");
            if (performance.now() - lastLoaded > 60000 && !el("sheet").open && !writing) await load();
        } catch { /* api applies the lock policy. */ }
        finally { if (generation === epoch) heartbeatRunning = false; }
    }, 3000);
    const loopback = ["127.0.0.1", "localhost", "[::1]"].includes(location.hostname);
    secure = window.isSecureContext && (location.protocol === "https:" || (location.protocol === "http:" && loopback));
    clearWorkspace(secure ? prompts.login : "Откройте календарь через HTTPS или локальный порт защищённого туннеля.");
})();
