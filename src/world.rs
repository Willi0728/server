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
        if x > 15 || y > 15 || z > 15 {
            return;
        }

        let entry_index = Self::entry_index(x, y, z);
        self.block_states =
            match std::mem::replace(&mut self.block_states, PalettedContainer::Single(0)) {
                PalettedContainer::Single(state) if block_state == state => {
                    PalettedContainer::Single(state)
                }
                PalettedContainer::Single(state) => {
                    let bits_per_entry = 1;
                    let mut data = vec![0; Self::packed_word_count(bits_per_entry)];
                    Self::set_packed_entry(&mut data, entry_index, bits_per_entry, 1);
                    PalettedContainer::Indirect {
                        bits_per_entry,
                        palette: vec![state, block_state],
                        data,
                    }
                }
                PalettedContainer::Indirect {
                    mut bits_per_entry,
                    mut palette,
                    mut data,
                } => {
                    let palette_index = match palette.iter().position(|&state| state == block_state)
                    {
                        Some(index) => index,
                        None => {
                            let next_index = palette.len();
                            let required_bits = Self::bits_required_for_palette_len(next_index + 1);
                            if required_bits > bits_per_entry {
                                data =
                                    Self::repack_packed_data(&data, bits_per_entry, required_bits);
                                bits_per_entry = required_bits;
                            }
                            palette.push(block_state);
                            next_index
                        }
                    };

                    Self::set_packed_entry(
                        &mut data,
                        entry_index,
                        bits_per_entry,
                        palette_index as u64,
                    );
                    PalettedContainer::Indirect {
                        bits_per_entry,
                        palette,
                        data,
                    }
                }
                PalettedContainer::Direct(mut data) => {
                    data[entry_index] = block_state;
                    PalettedContainer::Direct(data)
                }
            };
    }

    pub fn get_block(&self, x: u8, y: u8, z: u8) -> u16 {
        let entry_index = Self::entry_index(x, y, z);
        match &self.block_states {
            PalettedContainer::Single(s) => *s,
            PalettedContainer::Indirect {
                bits_per_entry,
                palette,
                data,
            } => palette[Self::packed_entry(data, entry_index, *bits_per_entry) as usize],
            PalettedContainer::Direct(data) => data[entry_index],
        }
    }

    pub fn fill(&mut self, (x1, y1, z1): (u8, u8, u8), (x2, y2, z2): (u8, u8, u8), block_state: u16) {
        for x in x1..x2 {
            for y in y1..y2 {
                for z in z1..z2 {
                    self.set_block(x, y, z, block_state);
                }
            }
        }
    }

    pub fn fill_all(&mut self, block_state: u16) {
        self.block_states = PalettedContainer::Single(block_state);
    }

    fn set_packed_entry(data: &mut [u64], entry_index: usize, bits_per_entry: u8, value: u64) {
        let (word_index, bit_offset) = Self::bit_index(entry_index, bits_per_entry);
        let mask = Self::entry_mask(bits_per_entry);
        let value = value & mask;

        data[word_index] &= !(mask << bit_offset);
        data[word_index] |= value << bit_offset;
    }

    fn packed_entry(data: &[u64], entry_index: usize, bits_per_entry: u8) -> u64 {
        let (word_index, bit_offset) = Self::bit_index(entry_index, bits_per_entry);
        let mask = Self::entry_mask(bits_per_entry);
        (data[word_index] >> bit_offset) & mask
    }

    fn repack_packed_data(
        data: &[u64],
        old_bits_per_entry: u8,
        new_bits_per_entry: u8,
    ) -> Vec<u64> {
        let mut repacked = vec![0; Self::packed_word_count(new_bits_per_entry)];
        for entry_index in 0..4096 {
            let value = Self::packed_entry(data, entry_index, old_bits_per_entry);
            Self::set_packed_entry(&mut repacked, entry_index, new_bits_per_entry, value);
        }
        repacked
    }

    fn bit_index(entry_index: usize, bits_per_entry: u8) -> (usize, u8) {
        let entries_per_word = Self::entries_per_word(bits_per_entry);
        let word_index = entry_index / entries_per_word;
        let bit_offset = ((entry_index % entries_per_word) * bits_per_entry as usize) as u8;
        (word_index, bit_offset)
    }

    fn entry_index(x: u8, y: u8, z: u8) -> usize {
        y as usize * 256 + z as usize * 16 + x as usize
    }

    fn packed_word_count(bits_per_entry: u8) -> usize {
        4096usize.div_ceil(Self::entries_per_word(bits_per_entry))
    }

    fn entries_per_word(bits_per_entry: u8) -> usize {
        64 / bits_per_entry as usize
    }

    fn bits_required_for_palette_len(palette_len: usize) -> u8 {
        debug_assert!(palette_len >= 2);
        usize::BITS.saturating_sub((palette_len - 1).leading_zeros()) as u8
    }

    fn entry_mask(bits_per_entry: u8) -> u64 {
        debug_assert!(bits_per_entry > 0 && bits_per_entry <= 64);
        if bits_per_entry == 64 {
            u64::MAX
        } else {
            (1u64 << bits_per_entry) - 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ChunkSection, PalettedContainer};

    #[test]
    fn grows_palette_and_repacks_existing_entries() {
        let mut section = ChunkSection::new();
        section.set_block(0, 0, 0, 1);
        section.set_block(1, 0, 0, 2);
        section.set_block(2, 0, 0, 3);

        let PalettedContainer::Indirect {
            bits_per_entry,
            palette,
            data,
        } = &section.block_states
        else {
            panic!("expected indirect block states");
        };

        assert_eq!(*bits_per_entry, 2);
        assert_eq!(palette, &vec![0, 1, 2, 3]);
        assert_eq!(
            ChunkSection::packed_entry(data, ChunkSection::entry_index(0, 0, 0), *bits_per_entry),
            1
        );
        assert_eq!(
            ChunkSection::packed_entry(data, ChunkSection::entry_index(1, 0, 0), *bits_per_entry),
            2
        );
        assert_eq!(
            ChunkSection::packed_entry(data, ChunkSection::entry_index(2, 0, 0), *bits_per_entry),
            3
        );
    }

    #[test]
    fn packed_entries_use_padded_word_boundaries() {
        let mut data = vec![0; ChunkSection::packed_word_count(5)];
        let last_in_first_word = 11;
        let first_in_second_word = 12;

        ChunkSection::set_packed_entry(&mut data, last_in_first_word, 5, 0b0_1010);
        ChunkSection::set_packed_entry(&mut data, first_in_second_word, 5, 0b1_1010);

        assert_eq!(
            ChunkSection::packed_entry(&data, last_in_first_word, 5),
            0b0_1010
        );
        assert_eq!(
            ChunkSection::packed_entry(&data, first_in_second_word, 5),
            0b1_1010
        );
        assert_eq!(data[0] >> 60, 0);
    }
}
