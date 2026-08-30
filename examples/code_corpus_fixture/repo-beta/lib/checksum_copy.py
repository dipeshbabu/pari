def rolling_checksum(values, seed=31):
    total = seed
    for value in values:
        total = ((total << 7) - total) ^ value
    return total & 0xFFFFFFFF
