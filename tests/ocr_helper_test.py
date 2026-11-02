"""Focused tests for scripts/ocr.py; pure cases need no native OCR stack."""

import contextlib
import importlib.util
import io
import struct
import sys
import tempfile
import unittest
import zlib
from pathlib import Path
from unittest import mock

MODULE_PATH = Path(__file__).resolve().parents[1] / "scripts" / "ocr.py"
SPEC = importlib.util.spec_from_file_location("ocr_helper", MODULE_PATH)
ocr = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ocr)

try:
    from PIL import Image
    HAVE_PIL = True
except ImportError:
    HAVE_PIL = False


def line(text, x, y, w, h, score=0.99):
    """RapidOCR-shaped record: 4-point box as [[x, y], ...]."""
    box = [[x, y], [x + w, y], [x + w, y + h], [x, y + h]]
    return {"box": box, "text": text, "score": score}


class SelectQueryTest(unittest.TestCase):
    def test_incomplete_geometry_and_nontext_are_not_invented(self):
        bad = [{"box": [], "text": "EMPTY"},
               {"box": [[0, 0], [10, 10]], "text": "PARTIAL"},
               line(None, 1, 1, 10, 10)]
        self.assertIsNone(ocr.select_query(bad))

    def test_nonfinite_scores_stay_valid_json(self):
        import json
        payload = ocr.clip_lines([line("TITLE", 1, 1, 20, 10, float("nan"))])
        json.dumps(payload, allow_nan=False)

    def test_unexpected_failure_has_only_safe_marker(self):
        with mock.patch.object(ocr, "run", side_effect=RuntimeError("private-input-path")):
            output = io.StringIO()
            with contextlib.redirect_stderr(output), self.assertRaises(SystemExit) as caught:
                ocr.main(["--image", "/private", "--max-bytes", "100", "--max-pixels", "100"])
        self.assertEqual(caught.exception.code, 1)
        self.assertEqual(output.getvalue(), "ocr: failed\n")

    def test_split_title_words_group_and_order(self):
        lines = [
            line("SWORD", 152, 30, 130, 41),
            line("ART", 287, 30, 81, 40),
            line("ONCINE", 390, 30, 140, 41),
            line("SONICSPECTER", 150, 73, 190, 30),
        ]
        self.assertEqual(ocr.select_query(lines), "SONICSPECTER SWORD ART ONCINE")

    def test_bottom_row_comes_first(self):
        lines = [
            line("GRADIENT", 144, 20, 194, 48),
            line("YKU", 146, 66, 51, 32),
        ]
        self.assertEqual(ocr.select_query(lines), "YKU GRADIENT")

    def test_tiny_cover_credits_rejected(self):
        lines = [
            line("FORHILL", 58, 20, 34, 8),
            line("ATLAS", 146, 26, 117, 43),
            line("FORHILL&JNFOUND", 148, 70, 273, 30),
            line("FIGMENTS", 57, 130, 36, 7),
        ]
        query = ocr.select_query(lines)
        self.assertEqual(query, "FORHILL & JNFOUND ATLAS")
        self.assertNotIn("FIGMENTS", query)

    def test_tiny_row_never_merges_into_main_row(self):
        lines = [
            line("SHADOWDREAM", 146, 22, 268, 44),
            line("SHADOWDREAM", 14, 34, 117, 14),
            line("BEOVOC", 146, 68, 97, 28),
        ]
        self.assertEqual(ocr.select_query(lines), "BEOVOC SHADOWDREAM")

    def test_exact_duplicate_rows_suppressed(self):
        lines = [
            line("WHITE", 142, 24, 126, 48),
            line("SHADES", 281, 23, 154, 51),
            line("SKYKOT", 146, 71, 99, 30),
            line("SKYKOT", 150, 110, 99, 28),
        ]
        query = ocr.select_query(lines)
        self.assertEqual(query, "SKYKOT WHITE SHADES")

    def test_no_text_returns_none(self):
        self.assertIsNone(ocr.select_query([]))
        self.assertIsNone(ocr.select_query([{"box": None, "text": "", "score": 1.0}]))

    def test_invalid_boxes_skipped(self):
        bad = float("nan")
        lines = [
            {"box": [[bad, bad], [bad, bad], [bad, bad], [bad, bad]],
             "text": "JUNK", "score": 1.0},
            {"box": [[0, 0], [10, 0], [10, 0], [0, 0]], "text": "FLAT", "score": 1.0},
            {"box": "not-a-box", "text": "JUNK", "score": 1.0},
            line("SOLO", 10, 10, 40, 20),
        ]
        self.assertEqual(ocr.select_query(lines), "SOLO")
        only_bad = lines[:3]
        self.assertIsNone(ocr.select_query(only_bad))

    def test_query_bounded_to_240_chars(self):
        rows = [line("A" * 400, 10, 10, 400, 40),
                line("B" * 400, 10, 60, 400, 30)]
        query = ocr.select_query(rows)
        self.assertEqual(len(query), ocr.QUERY_MAX_CHARS)
        self.assertTrue(query.startswith("B"))

    def test_only_one_row_survives(self):
        self.assertEqual(ocr.select_query([line("PRISM", 157, 34, 120, 45)]), "PRISM")

    def test_all_rows_tiny_still_usable(self):
        # No tall row to dominate: a single small row remains selectable.
        self.assertEqual(ocr.select_query([line("CREDIT", 5, 5, 30, 8)]), "CREDIT")

    def test_tuple_and_nested_sequence_boxes_accepted(self):
        lines = [{"box": ((10, 10), (50, 10), (50, 30), (10, 30)),
                  "text": "TUPLED", "score": 0.9}]
        self.assertEqual(ocr.select_query(lines), "TUPLED")

    def test_spelling_preserved_whitespace_normalized(self):
        lines = [line("  HOTEL\tPOOLS ", 146, 73, 176, 28),
                 line("ULTRAVIOLET", 144, 26, 255, 46)]
        self.assertEqual(ocr.select_query(lines), "HOTEL POOLS ULTRAVIOLET")

    def test_surrogate_code_points_dropped(self):
        text = "BAD\ud800ROW"
        lines = [line(text, 10, 10, 40, 40)]
        self.assertEqual(ocr.select_query(lines), "BADROW")


class ClipLinesTest(unittest.TestCase):
    def test_caps_records_and_text(self):
        lines = [line(f"T{i}", 0, 0, 10, 10) for i in range(40)]
        lines.append({"box": [[0, 0], [1, 0], [1, 1], [0, 1]], "text": "", "score": 1.0})
        clipped = ocr.clip_lines(lines)
        self.assertEqual(len(clipped), ocr.LINES_MAX)
        self.assertEqual([r["text"] for r in clipped[:3]], ["T0", "T1", "T2"])
        self.assertEqual(
            ocr.clip_lines([line("X" * 500, 0, 0, 10, 10)])[0]["text"],
            "X" * ocr.LINE_TEXT_MAX_CHARS,
        )

    def test_score_rounding_and_default(self):
        self.assertEqual(ocr.clip_lines([line("A", 0, 0, 1, 1, 0.9876543)])[0]["score"],
                         0.9877)
        self.assertEqual(ocr.clip_lines([{"text": "A", "score": None}])[0]["score"], 0.0)


class RunTest(unittest.TestCase):
    def test_empty_recognition_maps_to_empty_exit(self):
        with mock.patch.object(ocr, "inspect_image"), \
                mock.patch.object(ocr, "run_ocr", return_value=[]):
            with self.assertRaises(ocr.OcrFailure) as ctx:
                ocr.run("/x.png", 10, 10)
        self.assertEqual(ctx.exception.exit_code, ocr.EXIT_EMPTY)

    def test_payload_shape(self):
        raw = [line("KORIZON", 158, 80, 111, 28),
               line("PRISM", 157, 34, 120, 45)]
        with mock.patch.object(ocr, "inspect_image"), \
                mock.patch.object(ocr, "run_ocr", return_value=raw):
            payload = ocr.run("/x.png", 10, 10)
        self.assertEqual(payload["query"], "KORIZON PRISM")
        self.assertEqual(payload["lines"][0], {"text": "KORIZON", "score": 0.99})


class ParseArgsTest(unittest.TestCase):
    VALID = ["--image", "/abs/x.png", "--max-bytes", "10485760",
             "--max-pixels", "16000000"]

    def test_valid(self):
        args = ocr.parse_args(self.VALID)
        self.assertEqual((args.image, args.max_bytes, args.max_pixels),
                         ("/abs/x.png", 10485760, 16000000))

    def test_missing_flag_is_config_failure(self):
        for partial in (self.VALID[:2], self.VALID[:-2], []):
            with self.assertRaises(SystemExit) as ctx, \
                    contextlib.redirect_stderr(io.StringIO()) as err:
                ocr.parse_args(partial)
            self.assertEqual(ctx.exception.code, ocr.EXIT_INTERNAL)
            self.assertEqual(err.getvalue().strip(), ocr.SAFE_MARKER)

    def test_non_positive_limit_is_config_failure(self):
        for bad in ("0", "-5", "abc"):
            argv = self.VALID[:-1] + [bad]
            with self.assertRaises(SystemExit) as ctx:
                ocr.parse_args(argv)
            self.assertEqual(ctx.exception.code, ocr.EXIT_INTERNAL)

    def test_main_failure_stderr_marker(self):
        with self.assertRaises(SystemExit) as ctx, \
                contextlib.redirect_stderr(io.StringIO()) as err:
            ocr.main(["--image", "/abs/missing.png",
                      "--max-bytes", "10", "--max-pixels", "10"])
        self.assertEqual(ctx.exception.code, ocr.EXIT_IMAGE)
        self.assertEqual(err.getvalue().strip(), ocr.SAFE_MARKER)


@unittest.skipUnless(HAVE_PIL, "Pillow not available in this environment")
class InspectImageTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.max_bytes = 10485760
        self.max_pixels = 16000000

    def _png(self, width, height, with_raster):
        header = struct.pack(">IIBBBBB", width, height, 8, 0, 0, 0, 0)

        def chunk(kind, data):
            crc = zlib.crc32(kind + data) & 0xFFFFFFFF
            return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", crc)

        blob = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
        if with_raster:
            raw = b"\x00" * width
            blob += chunk(b"IDAT", zlib.compress(raw))
        blob += chunk(b"IEND", b"")
        return blob

    def test_valid_png_passes(self):
        target = self.root / "ok.png"
        Image.new("RGB", (8, 6), "white").save(target, format="PNG")
        ocr.inspect_image(target, self.max_bytes, self.max_pixels)

    def test_oversized_pixels_rejected_from_header(self):
        target = self.root / "huge.png"
        target.write_bytes(self._png(5000, 4000, with_raster=False))
        with self.assertRaises(ocr.OcrFailure) as ctx:
            ocr.inspect_image(target, self.max_bytes, self.max_pixels)
        self.assertEqual(ctx.exception.exit_code, ocr.EXIT_TOO_LARGE)

    def test_oversized_bytes_rejected_before_parse(self):
        target = self.root / "big.bin"
        target.write_bytes(b"x" * 32)
        with self.assertRaises(ocr.OcrFailure) as ctx:
            ocr.inspect_image(target, 16, self.max_pixels)
        self.assertEqual(ctx.exception.exit_code, ocr.EXIT_TOO_LARGE)

    def test_empty_file_rejected(self):
        target = self.root / "empty.png"
        target.write_bytes(b"")
        with self.assertRaises(ocr.OcrFailure) as ctx:
            ocr.inspect_image(target, self.max_bytes, self.max_pixels)
        self.assertEqual(ctx.exception.exit_code, ocr.EXIT_IMAGE)

    def test_garbage_rejected(self):
        target = self.root / "garbage.png"
        target.write_bytes(b"not an image at all")
        with self.assertRaises(ocr.OcrFailure) as ctx:
            ocr.inspect_image(target, self.max_bytes, self.max_pixels)
        self.assertEqual(ctx.exception.exit_code, ocr.EXIT_IMAGE)

    def test_unsupported_format_rejected(self):
        target = self.root / "tiny.gif"
        target.write_bytes(b"GIF87a" + struct.pack("<HHB", 10, 10, 0) + b"\x00")
        with self.assertRaises(ocr.OcrFailure) as ctx:
            ocr.inspect_image(target, self.max_bytes, self.max_pixels)
        self.assertEqual(ctx.exception.exit_code, ocr.EXIT_IMAGE)

    def test_truncated_png_rejected(self):
        target = self.root / "truncated.png"
        target.write_bytes(self._png(4, 3, with_raster=False))
        with self.assertRaises(ocr.OcrFailure) as ctx:
            ocr.inspect_image(target, self.max_bytes, self.max_pixels)
        self.assertEqual(ctx.exception.exit_code, ocr.EXIT_IMAGE)


if __name__ == "__main__":
    unittest.main()
