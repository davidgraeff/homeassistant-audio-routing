#!/usr/bin/env python3
"""Unit tests for the pure logic — runnable anywhere, no Pi and no PipeWire.

Everything that parses external output or makes a decision is tested here against
captured real-world samples, because the Pi this tool targets is not always
reachable and the shell-out layers cannot be exercised off-device.

Run:  python3 -m unittest discover -s tests -v      (from the app directory)
"""

from __future__ import annotations

import array
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import app  # noqa: E402
import btstat  # noqa: E402
import capture  # noqa: E402
import pwctl  # noqa: E402


def pcm(samples: list[int]) -> bytes:
    a = array.array("h", samples)
    if sys.byteorder != "little":
        a.byteswap()
    return a.tobytes()


class TestBlockAnalysis(unittest.TestCase):
    def test_all_zero_block_is_digital_silence(self):
        peak, rms = capture.block_peak_rms(pcm([0] * 128))
        self.assertEqual(peak, 0)
        self.assertEqual(rms, 0.0)

    def test_peak_is_absolute_and_catches_negative_extreme(self):
        # A negative extreme must win; -32768 has no positive counterpart.
        peak, _ = capture.block_peak_rms(pcm([100, -3000, 250]))
        self.assertEqual(peak, 3000)
        peak, _ = capture.block_peak_rms(pcm([-32768, 5]))
        self.assertEqual(peak, 32768)

    def test_rms_of_constant_amplitude_square_equals_amplitude(self):
        _, rms = capture.block_peak_rms(pcm([1000, -1000] * 64))
        self.assertAlmostEqual(rms, 1000.0, places=6)

    def test_empty_and_odd_length_input_do_not_raise(self):
        self.assertEqual(capture.block_peak_rms(b""), (0, 0.0))
        # A trailing odd byte is a torn sample; it must be dropped, not crash.
        self.assertEqual(capture.block_peak_rms(pcm([0, 0]) + b"\x01")[0], 0)

    def test_quiet_audio_is_not_reported_as_silence(self):
        # The whole point: 1 LSB is *not* digital silence.
        peak, _ = capture.block_peak_rms(pcm([0] * 100 + [1]))
        self.assertEqual(peak, 1)
        self.assertGreater(peak, capture.SILENCE_PEAK)


class TestSilenceTracker(unittest.TestCase):
    """Silence must be *measured*, never inferred from the clock.

    Every test here maps to a way the first version fabricated the exact symptom
    the app exists to detect — see capture.py's module docstring.
    """

    BLK = capture.BLOCK_S  # 0.02

    def feed(self, t, n, silent, start=0.0, trusted=True, step=None):
        """n consecutive blocks, one BLOCK_S apart. Returns the next stamp."""
        step = self.BLK if step is None else step
        now = start
        for _ in range(n):
            t.update(silent, now, trusted=trusted)
            now += step
        return now

    def test_streak_is_block_derived_and_does_not_grow_on_the_clock(self):
        """The headline bug: an 8-minute 'silence' with a frozen block cursor."""
        t = capture.SilenceTracker()
        self.feed(t, 50, True)                      # 50 blocks = 1.0 s of silence
        self.assertAlmostEqual(t.streak, 1.0)
        # No further blocks. Time passing must not extend the claim.
        self.assertAlmostEqual(t.streak, 1.0)
        self.assertTrue(t.stalled(now=1000.0))

    def test_streak_resets_on_audio(self):
        t = capture.SilenceTracker()
        nxt = self.feed(t, 10, True)
        self.assertGreater(t.streak, 0)
        t.update(False, nxt)
        self.assertEqual(t.streak, 0.0)

    def test_episode_logged_once_silence_ends_with_block_derived_duration(self):
        t = capture.SilenceTracker(min_episode_s=0.5)
        nxt = self.feed(t, 100, True)               # 2.0 s
        self.assertEqual(len(t.episodes), 0, "not logged until it ends")
        t.update(False, nxt)
        self.assertEqual(len(t.episodes), 1)
        self.assertAlmostEqual(t.episodes[0].duration, 2.0)

    def test_short_blip_is_not_an_episode(self):
        t = capture.SilenceTracker(min_episode_s=0.5)
        nxt = self.feed(t, 5, True)                 # 0.1 s
        t.update(False, nxt)
        self.assertEqual(len(t.episodes), 0)

    def test_a_stall_is_not_billed_as_silence(self):
        """One zero block + a 149 s stall used to be logged as a 149 s dropout."""
        t = capture.SilenceTracker(min_episode_s=0.5)
        t.update(True, 0.0)                         # a single silent block
        t.update(False, 149.0)                      # ...then the reader came back
        self.assertEqual(len(t.episodes), 0, "the gap was never observed as silence")
        self.assertEqual(len(t.stalls), 1)
        self.assertAlmostEqual(t.stalls[0].duration, 149.0 - capture.BLOCK_S, places=6)
        self.assertAlmostEqual(t.lost_s, 149.0 - capture.BLOCK_S, places=6)

    def test_stall_splits_a_silent_run_instead_of_merging_it(self):
        t = capture.SilenceTracker(min_episode_s=0.5)
        nxt = self.feed(t, 50, True)                # 1.0 s of real silence
        nxt = self.feed(t, 50, True, start=nxt + 30.0)   # after a 30 s hole
        t.update(False, nxt)
        # Two measured 1 s episodes, not one 32 s episode: the hole in between was
        # never observed, so it cannot be claimed as silence.
        self.assertEqual([round(e.duration, 3) for e in t.episodes], [1.0, 1.0])
        self.assertEqual(len(t.stalls), 1)
        self.assertAlmostEqual(t.stalls[0].duration, 30.0, places=6)

    def test_late_reader_within_tolerance_does_not_break_the_run(self):
        """A descheduled Python reader on a Zero 2 W loses nothing; blocks buffer."""
        t = capture.SilenceTracker(min_episode_s=0.5, max_gap_s=0.5)
        t.update(True, 0.0)
        t.update(True, 0.4)                         # late, but under tolerance
        nxt = self.feed(t, 30, True, start=0.42)
        t.update(False, nxt)
        self.assertEqual(len(t.stalls), 0)
        self.assertEqual(len(t.episodes), 1)
        self.assertAlmostEqual(t.episodes[0].duration, 32 * self.BLK)

    def test_untrusted_blocks_are_excluded_entirely(self):
        """Blocks captured while the stream was on the wrong node prove nothing."""
        t = capture.SilenceTracker(min_episode_s=0.5)
        nxt = self.feed(t, 100, True, trusted=False)   # 2 s of foreign silence
        self.assertEqual(t.total_blocks, 0)
        self.assertEqual(t.untrusted_blocks, 100)
        self.assertEqual(t.streak, 0.0)
        self.assertEqual(t.duty, 0.0)
        t.update(False, nxt)
        self.assertEqual(len(t.episodes), 0)

    def test_untrusted_stretch_splits_a_trusted_run(self):
        t = capture.SilenceTracker(min_episode_s=0.5)
        nxt = self.feed(t, 50, True)                          # 1 s, trusted
        nxt = self.feed(t, 50, True, start=nxt, trusted=False)
        nxt = self.feed(t, 50, True, start=nxt)               # 1 s, trusted again
        t.update(False, nxt)
        self.assertEqual([round(e.duration, 3) for e in t.episodes], [1.0, 1.0],
                         "the two trusted runs must not be spliced into one 3 s claim")

    def test_duty_cycle(self):
        t = capture.SilenceTracker()
        now = self.feed(t, 3, True)
        self.feed(t, 7, False, start=now)
        self.assertAlmostEqual(t.duty, 0.3)

    def test_coverage_reports_the_hole_in_the_record(self):
        """2 h on the bridge yielded 108.9 min of blocks: coverage 0.92, not 1.0."""
        t = capture.SilenceTracker()
        self.feed(t, 500, False)                    # 10 s of blocks...
        self.assertAlmostEqual(t.coverage(10.0), 1.0, places=3)
        self.assertAlmostEqual(t.coverage(20.0), 0.5, places=3)   # ...over 20 s
        self.assertIsNone(t.coverage(0.0))

    def test_stalled_is_true_before_any_block(self):
        t = capture.SilenceTracker()
        self.assertTrue(t.stalled(0.0))
        self.assertIsNone(t.since_last_block(0.0))


class TestRecordCmd(unittest.TestCase):
    """`pw-record --target` is a preference, not a pin — these two props fix that."""

    def test_target_is_pinned_so_a_rebind_cannot_happen_silently(self):
        cmd = capture.record_cmd("bluez_input.64_B5_F2_F9_A9_4A.2")
        props = cmd[cmd.index("-P") + 1]
        self.assertIn("node.dont-reconnect=true", props)
        self.assertIn(f"node.name={capture.STREAM_NODE}", props)
        self.assertEqual(cmd[cmd.index("--target") + 1],
                         "bluez_input.64_B5_F2_F9_A9_4A.2")

    def test_format_matches_what_the_bridge_transmits(self):
        cmd = capture.record_cmd("x")
        self.assertEqual(cmd[cmd.index("--rate") + 1], str(capture.RATE))
        self.assertEqual(cmd[cmd.index("--format") + 1], "s16")
        self.assertIn("--raw", cmd)
        self.assertEqual(cmd[-1], "-")

    def test_stream_node_is_distinct_from_the_bridges_own_capture(self):
        self.assertNotEqual(capture.STREAM_NODE, pwctl.LOOPBACK_CAPTURE)


class TestCaptureSnapshot(unittest.TestCase):
    """The snapshot must publish *why* a reading can or cannot be trusted.

    `Capture` is constructed but never started, so no `pw-record` is spawned — only
    the readout layer is exercised.
    """

    def setUp(self):
        self.cap = capture.Capture("bluez_input.64_B5_F2_F9_A9_4A.2")

    def test_fresh_capture_reports_stalled_and_unchecked(self):
        s = self.cap.snapshot()
        self.assertTrue(s["stalled"], "nothing has arrived yet")
        self.assertIsNone(s["bound"], "binding not checked yet")
        self.assertIsNone(s["last_block_ago_s"])
        self.assertEqual(s["silence_streak_s"], 0.0)
        self.assertEqual(s["stream_node"], capture.STREAM_NODE)

    def test_unbound_capture_reports_what_it_is_actually_on(self):
        self.cap.set_binding(False, ["rtp-bridge"])
        s = self.cap.snapshot()
        self.assertFalse(s["bound"])
        self.assertEqual(s["fed_by"], ["rtp-bridge"])

    def test_binding_is_cleared_when_the_target_changes(self):
        self.cap.set_binding(True, ["bluez_input.64_B5_F2_F9_A9_4A.2"])
        self.cap.set_target("rtp-bridge")
        s = self.cap.snapshot()
        self.assertIsNone(s["bound"], "stale binding must not carry over")
        self.assertEqual(s["fed_by"], [])
        self.assertEqual(s["target"], "rtp-bridge")

    def test_unbound_right_after_a_switch_is_unknown_not_wrong(self):
        """"Not linked yet" must not discard the first seconds of a measurement."""
        self.cap.set_target("rtp-bridge")            # starts the grace period
        self.cap.set_binding(False, [])
        self.assertIsNone(self.cap.snapshot()["bound"])

    def test_unbound_after_the_grace_period_is_believed(self):
        self.cap.set_target("rtp-bridge")
        self.cap._changed_at -= capture.BIND_GRACE_S + 1.0
        self.cap.set_binding(False, ["rtp-bridge-monitor"])
        self.assertFalse(self.cap.snapshot()["bound"])

    def test_a_positive_verdict_ends_the_grace_period(self):
        self.cap.set_target("rtp-bridge")
        self.cap.set_binding(True, ["rtp-bridge"])
        self.cap.set_binding(False, [])              # a real loss, not a slow link
        self.assertFalse(self.cap.snapshot()["bound"])

    def test_streak_and_episodes_come_from_the_tracker(self):
        for i in range(50):
            self.cap.silence.update(True, i * capture.BLOCK_S)
        s = self.cap.snapshot()
        self.assertAlmostEqual(s["silence_streak_s"], 1.0)
        self.assertAlmostEqual(s["measured_s"], 1.0)


class TestBindingDecision(unittest.TestCase):
    """When to trust the capture, and when to replace it.

    `node.dont-reconnect` leaves a stream whose target vanished unlinked *forever*
    (measured on the bridge), so without the respawn the app would go blind for the
    rest of the session the moment the phone dropped.
    """

    PHONE = "bluez_input.64_B5_F2_F9_A9_4A.2"

    def decide(self, **kw):
        args = dict(pipewire_ok=True, fed_by=[self.PHONE], target=self.PHONE,
                    node_names=[self.PHONE, "rtp-bridge"], unbound_since=None, now=100.0)
        args.update(kw)
        return app.binding_decision(**args)

    def test_bound_when_fed_by_exactly_the_target(self):
        self.assertEqual(self.decide(), (True, None, False))

    def test_unbound_when_fed_by_something_else(self):
        bound, since, respawn = self.decide(fed_by=["rtp-bridge"])
        self.assertFalse(bound)
        self.assertEqual(since, 100.0, "the clock starts on the unbound state")
        self.assertFalse(respawn, "not immediately — a normal relink gets a grace period")

    def test_respawn_once_the_grace_period_expires(self):
        _, _, respawn = self.decide(fed_by=[], unbound_since=100.0, now=100.0 + 6.0)
        self.assertTrue(respawn)

    def test_no_respawn_while_the_target_is_absent(self):
        """Phone off: nothing to bind to, so churning processes buys nothing."""
        bound, since, respawn = self.decide(fed_by=[], node_names=["rtp-bridge"],
                                            unbound_since=1.0, now=1000.0)
        self.assertFalse(bound)
        self.assertFalse(respawn)
        self.assertIsNone(since)

    def test_unreadable_pipewire_is_unknown_not_unbound(self):
        """A failed pw-dump must not discard good blocks as untrusted."""
        self.assertEqual(self.decide(pipewire_ok=False), (None, None, False))

    def test_extra_feed_is_not_bound(self):
        """Two sources into our stream means we cannot attribute what we hear."""
        bound, _, _ = self.decide(fed_by=[self.PHONE, "rtp-bridge"])
        self.assertFalse(bound)

    def test_no_target_is_never_bound(self):
        bound, _, respawn = self.decide(target=None, fed_by=[])
        self.assertFalse(bound)
        self.assertFalse(respawn)


class TestAutoTarget(unittest.TestCase):
    """Adopting a phone that connects later — without stealing a chosen target."""

    PHONE = "bluez_input.64_B5_F2_F9_A9_4A.2"
    TARGETS = [{"node": PHONE, "kind": "a2dp-source"},
               {"node": "rtp-bridge", "kind": "sender"}]

    def test_adopts_a_phone_that_appears_while_on_the_fallback(self):
        self.assertEqual(app.auto_target("rtp-bridge", self.TARGETS, True), self.PHONE)

    def test_stays_put_once_already_on_the_phone(self):
        self.assertIsNone(app.auto_target(self.PHONE, self.TARGETS, True))

    def test_never_moves_away_from_a_hand_picked_target(self):
        self.assertIsNone(app.auto_target("rtp-bridge", self.TARGETS, False))

    def test_no_phone_means_no_move(self):
        targets = [{"node": "rtp-bridge", "kind": "sender"}]
        self.assertIsNone(app.auto_target("rtp-bridge", targets, True))
        self.assertIsNone(app.auto_target(None, [], True))


class TestHciParsing(unittest.TestCase):
    # Real output from the Pi Zero 2 W bridge.
    SAMPLE = (
        "hci0:\tType: Primary  Bus: UART\n"
        "\tRX bytes:1156847603 acl:1764508 sco:0 events:3502 errors:0\n"
        "\tTX bytes:182816 acl:4431 sco:0 commands:490 errors:0\n"
    )

    def test_rx_tx_parsed(self):
        rx, tx = btstat.parse_hci_rx_tx(self.SAMPLE)
        self.assertEqual(rx, 1156847603)
        self.assertEqual(tx, 182816)

    def test_value_is_glued_to_label(self):
        """Regression: a field-split parser returns the string 'RX', not a number."""
        rx, _ = btstat.parse_hci_rx_tx("\tRX bytes:42 acl:1\n")
        self.assertEqual(rx, 42)

    def test_missing_input_is_none_not_error(self):
        self.assertEqual(btstat.parse_hci_rx_tx(""), (None, None))


class TestSnmpParsing(unittest.TestCase):
    SAMPLE = (
        "Udp: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors\n"
        "Udp: 100 2 0 987654 0 0\n"
    )

    def test_out_datagrams_found_by_column_name(self):
        self.assertEqual(btstat.parse_udp_out_datagrams(self.SAMPLE), 987654)

    def test_column_order_is_not_assumed(self):
        swapped = (
            "Udp: OutDatagrams InDatagrams\n"
            "Udp: 7 9\n"
        )
        self.assertEqual(btstat.parse_udp_out_datagrams(swapped), 7)

    def test_absent_section(self):
        self.assertIsNone(btstat.parse_udp_out_datagrams("Tcp: A B\nTcp: 1 2\n"))


class TestBusctlParsing(unittest.TestCase):
    def test_transport_paths_extracted_and_deduped(self):
        tree = (
            "    | | ├─ /org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/fd13\n"
            "    | | ├─ /org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/sep1\n"
            "    | | ├─ /org/bluez/hci0/dev_00_1A_7D_DA_71_15/fd7\n"
            "    | | ├─ /org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/fd13\n"
        )
        paths = btstat.parse_transport_paths(tree)
        self.assertEqual(len(paths), 2)
        self.assertTrue(all("/fd" in p for p in paths))
        self.assertFalse(any("sep" in p for p in paths))

    def test_transport_nested_under_a_sep(self):
        """Real BlueZ 5.82 output: the fd object sits under /sepN, not the device."""
        tree = (
            "    \u2502   \u2502 \u2514\u2500 /org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/sep3/fd1\n"
            "    \u2502   \u251c\u2500 /org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/sep4\n"
            "    \u2502   \u251c\u2500 /org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/player0\n"
        )
        paths = btstat.parse_transport_paths(tree)
        self.assertEqual(paths, ["/org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/sep3/fd1"])

    def test_both_transport_shapes_from_the_same_bridge(self):
        tree = (
            "/org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/fd13\n"
            "/org/bluez/hci0/dev_00_1A_7D_DA_71_15/sep1/fd2\n"
        )
        self.assertEqual(len(btstat.parse_transport_paths(tree)), 2)

    def test_scalar_forms(self):
        self.assertEqual(btstat.parse_busctl_scalar('s "active"'), "active")
        self.assertEqual(btstat.parse_busctl_scalar("y 255"), "255")
        self.assertEqual(btstat.parse_busctl_scalar("b true"), "true")
        self.assertIsNone(btstat.parse_busctl_scalar(""))


class TestRate(unittest.TestCase):
    def test_first_sample_has_no_rate(self):
        r = btstat.Rate()
        self.assertIsNone(r.update(1000, now=0.0))

    def test_rate_per_second(self):
        r = btstat.Rate()
        r.update(0, now=0.0)
        self.assertAlmostEqual(r.update(46000, now=1.0), 46000.0)

    def test_counter_reset_is_skipped_not_reported_negative(self):
        r = btstat.Rate()
        r.update(5000, now=0.0)
        self.assertIsNone(r.update(10, now=1.0))

    def test_none_clears_baseline(self):
        r = btstat.Rate()
        r.update(100, now=0.0)
        self.assertIsNone(r.update(None, now=1.0))
        self.assertIsNone(r.update(200, now=2.0), "baseline was cleared")


class TestCodecDetection(unittest.TestCase):
    def test_longest_match_wins(self):
        self.assertEqual(pwctl.codec_of("A2DP Sink (aptX HD)"), "aptx_hd")
        self.assertEqual(pwctl.codec_of("a2dp-sink-aptx"), "aptx")
        self.assertEqual(pwctl.codec_of("A2DP Source (SBC-XQ)"), "sbc_xq")
        self.assertEqual(pwctl.codec_of("a2dp-source-sbc"), "sbc")

    def test_no_codec_in_plain_profile(self):
        self.assertIsNone(pwctl.codec_of("off"))
        self.assertIsNone(pwctl.codec_of("Handsfree"))


class TestPwDumpParsing(unittest.TestCase):
    DUMP = [
        {
            "id": 40, "type": "PipeWire:Interface:Device",
            "info": {
                "props": {"device.api": "bluez5", "device.name": "bluez_card.64_B5",
                          "device.alias": "David's Phone",
                          "api.bluez5.address": "64:B5:F2:F9:A9:4A"},
                "params": {
                    "EnumProfile": [
                        {"index": 0, "name": "off", "description": "Off"},
                        {"index": 1, "name": "a2dp-source-sbc",
                         "description": "A2DP Source (SBC)", "available": "yes"},
                        {"index": 2, "name": "a2dp-source-aptx",
                         "description": "A2DP Source (aptX)", "available": "yes"},
                    ],
                    "Profile": [{"index": 2, "name": "a2dp-source-aptx"}],
                },
            },
        },
        {
            "id": 51, "type": "PipeWire:Interface:Node",
            "info": {"state": "running",
                     "props": {"node.name": "bluez_input.64_B5_F2_F9_A9_4A.2",
                               "node.description": "David's Phone",
                               "media.class": "Audio/Source",
                               "api.bluez5.codec": "aptx",
                               "api.bluez5.profile": "a2dp-source",
                               "api.bluez5.address": "64:B5:F2:F9:A9:4A"}},
        },
        {
            "id": 60, "type": "PipeWire:Interface:Node",
            "info": {"state": "running",
                     "props": {"node.name": "rtp-bridge", "media.class": "Audio/Sink",
                               "node.description": "Bluetooth to RTP bridge (sender)"}},
        },
        {
            "id": 61, "type": "PipeWire:Interface:Node",
            "info": {"state": "running", "props": {"node.name": "bt-bridge-capture"}},
        },
        {
            "id": 62, "type": "PipeWire:Interface:Node",
            "info": {"state": "running", "props": {"node.name": "bt-bridge-playback"}},
        },
        {
            "id": 70, "type": "PipeWire:Interface:Link",
            # Real pw-dump gives integer node IDS here, not names.
            "info": {"state": "active",
                     "props": {"link.output.node": 51, "link.input.node": 61}},
        },
        {
            "id": 71, "type": "PipeWire:Interface:Link",
            "info": {"state": "active",
                     "props": {"link.output.node": 62, "link.input.node": 60}},
        },
    ]

    def test_bluez_device_and_codec_profiles(self):
        devs = pwctl.parse_bluez_devices(self.DUMP)
        self.assertEqual(len(devs), 1)
        d = devs[0]
        self.assertEqual(d.address, "64:B5:F2:F9:A9:4A")
        self.assertEqual(d.current_profile, "a2dp-source-aptx")
        self.assertEqual([p.codec for p in d.codec_profiles], ["sbc", "aptx"])
        self.assertNotIn("off", [p.name for p in d.codec_profiles])

    def test_levers_detects_profile_switching(self):
        levers = pwctl.available_levers(pwctl.parse_bluez_devices(self.DUMP))
        self.assertTrue(levers["profile_switching"])
        self.assertTrue(levers["dropin"])

    def test_levers_without_codec_profiles(self):
        dump = [{
            "id": 1, "type": "PipeWire:Interface:Device",
            "info": {"props": {"device.api": "bluez5", "device.name": "x"},
                     "params": {"EnumProfile": [{"index": 0, "name": "off"}]}},
        }]
        levers = pwctl.available_levers(pwctl.parse_bluez_devices(dump))
        self.assertFalse(levers["profile_switching"])
        self.assertTrue(levers["dropin"], "the drop-in lever always exists")

    def test_a2dp_source_nodes_and_capture_targets(self):
        nodes = pwctl.parse_nodes(self.DUMP)
        srcs = pwctl.a2dp_source_nodes(nodes)
        self.assertEqual([n.bt_codec for n in srcs], ["aptx"])
        targets = pwctl.capture_targets(nodes)
        self.assertEqual(targets[0]["kind"], "a2dp-source",
                         "the phone must be the default target")
        self.assertIn("rtp-bridge", [t["node"] for t in targets])

    def test_links_resolve_integer_node_ids_to_names(self):
        """Regression: pw-dump gives node IDs, not names, on link props."""
        links = pwctl.parse_links(self.DUMP)
        self.assertTrue(links)
        pairs = {(l["output_node"], l["input_node"]) for l in links}
        self.assertIn(("bluez_input.64_B5_F2_F9_A9_4A.2", "bt-bridge-capture"), pairs)
        self.assertIn(("bt-bridge-playback", "rtp-bridge"), pairs)
        self.assertTrue(all(l["output_node"] for l in links), "names must resolve")

    def test_sender_chain_healthy(self):
        nodes, links = pwctl.parse_nodes(self.DUMP), pwctl.parse_links(self.DUMP)
        chain = pwctl.sender_chain(nodes, links)
        self.assertTrue(chain["capture_linked"])
        self.assertTrue(chain["sender_linked"])
        self.assertTrue(chain["capture_bound_to_a2dp"])
        self.assertEqual([h["present"] for h in chain["hops"]], [True, True, True])

    def test_sender_chain_flags_capture_bound_to_wrong_source(self):
        """The documented hazard: the unpinned capture end binding to a monitor."""
        dump = [o for o in self.DUMP if o["id"] != 70]
        dump.append({
            "id": 72, "type": "PipeWire:Interface:Link",
            "info": {"state": "active",
                     "props": {"link.output.node": 60, "link.input.node": 61}},
        })
        chain = pwctl.sender_chain(pwctl.parse_nodes(dump), pwctl.parse_links(dump))
        self.assertTrue(chain["capture_linked"])
        self.assertFalse(chain["capture_bound_to_a2dp"])

    def test_empty_dump_is_survivable(self):
        chain = pwctl.sender_chain([], [])
        self.assertFalse(chain["capture_linked"])
        self.assertEqual(chain["a2dp_sources"], [])
        self.assertEqual(pwctl.capture_targets([]), [])

    # -- our own capture stream's binding -------------------------------------

    PHONE = "bluez_input.64_B5_F2_F9_A9_4A.2"

    def _with_capture_link(self, output_id: int):
        """The DUMP plus our capture stream node, fed by `output_id`."""
        dump = list(self.DUMP)
        dump.append({
            "id": 93, "type": "PipeWire:Interface:Node",
            "info": {"state": "running",
                     "props": {"node.name": capture.STREAM_NODE,
                               "media.class": "Stream/Input/Audio"}},
        })
        dump.append({
            "id": 80, "type": "PipeWire:Interface:Link",
            "info": {"state": "active",
                     "props": {"link.output.node": output_id, "link.input.node": 93}},
        })
        return dump

    def test_capture_binding_on_the_phone_is_bound(self):
        links = pwctl.parse_links(self._with_capture_link(51))
        b = pwctl.capture_binding(links, capture.STREAM_NODE, self.PHONE)
        self.assertTrue(b["bound"])
        self.assertEqual(b["fed_by"], [self.PHONE])

    def test_capture_binding_catches_the_rebind_to_the_rtp_monitor(self):
        """The measured failure: target gone, stream silently moved to the sink's
        monitor, app still naming the phone and calling it digital silence."""
        links = pwctl.parse_links(self._with_capture_link(60))  # rtp-bridge
        b = pwctl.capture_binding(links, capture.STREAM_NODE, self.PHONE)
        self.assertFalse(b["bound"])
        self.assertEqual(b["fed_by"], ["rtp-bridge"])
        self.assertTrue(b["present"], "linked, just to the wrong thing")

    def test_capture_binding_with_no_link_or_no_target(self):
        links = pwctl.parse_links(self.DUMP)
        b = pwctl.capture_binding(links, capture.STREAM_NODE, self.PHONE)
        self.assertFalse(b["bound"])
        self.assertFalse(b["present"])
        self.assertEqual(b["fed_by"], [])
        # No target selected can never count as bound.
        b = pwctl.capture_binding(pwctl.parse_links(self._with_capture_link(51)),
                                  capture.STREAM_NODE, None)
        self.assertFalse(b["bound"])

    def test_feeding_nodes_is_deduped_and_sorted(self):
        links = [{"output_node": "b", "input_node": "x"},
                 {"output_node": "a", "input_node": "x"},
                 {"output_node": "a", "input_node": "x"},
                 {"output_node": "c", "input_node": "y"}]
        self.assertEqual(pwctl.feeding_nodes(links, "x"), ["a", "b"])
        self.assertEqual(pwctl.feeding_nodes(links, "z"), [])


class TestNormalizeCodecs(unittest.TestCase):
    def test_dedupes_and_orders_best_first(self):
        accepted, rejected = pwctl.normalize_codecs(["sbc", "aac", "sbc", "ldac"])
        self.assertEqual(accepted, ["ldac", "aac", "sbc"], "KNOWN_CODECS order")
        self.assertEqual(rejected, [])

    def test_case_and_dash_insensitive(self):
        accepted, rejected = pwctl.normalize_codecs(["AAC", "aptX-HD"])
        self.assertEqual(accepted, ["aptx_hd", "aac"])
        self.assertEqual(rejected, [])

    def test_unknown_names_reported_not_silently_dropped(self):
        accepted, rejected = pwctl.normalize_codecs(["sbc", "mp3", "nonsense"])
        self.assertEqual(accepted, ["sbc"])
        self.assertEqual(rejected, ["mp3", "nonsense"])

    def test_bare_string_is_a_one_element_set(self):
        self.assertEqual(pwctl.normalize_codecs("sbc")[0], ["sbc"])

    def test_empty_means_no_restriction(self):
        self.assertEqual(pwctl.normalize_codecs([]), ([], []))
        self.assertEqual(pwctl.normalize_codecs(None), ([], []))


class TestCodecDropin(unittest.TestCase):
    def test_allow_set_round_trip(self):
        with tempfile.TemporaryDirectory() as home:
            ok, msg = pwctl.write_codec_dropin(["sbc", "aac"], home=home)
            self.assertTrue(ok, msg)
            self.assertEqual(pwctl.read_codec_dropin(home=home), ["aac", "sbc"])

    def test_the_exclude_aptx_experiment(self):
        """The motivating case: allow everything except aptX and aptX-HD."""
        keep = [c for c in pwctl.KNOWN_CODECS if c not in ("aptx", "aptx_hd")]
        with tempfile.TemporaryDirectory() as home:
            ok, msg = pwctl.write_codec_dropin(keep, home=home)
            self.assertTrue(ok)
            self.assertIn("excluding", msg)
            self.assertIn("aptx", msg)
            got = pwctl.read_codec_dropin(home=home)
            self.assertNotIn("aptx", got)
            self.assertNotIn("aptx_hd", got)
            self.assertIn("aac", got)
            self.assertIn("sbc", got)
            text = pwctl.render_codec_dropin(keep)
            # The excluded names appear only in the human comment, never in the list.
            listed = text.split("bluez5.codecs")[1]
            self.assertNotIn("aptx", listed)

    def test_no_restriction_reads_back_empty(self):
        with tempfile.TemporaryDirectory() as home:
            pwctl.write_codec_dropin(["aptx"], home=home)
            self.assertEqual(pwctl.read_codec_dropin(home=home), ["aptx"])
            pwctl.write_codec_dropin([], home=home)
            self.assertEqual(pwctl.read_codec_dropin(home=home), [])

    def test_missing_file_is_empty(self):
        with tempfile.TemporaryDirectory() as home:
            self.assertEqual(pwctl.read_codec_dropin(home=home), [])

    def test_unknown_codec_is_refused_and_nothing_written(self):
        with tempfile.TemporaryDirectory() as home:
            ok, msg = pwctl.write_codec_dropin(["sbc", "bogus"], home=home)
            self.assertFalse(ok)
            self.assertIn("bogus", msg)
            self.assertFalse(os.path.exists(pwctl.codec_dropin_path(home)))

    def test_selection_that_allows_nothing_is_refused(self):
        """An empty allow-list would stop audio — and look like the very bug
        being investigated. Refuse rather than write it."""
        with tempfile.TemporaryDirectory() as home:
            ok, msg = pwctl.write_codec_dropin(["definitely_not_a_codec"], home=home)
            self.assertFalse(ok)
            self.assertFalse(os.path.exists(pwctl.codec_dropin_path(home)))

    def test_rendered_dropin_is_spa_json_list(self):
        text = pwctl.render_codec_dropin(["aac", "sbc"])
        self.assertIn("monitor.bluez.properties", text)
        self.assertIn("bluez5.codecs = [ aac sbc ]", text)

    def test_dropin_warns_about_reconnect_and_says_allow_list(self):
        text = pwctl.render_codec_dropin(["sbc"])
        self.assertIn("RECONNECT", text)
        self.assertIn("ALLOW-LIST", text)


class TestApplyCodecs(unittest.TestCase):
    """The safe apply ordering: disconnect -> restart WirePlumber -> reconnect.

    `restart` is injected in every case — an earlier smoke test that let it fall
    through to the real `systemctl --user restart wireplumber` duly restarted the
    developer's audio stack.
    """

    def setUp(self):
        self.home = tempfile.TemporaryDirectory()
        self.addCleanup(self.home.cleanup)
        self.calls = []

    def _ok(self, label):
        def f(*a):
            self.calls.append(label)
            return True, label
        return f

    def _fail(self, label):
        def f(*a):
            self.calls.append(label)
            return False, f"{label} failed"
        return f

    def test_happy_path_order(self):
        ok, steps = pwctl.apply_codecs(
            ["aac", "sbc"], "AA:BB:CC:DD:EE:FF",
            self._ok("disconnect"), self._ok("connect"),
            restart=self._ok("restart"), home=self.home.name)
        self.assertTrue(ok)
        self.assertEqual(self.calls, ["disconnect", "restart", "connect"])
        self.assertEqual(pwctl.read_codec_dropin(home=self.home.name), ["aac", "sbc"])

    def test_failed_disconnect_stops_before_the_restart(self):
        """The hazard guard: never restart WirePlumber with a phone attached."""
        ok, steps = pwctl.apply_codecs(
            ["aac"], "AA:BB:CC:DD:EE:FF",
            self._fail("disconnect"), self._ok("connect"),
            restart=self._ok("restart"), home=self.home.name)
        self.assertFalse(ok)
        self.assertNotIn("restart", self.calls)
        self.assertTrue(any("orphan" in s for s in steps))

    def test_failed_reconnect_is_reported_as_failure(self):
        """Audio is down until the phone returns — don't claim success."""
        ok, steps = pwctl.apply_codecs(
            ["aac"], "AA:BB:CC:DD:EE:FF",
            self._ok("disconnect"), self._fail("connect"),
            restart=self._ok("restart"), home=self.home.name)
        self.assertFalse(ok)
        self.assertTrue(any("reconnect it from the phone" in s for s in steps))

    def test_bad_codec_writes_nothing_and_skips_bluetooth(self):
        ok, steps = pwctl.apply_codecs(
            ["bogus"], "AA:BB:CC:DD:EE:FF",
            self._ok("disconnect"), self._ok("connect"),
            restart=self._ok("restart"), home=self.home.name)
        self.assertFalse(ok)
        self.assertEqual(self.calls, [], "must not touch Bluetooth on a bad request")

    def test_no_address_still_writes_and_restarts(self):
        ok, steps = pwctl.apply_codecs(
            ["sbc"], None,
            self._ok("disconnect"), self._ok("connect"),
            restart=self._ok("restart"), home=self.home.name)
        self.assertTrue(ok)
        self.assertEqual(self.calls, ["restart"])


class TestDropinLocation(unittest.TestCase):
    def test_dropin_goes_to_wireplumber_not_pipewire(self):
        """Verified on the bridge: bluez.lua reads `monitor.bluez.properties`
        from WirePlumber's config, so `bluez5.codecs` in pipewire.conf.d is
        silently ignored."""
        path = pwctl.codec_dropin_path("/home/x")
        self.assertIn("wireplumber.conf.d", path)
        self.assertNotIn("pipewire.conf.d", path)

    def test_section_is_monitor_bluez_properties(self):
        text = pwctl.render_codec_dropin(["sbc"])
        self.assertIn("monitor.bluez.properties", text)
        self.assertNotIn("context.properties", text)

    def test_prefix_outranks_setup_script_dropin(self):
        import os as _os
        name = _os.path.basename(pwctl.codec_dropin_path("/home/x"))
        self.assertGreater(name, "51-bt-rtp-bridge.conf")


if __name__ == "__main__":
    unittest.main()
