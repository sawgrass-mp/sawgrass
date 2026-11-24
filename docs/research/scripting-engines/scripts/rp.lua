-- The roleplay workloads in Lua. Written in the subset that Lua 5.4, LuaJIT (Lua 5.1) and Luau all run, so the
-- same file is measured in all three. Host functions: host_add, get_money, add_money, player_count, player_job.

local function lcg(seed)
  return (seed * 16807) % 2147483647
end

-- 64 circular zones (shops, job sites, safe zones) on a 4 km square, the same on every engine.
local ZONES = {}
do
  local s = 42
  for i = 1, 64 do
    s = lcg(s); local x = s % 4000 - 2000
    s = lcg(s); local y = s % 4000 - 2000
    s = lcg(s); local r = 20 + s % 80
    ZONES[i] = { x = x, y = y, r2 = r * r }
  end
end

local SALARY = { 0, 300, 450, 500, 650, 800, 1200, 2000 }
local NAMES = { "water", "bread", "phone", "bandage", "lockpick", "radio", "cigarettes", "lighter",
                "burger", "coffee", "repairkit", "rope", "flashlight", "map", "keys", "wallet" }
local WEIGHT = {}
for i, name in ipairs(NAMES) do WEIGHT[name] = i * 10 end

function noop(x)
  return x + 1
end

function host_loop(n)
  local s = 0
  for i = 1, n do s = s + host_add(i) end
  return s
end

function echo(s)
  return s .. "!"
end

-- Called once per player per tick: which zone is the player in (0-based), or -1.
function zone(x, y)
  for i = 1, #ZONES do
    local z = ZONES[i]
    local dx, dy = x - z.x, y - z.y
    if dx * dx + dy * dy <= z.r2 then return i - 1 end
  end
  return -1
end

-- A chat command from a player, moving money through the host.
function chat(from, line)
  local words = {}
  for w in string.gmatch(line, "%S+") do words[#words + 1] = w end
  local cmd = words[1]
  if cmd == "/pay" then
    local to, amount = tonumber(words[2]), tonumber(words[3])
    if to == nil or amount == nil or amount <= 0 then return "Usage: /pay <id> <amount>" end
    if get_money(from) < amount then return "Not enough money" end
    add_money(from, -amount)
    add_money(to, amount)
    return "Paid $" .. amount .. " to " .. to
  elseif cmd == "/me" then
    return "* Player " .. from .. " " .. table.concat(words, " ", 2)
  elseif cmd == "/roll" then
    return "Player " .. from .. " rolled " .. (from * 7) % 100
  end
  return "Unknown command: " .. cmd
end

-- Payday: every player is paid by job, one host call to read the job and one to pay.
function payday()
  local total = 0
  for i = 0, player_count() - 1 do
    local s = SALARY[player_job(i) + 1]
    if s > 0 then
      add_money(i, s)
      total = total + s
    end
  end
  return total
end

-- An inventory of 64 item records merged into stacks: allocates tables, the garbage-collector workload.
function inventory(seed)
  local items = {}
  local s = seed
  for k = 1, 64 do
    s = lcg(s); local name = NAMES[s % 16 + 1]
    s = lcg(s); items[k] = { name = name, count = 1 + s % 5 }
  end
  local stacks, distinct = {}, 0
  for k = 1, #items do
    local it = items[k]
    local c = stacks[it.name]
    if c == nil then distinct = distinct + 1; c = 0 end
    stacks[it.name] = c + it.count
  end
  local weight = 0
  for name, count in pairs(stacks) do weight = weight + count * WEIGHT[name] end
  return weight * 100 + distinct
end

-- Misbehaving scripts for the isolation tests.
function spin()
  local i = 0
  while true do i = i + 1 end
end

function bomb()
  local t, i = {}, 1
  while true do
    t[i] = string.rep("x", 1024) .. i
    i = i + 1
  end
end

function recurse(n)
  return recurse(n + 1) + 1
end

function fail()
  error("boom")
end
