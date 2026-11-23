"""Local media production for ClipForge Live. Reads one job from a JSON file."""
import argparse
import array
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import wave

MODEL = "Systran/faster-whisper-large-v3"
MODEL_REVISION = "edaa852ec7e145841d8ffdb056a99866b5f0a478"
MODEL_SHA256 = "69f74147e3334731bc3a76048724833325d2ec74642fb52620eda87352e3d4f1"

class BudgetExceeded(RuntimeError):
    pass


def run(tool, *args, cwd=None):
    command = [tool, *map(str, args)]
    completed = subprocess.run(
        command, cwd=cwd, stdin=subprocess.DEVNULL, capture_output=True,
        encoding="utf-8", errors="replace", check=False,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    if completed.returncode:
        raise RuntimeError(f"{tool} exited with {completed.returncode}: {completed.stderr[-1600:]}")
    return completed.stdout


def ffmpeg(*args, cwd=None):
    return run("ffmpeg", "-hide_banner", "-nostdin", "-loglevel", "error", "-y", *args, cwd=cwd)


def probe(path):
    return json.loads(run("ffprobe", "-v", "error", "-show_streams", "-show_format", "-of", "json", path))


def validate_video(path, expected=None):
    info = probe(path)
    video = next((track for track in info["streams"] if track["codec_type"] == "video"), None)
    audio = next((track for track in info["streams"] if track["codec_type"] == "audio"), None)
    duration = float(info["format"]["duration"])
    if video is None or audio is None or not math.isfinite(duration) or duration < 1:
        raise RuntimeError("Video failed validation. An intact audio and video track is required.")
    if expected is not None:
        if duration > 180:
            raise RuntimeError("A YouTube Short cannot exceed 180 seconds.")
        if (video["width"], video["height"], video["codec_name"], audio["codec_name"]) != (1080, 1920, "h264", "aac"):
            raise RuntimeError("Rendered file has incorrect resolution or codecs.")
        if abs(duration - expected) > 0.5:
            raise RuntimeError("Rendered file duration does not match the selected highlight.")
        for track in (audio, video):
            start = float(track.get("start_time", "nan"))
            length = float(track.get("duration", "nan"))
            if not (abs(start) <= 0.25 and abs(start + length - expected) <= 0.5):
                raise RuntimeError("A rendered track does not cover the selected highlight.")
        ffmpeg("-xerror", "-i", path, "-f", "null", "-")
    return duration, video


def voice_periodic(samples, rate):
    """Check short-time pitch periodicity without interpreting loudness as speech."""
    step = max(1, rate // 4000)
    centered = samples[len(samples)//2-rate//25:len(samples)//2+rate//25:step]
    mean = sum(centered) / len(centered)
    centered = [value - mean for value in centered]
    hz = rate / step
    # A sinusoid obeys a second-order recurrence. Real voiced sounds
    # carry harmonics or pitch variation that leave a larger residual.
    energy = sum(value*value for value in centered)
    if not energy:
        return False
    coefficient = sum(b*(a+c) for a,b,c in zip(centered, centered[1:], centered[2:])) / energy
    residual = sum((a+c-coefficient*b)**2 for a,b,c in zip(centered, centered[1:], centered[2:]))
    if residual < 0.001 * energy:
        return False
    for lag in range(max(2, round(hz / 500)), round(hz / 75) + 1):
        pairs = list(zip(centered[lag:], centered[:-lag]))
        left = sum(a*a for a, _ in pairs)
        right = sum(b*b for _, b in pairs)
        if left and right and sum(a*b for a, b in pairs) > 0.62 * math.sqrt(left*right):
            return True
    return False


def locate_reaction(levels, voiced, trigger):
    """Find a modulated, voice-like rise and its onset and release."""
    prior = sorted(levels[max(0, int((trigger-18)*4)):max(0, int((trigger-15)*4))])
    if len(prior) < 8:
        return None
    baseline = max(250, prior[len(prior)//2])
    active = [voice and level >= max(650, baseline*1.8)
              for level, voice in zip(levels, voiced)]
    candidates = []
    for i in range(max(0, int((trigger-14)*4)),
                   min(len(levels)-5, int((trigger+2)*4)+1)):
        group = levels[i:i+6]
        strong = [level for level, voice in zip(group, voiced[i:i+6])
                  if voice and level >= max(800, baseline*2.5)]
        if len(strong) < 4 or max(strong) < min(strong)*1.30:
            continue
        onset = next(j for j in range(i, i+6) if active[j])
        if onset / 4 > trigger + 2:
            continue
        while onset >= 2 and any(active[onset-2:onset]) and i-onset < 12:
            onset -= 1
        while not active[onset]:
            onset += 1
        tail = i + 6
        while tail+3 <= len(active) and any(active[tail:tail+3]):
            tail += 1
        if tail+3 > len(active):
            continue
        uplift = sorted(strong)[len(strong)//2] / baseline
        candidates.append((uplift / (1 + .13*abs(i/4-trigger)), onset, tail, uplift))
    if not candidates:
        return None
    _, onset, tail, uplift = max(candidates, key=lambda candidate: (candidate[0], -candidate[1]))
    strength = round(min(100, 75 + 10*math.log2(uplift / 2.5) + min(8, (tail-onset)/4)))
    return onset/4, tail/4, strength


def choose_window(audio_path, lead, minimum, maximum, words=(), *, evidence=None):
    """Choose pause- and phrase-aligned edges around the detected event."""
    with wave.open(str(audio_path), "rb") as wav:
        rate = wav.getframerate()
        window = max(rate // 4, 1)
        levels, voiced = [], []
        while data := wav.readframes(window):
            samples = array.array("h")
            samples.frombytes(data)
            if sys.byteorder != "little":
                samples.byteswap()
            levels.append(math.sqrt(sum(sample * sample for sample in samples) / len(samples)))
            voiced.append(len(samples) == window and levels[-1] >= 650 and voice_periodic(samples, rate))
        available = wav.getnframes() / rate
    if available < minimum or available <= lead:
        raise RuntimeError("Recording ended before the highlight could be captured.")
    maximum = min(available, maximum)
    ordered = sorted(levels)
    floor = ordered[len(ordered) // 5]
    peak = ordered[len(ordered) * 9 // 10]
    quiet = max(100, min(floor * 1.5 + 40, peak * 0.48))
    trigger = lead
    complete_windows = int(available * 4)
    reaction = locate_reaction(levels[:complete_windows], voiced[:complete_windows], trigger)
    release = trigger
    strength = 0
    if reaction:
        onset, tail, candidate_strength = reaction
        lead_in = min(1, maximum/4)
        if tail - onset + 2*lead_in <= maximum:
            lead, release, strength = onset, tail, candidate_strength
    if evidence is not None:
        evidence.update(peak=lead, release=release, surge=bool(strength), strength=strength)
    # Scan out from the event until a sustained pause or a long uninterrupted passage.
    start_goal = max(0, lead - min(8, maximum * 0.45))
    end_goal = min(available, lead + min(10, maximum * 0.58))
    for i in range(int(lead * 4) - 5, max(1, int(start_goal * 4)), -1):
        if max(levels[i-2:i+1]) <= quiet:
            start_goal = i / 4
            break
    before = sorted(levels[:max(1, int((lead - 6) * 4))])
    background = before[len(before)//2]
    elevated = max(200, background * 2.2)
    compact_end = end_goal
    active_run = False
    for i in range(int(lead * 4) + 6, min(len(levels) - 2, int((lead + maximum) * 4))):
        t = i / 4
        nearby = sorted(levels[max(0, i-3):i+1])
        if max(levels[i:i+3]) <= quiet or (active_run and max(levels[i:i+3]) < elevated * 0.65):
            end_goal = t + 0.25
            break
        if nearby[len(nearby)//2] >= elevated:
            active_run = True
            end_goal = max(end_goal, min(available, t + 0.75))
        elif t > compact_end + 5:
            break

    if strength:
        # Distribute required duration around setup and payoff and avoid a long ending.
        start_goal = max(0, lead - min(2, maximum * 0.4))
        end_goal = min(available, release + 0.65)
        extra = max(0, minimum - (end_goal - start_goal))
        end_goal = min(available, end_goal + min(extra * 0.42, 2.10))
        start_goal = max(0, min(start_goal, end_goal - minimum))

    edges = {0.0, available, start_goal, end_goal, *(i / 4 for i in range(len(levels)))}
    phrase_starts, phrase_ends = set(), set()
    for i, word in enumerate(words):
        edges.update((word["start"], word["end"]))
        if i == 0 or word["start"] - words[i-1]["end"] > 0.45 or re.search(r"[.!?]$", words[i-1]["text"]):
            phrase_starts.add(max(0, word["start"] - 0.12))
        if i+1 == len(words) or words[i+1]["start"] - word["end"] > 0.45 or re.search(r"[.!?]$", word["text"]):
            phrase_ends.add(min(available, word["end"] + 0.16))
    # Finish an utterance already in progress at the preferred end.
    for word in words:
        if word["start"] - 0.3 <= end_goal <= word["end"] + 0.3:
            end_goal = min((t for t in phrase_ends if t >= end_goal), default=end_goal)
            break
    edges.update(phrase_starts | phrase_ends)
    def cost(t, goal, phrases):
        cuts_word = any(w["start"] + 0.001 < t < w["end"] - 0.001 for w in words)
        loud = levels[min(len(levels)-1, int(t * 4))] > quiet
        distance = abs(t - goal)
        if strength and phrases is phrase_ends and t > goal:
            distance *= 3
        return distance + 50 * cuts_word + 1.5 * loud - 3 * (t in phrases)
    # Include exact duration-bound pivots for short, fractional or fixed-length captures.
    edges.update(t - minimum for t in list(edges) if t >= minimum)
    starts = [(t, cost(t, start_goal, phrase_starts)) for t in edges if 0 <= t <= lead - min(1, maximum/4)]
    ends = {t: cost(t, end_goal, phrase_ends) for t in edges if release + min(1, maximum/4, (available-release)/2) <= t <= available}
    best = None
    for start, start_cost in starts:
        candidates = ((end, value) for end, value in ends.items() if minimum-1e-6 <= end-start <= maximum+1e-6)
        for end, end_cost in candidates:
            score = (start_cost + end_cost, end-start, start, end)
            if best is None or score < best:
                best = score
    if best is None:
        raise RuntimeError("No complete clip fits the configured duration and trigger.")
    return best[2], best[3]


def timestamp(seconds, ass=False):
    scale = 100 if ass else 1000
    units = max(0, round(seconds * scale))
    hours, units = divmod(units, scale * 3600)
    minutes, units = divmod(units, scale * 60)
    whole, fraction = divmod(units, scale)
    if ass:
        return f"{hours}:{minutes:02}:{whole:02}.{fraction:02}"
    return f"{hours:02}:{minutes:02}:{whole:02},{fraction:03}"


def caption_lines(words, duration):
    """Group recognisable words into short, stable lines with non-overlapping timings."""
    current = []
    usable = []
    for word in words:
        start = max(0, word["start"])
        end = min(duration, word["end"])
        text = word["text"].strip()
        if 0 <= start < end <= duration and text and text != "[unclear]":
            usable.append((start, end, text))
    usable.sort(key=lambda item: item[0])
    for index, (start, end, text) in enumerate(usable):
        if index + 1 < len(usable):
            end = min(end, usable[index + 1][0])
        if round(start * 100) >= round(end * 100):
            continue
        if current and (len(current) == 5 or start - current[-1][1] > 0.55
                        or sum(len(item[2]) + 1 for item in current) + len(text) > 32
                        or end - current[0][0] > 2.7):
            yield current
            current = []
        current.append((start, end, text))
        if re.search(r"[.!?]$", text) and len(current) >= 2:
            yield current
            current = []
    if current:
        yield current


def caption_groups(words, duration):
    for line in caption_lines(words, duration):
        yield line[0][0], line[-1][1], " ".join(word[2] for word in line)


def write_captions(folder, words, duration, hook=""):
    from unicodedata import east_asian_width

    srt = []
    ass = [
        "[Script Info]", "ScriptType: v4.00+", "PlayResX: 1080", "PlayResY: 1920",
        "", "[V4+ Styles]",
        "Format: Name,Fontname,Fontsize,PrimaryColour,SecondaryColour,OutlineColour,BackColour,Bold,Italic,Underline,StrikeOut,ScaleX,ScaleY,Spacing,Angle,BorderStyle,Outline,Shadow,Alignment,MarginL,MarginR,MarginV,Encoding",
        "Style: Default,Arial,76,&H00FFFFFF,&H00FFFFFF,&H00000000,&H60000000,-1,0,0,0,100,100,0,0,1,7,3,2,100,222,652,1",
        "Style: Hook,Arial,76,&H00FFFFFF,&H00FFFFFF,&H00000000,&H88100808,-1,0,0,0,100,100,0,0,3,10,0,2,105,185,315,1",
        "", "[Events]", "Format: Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,Effect,Text",
    ]

    def ass_text(text):
        return text.translate(str.maketrans({chr(92): "＼", "{": "｛", "}": "｝", "\n": " ", "\r": " "}))

    def width_units(text):
        return sum(1.05 if east_asian_width(char) in ("F", "W") else
                   .93 if char.isupper() or char.lower() in "mw" else
                   .29 if char in " ilI.,'’!:|" else .68 for char in text)

    def emit(start, end, parts, size, line_break, active=None):
        if round(start * 100) >= round(end * 100):
            return
        tokens = [
            r"{\1c&H00E8FF&}" + token + r"{\1c&HFFFFFF&}" if index == active else token
            for index, token in enumerate(parts)
        ]
        tags = r"{\q2\fs" + str(size) + "}"
        rendered = " ".join(tokens) if line_break is None else (
            " ".join(tokens[:line_break]) + chr(92) + "N" + " ".join(tokens[line_break:])
        )
        ass.append(f"Dialogue: 0,{timestamp(start, True)},{timestamp(end, True)},Default,,0,0,0,,{tags}{rendered}")

    if hook:
        ass.append(f"Dialogue: 1,0:00:00.00,{timestamp(min(1.65, duration), True)},Hook,,0,0,0,,{ass_text(hook)}")
    for index, line in enumerate(caption_lines(words, duration), start=1):
        parts = [ass_text(word[2]) for word in line]
        text = " ".join(parts)
        # Reserve the right side for the Shorts controls, including generous outline padding.
        units = width_units(text)
        line_break = None
        if len(parts) > 1 and units * 66 > 700:
            widths = [
                (max(width_units(" ".join(parts[:index])), width_units(" ".join(parts[index:]))), index)
                for index in range(1, len(parts))
            ]
            units, line_break = min(widths)
        size = max(1, min(76, int(700 / max(1, units))))
        srt.extend((str(index), f"{timestamp(line[0][0])} --> {timestamp(line[-1][1])}",
                    " ".join(word[2] for word in line), ""))
        cursor = line[0][0]
        for active, (start, end, _) in enumerate(line):
            emit(cursor, start, parts, size, line_break)
            emit(start, end, parts, size, line_break, active)
            cursor = end
    (folder / "captions.srt").write_text("\n".join(srt), encoding="utf-8")
    (folder / "captions.ass").write_text("\n".join(ass) + "\n", encoding="utf-8")


def load_model(cache, download=False):
    from faster_whisper import WhisperModel
    from huggingface_hub import snapshot_download
    model_directory = snapshot_download(
        MODEL, revision=MODEL_REVISION, cache_dir=str(cache), local_files_only=not download,
        allow_patterns=["config.json", "preprocessor_config.json", "model.bin", "tokenizer.json", "vocabulary.json"],
    )
    if download:
        digest = hashlib.sha256()
        with (Path(model_directory) / "model.bin").open("rb") as model_file:
            for block in iter(lambda: model_file.read(1024 * 1024), b""):
                digest.update(block)
        if digest.hexdigest() != MODEL_SHA256:
            raise RuntimeError("The Whisper weight file failed its SHA-256 integrity check.")
    return WhisperModel(
        model_directory, device="cpu", compute_type="int8_float32", cpu_threads=4,
    )


def transcribe(model, audio_path):
    options = dict(beam_size=5, word_timestamps=True, vad_filter=True, condition_on_previous_text=False)
    segments, _ = model.transcribe(str(audio_path), **options)
    output = []
    for segment in segments:
        selected = segment.words or []
        if selected and sum(word.probability for word in selected) / len(selected) < 0.62:
            alternative, _ = model.transcribe(
                str(audio_path), **{**options, "beam_size": 8},
                clip_timestamps=f"{segment.start},{segment.end}",
            )
            candidate = [word for part in alternative for word in (part.words or [])
                         if segment.start <= word.start < word.end <= segment.end + 0.04]
            if candidate and sum(word.probability for word in candidate) / len(candidate) > (
                sum(word.probability for word in selected) / len(selected) + 0.08
            ):
                selected = candidate
        for word in selected:
            start = max(word.start, output[-1]["end"] if output else 0)
            if word.end > start:
                output.append({
                    "text": word.word.strip() if word.probability >= 0.45 else "[unclear]",
                    "start": start, "end": word.end, "confidence": word.probability,
                })
    return output


def find_facecam(path, offset, duration, video):
    """Use a stable edge face across multiple frames before reserving a camera panel."""
    try:
        import cv2
    except ImportError:
        return None
    width, height = video["width"], video["height"]
    if width < height or duration < 6:
        return None
    capture = cv2.VideoCapture(str(path))
    import statistics
    # OpenCV Zoo YuNet (March 2023), distributed with its MIT license.
    model_path = Path(__file__).with_name("face_detection_yunet_2023mar.onnx")
    scan_width = min(width, 1920)
    scan_height = round(height * scan_width / width)
    tracks = []
    try:
        model = cv2.FaceDetectorYN.create(str(model_path), "", (scan_width, scan_height), 0.8)
        for fraction in (0.08, 0.22, 0.36, 0.5, 0.64, 0.78, 0.92):
            capture.set(cv2.CAP_PROP_POS_MSEC, (offset + fraction * duration) * 1000)
            ret, frame = capture.read()
            if not ret:
                continue
            frame = cv2.resize(frame, (scan_width, scan_height)) if width != scan_width else frame
            _, faces = model.detect(frame)
            claimed = set()
            for box in ([] if faces is None else faces):
                x, y, fw, fh = map(float, box[:4])
                cx, cy = (x + fw/2)/scan_width, (y + fh/2)/scan_height
                if 0.31 < cx < 0.69 or fw > scan_width * 0.26:
                    continue
                point = (cx, cy, fw/scan_width, fh/scan_height, float(box[-1]))
                candidates = []
                for index, track in enumerate(tracks):
                    if index in claimed:
                        continue
                    tx, ty, tw, th, _ = map(statistics.median, zip(*track))
                    distance = abs(cx-tx) + abs(cy-ty)
                    if distance <= max(0.11, tw) and 0.4 <= point[2]/tw <= 2.6:
                        candidates.append((distance, index))
                if candidates:
                    index = min(candidates)[1]
                    tracks[index].append(point)
                else:
                    index = len(tracks)
                    tracks.append([point])
                claimed.add(index)
    finally:
        capture.release()
    stable = [track for track in tracks if len(track) >= 4]
    if not stable:
        return None
    def support(track):
        return len(track) * statistics.median(p[4] for p in track) * statistics.median(p[2] for p in track)**0.8
    points = max(stable, key=support)
    cx, cy, fw, fh, _ = map(statistics.median, zip(*points))
    # Keep the whole face across its observed movement and include headroom.
    half_width = max(abs(p[0]-cx)*width + p[2]*width*0.75 for p in points)
    half_height = max(abs(p[1]-cy)*height + p[3]*height*0.8 for p in points)
    desired = max(width*fw*2.5, height*fh*3, half_width*2, half_height*3)
    crop_width = max(6, int(min(width, height*1.5, desired)) // 6 * 6)
    crop_height = crop_width * 2 // 3
    x = max(0, min(width - crop_width, round(cx * width - crop_width / 2))) // 2 * 2
    y = max(0, min(height - crop_height, round(cy * height - crop_height / 2))) // 2 * 2
    return crop_width, crop_height, x, y


def gameplay_crop(video, facecam):
    """Remove a camera confined entirely to a narrow outside edge."""
    width, height = video["width"], video["height"]
    camera_width, _, x, _ = facecam
    limit = width * 0.125
    left = x + camera_width
    right = width - x
    if 0 < left <= limit:
        return width - left, height, left, 0
    if 0 < right <= limit:
        return width - right, height, 0, 0
    return None


def safe_layout(video, facecam=None, emphasis=None):
    width, height = video["width"], video["height"]
    if abs(width / height - 9 / 16) < 0.015:
        return "[0:v]scale=1080:1920,setsar=1,ass=captions.ass[v]"
    foreground_height = 820 if facecam else 1920
    crop = gameplay_crop(video, facecam) if facecam else None
    gameplay = f"crop={crop[0]}:{crop[1]}:{crop[2]}:{crop[3]}," if crop else ""
    backdrop = (
        "[back]scale=270:480:force_original_aspect_ratio=increase,crop=270:480,"
        "gblur=sigma=18,eq=brightness=-0.19,scale=1080:1920[background];"
        f"[front]{gameplay}scale=1080:{foreground_height}:force_original_aspect_ratio=decrease:force_divisible_by=2,setsar=1[foreground];"
    )
    if facecam:
        width, height, x, y = facecam
        return (
            "[0:v]split=3[back][front][cam];" + backdrop
            + f"[cam]crop={width}:{height}:{x}:{y},scale=972:648,setsar=1"
            + (f",eq=brightness=0.035:contrast=1.05:enable='between(t,{emphasis[0]:.3f},{emphasis[1]:.3f})'" if emphasis else "")
            + "[camera];[background][foreground]overlay=(W-w)/2:810[main];"
            "[main][camera]overlay=54:125,setsar=1,ass=captions.ass[v]"
        )
    return (
        "[0:v]split[back][front];"
        + backdrop + "[background][foreground]overlay=(W-w)/2:(H-h)/2,setsar=1,ass=captions.ass[v]"
    )


def speech_overlaps_reaction(words, moment, release):
    return release is not None and any(
        word["confidence"] >= 0.80
        and word["text"] != "[unclear]"
        and any(letter.isalnum() for letter in word["text"])
        and min(word["end"], release + 0.10) > max(word["start"], moment - 0.10)
        for word in words
    )


def metadata(job, words, moment, release=None, *, duration=None, picture_ok=True):
    surge = release is not None
    corroborated = speech_overlaps_reaction(words, moment, release)
    creator = job["creator"]
    nearby = [w for w in words if moment-4 <= w["start"] <= moment+4]
    phrases = []
    phrase = []
    for w in nearby:
        if phrase and (w["start"] - phrase[-1]["end"] > 0.65 or len(phrase) >= 10):
            phrases.append(phrase)
            phrase = []
        if w["text"] == "[unclear]":
            if phrase:
                phrases.append(phrase)
                phrase = []
            continue
        phrase.append(w)
        if re.search(r"[.!?]$", w["text"]):
            phrases.append(phrase)
            phrase = []
    if phrase:
        phrases.append(phrase)
    clear = [p for p in phrases if len(p) >= 3 and sum(w["confidence"] for w in p)/len(p) >= 0.75]
    best = min(clear, key=lambda p: abs((p[0]["start"]+p[-1]["end"])/2-moment), default=[])
    excerpt = re.sub(r"[<>\x00-\x1f]", "", " ".join(w["text"] for w in best)).strip()
    title = (f'{creator}: "{excerpt}"' if excerpt else f"{creator}'s live reaction")[:100]
    creator_tag = re.sub(r"[^A-Za-z0-9]", "", creator)
    hashtags = ["#Shorts", "#Twitch"]
    if creator_tag and creator_tag.lower() not in {"shorts", "twitch"}:
        hashtags.append(f"#{creator_tag}")
    context = f"{job.get('stream_title') or ''} {excerpt}"
    for pattern, tag in ((r"\b(?:GTA|Grand Theft Auto)\b", "#GTA"),
                         (r"\bMinecraft\b", "#Minecraft"), (r"\bValorant\b", "#VALORANT"),
                         (r"\bFortnite\b", "#Fortnite"), (r"\bLeague of Legends\b", "#LeagueOfLegends")):
        if re.search(pattern, context, re.IGNORECASE) and tag.lower() != f"#{creator_tag}".lower():
            hashtags.append(tag)
            break
    if surge:
        hashtags.append("#Reaction")
    caption = f'"{excerpt}"' if excerpt else f"A reaction from {creator}'s stream."
    description = f"{caption}\n\nWatch {creator} live: https://www.twitch.tv/{creator}"
    description += "\n\n" + " ".join(hashtags)
    hook = f"“{excerpt}”" if corroborated and 10 <= len(excerpt) <= 45 and moment >= 1.0 else ""
    reasons = []
    if not corroborated:
        reasons.append("No clear speech during the reaction.")
    if not picture_ok:
        reasons.append("The opening or payoff is too dark, static, or unavailable.")
    if duration is not None:
        opening_captions = any(start <= 1.0 for start, _, _ in caption_groups(words, duration))
        if moment > 1.0 and not opening_captions and not hook:
            reasons.append("No clear opening context.")
        if release is not None and duration - release > 3.0:
            reasons.append("The edit runs too long after the reaction.")
        if moment > 6.0:
            reasons.append("The reaction takes too long to arrive.")
    return {"title": title, "description": description, "hashtags": hashtags,
            "quality_version": 2 if corroborated else 0, "opening_hook": hook,
            "editorial_pass": not reasons, "editorial_reasons": reasons,
            "reaction_start_seconds": round(moment, 3) if surge else None,
            "reaction_end_seconds": round(release, 3) if surge else None}


def usable_picture(path, offset, duration, reaction):
    """Check sampled opening and payoff frames for legible picture and activity."""
    import cv2
    capture = cv2.VideoCapture(str(path))
    try:
        for times in ((0, 0.35, 0.7), (reaction, min(duration - .15, reaction + .35), min(duration - .10, reaction + .7))):
            frames = []
            for second in times:
                capture.set(cv2.CAP_PROP_POS_MSEC, (offset + second) * 1000)
                ok, frame = capture.read()
                if not ok:
                    return False
                gray = cv2.cvtColor(cv2.resize(frame, (160, 90)), cv2.COLOR_BGR2GRAY)
                frames.append(gray)
            if max(frame.mean() for frame in frames) < 22:
                return False
            if max(cv2.absdiff(a, b).mean() for a, b in zip(frames, frames[1:])) < 1.8:
                return False
        return True
    finally:
        capture.release()


def select_edit(job, master, source_offset, available, words, evidence, start, end):
    """Repair a weak opening or long tail when the source offers a better window."""
    onset = evidence["peak"]
    release = evidence["release"] if evidence["surge"] else None

    def evaluate(left, right):
        duration = right - left
        excerpt = [
            dict(word, start=max(0, word["start"]-left), end=min(duration, word["end"]-left))
            for word in words if word["start"] >= left-0.001 and word["end"] <= right+0.001
        ]
        try:
            picture_ok = usable_picture(master, source_offset + left, duration, onset - left)
        except Exception as error:
            print(f"Editorial picture check failed: {error}", file=sys.stderr)
            picture_ok = False
        details = metadata(job, excerpt, onset-left, release-left if release is not None else None,
                           duration=duration, picture_ok=picture_ok)
        return left, right, excerpt, details

    best = evaluate(start, end)
    if best[3]["editorial_pass"] or not speech_overlaps_reaction(words, onset, release):
        return best

    minimum = job["min_seconds"]
    maximum = job["max_seconds"]
    preferred_end = min(available, release + 1.15)
    speech = [word for word in words if word["confidence"] >= .55 and word["text"] != "[unclear]"]
    edges = {start, max(0, onset-6), max(0, onset-1)}
    for word in speech:
        if start-1 <= word["start"] <= onset-0.65:
            edges.update((max(0, word["start"]-.10), word["start"], word["end"]))
    edges.update(max(0, start + step/4) for step in range(-4, 13))
    candidates = set()
    for left in edges:
        if not 0.65 <= onset-left <= 6 or any(word["start"] < left < word["end"] for word in speech):
            continue
        right = max(left + minimum, preferred_end)
        while any(word["start"] < right < word["end"] for word in speech):
            right = max(word["end"] + .08 for word in speech if word["start"] < right < word["end"])
        if right <= min(available, left + maximum, release+3) and (left, right) != (start, end):
            candidates.add((left, right))
    ordered = sorted(candidates, key=lambda pair: (
        abs(pair[0]-start) + abs(pair[1]-preferred_end) * 2, pair[0], pair[1],
    ))
    for left, right in ordered[:8]:
        candidate = evaluate(left, right)
        if candidate[3]["editorial_pass"]:
            return candidate
        if (candidate[3]["quality_version"] == 2
                and len(candidate[3]["editorial_reasons"]) < len(best[3]["editorial_reasons"])):
            best = candidate
    return best


def produce(job):
    folder = Path(job["folder"])
    folder.mkdir(parents=True, exist_ok=True)
    folder = folder.resolve()
    master = folder / "source.mkv"
    if not master.is_file():
        manifest = folder / "segments.ffconcat"
        segment_lines = ["ffconcat version 1.0"]
        for index, segment in enumerate(job["segments"]):
            source = Path(segment).resolve(strict=True)
            filename = f"input-{index:04}.ts"
            target = folder / filename
            target.unlink(missing_ok=True)
            try:
                os.link(source, target)
            except OSError:
                shutil.copyfile(source, target)
            segment_lines.append(f"file {filename}")
        manifest.write_text("\n".join(segment_lines) + "\n", encoding="ascii")
        temp_master = folder / "source.tmp.mkv"
        ffmpeg("-f", "concat", "-safe", "1", "-i", manifest, "-map", "0:v:0", "-map", "0:a:0",
               "-c", "copy", temp_master, cwd=folder)
        validate_video(temp_master)
        if temp_master.stat().st_size >= job.get("remaining_saved_bytes", float("inf")):
            temp_master.unlink()
            manifest.unlink()
            for filename in folder.glob("input-*.ts"):
                filename.unlink()
            raise BudgetExceeded("The remuxed master would exceed the saved clip budget.")
        os.replace(temp_master, master)
        manifest.unlink()
        for filename in folder.glob("input-*.ts"):
            filename.unlink()

    available, video = validate_video(master)
    offset = job["offset_ms"] / 1000
    max_duration = min(available - offset, 20 + job["max_seconds"])
    if max_duration < 1:
        raise RuntimeError("Recording did not cover the requested lead-in.")
    waveform = folder / "analysis.wav"
    ffmpeg("-ss", f"{offset:.3f}", "-i", master, "-t", f"{max_duration:.3f}",
           "-vn", "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le", waveform)
    words = transcribe(load_model(Path(job["model_cache"])), waveform)
    evidence = {}
    start, end = choose_window(waveform, 20, job["min_seconds"], job["max_seconds"], words, evidence=evidence)
    with wave.open(str(waveform), "rb") as audio:
        analysis_seconds = audio.getnframes() / audio.getframerate()
    start, end, words, details = select_edit(
        job, master, offset, analysis_seconds, words, evidence, start, end,
    )
    duration = end - start
    offset += start
    moment = evidence["peak"] - start
    release = evidence["release"] - start if evidence["surge"] else None
    write_captions(folder, words, duration, hook=details["opening_hook"])
    (folder / "transcript.json").write_text(json.dumps(words, ensure_ascii=False), encoding="utf-8")
    rate_fields = video["r_frame_rate"].split("/")
    rate = int(rate_fields[0]) / max(1, int(rate_fields[1]))
    fps = min(60, max(1, rate))
    temporary = folder / "short.tmp.mp4"
    try:
        facecam = find_facecam(master, offset, duration, video)
    except Exception as error:
        # Facecam detection is optional. Keep the full frame if vision fails.
        print(f"Facecam analysis failed: {error}", file=sys.stderr)
        facecam = None
    ffmpeg("-ss", f"{offset:.3f}", "-i", master, "-t", f"{duration:.3f}",
           "-filter_complex_threads", "2", "-filter_complex", safe_layout(video, facecam, (moment, release) if release is not None else None),
           "-map", "[v]", "-map", "0:a:0", "-c:v", "libx264", "-preset", "slow",
           "-crf", "17", "-pix_fmt", "yuv420p", "-r", f"{fps:.6f}", "-threads", "4",
           "-af", "loudnorm=I=-16:LRA=11:TP=-1.5,aresample=48000", "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", temporary, cwd=folder)
    duration, _ = validate_video(temporary, duration)
    combined_size = master.stat().st_size + temporary.stat().st_size
    if combined_size > job.get("remaining_saved_bytes", float("inf")):
        temporary.unlink()
        raise BudgetExceeded("The master and rendered export would exceed the saved clip budget.")
    final = folder / "short.mp4"
    os.replace(temporary, final)
    thumbnail = folder / "thumbnail.jpg"
    ffmpeg("-ss", f"{max(0, min(duration-0.25, evidence['peak']-start)):.3f}", "-i", final, "-frames:v", "1",
           "-vf", "scale=270:480", "-q:v", "3", thumbnail)
    waveform.unlink(missing_ok=True)
    return {
        "reaction_strength": evidence["strength"],
        "reaction_range_ms": [round(evidence[key] * 1000) for key in ("peak", "release")] if evidence["surge"] else None,
        "start_offset_ms": round(start * 1000),
        "duration_ms": round(duration * 1000),
        "saved_bytes": combined_size,
        "metadata": details,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=["setup", "produce"])
    parser.add_argument("path", type=Path)
    args = parser.parse_args()
    if args.action == "setup":
        load_model(args.path, download=True)
    else:
        job = json.loads(args.path.read_text(encoding="utf-8"))
        result = produce(job)
        result_path = Path(job["folder"]) / "result.json"
        temporary = result_path.with_suffix(".tmp")
        temporary.write_text(json.dumps(result, ensure_ascii=False), encoding="utf-8")
        os.replace(temporary, result_path)


if __name__ == "__main__":
    try:
        main()
    except BudgetExceeded as error:
        print(error, file=sys.stderr)
        sys.exit(75)
