use protos::models as fb_models;
use serde::{Deserialize, Serialize};
use utils::bitset::ImmutBitSet;
use utils::BkdrHasher;

use crate::errors::{Error, Result};
use crate::{tag, SeriesId, Tag, TagValue};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SeriesKey {
    pub id: SeriesId,
    pub tags: Vec<Tag>,
    pub table: String,
    pub db: String,
}

impl SeriesKey {
    pub fn id(&self) -> SeriesId {
        self.id
    }

    pub fn set_id(&mut self, id: SeriesId) {
        self.id = id;
    }
    pub fn tags(&self) -> &Vec<Tag> {
        &self.tags
    }

    pub fn table(&self) -> &String {
        &self.table
    }

    pub fn encode(&self) -> Vec<u8> {
        bincode::serialize(self).unwrap()
    }

    pub fn tag_val(&self, key: &str) -> Option<TagValue> {
        for tag in &self.tags {
            if tag.key == key.as_bytes() {
                return Some(tag.value.clone());
            }
        }
        None
    }

    pub fn tag_string_val(&self, key: &str) -> Result<Option<String>> {
        match self.tag_val(key) {
            Some(v) => Ok(Some(String::from_utf8(v)?)),
            None => Ok(None),
        }
    }

    pub fn hash(&self) -> u64 {
        let mut hasher = BkdrHasher::new();
        hasher.hash_with(self.table.as_bytes());
        for tag in &self.tags {
            hasher.hash_with(&tag.key);
            hasher.hash_with(&tag.value);
        }

        hasher.number()
    }

    pub fn decode(data: &[u8]) -> Result<SeriesKey> {
        let key = bincode::deserialize(data);
        match key {
            Ok(key) => Ok(key),
            Err(err) => Err(Error::InvalidSerdeMessage {
                err: format!("Invalid serde message: {}", err),
            }),
        }
    }

    /// Returns a string with format `{table}[,{tag.key}={tag.value}]`.
    pub fn string(&self) -> String {
        let buf_len = self.tags.iter().fold(self.table.len(), |acc, tag| {
            acc + tag.key.len() + tag.value.len() + 2 // ,{key}={value}
        });
        let mut buf = Vec::with_capacity(buf_len);
        buf.extend_from_slice(self.table.as_bytes());
        for tag in self.tags.iter() {
            buf.extend(b",");
            buf.extend_from_slice(&tag.key);
            buf.extend_from_slice(b"=");
            buf.extend_from_slice(&tag.value);
        }

        String::from_utf8(buf).unwrap()
    }

    pub fn build_series_key(
        db_name: &str,
        tab_name: &str,
        tag_names: &[&str],
        point: &fb_models::Point,
    ) -> Result<Self> {
        let tag_nullbit_buffer = point.tags_nullbit().ok_or(Error::InvalidTag {
            err: "point tag null bit".to_string(),
        })?;
        let len = tag_names.len();
        let tag_nullbit = ImmutBitSet::new_without_check(len, tag_nullbit_buffer.bytes());
        let mut tags = Vec::new();
        for (idx, (tag_key, tag_value)) in tag_names
            .iter()
            .zip(point.tags().ok_or(Error::InvalidTag {
                err: "point tag value".to_string(),
            })?)
            .enumerate()
        {
            if !tag_nullbit.get(idx) {
                continue;
            }
            tags.push(Tag::new(
                tag_key.as_bytes().to_vec(),
                tag_value
                    .value()
                    .ok_or(Error::InvalidTag {
                        err: "tag missing value".to_string(),
                    })?
                    .bytes()
                    .into(),
            ))
        }

        tag::sort_tags(&mut tags);

        Ok(Self {
            id: 0,
            tags,
            table: tab_name.to_string(),
            db: db_name.to_string(),
        })
    }
}

impl PartialEq for SeriesKey {
    fn eq(&self, other: &Self) -> bool {
        if self.table != other.table {
            return false;
        }

        if self.tags != other.tags {
            return false;
        }

        true
    }
}

impl std::fmt::Display for SeriesKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.table)?;
        for tag in self.tags.iter() {
            write!(
                f,
                ",{}={}",
                std::str::from_utf8(&tag.key).map_err(|_| std::fmt::Error)?,
                std::str::from_utf8(&tag.value).map_err(|_| std::fmt::Error)?,
            )?;
        }

        Ok(())
    }
}
