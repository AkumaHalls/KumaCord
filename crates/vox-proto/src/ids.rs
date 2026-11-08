//! Strongly-typed ID wrappers for compile-time safety.
//!
//! These replace the raw `i64` / `String` type aliases to prevent mixing up
//! different ID kinds at compile time. All wrappers are `#[serde(transparent)]`
//! so they serialize as their raw value (not `{"0": 123}`).

use std::fmt;
use std::hash::Hash;
use std::ops::Deref;
use std::str::FromStr;
use serde::{Deserialize, Serialize, Deserializer, Serializer};

// ============================================================================
// UserId
// ============================================================================

/// Unique identifier for a user account.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct UserId(pub i64);

impl UserId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn into_inner(self) -> i64 {
        self.0
    }
}

impl From<i64> for UserId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<UserId> for i64 {
    fn from(id: UserId) -> Self {
        id.0
    }
}

impl Deref for UserId {
    type Target = i64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for UserId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for UserId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(i64::deserialize(deserializer)?))
    }
}

// SQLite support (enabled with `sqlite` feature)
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for UserId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for UserId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(i64::column_result(value)?))
    }
}

impl FromStr for UserId {
    type Err = <i64 as FromStr>::Err;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ============================================================================
// ChannelId
// ============================================================================

/// Unique identifier for a channel (text or voice).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct ChannelId(pub i64);

impl ChannelId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn into_inner(self) -> i64 {
        self.0
    }
}

impl From<i64> for ChannelId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<ChannelId> for i64 {
    fn from(id: ChannelId) -> Self {
        id.0
    }
}

impl Deref for ChannelId {
    type Target = i64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for ChannelId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for ChannelId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(i64::deserialize(deserializer)?))
    }
}

// SQLite support
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for ChannelId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for ChannelId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(i64::column_result(value)?))
    }
}

impl FromStr for ChannelId {
    type Err = <i64 as FromStr>::Err;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ============================================================================
// MessageId
// ============================================================================

/// Unique identifier for a channel message.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct MessageId(pub i64);

impl MessageId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn into_inner(self) -> i64 {
        self.0
    }
}

impl From<i64> for MessageId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<MessageId> for i64 {
    fn from(id: MessageId) -> Self {
        id.0
    }
}

impl Deref for MessageId {
    type Target = i64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for MessageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for MessageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(i64::deserialize(deserializer)?))
    }
}

// SQLite support
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for MessageId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for MessageId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(i64::column_result(value)?))
    }
}

impl FromStr for MessageId {
    type Err = <i64 as FromStr>::Err;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ============================================================================
// DmId
// ============================================================================

/// Unique identifier for a direct message conversation (1:1).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct DmId(pub i64);

impl DmId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn into_inner(self) -> i64 {
        self.0
    }
}

impl From<i64> for DmId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<DmId> for i64 {
    fn from(id: DmId) -> Self {
        id.0
    }
}

impl Deref for DmId {
    type Target = i64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for DmId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for DmId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for DmId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(i64::deserialize(deserializer)?))
    }
}

// SQLite support
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for DmId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for DmId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(i64::column_result(value)?))
    }
}

impl FromStr for DmId {
    type Err = <i64 as FromStr>::Err;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ============================================================================
// DmMessageId
// ============================================================================

/// Unique identifier for a direct message within a DM conversation.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct DmMessageId(pub i64);

impl DmMessageId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn into_inner(self) -> i64 {
        self.0
    }
}

impl From<i64> for DmMessageId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<DmMessageId> for i64 {
    fn from(id: DmMessageId) -> Self {
        id.0
    }
}

impl Deref for DmMessageId {
    type Target = i64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for DmMessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for DmMessageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for DmMessageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(i64::deserialize(deserializer)?))
    }
}

// SQLite support
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for DmMessageId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for DmMessageId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(i64::column_result(value)?))
    }
}

impl FromStr for DmMessageId {
    type Err = <i64 as FromStr>::Err;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ============================================================================
// ServerId
// ============================================================================

/// Unique identifier for a server (32 hex digits = 128 bits).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerId(pub String);

impl ServerId {
    pub fn new(id: String) -> Self {
        Self(id)
    }

    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            Some(Self(hex.to_lowercase()))
        } else {
            None
        }
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ServerId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<ServerId> for String {
    fn from(id: ServerId) -> Self {
        id.0
    }
}

impl Deref for ServerId {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for ServerId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ServerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(String::deserialize(deserializer)?))
    }
}

// SQLite support (stored as TEXT)
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for ServerId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for ServerId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(String::column_result(value)?))
    }
}

impl FromStr for ServerId {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

// ============================================================================
// CustomRoleId
// ============================================================================

/// Unique identifier for a custom role (v1.1).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct CustomRoleId(pub i64);

impl CustomRoleId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn into_inner(self) -> i64 {
        self.0
    }
}

impl From<i64> for CustomRoleId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<CustomRoleId> for i64 {
    fn from(id: CustomRoleId) -> Self {
        id.0
    }
}

impl Deref for CustomRoleId {
    type Target = i64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for CustomRoleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for CustomRoleId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for CustomRoleId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(i64::deserialize(deserializer)?))
    }
}

// SQLite support
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for CustomRoleId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for CustomRoleId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(i64::column_result(value)?))
    }
}

impl FromStr for CustomRoleId {
    type Err = <i64 as FromStr>::Err;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ============================================================================
// SoundId
// ============================================================================

/// Unique identifier for a soundboard sound (name-based, but typed for safety).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SoundId(pub String);

impl SoundId {
    pub fn new(name: String) -> Self {
        Self(name)
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SoundId {
    fn from(name: String) -> Self {
        Self(name)
    }
}

impl From<SoundId> for String {
    fn from(id: SoundId) -> Self {
        id.0
    }
}

impl Deref for SoundId {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for SoundId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for SoundId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SoundId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(String::deserialize(deserializer)?))
    }
}

// SQLite support
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for SoundId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for SoundId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        // `sounds.id` is `INTEGER PRIMARY KEY` while the id travels as its decimal string
        // (`hub::sounds::insert_sound` builds `SoundId(id.to_string())`), so either storage
        // form has to read back.
        Ok(Self(match value {
            rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
            rusqlite::types::ValueRef::Integer(i) => i.to_string(),
            _ => return Err(rusqlite::types::FromSqlError::InvalidType),
        }))
    }
}

impl FromStr for SoundId {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

// ============================================================================
// AttachmentId
// ============================================================================

/// Unique identifier for a chat attachment.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct AttachmentId(pub i64);

impl AttachmentId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn into_inner(self) -> i64 {
        self.0
    }
}

impl From<i64> for AttachmentId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<AttachmentId> for i64 {
    fn from(id: AttachmentId) -> Self {
        id.0
    }
}

impl Deref for AttachmentId {
    type Target = i64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for AttachmentId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for AttachmentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(i64::deserialize(deserializer)?))
    }
}

// SQLite support
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for AttachmentId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for AttachmentId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(i64::column_result(value)?))
    }
}

impl FromStr for AttachmentId {
    type Err = <i64 as FromStr>::Err;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ============================================================================
// InviteId
// ============================================================================

/// Unique identifier for an invite code (scrambled base32).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InviteId(pub String);

impl InviteId {
    pub fn new(code: String) -> Self {
        Self(code)
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for InviteId {
    fn from(code: String) -> Self {
        Self(code)
    }
}

impl From<InviteId> for String {
    fn from(id: InviteId) -> Self {
        id.0
    }
}

impl Deref for InviteId {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for InviteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for InviteId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for InviteId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(String::deserialize(deserializer)?))
    }
}

// SQLite support
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for InviteId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for InviteId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        // The column is `INTEGER PRIMARY KEY` (a rowid) while the id travels as its decimal
        // string (`hub::invites::insert` builds `InviteId(id.to_string())` from the `i64`
        // `ids::insert_with` hands it), so either storage form has to read back.
        Ok(Self(match value {
            rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
            rusqlite::types::ValueRef::Integer(i) => i.to_string(),
            _ => return Err(rusqlite::types::FromSqlError::InvalidType),
        }))
    }
}

impl FromStr for InviteId {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

// ============================================================================
// CallId
// ============================================================================

/// Unique identifier for a DM call.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct CallId(pub i64);

impl CallId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn into_inner(self) -> i64 {
        self.0
    }
}

impl From<i64> for CallId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<CallId> for i64 {
    fn from(id: CallId) -> Self {
        id.0
    }
}

impl Deref for CallId {
    type Target = i64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for CallId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for CallId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(i64::deserialize(deserializer)?))
    }
}

// SQLite support
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for CallId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for CallId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(i64::column_result(value)?))
    }
}

impl FromStr for CallId {
    type Err = <i64 as FromStr>::Err;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ============================================================================
// DeviceTokenId
// ============================================================================

/// Unique identifier for a device token (remember-device).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct DeviceTokenId(pub i64);

impl DeviceTokenId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn into_inner(self) -> i64 {
        self.0
    }
}

impl From<i64> for DeviceTokenId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<DeviceTokenId> for i64 {
    fn from(id: DeviceTokenId) -> Self {
        id.0
    }
}

impl Deref for DeviceTokenId {
    type Target = i64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for DeviceTokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for DeviceTokenId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for DeviceTokenId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(i64::deserialize(deserializer)?))
    }
}

// SQLite support
#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for DeviceTokenId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for DeviceTokenId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(i64::column_result(value)?))
    }
}

impl FromStr for DeviceTokenId {
    type Err = <i64 as FromStr>::Err;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}