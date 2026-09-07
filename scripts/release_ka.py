# -*- coding: utf-8 -*-
"""Выпуск сборки bsl-indexer для контура агента 1С: тесты -> тег v<база>-ka.N -> push -> GitHub Actions
(.github/workflows/release-ka.yml, windows-latest) собирает и публикует pre-release; скрипт ждет прогон.
--local — прежний путь: cargo build --release и релиз с этой машины.

Сборочная ветка build/ka-combined объединяет PR-ветки (формы/СКД, scope у грепов) поверх релиза автора.
Версия сборки — версия из Cargo.toml плюс суффикс `-ka.<N>` (SemVer pre-release), чтобы не занимать
номера автора; когда PR приняты, контур переходит на релиз автора. Релиз публикуется в СВОЙ форк
(remote по умолчанию `ka`), не в репозиторий автора (origin).

Использование:
    python scripts/release_ka.py                 # следующий N по тегам, тесты, тег, push, ожидание Actions
    python scripts/release_ka.py --local         # сборка и публикация с этой машины (без Actions)
    python scripts/release_ka.py --build 1       # явный номер сборки
    python scripts/release_ka.py --dry-run       # без тега и публикации
    python scripts/release_ka.py --remote ka --skip-tests

@layer infra
@tags code-index, релиз, cargo, gh
"""

from __future__ import annotations

import argparse
import io
import os
import re
import shutil
import subprocess
import sys

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = "bsl-indexer.exe"
GH_CANDIDATES = ["gh", r"C:\Program Files\GitHub CLI\gh.exe"]


def run(cmd, **kw):
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace", **kw)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def find_gh() -> str:
    for c in GH_CANDIDATES:
        path = shutil.which(c) if not os.path.isabs(c) else (c if os.path.isfile(c) else None)
        if path:
            return path
    raise SystemExit("GitHub CLI не найден: winget install --id GitHub.cli -e; затем gh auth login")


def base_version() -> str:
    text = io.open(os.path.join(ROOT, "Cargo.toml"), encoding="utf-8").read()
    m = re.search(r'^version = "([^"]+)"', text, re.M)
    if not m:
        raise SystemExit("В Cargo.toml не найдена version")
    return m.group(1)


def next_build(base: str) -> int:
    _, out = run(["git", "tag", "-l", f"v{base}-ka.*"])
    nums = [int(m.group(1)) for t in out.split() if (m := re.search(r"-ka\.(\d+)$", t))]
    return max(nums, default=0) + 1


def выпуск_через_actions(a, branch: str, version: str, tag: str) -> int:
    """Тег на HEAD, push в свой форк, ожидание workflow «Release KA build», ассеты релиза."""
    if a.dry_run:
        print("dry-run: тег и push не делаются; сборку и pre-release сделал бы GitHub Actions по тегу", tag)
        return 0
    gh = find_gh()
    code, url = run(["git", "remote", "get-url", a.remote])
    if code != 0 or "github.com" not in url:
        print(f"Нет remote «{a.remote}» на GitHub: git remote add {a.remote} https://github.com/<владелец>/code-index-mcp.git")
        return 2
    repo = re.sub(r"^.*github\.com[:/]", "", url.strip()).removesuffix(".git")
    _, existing = run(["git", "tag", "-l", tag])
    if existing.strip():
        print(f"Тег {tag} уже есть — укажите --build с другим номером")
        return 2
    run(["git", "tag", "-a", tag, "-m", f"bsl-indexer {version} (сборка контура 1С)"])
    code, out = run(["git", "push", a.remote, "HEAD", tag])
    if code != 0:
        print(out[-2000:])
        return 1
    print(f"push выполнен; GitHub Actions ({repo}) собирает bsl-indexer: тесты и release-сборка на windows-latest, 15-30 мин…")
    run_id = ""
    for _ in range(12):
        code, out = run([gh, "run", "list", "--repo", repo, "--workflow", "Release KA build", "--branch", tag,
                         "--limit", "1", "--json", "databaseId", "--jq", ".[0].databaseId"])
        run_id = out.strip()
        if run_id.isdigit():
            break
        subprocess.run([sys.executable, "-c", "import time; time.sleep(5)"])
    if not run_id.isdigit():
        print("Прогон не найден за минуту — проверьте вкладку Actions форка (включены ли workflows)")
        return 1
    code, out = run([gh, "run", "watch", run_id, "--repo", repo, "--exit-status", "--interval", "20"])
    print(out.strip()[-1500:])
    if code != 0:
        print(f"Прогон {run_id} упал: gh run view {run_id} --repo {repo} --log-failed")
        return 1
    code, out = run([gh, "release", "view", tag, "--repo", repo, "--json", "url,assets", "--jq",
                     '.url, (.assets[] | .name + " " + (.size|tostring) + " байт")'])
    print(out.strip())
    return 0 if code == 0 else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--build", type=int)
    ap.add_argument("--remote", default="ka", help="remote своего форка на GitHub (не origin автора)")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--skip-tests", action="store_true")
    ap.add_argument("--local", action="store_true", help="собрать и опубликовать с этой машины, не ждать Actions")
    a = ap.parse_args()

    _, out = run(["git", "status", "--porcelain"])
    if out.strip():
        print("Рабочее дерево не чистое — релиз только из зафиксированного состояния:\n" + out)
        return 2
    _, branch = run(["git", "branch", "--show-current"])
    base = base_version()
    n = a.build or next_build(base)
    version = f"{base}-ka.{n}"
    tag = "v" + version
    print(f"ветка {branch.strip()}, версия {version}, тег {tag}")

    if not a.skip_tests:
        print("cargo test --workspace --all-targets …")
        code, out = run(["cargo", "test", "--workspace", "--all-targets"])
        results = [l for l in out.splitlines() if l.startswith("test result")]
        passed = sum(int(re.search(r"(\d+) passed", l).group(1)) for l in results if "passed" in l)
        failed = sum(int(re.search(r"(\d+) failed", l).group(1)) for l in results if "failed" in l)
        print(f"  тестов пройдено {passed}, упало {failed}")
        if code != 0 or failed:
            print(out[-3000:])
            return 1

    if not a.local:
        return выпуск_через_actions(a, branch.strip(), version, tag)

    print("cargo build --release -p bsl-indexer --features enrichment …")
    code, out = run(["cargo", "build", "--release", "-p", "bsl-indexer", "--features", "enrichment"])
    exe = os.path.join(ROOT, "target", "release", EXE)
    if code != 0 or not os.path.isfile(exe):
        print(out[-3000:])
        return 1
    _, ver_out = run([exe, "--version"])
    print(f"  {exe} ({os.path.getsize(exe) // 1048576} МБ), --version: {ver_out.strip()}")

    if a.dry_run:
        print("dry-run: тег и релиз не создаются")
        return 0

    gh = find_gh()
    code, url = run(["git", "remote", "get-url", a.remote])
    if code != 0 or "github.com" not in url:
        print(f"Нет remote «{a.remote}» на GitHub: git remote add {a.remote} https://github.com/<владелец>/code-index-mcp.git")
        return 2
    repo = re.sub(r"^.*github\.com[:/]", "", url.strip()).removesuffix(".git")
    _, existing = run(["git", "tag", "-l", tag])
    if existing.strip():
        print(f"Тег {tag} уже есть — укажите --build с другим номером")
        return 2
    run(["git", "tag", "-a", tag, "-m", f"bsl-indexer {version} (сборка контура 1С)"])
    code, out = run(["git", "push", a.remote, "HEAD", tag])
    if code != 0:
        print(out[-2000:])
        return 1
    notes = (f"Сборка bsl-indexer для контура агента 1С: ветка {branch.strip()} (релиз автора {base} + PR-ветки "
             f"форм/СКД и scope у грепов). Состав — CHANGELOG.md, раздел Unreleased.")
    code, out = run([gh, "release", "create", tag, exe, "--repo", repo, "--title", f"bsl-indexer {version}",
                     "--notes", notes, "--prerelease"])
    print(out.strip()[-1500:])
    return 0 if code == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
