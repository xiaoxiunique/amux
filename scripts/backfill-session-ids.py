#!/usr/bin/env python3
"""backfill-session-ids: 给当前所有活着的 amux tmux 会话补录真实 agent session id。

复刻 amux 的 current_id 逻辑：对每个 amux 会话，取其 cwd，找该 cwd 最新的
codex rollout / claude 会话文件，把 id 写进 ~/.amux/session-ids.json。
不 attach、不打断任何会话，纯读 + 写一个 json。幂等，可重复跑。
"""
import json
import os
import re
import subprocess
import glob

HOME = os.path.expanduser("~")
STORE = os.path.join(HOME, ".amux", "session-ids.json")

# 会话名前缀 alias -> agent 名
ALIAS_TO_AGENT = {"cc": "claude", "cx": "codex"}
# 形如 <alias>[-<provider>]_<slug>_<8hex>
NAME_RE = re.compile(r"^([a-zA-Z0-9]+)(?:-[a-zA-Z0-9-]+)?_.+_[0-9a-f]{8}$")


def tmux_sessions():
    try:
        out = subprocess.check_output(
            ["tmux", "list-sessions", "-F", "#{session_name}"],
            text=True, stderr=subprocess.DEVNULL,
        )
    except Exception:
        return []
    return [l for l in out.splitlines() if l.strip()]


def session_cwd(name):
    try:
        return subprocess.check_output(
            ["tmux", "display-message", "-p", "-t", name, "#{pane_current_path}"],
            text=True, stderr=subprocess.DEVNULL,
        ).strip()
    except Exception:
        return ""


def newest_codex_id(cwd):
    files = glob.glob(os.path.join(HOME, ".codex/sessions/**/*.jsonl"), recursive=True)
    files.sort(key=lambda p: os.path.getmtime(p), reverse=True)
    for f in files[:200]:
        try:
            with open(f) as fh:
                meta = json.loads(fh.readline())
            p = meta.get("payload", {})
            if p.get("cwd") == cwd:
                return p.get("id")
        except Exception:
            continue
    return None


def newest_claude_id(cwd):
    escaped = re.sub(r"[^A-Za-z0-9]", "-", cwd)
    d = os.path.join(HOME, ".claude", "projects", escaped)
    files = glob.glob(os.path.join(d, "*.jsonl"))
    if not files:
        return None
    newest = max(files, key=lambda p: os.path.getmtime(p))
    return os.path.splitext(os.path.basename(newest))[0]


def base_alias(name):
    prefix = name.split("_", 1)[0]
    return prefix.split("-", 1)[0]  # 去掉 -provider


def main():
    store = {}
    if os.path.exists(STORE):
        try:
            store = json.load(open(STORE))
        except Exception:
            store = {}

    added, skipped = 0, 0
    for name in tmux_sessions():
        if not NAME_RE.match(name):
            continue
        agent = ALIAS_TO_AGENT.get(base_alias(name))
        if not agent:
            continue
        cwd = session_cwd(name)
        if not cwd:
            continue
        sid = newest_codex_id(cwd) if agent == "codex" else newest_claude_id(cwd)
        if sid:
            old = store.get(name)
            store[name] = sid
            mark = "更新" if old and old != sid else ("已有" if old == sid else "新增")
            print(f"  [{mark}] {name}  ->  {sid}   ({cwd})")
            added += 1
        else:
            print(f"  [跳过] {name}  该目录无 {agent} 会话记录   ({cwd})")
            skipped += 1

    os.makedirs(os.path.dirname(STORE), exist_ok=True)
    json.dump(store, open(STORE, "w"), indent=2, ensure_ascii=False)
    print(f"\n完成：处理 {added} 个，跳过 {skipped} 个。写入 {STORE}")


if __name__ == "__main__":
    main()
