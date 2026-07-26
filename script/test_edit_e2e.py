#!/usr/bin/env python3
"""End-to-end test for `lx-term edit` that stands in for the client.

Runs the real binary on a real pty, then plays the part the app plays: read the
EditFile hook off the pty, acknowledge it, edit the file the way a user would in
the built-in editor, and signal that the editor tab was closed. What is being
verified is the contract `kubectl edit` depends on — that the editor process
blocks until the edit is finished, and that the file it reads back afterwards
contains the edits.

This covers the CLI half of the feature without needing a GUI. The other half —
the hook opening an editor pane, and closing that pane writing the done marker —
needs a running client and is verified by hand.

Usage:  script/test_edit_e2e.py <path-to-lx-term-binary>

    ./script/run --  # or: cargo build --bin lx-term
    ./script/test_edit_e2e.py target/debug/lx-term
"""

import binascii
import fcntl
import json
import os
import pty
import re
import subprocess
import sys
import tempfile
import termios
import time

HOOK_RE = re.compile(rb"\x1b\]9278;d;([0-9a-fA-F]+)\x07")

ORIGINAL = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: demo\ndata:\n  replicas: \"1\"\n"
EDITED = ORIGINAL.replace('replicas: "1"', 'replicas: "3"')

TIMEOUT = 20


class Failure(Exception):
    pass


def read_hook(master_fd):
    """Reads from the pty until a complete EditFile hook has arrived."""
    buffered = b""
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        try:
            buffered += os.read(master_fd, 65536)
        except OSError:
            break
        match = HOOK_RE.search(buffered)
        if match:
            return json.loads(binascii.unhexlify(match.group(1)))
    raise Failure(f"no EditFile hook arrived within {TIMEOUT}s; got {buffered!r}")


def wait_for_exit(process, label):
    try:
        return process.wait(timeout=TIMEOUT)
    except subprocess.TimeoutExpired:
        process.kill()
        raise Failure(f"`warp edit` did not exit after {label}")


def assert_still_blocked(process, target):
    time.sleep(0.5)
    if process.poll() is not None:
        raise Failure(f"`warp edit` exited before {target} (exit {process.returncode})")


def run_case(binary, *, wait):
    """Drives one `warp edit` invocation, with or without --no-wait."""
    with tempfile.TemporaryDirectory() as workdir:
        resource = os.path.join(workdir, "kubectl-edit-1234.yaml")
        with open(resource, "w") as handle:
            handle.write(ORIGINAL)

        primary_fd, replica_fd = pty.openpty()
        argv = [binary, "edit", resource] + ([] if wait else ["--no-wait"])
        process = subprocess.Popen(
            argv,
            stdin=replica_fd,
            stdout=replica_fd,
            stderr=subprocess.PIPE,
            # `warp edit` writes the hook to /dev/tty rather than stdout, so the
            # child needs the pty as its *controlling* terminal, not merely as
            # its stdin. Inheriting the fd is not enough: it has to lead a new
            # session and then claim the terminal.
            start_new_session=True,
            preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
            env={
                **os.environ,
                # Stand in for the environment Warp gives its own shells.
                "WARP_IS_LOCAL_SHELL_SESSION": "1",
                "TERM_PROGRAM": "WarpTerminal",
            },
        )
        os.close(replica_fd)

        try:
            hook = read_hook(primary_fd)

            if hook.get("hook") != "EditFile":
                raise Failure(f"unexpected hook: {hook}")
            value = hook["value"]
            if value["path"] != resource:
                raise Failure(f"hook names the wrong file: {value['path']}")
            if value["wait"] is not wait:
                raise Failure(f"hook reports wait={value['wait']}, expected {wait}")

            if wait:
                assert_still_blocked(process, "the client acknowledged")

            # The client accepts the request and opens the file.
            open(value["ack_path"], "w").close()

            if wait:
                assert_still_blocked(process, "the editor tab was closed")

                # The user edits the file in the built-in editor and saves.
                with open(resource, "w") as handle:
                    handle.write(EDITED)

                # The editor tab is closed, which drops the pending session.
                with open(value["done_path"], "w") as handle:
                    handle.write("0")

            exit_code = wait_for_exit(process, "the edit completed")
            if exit_code != 0:
                stderr = process.stderr.read().decode(errors="replace")
                raise Failure(f"exit code {exit_code}; stderr: {stderr}")

            # This is the read-back `kubectl edit` performs to decide what to apply.
            with open(resource) as handle:
                final = handle.read()
            expected = EDITED if wait else ORIGINAL
            if final != expected:
                raise Failure(f"file contents after the edit: {final!r}")

            for marker in ("ack_path", "done_path"):
                if os.path.exists(value[marker]):
                    raise Failure(f"{marker} was left behind: {value[marker]}")
        finally:
            if process.poll() is None:
                process.kill()
            os.close(primary_fd)


def run_fallback_case(binary):
    """Outside a Warp session the shim must defer to a real editor."""
    with tempfile.TemporaryDirectory() as workdir:
        resource = os.path.join(workdir, "resource.yaml")
        with open(resource, "w") as handle:
            handle.write(ORIGINAL)

        # A stand-in editor that appends a line, so we can tell it really ran.
        fake_editor = os.path.join(workdir, "fake-editor")
        with open(fake_editor, "w") as handle:
            handle.write('#!/bin/sh\nprintf "edited: true\\n" >> "$1"\n')
        os.chmod(fake_editor, 0o755)

        environment = {
            key: value
            for key, value in os.environ.items()
            if key != "WARP_IS_LOCAL_SHELL_SESSION"
        }
        environment["WARP_EDIT_FALLBACK_EDITOR"] = fake_editor

        result = subprocess.run(
            [binary, "edit", resource],
            capture_output=True,
            timeout=TIMEOUT,
            env=environment,
        )
        if result.returncode != 0:
            raise Failure(f"fallback exited {result.returncode}: {result.stderr!r}")

        with open(resource) as handle:
            if "edited: true" not in handle.read():
                raise Failure("the fallback editor was never run")


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        return 2

    binary = sys.argv[1]
    cases = [
        ("blocks until the edit is finished", lambda: run_case(binary, wait=True)),
        ("--no-wait returns immediately", lambda: run_case(binary, wait=False)),
        ("falls back outside a Warp session", lambda: run_fallback_case(binary)),
    ]

    failed = False
    for name, case in cases:
        try:
            case()
        except Failure as failure:
            print(f"FAIL  {name}\n      {failure}")
            failed = True
        else:
            print(f"ok    {name}")

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
