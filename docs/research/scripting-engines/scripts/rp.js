// The roleplay workloads in JavaScript, run by QuickJS and V8. Host functions: host_add, get_money, add_money,
// player_count, player_job.

function lcg(seed) {
  return (seed * 16807) % 2147483647;
}

// 64 circular zones (shops, job sites, safe zones) on a 4 km square, the same on every engine.
const ZONES = [];
{
  let s = 42;
  for (let i = 0; i < 64; i++) {
    s = lcg(s); const x = s % 4000 - 2000;
    s = lcg(s); const y = s % 4000 - 2000;
    s = lcg(s); const r = 20 + s % 80;
    ZONES.push({ x, y, r2: r * r });
  }
}

const SALARY = [0, 300, 450, 500, 650, 800, 1200, 2000];
const NAMES = ["water", "bread", "phone", "bandage", "lockpick", "radio", "cigarettes", "lighter",
               "burger", "coffee", "repairkit", "rope", "flashlight", "map", "keys", "wallet"];
const WEIGHT = new Map(NAMES.map((name, i) => [name, (i + 1) * 10]));

function noop(x) {
  return x + 1;
}

function host_loop(n) {
  let s = 0;
  for (let i = 1; i <= n; i++) s += host_add(i);
  return s;
}

function echo(s) {
  return s + "!";
}

// Called once per player per tick: which zone is the player in (0-based), or -1.
function zone(x, y) {
  for (let i = 0; i < ZONES.length; i++) {
    const z = ZONES[i];
    const dx = x - z.x, dy = y - z.y;
    if (dx * dx + dy * dy <= z.r2) return i;
  }
  return -1;
}

// A chat command from a player, moving money through the host.
function chat(from, line) {
  const words = line.split(/\s+/).filter((w) => w.length > 0);
  const cmd = words[0];
  if (cmd === "/pay") {
    const to = Number(words[1]), amount = Number(words[2]);
    if (!Number.isFinite(to) || !Number.isFinite(amount) || amount <= 0) return "Usage: /pay <id> <amount>";
    if (get_money(from) < amount) return "Not enough money";
    add_money(from, -amount);
    add_money(to, amount);
    return "Paid $" + amount + " to " + to;
  } else if (cmd === "/me") {
    return "* Player " + from + " " + words.slice(1).join(" ");
  } else if (cmd === "/roll") {
    return "Player " + from + " rolled " + (from * 7) % 100;
  }
  return "Unknown command: " + cmd;
}

// Payday: every player is paid by job, one host call to read the job and one to pay.
function payday() {
  let total = 0;
  const n = player_count();
  for (let i = 0; i < n; i++) {
    const s = SALARY[player_job(i)];
    if (s > 0) {
      add_money(i, s);
      total += s;
    }
  }
  return total;
}

// An inventory of 64 item records merged into stacks: allocates objects, the garbage-collector workload.
function inventory(seed) {
  const items = [];
  let s = seed;
  for (let k = 0; k < 64; k++) {
    s = lcg(s); const name = NAMES[s % 16];
    s = lcg(s); items.push({ name, count: 1 + s % 5 });
  }
  const stacks = new Map();
  for (const it of items) stacks.set(it.name, (stacks.get(it.name) || 0) + it.count);
  let weight = 0;
  for (const [name, count] of stacks) weight += count * WEIGHT.get(name);
  return weight * 100 + stacks.size;
}

// Misbehaving scripts for the isolation tests.
function spin() {
  let i = 0;
  for (;;) i++;
}

function bomb() {
  const t = [];
  for (let i = 1; ; i++) t.push("x".repeat(1024) + i);
}

function recurse(n) {
  return recurse(n + 1) + 1;
}

function fail() {
  throw new Error("boom");
}

// One long call into the regular-expression engine: a backtracking pattern that cannot match, the same as the
// Lua pattern ".-.-.-.-.-b". No script code runs while it does.
function slow_builtin() {
  return /.*?.*?.*?.*?.*?b/.test("a".repeat(50000));
}
