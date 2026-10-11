import json
import math
import os
import select
import shutil
import socketserver
import struct
import subprocess
import threading
import time
import unittest


ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AGENT = os.path.join(ROOT, "target", "debug", "avr-volume-agent")


class DenonHandler(socketserver.StreamRequestHandler):
    def _reply(self, payload):
        try:
            self.request.sendall(payload)
            return True
        except OSError:
            return False

    def handle(self):
        self.server.register_client(self.request)
        data = bytearray()
        while True:
            chunk = self.request.recv(256)
            if not chunk:
                return
            data.extend(chunk)
            while b"\r" in data:
                raw, _, rest = data.partition(b"\r")
                data = bytearray(rest)
                line = raw.decode("ascii", "replace").strip()
                if not line:
                    continue
                self.server.commands.append_lock(line)
                if line == "MV?":
                    if not self._reply(b"MV44\rMVMAX 72\r"):
                        return
                elif line == "MU?":
                    if not self._reply(b"MUOFF\r"):
                        return
                elif line.startswith("MV") and line[2:].isdigit():
                    if self.server.reply_delay:
                        time.sleep(self.server.reply_delay)
                    if not self._reply((line + "\r").encode("ascii")):
                        return
                elif line in ("MUON", "MUOFF"):
                    if not self._reply((line + "\r").encode("ascii")):
                        return


class LockedCommands(list):
    def __init__(self):
        super().__init__()
        self.lock = threading.Lock()

    def append_lock(self, command):
        with self.lock:
            self.append(command)

    def snapshot(self):
        with self.lock:
            return list(self)


class MockDenonServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, reply_delay=0):
        super().__init__(("127.0.0.1", 0), DenonHandler)
        self.commands = LockedCommands()
        self.reply_delay = reply_delay
        self.clients = set()
        self.clients_lock = threading.Lock()
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()

    def register_client(self, client):
        with self.clients_lock:
            self.clients.add(client)

    def push_status(self, payload):
        with self.clients_lock:
            clients = tuple(self.clients)
        for client in clients:
            try:
                client.sendall(payload)
            except OSError:
                with self.clients_lock:
                    self.clients.discard(client)

    def close(self):
        self.shutdown()
        self.server_close()
        self.thread.join(timeout=2)


def pactl(*args):
    return subprocess.check_output(["pactl", *args], text=True).strip()


def wait_until(predicate, timeout=4):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return False


def sink_info(name):
    sinks = json.loads(pactl("--format=json", "list", "sinks"))
    return next((sink for sink in sinks if sink["name"] == name), None)


def monitor_rms(source, sink):
    frames = 24000
    tone = b"".join(
        struct.pack("<h", int(8000 * math.sin(2 * math.pi * 440 * frame / 48000)))
        * 6
        for frame in range(frames)
    )
    capture = subprocess.Popen(
        ["parec", f"--device={source}", "--raw", "--format=s16le",
         "--rate=48000", "--channels=6"],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    player = subprocess.Popen(
        ["paplay", f"--device={sink}", "--raw", "--format=s16le",
         "--rate=48000", "--channels=6"],
        stdin=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    try:
        player.communicate(tone, timeout=4)
        deadline = time.monotonic() + 3
        captured = bytearray()
        while len(captured) < len(tone) and time.monotonic() < deadline:
            readable, _, _ = select.select([capture.stdout], [], [], 0.2)
            if readable:
                chunk = os.read(capture.stdout.fileno(), len(tone) - len(captured))
                if not chunk:
                    break
                captured.extend(chunk)
        if len(captured) < len(tone):
            raise AssertionError("timed out capturing the virtual sink output")
        values = struct.unpack("<" + "h" * (len(captured) // 2), captured)
        first = len(values) // 5
        last = len(values) * 4 // 5
        steady = values[first:last]
        return math.sqrt(sum(value * value for value in steady) / len(steady))
    finally:
        if player.poll() is None:
            player.terminate()
        if capture.poll() is None:
            capture.terminate()
        player.wait(timeout=2)
        capture.wait(timeout=2)
        capture.stdout.close()


class PipeWireIntegrationTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        required = ("pactl", "wpctl", "parec", "paplay")
        if not os.path.isfile(AGENT):
            raise unittest.SkipTest("build avr-volume-agent first with `cargo build`")
        if any(shutil.which(tool) is None for tool in required):
            raise unittest.SkipTest("PipeWire Pulse tools are not installed")
        try:
            pactl("info")
        except subprocess.CalledProcessError:
            raise unittest.SkipTest("no PipeWire-Pulse server is available")

    def setUp(self):
        self.sink_name = f"avrtest_{os.getpid()}"
        self.node_name = f"avr_volume_test_{os.getpid()}"
        existing = sink_info(self.sink_name)
        if existing:
            self.fail(f"test sink already exists: {self.sink_name}")
        self.module_id = pactl(
            "load-module", "module-null-sink", f"sink_name={self.sink_name}",
            "channels=6",
            "channel_map=front-left,front-right,rear-left,rear-right,front-center,lfe",
            "rate=48000", "format=float32le",
        )
        self.avr = MockDenonServer()
        self.agent = subprocess.Popen(
            [AGENT, "--target-sink", self.sink_name,
             "--avr-host", "127.0.0.1", "--port", str(self.avr.server_address[1]),
             "--node-name", self.node_name],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        if not wait_until(lambda: sink_info(self.node_name) is not None):
            log = self._stop_agent()
            self.avr.close()
            pactl("unload-module", self.module_id)
            self.fail(f"virtual sink did not register; agent output: {log}")

    def tearDown(self):
        self._stop_agent()
        self.avr.close()
        pactl("unload-module", self.module_id)

    def _stop_agent(self):
        if self.agent.poll() is None:
            self.agent.terminate()
        try:
            _, log = self.agent.communicate(timeout=4)
        except subprocess.TimeoutExpired:
            self.agent.kill()
            _, log = self.agent.communicate(timeout=2)
        return log

    def _wait_command(self, command):
        if not wait_until(lambda: command in self.avr.commands.snapshot()):
            log = self._stop_agent()
            self.fail(
                f"AVR command was not sent: {command}; got "
                f"{self.avr.commands.snapshot()}; agent log: {log}"
            )

    def _set_slider(self, value):
        pactl("set-sink-volume", self.node_name, f"{value}%")
        code = round(value * 72 / 100)
        self._wait_command(f"MV{code:02d}")
        self.assertTrue(wait_until(
            lambda: sink_info(self.node_name)["volume"]["front-left"]["value_percent"]
            .rstrip("%") == str(value)
        ), f"sink slider did not report {value}%")

    def test_slider_controls_avr_and_audio_stays_unattenuated(self):
        target_monitor = f"{self.sink_name}.monitor"
        pactl("set-sink-volume", self.sink_name, "100%")
        self.assertTrue(wait_until(lambda: "MV?" in self.avr.commands.snapshot()))
        self.assertTrue(wait_until(lambda: "MU?" in self.avr.commands.snapshot()))
        time.sleep(1.1)
        startup_commands = self.avr.commands.snapshot()
        self.assertFalse(
            any(command.startswith("MV") and command != "MV?"
                for command in startup_commands),
            f"startup restored volume must not overwrite the AVR's current setting: {startup_commands}",
        )

        self._set_slider(10)
        low_volume_rms = monitor_rms(target_monitor, self.node_name)
        self._set_slider(100)
        full_volume_rms = monitor_rms(target_monitor, self.node_name)

        pactl("set-sink-mute", self.node_name, "1")
        self._wait_command("MUON")
        muted_rms = monitor_rms(target_monitor, self.node_name)
        pactl("set-sink-mute", self.node_name, "0")
        self._wait_command("MUOFF")

        self.assertGreater(low_volume_rms, 1000)
        self.assertAlmostEqual(low_volume_rms, full_volume_rms, delta=full_volume_rms * 0.05)
        self.assertAlmostEqual(low_volume_rms, muted_rms, delta=low_volume_rms * 0.05)

    def test_rapid_slider_changes_ignore_stale_avr_replies(self):
        self.avr.reply_delay = 0.25
        self.assertTrue(wait_until(lambda: "MV?" in self.avr.commands.snapshot()))
        time.sleep(2.1)

        pactl("set-sink-volume", self.node_name, "10%")
        self._wait_command("MV07")
        pactl("set-sink-volume", self.node_name, "50%")
        pactl("set-sink-volume", self.node_name, "30%")

        samples = []
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            current = sink_info(self.node_name)
            percent = int(current["volume"]["front-left"]["value_percent"].rstrip("%"))
            samples.append(percent)
            commands = self.avr.commands.snapshot()
            if commands.count("MV22") and 29 <= percent <= 32:
                time.sleep(0.6)
                current = sink_info(self.node_name)
                percent = int(current["volume"]["front-left"]["value_percent"].rstrip("%"))
                samples.append(percent)
                if 29 <= percent <= 32:
                    break
            time.sleep(0.05)

        self.assertIn("MV22", self.avr.commands.snapshot())
        self.assertTrue(samples, "no volume samples were collected")
        self.assertGreaterEqual(min(samples), 29, f"volume bounced to an obsolete value: {samples}")

    def test_receiver_volume_updates_desktop_without_echoing_back(self):
        self.assertTrue(wait_until(lambda: "MV?" in self.avr.commands.snapshot()))
        time.sleep(1.1)
        command_count = len(self.avr.commands.snapshot())

        self.avr.push_status(b"MV50\r")

        # The AVR code maps to cubic gain; Pulse reports that gain perceptually.
        self.assertTrue(wait_until(lambda: 67 <= int(
            sink_info(self.node_name)["volume"]["front-left"]["value_percent"].rstrip("%")
        ) <= 71))
        new_commands = self.avr.commands.snapshot()[command_count:]
        self.assertFalse(
            any(command.startswith("MV") and command != "MV?" for command in new_commands),
            f"external AVR volume was echoed back as a command: {new_commands}",
        )

        command_count = len(self.avr.commands.snapshot())
        self.avr.push_status(b"MUON\r")
        self.assertTrue(wait_until(lambda: sink_info(self.node_name)["mute"]))
        new_commands = self.avr.commands.snapshot()[command_count:]
        self.assertNotIn("MUON", new_commands, f"external AVR mute was echoed: {new_commands}")


if __name__ == "__main__":
    unittest.main()
