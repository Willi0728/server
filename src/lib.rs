pub mod world;

use std::io::{self, Read, Write};
use std::net::TcpStream;

pub trait Serialize {
    fn serialize(&self) -> Vec<u8>;
}

#[derive(Debug, Clone, Copy)]
pub struct VarInt(i32);

pub struct RawPacket {
    pub length: VarInt,
    pub id: VarInt,
    pub data: Vec<u8>,
}

pub enum DecodeResult {
    Ok(VarInt),
    Incomplete(VarInt),
    Err,
}

pub struct ConnReader {
    stream: TcpStream,
    buf: Vec<u8>,
    filled: usize,
}

impl ConnReader {
    pub fn new(stream: TcpStream, capacity: usize) -> Self {
        Self {
            stream,
            buf: vec![0u8; capacity],
            filled: 0,
        }
    }
    pub fn resize(&mut self, new_size: usize) -> Option<()> {
        if new_size < self.filled {
            None
        } else {
            let data = std::mem::replace(&mut self.buf, Vec::with_capacity(new_size));
            self.buf.extend(data);
            Some(())
        }
    }
    pub fn read(&mut self) -> io::Result<usize> {
        let n = self.stream.read(&mut self.buf[self.filled..])?;
        self.filled += n;
        Ok(n)
    }
    pub fn next_packet(&mut self) -> Option<RawPacket> {
        if !Self::has_packet(&self.buf[..self.filled]) {
            return None;
        }
        let (res, read) = RawPacket::next_packet(self.buf[..self.filled].to_vec());
        self.buf.copy_within(read..self.filled, 0);
        self.filled -= read;
        res
    }
    pub fn read_at_least(&mut self, n: usize) -> io::Result<()> {
        while self.filled < n {
            if self.read()? == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
        }
        Ok(())
    }
    pub fn read_one(&mut self) -> std::io::Result<Option<RawPacket>> {
        loop {
            if let Some(p) = self.next_packet() {
                return Ok(Some(p));
            }
            if self.read()? == 0 {
                return Ok(None);
            }
        }
    }
    pub fn get_buffer(&self) -> &[u8] {
        &self.buf[..self.filled]
    }
    pub fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.stream.write_all(data)
    }
    pub fn inner_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }
    fn has_packet(data: &[u8]) -> bool {
        let (read, res) = VarInt::decode_loose(data);
        data.len() - read >= res.value() as usize
        // no 16 bit computers exist today; usize is at least 32 bits. Casting from 32 to >32 bits is safe.
    }
}

impl Write for ConnReader {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner_mut().write(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.inner_mut().write_all(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner_mut().flush()
    }
}

impl VarInt {
    pub fn new(value: i32) -> Self {
        Self(value)
    }

    pub fn value(&self) -> i32 {
        self.0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(5);
        let mut copy = *self;
        loop {
            let byte = (copy.0 & 0x7F) as u8;
            copy.0 = ((copy.0 as u32) >> 7) as i32;
            if copy.0 == 0 {
                result.push(byte);
                break;
            }
            result.push(byte | 0x80);
        }
        result
    }

    /// Decodes a VarInt into a DecodeResult.
    /// If the data follows LEB128 spec and is valid, returns `Ok(VarInt)`.
    /// If the data is incomplete, returns `Incomplete(VarInt)` with the partial value.
    /// If the data is completely invalid (i.e. no bytes), returns `Err`.
    /// If you choose to do this:
    /// ```
    /// match decode(some_bytes) {
    ///     Ok(val) | Incomplete(val) => val,
    ///     Err => 0,
    /// }
    /// ```
    /// This is guranteed to give you the lower 32 bits of the VarInt.
    ///
    /// It will not return an Incomplete if the data fits in an i32, even if there
    /// is more data available.
    pub fn decode(data: &[u8]) -> (usize, DecodeResult) {
        use self::DecodeResult::*;
        let mut collector = 0;
        if data.is_empty() {
            return (0, Err);
        }
        for (i, byte) in data.iter().enumerate() {
            if i == 4 {
                if *byte > 0x0F {
                    return (
                        i + 1,
                        Incomplete(Self(collector | ((*byte & 0x0F) as i32) << (i * 7))),
                    );
                } else {
                    return (i + 1, Ok(Self(collector | (*byte as i32) << (i * 7))));
                }
            }
            if i > 4 {
                return (i + 1, Incomplete(Self(collector)));
            }
            collector |= ((*byte & 0x7F) as i32) << (i * 7);
            if byte & 0x80 == 0 {
                return (i + 1, Ok(Self(collector)));
            }
        }
        (data.len(), Incomplete(Self(collector)))
    }

    /// The loosest possible decoding, completely ignorant of the spec.
    /// Will never fail.
    pub fn decode_loose(data: &[u8]) -> (usize, VarInt) {
        use self::DecodeResult::*;
        let (read, result) = Self::decode(data);
        (
            read,
            match result {
                Ok(v) | Incomplete(v) => v,
                Err => VarInt(0),
            },
        )
    }

    pub fn decode_strict(data: &[u8]) -> (usize, Option<VarInt>) {
        use self::DecodeResult::*;
        let (read, result) = Self::decode(data);
        (
            read,
            match result {
                Ok(v) => Some(v),
                _ => None,
            },
        )
    }
}

impl Serialize for VarInt {
    fn serialize(&self) -> Vec<u8> {
        self.encode()
    }
}

impl Serialize for i32 {
    fn serialize(&self) -> Vec<u8> {
        VarInt::new(*self).encode()
    }
}

impl Serialize for u8 {
    fn serialize(&self) -> Vec<u8> {
        vec![*self]
    }
}

impl<T: Serialize> Serialize for [T] {
    fn serialize(&self) -> Vec<u8> {
        let mut buf = VarInt::new(self.len() as i32).serialize();
        for i in self {
            buf.extend(i.serialize());
        }
        buf
    }
}

impl<T: Serialize, const N: usize> Serialize for [T; N] {
    fn serialize(&self) -> Vec<u8> {
        self.as_slice().serialize()
    }
}

impl<T: Serialize> Serialize for Option<T> {
    fn serialize(&self) -> Vec<u8> {
        match self {
            Some(x) => {
                let mut buf = vec![1];
                buf.extend(x.serialize());
                buf
            }
            None => vec![0],
        }
    }
}

impl<T: Serialize> Serialize for Vec<T> {
    fn serialize(&self) -> Vec<u8> {
        <[T]>::serialize(self)
    }
}

impl Serialize for &str {
    fn serialize(&self) -> Vec<u8> {
        let mut buf = VarInt::new(self.len() as i32).encode();
        buf.extend_from_slice(self.as_bytes());
        buf
    }
}

impl Serialize for bool {
    fn serialize(&self) -> Vec<u8> {
        vec![*self as u8]
    }
}

impl Serialize for String {
    fn serialize(&self) -> Vec<u8> {
        self.as_str().serialize()
    }
}

impl Serialize for (i32, i32, i16) {
    fn serialize(&self) -> Vec<u8> {
        let (x, z, y) = self;
        let pos: i64 =
            ((*x as i64 & 0x3FFFFFF) << 38) | ((*z as i64 & 0x3FFFFFF) << 12) | (*y as i64 & 0xFFF);
        pos.to_be_bytes().to_vec()
    }
}

impl RawPacket {
    pub fn next_packet(data: Vec<u8>) -> (Option<Self>, usize) {
        use self::DecodeResult::*;

        let (len_read, len_res) = VarInt::decode(&data);
        let length = match len_res {
            Ok(v) => v,
            _ => return (None, 0),
        };

        let length: i32 = length.value();
        let length = length as usize;

        if data.len() < len_read + length {
            return (None, 0);
        }

        let body = &data[len_read..len_read + length];

        let (id_read, id_res) = VarInt::decode(body);
        let id = match id_res {
            Ok(v) => v,
            _ => return (None, 0),
        };

        let packet_data = &body[id_read..];

        (
            Some(Self {
                length: VarInt::new(length as i32),
                id,
                data: packet_data.to_vec(),
            }),
            len_read + length,
        )
    }
}

pub struct KnownPack {
    pub namespace: String,
    pub id: String,
    pub version: String,
}

//TODO: later introduce a Registry struct?

pub fn parse_known_packs(data: &[u8]) -> Option<Vec<KnownPack>> {
    let (read, count) = VarInt::decode_loose(data);
    let mut pointer = read;
    let mut packs = Vec::with_capacity(count.value() as usize);
    for _ in 0..count.value() {
        let (namespace, read) = next_string_len(data.get(pointer..)?)?;
        pointer += read as usize;
        let (id, read) = next_string_len(data.get(pointer..)?)?;
        pointer += read as usize;
        let (version, read) = next_string_len(data.get(pointer..)?)?;
        pointer += read as usize;
        packs.push(KnownPack {
            namespace,
            id,
            version,
        });
    }
    Some(packs)
}

pub fn decode_plugin_message(data: &[u8]) -> Option<(String, String)> {
    let (channel, read) = next_string_len(data)?;
    Some((channel, next_string(data.get(read as usize..)?)?))
}

pub fn create_known_packs<W: Write>(packs: Vec<KnownPack>, buf: &mut W) -> io::Result<()> {
    buf.write_all(&VarInt::new(packs.len() as i32).encode())?;
    for pack in packs {
        buf.write_all(&pack.namespace.serialize())?;
        buf.write_all(&pack.id.serialize())?;
        buf.write_all(&pack.version.serialize())?;
    }
    Ok(())
}

pub fn create_entry(registry: &str, entry: &str) -> Vec<u8> {
    let mut result = registry.serialize(); // Registry ID
    result.push(1); // Prefixed Array (array length is 1)
    let entry = entry.serialize(); // Entry name
    result.extend(entry);
    result.push(0); // meaning false. for the Prefixed Optional NBT
    result
}

pub fn create_registry() -> Option<Vec<u8>> {
    let mut buf = vec![];
    for (reg, item) in vec![
        "minecraft:worldgen/biome",
        "minecraft:chat_type",
        "minecraft:dimension_type",
        "minecraft:damage_type",
    ]
    .into_iter()
    .zip(vec![
        "minecraft:plains",
        "minecraft:chat",
        "minecraft:overworld",
        "minecraft:out_of_world",
    ]) {
        buf.extend(assemble(7, &create_entry(reg, item)));
    }
    Some(buf)
}

pub fn create_tags(_registries: Vec<(String, Vec<(String, Vec<VarInt>)>)>) -> Vec<u8> {
    vec![0x02, 0x0D, 0x00] //TODO actually use the registries to make tags
}

pub fn login_start(data: &[u8]) -> Option<(String, u128)> {
    let (name, read) = next_string_len(data)?;
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(data.get(read as usize..read as usize+16)?);
    let uuid = u128::from_be_bytes(bytes);
    Some((name, uuid))
}

// pub fn create_prefixed_array(arr: Vec<impl Any>) -> Vec<u8> {}
// TODO!!!!

pub fn login_play(
    eid: i32,
    hardcore: bool,
    dimensions: &[String],
    max_players: i32,
    view_distance: i32,
    sim_distance: i32,
    reduced_debug: bool,
    respawn_screen: bool,
    limited_crafting: bool,
    dimension_type: i32,
    dimension_name: String,
    seed_hash: i64,
    gamemode: u8,
    prev_gamemode: i8,
    debug_mode: bool,
    superflat: bool,
    death_location: Option<(String, (i32, i32, i16))>,
    portal_cooldown: i32,
    sea_level: i32,
    secure_chat: bool,
) -> Vec<u8> {
    let mut buf = vec![];
    buf.extend_from_slice(&eid.to_be_bytes());
    buf.extend(hardcore.serialize());
    buf.extend(dimensions.serialize());
    buf.extend(max_players.serialize());
    buf.extend(view_distance.serialize());
    buf.extend(sim_distance.serialize());
    buf.extend(reduced_debug.serialize());
    buf.extend(respawn_screen.serialize());
    buf.extend(limited_crafting.serialize());
    buf.extend(dimension_type.serialize());
    buf.extend(dimension_name.serialize());
    buf.extend(seed_hash.to_be_bytes());
    buf.extend(gamemode.to_be_bytes());
    buf.extend(prev_gamemode.to_be_bytes());
    buf.extend(debug_mode.serialize());
    buf.extend(superflat.serialize());
    buf.extend(death_location.is_some().serialize());
    if let Some((death_dim_name, pos)) = death_location {
        buf.extend(death_dim_name.serialize());
        buf.extend(pos.serialize());
    }
    buf.extend(portal_cooldown.serialize());
    buf.extend(sea_level.serialize());
    buf.extend(secure_chat.serialize());
    buf
}

pub fn game_event(event: u8, value: f32) -> Vec<u8> {
    let mut buf = vec![event];
    buf.extend(value.to_be_bytes());
    buf
}

pub const INVUNERABLE: u8 = 0x01;
pub const FLYING: u8 = 0x02;
pub const ALLOW_FLYING: u8 = 0x04;
pub const CREATIVE: u8 = 0x08;

pub fn player_abilities(flags: u8, flying_speed: f32, fov_modifier: f32) -> Vec<u8> {
    let mut buf = vec![flags];
    buf.extend(flying_speed.to_be_bytes());
    buf.extend(fov_modifier.to_be_bytes());
    buf
}

pub fn set_held_slot(slot: i32) -> Vec<u8> {
    slot.serialize()
}

pub fn update_recipes() -> Vec<u8> {
    //TODO accept recipes
    vec![0, 0]
}

pub fn set_entity_metadata(eid: i32) -> Vec<u8> {
    //TODO add real entries
    let mut buf = eid.serialize();
    buf.push(0xFF);
    buf
}

pub fn bundle_delimiter() -> Vec<u8> {
    vec![]
}

pub fn set_chunk_cache_center(x: i32, z: i32) -> Vec<u8> {
    let mut buf = x.serialize();
    buf.extend(z.serialize());
    buf
}

pub fn level_chunk_with_light(x: i32, z: i32) -> Vec<u8> {
    let mut buf = vec![];
    buf.extend(x.to_be_bytes());
    buf.extend(z.to_be_bytes());
    buf.extend(<[i32]>::serialize(&[]));

    let mut section1 = vec![];
    //block states:
    section1.extend(&[0x10, 0x00]); // Number of non-air blocks
    section1.push(0); //Bits Per Entry
    section1.push(1); // block id of stone

    //biomes:
    section1.push(0); // bits per entry
    section1.push(55); // plains? whatever 0 is. maybe void

    let mut sections = vec![];
    sections.extend(&[0x00, 0x00]); // non air blocks
    sections.push(0); // bits per entry
    sections.push(0); // air
    sections.push(0); // bpe
    sections.push(55); // whatever biome it is

    buf.extend((section1.len() as i32 + (sections.len() as i32) * 23).serialize());
    buf.extend(section1.repeat(2));
    buf.extend(sections.repeat(22)); // 24 sections total

    let block_entities: Vec<VarInt> = vec![];
    // we'll figure out the impl Seralize details for (u8, i16, VarInt, NBT). rn it's just no block entities.
    // TODO we just need it to compile for now
    buf.extend(block_entities.serialize());

    //light data?
    buf.extend(full_bitset(26)); // Sky Light fullbright
    buf.extend(empty_bitset()); // No block light
    buf.extend(empty_bitset()); // confirm that no chunks are without Sky Light
    buf.extend(full_bitset(26)); // no block light
    buf.extend([light_section(); 26].serialize()); // fullbright skylight
    buf.extend(<[i32]>::serialize(&[])); // empty block light
    buf
}

pub fn light_section() -> [u8; 2048] {
    [0xFF; 2048] // TODO is fullbright rn
}

pub fn full_bitset(bits: u32) -> Vec<u8> {
    let mut buf = vec![1];
    buf.extend(((1u64 << bits) - 1).to_be_bytes());
    buf
}

pub fn empty_bitset() -> Vec<u8> {
    vec![0]
}

pub fn set_default_spawn_position(
    dimension_name: String,
    location: (i32, i32, i16),
    yaw: f32,
    pitch: f32,
) -> Vec<u8> {
    let mut buf = dimension_name.serialize();
    buf.extend(location.serialize());
    buf.extend(yaw.to_be_bytes());
    buf.extend(pitch.to_be_bytes());
    buf
}

pub const RELATIVE_X: u32 = 0x0001;
pub const RELATIVE_Y: u32 = 0x0002;
pub const RELATIVE_Z: u32 = 0x0004;
pub const RELATIVE_YAW: u32 = 0x0008;
pub const RELATIVE_PITCH: u32 = 0x0010;
pub const RELATIVE_VELOCITY_X: u32 = 0x0020;
pub const RELATIVE_VELOCITY_Y: u32 = 0x0040;
pub const RELATIVE_VELOCITY_Z: u32 = 0x0080;
pub const ROTATE_VELOCITY: u32 = 0x0100;
pub fn player_position(
    tid: i32,
    x: f64,
    y: f64,
    z: f64,
    vx: f64,
    vy: f64,
    vz: f64,
    yaw: f32,
    pitch: f32,
    flags: u32,
) -> Vec<u8> {
    let mut buf = tid.serialize();
    buf.extend(x.to_be_bytes());
    buf.extend(y.to_be_bytes());
    buf.extend(z.to_be_bytes());
    buf.extend(vx.to_be_bytes());
    buf.extend(vy.to_be_bytes());
    buf.extend(vz.to_be_bytes());
    buf.extend(yaw.to_be_bytes());
    buf.extend(pitch.to_be_bytes());
    buf.extend(flags.to_be_bytes());
    buf
}

pub fn keep_alive(id: i64) -> Vec<u8> {
    id.to_be_bytes().to_vec()
}

pub fn consume_bytes<'a>(data: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if n > data.len() {
        return None;
    }
    let (prefix, remainder) = data.split_at(n);
    *data = remainder;
    Some(prefix)
}

pub fn next_string(data: &[u8]) -> Option<String> {
    match next_string_len(data) {
        Some((s, _)) => Some(s),
        None => None,
    }
}

pub fn next_string_len(data: &[u8]) -> Option<(String, i32)> {
    let (read, strlen) = VarInt::decode_loose(data);
    let strlen = strlen.value();
    let string = String::from_utf8(data.get(read..read + strlen as usize)?.to_vec()).ok()?;
    Some((string, read as i32 + strlen))
}

pub fn assemble_string(string: &str) -> Option<Vec<u8>> {
    let mut strlen = VarInt::new(string.len().try_into().ok()?).encode();
    strlen.extend_from_slice(string.as_bytes());
    Some(strlen)
}

pub fn read_until_packet<'a>(
    stream: &mut TcpStream,
    buf: &'a mut [u8],
    leftovers: usize,
) -> Option<(RawPacket, usize)> {
    let mut n = leftovers;
    while let (None, _) = RawPacket::next_packet(buf[..n].to_vec()) {
        let read = stream.read(&mut buf[n..]).ok()?;
        if read == 0 {
            return None;
        }
        n += read;
    }
    let (packet, consumed) = RawPacket::next_packet(buf.to_vec());
    Some((packet?, n - consumed))
}

pub fn assemble(id: i32, data: &[u8]) -> Vec<u8> {
    let id = VarInt::new(id).encode();
    let len = VarInt::new(
        (id.len() + data.len())
            .try_into()
            .expect("the length of this packet was too big"),
    )
    .encode();
    let mut result = Vec::with_capacity(len.len() + id.len() + data.len());
    result.extend(len);
    result.extend(id);
    result.extend_from_slice(data);
    result
}

#[cfg(test)]
mod tests {
    // use super::*;

    #[test]
    fn placeholder() {}
}
