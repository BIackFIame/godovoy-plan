"""Run published Windows executables in an isolated temporary profile on the runner."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import shutil
import threading
import urllib.request
import uuid
import zipfile

def cli(exe, *args):
    result = subprocess.run([exe, *args], capture_output=True, encoding="utf-8", timeout=30)
    if result.returncode:
        raise RuntimeError(f"{exe.name} {args[0]} failed ({result.returncode}): {result.stderr.strip()}")
    return result.stdout.strip()

def extract_release(release, name, programs, directory):
    matches = [a for a in release["assets"] if a["name"] == name]
    assert len(matches) == 1
    asset = matches[0]
    archive = directory / name
    if package_input := os.environ.get("GP_PACKAGE_INPUT"):
        archive = Path(package_input) / name
    else:
        urllib.request.urlretrieve(asset["browser_download_url"], archive)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    assert asset["digest"] == "sha256:" + digest and asset["size"] == archive.stat().st_size
    with zipfile.ZipFile(archive) as package:
        for prog in programs:
            assert package.namelist().count(prog) == 1
            (directory / prog).write_bytes(package.read(prog))
    print(name, digest, flush=True)

def get_authorized(port, key, path):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        conn.request("GET", path, headers={"Authorization": "Bearer " + key})
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()

def server_flow(exe, directory, *, diagnose_test_volume=True):
    vault = directory / "vault"
    try:
        cli(exe, "init", vault, "Windows QA")
    except RuntimeError:
        # Read ACLs only along the temporary test volume; no profile contents.
        for ancestor in ([directory, *directory.parents] if diagnose_test_volume else []):
            print("Test directory permissions:", ancestor, flush=True)
            subprocess.run(["icacls.exe", str(ancestor)], check=False)
        raise
    key = cli(exe, "grant", vault, "Employee", "--read-only").splitlines()[-1]
    process = subprocess.Popen([exe, "serve", vault, "--port", "0"], stderr=subprocess.PIPE,
                               text=True, encoding="utf-8")
    try:
        lines = queue.Queue()
        threading.Thread(target=lambda: lines.put(process.stderr.readline()), daemon=True).start()
        line = lines.get(timeout=15)
        port = int(re.search(r"127\.0\.0\.1:(\d+)", line)[1])
        assert get_authorized(port, key, "/v1/session")[0] == 204
        status, body = get_authorized(port, key, "/v1/state?year=2028")
        assert status == 200 and json.loads(body)["workspaceName"] == "Windows QA"
        cli(exe, "revoke", vault, key.split(".")[0])
        assert get_authorized(port, key, "/v1/session")[0] == 401
    finally:
        process.terminate()
        process.wait(timeout=10)
        process.stderr.close()
    print("PASS: init, read-only session, year and revocation", flush=True)

def main():
    tag = os.environ["RELEASE_TAG"]
    arch = os.environ["PACKAGE_ARCH"]
    assert os.name == "nt" and re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag)
    assert arch in ("x64", "x86", "ARM64")
    url = "https://api.github.com/repos/" + os.environ["GITHUB_REPOSITORY"] + "/releases/tags/" + tag
    if package_input := os.environ.get("GP_PACKAGE_INPUT"):
        release = json.loads((Path(package_input) / "release.json").read_bytes())
        assert release["draft"] is True
    else:
        with urllib.request.urlopen(url, timeout=30) as response:
            release = json.load(response)
        assert not release["draft"]
    assert release["tag_name"] == tag
    version = tag[1:]
    # Inherit the dedicated NTFS volume ACL. TemporaryDirectory installs its own OWNER RIGHTS ACE.
    directory = Path(os.environ["GP_TEST_VOLUME"]) / ("GodovoyQA-" + uuid.uuid4().hex)
    directory.mkdir()
    try:
        extract_release(release, f"GodovoyPlan-{version}-Windows-{arch}.zip",
                        ["godovoy-plan.exe", "godovoy-online.exe"], directory)
        desktop = directory / "godovoy-plan.exe"
        assert cli(desktop, "--version").endswith(version)
        online = directory / "godovoy-online.exe"
        assert cli(online, "--version") == "godovoy-online " + version
        server_flow(online, directory)
        if arch == "x64":
            standalone = directory / "server"
            standalone.mkdir()
            extract_release(release, f"GodovoyServer-{version}-Windows-x64.zip",
                            [online.name], standalone)
            server_flow(standalone / online.name, standalone)
        # Exercise normal user-profile ACLs, including all ancestors of LocalAppData.
        # Create only a unique child directory; do not repair or change inherited rights.
        local = Path(os.environ["LOCALAPPDATA"]) / ("GodovoyQA-" + uuid.uuid4().hex)
        local.mkdir()
        try:
            server_flow(online, local, diagnose_test_volume=False)
            print("PASS: unmodified user LocalAppData accepted", flush=True)
        finally:
            shutil.rmtree(local)
        if os.environ.get("GP_TEST_SCOPE") == "profile":
            return
        for dll in ("opengl32.dll", "libgallium_wgl.dll"):
            shutil.copyfile(Path(os.environ["GP_TEST_RENDERER"]) / dll, directory / dll)
        subprocess.run(["pwsh", "-NoProfile", "-File", Path(__file__).with_name("window-smoke.ps1"),
                        desktop, directory / "desktop-profile"], check=True, timeout=75)

    finally:
        shutil.rmtree(directory)

if __name__ == "__main__":
    main()
