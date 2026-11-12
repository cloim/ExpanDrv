"""Ephemeral file helper for SSH exec; Python 3.8+, no installation required.

Uses binary SFTP v3 framing internally so the existing bounded, streaming client
can share its filesystem implementation. It never starts or configures a server.
"""
import ctypes
import errno
import os
import platform
import stat
import struct
import sys

INPUT = sys.stdin.buffer
OUTPUT = sys.stdout.buffer
MAX_PACKET = 1024 * 1024
HANDLES = {}
NEXT_HANDLE = 0


def u32(n):
    return struct.pack(">I", n)


def string(value):
    if isinstance(value, str):
        value = os.fsencode(value)
    return u32(len(value)) + value


def packet(kind, payload):
    OUTPUT.write(u32(len(payload) + 1) + bytes([kind]) + payload)
    OUTPUT.flush()


def status(request_id, code=0, message=""):
    packet(101, u32(request_id) + u32(code) + string(message) + string(b""))


def read_exact(length):
    data = bytearray()
    while len(data) < length:
        chunk = INPUT.read(length - len(data))
        if not chunk:
            raise EOFError()
        data.extend(chunk)
    return bytes(data)


class Reader:
    def __init__(self, data):
        self.data = memoryview(data)
        self.pos = 0

    def take(self, n):
        if n > len(self.data) - self.pos:
            raise ValueError("Truncated request")
        value = self.data[self.pos:self.pos + n]
        self.pos += n
        return value

    def number(self, fmt=">I"):
        return struct.unpack(fmt, self.take(struct.calcsize(fmt)))[0]

    def string(self):
        return bytes(self.take(self.number()))

    def path(self):
        return os.fsdecode(self.string())

    def attrs(self):
        flags = self.number()
        result = {}
        if flags & 1:
            result["size"] = self.number(">Q")
        if flags & 2:
            result["uid"] = self.number()
            result["gid"] = self.number()
        if flags & 4:
            result["mode"] = self.number()
        if flags & 8:
            result["times"] = (self.number(), self.number())
        if flags & 0x80000000:
            for _ in range(self.number()):
                self.string()
                self.string()
        return result


def attrs(value):
    # v3 timestamps are unsigned 32-bit seconds.
    atime = max(0, min(0xffffffff, int(value.st_atime)))
    mtime = max(0, min(0xffffffff, int(value.st_mtime)))
    return u32(13) + struct.pack(">QIII", value.st_size, value.st_mode, atime, mtime)


def name(value, metadata):
    return string(value) + string(b"") + metadata


def handle(request_id, value):
    global NEXT_HANDLE
    NEXT_HANDLE += 1
    key = str(NEXT_HANDLE).encode("ascii")
    HANDLES[key] = value
    packet(102, u32(request_id) + string(key))


def set_attrs(target, values):
    is_fd = isinstance(target, int)
    if "size" in values:
        os.ftruncate(target, values["size"]) if is_fd else os.truncate(target, values["size"])
    if "mode" in values:
        os.fchmod(target, values["mode"] & 0o7777) if is_fd else os.chmod(target, values["mode"] & 0o7777)
    if "times" in values:
        os.utime(target, values["times"])
    if "uid" in values:
        os.fchown(target, values["uid"], values["gid"]) if is_fd else os.chown(target, values["uid"], values["gid"])


def rename_no_replace(source, target):
    # Linux renameat2(RENAME_NOREPLACE) closes the check/rename race, including
    # directories. Unsupported kernels fail safely instead of overwriting.
    if sys.platform == "linux":
        libc = ctypes.CDLL(None, use_errno=True)
        source, target = os.fsencode(source), os.fsencode(target)
        if hasattr(libc, "renameat2"):
            result = libc.renameat2(-100, ctypes.c_char_p(source), -100, ctypes.c_char_p(target), 1)
        else:
            call = {"x86_64": 316, "i686": 353, "aarch64": 276, "armv7l": 382}.get(platform.machine())
            if call is None:
                raise OSError(errno.ENOSYS, "Atomic rename without replacement unavailable")
            result = libc.syscall(ctypes.c_long(call), ctypes.c_int(-100), ctypes.c_char_p(source), ctypes.c_int(-100), ctypes.c_char_p(target), ctypes.c_uint(1))
        if result:
            code = ctypes.get_errno()
            raise OSError(code, os.strerror(code))
    elif os.name == "nt":
        os.rename(source, target)
    else:
        raise OSError(errno.ENOSYS, "Atomic rename without replacement unavailable")


def dispatch(kind, request_id, r):
    if kind == 3:  # OPEN
        path, flags, values = r.path(), r.number(), r.attrs()
        mode = os.O_RDWR if flags & 1 and flags & 2 else os.O_WRONLY if flags & 2 else os.O_RDONLY
        for flag, native in [(4, os.O_APPEND), (8, os.O_CREAT), (16, os.O_TRUNC), (32, os.O_EXCL)]:
            if flags & flag:
                mode |= native
        fd = os.open(path, mode | getattr(os, "O_BINARY", 0), values.get("mode", 0o666) & 0o7777)
        handle(request_id, ("file", fd))
    elif kind == 4:  # CLOSE
        tag, value = HANDLES.pop(r.string())
        os.close(value) if tag == "file" else value.close()
        status(request_id)
    elif kind == 5:  # READ
        _, fd = HANDLES[r.string()]
        offset, length = r.number(">Q"), min(r.number(), 64 * 1024)
        os.lseek(fd, offset, os.SEEK_SET)
        data = os.read(fd, length)
        packet(103, u32(request_id) + string(data)) if data else status(request_id, 1)
    elif kind == 6:  # WRITE
        _, fd = HANDLES[r.string()]
        offset, data = r.number(">Q"), memoryview(r.string())
        os.lseek(fd, offset, os.SEEK_SET)
        while data:
            written = os.write(fd, data)
            if written == 0:
                raise OSError(errno.EIO, "Short write")
            data = data[written:]
        status(request_id)
    elif kind in (7, 8, 17):  # LSTAT, FSTAT, STAT
        value = os.fstat(HANDLES[r.string()][1]) if kind == 8 else (os.lstat if kind == 7 else os.stat)(r.path())
        packet(105, u32(request_id) + attrs(value))
    elif kind in (9, 10):  # SETSTAT, FSETSTAT
        target = HANDLES[r.string()][1] if kind == 10 else r.path()
        set_attrs(target, r.attrs())
        status(request_id)
    elif kind == 11:  # OPENDIR
        handle(request_id, ("dir", os.scandir(r.path())))
    elif kind == 12:  # READDIR
        _, iterator = HANDLES[r.string()]
        entries = []
        for _ in range(128):
            try:
                entry = next(iterator)
            except StopIteration:
                break
            try:
                entries.append(name(entry.name, attrs(entry.stat(follow_symlinks=False))))
            except FileNotFoundError:
                continue
        packet(104, u32(request_id) + u32(len(entries)) + b"".join(entries)) if entries else status(request_id, 1)
    elif kind in (13, 15):  # REMOVE, RMDIR (never recursive)
        (os.unlink if kind == 13 else os.rmdir)(r.path())
        status(request_id)
    elif kind == 14:  # MKDIR
        path, values = r.path(), r.attrs()
        os.mkdir(path, values.get("mode", 0o777) & 0o7777)
        status(request_id)
    elif kind == 16:  # REALPATH
        path = os.path.realpath(os.path.expanduser(r.path()))
        packet(104, u32(request_id) + u32(1) + name(path, attrs(os.stat(path))))
    elif kind == 18:  # RENAME without replacement
        rename_no_replace(r.path(), r.path())
        status(request_id)
    elif kind == 200:  # EXTENDED
        extension = r.string()
        if extension == b"posix-rename@openssh.com":
            os.replace(r.path(), r.path())
            status(request_id)
        elif extension == b"statvfs@openssh.com":
            value = os.statvfs(r.path())
            fields = [value.f_bsize, value.f_frsize, value.f_blocks, value.f_bfree, value.f_bavail, value.f_files, value.f_ffree, value.f_favail, 0, value.f_flag, value.f_namemax]
            packet(201, u32(request_id) + struct.pack(">11Q", *fields))
        else:
            status(request_id, 8)
    else:
        status(request_id, 8)


def main():
    initialized = False
    try:
        while True:
            length = struct.unpack(">I", read_exact(4))[0]
            if length < 5 or length > MAX_PACKET:
                raise ValueError("Invalid packet size")
            raw = read_exact(length)
            kind, r = raw[0], Reader(raw[1:])
            request_id = r.number()
            if kind == 1 and not initialized:
                extensions = string("posix-rename@openssh.com") + string("1")
                if hasattr(os, "statvfs"):
                    extensions += string("statvfs@openssh.com") + string("2")
                packet(2, u32(3) + extensions)
                initialized = True
                continue
            if not initialized:
                raise ValueError("Missing initialization")
            try:
                dispatch(kind, request_id, r)
            except OSError as error:
                code = 2 if error.errno == errno.ENOENT else 3 if error.errno in (errno.EPERM, errno.EACCES) else 8 if error.errno in (errno.ENOSYS, errno.ENOTSUP) else 4
                status(request_id, code, "expandrv-errno:" + str(error.errno))
            except (ValueError, KeyError, OverflowError, struct.error):
                status(request_id, 5, "Invalid request")
    except EOFError:
        pass
    finally:
        for tag, value in HANDLES.values():
            try:
                os.close(value) if tag == "file" else value.close()
            except OSError:
                pass


if __name__ == "__main__":
    main()
