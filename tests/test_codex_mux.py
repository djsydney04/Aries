import re
import tempfile
import unittest
from pathlib import Path

from codex_mux import (
    Agent,
    SessionStore,
    build_agent_command,
    detect_help_signal,
    generate_slug,
)


class CodexMuxHelpersTests(unittest.TestCase):
    def test_generate_slug_format(self) -> None:
        slug = generate_slug()
        self.assertRegex(slug, r"^[a-z]+-[a-z]+-[0-9a-f]{4}$")

    def test_build_agent_command(self) -> None:
        argv = build_agent_command(
            template='python -c "print(\"{model}:{agent_id}\")"',
            model="gpt-x",
            prompt="hello",
            agent_id="agent-1",
            worktree="/tmp/w",
        )
        self.assertEqual(argv[0], "python")
        self.assertIn("gpt-x:agent-1", argv[-1])

    def test_detect_help_signal(self) -> None:
        self.assertTrue(detect_help_signal("[[NEEDS_HELP]] blocked", "[[NEEDS_HELP]]"))
        self.assertTrue(detect_help_signal("Agent needs help from human", "[[NEEDS_HELP]]"))
        self.assertFalse(detect_help_signal("normal progress output", "[[NEEDS_HELP]]"))


class SessionStoreTests(unittest.TestCase):
    def test_upsert_agent(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            db_path = Path(tmp) / "session.db"
            store = SessionStore(db_path)
            agent = Agent(id="a1", model="model", prompt="prompt", status="running")
            store.upsert_agent(agent)
            row = store.conn.execute("SELECT id, status FROM agents WHERE id='a1'").fetchone()
            self.assertEqual(row, ("a1", "running"))
            store.close()


if __name__ == "__main__":
    unittest.main()
