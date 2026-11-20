use super::*;
use crate::{
    chunk::{MIN_Y, SECTIONS},
    containers::{BlockEntity, ModBlockEntity, ModBlockEntityFields},
    lan::{Out, PlayerSender},
};
use horizon_core::nbt::{Compound, Tag};
use horizon_protocol::{buf::Reader, registry::Builtin};
use horizon_world::registry::BlockStateId;
use mod_api::{BlockState, BlockStateRecord, TerrainBand, TerrainPattern, TerrainView};
use std::{
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

const DIM: &str = "minecraft:overworld";
const CHEST: [i32; 3] = [15, 64, 1];
const MACHINE: [i32; 3] = [12, 64, 1];

fn tuple([x, y, z]: [i32; 3]) -> (i32, i32, i32) {
    (x, y, z)
}

fn setup() -> (Hub, [Receiver<Out>; 3], World) {
    let (mut hub, first) = hub_with_a_guest();
    let players = [2, 3].map(|conn| {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut guest = Guest::new(
            crate::lan::Joined {
                conn,
                uuid: u128::from(conn),
                name: format!("Player{conn}"),
                properties: Vec::new(),
                info: horizon_protocol::packets::configuration::ClientInformation::new(8),
                brand: "vanilla".into(),
                sender: PlayerSender::new(tx, Arc::default()),
                latency: Arc::default(),
                address: "127.0.0.1:25565".parse().unwrap(),
                expected: false,
            },
            conn as i32,
            [0.5, 64.0, 0.5],
            8,
        );
        guest.placed = true;
        guest.dimension = DIM.into();
        hub.guests.push(guest);
        rx
    });
    let builtin = Builtin::parse(
        r#"{"minecraft:block_entity_type":{"entries":{
        "minecraft:chest":{"protocol_id":1},"test:machine":{"protocol_id":49}
    }}}"#,
    )
    .unwrap();
    let types = crate::block_entity_types::BlockEntityTypes::new(
        [("test:machine".into(), "test:machine".into())].into(),
        builtin
            .get("minecraft:block_entity_type")
            .unwrap()
            .names
            .clone(),
    );
    let states = [
        "minecraft:air",
        "minecraft:stone",
        "minecraft:chest",
        "test:machine",
    ]
    .map(|state| (Arc::<str>::from(state), false, None));
    hub.encoder = ChunkEncoder::with_types(states, &builtin, &[], &[], Some(&types));
    hub.encoder.mod_update_tag = Some(|_, entity| Some(entity.nbt.clone()));

    let state = |id| {
        BlockState::from(BlockStateRecord::new(
            mod_api::__private::parts_installation::block_key(id),
            0,
        ))
    };
    hub.define_terrain_view(
        "test",
        1,
        &TerrainView::new(
            vec![TerrainBand::new(
                0,
                80,
                vec![state(2), state(3)],
                vec![state(1)],
                TerrainPattern::Block(0),
            )],
            vec![state(1)],
            true,
            2,
        ),
        |state| {
            Some(mod_api::__private::parts_installation::block_key_slot(
                state.block(),
            ))
        },
    )
    .unwrap();
    for conn in [1, 2] {
        hub.set_terrain_view(conn, DIM, "test", Some(1)).unwrap();
    }

    let mut world = World::new(BlockStateId(0), 0);
    world.block_log = Some(Vec::new());
    let mut first_chunk = Chunk::empty(0, 0);
    let mut next_chunk = Chunk::empty(1, 0);
    for [x, y, z] in [CHEST, MACHINE] {
        for (dx, dy, dz) in crate::lan::terrain_view::FACES {
            if x + dx == 16 {
                next_chunk.set(0, y + dy, z + dz, 1);
            } else {
                first_chunk.set(x + dx, y + dy, z + dz, 1);
            }
        }
    }
    first_chunk.set(CHEST[0], CHEST[1], CHEST[2], 2);
    first_chunk.set(MACHINE[0], MACHINE[1], MACHINE[2], 3);
    world
        .block_entities
        .insert(tuple(CHEST), BlockEntity::Chest(vec![None; 27]));
    let machine = ModBlockEntity::new(
        "test:machine".into(),
        ModBlockEntityFields {
            nbt: Compound::from([("persisted".into(), Tag::Int(7))]),
            ..Default::default()
        },
    );
    world
        .block_entities
        .insert(tuple(MACHINE), BlockEntity::Mod(Box::new(machine)));
    for mut chunk in [first_chunk, next_chunk] {
        chunk.lit = true;
        world.chunks.insert((chunk.x, chunk.z), Arc::new(chunk));
    }
    let [second, third] = players;
    (hub, [first, second, third], world)
}

fn drain(rx: &Receiver<Out>) -> Vec<C> {
    rx.try_iter()
        .flat_map(|out| match out {
            Out::Packet(body) => vec![body],
            Out::Packets(bodies) => bodies,
            _ => Vec::new(),
        })
        .map(|body| {
            let mut reader = Reader::new(&body);
            C::decode(reader.varint().unwrap(), reader.rest()).unwrap()
        })
        .collect()
}

fn update_state(packet: &C, pos: [i32; 3]) -> Option<i32> {
    match packet {
        C::BlockUpdate(p) if p.pos == pos => Some(p.state),
        C::SectionBlocksUpdate(p) if p.section == pos.map(|v| v >> 4) => {
            let index = (pos[0] & 15) << 8 | (pos[2] & 15) << 4 | (pos[1] & 15);
            p.changes
                .iter()
                .find(|&&v| (v as i32 & 4095) == index)
                .map(|v| (v >> 12) as i32)
        }
        _ => None,
    }
}

fn initial(packets: &[C]) -> Option<&play::LevelChunkWithLight> {
    packets.iter().find_map(|packet| match packet {
        C::LevelChunkWithLight(p) if (p.x, p.z) == (0, 0) => Some(p),
        _ => None,
    })
}

fn section_state(chunk: &play::LevelChunkWithLight, [x, y, z]: [i32; 3]) -> i32 {
    let sections = chunk.chunk.sections(SECTIONS, 15, 6).unwrap();
    sections[((y - MIN_Y) >> 4) as usize]
        .states
        .get(crate::chunk::index(x, y, z))
}

fn wait_for_chunks(hub: &mut Hub, world: &mut World, players: &[&Receiver<Out>]) -> Vec<Vec<C>> {
    let until = Instant::now() + Duration::from_secs(2);
    let mut all: Vec<Vec<C>> = players.iter().map(|_| Vec::new()).collect();
    while all.iter().any(|packets| initial(packets).is_none()) {
        assert!(Instant::now() < until, "terrain never arrived");
        for i in 0..hub.guests.len() {
            hub.guests.update(i, |g| g.unacknowledged = 0);
        }
        send_chunks(hub, world);
        for (packets, receiver) in all.iter_mut().zip(players) {
            packets.extend(drain(receiver));
        }
        std::thread::yield_now();
    }
    all
}

#[test]
fn terrain_views_filter_shared_batches_deltas_metadata_and_corrections() {
    let (mut hub, rx, mut world) = setup();
    let packets = wait_for_chunks(&mut hub, &mut world, &rx.iter().collect::<Vec<_>>());
    for (i, packets) in packets.iter().enumerate() {
        let chunk = initial(packets).unwrap();
        assert_eq!(section_state(chunk, CHEST), if i < 2 { 1 } else { 2 });
        assert_eq!(chunk.chunk.block_entities.len(), if i < 2 { 0 } else { 2 });
    }
    // The protected pair selected one view and got the same prepared body.
    assert_eq!(
        initial(&packets[0]).unwrap().chunk.buffer,
        initial(&packets[1]).unwrap().chunk.buffer
    );
    assert_eq!(hub.terrain.as_ref().unwrap().resident.submitted, 1);

    hub.send_block_correction(1, &world, CHEST);
    assert_eq!(
        drain(&rx[0]).iter().find_map(|p| update_state(p, CHEST)),
        Some(1)
    );

    let open_face = [16, 64, 1];
    Arc::make_mut(world.chunks.get_mut(&(1, 0)).unwrap()).set(0, open_face[1], open_face[2], 0);
    world.block_log.as_mut().unwrap().push(tuple(open_face));
    hub.broadcast_block_changes(&mut world);
    for receiver in &rx[..2] {
        assert_eq!(
            drain(receiver).iter().find_map(|p| update_state(p, CHEST)),
            Some(2)
        );
    }
    assert!(
        drain(&rx[2])
            .iter()
            .all(|p| update_state(p, CHEST).is_none())
    );

    let BlockEntity::Mod(machine) = &world.block_entities[&tuple(MACHINE)] else {
        panic!("machine")
    };
    machine.set_client_update_fields(Compound::from([("effect".into(), Tag::Int(11))]));
    world.block_log.as_mut().unwrap().push(tuple(MACHINE));
    hub.broadcast_block_changes(&mut world);
    for receiver in &rx[..2] {
        assert!(
            !drain(receiver)
                .iter()
                .any(|p| matches!(p, C::BlockEntityData(tag) if tag.pos == MACHINE))
        );
    }
    assert!(
        drain(&rx[2])
            .iter()
            .any(|p| matches!(p, C::BlockEntityData(tag)
        if tag.pos == MACHINE && tag.tag.get("effect") == Some(&Tag::Int(11))))
    );

    let BlockEntity::Mod(machine) = &world.block_entities[&tuple(MACHINE)] else {
        panic!("machine")
    };
    machine.set_client_update_fields(Compound::from([("effect".into(), Tag::Int(12))]));
    Arc::make_mut(world.chunks.get_mut(&(0, 0)).unwrap()).set(11, 64, 1, 0);
    world
        .block_log
        .as_mut()
        .unwrap()
        .extend([(11, 64, 1), tuple(MACHINE)]);
    hub.broadcast_block_changes(&mut world);
    for receiver in &rx {
        let packets = drain(receiver);
        let (at, _) = packets
            .iter()
            .enumerate()
            .find(|(_, p)| {
                matches!(p, C::BlockEntityData(tag)
            if tag.pos == MACHINE && tag.tag.get("effect") == Some(&Tag::Int(12)))
            })
            .unwrap();
        assert!(
            packets[..at]
                .iter()
                .any(|p| update_state(p, MACHINE) == Some(3))
        );
    }

    // A changed view invalidates even the terrain already tracked by the client.
    hub.set_terrain_view(3, DIM, "test", Some(1)).unwrap();
    Arc::make_mut(world.chunks.get_mut(&(1, 0)).unwrap()).set(0, 64, 1, 1);
    let changed = wait_for_chunks(&mut hub, &mut world, &[&rx[2]]);
    assert!(matches!(changed[0].first(), Some(C::ForgetLevelChunk(_))));
    assert_eq!(section_state(initial(&changed[0]).unwrap(), CHEST), 1);
    assert_eq!(world.block(CHEST[0], CHEST[1], CHEST[2]), BlockStateId(2));
    assert!(matches!(
        world.block_entities.get(&tuple(CHEST)),
        Some(BlockEntity::Chest(_))
    ));
}

#[test]
fn saved_far_view_uses_resident_faces_and_preserves_the_save() {
    use crate::{
        biome::Biomes,
        lan::far::{Far, Saved},
        save::{LevelShape, SaveDir},
    };
    use horizon_assets::resource_source::ResourceRoot;
    use horizon_world::registry::BlockRegistry;

    let (hub, _, mut world) = setup();
    let rules = hub.terrain_rules(1).unwrap();
    let root = tempfile::tempdir().unwrap();
    let biome_dir = root.path().join("data/minecraft/worldgen/biome");
    std::fs::create_dir_all(&biome_dir).unwrap();
    std::fs::write(biome_dir.join("plains.json"), "{}").unwrap();
    let biomes = Arc::new(
        Biomes::load_for_server(&ResourceRoot::directory(root.path().to_owned())).unwrap(),
    );
    let registry: &'static BlockRegistry = Box::leak(Box::new(
        BlockRegistry::from_facts_json(
            br#"{"defaults":{"minecraft:air":0,"minecraft:stone":1,"minecraft:chest":2},
            "states":[["air",true,false,"","minecraft:air"],
                      ["stone",false,false,"","minecraft:stone"],
                      ["chest",false,false,"","minecraft:chest"]]}"#
                .as_slice(),
        )
        .unwrap(),
    ));
    let save = Arc::new(SaveDir::new(
        root.path().join("save"),
        LevelShape::OVERWORLD,
    ));
    let mut chunk = Chunk::empty(12, 0);
    let [x, y, z] = CHEST;
    for (dx, dy, dz) in crate::lan::terrain_view::FACES {
        if x + dx != 16 {
            chunk.set(x + dx, y + dy, z + dz, 1);
        }
    }
    chunk.lit = true;
    chunk.light_loaded = true;
    chunk.set(x, y, z, 2);
    chunk
        .block_entities
        .push(((12 * 16 + x, y, z), BlockEntity::Chest(vec![None; 27])));
    save.save(&[Arc::new(chunk)], registry, &biomes).unwrap();

    let mut far = Far::new(24, registry, biomes.clone());
    far.set_level(DIM, &save, &hub.encoder);
    let mut get = |world: &World, protected| {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(Instant::now() < until, "saved column never arrived");
            far.poll();
            let saved = if protected {
                far.saved_for_view(12, 0, &rules, world)
            } else {
                far.saved(12, 0)
            };
            match saved {
                Saved::Body(data) => {
                    let mut reader = Reader::new(&data);
                    let C::LevelChunkWithLight(column) =
                        C::decode(reader.varint().unwrap(), reader.rest()).unwrap()
                    else {
                        panic!("far column")
                    };
                    return column;
                }
                Saved::Missing => panic!("saved column marked missing"),
                Saved::Waiting => std::thread::yield_now(),
            }
        }
    };

    let ordinary = get(&world, false);
    assert_eq!(section_state(&ordinary, CHEST), 2);
    assert_eq!(ordinary.chunk.block_entities.len(), 1);
    let buried = get(&world, true);
    assert_eq!(section_state(&buried, CHEST), 1);
    assert!(buried.chunk.block_entities.is_empty());

    // The current disk body was prepared with an unavailable east face. An all-air resident
    // neighbour is known, so it invalidates that body even without a block-log event.
    world.chunks.insert((13, 0), Arc::new(Chunk::empty(13, 0)));
    let exposed = get(&world, true);
    assert_eq!(section_state(&exposed, CHEST), 2);
    assert_eq!(exposed.chunk.block_entities.len(), 1);
    let saved = save.load_far(12, 0, registry, &biomes).unwrap();
    assert_eq!(saved.block(x, y, z), BlockStateId(2));
    assert_eq!(saved.block_entities.len(), 1);
}

#[test]
fn protected_shared_spawn() {
    if std::env::var_os("HORIZON_TERRAIN_BENCH").is_none() {
        return;
    }
    let (rate, ms) = super::terrain_send_bench::median(|| {
        let (mut hub, rx, _) = setup();
        hub.guests.remove(2);
        hub.guests.remove(1);
        for conn in 2..=8 {
            hub.set_terrain_view(conn, DIM, "test", Some(1)).unwrap();
        }
        let result = super::terrain_send_bench::burst(&mut hub);
        assert_eq!(hub.terrain.as_ref().unwrap().resident.submitted, 121);
        drop(rx);
        result
    });
    println!(
        "protected shared spawn: {rate:.0} recipient chunks/s, {ms:.3} ms total in chunk passes"
    );
}

#[test]
fn validated_break_reveal_is_bounded_and_sticks_to_the_actor() {
    let (mut hub, rx, mut world) = setup();
    wait_for_chunks(&mut hub, &mut world, &rx.iter().collect::<Vec<_>>());
    // The block at x=10 has just been broken, two steps from the still-covered machine.
    // The chest is farther away and remains covered by its own neighbours.
    hub.reveal_terrain_after_break(1, &mut world, [10, 64, 1]);
    let packets = drain(&rx[0]);
    assert!(
        matches!(&packets[..], [C::BlockUpdate(state), C::BlockEntityData(tag)]
        if state.pos == MACHINE && state.state == 3 && tag.pos == MACHINE)
    );
    assert!(drain(&rx[1]).is_empty() && drain(&rx[2]).is_empty());
    for conn in [1, 2] {
        hub.send_block_correction(conn, &world, MACHINE);
        hub.send_block_correction(conn, &world, CHEST);
    }
    for (i, rx) in rx[..2].iter().enumerate() {
        let packets = drain(rx);
        assert_eq!(
            packets.iter().find_map(|p| update_state(p, MACHINE)),
            Some(if i == 0 { 3 } else { 1 })
        );
        assert_eq!(packets.iter().find_map(|p| update_state(p, CHEST)), Some(1));
    }
}
