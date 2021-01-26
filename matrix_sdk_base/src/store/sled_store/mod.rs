// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod store_key;

use std::{collections::BTreeSet, convert::TryFrom, path::Path, sync::Arc, time::SystemTime};

use futures::{
    stream::{self, Stream},
    TryStreamExt,
};
use matrix_sdk_common::{
    async_trait,
    events::{
        presence::PresenceEvent,
        room::member::{MemberEventContent, MembershipState},
        AnySyncStateEvent, EventContent, EventType,
    },
    identifiers::{RoomId, UserId},
};
use serde::{Deserialize, Serialize};

use sled::{
    transaction::{ConflictableTransactionError, TransactionError},
    Config, Db, Transactional, Tree,
};
use tracing::info;

use crate::{deserialized_responses::MemberEvent, rooms::StrippedRoomInfo};

use self::store_key::{EncryptedEvent, StoreKey};

use super::{Result, RoomInfo, StateChanges, StateStore, StoreError};

#[derive(Debug, Serialize, Deserialize)]
pub enum DatabaseType {
    Unencrypted,
    Encrypted(store_key::EncryptedStoreKey),
}

#[derive(Debug, thiserror::Error)]
pub enum SerializationError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Encryption(#[from] store_key::Error),
}

impl From<TransactionError<SerializationError>> for StoreError {
    fn from(e: TransactionError<SerializationError>) -> Self {
        match e {
            TransactionError::Abort(e) => e.into(),
            TransactionError::Storage(e) => StoreError::Sled(e),
        }
    }
}

impl From<SerializationError> for StoreError {
    fn from(e: SerializationError) -> Self {
        match e {
            SerializationError::Json(e) => StoreError::Json(e),
            SerializationError::Encryption(e) => match e {
                store_key::Error::Random(e) => StoreError::Encryption(e.to_string()),
                store_key::Error::Serialization(e) => StoreError::Json(e),
                store_key::Error::Encryption(e) => StoreError::Encryption(e),
            },
        }
    }
}

trait EncodeKey {
    const SEPARATOR: u8 = 0xff;
    fn encode(&self) -> Vec<u8>;
}

impl EncodeKey for &str {
    fn encode(&self) -> Vec<u8> {
        [self.as_bytes(), &[Self::SEPARATOR]].concat()
    }
}

impl EncodeKey for (&str, &str) {
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_bytes(),
            &[Self::SEPARATOR],
            self.1.as_bytes(),
            &[Self::SEPARATOR],
        ]
        .concat()
    }
}

impl EncodeKey for (&str, &str, &str) {
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_bytes(),
            &[Self::SEPARATOR],
            self.1.as_bytes(),
            &[Self::SEPARATOR],
            self.2.as_bytes(),
            &[Self::SEPARATOR],
        ]
        .concat()
    }
}

#[derive(Debug, Clone)]
pub struct SledStore {
    pub(crate) inner: Db,
    store_key: Arc<Option<StoreKey>>,
    session: Tree,
    account_data: Tree,
    members: Tree,
    profiles: Tree,
    display_names: Tree,
    joined_user_ids: Tree,
    invited_user_ids: Tree,
    room_info: Tree,
    room_state: Tree,
    room_account_data: Tree,
    stripped_room_info: Tree,
    stripped_room_state: Tree,
    stripped_members: Tree,
    presence: Tree,
}

impl SledStore {
    fn open_helper(db: Db, store_key: Option<StoreKey>) -> Result<Self> {
        let session = db.open_tree("session")?;
        let account_data = db.open_tree("account_data")?;

        let members = db.open_tree("members")?;
        let profiles = db.open_tree("profiles")?;
        let display_names = db.open_tree("display_names")?;
        let joined_user_ids = db.open_tree("joined_user_ids")?;
        let invited_user_ids = db.open_tree("invited_user_ids")?;

        let room_state = db.open_tree("room_state")?;
        let room_info = db.open_tree("room_infos")?;
        let presence = db.open_tree("presence")?;
        let room_account_data = db.open_tree("room_account_data")?;

        let stripped_room_info = db.open_tree("stripped_room_info")?;
        let stripped_members = db.open_tree("stripped_members")?;
        let stripped_room_state = db.open_tree("stripped_room_state")?;

        Ok(Self {
            inner: db,
            store_key: store_key.into(),
            session,
            account_data,
            members,
            profiles,
            display_names,
            joined_user_ids,
            invited_user_ids,
            room_account_data,
            presence,
            room_state,
            room_info,
            stripped_room_info,
            stripped_members,
            stripped_room_state,
        })
    }

    pub fn open() -> Result<Self> {
        let db = Config::new().temporary(true).open()?;

        SledStore::open_helper(db, None)
    }

    pub fn open_with_passphrase(path: impl AsRef<Path>, passphrase: &str) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(path).open()?;

        let store_key: Option<DatabaseType> = db
            .get("store_key")?
            .map(|k| serde_json::from_slice(&k).map_err(StoreError::Json))
            .transpose()?;

        let store_key = if let Some(key) = store_key {
            if let DatabaseType::Encrypted(k) = key {
                StoreKey::import(passphrase, k).map_err(|_| StoreError::StoreLocked)?
            } else {
                return Err(StoreError::UnencryptedStore);
            }
        } else {
            let key = StoreKey::new().map_err::<StoreError, _>(|e| e.into())?;
            let encrypted_key = DatabaseType::Encrypted(
                key.export(passphrase)
                    .map_err::<StoreError, _>(|e| e.into())?,
            );
            db.insert("store_key", serde_json::to_vec(&encrypted_key)?)?;
            key
        };

        SledStore::open_helper(db, Some(store_key))
    }

    pub fn open_with_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(path).open()?;

        SledStore::open_helper(db, None)
    }

    fn serialize_event(
        &self,
        event: &impl Serialize,
    ) -> std::result::Result<Vec<u8>, SerializationError> {
        if let Some(key) = &*self.store_key {
            let encrypted = key.encrypt(event)?;
            Ok(serde_json::to_vec(&encrypted)?)
        } else {
            Ok(serde_json::to_vec(event)?)
        }
    }

    fn deserialize_event<T: for<'b> Deserialize<'b>>(
        &self,
        event: &[u8],
    ) -> std::result::Result<T, SerializationError> {
        if let Some(key) = &*self.store_key {
            let encrypted: EncryptedEvent = serde_json::from_slice(&event)?;
            Ok(key.decrypt(encrypted)?)
        } else {
            Ok(serde_json::from_slice(event)?)
        }
    }

    pub async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.session
            .insert(&format!("filter{}", filter_name), filter_id)?;

        Ok(())
    }

    pub async fn get_filter(&self, filter_name: &str) -> Result<Option<String>> {
        Ok(self
            .session
            .get(&format!("filter{}", filter_name))?
            .map(|f| String::from_utf8_lossy(&f).to_string()))
    }

    pub async fn get_sync_token(&self) -> Result<Option<String>> {
        Ok(self
            .session
            .get("sync_token")?
            .map(|t| String::from_utf8_lossy(&t).to_string()))
    }

    pub async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let now = SystemTime::now();

        let ret: std::result::Result<(), TransactionError<SerializationError>> = (
            &self.session,
            &self.account_data,
            &self.members,
            &self.profiles,
            &self.display_names,
            &self.joined_user_ids,
            &self.invited_user_ids,
            &self.room_info,
            &self.room_state,
            &self.room_account_data,
            &self.presence,
            &self.stripped_room_info,
            &self.stripped_members,
            &self.stripped_room_state,
        )
            .transaction(
                |(
                    session,
                    account_data,
                    members,
                    profiles,
                    display_names,
                    joined,
                    invited,
                    rooms,
                    state,
                    room_account_data,
                    presence,
                    striped_rooms,
                    stripped_members,
                    stripped_state,
                )| {
                    if let Some(s) = &changes.sync_token {
                        session.insert("sync_token", s.as_str())?;
                    }

                    for (room, events) in &changes.members {
                        let profile_changes = changes.profiles.get(room);

                        for event in events.values() {
                            let key = format!("{}{}", room.as_str(), event.state_key.as_str());

                            let old_profile: Option<MemberEventContent> = if let Some(p) = profiles
                                .get(key.as_str())?
                                .map(|p| self.deserialize_event(&p))
                                .transpose()
                                .map_err(ConflictableTransactionError::Abort)?
                            {
                                p
                            } else {
                                members
                                    .get(key.as_str())?
                                    .map(|m| self.deserialize_event::<MemberEvent>(&m))
                                    .transpose()
                                    .map_err(ConflictableTransactionError::Abort)?
                                    .map(|m| m.content)
                            };

                            let old_display_name = old_profile
                                .map(|m| {
                                    m.displayname
                                        .unwrap_or_else(|| event.state_key.localpart().to_string())
                                })
                                .unwrap_or_else(|| event.state_key.localpart().to_string());

                            let old_display_name_key = (
                                room.as_str(),
                                old_display_name.as_str(),
                                event.state_key.as_str(),
                            )
                                .encode();

                            let display_name = profile_changes
                                .map(|p| p.get(&event.state_key))
                                .flatten()
                                .as_ref()
                                .map(|m| m.displayname.as_deref())
                                .unwrap_or_else(|| Some(event.state_key.localpart()))
                                .unwrap_or_else(|| event.state_key.localpart());

                            let display_name_key =
                                (room.as_str(), display_name, event.state_key.as_str()).encode();

                            match event.content.membership {
                                MembershipState::Join => {
                                    joined.insert(key.as_str(), event.state_key.as_str())?;
                                    invited.remove(key.as_str())?;
                                    display_names.remove(old_display_name_key)?;
                                    display_names
                                        .insert(display_name_key, event.state_key.as_str())?;
                                }
                                MembershipState::Invite => {
                                    invited.insert(key.as_str(), event.state_key.as_str())?;
                                    joined.remove(key.as_str())?;
                                    display_names.remove(old_display_name_key)?;
                                    display_names
                                        .insert(display_name_key, event.state_key.as_str())?;
                                }
                                _ => {
                                    display_names.remove(old_display_name_key)?;
                                    joined.remove(key.as_str())?;
                                    invited.remove(key.as_str())?;
                                }
                            }

                            members.insert(
                                key.as_str(),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;

                            if let Some(profile) =
                                profile_changes.map(|p| p.get(&event.state_key)).flatten()
                            {
                                profiles.insert(
                                    key.as_str(),
                                    self.serialize_event(&profile)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    for (event_type, event) in &changes.account_data {
                        account_data.insert(
                            event_type.as_str(),
                            self.serialize_event(&event)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room, events) in &changes.room_account_data {
                        for (event_type, event) in events {
                            room_account_data.insert(
                                format!("{}{}", room.as_str(), event_type).as_str(),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.state {
                        for events in event_types.values() {
                            for event in events.values() {
                                state.insert(
                                    format!(
                                        "{}{}{}",
                                        room.as_str(),
                                        event.content().event_type(),
                                        event.state_key(),
                                    )
                                    .as_bytes(),
                                    self.serialize_event(&event)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    for (room_id, room_info) in &changes.room_infos {
                        rooms.insert(
                            room_id.as_bytes(),
                            self.serialize_event(room_info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (sender, event) in &changes.presence {
                        presence.insert(
                            sender.as_bytes(),
                            self.serialize_event(&event)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room_id, info) in &changes.invited_room_info {
                        striped_rooms.insert(
                            room_id.as_str(),
                            self.serialize_event(&info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room, events) in &changes.stripped_members {
                        for event in events.values() {
                            stripped_members.insert(
                                format!("{}{}", room.as_str(), &event.state_key).as_str(),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.stripped_state {
                        for events in event_types.values() {
                            for event in events.values() {
                                stripped_state.insert(
                                    format!(
                                        "{}{}{}",
                                        room.as_str(),
                                        event.content().event_type(),
                                        event.state_key(),
                                    )
                                    .as_bytes(),
                                    self.serialize_event(&event)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    Ok(())
                },
            );

        ret?;

        self.inner.flush_async().await?;

        info!("Saved changes in {:?}", now.elapsed());

        Ok(())
    }

    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<PresenceEvent>> {
        Ok(self
            .presence
            .get(user_id.as_bytes())?
            .map(|e| self.deserialize_event(&e))
            .transpose()?)
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<AnySyncStateEvent>> {
        Ok(self
            .room_state
            .get(format!("{}{}{}", room_id.as_str(), event_type, state_key).as_bytes())?
            .map(|e| self.deserialize_event(&e))
            .transpose()?)
    }

    pub async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEventContent>> {
        Ok(self
            .profiles
            .get(format!("{}{}", room_id.as_str(), user_id.as_str()))?
            .map(|p| self.deserialize_event(&p))
            .transpose()?)
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        Ok(self
            .members
            .get(format!("{}{}", room_id.as_str(), state_key.as_str()))?
            .map(|v| self.deserialize_event(&v))
            .transpose()?)
    }

    pub async fn get_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> impl Stream<Item = Result<UserId>> {
        stream::iter(
            self.invited_user_ids
                .scan_prefix(room_id.as_str())
                .map(|u| {
                    UserId::try_from(String::from_utf8_lossy(&u?.1).to_string())
                        .map_err(StoreError::Identifier)
                }),
        )
    }

    pub async fn get_joined_user_ids(
        &self,
        room_id: &RoomId,
    ) -> impl Stream<Item = Result<UserId>> {
        stream::iter(self.joined_user_ids.scan_prefix(room_id.as_str()).map(|u| {
            UserId::try_from(String::from_utf8_lossy(&u?.1).to_string())
                .map_err(StoreError::Identifier)
        }))
    }

    pub async fn get_room_infos(&self) -> impl Stream<Item = Result<RoomInfo>> {
        let db = self.clone();
        stream::iter(
            self.room_info
                .iter()
                .map(move |r| db.deserialize_event(&r?.1).map_err(|e| e.into())),
        )
    }

    pub async fn get_stripped_room_infos(&self) -> impl Stream<Item = Result<StrippedRoomInfo>> {
        let db = self.clone();
        stream::iter(
            self.stripped_room_info
                .iter()
                .map(move |r| db.deserialize_event(&r?.1).map_err(|e| e.into())),
        )
    }

    pub async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<UserId>> {
        let key = (room_id.as_str(), display_name).encode();

        self.display_names
            .scan_prefix(key)
            .map(|u| {
                UserId::try_from(String::from_utf8_lossy(&u?.1).to_string())
                    .map_err(StoreError::Identifier)
            })
            .collect()
    }
}

#[async_trait]
impl StateStore for SledStore {
    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.save_filter(filter_name, filter_id).await
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        self.save_changes(changes).await
    }

    async fn get_filter(&self, filter_id: &str) -> Result<Option<String>> {
        self.get_filter(filter_id).await
    }

    async fn get_sync_token(&self) -> Result<Option<String>> {
        self.get_sync_token().await
    }

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<PresenceEvent>> {
        self.get_presence_event(user_id).await
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<AnySyncStateEvent>> {
        self.get_state_event(room_id, event_type, state_key).await
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEventContent>> {
        self.get_profile(room_id, user_id).await
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        self.get_member_event(room_id, state_key).await
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        self.get_invited_user_ids(room_id).await.try_collect().await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        self.get_joined_user_ids(room_id).await.try_collect().await
    }

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        self.get_room_infos().await.try_collect().await
    }

    async fn get_stripped_room_infos(&self) -> Result<Vec<StrippedRoomInfo>> {
        self.get_stripped_room_infos().await.try_collect().await
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<UserId>> {
        self.get_users_with_display_name(room_id, display_name)
            .await
    }
}

#[cfg(test)]
mod test {
    use std::{convert::TryFrom, time::SystemTime};

    use matrix_sdk_common::{
        events::{
            room::member::{MemberEventContent, MembershipState},
            Unsigned,
        },
        identifiers::{room_id, user_id, EventId, UserId},
    };
    use matrix_sdk_test::async_test;

    use super::{SledStore, StateChanges};
    use crate::deserialized_responses::MemberEvent;

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    fn membership_event() -> MemberEvent {
        let content = MemberEventContent {
            avatar_url: None,
            displayname: None,
            is_direct: None,
            third_party_invite: None,
            membership: MembershipState::Join,
        };

        MemberEvent {
            event_id: EventId::try_from("$h29iv0s8:example.com").unwrap(),
            content,
            sender: user_id(),
            origin_server_ts: SystemTime::now(),
            state_key: user_id(),
            prev_content: None,
            unsigned: Unsigned::default(),
        }
    }

    #[async_test]
    async fn test_member_saving() {
        let store = SledStore::open().unwrap();
        let room_id = room_id!("!test:localhost");
        let user_id = user_id();

        assert!(store
            .get_member_event(&room_id, &user_id)
            .await
            .unwrap()
            .is_none());
        let mut changes = StateChanges::default();
        changes
            .members
            .entry(room_id.clone())
            .or_default()
            .insert(user_id.clone(), membership_event());

        store.save_changes(&changes).await.unwrap();
        assert!(store
            .get_member_event(&room_id, &user_id)
            .await
            .unwrap()
            .is_some());
    }
}
