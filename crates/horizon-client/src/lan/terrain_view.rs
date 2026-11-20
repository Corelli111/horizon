//! Prepared terrain rules and bounded resident-column work for outbound terrain.
use super::chunks::{ChunkEncoder, TerrainWires};
use crate::{
    chunk::Chunk,
    fast_hash::{FxHashMap, FxHashSet},
    world::World,
};
use mod_api::{BlockState, TerrainPattern, TerrainView, TerrainViewError};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, Weak, mpsc},
};

pub(super) type Column = (i32, i32);
pub(super) type Pos = (i32, i32, i32);
pub(super) type RuleId = (&'static str, u32);

pub(super) const FACES: [Pos; 6] = [
    (-1, 0, 0),
    (1, 0, 0),
    (0, -1, 0),
    (0, 1, 0),
    (0, 0, -1),
    (0, 0, 1),
];
const HORIZONTAL: [Column; 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
const IN_FLIGHT: usize = 32;
const CACHE_BYTES: usize = 32 << 20;
const CACHE_COLUMNS: usize = 512;

pub(super) fn column((x, _, z): Pos) -> Column {
    (x >> 4, z >> 4)
}

pub(super) fn adjacent((x, z): Column) -> impl Iterator<Item = Column> {
    HORIZONTAL.into_iter().map(move |(dx, dz)| (x + dx, z + dz))
}

pub(super) fn offset((x, y, z): Pos, (dx, dy, dz): Pos) -> Pos {
    (x + dx, y + dy, z + dz)
}

pub(super) fn known(chunk: &Chunk, x: i32, y: i32, z: i32) -> Option<u32> {
    chunk
        .section_at(y)
        .ok()
        .map(|s| s.get(crate::chunk::index(x, y, z)))
}

fn world_state(world: &World, pos @ (x, y, z): Pos) -> Option<u32> {
    world
        .chunks
        .get(&column(pos))
        .and_then(|c| known(c, x, y, z))
}

struct Band {
    ys: std::ops::Range<i32>,
    targets: FxHashSet<u32>,
    replacements: Vec<u32>,
    pattern: TerrainPattern,
}

pub(super) struct Rules {
    pub(super) generation: u64,
    pub(super) spec: TerrainView,
    bands: Vec<Band>,
    targets: FxHashSet<u32>,
    occluding: FxHashSet<u32>,
}

impl Rules {
    fn compile(
        generation: u64,
        spec: &TerrainView,
        resolve: impl Fn(BlockState) -> Option<u32>,
    ) -> Result<Self, TerrainViewError> {
        if spec.bands.len() > 16
            || spec.reveal_radius > 4
            || spec.occluding.len().saturating_add(
                spec.bands
                    .iter()
                    .map(|band| band.targets.len())
                    .sum::<usize>(),
            ) > 65_536
        {
            return Err(TerrainViewError::InvalidRules);
        }
        let invalid = TerrainViewError::InvalidRules;
        let occluding = spec
            .occluding
            .iter()
            .copied()
            .map(&resolve)
            .collect::<Option<_>>()
            .ok_or(invalid)?;
        let mut bands = Vec::new();
        for band in &spec.bands {
            if band.min_y >= band.max_y
                || band.replacements.is_empty()
                || band.replacements.len() > 256
                || !matches!(
                    band.pattern,
                    TerrainPattern::Block(_) | TerrainPattern::Layer(_)
                )
            {
                return Err(TerrainViewError::InvalidRules);
            }
            bands.push(Band {
                ys: band.min_y..band.max_y,
                targets: band
                    .targets
                    .iter()
                    .copied()
                    .map(&resolve)
                    .collect::<Option<_>>()
                    .ok_or(invalid)?,
                replacements: band
                    .replacements
                    .iter()
                    .copied()
                    .map(&resolve)
                    .collect::<Option<_>>()
                    .ok_or(invalid)?,
                pattern: band.pattern,
            });
        }
        Ok(Self {
            generation,
            spec: spec.clone(),
            targets: bands
                .iter()
                .flat_map(|b| b.targets.iter().copied())
                .collect(),
            bands,
            occluding,
        })
    }

    pub(super) fn eligible(&self, y: i32, palette: Option<&[u32]>) -> bool {
        self.bands
            .iter()
            .any(|band| band.ys.start < y + 16 && y < band.ys.end)
            && palette.is_none_or(|values| values.iter().any(|v| self.targets.contains(v)))
    }

    pub(super) fn targets_state(&self, y: i32, state: u32) -> bool {
        self.targets.contains(&state)
            && self
                .bands
                .iter()
                .any(|band| band.ys.contains(&y) && band.targets.contains(&state))
    }

    pub(super) fn state(&self, pos: Pos, state: u32, read: impl Fn(Pos) -> Option<u32>) -> u32 {
        let (x, y, z) = pos;
        if !self.targets.contains(&state) {
            return state;
        }
        let Some(band) = self
            .bands
            .iter()
            .find(|b| b.ys.contains(&y) && b.targets.contains(&state))
        else {
            return state;
        };
        if !FACES.iter().all(|&face| {
            read(offset(pos, face)).map_or(self.spec.unavailable_occludes, |s| {
                self.occluding.contains(&s)
            })
        }) {
            return state;
        }
        if band.replacements.len() == 1 {
            return band.replacements[0];
        }
        let ((x, z), seed) = match band.pattern {
            TerrainPattern::Block(seed) => ((x, z), seed),
            TerrainPattern::Layer(seed) => (column(pos), seed),
            _ => unreachable!("terrain patterns are validated by the SDK"),
        };
        let mut hash = seed
            ^ (x as u64).wrapping_mul(0x9e3779b97f4a7c15)
            ^ (y as u64).wrapping_mul(0xbf58476d1ce4e5b9)
            ^ (z as u64).wrapping_mul(0x94d049bb133111eb);
        hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d049bb133111eb);
        band.replacements[((hash ^ (hash >> 31)) % band.replacements.len() as u64) as usize]
    }

    pub(super) fn in_world(&self, world: &World, pos: Pos, state: u32) -> u32 {
        self.state(pos, state, |p| world_state(world, p))
    }
}

pub(super) type Neighbours = [Option<Arc<Chunk>>; 4];

pub(super) fn neighbours(world: &World, (x, z): Column) -> Neighbours {
    HORIZONTAL.map(|(dx, dz)| world.chunks.get(&(x + dx, z + dz)).cloned())
}

impl Selection {
    pub(super) fn state(&self, world: &World, rules: &Rules, pos: Pos, state: u32) -> u32 {
        if self.reveals.contains(&pos) {
            state
        } else {
            rules.in_world(world, pos, state)
        }
    }
}

#[derive(Default)]
pub(super) struct Selection {
    pub(super) id: Option<RuleId>,
    pub(super) refresh: bool,
    pub(super) reveals: FxHashSet<Pos>,
}

#[derive(Default)]
pub(super) struct Views {
    rules: FxHashMap<RuleId, Arc<Rules>>,
    generation: u64,
    pub(super) players: FxHashMap<u32, FxHashMap<String, Selection>>,
    pub(super) resident: Resident,
}

impl Views {
    pub(super) fn define(
        &mut self,
        id: RuleId,
        spec: &TerrainView,
        resolve: impl Fn(BlockState) -> Option<u32>,
    ) -> Result<(), TerrainViewError> {
        if let Some(old) = self.rules.get(&id) {
            if old.spec == *spec {
                return Ok(());
            }
        } else if self.rules.len() == 16 {
            return Err(TerrainViewError::Limit);
        }
        let rules = if let Some(equal) = self.rules.values().find(|rules| rules.spec == *spec) {
            equal.clone()
        } else {
            let next = self.generation + 1;
            let rules = Rules::compile(next, spec, resolve)?;
            self.generation = next;
            Arc::new(rules)
        };
        self.rules.insert(id, rules);
        for selection in self.players.values_mut().flat_map(|v| v.values_mut()) {
            if selection.id == Some(id) {
                selection.refresh = true;
                selection.reveals.clear();
            }
        }
        Ok(())
    }

    pub(super) fn select(
        &mut self,
        conn: u32,
        dimension: &str,
        owner: &'static str,
        id: Option<u32>,
    ) -> Result<(), TerrainViewError> {
        let id = id.map(|id| (owner, id));
        if id.is_some_and(|id| !self.rules.contains_key(&id)) {
            return Err(TerrainViewError::UnknownView);
        }
        let dimensions = self.players.entry(conn).or_default();
        if let Some(selection) = dimensions.get_mut(dimension) {
            if selection.id == id {
                return Ok(());
            }
            if selection.id.is_some_and(|(held, _)| held != owner) {
                return Err(TerrainViewError::Owned);
            }
            if let (Some(old), Some(new)) = (selection.id, id)
                && Arc::ptr_eq(&self.rules[&old], &self.rules[&new])
            {
                selection.id = id;
                return Ok(());
            }
            *selection = Selection {
                id,
                refresh: true,
                ..Selection::default()
            };
        } else if id.is_some() {
            if dimensions.len() == 32 {
                return Err(TerrainViewError::Limit);
            }
            dimensions.insert(
                dimension.to_owned(),
                Selection {
                    id,
                    refresh: true,
                    ..Selection::default()
                },
            );
        }
        Ok(())
    }

    pub(super) fn targets_state(&self, y: i32, state: u32) -> bool {
        self.rules
            .values()
            .any(|rules| rules.targets_state(y, state))
    }

    pub(super) fn view(&self, conn: u32, dimension: &str) -> Option<(&Selection, &Arc<Rules>)> {
        let selection = self.players.get(&conn)?.get(dimension)?;
        Some((selection, self.rules.get(&selection.id?)?))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key(Arc<str>, u64, Column);

type Identity = [Option<Weak<Chunk>>; 5];

fn identity(chunk: &Arc<Chunk>, neighbours: &Neighbours) -> Identity {
    std::array::from_fn(|i| {
        if i == 0 {
            Some(Arc::downgrade(chunk))
        } else {
            neighbours[i - 1].as_ref().map(Arc::downgrade)
        }
    })
}

struct Job {
    key: Key,
    identity: Identity,
    chunk: Arc<Chunk>,
    neighbours: Neighbours,
    encoder: ChunkEncoder,
    rules: Arc<Rules>,
}

type Done = (Key, Identity, Arc<TerrainWires>);

struct Worker {
    jobs: mpsc::SyncSender<Job>,
    done: mpsc::Receiver<Done>,
}

impl Worker {
    fn start() -> Option<Self> {
        let (jobs, work) = mpsc::sync_channel::<Job>(IN_FLIGHT);
        let (reply, done) = mpsc::channel();
        let work = Arc::new(Mutex::new(work));
        let workers =
            std::thread::available_parallelism().map_or(1, |n| if n.get() >= 4 { 2 } else { 1 });
        let mut started = false;
        for index in 0..workers {
            let (work, reply) = (work.clone(), reply.clone());
            started |= std::thread::Builder::new()
                .name(format!("Terrain view encoder #{index}"))
                .spawn(move || {
                    loop {
                        let Ok(job) = work.lock().unwrap().recv() else {
                            return;
                        };
                        let wires = Arc::new(job.encoder.terrain_wires(
                            &job.chunk,
                            &job.neighbours,
                            &job.rules,
                        ));
                        if reply.send((job.key, job.identity, wires)).is_err() {
                            return;
                        }
                    }
                })
                .is_ok();
        }
        started.then_some(Self { jobs, done })
    }
}

#[derive(Default)]
pub(super) struct Resident {
    #[cfg(test)]
    pub(in crate::lan) submitted: usize,
    worker: Option<Worker>,
    dimension: Option<Arc<str>>,
    // Every candidate is checked or queued once per level pass, even while many guests share
    // the same queue at spawn. None means the work is still pending or the queue had no room.
    pass: FxHashMap<Key, Option<Arc<TerrainWires>>>,
    pending: FxHashSet<Key>,
    cache: FxHashMap<Key, (Identity, Arc<TerrainWires>)>,
    ordering: VecDeque<Key>,
    bytes: usize,
}

impl Resident {
    pub(super) fn poll(&mut self) {
        self.pass.clear();
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        while let Ok((key, identity, result)) = worker.done.try_recv() {
            self.pending.remove(&key);
            self.bytes += result.bytes();
            self.cache.insert(key.clone(), (identity, result));
            self.ordering.push_back(key);
            while self.bytes > CACHE_BYTES || self.cache.len() > CACHE_COLUMNS {
                if let Some((_, old)) = self
                    .ordering
                    .pop_front()
                    .and_then(|k| self.cache.remove(&k))
                {
                    self.bytes -= old.bytes();
                }
            }
        }
    }

    pub(super) fn get(
        &mut self,
        dimension: &str,
        encoder: &ChunkEncoder,
        rules: Arc<Rules>,
        world: &World,
        at: Column,
    ) -> Option<Arc<TerrainWires>> {
        if self.dimension.as_deref() != Some(dimension) {
            self.dimension = Some(Arc::from(dimension));
        }
        let key = Key(
            self.dimension.as_ref().unwrap().clone(),
            rules.generation,
            at,
        );
        if let Some(wires) = self.pass.get(&key) {
            return wires.clone();
        }
        let wires = self.lookup(&key, encoder, rules, world);
        if self.pass.len() < CACHE_COLUMNS {
            self.pass.insert(key, wires.clone());
        }
        wires
    }

    fn lookup(
        &mut self,
        key: &Key,
        encoder: &ChunkEncoder,
        rules: Arc<Rules>,
        world: &World,
    ) -> Option<Arc<TerrainWires>> {
        let at = key.2;
        let chunk = world.chunks.get(&at)?;
        if let Some((old, wires)) = self.cache.get(key) {
            let current = std::iter::once(Some(chunk))
                .chain(adjacent(at).map(|next| world.chunks.get(&next)));
            if current.zip(old).all(|(chunk, stamp)| match (chunk, stamp) {
                (None, None) => true,
                (Some(chunk), Some(stamp)) => Arc::as_ptr(chunk) == Weak::as_ptr(stamp),
                _ => false,
            }) {
                return Some(wires.clone());
            }
            self.bytes -= wires.bytes();
            self.cache.remove(key);
            self.ordering.retain(|k| k != key);
        }
        if self.pending.len() == IN_FLIGHT || self.pending.contains(key) {
            return None;
        }
        let neighbours = neighbours(world, at);
        let keys = identity(chunk, &neighbours);
        if self.worker.is_none() {
            self.worker = Worker::start();
        }
        if self
            .worker
            .as_ref()?
            .jobs
            .try_send(Job {
                key: key.clone(),
                identity: keys,
                chunk: chunk.clone(),
                neighbours,
                encoder: encoder.detached(),
                rules,
            })
            .is_ok()
        {
            self.pending.insert(key.clone());
            #[cfg(test)]
            {
                self.submitted += 1;
            }
        }
        None
    }
}
