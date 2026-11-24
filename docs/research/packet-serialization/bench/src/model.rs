//! The game messages every format encodes, and a seeded generator of realistic values.
//!
//! Values are already quantized as `sawgrass-protocol` quantizes them (positions in centimetres, velocities in
//! cm/s, rotations as 32 smallest-three bits), so every format carries exactly the same information and the
//! comparison is of the encoding alone.

use serde::{Deserialize, Serialize};

macro_rules! wire {
    ($($item:item)*) => {
        $(
            #[derive(
                Clone, Debug, PartialEq, Serialize, Deserialize, bitcode::Encode, bitcode::Decode,
                rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
            )]
            $item
        )*
    };
}

wire! {
    /// One entity's state: what `sawgrass_protocol::EntityState` holds.
    pub struct Entity {
        pub id: u32,
        /// Centimetres; each fits in 24 bits.
        pub pos: [i32; 3],
        /// Centimetres per second.
        pub vel: [i16; 3],
        /// Smallest-three rotation bits.
        pub rot: u32,
    }

    /// Server to client, 20 times a second: the entities this player can see.
    pub struct Snapshot {
        pub seq: u16,
        pub tick: u32,
        pub entities: Vec<Entity>,
    }

    /// Client to server, 20 times a second: its own state and input.
    pub struct ClientState {
        pub seq: u16,
        pub tick: u32,
        pub me: Entity,
        pub buttons: u32,
        pub aim: [i16; 2],
    }

    /// A reliable chat line.
    pub struct Chat {
        pub seq: u16,
        pub tick: u32,
        pub from: u32,
        pub channel: u8,
        pub text: String,
    }

    /// A reliable event raised by a game-mode script, with typed arguments.
    pub struct ScriptEvent {
        pub seq: u16,
        pub tick: u32,
        pub name: String,
        pub args: Vec<Arg>,
    }

    /// A script argument.
    pub enum Arg {
        Bool(bool),
        Int(i64),
        Float(f32),
        Str(String),
    }

    /// Any packet. The receiver does not know which kind comes next, so every format encodes this enum.
    pub enum Packet {
        Snapshot(Snapshot),
        ClientState(ClientState),
        Chat(Chat),
        ScriptEvent(ScriptEvent),
    }
}

/// Deterministic xorshift64* generator.
#[derive(Debug, Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
}

/// An entity near `centre` (metres): 30% standing, 40% on foot, 30% in vehicles, any heading.
pub fn entity(rng: &mut Rng, centre: [f64; 2]) -> Entity {
    let pos = [
        centre[0] + rng.range(-250.0, 250.0),
        centre[1] + rng.range(-250.0, 250.0),
        rng.range(0.0, 120.0),
    ];
    let kind = rng.unit();
    let speed = if kind < 0.3 {
        0.0
    } else if kind < 0.7 {
        rng.range(0.5, 6.0)
    } else {
        rng.range(5.0, 45.0)
    };
    let heading = rng.range(0.0, std::f64::consts::TAU);
    let vel = [
        speed * heading.cos(),
        speed * heading.sin(),
        if speed > 0.0 { rng.range(-1.0, 1.0) } else { 0.0 },
    ];
    let half = heading / 2.0;
    let rot = sawgrass_protocol::Rotation::from_quaternion([0.0, 0.0, half.sin() as f32, half.cos() as f32])
        .expect("unit quaternion");
    let p = sawgrass_protocol::Position::from_metres(pos.map(|v| v as f32)).expect("in range");
    let v = sawgrass_protocol::Velocity::from_metres_per_second(vel.map(|v| v as f32)).expect("in range");
    Entity {
        id: (rng.next() % 20_000) as u32 + 1,
        pos: p.cm(),
        vel: v.cm_per_s,
        rot: rot.bits,
    }
}

/// The messages measured, by name.
pub fn messages(seed: u64, visible: usize) -> Vec<(&'static str, Packet)> {
    let mut rng = Rng(seed | 1);
    let centre = [rng.range(-4000.0, 4000.0), rng.range(-4000.0, 4000.0)];
    let entities = (0..visible).map(|_| entity(&mut rng, centre)).collect();
    vec![
        (
            "snapshot",
            Packet::Snapshot(Snapshot {
                seq: 4711,
                tick: 1_234_567,
                entities,
            }),
        ),
        (
            "client_state",
            Packet::ClientState(ClientState {
                seq: 4712,
                tick: 1_234_566,
                me: entity(&mut rng, centre),
                buttons: 0b1000_0000_0101,
                aim: [-3120, 845],
            }),
        ),
        (
            "chat",
            Packet::Chat(Chat {
                seq: 17,
                tick: 1_234_570,
                from: 1042,
                channel: 2,
                text: "Meet me at the pier in five minutes, bring the van.".into(),
            }),
        ),
        (
            "script_event",
            Packet::ScriptEvent(ScriptEvent {
                seq: 18,
                tick: 1_234_571,
                name: "inventory:use".into(),
                args: vec![
                    Arg::Int(1042),
                    Arg::Int(3),
                    Arg::Str("medkit".into()),
                    Arg::Bool(true),
                ],
            }),
        ),
    ]
}
