import assert from "node:assert/strict";
import test from "node:test";
import {continuedYear, decodeFullCardsInPlace, validFullEvent} from "./calendar-wire.mjs";

const id = "00000000-0000-0000-0000-000000000001";
const owner = "00000000-0000-0000-0000-000000000002";
const task = "00000000-0000-0000-0000-000000000003";
const day = "2028-01-01";
const card = [id, 2028, "Событие", "", "Team", day, true, owner, 0, day,
    [[task, "Поздравить", 0, day, false]], "", "1:" + id];

test("a compact card and a personal row share one decoded event within the supplied calendar", () => {
    const calendar = {year: 2028, cardFormat: "tuple-v1", events: [card], tasks: [0],
        assignedOnly: true, assignedOwnerId: owner, allowFreeText: false};
    assert.ok(decodeFullCardsInPlace(calendar));
    const event = calendar.events[0];
    assert.equal(calendar.tasks[0], event);
    assert.equal(event.checklist[0].id, task);
    assert.equal(validFullEvent(event, id, task, calendar), false);
    assert.equal(validFullEvent({...event, note: "Запись"}, id, owner, calendar), false);
    assert.equal(validFullEvent({...event, checklist: [event.checklist[0], event.checklist[0]]}, id, owner, calendar), false);
});

test("a continuation only appends ordered rows from the same snapshot", () => {
    const tag = digit => '"' + digit.repeat(64) + '"';
    const row = {id, date: day};
    const prior = {events: [row], employees: [{id: owner}], eventCount: 2, rowFormat: "tuple-v2"};
    const read = {year: 2028, after: {tag: tag("1"), snapshot: tag("2"), size: 1}};
    const response = {continued: tag("1"), advertisedSnapshot: tag("2"), httpTag: tag("3"), pageTag: null,
        value: {year: 2028, offset: 1, eventCount: 2, events: [[task, 2028, "Дальше", day, "Team", 0, 0, 1, null]]}};
    const next = continuedYear(response, read, prior);
    assert.equal(next.events[0], row);
    assert.equal(next.events[1].ownerId, owner);
    assert.equal(next.eventCount, undefined);
    assert.equal(prior.events.length, 1);
    assert.equal(continuedYear({...response, advertisedSnapshot: tag("4")}, read, prior), null);
    assert.equal(continuedYear({...response, value: {...response.value, events: [[id, 2028, "Повтор", day, "Team", 0, 0, 1, null]]}}, read, prior), null);
});
