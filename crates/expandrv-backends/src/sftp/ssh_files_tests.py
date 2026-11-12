"""Exercise the SSH helper as a subprocess through its binary wire protocol."""
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest


def num(n):
    return struct.pack(">I", n)


def text(value):
    value = os.fsencode(value) if isinstance(value, str) else value
    return num(len(value)) + value


def unpack_text(data):
    length = struct.unpack(">I", data[:4])[0]
    return data[4:4 + length], data[4 + length:]


class FileHelperTests(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory()
        self.addCleanup(self.folder.cleanup)
        self.process = subprocess.Popen(
            [sys.executable, "-I", "-u", str(Path(__file__).with_name("ssh_files.py"))],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        self.addCleanup(self.stop)
        kind, response = self.send(1, num(3), raw=True)
        self.assertEqual((kind, response[:4]), (2, num(3)))
        self.assertIn(b"posix-rename@openssh.com", response)
        self.sequence = 0

    def stop(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.process.stdout.close()
        self.process.stderr.close()

    def exact(self, length):
        data = b""
        while len(data) < length:
            chunk = self.process.stdout.read(length - len(data))
            self.assertTrue(chunk, "helper closed its output")
            data += chunk
        return data

    def send(self, kind, payload, raw=False):
        if not raw:
            self.sequence += 1
            payload = num(self.sequence) + payload
        self.process.stdin.write(num(len(payload) + 1) + bytes([kind]) + payload)
        self.process.stdin.flush()
        length = struct.unpack(">I", self.exact(4))[0]
        response = self.exact(length)
        if raw:
            return response[0], response[1:]
        self.assertEqual(response[1:5], num(self.sequence))
        return response[0], response[5:]

    def path(self, name):
        return os.path.join(self.folder.name, name)

    def ok(self, kind, payload):
        result, data = self.send(kind, payload)
        self.assertEqual((result, data[:4]), (101, num(0)))

    def open(self, path, flags=26):
        result, data = self.send(3, text(path) + num(flags) + num(0))
        self.assertEqual(result, 102)
        return unpack_text(data)[0]

    def test_binary_upload_ranges_overwrite_unicode_and_metadata(self):
        path = self.path("한글 ' $() & name.bin")
        handle = self.open(path)
        contents = bytes(range(256)) * 1024
        self.ok(6, text(handle) + struct.pack(">Q", 0) + text(contents))
        self.ok(4, text(handle))
        self.assertEqual(Path(path).read_bytes(), contents)
        handle = self.open(path, 1)
        kind, data = self.send(5, text(handle) + struct.pack(">QI", 12345, 5678))
        self.assertEqual((kind, unpack_text(data)[0]), (103, contents[12345:12345 + 5678]))
        kind, data = self.send(5, text(handle) + struct.pack(">QI", len(contents), 1024))
        self.assertEqual((kind, data[:4]), (101, num(1)))
        self.ok(4, text(handle))
        self.ok(9, text(path) + num(9) + struct.pack(">QII", 12, 1700000000, 1700000100))
        kind, data = self.send(17, text(path))
        self.assertEqual(kind, 105)
        self.assertEqual(struct.unpack(">Q", data[4:12])[0], 12)
        self.assertEqual(int(os.stat(path).st_mtime), 1700000100)

    def test_directory_listing_realpath_and_nonrecursive_delete(self):
        folder = self.path("directory")
        self.ok(14, text(folder) + num(0))
        Path(folder, "child").write_bytes(b"safe")
        kind, data = self.send(11, text(folder))
        self.assertEqual(kind, 102)
        handle = unpack_text(data)[0]
        kind, data = self.send(12, text(handle))
        self.assertEqual((kind, data[:4]), (104, num(1)))
        self.assertEqual(unpack_text(data[4:])[0], b"child")
        kind, data = self.send(12, text(handle))
        self.assertEqual((kind, data[:4]), (101, num(1)))
        self.ok(4, text(handle))
        kind, data = self.send(15, text(folder))
        self.assertEqual((kind, data[:4]), (101, num(4)))
        self.assertEqual(Path(folder, "child").read_bytes(), b"safe")
        kind, data = self.send(16, text(folder))
        self.assertEqual(kind, 104)
        self.assertEqual(os.path.realpath(os.fsdecode(unpack_text(data[4:])[0])), os.path.realpath(folder))
        self.ok(13, text(os.path.join(folder, "child")))
        self.ok(15, text(folder))

    def test_rename_never_replaces_without_explicit_request(self):
        source, target = self.path("source"), self.path("target")
        Path(source).write_bytes(b"new")
        Path(target).write_bytes(b"original")
        kind, data = self.send(18, text(source) + text(target))
        self.assertEqual((kind, data[:4]), (101, num(4)))
        self.assertEqual(Path(target).read_bytes(), b"original")
        self.assertEqual(Path(source).read_bytes(), b"new")
        self.ok(200, text("posix-rename@openssh.com") + text(source) + text(target))
        self.assertEqual(Path(target).read_bytes(), b"new")
        self.assertFalse(Path(source).exists())
        folder = self.path("source-dir")
        os.mkdir(folder)
        self.ok(18, text(folder) + text(self.path("target-dir")))

    def test_missing_files_bad_handles_and_unknown_extensions(self):
        kind, data = self.send(17, text(self.path("missing")))
        self.assertEqual((kind, data[:4]), (101, num(2)))
        kind, data = self.send(4, text("bad-handle"))
        self.assertEqual((kind, data[:4]), (101, num(5)))
        kind, data = self.send(200, text("unknown"))
        self.assertEqual((kind, data[:4]), (101, num(8)))


if __name__ == "__main__":
    unittest.main()
