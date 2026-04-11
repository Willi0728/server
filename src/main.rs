use indoc::indoc;
use rand::Rng;
use serde::Deserialize;
use server::world;
use server::world::*;
use server::*;
use sha2::{Digest, Sha256};
use std::io;
use std::ops::Deref;
use std::sync::RwLock;
use std::sync::atomic::{AtomicI32, Ordering};
use std::{
    fs,
    io::ErrorKind,
    net::{IpAddr, SocketAddr, TcpListener},
    sync::Arc,
    thread,
    time::Instant,
};
use toml;
use tracing::{Level, event, span};

const DISCONNECT: &[u8] = &[
    0x21, 0x00, 0x1F, 0x7B, 0x22, 0x74, 0x65, 0x78, 0x74, 0x22, 0x3A, 0x20, 0x22, 0x54, 0x68, 0x65,
    0x20, 0x73, 0x65, 0x72, 0x76, 0x65, 0x72, 0x20, 0x69, 0x73, 0x20, 0x66, 0x75, 0x6C, 0x6C, 0x21,
    0x22, 0x7D,
];
const LEGACY: &[u8] = &[
    0xFF, 0x00, 0x21, 0x00, 0xA7, 0x00, 0x31, 0x00, 0x00, 0x00, 0x31, 0x00, 0x32, 0x00, 0x37, 0x00,
    0x00, 0x00, 0x31, 0x00, 0x2E, 0x00, 0x37, 0x00, 0x2E, 0x00, 0x31, 0x00, 0x30, 0x00, 0x00, 0x00,
    0x48, 0x00, 0x65, 0x00, 0x6C, 0x00, 0x6C, 0x00, 0x6F, 0x00, 0x20, 0x00, 0x66, 0x00, 0x72, 0x00,
    0x6F, 0x00, 0x6D, 0x00, 0x20, 0x00, 0x52, 0x00, 0x75, 0x00, 0x73, 0x00, 0x74, 0x00, 0x21, 0x00,
    0x00, 0x00, 0x30, 0x00, 0x00, 0x00, 0x31, 0x00, 0x30,
];
const CONFIG_FILE: &str = "config.toml";
const BASE_DATA: &[u8] = indoc! {r#"
    [status]
    protocol_version = 774
    version_string = "1.21.11"
    echo_version = false
    motd = "A Minecraft Server"
    [server]
    bind = "0.0.0.0" # Any IPv4 or IPv6 number
    port = 25565  # Valid values: any number 0-65536, preferably 1024-49151
    max_players = 20
    buffer_size = 65536
    log_level = "INFO" # Valid values: TRACE, DEBUG, INFO, WARN, ERROR
    [world]
    hardcore = false
    view_distance = 10
    simulation_distance = 6
    reduced_debug_info = false
    respawn_screen = true
    limited_crafting = false
    seed = 0
    default_gamemode = 1 # Values: 0 (Survival), 1 (Creative), 2 (Adventure), 3 (Spectator)
    debug = false
    superflat = true
    portal_cooldown = 300
    sea_level = 64
    enforce_secure_chat = false
 "#}
.as_bytes();
const REGISTRY_DATA: &[u8] = include_bytes!("registry_data.bin");
const UPDATE_TAGS: &[u8] = include_bytes!("update_tags.bin");

#[derive(Deserialize)]
struct Config {
    status: StatusConfig,
    server: ServerConfig,
    world: WorldConfig,
}

#[derive(Deserialize)]
struct WorldConfig {
    hardcore: bool,
    view_distance: i32,
    simulation_distance: i32,
    reduced_debug_info: bool,
    respawn_screen: bool,
    limited_crafting: bool,
    #[serde(default)]
    seed: i64,
    default_gamemode: u8,
    debug: bool,
    superflat: bool,
    portal_cooldown: i32,
    sea_level: i32,
    enforce_secure_chat: bool,
}

#[derive(Deserialize)]
struct StatusConfig {
    protocol_version: i32,
    version_string: String,
    echo_version: bool,
    motd: String,
}

#[derive(Deserialize)]
struct ServerConfig {
    bind: IpAddr,
    port: u16,
    max_players: i32,
    buffer_size: usize,
    log_level: String,
}

struct PlayerContext {
    name: String,
    uuid: u128,
    entity_id: i32,
    peer_addr: SocketAddr,
    connected_addr: String,
    connected_port: u16,
    connected_at: Instant,
}

struct ThreadsGuard<'a> {
    threads: &'a AtomicI32,
}

impl Drop for ThreadsGuard<'_> {
    fn drop(&mut self) {
        self.threads.fetch_sub(1, Ordering::AcqRel);
    }
}

impl<'a> Deref for ThreadsGuard<'a> {
    type Target = AtomicI32;
    fn deref(&self) -> &Self::Target {
        self.threads
    }
}

impl<'a> ThreadsGuard<'a> {
    fn new(threads: &'a AtomicI32) -> (i32, Self) {
        (threads.fetch_add(1, Ordering::AcqRel), Self { threads })
    }
}

fn stage_0(data: &[u8]) -> Option<(VarInt, String, u16, VarInt)> {
    let mut pos = 0;
    let (read, proto) = VarInt::decode_loose(data);
    pos += read;
    let (server_addr, len) = next_string_len(data.get(read..)?)?;
    pos += len as usize; // 32 bits to >=32 bits
    let port = u16::from_be_bytes(data.get(pos..pos + 2)?.try_into().ok()?);
    pos += 2;
    let (_, next) = VarInt::decode_loose(data.get(pos..)?);
    Some((proto, server_addr, port, next))
}

fn login_success(name: &str, uuid: u128, properties: i32) -> Option<Vec<u8>> {
    event!(Level::TRACE, "Login Success");
    let mut buf = uuid.to_be_bytes().to_vec();
    buf.extend(assemble_string(name)?);
    buf.extend(VarInt::new(properties).serialize());
    Some(buf)
}

fn assemble_0(json: &str) -> Option<Vec<u8>> {
    let json_len = VarInt::new(json.len().try_into().ok()?).serialize();
    let packet_len = VarInt::new((json.len() + 1 + json_len.len()).try_into().ok()?).serialize();
    let mut result = Vec::with_capacity(packet_len.len() + 1 + json_len.len() + json.len());
    result.extend(packet_len);
    result.push(0);
    result.extend(json_len);
    result.extend_from_slice(json.as_bytes());
    Some(result)
}

fn handle_play(
    mut conn: ConnReader,
    settings: Arc<Config>,
    ctx: PlayerContext,
    level: Arc<RwLock<world::Level>>,
) -> io::Result<()> {
    let _guard = span!(Level::INFO, "play");
    let w = &settings.world;
    let seed_hash = i64::from_be_bytes(
        Sha256::digest(w.seed.to_be_bytes())[..8]
            .try_into()
            .unwrap(),
    );
    //TODO complete set of dimensions
    conn.write_all(&assemble(
        0x30,
        &login_play(
            ctx.entity_id,
            w.hardcore,
            &[
                "minecraft:overworld".into(),
                "minecraft:the_nether".into(),
                "minecraft:the_end".into(),
            ],
            settings.server.max_players,
            w.view_distance,
            w.simulation_distance,
            w.reduced_debug_info,
            w.respawn_screen,
            w.limited_crafting,
            0,
            "minecraft:overworld".into(),
            seed_hash,
            w.default_gamemode,
            -1, //TODO previous gamemode?
            w.debug,
            w.superflat,
            None,
            w.portal_cooldown,
            w.sea_level,
            w.enforce_secure_chat,
        ),
    ))?;
    event!(Level::TRACE, "Sent Login (play)");
    conn.write_all(&assemble(0x26, &game_event(13, 0.0)))?;
    event!(
        Level::TRACE,
        "Sent Game Event 13 (start waiting for level chunks)"
    );
    // spectator mode. TODO adjust bitflags for other modes
    conn.write_all(&assemble(
        0x3E,
        &player_abilities(INVUNERABLE | FLYING | ALLOW_FLYING | CREATIVE, 0.05, 0.1),
    ))?;
    event!(Level::TRACE, "Sent Player Abilities for Creative mode");
    conn.write_all(&assemble(0x67, &set_held_slot(0)))?;
    event!(Level::TRACE, "Set player slot to 0");
    conn.write_all(&assemble(0x83, &update_recipes()))?; // TODO add recipes
    event!(Level::TRACE, "Updated no recipes");
    conn.write_all(&assemble(0x61, &set_entity_metadata(1)))?; // TODO add metadata // TODO also set entity id
    event!(Level::TRACE, "Set no metadata for player");
    conn.write_all(&assemble(0x5C, &set_chunk_cache_center(0, 0)))?;
    event!(Level::TRACE, "set center chunk to 0, 0");
    let r = settings.world.view_distance;
    let mut tempbuf = vec![];
    for (x, z) in (-r..=r).flat_map(|x| (-r..=r).map(move |z| (x, z))) {
        tempbuf.extend(assemble(
            0x2C,
            &level
                .write()
                .unwrap()
                .get_or_create_chunk((x, z), &mut |_x, _z, _seed| {
                    let mut chunk = LevelChunk::new();
                    chunk.fill((0, -64, 0), (16, -32, 16), 1);
                    chunk
                })
                .serialize(x, z),
        ));
        event!(Level::TRACE, "Sent chunk {x}, {z}");
    }
    conn.write_all(&assemble(
        0x5F,
        &set_default_spawn_position("minecraft:overworld".to_owned(), (8, -30, 8), 0.0, 0.0), //TODO make this a config
    ))?;
    conn.write_all(&assemble(
        0x46,
        &player_position(0, 8.0, -30.0, 8.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0),
    ))?;
    event!(Level::TRACE, "Synchronized player position");
    let _ = conn.read_one()?;
    event!(Level::TRACE, "Read Teleport Confirm");

    event!(
        Level::INFO,
        "{} joined in {}",
        ctx.name,
        Instant::now()
            .checked_duration_since(ctx.connected_at)
            .map(|d| format!("{d:?}"))
            .unwrap_or_else(|| "unknown time".to_string())
    );

    conn.write_all(&tempbuf)?;
    loop {
        if let Some(packet) = conn.read_one()? {
            match packet.id.value() {
                0x0C => {
                    keep_alive_with_packet_id(rand::rng().next_u64() as i64, &mut conn)?;
                    event!(Level::TRACE, "Sent Keep Alive");
                }
                0x28 => {
                    let (status, (x, y, z), _face, _sequence) =
                        decode_player_action(&packet.data).map_err(|e| io::Error::other(e))?;
                    match status {
                        0 | 2 => {
                            level
                                .write()
                                .unwrap()
                                .get_chunk_mut(x / 16, z / 16)
                                .ok_or(io::Error::other("Mined block in unloaded chunk"))?
                                .set_block(
                                    x.rem_euclid(16) as u8,
                                    y as i32,
                                    z.rem_euclid(16) as u8,
                                    0,
                                );
                            event!(Level::INFO, "{} mined block at {x} {y} {z}", ctx.name);
                        }
                        id => event!(Level::WARN, "Mining status {id} not implemented yet"),
                    }
                }
                0x3F => {
                    let (hand, (x, y, z), face, cx, cy, cz, inside, hit, seq) =
                        decode_use_item_on(&packet.data).map_err(|s| io::Error::other(s))?;
                    let (bx, by, bz) = match face {
                        0 => (x, y - 1, z),
                        1 => (x, y + 1, z),
                        2 => (x, y, z - 1),
                        3 => (x, y, z + 1),
                        4 => (x - 1, y, z),
                        5 => (x + 1, y, z),
                        _ => {
                            event!(
                                Level::ERROR,
                                "Invalid Face for VarInt Enum during Use Item On"
                            );
                            return Err(io::Error::other(
                                "Invalid Face for VarInt Enum during Use Item On",
                            ));
                        }
                    };
                }
                id => {
                    event!(Level::TRACE, "Recieved unknown packet {id}");
                }
            }
        } else {
            event!(Level::INFO, "{} left", ctx.name);
            break;
        }
    }
    Ok(())
}

fn handle_configuration(
    mut conn: ConnReader,
    settings: Arc<Config>,
    ctx: PlayerContext,
    level: Arc<RwLock<world::Level>>,
) -> io::Result<()> {
    let guard = span!(Level::INFO, "configuration");
    event!(Level::TRACE, "Configuring");
    let mut temp = vec![];
    create_known_packs(
        vec![KnownPack {
            namespace: "minecraft".into(),
            id: "core".into(),
            version: "1.21.11".into(),
        }],
        &mut temp,
    )?;
    conn.write_all(&assemble(0x0E, &temp))?;
    event!(Level::TRACE, "Sent known packs");
    loop {
        if let Some(RawPacket {
            length: _,
            id,
            mut data,
        }) = conn.read_one()?
        {
            if id.value() == 0x07 {
                event!(Level::TRACE, "Read known packs: {:?}", data);
                let Some(_) = parse_known_packs(&mut data) else {
                    event!(
                        Level::WARN,
                        "Failed to decode packet serverbound/minecraft:select_known_packs"
                    );
                    panic!();
                };
                event!(Level::TRACE, "Parsed known packs");
                break;
            } else if id.value() == 0x02 {
                event!(Level::TRACE, "Reading Plugin Message");
                let Some(message) = decode_plugin_message(&mut data) else {
                    event!(
                        Level::WARN,
                        "Unable to decode packet serverbound/minecraft:custom_payload"
                    );
                    continue;
                };
                event!(
                    Level::DEBUG,
                    "Plugin channel {} sent message containing {}",
                    message.0,
                    message.1
                );
            } else {
                event!(Level::DEBUG, "packet id {id:?}");
            }
        } else {
            event!(Level::WARN, "Unable to decode serverbound packet");
        }
    }
    // TODO currently agrees regardless of client supports. However, all modern versions come with minecraft:core.
    // minecraft:core cannot be deleted, it's embedded in the jar.
    // malformed/hacked clients are a separate thing

    // let reg_data = create_registry()?; // these are 3 separate packets
    // event!(Level::TRACE, "Created registry");
    conn.write_all(REGISTRY_DATA)?; //TODO replace with actual reg data
    event!(Level::TRACE, "Sent registry data packets");
    // stream.write_all(&create_tags(vec![])).ok()?;
    conn.write_all(UPDATE_TAGS)?; //TODO replace with real customizable tags
    event!(Level::TRACE, "Created empty tags packet and sent");
    conn.write_all(&[1, 3])?;
    event!(Level::TRACE, "Finished configuration");
    let Some(_) = conn.read_one()? else {
        event!(
            Level::WARN,
            "Failed to decode packet serverbound/minecraft:finish_configuration"
        );
        panic!();
    }; // ack finish config
    event!(
        Level::TRACE,
        "Client acknowledged finish config, transitioning to Play"
    );
    std::mem::drop(guard);
    handle_play(conn, settings, ctx, level)
}

fn handle_player(
    mut conn: ConnReader,
    settings: Arc<Config>,
    mut ctx: PlayerContext,
    level: Arc<RwLock<world::Level>>,
) -> Option<()> {
    let guard = span!(Level::INFO, "login");
    event!(Level::DEBUG, "Logging in");
    // Login Request
    let mut start = conn.read_one().ok()??;
    event!(Level::DEBUG, "Read Login Request: {:?}", start.data);
    event!(Level::TRACE, "Login Start");
    let (name, uuid) = login_start(&mut start.data)?;
    event!(
        Level::INFO,
        "Player {name} joined with UUID {uuid:032x} from {} using {}:{}",
        ctx.peer_addr,
        ctx.connected_addr,
        ctx.connected_port
    );
    // Login Success
    let success = assemble(2, &login_success(&name, uuid, 0)?);
    ctx.name = name;
    ctx.uuid = uuid;
    conn.write_all(&success).ok()?;
    event!(Level::DEBUG, "Sent Login Success");
    // Discard Login Ack
    conn.read_one().ok()??;
    event!(Level::DEBUG, "Read Login Acknowledge");
    std::mem::drop(guard);
    handle_configuration(conn, settings, ctx, level).ok()
}

fn handle_client(
    mut conn: ConnReader,
    settings: Arc<Config>,
    threads: Arc<AtomicI32>,
    mut ctx: PlayerContext,
    level: Arc<RwLock<world::Level>>,
) -> Option<()> {
    let status = &settings.status;
    // Handshaking 0 ( Handshake )
    match conn.read_at_least(2) {
        Err(e) => eprintln!("Reading failed with error {e:?}"),
        Ok(()) => (),
    }
    ctx.peer_addr = conn
        .inner_mut()
        .peer_addr()
        .expect("Peer addr is Ok if bytes were received");
    // legacy??
    if conn.get_buffer()[..2] == [254u8, 1] {
        conn.write_all(LEGACY).ok()?;
        return None;
    }
    let handshake = conn.read_one().ok()??;
    event!(Level::TRACE, "handshake: {:?}", handshake.data);
    let (proto, addr, port, next) = stage_0(&mut &handshake.data[..])?;
    ctx.connected_addr = addr;
    ctx.connected_port = port;
    // Play state
    if next.value() == 2 {
        let (prev, _guard) = ThreadsGuard::new(&threads);
        if prev >= settings.server.max_players {
            conn.write_all(DISCONNECT).ok()?;
            return None;
        }
        let res = handle_player(conn, settings, ctx, level);
        return res;
    }
    event!(Level::DEBUG, "Pinging");
    // Status 0 ( Status Request )
    let statreq = conn.read_one().ok()??;
    if statreq.length.value() != 1 {
        event!(
            Level::WARN,
            "{} sent invalid Status Request {:?}, continuing anyways.",
            ctx.peer_addr,
            statreq.data
        );
        event!(
            Level::WARN,
            "Previous handshake:  (proto, addr, port, next) = {:?}",
            (&proto, ctx.connected_addr, port, next)
        )
    }
    // Status 0 ( Status Response )
    let status_response = assemble_0(&format!(
        indoc!(
            r#"
            {{
                "version": {{
                    "name": "{}",
                    "protocol": {}
                }},
                "players": {{
                    "max": {},
                    "online": {},
                    "sample": []
                }},
                "description": {{
                    "text" : "{}"
                }}
            }}
        "#
        ),
        status.version_string,
        if status.echo_version {
            proto.value()
        } else {
            status.protocol_version
        },
        settings.server.max_players,
        threads.load(Ordering::Acquire),
        status.motd // limitation: breaks if has quote or \
    ))?;
    conn.write_all(&status_response).ok()?;
    // Status 1 ( Ping Request )
    let n = conn.read_one().ok()??;
    // Status 1 ( Ping Response )
    conn.write_all(&n.data).ok()?;
    Some(())
}

fn main() {
    let program_start = Instant::now();
    let settings = match fs::read(CONFIG_FILE) {
        Ok(s) => s,
        Err(e) => match e.kind() {
            ErrorKind::NotFound => {
                if let Err(e) = fs::write(CONFIG_FILE, BASE_DATA) {
                    panic!("Could not write default config to file: {e}")
                }
                BASE_DATA.to_vec()
            }
            ErrorKind::PermissionDenied => {
                panic!("Permission denied reading config file.")
            }
            e => {
                panic!("Unknown error reading config file: {e}")
            }
        },
    };
    let settings = String::from_utf8(settings).expect("Config file contains invalid UTF-8.");
    let settings: Config = match toml::from_str(&settings) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Invalid config format.");
            eprintln!("Error message: {}", e.message());
            if e.message().starts_with("missing field") {
                eprintln!("To resolve this error, add the missing field in your configuration.")
            }
            eprintln!("To generate a new, clean config, delete or rename config.toml.");
            panic!()
        }
    };
    tracing_subscriber::fmt()
        .with_max_level(match &*settings.server.log_level.to_ascii_uppercase() {
            "TRACE" => Level::TRACE,
            "DEBUG" => Level::DEBUG,
            "INFO" => Level::INFO,
            "WARN" => Level::WARN,
            "ERROR" => Level::ERROR,
            _ => {
                eprintln!(
                    "WARNING: Log level was not set to one of TRACE, DEBUG, INFO, WARN, ERROR"
                );
                eprintln!("WARNING: Defaulting to INFO");
                Level::INFO
            }
        })
        .init();
    let threads = Arc::new(AtomicI32::new(0));
    let listener =
        TcpListener::bind(SocketAddr::new(settings.server.bind, settings.server.port)).unwrap();
    let shared_settings = Arc::new(settings);
    let world = Arc::new(RwLock::new(world::Level::new(None)));
    event!(Level::INFO, "Ready in {:?}", program_start.elapsed());
    for stream in listener.incoming() {
        let thread_settings = Arc::clone(&shared_settings);
        let threads = Arc::clone(&threads);
        let world = Arc::clone(&world);
        thread::spawn(move || match stream {
            Ok(s) => {
                let peer = s.peer_addr();
                let conn_reader = ConnReader::new(s, thread_settings.server.buffer_size);
                let now = Instant::now();
                event!(Level::DEBUG, "Got new connection from {:?}", peer);
                let player_context = PlayerContext {
                    name: String::new(),
                    uuid: 0xDEADBEEFDEADBEEFDEADBEEFDEADBEEF,
                    entity_id: 0,
                    peer_addr: "0.0.0.0:0".parse().unwrap(),
                    connected_addr: String::new(),
                    connected_port: 0,
                    connected_at: now, // connencted at
                };
                if handle_client(
                    conn_reader,
                    thread_settings,
                    threads.clone(),
                    player_context,
                    world,
                )
                .is_none()
                {
                    event!(
                        Level::WARN,
                        "Client {:?} dropped connection or packet reading failed.",
                        peer
                    );
                };
            }
            Err(e) => {
                eprintln!("Failed to accept connection: {e}");
            }
        });
    }
}
