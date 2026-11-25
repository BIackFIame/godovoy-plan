"""Verify the pinned local-QA archive before exposing its two DLLs to smoke.py."""
import hashlib
import hmac
import os
from pathlib import Path
import struct
import zipfile


def install(source, destination, arch, digest):
    machine, optional_magic = {"x86": (0x14c, 0x10b), "x64": (0x8664, 0x20b)}[arch]
    archive = source / f"Mesa-26.3.1-Softpipe-Windows-{arch}-local-QA.zip"
    with archive.open("rb") as stream:
        if not hmac.compare_digest(hashlib.file_digest(stream, "sha256").hexdigest(), digest):
            raise ValueError(f"Incorrect SHA-256 for {archive.name}")
        stream.seek(0)
        with zipfile.ZipFile(stream) as package:
            names = ("opengl32.dll", "libgallium_wgl.dll")
            manifest = package.read("SHA256SUMS.txt").decode("ascii").splitlines()
            libraries = {}
            for name in names:
                if package.namelist().count(name) != 1:
                    raise ValueError(f"Missing or repeated {name}")
                data = package.read(name)
                checksum = hashlib.sha256(data).hexdigest()
                if manifest.count(f"{checksum}  {name}") != 1 or data[:2] != b"MZ":
                    raise ValueError(f"Invalid {name}")
                offset = struct.unpack_from("<I", data, 0x3c)[0]
                if (data[offset:offset+4] != b"PE\0\0" or
                    struct.unpack_from("<H", data, offset+4)[0] != machine or
                    struct.unpack_from("<H", data, offset+24)[0] != optional_magic or
                    not struct.unpack_from("<H", data, offset+22)[0] & 0x2000):
                    raise ValueError(f"Wrong PE architecture for {name}")
                libraries[name] = data
    destination.mkdir()
    for name, data in libraries.items():
        (destination / name).write_bytes(data)
    print(f"Verified {archive.name} SHA-256 {digest}")


if __name__ == "__main__":
    arch = os.environ["PACKAGE_ARCH"]
    destination = Path(os.environ["RUNNER_TEMP"]) / "verified-draft-renderer"
    install(Path(os.environ["GP_PACKAGE_INPUT"]), destination, arch, os.environ[f"MESA_{arch.upper()}_SHA256"])
    with open(os.environ["GITHUB_ENV"], "a", encoding="utf-8") as env:
        print(f"GP_TEST_RENDERER={destination}", file=env)
