export const categories = {Campaign: "Акции", Partners: "Партнёры", Team: "Команда", Operations: "Операции"};

export const validTag = tag => /^(?:W\/)?"[!#-~]{1,128}"$/.test(tag || "");
export const snapshotTag = tag => /^"[0-9a-f]{64}"$/.test(tag || "");

export const uuid = value => typeof value === "string" && /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(value);
const date = value => typeof value === "string" && /^\d{4}-\d{2}-\d{2}$/.test(value);
export const count = value => Number.isSafeInteger(value) && value >= 0;
function validRow(event) {
    return uuid(event?.id) && Number.isInteger(event.year) && event.year >= 1900 && event.year <= 9999 &&
        typeof event.title === "string" && Object.hasOwn(categories, event.category) && date(event.date) &&
        (event.ownerId === null || uuid(event.ownerId));
}
export function validOverview(event, personal) {
    return validRow(event) && count(event.completed) && count(event.total) && event.completed <= event.total &&
        !Object.hasOwn(event, "note") && !Object.hasOwn(event, "description") && !Object.hasOwn(event, "checklist") &&
        (event.next === undefined || (personal && uuid(event.next?.id) &&
            typeof event.next.title === "string" && date(event.next.dueDate)));
}
export function validFullEvent(event, id, owner, calendar) {
    if (!validRow(event) || event.id !== id || typeof event.note !== "string" || typeof event.description !== "string" ||
        !date(event.preparationDate) || !count(event.preparationDays) || typeof event.yearly !== "boolean" ||
        typeof event.revision !== "string" || !/^\d+:[a-f0-9-]{36}$/.test(event.revision) ||
        !Array.isArray(event.checklist) || (calendar.assignedOnly && event.ownerId !== owner) ||
        (calendar.allowFreeText === false && (event.note !== "" || event.description !== ""))) return false;
    const ids = new Set();
    for (const task of event.checklist) {
        if (!uuid(task?.id) || ids.has(task.id) || typeof task.title !== "string" ||
            typeof task.done !== "boolean" || !count(task.daysBefore) || !date(task.dueDate)) return false;
        ids.add(task.id);
    }
    return true;
}

function unpackCard(tuple) {
    if (!Array.isArray(tuple) || tuple.length !== 13 || !Array.isArray(tuple[10])) return null;
    const [id, year, title, description, category, date, yearly, ownerId, preparationDays,
        preparationDate, items, note, revision] = tuple;
    const checklist = [];
    for (const item of items) {
        if (!Array.isArray(item) || item.length !== 5) return null;
        const [id, title, daysBefore, dueDate, done] = item;
        checklist.push({id, title, daysBefore, dueDate, done});
    }
    return {id, year, title, description, category, date, yearly, ownerId, preparationDays,
        preparationDate, checklist, note, revision};
}
export function decodeFullCardsInPlace(next) {
    if (next.cardFormat !== "tuple-v1" || next.overview !== undefined || next.rowFormat !== undefined ||
        next.eventCount !== undefined || !Array.isArray(next.events) || !Array.isArray(next.tasks) ||
        (next.assignedOnly && (next.events.length || next.tasks.length) && !uuid(next.assignedOwnerId))) return false;
    const decode = tuple => {
        const event = unpackCard(tuple);
        return event && validFullEvent(event, event.id, next.assignedOwnerId, next) ? event : null;
    };
    for (let i = 0; i < next.events.length; i++) {
        const row = decode(next.events[i]);
        if (!row || row.year !== next.year) return false;
        next.events[i] = row;
    }
    for (let i = 0; i < next.tasks.length; i++) {
        const value = next.tasks[i];
        const row = Number.isSafeInteger(value) ? next.events[value] : decode(value);
        if (!row) return false;
        next.tasks[i] = row;
    }
    return true;
}
export function expandOverviewRow(row, employees) {
    if (!Array.isArray(row) || row.length !== 9) return null;
    const [id, year, title, date, category, owner, completed, total, following] = row;
    const ownerId = Number.isSafeInteger(owner) && owner >= 0
        ? employees[owner]?.id : owner;
    if (ownerId !== null && !uuid(ownerId)) return null;
    const named = {id, year, title, date, category, ownerId, completed, total};
    if (following !== null) named.next = following;
    return named;
}
export function continuedYear(response, read, prior) {
    const chunk = response.value;
    const after = read.after;
    const bodyTag = response.httpTag?.replace(/^W\//, "");
    if (!after || !prior || !chunk || prior.assignedOnly || prior.rowFormat !== "tuple-v2" ||
        response.base !== undefined || response.continued !== after.tag || response.advertisedSnapshot !== after.snapshot ||
        !snapshotTag(bodyTag) || bodyTag === after.tag || bodyTag === after.snapshot || bodyTag === response.pageTag ||
        prior.events.length !== after.size || chunk.year !== read.year ||
        chunk.offset !== after.size || chunk.eventCount !== prior.eventCount || !Array.isArray(chunk.events)) return null;
    const target = Math.min(2 * after.size, prior.eventCount);
    const partial = target < prior.eventCount;
    if ((partial && read.prefix !== target) || chunk.events.length !== target - after.size ||
        (partial ? (!snapshotTag(response.pageTag) || response.pageTag === after.snapshot || response.pageTag === after.tag)
                 : response.pageTag !== null)) return null;
    const ids = new Set(prior.events.map(row => row.id));
    const added = [];
    let previous = prior.events[after.size - 1];
    for (const tuple of chunk.events) {
        const row = expandOverviewRow(tuple, prior.employees);
        if (!validOverview(row, false) || row.year !== read.year || ids.has(row.id)) return null;
        if (row.date < previous.date || (row.date === previous.date && row.id <= previous.id)) return null;
        ids.add(row.id);
        added.push(row);
        previous = row;
    }
    const next = {...prior, events: prior.events.concat(added)};
    if (!partial) delete next.eventCount;
    return next;
}
