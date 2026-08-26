if redis.call('EXISTS', KEYS[1]) ~= 0 or redis.call('EXISTS', KEYS[2]) ~= 0 or redis.call('EXISTS', KEYS[3]) ~= 0 then
  return 0
end
redis.call('SET', KEYS[1], ARGV[1])
local ttl = tonumber(ARGV[2])
if ttl > 0 then
  redis.call('EXPIRE', KEYS[1], ttl)
end
return 1
