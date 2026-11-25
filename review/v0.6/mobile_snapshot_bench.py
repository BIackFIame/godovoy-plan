"""Compare two local Rust mobile libraries on the same synthetic, offline calendar."""
import argparse
from contextlib import contextmanager
import ctypes
from datetime import date, timedelta
import hashlib
import json
from pathlib import Path
import shutil
from statistics import median
import tempfile
import time


class Library:
    def __init__(self, path):
        self.lib = ctypes.CDLL(str(path.resolve()))
        self.lib.gp_open.argtypes = [ctypes.c_char_p]
        self.lib.gp_open.restype = ctypes.c_void_p
        self.lib.gp_request.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        self.lib.gp_request.restype = ctypes.c_void_p
        self.lib.gp_string_free.argtypes = [ctypes.c_void_p]
        self.lib.gp_string_free.restype = None
        self.lib.gp_close.argtypes = [ctypes.c_void_p]
        self.lib.gp_close.restype = None

    @contextmanager
    def profile(self, path):
        handle = self.lib.gp_open(str(path.resolve()).encode("utf-8"))
        if not handle:
            raise RuntimeError("Не удалось открыть временный профиль")
        try:
            yield handle
        finally:
            self.lib.gp_close(handle)

    def request(self, handle, command):
        ptr = self.lib.gp_request(handle, command)
        if not ptr:
            raise RuntimeError("Ядро не вернуло ответ")
        try:
            return ctypes.string_at(ptr)
        finally:
            self.lib.gp_string_free(ptr)


def command(op, **fields):
    return json.dumps({"op": op, **fields}, ensure_ascii=False).encode("utf-8")


def data(reply):
    envelope = json.loads(reply)
    if envelope["ok"] is not True:
        raise RuntimeError(envelope.get("error"))
    return envelope["data"]


def populate(lib, folder, count, target):
    with lib.profile(folder) as h:
        data(lib.request(h, command("createWorkspace", name="Тестовый офис", employee="Анна")))
        state = data(lib.request(h, command("state", year=target.year)))
        for n in range(count):
            event = command("createEvent", title=f"Подготовка акции {n + 1}",
                            description="Согласовать текст и подготовить рассылку.",
                            date=target.isoformat(), yearly=True, preparationDays=30,
                            ownerId=state["deviceId"], category="Partners",
                            checklist=[{"title": f"Согласовать пункт {i + 1}", "daysBefore": i}
                                       for i in range(8)])
            data(lib.request(h, event))


def measure(lib, profile, year, samples):
    request = command("state", year=year)
    cpu, wall = [], []
    with lib.profile(profile) as h:
        first = data(lib.request(h, request))
        for _ in range(samples):
            start_cpu, start_wall = time.process_time_ns(), time.perf_counter_ns()
            reply = lib.request(h, request)
            wall.append((time.perf_counter_ns() - start_wall) / 1e6)
            cpu.append((time.process_time_ns() - start_cpu) / 1e6)
        final = data(reply)
        if final != first:
            raise RuntimeError("Снимок изменился во время измерения")
        conditional = data(lib.request(h, command("state", year=year, ifToken=final["stateToken"])))
        if conditional != {"unchanged": True}:
            raise RuntimeError("Условное чтение вернуло неожиданный результат")
    del final["stateToken"]
    final.pop("supportsSyncStamp", None)
    return final, {"cpu_ms": median(cpu), "wall_ms": median(wall), "bytes": len(reply)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--events", type=int, default=1000)
    parser.add_argument("--samples", type=int, default=12)
    args = parser.parse_args()
    if not 1 <= args.events <= 2000 or not 1 <= args.samples <= 1000:
        parser.error("Допустимо 1–2000 событий и 1–1000 измерений")
    paths = {"baseline": args.baseline, "candidate": args.candidate}
    libraries = {name: Library(path) for name, path in paths.items()}
    target = date.today() + timedelta(days=7)
    with tempfile.TemporaryDirectory(prefix="godovoy-bench-") as tmp:
        root = Path(tmp)
        populate(libraries["baseline"], root / "seed", args.events, target)
        expected = None
        measurements = []
        for pair, order in enumerate((list(paths), list(reversed(paths)))):
            for name in order:
                copy = root / f"{name}-{pair}"
                shutil.copytree(root / "seed", copy)
                state, stats = measure(libraries[name], copy, target.year, args.samples)
                if len(state["events"]) != args.events or (expected is not None and state != expected):
                    raise RuntimeError("Календари сравниваемых сборок различаются")
                expected = state
                measurements.append({"pair": pair + 1, "build": name, **stats})
        print(json.dumps({"events": args.events, "samples_per_run": args.samples,
                          "sha256": {k: hashlib.sha256(v.read_bytes()).hexdigest() for k, v in paths.items()},
                          "measurements": measurements}, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
