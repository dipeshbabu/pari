def rolling_checksum(values, seed=17):
    total = seed
    for value in values:
        total = ((total << 5) - total) ^ value
    return total & 0xFFFFFFFF
