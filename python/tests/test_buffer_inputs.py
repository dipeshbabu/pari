from __future__ import annotations

import unittest
from array import array

from pari import DedupeIndex, MinHash, MinHash64


class BufferInputTests(unittest.TestCase):
    def test_buffer_inputs_hash_their_raw_bytes(self) -> None:
        buffers = [
            bytearray(b"abc"),
            memoryview(b"abc"),
            memoryview(array("H", [1, 2, 3])),
            memoryview(array("H", [256, 257, 258])),
            memoryview(array("I", [1, 2, 3, 4]))[::2],
            memoryview(array("H", [1, 2, 3]))[::-1],
            memoryview(array("b", [-1, 2, -3])),
            memoryview(bytearray(range(6))).cast("B", shape=[2, 3]),
            array("H", [1, 2, 3]),
        ]
        for sketch_type in (MinHash, MinHash64):
            for position, buffer in enumerate(buffers):
                with self.subTest(sketch=sketch_type.__name__, buffer=position):
                    raw = memoryview(buffer).tobytes()
                    expected = sketch_type.from_values([raw], num_perm=32, seed=7)
                    scalar = sketch_type(num_perm=32, seed=7)
                    scalar.update(buffer)
                    self.assertEqual(scalar.signature, expected.signature)
                    bulk = sketch_type(num_perm=32, seed=7)
                    bulk.update_many([buffer])
                    self.assertEqual(bulk.signature, expected.signature)
                    self.assertEqual(
                        sketch_type.from_values(
                            [buffer], num_perm=32, seed=7
                        ).signature,
                        expected.signature,
                    )
                    for threads in (1, 2):
                        actual = sketch_type.from_batch(
                            [[buffer]] * 256, num_perm=32, seed=7, threads=threads
                        )
                        self.assertTrue(
                            all(
                                value.signature == expected.signature
                                for value in actual
                            )
                        )

    def test_dedupe_matches_buffers_with_the_equivalent_bytes(self) -> None:
        buffer = memoryview(array("H", [1, 2, 3]))
        with DedupeIndex(num_perm=32, seed=7) as index:
            index.add_many_features([("view", [buffer]), ("bytes", [buffer.tobytes()])])
            self.assertEqual(index.candidate_pairs(), (("view", "bytes"),))
            self.assertEqual(index.result().dropped_indices, (1,))

    def test_integer_sequences_keep_existing_byte_value_behavior(self) -> None:
        for sketch_type in (MinHash, MinHash64):
            for values in ([1, 2, 255], (1, 2, 255)):
                with self.subTest(sketch=sketch_type.__name__, values=values):
                    self.assertEqual(
                        sketch_type.from_values([values]).signature,
                        sketch_type.from_values([bytes(values)]).signature,
                    )

    def test_batch_rows_of_any_iterable_shape_match_from_values(self) -> None:
        class OverriddenList(list):
            # A list subclass is iterated, not read in place.
            def __iter__(self):
                return iter([b"override"])

        def rows() -> list[tuple[object, list[object]]]:
            # (row passed to from_batch, the same features as a list)
            return [
                ([b"a", b"bb", b""], [b"a", b"bb", b""]),
                ((b"a", b"bb", b""), [b"a", b"bb", b""]),
                (iter([b"x", bytearray(b"y")]), [b"x", b"y"]),
                ([], []),
                (OverriddenList([b"ignored"]), [b"override"]),
                ([[1, 2, 255], memoryview(b"z")], [bytes([1, 2, 255]), b"z"]),
            ]

        for sketch_type in (MinHash, MinHash64):
            for threads in (1, 2):
                with self.subTest(sketch=sketch_type.__name__, threads=threads):
                    batch_rows: list[object] = []
                    expected: list[list[object]] = []
                    # Enough rows for the parallel batch path.
                    for _ in range(64):
                        for row, features in rows():
                            batch_rows.append(row)
                            expected.append(features)
                    actual = sketch_type.from_batch(
                        batch_rows, num_perm=32, seed=7, threads=threads
                    )
                    self.assertEqual(
                        [sketch.signature for sketch in actual],
                        [
                            sketch_type.from_values(
                                features, num_perm=32, seed=7
                            ).signature
                            for features in expected
                        ],
                    )
                    with self.assertRaises(TypeError):
                        sketch_type.from_batch([[b"a", object(), b"b"]])

    def test_released_views_and_non_buffer_values_fail(self) -> None:
        released = memoryview(b"abc")
        released.release()
        for sketch_type in (MinHash, MinHash64):
            with self.subTest(sketch=sketch_type.__name__):
                with self.assertRaises(ValueError):
                    sketch_type.from_values([released])
                with self.assertRaises(TypeError):
                    sketch_type.from_values([object()])


if __name__ == "__main__":
    unittest.main()
