#!/usr/bin/env python3
"""Integration tests: PROTON=/path/to/proton python3 app/linux/test_proton.py.

Creates only a temporary prefix and synthetic clients; never touches real games,
production accounts, Steam prefixes, or the server. Needs MinGW and a display
(xvfb-run is sufficient). Retains logs and executables for inspection.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import time

HERE = Path(__file__).resolve().parent
TEMP = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else tempfile.gettempdir()
ROOT = Path(tempfile.mkdtemp(prefix="forever-proton-test-", dir=os.environ.get("TMPDIR", TEMP)))
PROTON = os.environ["PROTON"]
KEY = "02596F0D0C061A8B30745988FD72C59E29EC367FB0F341F28E0F08D037BAFC69"


def win(path):
    return "Z:" + str(path.resolve()).replace("/", "\\")


def build(source, target, unicode=False):
    subprocess.run(["x86_64-w64-mingw32-gcc", "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-static",
                    *( ["-municode"] if unicode else []), str(source), "-o", str(target)], check=True)


helper = ROOT / "forever-keypatch.exe"
build(HERE / "keypatch.c", helper, unicode=True)
env = dict(os.environ, STEAM_COMPAT_DATA_PATH=str(ROOT / "prefix"),
           STEAM_COMPAT_CLIENT_INSTALL_PATH=str(ROOT / "steam"), WINEDEBUG="-all")
(ROOT / "prefix").mkdir()
(ROOT / "steam").mkdir()
for mode in ("valid", "delayed", "restore", "unknown", "duplicate", "readonly", "respawn", "unrelated"):
    game_dir = ROOT / (mode + " Игра с пробелами")
    game_dir.mkdir()
    game = game_dir / "Wow.exe"
    build(HERE / "test_client.c", game)
    log = game_dir / "keypatch.log"
    runner = game_dir / "proton.log"
    mode_env = dict(env, FOREVER_FIXTURE_MODE="valid" if mode == "unrelated" else mode,
                    STEAM_COMPAT_INSTALL_PATH=str(game_dir))
    with runner.open("wb") as output:
        command = [PROTON, "run", win(helper), win(log), win(game), KEY, "8"]
        if mode == "unrelated":
            process = subprocess.Popen(command, cwd=game_dir, env=mode_env, stdout=output, stderr=subprocess.STDOUT)
            # Start an independent same-path client AFTER this launch. Timestamp/path matching
            # alone would be unsafe; the helper must reject it by ownership as well.
            time.sleep(0.5)
            outsider_env = dict(mode_env, FOREVER_FIXTURE_MODE="unowned")
            subprocess.run([PROTON, "runinprefix", win(game)], cwd=game_dir, env=outsider_env,
                           stdout=output, stderr=subprocess.STDOUT, check=True, timeout=60)
            assert process.wait(timeout=120) == 0
        else:
            subprocess.run(command, cwd=game_dir, env=mode_env, stdout=output, stderr=subprocess.STDOUT,
                           check=True, timeout=120)
    text = log.read_text()
    output = runner.read_text(errors="replace")
    assert "CLOSED exit=0" in text, (mode, text, output[-4000:])
    result = (game_dir / "fixture-result.txt").read_text()
    assert "FIXTURE PASS" in result and "FIXTURE FAIL" not in result, (mode, result)
    ready = sum(line.startswith("READY ") for line in text.splitlines())
    assert ready == (2 if mode == "restore" else 1 if mode in ("valid", "delayed", "respawn", "unrelated") else 0), (mode, text)
    if mode == "unrelated":
        assert "FIXTURE PASS no speculative write" in result, result
    print(f"PASS {mode}: READY={ready}", flush=True)

for name, game, key, group in (("missing", ROOT / "missing.exe", KEY, "8"),
                               ("bad-key", ROOT / "missing.exe", "Z" + KEY[1:], "8"),
                               ("bad-group", ROOT / "missing.exe", KEY, "9")):
    log = ROOT / (name + ".log")
    with (ROOT / (name + "-proton.log")).open("wb") as output:
        process = subprocess.run([PROTON, "run", win(helper), win(log), win(game), key, group],
                                 cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT, timeout=60)
    text = log.read_text()
    assert process.returncode != 0 and "ERROR " in text and "PROCESS " not in text, (name, text)
    print(f"PASS {name}: explicit failure before game launch", flush=True)
print(f"All Proton tests passed. Artifacts: {ROOT}")
