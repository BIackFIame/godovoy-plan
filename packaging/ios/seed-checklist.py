"""Create and verify a synthetic on-disk profile through the production mobile C ABI."""
import ctypes as ct
from datetime import date, timedelta
import json
import os
from pathlib import Path
import sys
import uuid

library, folder = sys.argv[1:]
folder = Path(folder).absolute()
assert uuid.UUID(folder.name)
folder.mkdir(mode=0o700, parents=False, exist_ok=False)
lib = ct.CDLL(library)
lib.gp_open.argtypes = [ct.c_char_p]
lib.gp_open.restype = ct.c_void_p
lib.gp_open_error.argtypes = []
lib.gp_open_error.restype = ct.c_void_p
lib.gp_close.argtypes = [ct.c_void_p]
lib.gp_close.restype = None
lib.gp_request.argtypes = [ct.c_void_p, ct.c_char_p]
lib.gp_request.restype = ct.c_void_p
lib.gp_string_free.argtypes = [ct.c_void_p]
lib.gp_string_free.restype = None


def read_owned(ptr):
    if not ptr:
        raise RuntimeError("Rust core returned NULL")
    try:
        return ct.string_at(ptr).decode()
    finally:
        lib.gp_string_free(ptr)


def open_profile():
    handle = lib.gp_open(os.fsencode(folder))
    if not handle:
        raise RuntimeError(read_owned(lib.gp_open_error()))
    return handle


def call(op, **fields):
    fields["op"] = op
    response = json.loads(read_owned(lib.gp_request(handle, json.dumps(fields).encode())))
    assert response["ok"], response.get("error")
    return response.get("data")


event_date = date.today() + timedelta(days=5)
handle = open_profile()
try:
    call("createWorkspace", name="Fixture team", employee="Maria")
    state = call("state", year=event_date.year)
    assert state["lanEnabled"] is False
    call("createEvent", title="Partner briefing", date=event_date.isoformat(), yearly=False,
         ownerId=state["deviceId"], category="Partners", preparationDays=7, description="",
         checklist=[{"title": f"Partner packet {n:02}", "daysBefore": {35: 6, 14: 5}.get(n, 0)} for n in range(1, 41)])
finally:
    lib.gp_close(handle)

handle = open_profile()
try:
    state = call("state", year=event_date.year)
    event, = state["tasks"]
    rows = event["checklist"]
    assert len(rows) == 40 and rows[34]["title"] == "Partner packet 35"
    assert all(not row["done"] for row in rows)
    assert min(rows, key=lambda row: row["dueDate"])["id"] == rows[34]["id"]
    reminders = {r["taskId"]: r["title"] for r in state["reminders"] if r.get("taskId")}
    assert reminders == {rows[i]["id"]: "Partner briefing · " + rows[i]["title"] for i in (13, 34)}, reminders
finally:
    lib.gp_close(handle)
print("Validated 40 persisted items; #35 next, independent reminders for #14 and #35.")
