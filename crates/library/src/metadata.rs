//! Accepted metadata, shared by catalog storage, current views and transfers.
use crate::AudioProperties;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "bindings", derive(uniffi::Record))]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_display_artist: Option<String>,
    pub sort_text: String,
    pub duration_millis: i64,
    pub disc_number: i64,
    pub track_number: i64,
    pub year: Option<i64>,
    pub release_date: Option<String>,
    pub date_added: Option<String>,
    pub source_format: Option<String>,
    pub audio_properties: AudioProperties,
    pub comment: Option<String>,
    /// Parental advisory: `true` when the source marks the track explicit.
    /// Clean and unflagged tracks are both `false` — there is no clean badge.
    /// The reader tolerates integers 0/1: SQLite JSONB carries booleans as
    /// integers, and the authoritative catalog merge of older builds stored
    /// them that way (fork registry №18).
    #[serde(default, deserialize_with = "de_jsonb_bool")]
    pub explicit: bool,
    pub bpm: Option<i64>,
    pub musicbrainz_recording_id: Option<String>,
    pub musicbrainz_release_track_id: Option<String>,
    pub musicbrainz_album_id: Option<String>,
    pub musicbrainz_release_group_id: Option<String>,
    pub primary_artist_musicbrainz_id: Option<String>,
    pub artists: Vec<ArtistCredit>,
    pub album_artists: Vec<ArtistCredit>,
    pub genres: Vec<TagReference>,
    pub moods: Vec<TagReference>,
    pub album_tags: Option<AlbumTags>,
    pub source_loudness: Option<NativeGain>,
    pub album_loudness: Option<NativeGain>,
}

/// None on TrackMetadata means the old cache never observed raw album tags.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "bindings", derive(uniffi::Record))]
pub struct AlbumTags {
    pub sort_text: String,
    pub is_compilation: Option<bool>,
    pub release_types: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "bindings", derive(uniffi::Record))]
pub struct AlbumMetadata {
    pub title: String,
    pub artist: String,
    pub sort_text: String,
    pub year: Option<i64>,
    pub release_date: Option<String>,
    pub date_added: Option<String>,
    pub musicbrainz_release_id: Option<String>,
    pub musicbrainz_release_group_id: Option<String>,
    pub is_compilation: Option<bool>,
    pub album_artists: Vec<ArtistCredit>,
    pub genres: Vec<TagReference>,
    pub release_types: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "bindings", derive(uniffi::Record))]
pub struct ArtistMetadata {
    pub name: String,
    pub sort_text: Option<String>,
    pub musicbrainz_artist_id: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "bindings", derive(uniffi::Record))]
pub struct TagMetadata {
    pub name: String,
    pub sort_text: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "bindings", derive(uniffi::Record))]
pub struct FolderMetadata {
    pub name: Option<String>,
    pub sort_text: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "bindings", derive(uniffi::Record))]
pub struct ArtistCredit {
    pub object_id: String,
    pub name: String,
    pub sort_text: Option<String>,
    pub musicbrainz_artist_id: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "bindings", derive(uniffi::Record))]
pub struct TagReference {
    pub object_id: String,
    pub name: String,
    pub sort_text: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "bindings", derive(uniffi::Record))]
pub struct NativeGain {
    pub integrated_lufs: Option<f64>,
    pub true_peak: Option<f64>,
    pub replay_gain_db: Option<f64>,
    pub replay_gain_peak: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, sqlx::FromRow)]
#[cfg_attr(feature = "bindings", derive(uniffi::Record))]
pub struct TrackReference {
    pub media_uri: String,
    #[serde(flatten)]
    #[sqlx(json)]
    pub metadata: TrackMetadata,
    #[serde(skip)]
    #[sqlx(skip)]
    pub artwork_binding: Option<Vec<u8>>,
}

impl<'de> Deserialize<'de> for TrackReference {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let media_uri = value
            .get("media_uri")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(metadata) = value.get_mut("metadata") {
            value = metadata.take();
        }
        if let Some(object) = value.as_object_mut() {
            object.retain(|_, value| !value.is_null());
        }
        let metadata = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            media_uri,
            metadata,
            artwork_binding: None,
        })
    }
}

/// Accepts a JSON boolean or the integers 0/1 (and `null` as `false`).
/// SQLite's JSONB carries booleans as integers, and the authoritative
/// catalog merge of builds before registry №18 stored `explicit` that way;
/// the field reads such rows until the next refresh rewrites them.
fn de_jsonb_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct JsonbBool;
    impl serde::de::Visitor<'_> for JsonbBool {
        type Value = bool;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a boolean or 0/1")
        }
        fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
            Ok(value)
        }
        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            match value {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(E::custom("explicit flag out of range")),
            }
        }
        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            match value {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(E::custom("explicit flag out of range")),
            }
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(false)
        }
    }
    deserializer.deserialize_any(JsonbBool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_decodes_booleans_and_jsonb_integers() {
        let from_bool: TrackMetadata = serde_json::from_str(r#"{"explicit":true}"#).unwrap();
        assert!(from_bool.explicit);
        let from_int: TrackMetadata = serde_json::from_str(r#"{"explicit":1}"#).unwrap();
        assert!(from_int.explicit);
        let from_zero: TrackMetadata = serde_json::from_str(r#"{"explicit":0}"#).unwrap();
        assert!(!from_zero.explicit);
        let from_null: TrackMetadata = serde_json::from_str(r#"{"explicit":null}"#).unwrap();
        assert!(!from_null.explicit);
        let missing: TrackMetadata = serde_json::from_str("{}").unwrap();
        assert!(!missing.explicit);
    }
}
