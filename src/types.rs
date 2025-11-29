use num_derive::FromPrimitive;
use prost::Message;
use tokio::sync::mpsc;
use tokio::{
    io::{ReadHalf, WriteHalf},
    net::TcpStream,
};
use tokio_rustls::client::TlsStream;

use crate::mumble_proto;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub rspotify_client_id: String,
    pub rspotify_client_secret: String,
}

#[derive(Debug, Clone)]
pub enum PlayerAction {
    PlaySong(Song),
    Stop,
    Pause,
    Resume,
    Next,
    ShowQueue,
    SetVolume(f64),
}

#[derive(Debug, Clone)]
pub enum SongType {
    Spotify,
}

#[derive(Debug, Clone)]
pub struct Song {
    pub name: String,
    pub id: String,
    pub song_type: SongType,
}

#[repr(u16)]
#[derive(FromPrimitive, Debug, PartialEq, Eq, Clone, Copy)]
pub enum MumbleType {
    Version = 0,
    UDPTunnel,
    Authenticate,
    Ping,
    Reject,
    ServerSync,
    ChannelRemove,
    ChannelState,
    UserRemove,
    UserState,
    BanList,
    TextMessage,
    PermissionDenied,
    Acl,
    QueryUsers,
    CryptSetup,
    ContextActionModify,
    ContextAction,
    UserList,
    VoiceTarget,
    PermissionQuery,
    CodecVersion,
    UserStats,
    RequestBlob,
    ServerConfig,
    SuggestConfig,
}

/**
Encode a u64 as a Mumble varint.
Implementation translated from the Mumble impl in PacketDataStream.h
 */
pub fn varint_encode(val: u64) -> Vec<u8> {
    let mut v: Vec<u8> = vec![];
    let mut i = val;

    if ((i & 0x8000000000000000) > 0) && (!i < 0x100000000) {
        // Signed number.
        i = !i;
        if i <= 0x3 {
            // Special case for -1 to -4. The most significant bits of the first byte must be (in binary) 111111
            // followed by the 2 bits representing the absolute value of the encoded number. Shortcase for -1 to -4
            v.push(0xFC | (i as u8));
            return v;
        } else {
            // Add flag byte, whose most significant bits are (in binary) 111110 that indicates
            // that what follows is the varint encoding of the absolute value of i, but that the
            // value itself is supposed to be negative.
            v.push(0xF8);
        }
    }

    if i < 0x80 {
        // Encode as 7-bit, positive number -> most significant bit of first byte must be zero
        v.push(i as u8);
    } else if i < 0x4000 {
        // Encode as 14-bit, positive number -> most significant bits of first byte must be (in binary) 10
        v.push(((i >> 8) | 0x80) as u8);
        v.push((i & 0xFF) as u8);
    } else if i < 0x200000 {
        // Encode as 21-bit, positive number -> most significant bits of first byte must be (in binary) 110
        v.push(((i >> 16) | 0xC0) as u8);
        v.push(((i >> 8) & 0xFF) as u8);
        v.push((i & 0xFF) as u8);
    } else if i < 0x10000000 {
        // Encode as 28-bit, positive number -> most significant bits of first byte must be (in binary) 1110
        v.push(((i >> 24) | 0xE0) as u8);
        v.push(((i >> 16) & 0xFF) as u8);
        v.push(((i >> 8) & 0xFF) as u8);
        v.push((i & 0xFF) as u8);
    } else if i < 0x100000000 {
        // Encode as 32-bit, positive number -> most significant bits of first byte must be (in binary) 111100
        // Remaining bits in first byte remain unused
        v.push(0xF0);
        v.push(((i >> 24) & 0xFF) as u8);
        v.push(((i >> 16) & 0xFF) as u8);
        v.push(((i >> 8) & 0xFF) as u8);
        v.push((i & 0xFF) as u8);
    } else {
        // Encode as 64-bit, positive number -> most significant bits of first byte must be (in binary) 111101
        // Remaining bits in first byte remain unused
        v.push(0xF4);
        v.push(((i >> 56) & 0xFF) as u8);
        v.push(((i >> 48) & 0xFF) as u8);
        v.push(((i >> 40) & 0xFF) as u8);
        v.push(((i >> 32) & 0xFF) as u8);
        v.push(((i >> 24) & 0xFF) as u8);
        v.push(((i >> 16) & 0xFF) as u8);
        v.push(((i >> 8) & 0xFF) as u8);
        v.push((i & 0xFF) as u8);
    }

    v
}

#[derive(Debug, Clone)]
pub enum MumbleMsg {
    Version(mumble_proto::Version),
    UDPTunnel(Vec<u8>),
    Authenticate(mumble_proto::Authenticate),
    Ping(mumble_proto::Ping),
    Reject(mumble_proto::Reject),
    ServerSync(mumble_proto::ServerSync),
    ChannelRemove(mumble_proto::ChannelRemove),
    ChannelState(mumble_proto::ChannelState),
    UserRemove(mumble_proto::UserRemove),
    UserState(mumble_proto::UserState),
    BanList(mumble_proto::BanList),
    TextMessage(mumble_proto::TextMessage),
    PermissionDenied(mumble_proto::PermissionDenied),
    Acl(mumble_proto::Acl),
    QueryUsers(mumble_proto::QueryUsers),
    CryptSetup(mumble_proto::CryptSetup),
    ContextActionModify(mumble_proto::ContextActionModify),
    ContextAction(mumble_proto::ContextAction),
    UserList(mumble_proto::UserList),
    VoiceTarget(mumble_proto::VoiceTarget),
    PermissionQuery(mumble_proto::PermissionQuery),
    CodecVersion(mumble_proto::CodecVersion),
    UserStats(mumble_proto::UserStats),
    RequestBlob(mumble_proto::RequestBlob),
    ServerConfig(mumble_proto::ServerConfig),
    SuggestConfig(mumble_proto::SuggestConfig),
}

impl MumbleMsg {
    pub fn tag(&self) -> MumbleType {
        match &self {
            MumbleMsg::Version(_) => MumbleType::Version,
            MumbleMsg::UDPTunnel(_) => MumbleType::UDPTunnel,
            MumbleMsg::Authenticate(_) => MumbleType::Authenticate,
            MumbleMsg::Ping(_) => MumbleType::Ping,
            MumbleMsg::Reject(_) => MumbleType::Reject,
            MumbleMsg::ServerSync(_) => MumbleType::ServerSync,
            MumbleMsg::ChannelRemove(_) => MumbleType::ChannelRemove,
            MumbleMsg::ChannelState(_) => MumbleType::ChannelState,
            MumbleMsg::UserRemove(_) => MumbleType::UserRemove,
            MumbleMsg::UserState(_) => MumbleType::UserState,
            MumbleMsg::BanList(_) => MumbleType::BanList,
            MumbleMsg::TextMessage(_) => MumbleType::TextMessage,
            MumbleMsg::PermissionDenied(_) => MumbleType::PermissionDenied,
            MumbleMsg::Acl(_) => MumbleType::Acl,
            MumbleMsg::QueryUsers(_) => MumbleType::QueryUsers,
            MumbleMsg::CryptSetup(_) => MumbleType::CryptSetup,
            MumbleMsg::ContextActionModify(_) => MumbleType::ContextActionModify,
            MumbleMsg::ContextAction(_) => MumbleType::ContextAction,
            MumbleMsg::UserList(_) => MumbleType::UserList,
            MumbleMsg::VoiceTarget(_) => MumbleType::VoiceTarget,
            MumbleMsg::PermissionQuery(_) => MumbleType::PermissionQuery,
            MumbleMsg::CodecVersion(_) => MumbleType::CodecVersion,
            MumbleMsg::UserStats(_) => MumbleType::UserStats,
            MumbleMsg::RequestBlob(_) => MumbleType::RequestBlob,
            MumbleMsg::ServerConfig(_) => MumbleType::ServerConfig,
            MumbleMsg::SuggestConfig(_) => MumbleType::SuggestConfig,
        }
    }

    pub fn as_data(&self) -> Vec<u8> {
        match self {
            MumbleMsg::Version(version) => version.encode_to_vec(),
            MumbleMsg::UDPTunnel(_) => unimplemented!(),
            MumbleMsg::Authenticate(authenticate) => authenticate.encode_to_vec(),
            MumbleMsg::Ping(ping) => ping.encode_to_vec(),
            MumbleMsg::Reject(reject) => reject.encode_to_vec(),
            MumbleMsg::ServerSync(server_sync) => server_sync.encode_to_vec(),
            MumbleMsg::ChannelRemove(channel_remove) => channel_remove.encode_to_vec(),
            MumbleMsg::ChannelState(channel_state) => channel_state.encode_to_vec(),
            MumbleMsg::UserRemove(user_remove) => user_remove.encode_to_vec(),
            MumbleMsg::UserState(user_state) => user_state.encode_to_vec(),
            MumbleMsg::BanList(ban_list) => ban_list.encode_to_vec(),
            MumbleMsg::TextMessage(text_message) => text_message.encode_to_vec(),
            MumbleMsg::PermissionDenied(permission_denied) => permission_denied.encode_to_vec(),
            MumbleMsg::Acl(acl) => acl.encode_to_vec(),
            MumbleMsg::QueryUsers(query_users) => query_users.encode_to_vec(),
            MumbleMsg::CryptSetup(crypt_setup) => crypt_setup.encode_to_vec(),
            MumbleMsg::ContextActionModify(context_action_modify) => {
                context_action_modify.encode_to_vec()
            }
            MumbleMsg::ContextAction(context_action) => context_action.encode_to_vec(),
            MumbleMsg::UserList(user_list) => user_list.encode_to_vec(),
            MumbleMsg::VoiceTarget(voice_target) => voice_target.encode_to_vec(),
            MumbleMsg::PermissionQuery(permission_query) => permission_query.encode_to_vec(),
            MumbleMsg::CodecVersion(codec_version) => codec_version.encode_to_vec(),
            MumbleMsg::UserStats(user_stats) => user_stats.encode_to_vec(),
            MumbleMsg::RequestBlob(request_blob) => request_blob.encode_to_vec(),
            MumbleMsg::ServerConfig(server_config) => server_config.encode_to_vec(),
            MumbleMsg::SuggestConfig(suggest_config) => suggest_config.encode_to_vec(),
        }
    }

    pub fn from_tagged_data(tag: MumbleType, buf: &[u8]) -> anyhow::Result<Self> {
        let msg = match tag {
            MumbleType::Version => MumbleMsg::Version(mumble_proto::Version::decode(buf)?),
            MumbleType::UDPTunnel => unimplemented!(),
            MumbleType::Authenticate => {
                MumbleMsg::Authenticate(mumble_proto::Authenticate::decode(buf)?)
            }
            MumbleType::Ping => MumbleMsg::Ping(mumble_proto::Ping::decode(buf)?),
            MumbleType::Reject => MumbleMsg::Reject(mumble_proto::Reject::decode(buf)?),
            MumbleType::ServerSync => MumbleMsg::ServerSync(mumble_proto::ServerSync::decode(buf)?),
            MumbleType::ChannelRemove => {
                MumbleMsg::ChannelRemove(mumble_proto::ChannelRemove::decode(buf)?)
            }
            MumbleType::ChannelState => {
                MumbleMsg::ChannelState(mumble_proto::ChannelState::decode(buf)?)
            }
            MumbleType::UserRemove => MumbleMsg::UserRemove(mumble_proto::UserRemove::decode(buf)?),
            MumbleType::UserState => MumbleMsg::UserState(mumble_proto::UserState::decode(buf)?),
            MumbleType::BanList => MumbleMsg::BanList(mumble_proto::BanList::decode(buf)?),
            MumbleType::TextMessage => {
                MumbleMsg::TextMessage(mumble_proto::TextMessage::decode(buf)?)
            }
            MumbleType::PermissionDenied => {
                MumbleMsg::PermissionDenied(mumble_proto::PermissionDenied::decode(buf)?)
            }
            MumbleType::Acl => MumbleMsg::Acl(mumble_proto::Acl::decode(buf)?),
            MumbleType::QueryUsers => MumbleMsg::QueryUsers(mumble_proto::QueryUsers::decode(buf)?),
            MumbleType::CryptSetup => MumbleMsg::CryptSetup(mumble_proto::CryptSetup::decode(buf)?),
            MumbleType::ContextActionModify => {
                MumbleMsg::ContextActionModify(mumble_proto::ContextActionModify::decode(buf)?)
            }
            MumbleType::ContextAction => {
                MumbleMsg::ContextAction(mumble_proto::ContextAction::decode(buf)?)
            }
            MumbleType::UserList => MumbleMsg::UserList(mumble_proto::UserList::decode(buf)?),
            MumbleType::VoiceTarget => {
                MumbleMsg::VoiceTarget(mumble_proto::VoiceTarget::decode(buf)?)
            }
            MumbleType::PermissionQuery => {
                MumbleMsg::PermissionQuery(mumble_proto::PermissionQuery::decode(buf)?)
            }
            MumbleType::CodecVersion => {
                MumbleMsg::CodecVersion(mumble_proto::CodecVersion::decode(buf)?)
            }
            MumbleType::UserStats => MumbleMsg::UserStats(mumble_proto::UserStats::decode(buf)?),
            MumbleType::RequestBlob => {
                MumbleMsg::RequestBlob(mumble_proto::RequestBlob::decode(buf)?)
            }
            MumbleType::ServerConfig => {
                MumbleMsg::ServerConfig(mumble_proto::ServerConfig::decode(buf)?)
            }
            MumbleType::SuggestConfig => {
                MumbleMsg::SuggestConfig(mumble_proto::SuggestConfig::decode(buf)?)
            }
        };

        Ok(msg)
    }
}

pub type WriteStream = WriteHalf<TlsStream<TcpStream>>;
pub type ReadStream = ReadHalf<TlsStream<TcpStream>>;

pub type MumbleMsgSink = mpsc::Sender<MumbleMsg>;
pub type MumbleMsgSource = mpsc::Receiver<MumbleMsg>;
