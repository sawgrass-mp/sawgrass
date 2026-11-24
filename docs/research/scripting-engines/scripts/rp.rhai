// The roleplay workloads in Rhai. Host functions: host_add, get_money, add_money, player_count, player_job.
// Rhai functions cannot see script variables, so the tables are global constants, read through `rp::`.

fn lcg(seed) {
    (seed * 16807) % 2147483647
}

// 64 circular zones (shops, job sites, safe zones) on a 4 km square, the same on every engine.
const ZONES = {
    let zones = [];
    let s = 42;
    for i in 0..64 {
        s = lcg(s); let x = s % 4000 - 2000;
        s = lcg(s); let y = s % 4000 - 2000;
        s = lcg(s); let r = 20 + s % 80;
        zones.push(#{ x: x.to_float(), y: y.to_float(), r2: (r * r).to_float() });
    }
    zones
};

const SALARY = [0, 300, 450, 500, 650, 800, 1200, 2000];
const NAMES = ["water", "bread", "phone", "bandage", "lockpick", "radio", "cigarettes", "lighter",
               "burger", "coffee", "repairkit", "rope", "flashlight", "map", "keys", "wallet"];
const WEIGHT = {
    let w = #{};
    for (name, i) in NAMES { w[name] = (i + 1) * 10; }
    w
};

fn noop(x) {
    x + 1
}

fn host_loop(n) {
    let s = 0;
    for i in 1..=n { s += host_add(i); }
    s
}

fn echo(s) {
    s + "!"
}

// Called once per player per tick: which zone is the player in (0-based), or -1.
fn zone(x, y) {
    for (z, i) in rp::ZONES {
        let dx = x - z.x;
        let dy = y - z.y;
        if dx * dx + dy * dy <= z.r2 { return i; }
    }
    -1
}

// An integer, or () when the word is not one.
fn to_int(w) {
    try { return w.parse_int(); } catch { return (); }
}

// A chat command from a player, moving money through the host.
fn chat(from, line) {
    let words = line.split(' ').filter(|w| w.len() > 0);
    let cmd = words[0];
    if cmd == "/pay" {
        let to = if words.len() > 1 { to_int(words[1]) } else { () };
        let amount = if words.len() > 2 { to_int(words[2]) } else { () };
        if to == () || amount == () || amount <= 0 { return "Usage: /pay <id> <amount>"; }
        if get_money(from) < amount { return "Not enough money"; }
        add_money(from, -amount);
        add_money(to, amount);
        return `Paid $${amount} to ${to}`;
    } else if cmd == "/me" {
        words.remove(0);
        return `* Player ${from} ${words.reduce(|acc, w| if acc == "" { w } else { acc + " " + w }, "")}`;
    } else if cmd == "/roll" {
        return `Player ${from} rolled ${(from * 7) % 100}`;
    }
    `Unknown command: ${cmd}`
}

// Payday: every player is paid by job, one host call to read the job and one to pay.
fn payday() {
    let total = 0;
    for i in 0..player_count() {
        let s = rp::SALARY[player_job(i)];
        if s > 0 {
            add_money(i, s);
            total += s;
        }
    }
    total
}

// An inventory of 64 item records merged into stacks: allocates maps, the garbage-collector workload.
fn inventory(seed) {
    let items = [];
    let s = seed;
    for k in 0..64 {
        s = lcg(s); let name = rp::NAMES[s % 16];
        s = lcg(s); items.push(#{ name: name, count: 1 + s % 5 });
    }
    let stacks = #{};
    for it in items {
        let c = stacks[it.name];
        stacks[it.name] = if c == () { it.count } else { c + it.count };
    }
    let weight = 0;
    for name in stacks.keys() { weight += stacks[name] * rp::WEIGHT[name]; }
    weight * 100 + stacks.len()
}

// Misbehaving scripts for the isolation tests.
fn spin() {
    let i = 0;
    loop { i += 1; }
}

fn bomb() {
    let t = [];
    let i = 1;
    loop {
        let s = "";
        s.pad(1024, "x");
        t.push(s + i);
        i += 1;
    }
}

fn recurse(n) {
    recurse(n + 1) + 1
}

fn fail() {
    throw "boom";
}
