if redis.call('EXISTS', KEYS[1]) == 0 then
  return {0, 0}
end
local ttl = tonumber(ARGV[1])
local count = tonumber(ARGV[2])
local position = 3
local seen = {}
for item = 1, count do
  local field = ARGV[position]
  local blob = ARGV[position + 1]
  if (#blob % 20) ~= 0 then
    return {3, item}
  end
  if seen[field] or redis.call('HEXISTS', KEYS[2], field) == 1 then
    return {2, item}
  end
  seen[field] = true
  position = position + 2
end
position = 3
for _ = 1, count do
  local field = ARGV[position]
  local blob = ARGV[position + 1]
  redis.call('HSET', KEYS[2], field, blob)
  for offset = 1, #blob, 20 do
    redis.call('ZADD', KEYS[3], 0, string.sub(blob, offset, offset + 19))
  end
  position = position + 2
end
if ttl > 0 then
  redis.call('EXPIRE', KEYS[1], ttl)
  redis.call('EXPIRE', KEYS[2], ttl)
  redis.call('EXPIRE', KEYS[3], ttl)
end
return {1, count}
