use std::collections::HashMap;
use std::num::NonZeroU64;

pub enum TaskSchedule {
    EveryTick,
    EveryN(NonZeroU64),
    Once(u64),
}

pub trait TaskClosure: FnMut(&mut Level) + Send + 'static {}
impl<T> TaskClosure for T where T: FnMut(&mut Level) + Send + 'static {}

pub type Task = (Box<dyn FnMut(&mut Level) + Send>, TaskSchedule);

pub struct Level {
    tasks: Vec<Option<Task>>,
    time: u64,
    chunks: HashMap<(i32, i32), LevelChunk>,
}

pub struct LevelChunk {
    sections: [ChunkSection; 24],
}

pub struct ChunkSection {
    block_count: u16,
    block_states: BlockPalettedContainer,
    biomes: BiomePalettedContainer,
}

pub enum PalettedContainer<T> {
    Single(T),
    Indirect {
        bits_per_entry: u8,
        palette: Vec<T>,
        data: Vec<u64>,
    },
    Direct(Vec<T>),
}

pub type BlockPalettedContainer = PalettedContainer<u16>;
pub type BiomePalettedContainer = PalettedContainer<u8>;

impl Level {
    pub fn tick(&mut self) {
        let mut for_deletion = vec![];
        let mut for_execution = vec![];
        for (i, v) in self.tasks.iter().enumerate() {
            if let Some(v) = v {
                match v.1 {
                    TaskSchedule::EveryTick => for_execution.push(i),
                    TaskSchedule::EveryN(n) if self.time % n.get() == 0 => for_execution.push(i),
                    TaskSchedule::EveryN(_) => (),
                    TaskSchedule::Once(n) if self.time == n => {
                        for_execution.push(i);
                        for_deletion.push(i);
                    }
                    TaskSchedule::Once(n) if self.time > n => for_deletion.push(i),
                    TaskSchedule::Once(_) => (),
                }
            }
        }
        for i in for_execution.into_iter().rev() {
            let mut task = self.tasks[i].take();
            if let Some(ref mut t) = task {
                t.0(self)
            }
            self.tasks[i] = task;
        }
        for i in for_deletion.into_iter().rev() {
            self.tasks.remove(i);
        }
        self.time += 1;
    }

    pub fn new() -> Self {
        Self {
            tasks: vec![],
            time: 0,
            chunks: HashMap::new(),
        }
    }

    pub fn register_task(&mut self, task: Task) {
        self.tasks.push(Some(task));
    }

    pub fn every_tick<F: TaskClosure>(&mut self, closure: F) {
        self.register_task((Box::new(closure), TaskSchedule::EveryTick));
    }

    pub fn every_n<F: TaskClosure>(&mut self, closure: F, every: NonZeroU64) {
        self.register_task((Box::new(closure), TaskSchedule::EveryN(every)));
    }

    pub fn once<F: TaskClosure>(&mut self, closure: F, when: u64) {
        self.register_task((Box::new(closure), TaskSchedule::Once(when)));
    }
}

impl LevelChunk {
    pub fn new() -> Self {
        Self {
            sections: std::array::from_fn(|_| ChunkSection::new()),
        }
    }
}

impl ChunkSection {
    pub fn new() -> Self {
        Self {
            block_count: 0,
            block_states: BlockPalettedContainer::Single(0),
            biomes: BiomePalettedContainer::Single(0),
        }
    }
    pub fn set_block(&mut self, x: u8, y: u8, z: u8, block_state: u16) {
        if x > 15 || y > 15 || z > 15 { return; }
        use PalettedContainer::*;
        match &mut self.block_states {
            Single(n) if block_state == *n => (),
            Single(n) => {
                self.block_states = Indirect {
                    bits_per_entry: 1,
                    palette: vec![*n, block_state],
                    data: vec![0u64].repeat(64)
                };
                let (i, j) = Self::index(x, y, z, 1);
                let Indirect{bits_per_entry: _, palette: _, ref mut data} = self.block_states else {
                    eprintln!("Block states were changed while setting block {x} {y} {z} to {block_state} from Simple to Indirect");
                    return;
                };
                data[i] = 1 << j;
            },
            &mut Indirect { ref bits_per_entry, ref palette, ref mut data } if palette.contains(&block_state) => {
                let (i, j) = Self::index(x, y, z, *bits_per_entry);
                let Some(index) = palette.iter().position(|x| *x == block_state) else {
                    eprintln!("Block states were changed while setting block {x} {y} {z} to {block_state} from Indirect to Indirect");
                    return;
                };
                data[i] |= (index as u64) << j as u64;
            }
            _ => ()
        }
    }
    fn index(x: u8, y: u8, z: u8, bpe: u8) -> (usize, u8) {
        let mut index = y as u32 * 256 + z as u32 * 16 + x as u32;
        index *= bpe as u32;
        ((index >> 6) as usize, (index & 63) as u8)
    }
}
