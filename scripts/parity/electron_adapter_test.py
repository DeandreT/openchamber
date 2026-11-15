"""Control targets keep their selector or exact-button contract through CDP."""

from pathlib import Path
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from electron_adapter import ElectronClient


class ElectronTargetTest(unittest.TestCase):
    def test_control_click_accepts_both_target_kinds(self):
        client = ElectronClient(Path("unused"), {}, 1, 1280, 800)
        for spec in ({"button": "Choose project"}, {"selector": "button[aria-haspopup='dialog']"}):
            with patch.object(client, "_call", return_value={}) as call:
                client.click_target(spec)
                call.assert_called_once_with("click", **spec)

    def test_launch_keeps_output_inside_the_private_profile(self):
        env = {"OPENCHAMBER_DATA_DIR": "/private/run/state/oc-data"}
        client = ElectronClient(Path("unused"), env, 1, 1280, 800)
        with patch("virtual_display.child_environment", return_value=env):
            with patch("electron_adapter.subprocess.Popen"):
                with patch.object(client, "_call", return_value={}) as call:
                    client.launch()
        self.assertEqual(call.call_args.kwargs["logPath"], "/private/run/state/electron.log")

    def test_chords_send_every_press_and_release(self):
        client = ElectronClient(Path("unused"), {}, 1, 1280, 800)
        with patch.object(client, "_call", return_value={}) as call:
            client.key("ctrl+z")
        command, = call.call_args.args
        events = call.call_args.kwargs["events"]
        self.assertEqual(command, "key")
        self.assertEqual([(event["type"], event["key"]) for event in events],
                         [("rawKeyDown", "Control"), ("rawKeyDown", "z"), ("keyUp", "z"), ("keyUp", "Control")])

    def test_step_commands_carry_paths_queries_and_rules(self):
        client = ElectronClient(Path("unused"), {}, 1, 1280, 800)
        with patch.object(client, "_call", return_value={"hits": []}) as call:
            client.click_named({"name": "Send message", "role": "button"})
            client.drop({"selector": ".composer"}, [Path("/private/inputs/1/a.txt")])
            client.pick([Path("/private/inputs/2/b.txt")], 2.5)
            client.restart("exit", 20)
            client.commit_composition("raw", "nihao")
            client.add_fault({"id": "f"})
        self.assertEqual([entry.args[0] for entry in call.call_args_list], ["clickNamed", "drop", "pick", "restart", "ime", "fault"])
        self.assertEqual(call.call_args_list[0].kwargs, {"name": "Send message", "role": "button"})
        self.assertEqual(call.call_args_list[1].kwargs, {"selector": ".composer", "files": ["/private/inputs/1/a.txt"]})
        self.assertEqual(call.call_args_list[2].kwargs, {"files": ["/private/inputs/2/b.txt"], "timeoutMs": 2500})
        self.assertEqual(call.call_args_list[3].kwargs, {"after": "exit", "timeoutMs": 20000})
        self.assertEqual(call.call_args_list[4].kwargs, {"commit": "nihao"})
        self.assertEqual(call.call_args_list[5].kwargs, {"rule": {"id": "f"}})

    def test_parameters_cannot_replace_the_message_id(self):
        client = ElectronClient(Path("unused"), {}, 1, 1280, 800)
        client._bridge = SimpleNamespace(stdin=SimpleNamespace(write=lambda _: None, flush=lambda: None), stdout=object())
        with self.assertRaises(ValueError):
            client._send("release", id="hold")
        with patch.object(client, "_call", return_value={}) as call:
            client.release_fault("hold")
        call.assert_called_once_with("release", faultId="hold")

    def test_a_pending_chord_does_not_block_and_later_replies_stay_in_order(self):
        import os
        client = ElectronClient(Path("unused"), {}, 1, 1280, 800)
        read, write = os.pipe()
        sent = []
        client._bridge = SimpleNamespace(stdin=SimpleNamespace(write=sent.append, flush=lambda: None), stdout=os.fdopen(read, "rb"))
        handled = client.begin_key("ctrl+v")
        self.assertFalse(handled())
        os.write(write, b'{"id": 1, "ok": true}\n{"id": 2, "ok": true, "value": "x"}\n')
        self.assertTrue(handled())
        self.assertEqual(client._call("observe", expression="1")["value"], "x")
        self.assertEqual(len(sent), 2)
        os.close(write)


if __name__ == '__main__':
    unittest.main()
