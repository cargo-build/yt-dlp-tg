#!/usr/bin/env python3
"""On-demand CPU OCR helper for authorized photo cards (DL-12).

Contract (SPEC.md DL-12): one JSON object on stdout with a
required nonblank ``query`` (<= 240 chars) and optional ``lines`` (<= 32
records). Exit 0 success, 1 internal/model/config, 2 empty/no usable text,
3 malformed/unsupported image, 4 too large (bytes/pixels). Every failure
prints only the fixed marker on stderr; no paths or raw OCR text leak.
Stdlib-only at import time so pure query tests need no native OCR stack.
"""

import argparse
import contextlib
import hashlib
import json
import math
import os
from pathlib import Path
import sys

EXIT_INTERNAL = 1
EXIT_EMPTY = 2
EXIT_IMAGE = 3
EXIT_TOO_LARGE = 4

SAFE_MARKER = "ocr: failed"

QUERY_MAX_CHARS = 240
LINES_MAX = 32
LINE_TEXT_MAX_CHARS = 120

SUPPORTED_FORMATS = frozenset({"PNG", "JPEG", "WEBP"})

# RapidOCR 3.10.0 bundled defaults (v6 small det/rec + mobile cls), pinned to
# the SHA256 values recorded by the 2026-10-29 probe. Explicit paths keep the
# engine on local files only; a missing or mismatched model fails closed.
RAPIDOCR_MODELS = {
    "det": ("PP-OCRv6_det_small.onnx",
            "090f04abcd9d9a7498bc4ebf677e4cb9bdce1fe4197ddb7e529f1ef44e1ff94f"),
    "cls": ("ch_ppocr_mobile_v2.0_cls_mobile.onnx",
            "e47acedf663230f8863ff1ab0e64dd2d82b838fceb5957146dab185a89d6215c"),
    "rec": ("PP-OCRv6_rec_small.onnx",
            "6f327246b50388f3c176ae304bd95767ea6dc0c9ae92153ef8cbe210b3c14884"),
}

# Two boxes share a row when their vertical ranges overlap at least half of
# the shorter box and their heights are within 2x, so side-by-side words of
# one text row merge while tiny cover credits never join a main row.
ROW_VOVERLAP_RATIO = 0.5
ROW_HEIGHT_RATIO = 0.5

# Rows far shorter than the tallest row are cover credits, not title/artist.
TINY_ROW_RATIO = 0.5


class OcrFailure(Exception):
    """Process failure carrying its pinned exit code."""

    def __init__(self, exit_code):
        super().__init__(exit_code)
        self.exit_code = exit_code


def normalize_text(text):
    """Collapse whitespace and drop unencodable surrogates; keep spelling."""
    if not isinstance(text, str):
        return ""
    cleaned = " ".join(text.split())
    return cleaned.encode("utf-8", "ignore").decode("utf-8")


def _box_metrics(box):
    try:
        corners = list(box[:4])
    except (TypeError, ValueError, IndexError, KeyError):
        return None
    if len(corners) != 4:
        return None
    points = []
    for point in corners:
        try:
            x = float(point[0])
            y = float(point[1])
        except (TypeError, ValueError, IndexError, KeyError):
            return None
        if not math.isfinite(x) or not math.isfinite(y):
            return None
        points.append((x, y))
    xs = [p[0] for p in points]
    ys = [p[1] for p in points]
    x0, y0, x1, y1 = min(xs), min(ys), max(xs), max(ys)
    if x1 - x0 <= 0 or y1 - y0 <= 0:
        return None
    return x0, y0, x1, y1


def _same_row(a, b):
    overlap = min(a[3], b[3]) - max(a[1], b[1])
    shorter_h = min(a[3] - a[1], b[3] - b[1])
    taller_h = max(a[3] - a[1], b[3] - b[1])
    return (overlap >= ROW_VOVERLAP_RATIO * shorter_h
            and shorter_h >= ROW_HEIGHT_RATIO * taller_h)


def group_rows(lines):
    """Group recognized lines into text rows; returns [] when none survive.

    Each row is a dict with joined ``text`` (left-to-right), box ``height``,
    ``y_center`` and horizontal ``span``. Lines with invalid boxes or empty
    text are skipped; geometry is required, never invented.
    """
    entries = []
    for line in lines:
        metrics = _box_metrics(line.get("box"))
        text = normalize_text(line.get("text", ""))
        if metrics is None or not text:
            continue
        x0, y0, x1, y1 = metrics
        entries.append({
            "box": metrics,
            "x0": x0, "x1": x1, "y0": y0, "y1": y1,
            "y_center": (y0 + y1) / 2,
            "height": y1 - y0,
            "text": text,
        })
    if not entries:
        return []

    parent = list(range(len(entries)))

    def find(i):
        while parent[i] != i:
            parent[i] = parent[parent[i]]
            i = parent[i]
        return i

    for i in range(len(entries)):
        for j in range(i + 1, len(entries)):
            if find(i) != find(j) and _same_row(entries[i]["box"], entries[j]["box"]):
                parent[find(j)] = find(i)

    clusters = {}
    for idx, entry in enumerate(entries):
        clusters.setdefault(find(idx), []).append(entry)

    rows = []
    for members in clusters.values():
        members.sort(key=lambda e: e["x0"])
        rows.append({
            "text": " ".join(m["text"] for m in members),
            "height": max(m["height"] for m in members),
            "y_center": sum(m["y_center"] for m in members) / len(members),
            "span": max(m["x1"] for m in members) - min(m["x0"] for m in members),
        })
    return rows


def select_query(lines):
    """Pick a short pair of dominant rows; None when nothing usable remains.

    Generic geometry only: reject rows far shorter than the tallest row
    (cover credits), drop exact duplicate rows (keep the taller instance),
    take the two dominant rows and read them bottom row first, matching the
    artist-then-title convention of these cards.
    """
    rows = [row for row in group_rows(lines) if row["text"]]
    if not rows:
        return None

    tallest = max(row["height"] for row in rows)
    rows = [row for row in rows if row["height"] >= TINY_ROW_RATIO * tallest]
    if not rows:
        return None

    deduped = {}
    for row in rows:
        kept = deduped.get(row["text"])
        if kept is None or row["height"] > kept["height"]:
            deduped[row["text"]] = row
    rows = sorted(deduped.values(),
                  key=lambda r: (-r["height"], -r["span"], r["y_center"]))

    chosen = sorted(rows[:2], key=lambda r: r["y_center"], reverse=True)
    joined = " ".join(row["text"] for row in chosen)
    query = normalize_text(joined.replace("&", " & "))[:QUERY_MAX_CHARS].strip()
    if not query:
        return None
    return query


def clip_lines(lines):
    """Bound the optional preview records: 32 rows, short texts, plain scores."""
    clipped = []
    for line in lines[:LINES_MAX]:
        text = normalize_text(line.get("text", ""))[:LINE_TEXT_MAX_CHARS]
        if not text:
            continue
        try:
            score = round(float(line.get("score", 0.0)), 4)
        except (TypeError, ValueError):
            score = 0.0
        if not math.isfinite(score):
            score = 0.0
        clipped.append({"text": text, "score": score})
    return clipped


def inspect_image(path, max_bytes, max_pixels):
    """Validate the input from bytes and header before any model loads."""
    target = Path(path)
    try:
        size = target.stat().st_size
    except OSError:
        raise OcrFailure(EXIT_IMAGE) from None
    if size == 0:
        raise OcrFailure(EXIT_IMAGE)
    if size > max_bytes:
        raise OcrFailure(EXIT_TOO_LARGE)

    from PIL import Image, UnidentifiedImageError

    # The mandatory max_pixels cap below replaces PIL's decompression-bomb
    # guard, whose warning/error on huge declared headers would noise stderr
    # or misclassify an oversized image as malformed.
    Image.MAX_IMAGE_PIXELS = None

    try:
        with Image.open(target) as img:
            if img.format not in SUPPORTED_FORMATS:
                raise OcrFailure(EXIT_IMAGE)
            width, height = img.size
            if width <= 0 or height <= 0:
                raise OcrFailure(EXIT_IMAGE)
            if width * height > max_pixels:
                raise OcrFailure(EXIT_TOO_LARGE)
            img.load()
    except OcrFailure:
        raise
    except (UnidentifiedImageError, OSError, ValueError):
        raise OcrFailure(EXIT_IMAGE) from None


@contextlib.contextmanager
def _stdout_to_stderr():
    """Keep fd 1 clean: C-level library output lands on stderr during OCR."""
    saved = os.dup(1)
    try:
        os.dup2(2, 1)
        yield
    finally:
        os.dup2(saved, 1)
        os.close(saved)


def _resolve_models():
    import rapidocr

    model_dir = Path(rapidocr.__file__).resolve().parent / "models"
    resolved = {}
    for stage, (name, expected_sha) in RAPIDOCR_MODELS.items():
        model_path = model_dir / name
        try:
            digest = hashlib.sha256(model_path.read_bytes()).hexdigest()
        except OSError:
            raise OcrFailure(EXIT_INTERNAL) from None
        if digest != expected_sha:
            raise OcrFailure(EXIT_INTERNAL)
        resolved[stage] = str(model_path)
    return resolved


def run_ocr(path):
    """Recognize one image on CPU; returns raw line records (may be empty)."""
    with _stdout_to_stderr():
        import cv2
        from rapidocr import RapidOCR

        cv2.setNumThreads(1)
        models = _resolve_models()
        params = {
            "Global.log_level": "error",
            "Det.model_path": models["det"],
            "Cls.model_path": models["cls"],
            "Rec.model_path": models["rec"],
            "EngineConfig.onnxruntime.intra_op_num_threads": 1,
            "EngineConfig.onnxruntime.inter_op_num_threads": 1,
            "EngineConfig.onnxruntime.use_cuda": False,
            "EngineConfig.onnxruntime.use_dml": False,
            "EngineConfig.onnxruntime.use_cann": False,
            "EngineConfig.onnxruntime.use_coreml": False,
        }
        engine = RapidOCR(params=params)
        result = engine(str(path))

        lines = []
        boxes = getattr(result, "boxes", None)
        if boxes is None:
            boxes = []
        txts = getattr(result, "txts", None) or ()
        scores = getattr(result, "scores", None) or ()
        for box, text, score in zip(boxes, txts, scores):
            lines.append({"box": box, "text": text, "score": score})
        if not any(normalize_text(line["text"]) for line in lines):
            return lines

        for stage_name in ("text_det", "text_cls", "text_rec"):
            stage = getattr(engine, stage_name, None)
            session = getattr(getattr(stage, "session", None), "session", None)
            providers = session.get_providers() if session is not None else []
            if providers != ["CPUExecutionProvider"]:
                raise OcrFailure(EXIT_INTERNAL)
        return lines


def run(path, max_bytes, max_pixels):
    inspect_image(path, max_bytes, max_pixels)
    try:
        lines = run_ocr(path)
    except OcrFailure:
        raise
    except Exception:
        raise OcrFailure(EXIT_INTERNAL) from None

    query = select_query(lines)
    if query is None:
        raise OcrFailure(EXIT_EMPTY)
    return {"query": query, "lines": clip_lines(lines)}


def _positive_int(value):
    try:
        number = int(value)
    except (TypeError, ValueError):
        raise argparse.ArgumentTypeError("not an integer") from None
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


class _Parser(argparse.ArgumentParser):
    """Bad CLI usage is a config failure (exit 1), not argparse's exit 2."""

    def error(self, message):
        print(SAFE_MARKER, file=sys.stderr)
        raise SystemExit(EXIT_INTERNAL)


def parse_args(argv):
    parser = _Parser(add_help=False)
    parser.add_argument("--image", required=True)
    parser.add_argument("--max-bytes", type=_positive_int, required=True)
    parser.add_argument("--max-pixels", type=_positive_int, required=True)
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    try:
        payload = run(args.image, args.max_bytes, args.max_pixels)
    except OcrFailure as failure:
        print(SAFE_MARKER, file=sys.stderr)
        raise SystemExit(failure.exit_code) from None
    except Exception:
        print(SAFE_MARKER, file=sys.stderr)
        raise SystemExit(EXIT_INTERNAL) from None
    print(json.dumps(payload, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
