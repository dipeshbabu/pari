from __future__ import annotations

import unittest

from pari import ConfigurationError, MinHash, deduplicate, shingles


class ShingleTests(unittest.TestCase):
    def test_word_shingles_normalize_case_punctuation_and_width(self) -> None:
        self.assertEqual(
            shingles("The quick, brown FOX jumps", 2),
            [b"the\x1fquick", b"quick\x1fbrown", b"brown\x1ffox", b"fox\x1fjumps"],
        )
        self.assertEqual(
            shingles("The quick brown fox", 2),
            shingles("ＴＨＥ   quick\nbrown  fox!", 2),
        )

    def test_short_empty_and_duplicate_inputs(self) -> None:
        self.assertEqual(shingles("one two", 3), [b"one\x1ftwo"])
        self.assertEqual(shingles("", 3), [])
        self.assertEqual(shingles("  ...  ", 3), [])
        self.assertEqual(shingles("a b a b a b", 2), [b"a\x1fb", b"b\x1fa"])

    def test_character_shingles(self) -> None:
        self.assertEqual(shingles("Abcd", 3, unit="char"), [b"abc", b"bcd"])
        self.assertEqual(shingles("a  b", 3, unit="char"), [b"a b"])
        self.assertEqual(shingles("hi", 3, unit="char"), [b"hi"])
        self.assertEqual(shingles("   ", 3, unit="char"), [])
        self.assertEqual(
            shingles("Hello World", 4, unit="char", normalize=False)[0], b"Hell"
        )

    def test_without_normalization_words_split_on_whitespace(self) -> None:
        self.assertEqual(
            shingles("Hello, World", 1, normalize=False), [b"Hello,", b"World"]
        )

    def test_unicode_is_encoded_as_utf8(self) -> None:
        self.assertEqual(shingles("Café crème", 1), ["café".encode(), "crème".encode()])

    def test_invalid_arguments(self) -> None:
        for size in (0, -1, 1.5, True):
            with self.subTest(size=size), self.assertRaises(ConfigurationError):
                shingles("text", size)  # type: ignore[arg-type]
        with self.assertRaises(ConfigurationError):
            shingles("text", unit="line")  # type: ignore[arg-type]
        with self.assertRaises(TypeError):
            shingles(b"text")  # type: ignore[arg-type]

    def test_feeds_minhash_and_deduplicate(self) -> None:
        first = "Pari finds similar records and deduplicates datasets quickly"
        second = "PARI finds similar records, and deduplicates datasets quickly!"
        left = MinHash.from_values(shingles(first))
        right = MinHash.from_values(shingles(second))
        self.assertEqual(left.jaccard(right), 1.0)
        result = deduplicate(
            [first, second, "something else entirely"],
            feature=lambda text: shingles(text, 3),
        )
        self.assertEqual(result.dropped_indices, (1,))


if __name__ == "__main__":
    unittest.main()
