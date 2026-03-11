#!/usr/bin/env python3
from __future__ import annotations

import argparse
import curses
import os
import pty
import queue
import random
import shlex
import signal
import sqlite3
import string
import subprocess
import threading
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Deque, Optional


ADJECTIVES = [
    "agile",
    "brisk",
    "calm",
    "clever",
    "eager",
    "fierce",
    "focused",
    "nimble",
    "rapid",
    "steady",
]

ANIMALS = [
    "badger",
    "falcon",
    "fox",
    "lynx",
    "otter",
    "owl",
    "panther",
    "raven",
    "tiger",
    "wolf",
]


def generate_slug() -> str:
    adjective = random.choice(ADJECTIVES)
    animal = random.choice(ANIMALS)
    hex4 = "".join(random.choices("0123456789abcdef", k=4))
    return f"{adjective}-{animal}-{hex4}"


def detect_help_signal(line: str, help_token: str) -> bool:
    lower = line.lower()
    return help_token.lower() in lower or "needs help" in lower or "need help" in lower


def build_agent_command(
    template: str,
    model: str,
    prompt: str,
    agent_id: str,
    worktree: str,
) -> list[str]:
    command = template.format(
        model=model,
        prompt=prompt,
        agent_id=agent_id,
        worktree=worktree,
    )
    return shlex.split(command)


@dataclass
class WorktreeInfo:
    slug: str
    branch: str
    path: Path
    base_branch: str


@dataclass
class Agent:
    id: str
    model: str
    prompt: str
    status: str = "starting"
    worktree: Optional[WorktreeInfo] = None
    pid: Optional[int] = None
    return_code: Optional[int] = None
    logs: Deque[str] = field(default_factory=lambda: deque(maxlen=4000))
    created_at: float = field(default_factory=time.time)
    updated_at: float = field(default_factory=time.time)
    needs_help: bool = False


@dataclass
class Alert:
    agent_id: str
    message: str
    created_at: float = field(default_factory=time.time)
    acknowledged: bool = False


@dataclass
class SupervisorEvent:
    type: str
    agent_id: str
    payload: Any = None


class SessionStore:
    def __init__(self, db_path: Path) -> None:
        self.db_path = db_path
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self.conn = sqlite3.connect(str(self.db_path), check_same_thread=False)
        self.lock = threading.Lock()
        self._init_schema()

    def _init_schema(self) -> None:
        with self.conn:
            self.conn.execute(
                """
                CREATE TABLE IF NOT EXISTS agents (
                    id TEXT PRIMARY KEY,
                    model TEXT NOT NULL,
                    prompt TEXT NOT NULL,
                    status TEXT NOT NULL,
                    branch TEXT,
                    worktree TEXT,
                    pid INTEGER,
                    return_code INTEGER,
                    created_at REAL NOT NULL,
                    updated_at REAL NOT NULL
                )
                """
            )
            self.conn.execute(
                """
                CREATE TABLE IF NOT EXISTS events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    agent_id TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    message TEXT,
                    created_at REAL NOT NULL
                )
                """
            )
            self.conn.execute(
                """
                CREATE TABLE IF NOT EXISTS alerts (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    agent_id TEXT NOT NULL,
                    message TEXT NOT NULL,
                    acknowledged INTEGER NOT NULL,
                    created_at REAL NOT NULL
                )
                """
            )

    def upsert_agent(self, agent: Agent) -> None:
        branch = agent.worktree.branch if agent.worktree else None
        worktree = str(agent.worktree.path) if agent.worktree else None
        with self.lock, self.conn:
            self.conn.execute(
                """
                INSERT INTO agents (
                    id, model, prompt, status, branch, worktree, pid,
                    return_code, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(id) DO UPDATE SET
                    status=excluded.status,
                    branch=excluded.branch,
                    worktree=excluded.worktree,
                    pid=excluded.pid,
                    return_code=excluded.return_code,
                    updated_at=excluded.updated_at
                """,
                (
                    agent.id,
                    agent.model,
                    agent.prompt,
                    agent.status,
                    branch,
                    worktree,
                    agent.pid,
                    agent.return_code,
                    agent.created_at,
                    agent.updated_at,
                ),
            )

    def add_event(self, agent_id: str, event_type: str, message: str) -> None:
        with self.lock, self.conn:
            self.conn.execute(
                "INSERT INTO events (agent_id, event_type, message, created_at) VALUES (?, ?, ?, ?)",
                (agent_id, event_type, message, time.time()),
            )

    def add_alert(self, alert: Alert) -> None:
        with self.lock, self.conn:
            self.conn.execute(
                "INSERT INTO alerts (agent_id, message, acknowledged, created_at) VALUES (?, ?, ?, ?)",
                (alert.agent_id, alert.message, 1 if alert.acknowledged else 0, alert.created_at),
            )

    def mark_alert_acknowledged(self, agent_id: str) -> None:
        with self.lock, self.conn:
            self.conn.execute(
                "UPDATE alerts SET acknowledged=1 WHERE agent_id=? AND acknowledged=0",
                (agent_id,),
            )

    def close(self) -> None:
        self.conn.close()


class WorktreeManager:
    def __init__(self, repo_root: Path, base_branch: str = "main", worktrees_dir: str = ".worktrees") -> None:
        self.repo_root = repo_root
        self.base_branch = base_branch
        self.worktrees_dir = repo_root / worktrees_dir
        self.worktrees_dir.mkdir(parents=True, exist_ok=True)
        self._verify_git_repo()

    def _run_git(self, *args: str, cwd: Optional[Path] = None) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", *args],
            cwd=str(cwd or self.repo_root),
            capture_output=True,
            text=True,
        )

    def _verify_git_repo(self) -> None:
        check = self._run_git("rev-parse", "--is-inside-work-tree")
        if check.returncode != 0:
            raise ValueError(f"Not a git repository: {self.repo_root}")

    def _resolve_base_branch(self) -> str:
        check = self._run_git("rev-parse", "--verify", self.base_branch)
        if check.returncode == 0:
            return self.base_branch
        return "HEAD"

    def create_worktree(self) -> WorktreeInfo:
        slug = generate_slug()
        branch = f"agent/{slug}"
        path = self.worktrees_dir / slug
        base = self._resolve_base_branch()
        create = self._run_git("worktree", "add", "-b", branch, str(path), base)
        if create.returncode != 0:
            raise RuntimeError(create.stderr.strip() or create.stdout.strip() or "failed to create worktree")
        return WorktreeInfo(slug=slug, branch=branch, path=path, base_branch=base)

    def _is_branch_merged_or_closed(self, info: WorktreeInfo) -> tuple[bool, str]:
        merged = self._run_git("branch", "--merged", info.base_branch)
        if merged.returncode == 0:
            merged_branches = {line.strip().replace("* ", "") for line in merged.stdout.splitlines() if line.strip()}
            if info.branch in merged_branches:
                return True, "merged"
        divergence = self._run_git("rev-list", "--count", f"{info.base_branch}..{info.branch}")
        if divergence.returncode == 0 and divergence.stdout.strip() == "0":
            return True, "closed"
        return False, "not merged"

    def cleanup_if_safe(self, info: WorktreeInfo) -> tuple[bool, str]:
        if not info.path.exists():
            return True, "worktree already removed"
        clean = self._run_git("status", "--porcelain", cwd=info.path)
        if clean.returncode != 0:
            return False, clean.stderr.strip() or "unable to inspect worktree"
        if clean.stdout.strip():
            return False, "worktree has uncommitted changes"

        removable, reason = self._is_branch_merged_or_closed(info)
        if not removable:
            return False, reason

        remove_worktree = self._run_git("worktree", "remove", str(info.path))
        if remove_worktree.returncode != 0:
            return False, remove_worktree.stderr.strip() or "failed to remove worktree"

        delete_branch = self._run_git("branch", "-d", info.branch)
        if delete_branch.returncode != 0 and "not found" not in delete_branch.stderr:
            return False, delete_branch.stderr.strip() or "failed to delete branch"
        return True, f"worktree deleted ({reason})"


class AgentSupervisor:
    def __init__(self) -> None:
        self.events: queue.Queue[SupervisorEvent] = queue.Queue()
        self.processes: dict[str, subprocess.Popen[bytes]] = {}
        self.masters: dict[str, int] = {}
        self.lock = threading.Lock()

    def start(self, agent_id: str, command: list[str], cwd: Path) -> None:
        master_fd, slave_fd = pty.openpty()
        process = subprocess.Popen(
            command,
            cwd=str(cwd),
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            preexec_fn=os.setsid,
            close_fds=True,
        )
        os.close(slave_fd)
        with self.lock:
            self.processes[agent_id] = process
            self.masters[agent_id] = master_fd
        self.events.put(SupervisorEvent(type="started", agent_id=agent_id, payload=process.pid))
        threading.Thread(target=self._stream_reader, args=(agent_id,), daemon=True).start()

    def _stream_reader(self, agent_id: str) -> None:
        with self.lock:
            process = self.processes.get(agent_id)
            master_fd = self.masters.get(agent_id)
        if process is None or master_fd is None:
            return

        remainder = ""
        while True:
            try:
                data = os.read(master_fd, 4096)
            except OSError:
                break
            if not data:
                break
            chunk = data.decode(errors="replace")
            text = remainder + chunk
            lines = text.splitlines(keepends=True)
            remainder = ""
            if lines and not lines[-1].endswith("\n"):
                remainder = lines.pop()
            for line in lines:
                self.events.put(SupervisorEvent(type="output", agent_id=agent_id, payload=line.rstrip("\n")))

        if remainder:
            self.events.put(SupervisorEvent(type="output", agent_id=agent_id, payload=remainder))

        return_code = process.wait()
        self.events.put(SupervisorEvent(type="exited", agent_id=agent_id, payload=return_code))
        try:
            os.close(master_fd)
        except OSError:
            pass
        with self.lock:
            self.processes.pop(agent_id, None)
            self.masters.pop(agent_id, None)

    def stop(self, agent_id: str) -> None:
        with self.lock:
            process = self.processes.get(agent_id)
        if process is None:
            return
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            return

    def is_running(self, agent_id: str) -> bool:
        with self.lock:
            process = self.processes.get(agent_id)
        return process is not None and process.poll() is None

    def stop_all(self) -> None:
        with self.lock:
            ids = list(self.processes.keys())
        for agent_id in ids:
            self.stop(agent_id)


class CodexMuxApp:
    def __init__(
        self,
        repo_root: Path,
        base_branch: str,
        db_path: Path,
        command_template: str,
        help_token: str,
        use_worktrees: bool,
        worktrees_dir: str,
    ) -> None:
        self.repo_root = repo_root
        self.base_branch = base_branch
        self.command_template = command_template
        self.help_token = help_token
        self.use_worktrees = use_worktrees
        self.store = SessionStore(db_path)
        self.supervisor = AgentSupervisor()
        self.worktree_manager: Optional[WorktreeManager] = None
        if use_worktrees:
            self.worktree_manager = WorktreeManager(repo_root=repo_root, base_branch=base_branch, worktrees_dir=worktrees_dir)

        self.agents: dict[str, Agent] = {}
        self.order: list[str] = []
        self.alerts: Deque[Alert] = deque(maxlen=200)
        self.selected = 0
        self.status_message = "Ready"
        self.running = True

        self.mode = "normal"
        self.input_prompt = ""
        self.input_buffer = ""
        self.new_model = ""
        self.new_prompt = ""

    def safe_status(self, message: str) -> None:
        self.status_message = message

    def _next_agent_id(self) -> str:
        millis = int(time.time() * 1000)
        suffix = "".join(random.choices(string.ascii_lowercase + string.digits, k=4))
        return f"agent-{millis}-{suffix}"

    def _current_agent(self) -> Optional[Agent]:
        if not self.order:
            return None
        if self.selected >= len(self.order):
            self.selected = len(self.order) - 1
        if self.selected < 0:
            self.selected = 0
        return self.agents.get(self.order[self.selected])

    def _add_alert(self, agent_id: str, message: str) -> None:
        alert = Alert(agent_id=agent_id, message=message)
        self.alerts.appendleft(alert)
        self.store.add_alert(alert)
        self.safe_status(f"{agent_id}: {message}")

    def spawn_agent(self, model: str, prompt: str) -> None:
        agent_id = self._next_agent_id()
        agent = Agent(id=agent_id, model=model, prompt=prompt, status="starting")
        self.agents[agent_id] = agent
        self.order.append(agent_id)
        self.selected = len(self.order) - 1

        work_dir = self.repo_root
        if self.worktree_manager is not None:
            try:
                info = self.worktree_manager.create_worktree()
            except Exception as exc:
                agent.status = "blocked"
                agent.updated_at = time.time()
                self.store.upsert_agent(agent)
                self._add_alert(agent.id, f"worktree creation failed: {exc}")
                return
            agent.worktree = info
            work_dir = info.path

        try:
            argv = build_agent_command(
                self.command_template,
                model=model,
                prompt=prompt,
                agent_id=agent_id,
                worktree=str(work_dir),
            )
            if not argv:
                raise ValueError("command template produced empty command")
            self.supervisor.start(agent_id=agent_id, command=argv, cwd=work_dir)
            self.safe_status(f"spawned {agent_id} in {work_dir}")
            self.store.add_event(agent_id, "spawn", "agent created")
        except Exception as exc:
            agent.status = "blocked"
            self._add_alert(agent.id, f"spawn failed: {exc}")
        finally:
            agent.updated_at = time.time()
            self.store.upsert_agent(agent)

    def stop_selected_agent(self) -> None:
        agent = self._current_agent()
        if agent is None:
            self.safe_status("no agents")
            return
        self.supervisor.stop(agent.id)
        self.safe_status(f"sent stop signal to {agent.id}")
        self.store.add_event(agent.id, "stop", "stop requested")

    def acknowledge_selected_alerts(self) -> None:
        agent = self._current_agent()
        if agent is None:
            return
        changed = False
        for alert in self.alerts:
            if alert.agent_id == agent.id and not alert.acknowledged:
                alert.acknowledged = True
                changed = True
        if changed:
            self.store.mark_alert_acknowledged(agent.id)
            agent.needs_help = False
            if self.supervisor.is_running(agent.id):
                agent.status = "running"
            self.safe_status(f"acknowledged alerts for {agent.id}")
            agent.updated_at = time.time()
            self.store.upsert_agent(agent)

    def _cleanup_worktree_if_needed(self, agent: Agent) -> None:
        if self.worktree_manager is None or agent.worktree is None:
            return
        cleaned, message = self.worktree_manager.cleanup_if_safe(agent.worktree)
        if cleaned:
            self.store.add_event(agent.id, "cleanup", message)
            self.safe_status(f"{agent.id}: {message}")
        else:
            self._add_alert(agent.id, f"cleanup skipped: {message}")

    def process_events(self) -> None:
        while True:
            try:
                event = self.supervisor.events.get_nowait()
            except queue.Empty:
                break
            agent = self.agents.get(event.agent_id)
            if agent is None:
                continue

            now = time.time()
            if event.type == "started":
                agent.pid = int(event.payload)
                agent.status = "running"
                self.store.add_event(agent.id, "started", f"pid={agent.pid}")
            elif event.type == "output":
                line = str(event.payload)
                agent.logs.append(line)
                if detect_help_signal(line, self.help_token):
                    if not agent.needs_help:
                        agent.needs_help = True
                        agent.status = "needs_help"
                        self._add_alert(agent.id, "agent requested help")
                        try:
                            curses.beep()
                        except curses.error:
                            pass
                        self.store.add_event(agent.id, "needs_help", line)
            elif event.type == "exited":
                rc = int(event.payload)
                agent.return_code = rc
                if rc == 0:
                    agent.status = "done"
                    self.store.add_event(agent.id, "exited", "exit=0")
                    self._cleanup_worktree_if_needed(agent)
                else:
                    agent.status = "failed"
                    agent.needs_help = True
                    self._add_alert(agent.id, f"process exited with code {rc}")
                    try:
                        curses.beep()
                    except curses.error:
                        pass
                    self.store.add_event(agent.id, "exited", f"exit={rc}")

            agent.updated_at = now
            self.store.upsert_agent(agent)

    def draw(self, stdscr: curses.window) -> None:
        stdscr.erase()
        height, width = stdscr.getmaxyx()
        if height < 12 or width < 70:
            stdscr.addstr(0, 0, "Terminal too small. Resize to at least 70x12.")
            stdscr.refresh()
            return

        left_width = max(24, min(36, width // 3))
        help_height = max(6, height // 4)
        main_height = height - help_height - 1

        agents_win = stdscr.derwin(main_height, left_width, 0, 0)
        logs_win = stdscr.derwin(main_height, width - left_width, 0, left_width)
        help_win = stdscr.derwin(help_height, width, main_height, 0)

        self._draw_agents(agents_win)
        self._draw_logs(logs_win)
        self._draw_help(help_win)
        self._draw_status(stdscr, width, height)

        stdscr.refresh()

    def _draw_agents(self, win: curses.window) -> None:
        win.box()
        self._safe_add(win, 0, 2, " Agents ")
        if not self.order:
            self._safe_add(win, 2, 2, "No agents. Press 'n' to spawn.")
            return
        max_lines = max(1, win.getmaxyx()[0] - 2)
        start = 0
        if self.selected >= max_lines:
            start = self.selected - max_lines + 1
        visible_ids = self.order[start : start + max_lines]
        for idx, agent_id in enumerate(visible_ids):
            absolute_index = start + idx
            agent = self.agents[agent_id]
            marker = ">" if absolute_index == self.selected else " "
            status = self._status_badge(agent)
            text = f"{marker} {status} {agent.model}"
            self._safe_add(win, idx + 1, 1, text)

    def _draw_logs(self, win: curses.window) -> None:
        win.box()
        agent = self._current_agent()
        title = " Stream " if agent is None else f" Stream: {agent.id} "
        self._safe_add(win, 0, 2, title)
        if agent is None:
            self._safe_add(win, 2, 2, "Select an agent to view output.")
            return

        lines_available = win.getmaxyx()[0] - 2
        content_width = win.getmaxyx()[1] - 2
        logs = list(agent.logs)[-lines_available:]
        for idx, line in enumerate(logs):
            self._safe_add(win, idx + 1, 1, line[: max(0, content_width - 1)])

    def _draw_help(self, win: curses.window) -> None:
        win.box()
        unresolved = sum(1 for alert in self.alerts if not alert.acknowledged)
        self._safe_add(win, 0, 2, f" Alerts ({unresolved}) ")
        lines_available = win.getmaxyx()[0] - 2
        if not self.alerts:
            self._safe_add(win, 1, 2, "No alerts")
            return
        for idx, alert in enumerate(list(self.alerts)[:lines_available]):
            flag = " " if alert.acknowledged else "!"
            text = f"{flag} {alert.agent_id}: {alert.message}"
            self._safe_add(win, idx + 1, 1, text)

    def _draw_status(self, stdscr: curses.window, width: int, height: int) -> None:
        help_text = "n:new  j/k:select  x:stop  a:ack  q:quit"
        if self.mode == "input":
            help_text = f"{self.input_prompt}{self.input_buffer}"
        left = self.status_message
        status_line = f"{left} | {help_text}"
        if len(status_line) > width - 1:
            status_line = status_line[: width - 1]
        self._safe_add(stdscr, height - 1, 0, status_line)

    def _safe_add(self, win: curses.window, y: int, x: int, text: str) -> None:
        max_y, max_x = win.getmaxyx()
        if y < 0 or y >= max_y or x < 0 or x >= max_x:
            return
        visible = text[: max_x - x - 1]
        try:
            win.addstr(y, x, visible)
        except curses.error:
            pass

    def _status_badge(self, agent: Agent) -> str:
        badges = {
            "starting": "S",
            "running": "R",
            "needs_help": "!",
            "failed": "F",
            "done": "D",
            "blocked": "B",
        }
        return badges.get(agent.status, "?")

    def handle_normal_key(self, key: int) -> None:
        if key in (ord("q"),):
            self.running = False
            return
        if key in (ord("j"), curses.KEY_DOWN):
            if self.order:
                self.selected = min(len(self.order) - 1, self.selected + 1)
            return
        if key in (ord("k"), curses.KEY_UP):
            if self.order:
                self.selected = max(0, self.selected - 1)
            return
        if key == ord("n"):
            self.mode = "input"
            self.input_prompt = "Model: "
            self.input_buffer = ""
            self.new_model = ""
            self.new_prompt = ""
            return
        if key == ord("x"):
            self.stop_selected_agent()
            return
        if key == ord("a"):
            self.acknowledge_selected_alerts()

    def handle_input_key(self, key: int) -> None:
        if key in (27,):
            self.mode = "normal"
            self.input_buffer = ""
            self.input_prompt = ""
            return
        if key in (curses.KEY_BACKSPACE, 127, 8):
            self.input_buffer = self.input_buffer[:-1]
            return
        if key in (10, 13):
            if self.input_prompt.startswith("Model"):
                self.new_model = self.input_buffer.strip()
                if not self.new_model:
                    self.safe_status("model cannot be empty")
                    self.mode = "normal"
                    return
                self.input_prompt = "Prompt: "
                self.input_buffer = ""
                return
            self.new_prompt = self.input_buffer.strip()
            if not self.new_prompt:
                self.safe_status("prompt cannot be empty")
                self.mode = "normal"
                return
            self.spawn_agent(self.new_model, self.new_prompt)
            self.mode = "normal"
            self.input_buffer = ""
            self.input_prompt = ""
            return
        if 32 <= key <= 126:
            self.input_buffer += chr(key)

    def loop(self, stdscr: curses.window) -> None:
        curses.curs_set(0)
        stdscr.timeout(100)
        while self.running:
            self.process_events()
            self.draw(stdscr)
            key = stdscr.getch()
            if key == -1:
                continue
            if self.mode == "input":
                self.handle_input_key(key)
            else:
                self.handle_normal_key(key)

        self.supervisor.stop_all()
        self.store.close()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Codex multi-agent terminal supervisor")
    parser.add_argument("--repo", default=os.getcwd(), help="repository root used for worktrees")
    parser.add_argument("--base-branch", default="main", help="default base branch for new worktrees")
    parser.add_argument("--worktrees-dir", default=".worktrees", help="directory under repo for worktrees")
    parser.add_argument(
        "--db-path",
        default=".codex_mux/session.db",
        help="sqlite database path",
    )
    parser.add_argument(
        "--agent-cmd-template",
        default='codex --model "{model}" "{prompt}"',
        help="command template with placeholders: {model} {prompt} {agent_id} {worktree}",
    )
    parser.add_argument(
        "--help-token",
        default="[[NEEDS_HELP]]",
        help="token that marks agent output requiring human intervention",
    )
    parser.add_argument("--no-worktree", action="store_true", help="disable git worktree creation")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo_root = Path(args.repo).resolve()
    db_path = Path(args.db_path)

    try:
        app = CodexMuxApp(
            repo_root=repo_root,
            base_branch=args.base_branch,
            db_path=db_path,
            command_template=args.agent_cmd_template,
            help_token=args.help_token,
            use_worktrees=not args.no_worktree,
            worktrees_dir=args.worktrees_dir,
        )
    except ValueError as exc:
        print(f"error: {exc}. Use --no-worktree or --repo <git-repo>")
        return 2
    except Exception as exc:
        print(f"error: {exc}")
        return 1

    curses.wrapper(app.loop)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
