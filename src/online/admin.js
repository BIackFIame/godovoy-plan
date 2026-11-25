"use strict";

(() => {
    const el = (id) => document.getElementById(id);
    const fields = {
        organization: "Название организации", contact: "Служебный контакт ответственного",
        purpose: "Цели обработки", legalBasis: "Правовые основания",
        hosting: "Размещение сервера", backupLocation: "Размещение резервных копий",
        retention: "Сроки хранения и удаления", securityAssessment: "Оценка защиты и внутренние документы"
    };
    const rules = ["russia", "belarus", "allowRemote", "allowFreeText"];
    const secure = location.protocol === "http:" && ["127.0.0.1", "[::1]", "localhost"].includes(location.hostname);
    let secret = "", epoch = 0, active = false, backupTimer = null;
    const controllers = new Set();
    let policy = null, operator = null, employees = [], selected = null;
    let hadProfile = false, backupSequence = 0, verifying = false;

    for (const [key, caption] of Object.entries(fields)) {
        const label = document.createElement("label");
        label.textContent = caption;
        const value = document.createElement("textarea");
        value.id = "org-" + key;
        value.rows = 2;
        value.maxLength = 1000;
        value.spellcheck = false;
        label.append(value);
        el("operator-fields").append(label);
    }

    function reset(message) {
        epoch++;
        for (const controller of controllers) controller.abort();
        controllers.clear();
        clearTimeout(backupTimer);
        backupSequence++;
        el("start-backup").disabled = true;
        el("backup-location").hidden = true;
        secret = "";
        active = false;
        policy = operator = selected = null;
        employees = [];
        for (const form of document.forms) form.reset();
        for (const detail of document.querySelectorAll("details")) detail.open = false;
        for (const id of ["evidence-values", "staff-list", "staff-count", "person-title", "revoke-count",
            "policy-status", "operator-status", "report-status", "staff-status", "backup-status"]) el(id).replaceChildren();
        el("person").hidden = el("logout").hidden = el("workspace").hidden = true;
        el("workspace").inert = false;
        el("gate").hidden = false;
        el("enter").disabled = !secure;
        el("login-status").textContent = message;
    }

    class RequestError extends Error {
        constructor(status, message) { super(message); this.status = status; }
    }
    async function request(path, method = "GET", body) {
        const sent = epoch;
        if (!secret || document.hidden) throw new Error();
        const abort = new AbortController();
        controllers.add(abort);
        const timer = setTimeout(() => abort.abort(), 8000);
        try {
            const headers = {Authorization: "Bearer " + secret};
            if (body !== undefined) headers["Content-Type"] = "application/json";
            const res = await fetch(path, {method, headers, body: JSON.stringify(body), mode: "same-origin",
                cache: "no-store", referrerPolicy: "no-referrer", credentials: "omit", redirect: "error", signal: abort.signal});
            if (res.status === 401 || res.status === 403) {
                if (sent === epoch) reset("Доступ к панели закрыт. Запустите панель на сервере и введите действующий ключ.");
                throw new Error();
            }
            const obj = res.status === 204 ? null : await res.json();
            if (sent !== epoch || document.hidden) throw new Error();
            if (!res.ok) throw new RequestError(res.status, typeof obj?.error === "string" ? obj.error : "Сервер отклонил запрос.");
            return obj;
        } catch (err) {
            if (sent === epoch && !(err instanceof RequestError)) {
                const message = method === "GET" ? "Связь с панелью прервана. Подключитесь снова."
                    : "Связь прервана. Войдите снова и проверьте, успел ли сервер сохранить изменение.";
                reset(message);
            }
            throw err;
        } finally {
            clearTimeout(timer);
            controllers.delete(abort);
        }
    }
    async function action(status, fn) {
        if (active || document.hidden) return;
        const sent = epoch;
        active = true;
        el("workspace").inert = true;
        el("enter").disabled = true;
        el(status).textContent = "Подождите…";
        try {
            const message = await fn();
            if (sent === epoch) el(status).textContent = message || "";
        } catch (e) {
            if (sent === epoch) el(status).textContent = e.message || "Проверьте состояние сервера и повторите операцию.";
        } finally {
            if (sent === epoch) {
                active = false;
                el("workspace").inert = false;
                el("enter").disabled = !secure;
            }
        }
    }
    const hasProfile = (s) => s.russia || s.belarus;
    function readRules() {
        return Object.fromEntries(rules.map(k => [k, el(k).checked]));
    }
    function syncRules() {
        const current = readRules();
        const enabled = hasProfile(current);
        el("exceptions").hidden = !enabled;
        const removing = hasProfile(policy.settings) && !enabled;
        el("removal").hidden = !removing;
        if (!removing) el("confirm-removal").checked = false;
        const dirty = rules.some(k => current[k] !== policy.settings[k]);
        el("save-policy").disabled = !dirty || (removing && !el("confirm-removal").checked);
        hadProfile = enabled;
    }
    function renderPolicy(record) {
        if (!Number.isSafeInteger(record?.revision) || !rules.every(k => typeof record.settings?.[k] === "boolean")) {
            throw new Error("Сервер вернул неизвестный формат настроек.");
        }
        policy = record;
        for (const k of rules) el(k).checked = record.settings[k];
        el("confirm-removal").checked = false;
        syncRules();
    }
    el("policy").addEventListener("change", () => {
        if (!policy) return;
        if (hasProfile(readRules()) !== hadProfile) el("allowRemote").checked = el("allowFreeText").checked = false;
        syncRules();
        el("policy-status").textContent = rules.some(k => el(k).checked !== policy.settings[k])
            ? "Изменения вступят в силу после сохранения." : "";
    });
    function discardEvidence() {
        el("evidence").open = false;
        el("evidence-values").replaceChildren();
        el("report-status").textContent = "";
    }
    async function persist(route, value, render) {
        try {
            const result = await request(route, "PUT", value);
            render(result ?? await request(route));
            discardEvidence();
            return "Сохранено на сервере.";
        } catch (e) {
            if (e.status !== 409) throw e;
            render(await request(route));
            return "Сервер отклонил изменение. Загружены действующие значения. Проверьте их перед сохранением.";
        }
    }
    const onSubmit = (id, fn) => el(id).addEventListener("submit", event => { event.preventDefault(); fn(); });
    onSubmit("login", () => {
        if (!secure || active || document.hidden) return;
        const value = el("secret").value.trim();
        el("secret").value = "";
        if (!value || !/^[\x21-\x7e]+$/.test(value)) {
            el("login-status").textContent = "Введите временный ключ целиком, без пробелов.";
            return;
        }
        secret = value;
        action("login-status", async () => {
            renderPolicy(await request("/v1/settings"));
            el("gate").hidden = true;
            el("workspace").hidden = el("logout").hidden = false;
            el("workspace").inert = false;
            el("page-title").focus();
        });
    });
    onSubmit("policy", () => {
        if (!policy || el("save-policy").disabled) return;
        action("policy-status", () => persist("/v1/settings", {revision: policy.revision, settings: readRules()}, renderPolicy));
    });
    function renderOperator(record) {
        if (!Number.isSafeInteger(record?.revision) || !Object.keys(fields).every(k => typeof record.details?.[k] === "string")) {
            throw new Error("Сервер вернул неизвестный формат сведений организации.");
        }
        operator = record;
        for (const k of Object.keys(fields)) el("org-" + k).value = record.details[k];
    }
    el("organization").addEventListener("toggle", () => {
        if (!el("organization").open || !secret || operator) return;
        action("operator-status", async () => { renderOperator(await request("/v1/operator")); });
    });
    onSubmit("operator", () => {
        if (!operator) return;
        const details = Object.fromEntries(Object.keys(fields).map(k => [k, el("org-" + k).value]));
        action("operator-status", () => persist("/v1/operator", {revision: operator.revision, details}, renderOperator));
    });

    function staffList() {
        const filter = el("staff-search").value.trim().toLocaleLowerCase("ru");
        const matches = employees.filter(p => p.name.toLocaleLowerCase("ru").includes(filter));
        el("staff-count").textContent = matches.length > 80 ? "Показаны первые 80. Уточните имя для поиска."
            : (matches.length ? "Найдено сотрудников — " + matches.length : "Сотрудники не найдены.");
        const list = el("staff-list");
        list.replaceChildren();
        for (const person of matches.slice(0, 80)) {
            const li = document.createElement("li"), button = document.createElement("button");
            button.type = "button";
            button.textContent = person.name + " · " + person.id.slice(0, 8);
            button.setAttribute("aria-pressed", String(selected?.id === person.id));
            button.addEventListener("click", () => { showPerson(person); staffList(); el("staff-status").textContent = ""; });
            li.append(button);
            list.append(li);
        }
    }
    function showPerson(person) {
        selected = person;
        el("person").hidden = !person;
        el("person-title").textContent = person?.name || "";
        el("new-name").value = person?.name || "";
        el("assigned-only").checked = person?.assignedOnly === true;
        el("save-scope").disabled = true;
        el("revoke-count").textContent = person ? "Действующих ключей — " + person.keys + "." : "";
        el("confirm-name").value = "";
        el("revoke-button").disabled = true;
        el("revoke").hidden = !person?.keys;
    }
    async function reloadEmployees(id) {
        const result = await request("/v1/employees");
        if (!Array.isArray(result?.employees) || !result.employees.every(p => typeof p.id === "string" &&
            typeof p.name === "string" && Number.isSafeInteger(p.keys))) throw new Error("Сервер вернул неизвестный формат реестра.");
        employees = result.employees;
        showPerson(employees.find(p => p.id === id) || null);
        staffList();
    }
    el("employees").addEventListener("toggle", () => {
        if (el("employees").open && secret) action("staff-status", () => reloadEmployees(null));
    });
    el("staff-search").addEventListener("input", () => { showPerson(null); staffList(); });
    el("confirm-name").addEventListener("input", () => {
        el("revoke-button").disabled = !selected?.keys || el("confirm-name").value !== selected.name;
    });
    onSubmit("rename", () => {
        if (!selected) return;
        const {id, name: previousName} = selected;
        const name = el("new-name").value.trim();
        if (!name || name === previousName) { el("staff-status").textContent = "Введите новое имя."; return; }
        action("staff-status", async () => {
            try {
                await request("/v1/employees/" + encodeURIComponent(id) + "/name", "PUT", {previousName, name});
            } catch (e) {
                if (e.status !== 409) throw e;
                await reloadEmployees(id);
                return "Загружено текущее имя. Проверьте его перед повторным сохранением.";
            }
            await reloadEmployees(id);
            return "Имя в рабочем реестре сохранено.";
        });
    });
    onSubmit("revoke", () => {
        if (!selected?.keys || el("confirm-name").value !== selected.name) return;
        const id = selected.id;
        action("staff-status", async () => {
            const result = await request("/v1/employees/" + encodeURIComponent(id) + "/revoke", "POST", {});
            if (!Number.isSafeInteger(result?.revoked)) throw new Error("Проверьте число действующих ключей на сервере.");
            await reloadEmployees(id);
            discardEvidence();
            return "Отозвано ключей — " + result.revoked + ".";
        });
    });
    el("assigned-only").addEventListener("change", () => {
        el("save-scope").disabled = !selected || el("assigned-only").checked === selected.assignedOnly;
    });
    onSubmit("scope", () => {
        if (!selected || el("save-scope").disabled) return;
        const id = selected.id, assignedOnly = el("assigned-only").checked;
        action("staff-status", async () => {
            await request("/v1/employees/" + encodeURIComponent(id) + "/scope", "PUT", {assignedOnly});
            await reloadEmployees(id);
            discardEvidence();
            return "Видимость событий сохранена для ключей сотрудника.";
        });
    });

    function backupState(result) {
        if (!["idle", "running", "verified", "failed"].includes(result?.status) || typeof result.available !== "boolean") {
            throw new Error("Сервер вернул неизвестное состояние резервной копии.");
        }
        clearTimeout(backupTimer);
        el("backup-location").hidden = result.available;
        el("start-backup").disabled = !result.available || result.status === "running";
        if (result.status === "running") {
            const expected = epoch;
            backupTimer = setTimeout(() => pollBackup(expected), 2000);
            return "Сервер создаёт и проверяет копию…";
        }
        if (result.status === "verified") {
            discardEvidence();
            const date = new Date(result.finishedAt || NaN);
            return Number.isFinite(date.getTime())
                ? "Копия проверена " + date.toLocaleString("ru") + "." : "Копия проверена.";
        }
        if (result.status === "failed") return "Проверка копии завершилась ошибкой. Проверьте каталог на сервере.";
        return result.available ? "Можно создать новую копию." : "Каталог для копий не задан.";
    }
    async function pollBackup(expected) {
        if (expected !== epoch || !secret || document.hidden) return;
        const sequence = ++backupSequence;
        try {
            const result = await request("/v1/backup");
            if (expected === epoch && sequence === backupSequence) el("backup-status").textContent = backupState(result);
        } catch (e) {
            if (expected === epoch && sequence === backupSequence) el("backup-status").textContent = e.message || "Не удалось проверить состояние копии.";
        }
    }
    el("backup").addEventListener("toggle", () => {
        if (el("backup").open && secret) pollBackup(epoch);
    });
    el("start-backup").addEventListener("click", () => action("backup-status", async () => {
        el("start-backup").disabled = true;
        clearTimeout(backupTimer);
        backupSequence++;
        try {
            return backupState(await request("/v1/backup", "POST", {}));
        } catch (e) {
            if (e instanceof RequestError) await pollBackup(epoch);
            throw e;
        }
    }));

    el("evidence").addEventListener("toggle", () => {
        if (!el("evidence").open || !secret) return;
        action("report-status", async () => {
            const r = await request("/v1/report");
            const items = [
                ["Версия сервера", r.serverVersion], ["Действующие ключи", r.credentials.active],
                ["Ключи только для чтения", r.credentials.readOnly], ["Месяцев в журнале", r.audit.months],
                ["Последняя проверка копии", r.backup.lastVerifiedAt || "Нет записи о проверке"]
            ];
            el("evidence-values").replaceChildren();
            for (const [label, value] of items) {
                const term = document.createElement("dt"), detail = document.createElement("dd");
                term.textContent = label;
                detail.textContent = String(value);
                el("evidence-values").append(term, detail);
            }
        });
    });
    el("download").addEventListener("click", () => action("report-status", async () => {
        const report = await request("/v1/report");
        const blob = new Blob([JSON.stringify(report, null, 2) + "\n"], {type: "application/json"});
        const url = URL.createObjectURL(blob);
        const a = document.createElement("a");
        a.href = url;
        a.download = "godovoy-plan-report-" + new Date().toISOString().slice(0, 10) + ".json";
        document.body.append(a);
        a.click();
        a.remove();
        setTimeout(() => URL.revokeObjectURL(url), 1000);
        return "Отчёт передан браузеру для сохранения.";
    }));
    el("logout").addEventListener("click", () => { reset("Вы вышли из панели."); el("secret").focus(); });
    window.addEventListener("pagehide", () => reset(""));
    window.addEventListener("offline", () => reset("Связь с панелью прервана."));
    document.addEventListener("visibilitychange", () => { if (document.hidden) reset("Для продолжения снова введите ключ администратора."); });
    setInterval(async () => {
        if (!policy || !secret || active || verifying || document.hidden) return;
        verifying = true;
        const sent = epoch;
        try { await request("/v1/settings"); }
        catch { if (sent === epoch) reset("Панель недоступна. Подключитесь к серверу снова."); }
        finally { verifying = false; }
    }, 10000);
    reset(secure ? "" : "Откройте локальный адрес, напечатанный при запуске панели на сервере.");
})();
