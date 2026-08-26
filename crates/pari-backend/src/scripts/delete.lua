if redis.call('EXISTS', KEYS[1]) == 0 then
  return -1
end
local ttl = tonumber(ARGV[1])
local blobs = {}
for position = 2, #ARGV do
  local blob = redis.call('HGET', KEYS[2], ARGV[position])
  if blob and (#blob % 20) ~= 0 then
    return -2
  end
  blobs[position] = blob
end
local removed = 0
for position = 2, #ARGV do
  local blob = blobs[position]
  if blob then
    redis.call('HDEL', KEYS[2], ARGV[position])
    for offset = 1, #blob, 20 do
      redis.call('ZREM', KEYS[3], string.sub(blob, offset, offset + 19))
    end
    removed = removed + 1
  end
end
if ttl > 0 and removed > 0 then
  redis.call('EXPIRE', KEYS[1], ttl)
  redis.call('EXPIRE', KEYS[2], ttl)
  redis.call('EXPIRE', KEYS[3], ttl)
end
return removed
