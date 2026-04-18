pub mod world;

use bytes::{Buf, Bytes, BytesMut};
use std::io::{self, Write};
use std::marker::Unpin;
use std::num::NonZeroU32;
use tokio::io::AsyncWriteExt;
use tokio_util::codec::{Decoder, Encoder};

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

pub struct MCCodec;

impl Decoder for MCCodec {
    type Item = RawPacket;
    type Error = io::Error;
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // check if enough bytes for VarInt?
        let (read, len) = VarInt::decode(src);
        let len = match len {
            DecodeResult::Err | DecodeResult::Incomplete(_) => return Ok(None),
            DecodeResult::Ok(n) => n,
        };
        if src.len() < read + len.value() as usize {
            return Ok(None);
        }
        ////////////// we don't use return Ok(None) after here ////////////////////////////////////
        src.advance(read); // advance length of packet length
        let (read, id) = VarInt::decode(src);
        src.advance(read); // advance packet ID
        let id = match id {
            DecodeResult::Err => VarInt::new(0),
            DecodeResult::Ok(n) => n,
            DecodeResult::Incomplete(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing packet ID",
                ));
            }
        };
        Ok(Some(RawPacket {
            length: len,
            id,
            data: src.split_to(len.value() as usize - read).to_vec(),
        })) //TODO make RawPacket store BytesMut
    }
}

impl Encoder<(i32, Bytes)> for MCCodec {
    type Error = io::Error;
    fn encode(&mut self, (id, data): (i32, Bytes), dst: &mut BytesMut) -> Result<(), Self::Error> {
        let id_varint = id.serialize();
        dst.extend(((data.len() + id_varint.len()) as i32).serialize()); //TODO make a serialize_into or something
        dst.extend(id_varint); //TODO i really don't want to do this now
        dst.extend(data); //TODO so just do it later
        Ok(())
    }
}

impl VarInt {
    #[inline]
    pub fn new(value: i32) -> Self {
        Self(value)
    }

    #[inline]
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
    /// use server::{DecodeResult, VarInt};
    ///
    /// let decoded = match VarInt::decode(&[0x80]) {
    ///     (_, DecodeResult::Ok(val)) | (_, DecodeResult::Incomplete(val)) => val,
    ///     (_, DecodeResult::Err) => VarInt::new(0),
    /// };
    ///
    /// assert_eq!(decoded.value(), 0);
    /// ```
    /// This is guranteed to give you the lower 32 bits of the VarInt.
    ///
    /// It will not return an Incomplete if the data fits in an i32, even if there
    /// is more data available.
    ///
    /// Ok(n): the VarInt was decoded successfully
    /// Incomplete(n): The last bit was a continuation bit, and we need more data
    /// Err: The buffer was empty
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
    bytes.copy_from_slice(data.get(read as usize..read as usize + 16)?);
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

pub async fn keep_alive_with_packet_id(
    id: i64,
    to: &mut (impl AsyncWriteExt + Unpin),
) -> io::Result<()> {
    let mut buf = [0u8; 10];
    buf[0] = 0x09;
    buf[1] = 0x2b;
    buf[2..].copy_from_slice(&id.to_be_bytes());
    to.write_all(&buf).await
}

pub fn decode_player_action(data: &[u8]) -> Result<(i32, (i32, i16, i32), u8, i32), String> {
    let (read, status) = VarInt::decode_strict(data);
    let status = status.ok_or_else(|| "VarInt too big")?;
    let position_long = i64::from_be_bytes(
        data.get(read..read + 8)
            .ok_or_else(|| packet_too_short("player_action"))?
            .try_into()
            .ok()
            .ok_or_else(|| "Unknown error")?,
    );
    let x = position_long >> 38;
    let y = position_long << 52 >> 52;
    let z = position_long << 26 >> 38;
    let face = *data.get(read + 8).ok_or_else(|| "Unknown error")?;
    let (_, seq) = VarInt::decode_strict(
        data.get(read + 9..)
            .ok_or_else(|| packet_too_short("player_action"))?,
    );
    let seq = seq.ok_or_else(|| "VarInt too big")?;
    Ok((
        status.value(),
        (x as i32, y as i16, z as i32),
        face,
        seq.value(),
    ))
}

pub fn decode_use_item_on(
    data: &[u8],
) -> Result<(i32, (i32, i16, i32), i32, f32, f32, f32, bool, bool, i32), String> {
    let (read, hand) = VarInt::decode_strict(data);
    let hand = hand.ok_or_else(|| packet_too_short("use_item_on"))?;
    let (x, y, z) = next_position(data, read, "use_item_on")?;
    let (read_temp, face) = VarInt::decode_strict(
        data.get(read + 8..)
            .ok_or_else(|| packet_too_short("use_item_on"))?,
    );
    let read = read + 8 + read_temp;
    let face = face.ok_or_else(|| packet_too_short("use_item_on"))?;
    fn decode_float(data: &[u8], cursor: usize) -> Result<f32, String> {
        Ok(f32::from_be_bytes(
            data.get(cursor..cursor + 4)
                .ok_or_else(|| packet_too_short("use_item_on"))?
                .try_into()
                .map_err(|_| "Unknown error")?,
        ))
    }
    let curposx = decode_float(data, read)?;
    let curposy = decode_float(data, read + 4)?;
    let curposz = decode_float(data, read + 8)?;
    let inside = *data
        .get(read + 12)
        .ok_or_else(|| packet_too_short("use_item_on"))?
        != 0;
    let border_hit = *data
        .get(read + 13)
        .ok_or_else(|| packet_too_short("use_item_on"))?
        != 0;
    let (_, seq) = VarInt::decode_strict(
        data.get(read + 14..)
            .ok_or_else(|| packet_too_short("use_item_on"))?,
    );
    let seq = seq.ok_or_else(|| "VarInt too big")?;
    Ok((
        hand.value(),
        (x, y, z),
        face.value(),
        curposx,
        curposy,
        curposz,
        inside,
        border_hit,
        seq.value(),
    ))
}

pub struct SlotFormat {
    pub count: NonZeroU32,
    pub id: i32,
    pub components_to_add: Vec<(i32, Vec<u8>)>,
    pub components_to_remove: Vec<i32>,
}

pub fn decode_slot(data: &[u8], packet: &str) -> Result<Option<SlotFormat>, String> {
    let more_bytes = packet_too_short(packet);
    let (read, count) = VarInt::decode_strict(data);
    let mut cursor = read;
    let count = count.ok_or("VarInt too big")?.value() as u32;
    if count == 0 {
        return Ok(None);
    }
    let (read, id) = VarInt::decode_strict(data.get(cursor..).ok_or(&more_bytes)?);
    cursor += read;
    let id = id.ok_or("VarInt too big")?;
    let (read, num_add) = VarInt::decode_strict(data.get(cursor..).ok_or(&more_bytes)?);
    cursor += read;
    let _num_add = num_add.ok_or("VarInt too big")?;
    let (_read, num_remove) = VarInt::decode_strict(data.get(cursor..).ok_or(&more_bytes)?);
    // cursor += read;
    let _num_remove = num_remove.ok_or("VarInt too big")?;
    let add: Vec<(i32, Vec<u8>)> = vec![];
    // for _ in 0..num_add.value() {
    //     let (read, component_type) = VarInt::decode_strict(data.get(cursor..).ok_or(&more_bytes)?);
    //     cursor += read;
    //     let component_type = component_type.ok_or("VarInt too big")?;
    //     //TODO determine lengths and decode components
    // }
    //TODO do the remove
    let remove: Vec<i32> = vec![];
    Ok(Some(SlotFormat {
        count: NonZeroU32::new(count).unwrap(),
        id: id.value(),
        components_to_add: add,
        components_to_remove: remove,
    }))
}

pub fn decode_set_creative_mode_slot(data: &[u8]) -> Result<(i16, Option<SlotFormat>), String> {
    let more_bytes = packet_too_short("set_creative_mode_slot");
    Ok((
        i16::from_be_bytes(
            data.get(0..2)
                .ok_or(&more_bytes)?
                .try_into()
                .map_err(|_| "Unknown error")?,
        ),
        decode_slot(data.get(2..).ok_or(more_bytes)?, "set_creative_mode_slot")?,
    ))
}

pub fn packet_too_short(packet: &str) -> String {
    format!("Failed to decode packet serverbound/minecraft:{packet}: Not enough bytes in buffer")
        .to_string()
}

pub fn next_position(data: &[u8], cursor: usize, packet: &str) -> Result<(i32, i16, i32), String> {
    let position_long = i64::from_be_bytes(
        data.get(cursor..cursor + 8)
            .ok_or_else(|| packet_too_short(packet).to_string())?
            .try_into()
            .map_err(|_| "Unknown error".to_string())?,
    );
    let x = position_long >> 38;
    let y = position_long << 52 >> 52;
    let z = position_long << 26 >> 38;
    Ok((x as i32, y as i16, z as i32))
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
