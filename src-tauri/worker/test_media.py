import array
import math
import random
import tempfile
import unittest
import wave
from pathlib import Path

from media import metadata, caption_groups, choose_window, safe_layout, timestamp, write_captions


class MediaTests(unittest.TestCase):
    def test_subtitle_timing_and_ass_text_escaping(self):
        words = [
            {"start": index * 0.35, "end": index * 0.35 + 0.25, "text": text}
            for index, text in enumerate(["Those", "braces", "{hide}", "nothing", "here.", "Next", "line."])
        ]
        with tempfile.TemporaryDirectory() as root:
            target = Path(root)
            write_captions(target, words, 4)
            self.assertIn("00:00:00,000 --> 00:00:01,300", (target / "captions.srt").read_text(encoding="utf-8"))
            self.assertIn("｛hide｝", (target / "captions.ass").read_text(encoding="utf-8"))
        groups = list(caption_groups(words, 4))
        self.assertEqual(len(groups), 2)
        self.assertLessEqual(groups[0][1], groups[1][0])
        self.assertEqual(timestamp(59.9996), "00:01:00,000")

    def test_end_prefers_pause_after_reaction(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "analysis.wav"
            with wave.open(str(path), "wb") as output:
                output.setparams((1, 2, 8000, 0, "NONE", "not compressed"))
                output.writeframes(array.array("h", [2000] * (26 * 8000) + [0] * (4 * 8000)).tobytes())
            start, end = choose_window(path, 20, 25, 30)
        self.assertTrue(start <= 20 < end)
        self.assertGreaterEqual(end-start, 25)
        self.assertAlmostEqual(end, 26.25, delta=0.25)

    def test_adaptive_window_tracks_reaction_tail_and_respects_short_caps(self):
        def window(levels, limits=(10,45), words=()):
            with tempfile.TemporaryDirectory() as root:
                path = Path(root) / "audio.wav"
                with wave.open(str(path), "wb") as output:
                    output.setparams((1, 2, 800, 0, "NONE", ""))
                    for level in levels:
                        output.writeframes(array.array("h", [level]*800).tobytes())
                return choose_window(path, 20, *limits, words)
        compact = window([600]*60)
        brief = window([180]*18 + [2000]*6 + [180]*36)
        sustained = window([180]*18 + [2000]*24 + [180]*18)
        no_release = window([180]*18 + [2000]*32)
        self.assertLess(compact[1]-compact[0], 23)
        self.assertLess(brief[1]-brief[0], sustained[1]-sustained[0])
        self.assertGreaterEqual(sustained[1], 42)
        self.assertGreaterEqual(no_release[1], 49)
        for limits in ((5,5),(7,12),(12,20),(40,45)):
            start,end = window([500]*60,limits)
            self.assertTrue(start<20<end)
            self.assertTrue(limits[0]<=end-start<=limits[1])
        words = [dict(start=19+i*.4,end=19.4+i*.4,text="word") for i in range(50)]
        words[-1]["text"]="done!"
        self.assertGreaterEqual(window([500]*60,words=words)[1],39)
        with self.assertRaises(RuntimeError):
            window([500]*19)

    def test_reaction_alignment_and_publication_evidence(self):
        rng = random.Random(13)
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "analysis.wav"
            for scenario in ("reaction", "impact", "steady", "speech", "truncated", "alert", "late"):
                with wave.open(str(path), "wb") as out:
                    out.setparams((1, 2, 8000, 0, "NONE", ""))
                    for i in range(132):
                        level = 250
                        if (32 <= i < 40 and scenario in ("reaction", "alert")) or (i >= 32 and scenario == "truncated"):
                            level = [3200, 5600, 5200, 4200][i%4]
                        if 90 <= i < 98 and scenario == "late":
                            level = [3200, 5600, 5200, 4200][i%4]
                        if 56 <= i < 64 and scenario == "steady":
                            level = 3900
                        if 56 <= i < 64 and scenario == "speech":
                            level = [550, 880, 750, 650][i%4]
                        if i == 56 and scenario == "impact":
                            out.writeframes(array.array("h", [rng.randrange(-20000, 20001) for _ in range(2000)]).tobytes())
                        else:
                            harmonic = 0 if scenario == "alert" else 0.36
                            out.writeframes(array.array("h", [
                                round(level*(math.sin(phase) + harmonic*math.sin(3*phase)))
                                for phase in (2*math.pi*180*n/8000 for n in range(2000))
                            ]).tobytes())
                evidence = {}
                start, end = choose_window(path, 20, 10, 10, evidence=evidence)
                self.assertLessEqual(10, end-start)
                self.assertLessEqual(end-start, 10)
                self.assertEqual(evidence["surge"], scenario == "reaction", scenario)
                if scenario == "reaction":
                    self.assertEqual(evidence["peak"], 8)
                    self.assertGreaterEqual(evidence["release"], 10)
                    self.assertTrue(start <= 7 < 8 < 10 < end < 20)
                    self.assertGreaterEqual(evidence["strength"], 90)
                else:
                    self.assertTrue(start < 20 < end)
        job = {"creator": "Example"}
        self.assertEqual(metadata(job, [], 5, 6)["quality_version"], 0)
        words = [dict(text=text, start=3+i*.4, end=3.4+i*.4, confidence=.94)
                 for i,text in enumerate(("Did", "you", "see", "that?"))]
        result = metadata(job, words, 4, 5)
        self.assertIn("Did you see that?", result["title"])
        self.assertIn("#Reaction", result["description"])
        self.assertEqual(result["quality_version"], 2)
        self.assertEqual(metadata(job, words, 4)["quality_version"], 0)
        self.assertEqual(metadata(job, words, 5, 6)["quality_version"], 0)
        self.assertEqual(metadata(job, [dict(text="Ah!", start=5.2, end=5.4, confidence=.96)],
                                  5, 6)["quality_version"], 2)
        self.assertEqual(metadata(job, [dict(text="Ah!", start=5.2, end=5.4, confidence=.55)],
                                  5, 6)["quality_version"], 0)

    def test_layout_keeps_full_gameplay_in_each_landscape_strategy(self):
        landscape = {"width": 1920, "height": 1080}
        self.assertIn("force_original_aspect_ratio=decrease", safe_layout(landscape))
        camera = safe_layout(landscape, (480, 240, 100, 100))
        self.assertIn("crop=480:240:100:100", camera)
        self.assertIn("force_original_aspect_ratio=decrease", camera)
        self.assertIn("ass=captions.ass", camera)


if __name__ == "__main__":
    unittest.main()

