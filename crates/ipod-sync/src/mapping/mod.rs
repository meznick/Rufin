//! B-008: `.itl` ↔ libgpod model mapping (see `deltas/b008_plan.md`).
//!
//! The mapping is pure and deterministic: it consumes owned host records
//! ([`host`]) and the wrapper's owned device records ([`ipod::TrackInfo`],
//! [`ipod::PlaylistInfo`]) and produces evidence ([`map`]) — matches,
//! device-only entries and diagnostics. No device writes, no `.itl` writes,
//! no player state. The output is the input contract for B-009's dry run
//! and B-010's sync; nothing here decides a sync action.
//!
//! Dates and stats stay raw (Apple-epoch local wall-clock) end to end; the
//! single conversion to a real instant happens at the `itl-sync` write
//! boundary. Identity is the verified PID ↔ dbid chain — never titles,
//! paths or sizes (the owner's Convert-to-AAC option makes device sizes
//! legitimately different).

pub mod host;
pub mod map;
pub mod smart;

pub use host::{
    HostLibrary, HostLocation, HostPlaylist, HostSmartPlaylist, HostStats, HostTrack,
    HostTrackMetadata, host_library, host_playlists, host_tracks, resolve_playlist_members,
};
pub use map::{
    DeviceOnlyPlaylist, MappingDiagnostic, PlaylistMapping, PlaylistMatch, PlaylistOutcome,
    TrackMapping, TrackMatch, TrackOutcome, map_playlists, map_tracks,
};
pub use smart::{
    SLST_MAGIC, SmartLeaf, SmartRule, SmartRules, SmartRulesError, parse_smart_rules,
    smart_action_name, smart_field_name,
};
