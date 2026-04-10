use rand::Rng;

use crate::*;
use std::collections::HashMap;
use std::num::NonZeroU64;

const fn generate_arrays() -> [[u8; 2048]; 16] {
    let mut out = [[0u8; 2048]; 16];
    let mut v = 0;
    while v < 16 {
        out[v] = [((v as u8) << 4) | (v as u8); 2048];
        v += 1;
    }
    out
}

const LIGHT_SECTION: [[u8; 2048]; 16] = generate_arrays();

pub enum TaskSchedule {
    EveryTick,
    EveryN(NonZeroU64),
    Once(u64),
}

pub trait TaskClosure: FnMut(&mut Level) + Send + Sync + 'static {}
impl<T> TaskClosure for T where T: FnMut(&mut Level) + Send + Sync + 'static {}

pub type Task = (Box<dyn FnMut(&mut Level) + Send + Sync>, TaskSchedule);

pub struct Level {
    tasks: Vec<Option<Task>>,
    time: u64,
    chunks: HashMap<(i32, i32), LevelChunk>,
    seed: i64,
}

#[derive(Debug)]
pub struct LevelChunk {
    sections: [ChunkSection; 24],
    skylight: [LightSection; 26],
    blocklight: [LightSection; 26],
}

#[derive(Debug)]
pub enum LightSection {
    Single(u8),
    Direct(Box<[u8; 2048]>),
}

#[derive(Debug)]
pub struct ChunkSection {
    block_count: u16,
    block_states: BlockPalettedContainer,
    biomes: BiomePalettedContainer,
}

#[derive(Debug)]
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

impl Serialize for BlockPalettedContainer {
    fn serialize(&self) -> Vec<u8> {
        match self {
            Self::Single(n) => serialize_single_paletted(*n),
            Self::Indirect {
                bits_per_entry,
                palette,
                data,
            } => serialize_block_indirect_paletted(*bits_per_entry, palette, data),
            Self::Direct(data) => serialize_direct_paletted(
                15,
                &data.iter().map(|&entry| entry as u64).collect::<Vec<_>>(),
            ),
        }
    }
}

impl Serialize for BiomePalettedContainer {
    fn serialize(&self) -> Vec<u8> {
        match self {
            Self::Single(n) => serialize_single_paletted(*n),
            Self::Indirect {
                bits_per_entry,
                palette,
                data,
            } => serialize_indirect_paletted(
                (*bits_per_entry).max(1),
                &palette
                    .iter()
                    .map(|&entry| VarInt::new(entry as i32))
                    .collect::<Vec<_>>(),
                data,
            ),
            Self::Direct(data) => {
                let bits_per_entry = bits_required_for_value_range(
                    data.iter().copied().map(u64::from).max().unwrap_or(0),
                );
                serialize_direct_paletted(
                    bits_per_entry,
                    &data.iter().map(|&entry| entry as u64).collect::<Vec<_>>(),
                )
            }
        }
    }
}

fn serialize_single_paletted<T: Into<i32>>(value: T) -> Vec<u8> {
    let mut out = vec![0];
    out.extend(VarInt::new(value.into()).serialize());
    out
}

fn serialize_block_indirect_paletted(bits_per_entry: u8, palette: &[u16], data: &[u64]) -> Vec<u8> {
    let wire_bits_per_entry = bits_per_entry.max(4);
    let wire_data = if wire_bits_per_entry == bits_per_entry {
        data.to_vec()
    } else {
        ChunkSection::repack_packed_data(data, bits_per_entry, wire_bits_per_entry)
    };

    serialize_indirect_paletted(
        wire_bits_per_entry,
        &palette
            .iter()
            .map(|&entry| VarInt::new(entry as i32))
            .collect::<Vec<_>>(),
        &wire_data,
    )
}

fn serialize_indirect_paletted(bits_per_entry: u8, palette: &[VarInt], data: &[u64]) -> Vec<u8> {
    let mut out = vec![bits_per_entry];
    out.extend(palette.serialize());
    out.extend(serialize_fixed_long_array(data));
    out
}

fn serialize_direct_paletted(bits_per_entry: u8, values: &[u64]) -> Vec<u8> {
    let mut out = vec![bits_per_entry];
    out.extend(serialize_fixed_long_array(&pack_entries(
        values,
        bits_per_entry,
    )));
    out
}

fn serialize_fixed_long_array(data: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * size_of::<u64>());
    for word in data {
        out.extend_from_slice(&word.to_be_bytes());
    }
    out
}

fn pack_entries(values: &[u64], bits_per_entry: u8) -> Vec<u64> {
    let entries_per_word = 64 / bits_per_entry as usize;
    let mut out = vec![0; values.len().div_ceil(entries_per_word)];
    let mask = entry_mask(bits_per_entry);

    for (entry_index, value) in values.iter().copied().enumerate() {
        let word_index = entry_index / entries_per_word;
        let bit_offset = ((entry_index % entries_per_word) * bits_per_entry as usize) as u8;
        out[word_index] |= (value & mask) << bit_offset;
    }

    out
}

fn bits_required_for_value_range(max: u64) -> u8 {
    (u64::BITS.saturating_sub(max.leading_zeros())).max(1) as u8
}

fn entry_mask(bits_per_entry: u8) -> u64 {
    debug_assert!(bits_per_entry > 0 && bits_per_entry <= 64);
    if bits_per_entry == 64 {
        u64::MAX
    } else {
        (1u64 << bits_per_entry) - 1
    }
}

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

    pub fn get_or_create_chunk(
        &mut self,
        (x, z): (i32, i32),
        generator: &mut dyn FnMut(i32, i32, i64) -> LevelChunk,
    ) -> &LevelChunk {
        self.chunks
            .entry((x, z))
            .or_insert_with(|| generator(x, z, self.seed))
    }

    pub fn new(seed: Option<i64>) -> Self {
        Self {
            tasks: vec![],
            time: 0,
            chunks: HashMap::new(),
            seed: if let Some(seed) = seed {
                seed
            } else {
                rand::rng().next_u64() as i64
            },
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

    pub fn serialize(&self, x: i32, z: i32) -> Option<Vec<u8>> {
        Some(self.chunks.get(&(x, z))?.serialize(x, z))
    }

    pub fn get_chunk(&self, x: i32, z: i32) -> Option<&LevelChunk> {
        self.chunks.get(&(x, z))
    }

    pub fn get_chunk_mut(&mut self, x: i32, z: i32) -> Option<&mut LevelChunk> {
        self.chunks.get_mut(&(x, z))
    }
}

impl LevelChunk {
    const LIGHT_SECTION_COUNT: usize = 26;
    const LIGHT_SECTION_MASK: u64 = (1u64 << Self::LIGHT_SECTION_COUNT) - 1;

    pub fn new() -> Self {
        use std::array::from_fn;
        Self {
            sections: from_fn(|_| ChunkSection::new()),
            skylight: from_fn(|_| LightSection::new()),
            blocklight: from_fn(|_| LightSection::new()),
        }
    }

    pub fn set_block(&mut self, x: u8, y: i32, z: u8, block_state: u16) {
        if y >= 320 || y < -64 {
            return;
        }
        let section_index = ((y + 64) / 16) as usize;
        let section_y = y.rem_euclid(16) as u8;
        self.sections[section_index].set_block(x, section_y, z, block_state)
    }

    pub fn fill(
        &mut self,
        (x1, y1, z1): (u8, i32, u8),
        (x2, y2, z2): (u8, i32, u8),
        block_state: u16,
    ) {
        for x in x1..x2 {
            for y in y1..y2 {
                for z in z1..z2 {
                    self.set_block(x, y, z, block_state);
                }
            }
        }
    }

    fn light_mask(light: &[LightSection; 26]) -> u64 {
        let mut out = 0u64;
        for (i, section) in light.iter().enumerate() {
            if !matches!(section, LightSection::Single(0)) {
                out |= 1 << i;
            }
        }
        out
    }

    fn write_bitset(to: &mut Vec<u8>, mask: u64) {
        if mask == 0 {
            to.extend(0i32.serialize());
            return;
        }

        to.extend(1i32.serialize());
        to.extend_from_slice(&mask.to_be_bytes());
    }

    fn write_light_sections(light: &[LightSection; 26], to: &mut Vec<u8>) {
        let present_count = light
            .iter()
            .filter(|section| !matches!(section, LightSection::Single(0)))
            .count();
        to.extend((present_count as i32).serialize());
        for section in light {
            if !matches!(section, LightSection::Single(0)) {
                to.extend(2048i32.serialize());
                section.write_to(to).expect("Vec<u8> writes do not fail");
            }
        }
    }

    pub fn serialize(&self, x: i32, z: i32) -> Vec<u8> {
        let mut out = vec![];
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        out.extend(<[i32]>::serialize(&[])); // heightmaps
        // byte[]
        let mut data: Vec<u8> = vec![];
        for i in &self.sections {
            data.extend(i.block_count.to_be_bytes());
            data.extend(i.block_states.serialize());
            data.extend(i.biomes.serialize());
        }
        out.extend((data.len() as i32).serialize()); // no way chunk data is bigger than i32::MAX bytes...right?
        out.extend(data);
        out.extend(<[i32]>::serialize(&[])); // no block entities yet
        // Bitset  set 1
        let skyset = Self::light_mask(&self.skylight);
        let blockset = Self::light_mask(&self.blocklight);
        let empty_skyset = skyset ^ Self::LIGHT_SECTION_MASK;
        let empty_blockset = blockset ^ Self::LIGHT_SECTION_MASK;
        Self::write_bitset(&mut out, skyset);
        Self::write_bitset(&mut out, blockset);
        Self::write_bitset(&mut out, empty_skyset);
        Self::write_bitset(&mut out, empty_blockset);
        Self::write_light_sections(&self.skylight, &mut out);
        Self::write_light_sections(&self.blocklight, &mut out);
        out
    }
}

impl LightSection {
    pub fn new() -> Self {
        Self::Single(0)
    }

    fn write_to<W: Write>(&self, to: &mut W) -> io::Result<()> {
        to.write_all(match self {
            Self::Single(n) => &LIGHT_SECTION[*n as usize],
            Self::Direct(a) => &**a,
        })
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

        let previous_block_state = self.get_block(x, y, z);
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

        match (previous_block_state == 0, block_state == 0) {
            (true, false) => self.block_count += 1,
            (false, true) => self.block_count -= 1,
            _ => (),
        }
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

    pub fn fill(
        &mut self,
        (x1, y1, z1): (u8, u8, u8),
        (x2, y2, z2): (u8, u8, u8),
        block_state: u16,
    ) {
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
        self.block_count = if block_state == 0 { 0 } else { 4096 };
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
        entry_mask(bits_per_entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
