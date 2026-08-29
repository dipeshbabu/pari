from typing import Any, Protocol

from . import MinHash

class DatasketchMinHash(Protocol):
    hashvalues: Any
    num_perm: int
    permutations: Any
    scheme: str
    seed: int

def datasketch_version() -> str: ...
def to_datasketch(sketch: MinHash) -> DatasketchMinHash: ...
def from_datasketch(sketch: DatasketchMinHash) -> MinHash: ...
def is_compatible(sketch: object) -> bool: ...
