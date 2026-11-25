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
    let heartbeatTimer = null;
    let currentYear = new Date().getFullYear();
    let mode = "all";
    let limit = 80;
    let state = null;
    let stateTag = null;
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
        const dialog = el("sheet");
        const origin = dialog.dataset.origin;
        delete dialog.dataset.origin;
        if (dialog.open) dialog.close();
        el("sheet-body").replaceChildren();
        el("sheet-error").textContent = "";
        el("sheet-title").textContent = "";
        if (origin && state) (document.getElementById(origin) || el(mode)).focus();
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
        clearInterval(heartbeatTimer);
        heartbeatTimer = null;
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
        stateTag = null;
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
    async function api(path, {body, data = false, validator = null, receipt = null} = {}) {
        const epoch = generation;
        if (!credential || document.hidden) throw new Problem();
        const controller = new AbortController();
        controllers.add(controller);
        const timeout = setTimeout(() => controller.abort(), 4500);
        try {
            const headers = {Authorization: "Bearer " + credential};
            if (body !== undefined) headers["Content-Type"] = "application/json";
            if (receipt) {
                headers["X-Godovoy-Base"] = receipt.tag;
                headers["X-Godovoy-Year"] = String(receipt.year);
            }
            if (validator) {
                headers["If-None-Match"] = validator;
                headers["X-Godovoy-Delta"] = "event-v1";
            }
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
            const tag = response.headers.get("etag");
            const unchanged = response.status === 304;
            // Proxies may weaken the ETag on a compressed response.
            if (unchanged && (!data || !validator || tag?.replace(/^W\//, "") !== validator.replace(/^W\//, "") ||
                (scope !== "true" && scope !== "false") || (textPolicy !== "true" && textPolicy !== "false"))) {
                throw new Error("Unexpected conditional calendar response");
            }
            const authorized = response.ok || unchanged;
            const scopeChanged = authorized && state && (scope === "true" || scope === "false") &&
                (state.assignedOnly === true) !== (scope === "true");
            const textChanged = authorized && state && (textPolicy === "true" || textPolicy === "false") &&
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
            if (!authorized) {
                if (response.status >= 500 || body === undefined) throw new Error("Connection unavailable");
                const problem = await response.json();
                if (epoch !== generation || document.hidden) throw new Problem();
                throw new Problem(typeof problem.error === "string" ? problem.error : "Компьютер компании отклонил изменение.");
            }
            const hasReceipt = receipt && response.status === 200;
            const expectJson = (data && !unchanged) || hasReceipt;
            if (expectJson ? response.status !== 200 || !/^application\/json(?:;|$)/i.test(response.headers.get("Content-Type") || "")
                           : response.status !== (unchanged ? 304 : 204)) {
                throw new Error("Unexpected response from calendar host");
            }
            const result = expectJson ? await response.json() : null;
            if (epoch !== generation || document.hidden) throw new Problem();
            const until = response.headers.get("x-godovoy-standby-until") || "";
            standbyNotice(/^\d{1,11}$/.test(until) ? until : "");
            lastVerified = performance.now();
            if (hasReceipt) return {value: result, tag: response.headers.get("x-godovoy-state-tag")};
            const base = response.headers.get("x-godovoy-delta-base");
            if (data && base !== null) {
                return {value: result, base, tag: response.headers.get("x-godovoy-state-tag")};
            }
            return data ? {value: result, tag, unchanged} : null;
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
        const prior = state;
        const validator = year === currentYear ? stateTag : null;
        const path = "/v1/state?year=" + year;
        let response = await api(path, {data: true, validator});
        if (sequence !== requestNumber) throw new Problem();
        if (response.base !== undefined) {
            if (prior && state === prior && stateTag === validator && currentYear === year &&
                response.base === validator && response.tag !== validator &&
                (response.value?.events?.length || response.value?.tasks?.length) &&
                applyReceipt(response, response.value?.id, year)) return;
            response = await api(path, {data: true});
            if (sequence !== requestNumber) throw new Problem();
        }
        if (response.unchanged) {
            lastLoaded = performance.now();
            return;
        }
        const next = response.value;
        if (response.base !== undefined || !next || next.year !== year || !Array.isArray(next.events) || !Array.isArray(next.employees) || !Array.isArray(next.tasks)) {
            clearWorkspace("Компьютер компании вернул несовместимую версию календаря.");
            throw new Problem();
        }
        state = next;
        if (heartbeatTimer === null) heartbeatTimer = setInterval(heartbeat, 3000);
        stateTag = /^(?:W\/)?"[!#-~]{1,128}"$/.test(response.tag || "") ? response.tag : null;
        currentYear = year;
        lastLoaded = performance.now();
        el("gate").hidden = true;
        el("logout").hidden = false;
        el("workspace").hidden = false;
        el("create").hidden = !state.canEdit || state.assignedOnly === true;
        renderList();
    }
    function ownerName(id) {
        return state?.employees.find((person) => person.id === id)?.name || "";
    }
    function visibleEvents() { return mode === "all" ? state.events : state.tasks; }
    function showProgress(counter, checklist) {
        const done = checklist.reduce((count, task) => count + Number(task.done), 0);
        const label = done + "/" + checklist.length;
        if (counter.textContent === label) return;
        counter.textContent = label;
        counter.setAttribute("aria-label", "Выполнено " + done + " из " + checklist.length);
    }
    function localDay() {
        const now = new Date();
        return [now.getFullYear(), String(now.getMonth() + 1).padStart(2, "0"),
            String(now.getDate()).padStart(2, "0")].join("-");
    }
    function personalRows(events) {
        return events.map(event => {
            let next = null;
            for (const task of event.checklist) {
                if (!task.done && (!next || task.dueDate < next.dueDate)) next = task;
            }
            return {event, next, due: next?.dueDate || event.date};
        }).sort((a, b) => a.due < b.due ? -1 : a.due > b.due ? 1 : 0);
    }
    function renderList() {
        if (!state) return;
        el("all").setAttribute("aria-pressed", String(mode === "all"));
        el("mine").setAttribute("aria-pressed", String(mode === "mine"));
        el("year-tools").hidden = mode !== "all";
        el("year").textContent = String(currentYear);
        el("previous").disabled = currentYear <= 1900;
        el("next").disabled = currentYear >= 9999;
        el("account").textContent = (ownerName(state.employeeId) || "Сотрудник") +
            (mode === "mine" ? " · " + state.workspaceName : "") +
            (state.assignedOnly ? " · Только назначенные события" : "") +
            (state.canEdit ? "" : " · Только чтение");
        el("calendar-name").textContent = mode === "mine" ? "Мои задачи" : state.workspaceName;
        el("create").classList.toggle("primary", mode === "all");
        const list = el("list");
        const focusedId = list.contains(document.activeElement) ? document.activeElement.id : "";
        list.replaceChildren();
        if (!visibleEvents().length) {
            list.append(element("p", mode === "mine" ? "Сейчас у вас нет задач, требующих внимания." : "В этом году событий пока нет.", "empty"));
            if (focusedId) el(mode).focus();
            return;
        }
        const personal = mode === "mine" ? personalRows(state.tasks) : null;
        const today = localDay();
        let lastMonth = "";
        let rows;
        function appendPage(start) {
            let first;
            const events = visibleEvents();
            for (let i = start; i < Math.min(events.length, limit); i++) {
                const entry = personal?.[i];
                const event = entry?.event || events[i];
                const day = entry?.due || event.date;
                const month = entry ? (day < today ? "Просрочено" : day === today ? "Сегодня" : "Предстоит") : day.slice(0, 7);
                const date = isoDate(day);
                if (month !== lastMonth) {
                    const group = element("section", undefined, "month");
                    const label = entry ? month : monthLabel.format(date);
                    if (entry && day < today) group.classList.add("overdue");
                    const heading = element("h2", label[0].toLocaleUpperCase("ru") + label.slice(1));
                    heading.id = "agenda-group-" + i;
                    group.setAttribute("aria-labelledby", heading.id);
                    group.append(heading);
                    rows = element("ul", undefined, "items");
                    group.append(rows);
                    list.append(group);
                    lastMonth = month;
                }
                const item = element("li");
                const open = button("", () => {
                    el("sheet").dataset.origin = open.id;
                    detail(entry ? event : visibleEvents()[i], entry?.next?.id);
                }, entry ? "event personal" : "event");
                open.id = (entry ? "personal-" : "annual-") + event.id + "-" + event.year;
                const stamp = element("time", dateLabel.format(date), "date");
                stamp.dateTime = day;
                if (entry && day.slice(0, 4) !== today.slice(0, 4)) {
                    stamp.append(element("span", day.slice(0, 4), "sub"));
                }
                open.append(stamp);
                const summary = element("span");
                if (entry) {
                    summary.append(element("strong", entry.next?.title || event.title),
                        element("span", entry.next ? event.title : "Дата события", "sub"));
                } else {
                    summary.append(element("strong", event.title), element("span",
                        [categories[event.category], ownerName(event.ownerId)].filter(Boolean).join(" · "), "sub"));
                }
                open.append(summary);
                if (!entry && event.checklist.length) {
                    const counter = element("span", "", "count");
                    counter.id = "progress-" + i;
                    showProgress(counter, event.checklist);
                    open.append(counter);
                }
                item.append(open);
                rows.append(item);
                if (!first) first = open;
            }
            if (events.length > limit) {
                const more = button("Показать ещё", () => {
                    const start = limit;
                    const focused = document.activeElement === more;
                    more.remove();
                    limit += 80;
                    const added = appendPage(start);
                    if (focused && added) added.focus({preventScroll: true});
                }, "more");
                more.id = "agenda-more";
                list.append(more);
            }
            return first;
        }
        appendPage(0);
        if (focusedId) (document.getElementById(focusedId) || el(mode)).focus();
    }

    function section(title) {
        const block = element("section", undefined, "section");
        block.append(element("h3", title));
        return block;
    }
    function target(event) { return {eventId: event.id, year: event.year}; }
    function detail(event, taskId) {
        let focusTarget;
        const body = sheet(event.title);
        body.append(element("p", [fullDate.format(isoDate(event.date)), categories[event.category],
            event.yearly ? "Каждый год" : "", ownerName(event.ownerId)].filter(Boolean).join(" · "), "sub"));
        body.append(element("p", "Начало подготовки — " + fullDate.format(isoDate(event.preparationDate))));
        const freeText = state.allowFreeText !== false;
        if (freeText && event.description) body.append(element("p", event.description, "description"));
        const tasks = section("Чек-лист");
        const today = localDay();
        if (!event.checklist.length) tasks.append(element("p", "Пунктов пока нет.", "sub"));
        for (const task of event.checklist) {
            const row = element("label", undefined, "task");
            const box = element("input");
            box.type = "checkbox";
            box.checked = task.done;
            box.disabled = !state.canEdit;
            if (task.id === taskId) {
                row.classList.add("current-task");
                if (box.disabled) row.tabIndex = -1;
                focusTarget = box.disabled ? row : box;
            }
            box.addEventListener("change", () => {
                const done = box.checked;
                box.checked = task.done;
                save({op: "check", ...target(event), taskId: task.id, done}, event);
            });
            const label = element("span", task.title);
            const due = dateLabel.format(isoDate(task.dueDate));
            const overdue = !task.done && task.dueDate < today;
            const caption = overdue ? "Просрочено · " + fullDate.format(isoDate(task.dueDate))
                : !task.done && task.dueDate === today ? "Сегодня · " + due
                : "До " + fullDate.format(isoDate(task.dueDate));
            label.append(element("small", caption, overdue ? "sub late" : "sub"));
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
        if (focusTarget) {
            focusTarget.focus({preventScroll: true});
            focusTarget.scrollIntoView({block: "nearest"});
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

    function refreshChangedRows(previous, next) {
        if (mode === "mine" || previous.length !== next.length) return renderList();
        for (let i = 0; i < next.length; i++) {
            const before = previous[i], after = next[i];
            if (before === after) continue;
            if (before.id !== after.id || before.year !== after.year || before.date !== after.date ||
                before.title !== after.title || before.category !== after.category || before.ownerId !== after.ownerId ||
                before.checklist.length !== after.checklist.length) return renderList();
            const counter = document.getElementById("progress-" + i);
            if (counter) showProgress(counter, after.checklist);
        }
    }

    function applyReceipt(reply, id, year) {
        const receipt = reply?.value;
        const uuid = value => typeof value === "string" && /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(value);
        const date = value => typeof value === "string" && /^\d{4}-\d{2}-\d{2}$/.test(value);
        const count = value => Number.isSafeInteger(value) && value >= 0;
        // Assigned-only rows in the authenticated snapshot use the resolved person ID.
        // A grant may still name one of that person's older device IDs.
        const assignedOwner = state.events[0]?.ownerId ?? state.tasks[0]?.ownerId ?? state.employeeId;
        function validEvent(event) {
            if (event?.id !== id || !Number.isInteger(event.year) || event.year < 1900 || event.year > 9999 ||
                typeof event.title !== "string" || typeof event.note !== "string" || typeof event.description !== "string" ||
                !Object.hasOwn(categories, event.category) || !date(event.date) || !date(event.preparationDate) ||
                !count(event.preparationDays) || typeof event.yearly !== "boolean" ||
                !(event.ownerId === null || uuid(event.ownerId)) || typeof event.revision !== "string" ||
                !/^\d+:[a-f0-9-]{36}$/.test(event.revision) || !Array.isArray(event.checklist) ||
                (state.assignedOnly && event.ownerId !== assignedOwner) ||
                (state.allowFreeText === false && (event.note !== "" || event.description !== ""))) return false;
            const ids = new Set();
            for (const task of event.checklist) {
                if (!uuid(task?.id) || ids.has(task.id) || typeof task.title !== "string" ||
                    typeof task.done !== "boolean" || !count(task.daysBefore) || !date(task.dueDate)) return false;
                ids.add(task.id);
            }
            return true;
        }
        if (!uuid(id) || !receipt || receipt.id !== id || receipt.year !== year || !/^"[0-9a-f]{64}"$/.test(reply.tag || "")) return false;
        function replaceRows(previous, changes, annual) {
            if (!Array.isArray(changes)) return null;
            if (annual && changes.length > 1) return null;
            const years = new Set();
            const rows = previous.filter(event => event.id !== id);
            let lastIndex = -1;
            for (const change of changes) {
                if (!Number.isInteger(change?.index) || change.index <= lastIndex || change.index > rows.length ||
                    !validEvent(change.event) || (annual && change.event.year !== year) || years.has(change.event.year)) return null;
                years.add(change.event.year);
                rows.splice(change.index, 0, change.event);
                lastIndex = change.index;
            }
            return rows;
        }
        const events = replaceRows(state.events, receipt.events, true);
        const tasks = replaceRows(state.tasks, receipt.tasks, false);
        if (!events || !tasks) return false;
        const previous = visibleEvents();
        state = {...state, events, tasks};
        stateTag = reply.tag;
        lastLoaded = performance.now();
        refreshChangedRows(previous, visibleEvents());
        return true;
    }

    async function save(command, reopen = null, year = currentYear) {
        if (writing || !state || !state.canEdit) return;
        const epoch = generation;
        writing = true;
        el("sheet-body").inert = true;
        el("close").disabled = true;
        el("sheet-error").textContent = "";
        try {
            const prior = state;
            const receipt = stateTag && year === currentYear && (command.op === "check" || command.op === "note")
                ? {tag: stateTag, year} : null;
            ++requestNumber;
            const reply = await api("/v1/command", {body: command, receipt});
            if (epoch !== generation || document.hidden) return;
            writing = false;
            if (!receipt || state !== prior || stateTag !== receipt.tag || !applyReceipt(reply, command.eventId, year)) {
                await load(year);
            }
            if (epoch !== generation) return;
            const fresh = reopen && (state.events.find(e => e.id === reopen.id && e.year === reopen.year) ||
                state.tasks.find(e => e.id === reopen.id && e.year === reopen.year));
            if (fresh) {
                el("sheet-body").inert = false;
                detail(fresh, command.op === "check" ? command.taskId : undefined);
            }
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
    async function heartbeat() {
        if (!state || document.hidden) return;
        if (performance.now() - lastVerified >= leaseMs) {
            clearWorkspace(writing ? prompts.uncertain : prompts.disconnected);
            return;
        }
        if (heartbeatRunning) return;
        const epoch = generation;
        heartbeatRunning = true;
        try {
            if (performance.now() - lastLoaded > 60000 && !el("sheet").open && !writing) {
                await load();
            } else {
                await api("/v1/session");
            }
        } catch { /* api applies the lock policy. */ }
        finally { if (generation === epoch) heartbeatRunning = false; }
    }
    const loopback = ["127.0.0.1", "localhost", "[::1]"].includes(location.hostname);
    secure = window.isSecureContext && (location.protocol === "https:" || (location.protocol === "http:" && loopback));
    clearWorkspace(secure ? prompts.login : "Откройте календарь через HTTPS или локальный порт защищённого туннеля.");
})();
