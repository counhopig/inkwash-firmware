#!/usr/bin/env python3
"""Host checks for the USB event-log exporter; no device is opened."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("export_logs", Path(__file__).parents[1] / "device/export-logs.py")
EXPORT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EXPORT)


class Link:
    def __init__(self, **_):
        self.dtr = self.rts = None
        self.writes = []
        self.chunks = []
        self.closed = False

    def open(self):
        assert self.dtr is False and self.rts is False

    def close(self):
        self.closed = True

    def write(self, wire):
        self.writes.append(json.loads(wire[5:]))

    def flush(self):
        pass

    def read(self, _):
        return self.chunks.pop(0) if self.chunks else b""


def page(cursor, entries, done=False):
    return {"status": "ok", "anchor": 1, "snapshot": "3", "capacity": 64,
            "invalid_records": 0, "dropped_this_boot": 0, "io_errors_this_boot": 0,
            "cursor": cursor, "entries": [{"sequence": str(i), "event": "boot"} for i in entries], "done": done}


class ExportTests(unittest.TestCase):
    def test_framing_fragments_and_reply_id(self):
        link = Link()
        link.chunks = [b'I (10) app: test\n<<IW {"status":"ok","id":"other"}\n',
                       b'<<IW {"status":"ok",', b'"id":"logs-0"}\n']
        reply = EXPORT.request(link, {"cmd": "get_logs", "id": "logs-0"}, bytearray())
        self.assertEqual(reply["id"], "logs-0")
        self.assertEqual(len(link.writes), 1)

    def test_timeout_retries_identical_cursor(self):
        link = Link()
        command = {"cmd": "get_logs", "id": "logs-8", "cursor": 8, "anchor": 0, "snapshot": "3"}
        with mock.patch.object(EXPORT.time, "monotonic", side_effect=[0, 9, 10, 19, 20, 29]):
            with self.assertRaises(TimeoutError):
                EXPORT.request(link, command, bytearray())
        self.assertEqual(link.writes, [command, command, command])

    def test_complete_snapshot_and_port_controls(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "logs.jsonl"
            link = Link()
            pages = [page(10, [1, 2]), page(64, [3], True)]
            with mock.patch.object(EXPORT.serial, "Serial", return_value=link), \
                 mock.patch.object(EXPORT, "request", side_effect=pages) as request, \
                 mock.patch.object(EXPORT.time, "sleep"), \
                 mock.patch("sys.argv", ["export-logs.py", "--port", "FAKE", "--output", str(output)]):
                self.assertEqual(EXPORT.main(), 0)
            rows = [json.loads(row) for row in output.read_text().splitlines()]
            self.assertEqual([row["type"] for row in rows], ["export_start", "event", "event", "event", "export_end"])
            self.assertEqual(rows[-1]["records"], 3)
            resumed = request.call_args_list[1].args[1]
            self.assertEqual((resumed["cursor"], resumed["anchor"], resumed["snapshot"]), (10, 1, "3"))
            self.assertTrue(link.closed)
            with mock.patch("sys.argv", ["export-logs.py", "--port", "FAKE", "--output", str(output)]):
                with self.assertRaises(FileExistsError):
                    EXPORT.main()

    def test_failure_keeps_partial_export_without_complete_marker(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "logs.jsonl"
            link = Link()
            with mock.patch.object(EXPORT.serial, "Serial", return_value=link), \
                 mock.patch.object(EXPORT, "request", side_effect=[page(10, [1]), RuntimeError("snapshot expired")]), \
                 mock.patch.object(EXPORT.time, "sleep"), \
                 mock.patch("sys.argv", ["export-logs.py", "--port", "FAKE", "--output", str(output)]):
                with self.assertRaisesRegex(RuntimeError, "snapshot expired"):
                    EXPORT.main()
            self.assertTrue(link.closed)
            rows = [json.loads(row) for row in output.read_text().splitlines()]
            self.assertEqual([row["type"] for row in rows], ["export_start", "event"])

    def test_reordered_snapshot_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "logs.jsonl"
            link = Link()
            with mock.patch.object(EXPORT.serial, "Serial", return_value=link), \
                 mock.patch.object(EXPORT, "request", return_value=page(64, [2, 1], True)), \
                 mock.patch.object(EXPORT.time, "sleep"), \
                 mock.patch("sys.argv", ["export-logs.py", "--port", "FAKE", "--output", str(output)]):
                with self.assertRaisesRegex(RuntimeError, "Logs changed"):
                    EXPORT.main()
            self.assertNotIn('"type": "export_end"', output.read_text())


if __name__ == "__main__":
    unittest.main()
