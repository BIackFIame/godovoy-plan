"use strict";

(() => {
    const form = document.getElementById("login");
    const token = document.getElementById("token");
    const connect = document.getElementById("connect");
    const status = document.getElementById("gate-status");
    const instructions = status.textContent;
    const loopback = ["127.0.0.1", "localhost", "[::1]"].includes(location.hostname);
    if (!window.isSecureContext || !(location.protocol === "https:" || (location.protocol === "http:" && loopback))) return;

    const waitMs = 4500;
    let deadline = 0;
    let expires = null;
    function cancel() {
        clearTimeout(expires);
        expires = null;
        token.value = "";
        status.textContent = instructions;
    }

    token.value = "";
    token.disabled = false;
    connect.disabled = false;
    form.dataset.earlyGate = "";
    function submit(event) {
        event.preventDefault();
        if (document.hidden) return;
        clearTimeout(expires);
        deadline = performance.now() + waitMs;
        expires = setTimeout(() => {
            cancel();
            status.textContent = "Страница загружается. Введите ключ ещё раз после загрузки.";
        }, waitMs);
        status.textContent = "Подключение…";
    }
    function edit() {
        clearTimeout(expires);
        expires = null;
        status.textContent = instructions;
    }
    function loadFailure(event) {
        if (event.target?.id !== "calendar-program") return;
        cancel();
        token.disabled = true;
        connect.disabled = true;
        status.textContent = "Программа календаря не загрузилась. Откройте форму входа заново.";
        const link = document.getElementById("reload-calendar");
        link.hidden = false;
        if (!document.hidden && navigator.onLine && location.search !== link.search) {
            location.replace(link.href);
        }
    }
    window.addEventListener("error", loadFailure, true);
    const listeners = [
        [form, "submit", submit],
        [token, "input", edit],
        [window, "pagehide", cancel],
        [window, "offline", cancel],
        [document, "visibilitychange", () => { if (document.hidden) cancel(); }],
    ];
    for (const [target, type, handler] of listeners) target.addEventListener(type, handler);
    form.addEventListener("calendar-ready", () => {
        window.removeEventListener("error", loadFailure, true);
        for (const [target, type, handler] of listeners) target.removeEventListener(type, handler);
        delete form.dataset.earlyGate;
        if (document.hidden || (expires !== null && performance.now() >= deadline)) cancel();
        if (expires !== null) {
            clearTimeout(expires);
            connect.click();
        }
    }, {once: true});
})();
