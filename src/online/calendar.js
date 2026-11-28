import {
    categories, validTag, snapshotTag, uuid, count, validOverview, validFullEvent,
    decodeFullCardsInPlace, expandOverviewRow, continuedYear,
} from "./calendar-wire.mjs";

(() => {
    // Resource Timing retains request URLs, including record IDs, in this document.
    window.performance?.setResourceTimingBufferSize?.(0);
    const el = (id) => document.getElementById(id);
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
    const heartbeatMs = 4000;
    const calendarPollMs = 60000;
    const leaseMs = 9000;
    const requestMs = 4500;
    const calendarBodyMs = 45000;
    const pageSize = 80;
    const pageOffer = size => size === pageSize ? "first-v1" : "prefix-" + size + "-v1";
    const controllers = new Set();
    let credential = "";
    let generation = 0;
    let lastVerified = 0;
    let lastPolled = 0;
    let requestNumber = 0;
    let detailNumber = 0;
    let activeDetail = null;
    let readOnlyCard = null;
    let sheetOrigin = null;
    let heartbeatRunning = false;
    let heartbeatTimer = null;
    let calendarTimer = null;
    let leaseTimer = null;
    let activeRead = null;
    let currentYear = new Date().getFullYear();
    let mode = "all";
    let renderedPersonalDay = null;
    let limit = pageSize;
    let state = null;
    let stateTag = null;
    let pageTag = null;
    let useContinuation = false;
    let calendarWatch = null;
    let pendingWatch = null;
    let overviewEnabled = true;
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
    function rememberSheetOrigin(button) {
        const rows = Array.from(el("list").querySelectorAll("button.event"));
        const index = rows.indexOf(button);
        sheetOrigin = {
            ids: [button.id, rows[index + 1]?.id, rows[index - 1]?.id].filter(Boolean),
            top: button.getBoundingClientRect().top,
        };
    }
    function closeSheet() {
        readOnlyCard = null;
        cancelDetail();
        detailNumber++;
        const dialog = el("sheet");
        const origin = sheetOrigin;
        sheetOrigin = null;
        if (dialog.open) dialog.close();
        el("sheet-body").replaceChildren();
        el("sheet-error").textContent = "";
        const title = el("sheet-title");
        title.textContent = "";
        title.removeAttribute("title");
        if (origin && state) {
            const row = origin.ids.map(el).find(Boolean);
            if (row) {
                row.focus({preventScroll: true});
                const bounds = row.getBoundingClientRect();
                const top = Math.min(Math.max(0, origin.top), Math.max(0, innerHeight - bounds.height));
                window.scrollBy(0, bounds.top - top);
            } else {
                el(mode).focus();
            }
        }
        pollCalendar();
    }
    function sheet(title) {
        readOnlyCard = null;
        const heading = el("sheet-title");
        heading.textContent = title;
        heading.title = title;
        el("sheet-error").textContent = "";
        const body = el("sheet-body");
        body.replaceChildren();
        body.scrollTop = 0;
        if (!el("sheet").open) el("sheet").showModal();
        return body;
    }
    // Drop all workspace DOM, including modal form values. Late responses are
    // fenced by generation even if a browser completes an aborted fetch.
    function clearWorkspace(message, retainCredential = false) {
        clearTimeout(leaseTimer);
        leaseTimer = null;
        activeRead?.controller.abort();
        activeRead = null;
        clearTimeout(heartbeatTimer);
        heartbeatTimer = null;
        clearTimeout(calendarTimer);
        calendarTimer = null;
        generation++;
        requestNumber++;
        for (const controller of controllers) controller.abort();
        controllers.clear();
        if (!retainCredential) credential = "";
        writing = false;
        heartbeatRunning = false;
        loginPending = false;
        lastVerified = 0;
        lastPolled = 0;
        state = null;
        stateTag = null;
        pageTag = null;
        useContinuation = false;
        calendarWatch = null;
        pendingWatch = null;
        renderedPersonalDay = null;
        standbyNotice("");
        el("workspace").hidden = true;
        el("list").removeAttribute("aria-busy");
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
        limit = pageSize;
    }

    class Problem extends Error {}
    function verified(at) {
        if (at <= lastVerified) return;
        lastVerified = at;
        clearTimeout(leaseTimer);
        leaseTimer = setTimeout(() => clearWorkspace(writing ? prompts.uncertain : prompts.disconnected),
            Math.max(0, lastVerified + leaseMs - performance.now()));
        scheduleHeartbeat();
    }

    async function api(path, {body, data = false, validator = null, receipt = null, read = null, head = false, detailRequest = null} = {}) {
        const epoch = generation;
        if (!credential || document.hidden) throw new Problem();
        const started = performance.now();
        const current = () => epoch === generation && !document.hidden &&
            (!read || read === activeRead && read.number === requestNumber) &&
            (!detailRequest || detailRequest === activeDetail);
        const controller = read?.controller || detailRequest?.controller || new AbortController();
        controllers.add(controller);
        let timeout = setTimeout(() => controller.abort(), requestMs);
        try {
            const headers = {Authorization: "Bearer " + credential};
            if (read || path === "/v1/session" || receipt) headers["X-Godovoy-Watch-Year"] = String(read?.year ?? currentYear);
            if (body !== undefined) headers["Content-Type"] = "application/json";
            if (overviewEnabled && (path.startsWith("/v1/state?") || path === "/v1/command")) {
                headers["X-Godovoy-View"] = "overview-v1";
            }
            if (headers["X-Godovoy-View"] || detailRequest) {
                headers["X-Godovoy-Rows"] = "tuple-v2";
                headers["X-Godovoy-Cards"] = "tuple-v1";
                headers["X-Godovoy-Assigned-View"] = "overview-v1";
            }
            const prefix = head ? 0 : read?.prefix || (receipt && pageTag ? state.events.length : 0);
            if (prefix) headers["X-Godovoy-Page"] = pageOffer(prefix);
            if (read?.after && !head) {
                headers["X-Godovoy-Continue"] = pageOffer(read.after.size);
                headers["X-Godovoy-Base"] = read.after.tag;
            }
            if (detailRequest && !head) headers["If-Match"] = detailRequest.tag;
            if (receipt) {
                if (receipt.rowsOnly) headers["X-Godovoy-Receipt"] = "rows-only-v1";
                headers["X-Godovoy-Base"] = receipt.tag;
                headers["X-Godovoy-Year"] = String(receipt.year);
            }
            if (validator) {
                headers["If-None-Match"] = validator;
                if (!head) headers["X-Godovoy-Delta"] = prefix && pageTag ? "page-event-v1" : "event-v1";
            }
            const url = new URL(path, location.origin);
            if (url.origin !== location.origin) throw new Error("Unexpected API origin");
            // Firefox needs CORS mode to send Origin on writes with no Referer.
            const response = await fetch(url, {
                method: head ? "HEAD" : body === undefined ? "GET" : "POST",
                body: body === undefined ? undefined : JSON.stringify(body),
                headers, cache: "no-store", credentials: "omit", referrerPolicy: "no-referrer",
                mode: body === undefined ? "same-origin" : "cors", redirect: "error", signal: controller.signal,
            });
            if (!current()) throw new Problem();
            if (performance.now() - started >= requestMs) throw new Error("Response headers timed out");
            const restriction = response.headers.get("x-godovoy-access");
            const textPolicy = response.headers.get("x-godovoy-allow-free-text");
            const scope = response.headers.get("x-godovoy-assigned-only");
            const tag = response.headers.get("etag");
            const advertisedSnapshot = response.headers.get("x-godovoy-state-tag");
            const unchanged = response.status === 304;
            // Proxies may weaken the ETag on a compressed response.
            if (unchanged && (!data || !validator || tag?.replace(/^W\//, "") !== validator.replace(/^W\//, "") ||
                (scope !== "true" && scope !== "false") || (textPolicy !== "true" && textPolicy !== "false") ||
                (read?.prefix && !head && pageTag !== null && advertisedSnapshot !== stateTag))) {
                throw new Error("Unexpected conditional calendar response");
            }
            const authorized = response.ok || unchanged;
            const editPolicy = response.headers.get("x-godovoy-can-edit");
            const editChanged = authorized && state && (editPolicy === "true" || editPolicy === "false") &&
                state.canEdit !== (editPolicy === "true");
            const scopeChanged = authorized && state && (scope === "true" || scope === "false") &&
                (state.assignedOnly === true) !== (scope === "true");
            const textChanged = authorized && state && (textPolicy === "true" || textPolicy === "false") &&
                (state.allowFreeText !== false) !== (textPolicy === "true");
            if (scopeChanged || textChanged || editChanged || (response.status === 403 &&
                (restriction === "free-text-disabled" || restriction === "assigned-only"))) {
                let message = "Администратор закрыл описания и дневник. Названия событий и чек-листы доступны.";
                if (textPolicy === "true") message = "Администратор разрешил описания и дневник.";
                if (scopeChanged || restriction === "assigned-only") message = scope === "false"
                    ? "Теперь вам доступен весь календарь." : "Теперь вам доступны только назначенные вам события.";
                if (editChanged) message = editPolicy === "true"
                    ? "Теперь вы можете изменять календарь." : "Теперь календарь доступен только для чтения.";
                const section = mode;
                clearWorkspace(message, true);
                mode = section;
                const updated = generation;
                load(currentYear).then(() => {
                    if (updated !== generation) return;
                    notice(message);
                    el("feedback").focus();
                }).catch(() => {});
                throw new Problem();
            }
            if (response.status === 429) {
                if (read?.loginRetry === "available" && !head && response.headers.get("Retry-After") === "1") {
                    read.loginRetry = "waiting";
                    return {retryLogin: true};
                }
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
            if (detailRequest && (response.status === 412 || response.status === 404)) return {stale: true};
            if (!authorized) {
                if (response.status >= 500 || body === undefined) throw new Error("Connection unavailable");
                const problem = await response.json();
                if (!current()) throw new Problem();
                throw new Problem(typeof problem.error === "string" ? problem.error : "Компьютер компании отклонил изменение.");
            }
            if (head && !unchanged) {
                if (response.status !== 200 || !validTag(tag)) throw new Error("Invalid calendar version");
                return {unchanged: false};
            }
            const hasReceipt = receipt && response.status === 200;
            const expectJson = (data && !unchanged) || hasReceipt;
            if (expectJson ? response.status !== 200 || !/^application\/json(?:;|$)/i.test(response.headers.get("Content-Type") || "")
                           : response.status !== (unchanged ? 304 : 204)) {
                throw new Error("Unexpected response from calendar host");
            }
            const base = response.headers.get("x-godovoy-delta-base");
            const viewTag = advertisedSnapshot || tag;
            // Only an authenticated, versioned calendar body may outlive the short request timeout.
            const extended = read && !head && expectJson && validTag(viewTag);
            if (extended) {
                if (!state) el("gate-status").textContent = "Загружаем календарь…";
                clearTimeout(timeout);
                timeout = setTimeout(() => controller.abort(), calendarBodyMs);
            }
            const bodyStarted = performance.now();
            const result = expectJson ? await response.json() : null;
            if (!current()) throw new Problem();
            const now = performance.now();
            if (controller.signal.aborted || now - bodyStarted >= calendarBodyMs ||
                (!extended && now - started >= requestMs) || (state && now - lastVerified >= leaseMs)) {
                throw new Error("Calendar authorization timed out");
            }
            const slow = extended && now - started >= requestMs;
            if (!slow) {
                const until = response.headers.get("x-godovoy-standby-until") || "";
                standbyNotice(/^\d{1,11}$/.test(until) ? until : "");
                verified(started);
            }
            const stamp = response.headers.get("x-godovoy-watch");
            const watch = stamp === "0" || /^[0-9a-f]{32}$/.test(stamp || "") ? stamp : null;
            if (hasReceipt) return {watch, value: result, tag: response.headers.get("x-godovoy-state-tag"),
                pageTag: response.headers.get("x-godovoy-page-tag")};
            if (!data) return watch;
            const personalCheck = response.headers.get("x-godovoy-personal-check") === "assigned-v1";
            // Measure the authorized body we already needed. Length describes the wire format,
            // including compression by the company's gateway. Keep the decision in this session.
            const transferMs = now - bodyStarted;
            const bytes = Number(response.headers.get("content-length"));
            const slowPage = response.headers.get("x-godovoy-continuations") === "v1" && transferMs >= 50 &&
                (bytes > 0 ? bytes / transferMs <= 160 : transferMs >= 600);
            const reply = {watch, personalCheck, slowPage, value: result, tag: viewTag, httpTag: tag, advertisedSnapshot,
                continued: response.headers.get("x-godovoy-continued"), pageTag: response.headers.get("x-godovoy-page-tag"),
                unchanged, slow, scope, textPolicy};
            if (base !== null) reply.base = base;
            return reply;
        } catch (error) {
            if (!(error instanceof Problem) && current()) {
                clearWorkspace(writing ? prompts.uncertain : prompts.disconnected);
            }
            throw error;
        } finally {
            clearTimeout(timeout);
            controllers.delete(controller);
        }
    }

    function load(year = currentYear, expandPage = null) {
        const complete = expandPage !== null;
        if (activeRead?.year === year && activeRead.number === requestNumber &&
            (!complete || activeRead.complete)) return activeRead.promise;
        activeRead?.controller.abort();
        // Header negotiation also permits a piecemeal frontend/server upgrade.
        const previousPrefix = year === currentYear && pageTag ? state.events.length : 0;
        const expanded = useContinuation ? 2 * previousPrefix : 0;
        const prefix = complete ? (expanded <= 1280 && expanded < state?.eventCount ? expanded : 0)
            : !state || year !== currentYear ? pageSize : previousPrefix;
        const after = complete && useContinuation && previousPrefix ? {size: previousPrefix, tag: pageTag, snapshot: stateTag} : null;
        const read = {year, prefix, complete, after, expandPage, number: ++requestNumber, controller: new AbortController(),
            loginRetry: loginPending && !state && prefix ? "available" : null};
        activeRead = read;
        read.promise = readYear(read).finally(() => { if (activeRead === read) activeRead = null; });
        return read.promise;
    }

    async function readYear(read) {
        const {number, year, prefix, complete} = read;
        const prior = state;
        const validator = !complete && year === currentYear ? (prefix ? pageTag : stateTag) : null;
        const path = "/v1/state?year=" + year;
        let response;
        for (let attempt = 0; attempt < 2; attempt++) {
            response = await api(path, {data: true, validator: attempt === 0 ? validator : null, read});
            if (response.retryLogin) {
                if (number !== requestNumber || document.hidden) throw new Problem();
                el("gate-status").textContent = "Сервер занят. Повторим подключение через секунду…";
                const signal = read.controller.signal;
                await new Promise(resolve => {
                    const finish = () => {
                        clearTimeout(timer);
                        signal.removeEventListener("abort", finish);
                        resolve();
                    };
                    const timer = setTimeout(finish, 1000);
                    if (signal.aborted) finish();
                    else signal.addEventListener("abort", finish, {once: true});
                });
                if (signal.aborted || number !== requestNumber || document.hidden) throw new Problem();
                response = await api(path, {data: true, read});
            }
            read.loginRetry = null;
            if (number !== requestNumber) throw new Problem();
            if (overviewEnabled && response.value?.overview === true && !/^"[0-9a-f]{64}"$/.test(response.tag || "")) {
                overviewEnabled = false;
                response = null;
                continue;
            }
            if (response.slow || complete) {
                if (!state) el("gate-status").textContent = "Проверяем актуальность календаря…";
                // The full body can outlive a policy change. Recheck current
                // authorization and the same complete snapshot before installing it.
                const proof = await api(path, {data: true, validator: response.tag, read, head: true});
                if (number !== requestNumber) throw new Problem();
                if (!proof.unchanged || proof.scope !== response.scope || proof.textPolicy !== response.textPolicy) {
                    response = null;
                    continue;
                }
            }
            if (response.continued !== null) {
                if (state !== prior || pageTag !== read.after?.tag || stateTag !== read.after?.snapshot) {
                    read.after = null;
                    response = null;
                    continue;
                }
                const next = continuedYear(response, read, prior);
                if (!next) {
                    clearWorkspace("Компьютер компании вернул несовместимое продолжение календаря.");
                    throw new Problem();
                }
                installYear(next, response, response.pageTag, read, prior);
                return;
            }
            if (response.base === undefined) break;
            const currentBase = prefix ? pageTag : stateTag;
            if (prior && state === prior && currentBase === validator && currentYear === year &&
                response.base === validator && response.tag !== stateTag &&
                (!prefix || (response.pageTag !== validator && response.value?.events?.length === 1)) &&
                (response.value?.events?.length || response.value?.tasks?.length) &&
                applyReceipt(response, response.value?.id, year)) {
                recordCalendarRead(response.watch);
                return;
            }
            response = null;
        }
        if (!response) {
            clearWorkspace("Календарь изменился во время загрузки. Войдите снова, чтобы открыть актуальную версию.");
            throw new Problem();
        }
        if (response.unchanged) {
            recordCalendarRead(response.watch);
            return;
        }
        const next = response.value;
        if (next?.cardFormat !== undefined && !decodeFullCardsInPlace(next)) {
            clearWorkspace("Компьютер компании вернул несовместимый формат карточек.");
            throw new Problem();
        }
        const nextHttpTag = response.httpTag?.replace(/^W\//, "");
        const separateTag = prefix > 0 && response.advertisedSnapshot !== null &&
            nextHttpTag !== response.advertisedSnapshot;
        const nextPageTag = separateTag && snapshotTag(response.advertisedSnapshot) &&
            snapshotTag(nextHttpTag) ? nextHttpTag : null;
        if (next?.rowFormat !== undefined) {
            if (next.rowFormat === "tuple-v2" && next.overview === true && Array.isArray(next.employees) &&
                Array.isArray(next.events) && Array.isArray(next.tasks)) {
                for (const rows of [next.events, next.tasks]) {
                    for (let i = 0; i < rows.length; i++) rows[i] = expandOverviewRow(rows[i], next.employees);
                }
            } else {
                clearWorkspace("Компьютер компании вернул несовместимую версию календаря.");
                throw new Problem();
            }
        }
        if (response.base !== undefined || !next || next.year !== year || !Array.isArray(next.events) ||
            !Array.isArray(next.employees) || !Array.isArray(next.tasks) ||
            (next.overview === true && next.assignedOnly === true && (!uuid(next.assignedOwnerId) ||
                next.events.some(row => row?.ownerId !== next.assignedOwnerId) ||
                next.tasks.some(row => row?.ownerId !== next.assignedOwnerId))) ||
            (separateTag && !nextPageTag) || ((next.eventCount !== undefined) !== (nextPageTag !== null)) ||
            (next.eventCount !== undefined && (!nextPageTag || next.overview !== true ||
                next.assignedOnly || next.rowFormat !== "tuple-v2" || !count(next.eventCount) ||
                next.events.length !== prefix || next.eventCount <= prefix || next.eventCount > 2000)) ||
            (next.overview === true && !(/^"[0-9a-f]{64}"$/.test(response.tag || "") &&
                next.events.every(row => validOverview(row, false)) &&
                next.tasks.every(row => validOverview(row, true))))) {
            clearWorkspace("Компьютер компании вернул несовместимую версию календаря.");
            throw new Problem();
        }
        installYear(next, response, nextPageTag, read, prior);
    }
    function installYear(next, response, nextPageTag, read, prior) {
        const samePage = prior === state && pageTag !== null && snapshotTag(stateTag) &&
            stateTag === response.tag && prior?.eventCount === (next.eventCount ?? next.events.length);
        const year = read.year;
        next.personalCheck = response.personalCheck;
        state = next;
        stateTag = validTag(response.tag) ? response.tag : null;
        pageTag = nextPageTag;
        if (!nextPageTag) useContinuation = false;
        else if (next.events.length === pageSize) useContinuation = response.slowPage;
        currentYear = year;
        recordCalendarRead(response.watch);
        scheduleHeartbeat();
        el("gate").hidden = true;
        el("logout").hidden = false;
        el("workspace").hidden = false;
        el("create").hidden = !state.canEdit || state.assignedOnly === true;
        if (!read.expandPage?.(samePage)) renderList();
    }
    function ownerName(id) {
        return state?.employees.find((person) => person.id === id)?.name || "";
    }
    function eventOwnerLabel(id) {
        return id ? ownerName(id) || "Прежний ответственный" : "";
    }
    function visibleEvents() { return mode === "all" ? state.events : state.tasks; }
    function taskCount(event) { return event.checklist?.length ?? event.total; }
    function showProgress(counter, event) {
        const done = event.completed ?? event.checklist.reduce((count, task) => count + Number(task.done), 0);
        const total = taskCount(event);
        const label = done + "/" + total;
        if (counter.textContent === label) return;
        counter.textContent = label;
        counter.setAttribute("aria-label", "Выполнено " + done + " из " + total);
    }
    function localDay() {
        const now = new Date();
        return [now.getFullYear(), String(now.getMonth() + 1).padStart(2, "0"),
            String(now.getDate()).padStart(2, "0")].join("-");
    }
    function personalRows(events) {
        return events.map((event, index) => {
            let next = event.next || null;
            for (const task of event.checklist || []) {
                if (!task.done && (!next || task.dueDate < next.dueDate)) next = task;
            }
            return {event, index, next, due: next?.dueDate || event.date};
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
        const today = localDay();
        renderedPersonalDay = mode === "mine" ? today : null;
        if (!visibleEvents().length) {
            list.append(element("p", mode === "mine" ? "Сейчас у вас нет задач, требующих внимания." : "В этом году событий пока нет.", "empty"));
            if (focusedId) el(mode).focus();
            return;
        }
        const personal = mode === "mine" ? personalRows(state.tasks) : null;
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
                const open = button("", click => {
                    // Finishing a task can move this row under a second tap of the same gesture.
                    if (entry && click.detail > 1) return;
                    rememberSheetOrigin(open);
                    openCard(entry ? state?.tasks[entry.index] : visibleEvents()[i], entry?.next?.id);
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
                        [categories[event.category], eventOwnerLabel(event.ownerId)].filter(Boolean).join(" · "), "sub"));
                }
                open.append(summary);
                if (!entry && taskCount(event)) {
                    const counter = element("span", "", "count");
                    counter.id = "progress-" + i;
                    showProgress(counter, event);
                    open.append(counter);
                }
                item.append(open);
                if (entry?.next && state.canEdit && state.personalCheck) {
                    item.classList.add("actionable");
                    const done = button("Готово", e => {
                        if (e.detail > 1) return;
                        save({op: "check", ...target(event), taskId: entry.next.id, done: true, onlyIfAssigned: true}, null, currentYear, done);
                    }, "complete-task");
                    done.id = "complete-" + event.id + "-" + event.year;
                    done.setAttribute("aria-label", "Готово — " + entry.next.title + " · " + event.title);
                    item.append(done);
                }
                rows.append(item);
                if (!first) first = open;
            }
            const partial = mode === "all" && state.eventCount !== undefined;
            if (events.length > limit || partial) {
                const moreLabel = "Показать ещё · " + Math.min(events.length, limit) + " из " +
                    (partial ? state.eventCount : events.length);
                const more = button(moreLabel, async () => {
                    const start = limit;
                    const focused = document.activeElement === more;
                    if (partial && start >= events.length) {
                        const year = currentYear;
                        const epoch = generation;
                        let restoreFocus = false;
                        more.disabled = true;
                        more.textContent = "Загружаем список…";
                        more.setAttribute("aria-busy", "true");
                        try {
                            await load(year, samePage => {
                                if (!more.isConnected || mode !== "all" || generation !== epoch ||
                                    currentYear !== year) return false;
                                restoreFocus = focused && !el("sheet").open &&
                                    (document.activeElement === more || document.activeElement === document.body);
                                limit = start + pageSize;
                                if (!samePage) return false;
                                more.remove();
                                appendPage(start);
                                return true;
                            });
                            if (generation !== epoch || currentYear !== year || mode !== "all" ||
                                !state) return;
                            const next = state.events[start];
                            if (restoreFocus && next) el("annual-" + next.id + "-" + next.year)?.focus({preventScroll: true});
                        } catch { /* api clears invalid disclosure or access. */ }
                        finally {
                            if (more.isConnected) {
                                more.disabled = false;
                                more.textContent = moreLabel;
                                more.removeAttribute("aria-busy");
                            }
                        }
                        return;
                    }
                    more.remove();
                    limit += pageSize;
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
    function currentEvent(row) {
        return state?.events.find(e => e.id === row.id && e.year === row.year) ||
            state?.tasks.find(e => e.id === row.id && e.year === row.year);
    }
    function cancelDetail() {
        activeDetail?.controller.abort();
        activeDetail = null;
    }

    async function openCard(row, taskId, mayReload = true, refreshing = null) {
        cancelDetail();
        if (!row || !state) return;
        if (!state.overview) { detail(row, taskId, refreshing); return; }
        const request = ++detailNumber;
        const year = currentYear, tag = stateTag, epoch = generation;
        const pending = {tag, controller: new AbortController()};
        activeDetail = pending;
        const sameSheet = () => request === detailNumber && epoch === generation &&
            currentYear === year && el("sheet").open && !document.hidden && (!refreshing || readOnlyCard === refreshing);
        if (!refreshing) {
            const loading = element("p", "Загружаем карточку…", "sub");
            loading.setAttribute("role", "status");
            sheet(row.title).append(loading);
        }
        try {
            const path = "/v1/event?year=" + year + "&eventYear=" + row.year + "&id=" + encodeURIComponent(row.id);
            const reply = await api(path, {data: true, detailRequest: pending});
            if (!sameSheet()) return;
            let stale = reply.stale || stateTag !== tag;
            if (!stale && state.assignedOnly) {
                // Access to an assignment can change while the card body is in transit.
                const proof = await api("/v1/state?year=" + year, {
                    data: true, head: true, validator: tag, detailRequest: pending,
                });
                if (!sameSheet()) return;
                stale = !proof.unchanged || stateTag !== tag;
            }
            if (stale) {
                if (!mayReload) { closeSheet(); return; }
                if (stateTag === tag) await load(year);
                if (!sameSheet()) return;
                const fresh = currentEvent(row);
                if (fresh) await openCard(fresh, taskId, false, refreshing);
                else closeSheet();
                return;
            }
            if (reply.tag !== tag || reply.value?.year !== row.year ||
                !validFullEvent(reply.value, row.id, row.ownerId, state)) {
                clearWorkspace("Компьютер компании вернул несовместимую карточку.");
                return;
            }
            detail(reply.value, taskId, refreshing);
        } catch {
            if (sameSheet()) closeSheet();
        } finally {
            if (activeDetail === pending) activeDetail = null;
        }
    }
    function detail(event, taskId, refreshing = null) {
        const owner = eventOwnerLabel(event.ownerId);
        if (refreshing && refreshing.owner === owner && JSON.stringify(refreshing.event) === JSON.stringify(event)) return;
        const previousScroll = refreshing ? el("sheet-body").scrollTop : 0;
        const restoreFocus = refreshing && el("sheet-body").contains(document.activeElement);
        let focusTarget;
        let focusRow;
        const body = sheet(event.title);
        body.append(element("p", [fullDate.format(isoDate(event.date)), categories[event.category],
            event.yearly ? "Каждый год" : "", owner].filter(Boolean).join(" · "), "sub"));
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
                focusRow = row;
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
        if (!state.canEdit) readOnlyCard = {event, taskId, owner};
        if (refreshing) {
            if (restoreFocus) (focusTarget || el("close")).focus({preventScroll: true});
            body.scrollTop = previousScroll;
        } else if (focusTarget) {
            focusTarget.focus({preventScroll: true});
            focusRow.scrollIntoView({block: "nearest"});
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

    function samePersonalRow(before, after) {
        return before === after ||
            (before.id === after.id && before.year === after.year && before.title === after.title &&
                before.date === after.date && before.next?.id === after.next?.id &&
                before.next?.title === after.next?.title && before.next?.dueDate === after.next?.dueDate);
    }

    function refreshChangedRows(previous, next) {
        if (previous.length !== next.length) return renderList();
        if (mode === "mine") {
            // Identical ordered inputs keep both the sorted DOM and the row handlers' indices valid.
            if (!state.overview || renderedPersonalDay !== localDay() ||
                !previous.every((row, i) => samePersonalRow(row, next[i]))) renderList();
            return;
        }
        for (let i = 0; i < next.length; i++) {
            const before = previous[i], after = next[i];
            if (before === after) continue;
            if (before.id !== after.id || before.year !== after.year || before.date !== after.date ||
                before.title !== after.title || before.category !== after.category || before.ownerId !== after.ownerId ||
                taskCount(before) !== taskCount(after)) return renderList();
            const counter = document.getElementById("progress-" + i);
            if (counter) showProgress(counter, after);
        }
    }

    function applyReceipt(reply, id, year) {
        const receipt = reply?.value;
        // Assigned-only rows in the authenticated snapshot use the resolved person ID.
        // A grant may still name one of that person's older device IDs.
        const assignedOwner = state.assignedOwnerId ?? state.events[0]?.ownerId ?? state.tasks[0]?.ownerId ?? state.employeeId;
        if (!uuid(id) || !receipt || receipt.id !== id || receipt.year !== year || !snapshotTag(reply.tag) ||
            (pageTag !== null && (!snapshotTag(reply.pageTag) || reply.pageTag === reply.tag))) return false;
        function replaceRows(previous, changes, annual) {
            if (!Array.isArray(changes)) return null;
            if (annual && changes.length > 1) return null;
            const years = new Set();
            const rows = previous.filter(event => event.id !== id);
            let lastIndex = -1;
            for (const change of changes) {
                const event = change?.event;
                const valid = state.overview ? validOverview(event, !annual) && event.id === id &&
                    (!state.assignedOnly || event.ownerId === assignedOwner) : validFullEvent(event, id, assignedOwner, state);
                if (!Number.isInteger(change?.index) || change.index <= lastIndex || change.index > rows.length ||
                    !valid || (annual && event.year !== year) || years.has(event.year)) return null;
                years.add(event.year);
                rows.splice(change.index, 0, event);
                lastIndex = change.index;
            }
            return rows;
        }
        const events = replaceRows(state.events, receipt.events, true);
        const tasks = replaceRows(state.tasks, receipt.tasks, false);
        if (!events || !tasks || (state.eventCount !== undefined &&
            (events.length !== state.events.length || events.some((row, index) => row.id !== state.events[index].id)))) return false;
        const previous = visibleEvents();
        state = {...state, events, tasks};
        stateTag = reply.tag;
        if (pageTag !== null) pageTag = reply.pageTag;
        refreshChangedRows(previous, visibleEvents());
        return true;
    }

    async function save(command, reopen = null, year = currentYear, personalControl = null) {
        if (writing || !state || !state.canEdit) return;
        const epoch = generation;
        const control = personalControl || document.activeElement;
        const fromList = personalControl !== null;
        const origin = fromList ? control?.closest("li") : null;
        const focusRows = origin ? [origin, origin.nextElementSibling, origin.previousElementSibling]
            .map(item => item?.querySelector(".event")?.id).filter(Boolean) : [];
        let refused = false;
        writing = true;
        if (fromList) {
            el("list").setAttribute("aria-busy", "true");
            control?.setAttribute("aria-disabled", "true");
            notice("Сохраняем отметку…");
        } else {
            el("sheet-body").inert = true;
            el("close").disabled = true;
            el("sheet-error").textContent = "";
        }
        try {
            const prior = state;
            const receipt = stateTag && year === currentYear && (command.op === "check" || command.op === "note")
                ? {tag: stateTag, year, rowsOnly: fromList} : null;
            ++requestNumber;
            activeRead?.controller.abort();
            const reply = await api("/v1/command", {body: command, receipt});
            if (epoch !== generation || document.hidden) return;
            if (!fromList) writing = false;
            const applied = receipt && state === prior && stateTag === receipt.tag && applyReceipt(reply, command.eventId, year);
            if (applied) calendarWatch = reply.watch;
            if (!applied) {
                await load(year);
            }
            if (epoch !== generation) return;
            const fresh = reopen && currentEvent(reopen);
            if (fresh) {
                el("sheet-body").inert = false;
                const task = command.op === "check" ? command.taskId : undefined;
                const updated = applied && reply.value?.detail;
                if (state.overview && updated?.year === fresh.year && validFullEvent(updated, fresh.id, fresh.ownerId, state)) {
                    detail(updated, task);
                } else {
                    await openCard(fresh, task);
                }
            }
            else if (!fromList) closeSheet();
            notice("Изменение сохранено на компьютере компании.");
        } catch (error) {
            if (epoch === generation && error instanceof Problem && error.message) {
                if (fromList) {
                    try { await load(year); } catch { return; }
                    if (generation === epoch) notice(error.message);
                } else el("sheet-error").textContent = error.message;
                refused = true;
            }
        } finally {
            if (epoch === generation) {
                writing = false;
                if (fromList) {
                    el("list").removeAttribute("aria-busy");
                    control?.removeAttribute("aria-disabled");
                    const focused = document.activeElement;
                    if (!el("sheet").open && mode === "mine" &&
                        (focused === document.body || focused === control || focused?.id === control?.id)) {
                        const next = refused && control?.isConnected ? control : focusRows.map(el).find(Boolean);
                        (next || el("mine")).focus();
                    }
                } else {
                    el("sheet-body").inert = false;
                    el("close").disabled = false;
                }
                if (refused && control instanceof HTMLElement && control.isConnected && el("sheet").open) {
                    control.focus();
                }
            }
        }
    }
    function changeMode(next) { mode = next; limit = pageSize; notice(""); renderList(); }
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
        el("login").hidden = true;
        el("logout").hidden = false;
        el("token").value = "";
        el("connect").disabled = true;
        el("gate-status").textContent = "Подключение…";
        const epoch = generation;
        try { await load(); }
        catch { /* api shows the reason when it locks the view. */ }
        finally { if (epoch === generation) { loginPending = false; el("connect").disabled = !secure; } }
    });
    el("list").addEventListener("click", event => {
        if (writing && el("list").hasAttribute("aria-busy")) {
            event.preventDefault();
            event.stopPropagation();
        }
    }, true);
    el("logout").addEventListener("click", () => clearWorkspace(prompts.login));
    el("all").addEventListener("click", () => changeMode("all"));
    el("mine").addEventListener("click", () => changeMode("mine"));
    el("previous").addEventListener("click", () => { limit = pageSize; refresh(Math.max(1900, currentYear - 1)); });
    el("next").addEventListener("click", () => { limit = pageSize; refresh(Math.min(9999, currentYear + 1)); });
    el("refresh").addEventListener("click", () => { refresh(); });
    el("create").addEventListener("click", () => editor(null));
    el("close").addEventListener("click", closeSheet);
    el("sheet").addEventListener("cancel", (evt) => { evt.preventDefault(); if (!writing) closeSheet(); });

    window.addEventListener("offline", () => clearWorkspace(writing ? prompts.uncertain : prompts.disconnected));
    window.addEventListener("pagehide", () => clearWorkspace(prompts.login));
    document.addEventListener("visibilitychange", () => {
        if (document.hidden) {
            clearWorkspace(writing ? prompts.uncertain : prompts.hidden, !writing && activeRead?.loginRetry !== "waiting");
            return;
        }
        if (credential) refresh();
    });
    function recordCalendarRead(watch) {
        calendarWatch = watch;
        pendingWatch = null;
        lastPolled = performance.now();
        clearTimeout(calendarTimer);
        calendarTimer = setTimeout(pollCalendar, calendarPollMs);
    }
    function scheduleChangedRead(watch) {
        if (calendarWatch === null || watch === calendarWatch || pendingWatch) return;
        if (document.hidden || writing || activeRead || sheetBlocksCalendarRead()) return;
        // The host gives each credential an independent stamp. Spread a burst of
        // visible edits across existing connections, using one wakeup per change.
        // After a listener restart, use the previous stamp to spread re-registration.
        const stamp = watch && watch !== "0" ? watch : calendarWatch;
        const wait = stamp === "0" ? Math.floor(Math.random() * 2400) : parseInt(stamp.slice(0, 8), 16) % 2400;
        pendingWatch = true;
        clearTimeout(calendarTimer);
        calendarTimer = setTimeout(() => {
            pendingWatch = null;
            if (calendarWatch !== watch) pollCalendar(true);
        }, wait);
    }
    function sheetBlocksCalendarRead() {
        return el("sheet").open && (!readOnlyCard || activeDetail !== null);
    }
    async function refreshVisibleCalendar() {
        const card = readOnlyCard;
        const prior = card && currentEvent(card.event);
        await load();
        if (!card || readOnlyCard !== card) return;
        const row = currentEvent(card.event);
        if (!row) { closeSheet(); return; }
        // A single-event delta retains the identity of every untouched row.
        if (row !== prior) await openCard(row, card.taskId, true, card);
    }
    function pollCalendar(changed = false) {
        if (!state || document.hidden || activeRead || sheetBlocksCalendarRead() || writing) return;
        const now = performance.now();
        if (now - lastVerified < leaseMs && (changed || now - lastPolled >= calendarPollMs)) {
            refreshVisibleCalendar().catch(() => {});
        }
    }
    function scheduleHeartbeat() {
        clearTimeout(heartbeatTimer);
        heartbeatTimer = null;
        if (!state || document.hidden || heartbeatRunning) return;
        // Pending requests do not postpone an access check. As with the lease,
        // measure the delay from the start of a confirmed request.
        const delay = lastVerified + heartbeatMs - performance.now();
        heartbeatTimer = setTimeout(heartbeat, Math.max(0, Math.ceil(delay)));
    }
    async function heartbeat() {
        heartbeatTimer = null;
        if (!state || document.hidden) return;
        const now = performance.now();
        if (now - lastVerified >= leaseMs) {
            clearWorkspace(writing ? prompts.uncertain : prompts.disconnected);
            return;
        }
        if (now - lastVerified < heartbeatMs) {
            scheduleHeartbeat();
            return;
        }
        const epoch = generation;
        heartbeatRunning = true;
        try {
            const watch = await api("/v1/session");
            if (generation === epoch) {
                scheduleChangedRead(watch);
                if (!pendingWatch) pollCalendar();
            }
        }
        catch { /* api applies the lock policy. */ }
        finally {
            if (generation === epoch) {
                heartbeatRunning = false;
                scheduleHeartbeat();
            }
        }
    }
    const loopback = ["127.0.0.1", "localhost", "[::1]"].includes(location.hostname);
    secure = window.isSecureContext && (location.protocol === "https:" || (location.protocol === "http:" && loopback));
    const form = el("login");
    if (secure && form.hasAttribute("data-early-gate")) {
        form.dispatchEvent(new Event("calendar-ready"));
    } else {
        el("token").disabled = !secure;
        clearWorkspace(secure ? prompts.login : "Откройте календарь через HTTPS или локальный порт защищённого туннеля.");
    }
})();
