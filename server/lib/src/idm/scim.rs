use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};

use compact_jwt::{Jws, JwsCompact, JwsEs256Signer, JwsSigner};
use kanidm_proto::internal::{ApiTokenPurpose, ScimSyncToken};
use kanidm_proto::scim_v1::*;
use std::collections::{BTreeMap, BTreeSet};

use crate::credential::totp::{Totp, TotpAlgo, TotpDigits};
use crate::idm::server::{IdmServerProxyReadTransaction, IdmServerProxyWriteTransaction};
use crate::prelude::*;
use crate::value::ApiToken;

use crate::schema::{SchemaClass, SchemaTransaction};
use sshkey_attest::proto::PublicKey as SshPublicKey;

// Internals of a Scim Sync token

#[allow(dead_code)]
pub(crate) struct SyncAccount {
    pub name: String,
    pub uuid: Uuid,
    pub sync_tokens: BTreeMap<Uuid, ApiToken>,
    pub jws_key: Option<JwsEs256Signer>,
}

macro_rules! try_from_entry {
    ($value:expr) => {{
        // Check the classes
        if !$value.attribute_equality(Attribute::Class, &EntryClass::SyncAccount.into()) {
            return Err(OperationError::MissingClass(ENTRYCLASS_SYNC_ACCOUNT.into()));
        }

        let name = $value
            .get_ava_single_iname(Attribute::Name)
            .map(|s| s.to_string())
            .ok_or(OperationError::MissingAttribute(Attribute::Name))?;

        let jws_key = $value
            .get_ava_single_jws_key_es256(Attribute::JwsEs256PrivateKey)
            .cloned()
            .map(|jws_key| {
                jws_key
                    .set_sign_option_embed_jwk(true)
                    .set_sign_option_legacy_kid(true)
            });

        let sync_tokens = $value
            .get_ava_as_apitoken_map(Attribute::SyncTokenSession)
            .cloned()
            .unwrap_or_default();

        let uuid = $value.get_uuid().clone();

        Ok(SyncAccount {
            name,
            uuid,
            sync_tokens,
            jws_key,
        })
    }};
}

impl SyncAccount {
    #[instrument(level = "debug", skip_all)]
    pub(crate) fn try_from_entry_rw(
        value: &Entry<EntrySealed, EntryCommitted>,
        // qs: &mut QueryServerWriteTransaction,
    ) -> Result<Self, OperationError> {
        // let groups = Group::try_from_account_entry_rw(value, qs)?;
        try_from_entry!(value)
    }

    pub(crate) fn check_sync_token_valid(
        _ct: Duration,
        sst: &ScimSyncToken,
        entry: &Entry<EntrySealed, EntryCommitted>,
    ) -> bool {
        let valid_purpose = matches!(sst.purpose, ApiTokenPurpose::Synchronise);

        // Get the sessions. There are no gracewindows on sync, we are much stricter.
        let session_present = entry
            .get_ava_as_apitoken_map(Attribute::SyncTokenSession)
            .map(|session_map| session_map.get(&sst.token_id).is_some())
            .unwrap_or(false);

        debug!(?session_present, valid_purpose);

        session_present && valid_purpose
    }
}

// Need to create a Sync input source
//

pub struct GenerateScimSyncTokenEvent {
    // Who initiated this?
    pub ident: Identity,
    // Who is it targeting?
    pub target: Uuid,
    // The label
    pub label: String,
}

impl GenerateScimSyncTokenEvent {
    #[cfg(test)]
    pub fn new_internal(target: Uuid, label: &str) -> Self {
        GenerateScimSyncTokenEvent {
            ident: Identity::from_internal(),
            target,
            label: label.to_string(),
        }
    }
}

impl IdmServerProxyWriteTransaction<'_> {
    pub fn scim_sync_generate_token(
        &mut self,
        gte: &GenerateScimSyncTokenEvent,
        ct: Duration,
    ) -> Result<JwsCompact, OperationError> {
        // Get the target signing key.
        let sync_account = self
            .qs_write
            .internal_search_uuid(gte.target)
            .and_then(|entry| SyncAccount::try_from_entry_rw(&entry))
            .map_err(|e| {
                admin_error!(?e, "Failed to search service account");
                e
            })?;

        let session_id = Uuid::new_v4();
        let issued_at = time::OffsetDateTime::UNIX_EPOCH + ct;

        let scope = ApiTokenScope::Synchronise;
        let purpose = scope.try_into()?;

        let session = Value::ApiToken(
            session_id,
            ApiToken {
                label: gte.label.clone(),
                expiry: None,
                // Need the other inner bits?
                // for the gracewindow.
                issued_at,
                // Who actually created this?
                issued_by: gte.ident.get_event_origin_id(),
                // What is the access scope of this session? This is
                // for auditing purposes.
                scope,
            },
        );

        let scim_sync_token = ScimSyncToken {
            token_id: session_id,
            issued_at,
            purpose,
        };

        let token = Jws::into_json(&scim_sync_token).map_err(|err| {
            error!(?err, "Unable to serialise JWS");
            OperationError::SerdeJsonError
        })?;

        let modlist =
            ModifyList::new_list(vec![Modify::Present(Attribute::SyncTokenSession, session)]);

        self.qs_write
            .impersonate_modify(
                // Filter as executed
                &filter!(f_eq(Attribute::Uuid, PartialValue::Uuid(gte.target))),
                // Filter as intended (acp)
                &filter_all!(f_eq(Attribute::Uuid, PartialValue::Uuid(gte.target))),
                &modlist,
                // Provide the event to impersonate
                &gte.ident,
            )
            .map_err(|err| {
                error!(?err, "Failed to generate sync token");
                err
            })?;

        // The modify succeeded and was allowed, now sign the token for return.
        if self.qs_write.get_domain_version() < DOMAIN_LEVEL_6 {
            sync_account
                .jws_key
                .as_ref()
                .ok_or_else(|| {
                    admin_error!("Unable to sign sync token, no sync keys available");
                    OperationError::CryptographyError
                })
                .and_then(|jws_key| {
                    jws_key.sign(&token).map_err(|err| {
                        admin_error!(?err, "Unable to sign sync token");
                        OperationError::CryptographyError
                    })
                })
        } else {
            self.qs_write
                .get_domain_key_object_handle()?
                .jws_es256_sign(&token, ct)
        }
        // Done!
    }

    pub fn sync_account_destroy_token(
        &mut self,
        ident: &Identity,
        target: Uuid,
        _ct: Duration,
    ) -> Result<(), OperationError> {
        let modlist = ModifyList::new_list(vec![Modify::Purged(Attribute::SyncTokenSession)]);

        self.qs_write
            .impersonate_modify(
                // Filter as executed
                &filter!(f_eq(Attribute::Uuid, PartialValue::Uuid(target))),
                // Filter as intended (acp)
                &filter!(f_eq(Attribute::Uuid, PartialValue::Uuid(target))),
                &modlist,
                // Provide the event to impersonate
                ident,
            )
            .map_err(|e| {
                admin_error!("Failed to destroy api token {:?}", e);
                e
            })
    }
}

pub struct ScimSyncFinaliseEvent {
    pub ident: Identity,
    pub target: Uuid,
}

impl IdmServerProxyWriteTransaction<'_> {
    pub fn scim_sync_finalise(
        &mut self,
        sfe: &ScimSyncFinaliseEvent,
    ) -> Result<(), OperationError> {
        // Get the target and ensure it's really a sync account
        let entry = self
            .qs_write
            .internal_search_uuid(sfe.target)
            .map_err(|e| {
                admin_error!(?e, "Failed to search sync account");
                e
            })?;

        let sync_account = SyncAccount::try_from_entry_rw(&entry).map_err(|e| {
            admin_error!(?e, "Failed to convert sync account");
            e
        })?;
        let sync_uuid = sync_account.uuid;

        // Do we have permission to delete it?
        let effective_perms = self
            .qs_write
            .get_accesscontrols()
            .effective_permission_check(&sfe.ident, Some(BTreeSet::default()), &[entry])?;

        let eperm = effective_perms.first().ok_or_else(|| {
            admin_error!("Effective Permission check returned no results");
            OperationError::InvalidState
        })?;

        if eperm.target != sync_account.uuid {
            admin_error!("Effective Permission check target differs from requested entry uuid");
            return Err(OperationError::InvalidEntryState);
        }

        // ⚠️  Assume that anything before this line is unauthorised, and after this line IS
        // authorised!
        //
        // We do this check via effective permissions because a lot of the operations that
        // follow will require permissions beyond what system admins have.

        if !eperm.delete {
            security_info!(
                "Requester {} does not have permission to delete sync account {}",
                sfe.ident,
                sync_account.name
            );
            return Err(OperationError::NotAuthorised);
        }

        // Referential integrity tries to assert that the reference to sync_parent_uuid is valid
        // from within the recycle bin. To prevent this, we have to "finalise" first, transfer
        // authority to kanidm, THEN we do the delete which breaks the reference requirement.
        //
        // Importantly, we have to do this for items that are in the recycle bin!

        // First, get the set of uuids that exist. We need this so we have the set of uuids we'll
        // be deleting *at the end*.
        let f_all_sync = filter_all!(f_and!([
            f_eq(Attribute::Class, EntryClass::SyncObject.into()),
            f_eq(Attribute::SyncParentUuid, PartialValue::Refer(sync_uuid))
        ]));

        // TODO: This could benefit from a search that only grabs uuids?
        let existing_entries = self
            .qs_write
            // .internal_search(f_all_sync.clone())
            .internal_exists(f_all_sync.clone())
            .inspect_err(|_e| {
                error!("Failed to determine existing entries set");
            })?;

        /*
        let filter_or: Vec<_> = existing_entries
            .iter()
            .map(|e| f_eq(Attribute::Uuid, PartialValue::Uuid(e.get_uuid())))
            .collect();
        */

        // We only need to delete the sync account itself.
        let delete_filter = filter!(f_eq(Attribute::Uuid, PartialValue::Uuid(sync_uuid)));

        if existing_entries {
            // Now modify these to remove their sync related attributes.
            let schema = self.qs_write.get_schema();
            let sync_class = schema
                .get_classes()
                .get(EntryClass::SyncObject.into())
                .ok_or_else(|| {
                    error!(
                        "Failed to access {} class, schema corrupt",
                        EntryClass::SyncObject
                    );
                    OperationError::InvalidState
                })?;

            let modlist = std::iter::once(Modify::Removed(
                Attribute::Class,
                EntryClass::SyncObject.into(),
            ))
            .chain(
                sync_class
                    .may_iter()
                    .map(|aname| Modify::Purged(aname.clone())),
            )
            .collect();

            let mods = ModifyList::new_list(modlist);

            self.qs_write
                .internal_modify(&f_all_sync, &mods)
                .inspect_err(|_e| {
                    error!("Failed to modify sync objects to grant authority to kanidm");
                })?;
        };

        self.qs_write
            .internal_delete(&delete_filter)
            .inspect_err(|e| {
                error!(?e, "Failed to terminate sync account");
            })
    }
}

pub struct ScimSyncTerminateEvent {
    pub ident: Identity,
    pub target: Uuid,
}

impl IdmServerProxyWriteTransaction<'_> {
    pub fn scim_sync_terminate(
        &mut self,
        ste: &ScimSyncTerminateEvent,
    ) -> Result<(), OperationError> {
        // Get the target and ensure it's really a sync account
        let entry = self
            .qs_write
            .internal_search_uuid(ste.target)
            .inspect_err(|e| {
                admin_error!(?e, "Failed to search sync account");
            })?;

        let sync_account = SyncAccount::try_from_entry_rw(&entry).map_err(|e| {
            admin_error!(?e, "Failed to convert sync account");
            e
        })?;
        let sync_uuid = sync_account.uuid;

        // Do we have permission to delete it?
        let effective_perms = self
            .qs_write
            .get_accesscontrols()
            .effective_permission_check(&ste.ident, Some(BTreeSet::default()), &[entry])?;

        let eperm = effective_perms.first().ok_or_else(|| {
            admin_error!("Effective Permission check returned no results");
            OperationError::InvalidState
        })?;

        if eperm.target != sync_account.uuid {
            admin_error!("Effective Permission check target differs from requested entry uuid");
            return Err(OperationError::InvalidEntryState);
        }

        // ⚠️  Assume that anything before this line is unauthorised, and after this line IS
        // authorised!
        //
        // We do this check via effective permissions because a lot of the operations that
        // follow will require permissions beyond what system admins have.

        if !eperm.delete {
            security_info!(
                "Requester {} does not have permission to delete sync account {}",
                ste.ident,
                sync_account.name
            );
            return Err(OperationError::NotAuthorised);
        }

        // Referential integrity tries to assert that the reference to sync_parent_uuid is valid
        // from within the recycle bin. To prevent this, we have to "finalise" first, transfer
        // authority to kanidm, THEN we do the delete which breaks the reference requirement.
        //
        // Importantly, we have to do this for items that are in the recycle bin!

        // First, get the set of uuids that exist. We need this so we have the set of uuids we'll
        // be deleting *at the end*.
        let f_all_sync = filter_all!(f_and!([
            f_eq(Attribute::Class, EntryClass::SyncObject.into()),
            f_eq(Attribute::SyncParentUuid, PartialValue::Refer(sync_uuid))
        ]));

        // TODO: This could benefit from a search that only grabs uuids?
        let existing_entries = self
            .qs_write
            .internal_search(f_all_sync.clone())
            .inspect_err(|err| {
                error!(?err, "Failed to determine existing entries set");
            })?;

        let delete_filter = if existing_entries.is_empty() {
            // We only need to delete the sync account itself.
            filter!(f_eq(Attribute::Uuid, PartialValue::Uuid(sync_uuid)))
        } else {
            // This is the delete filter we need later.
            let filter_or: Vec<_> = existing_entries
                .iter()
                .map(|e| f_eq(Attribute::Uuid, PartialValue::Uuid(e.get_uuid())))
                .collect();

            // Now modify these to remove their sync related attributes.
            let schema = self.qs_write.get_schema();
            let sync_class = schema
                .get_classes()
                .get(EntryClass::SyncObject.into())
                .ok_or_else(|| {
                    error!(
                        "Failed to access {} class, schema corrupt",
                        EntryClass::SyncObject
                    );
                    OperationError::InvalidState
                })?;

            let modlist = std::iter::once(Modify::Removed(
                Attribute::Class,
                EntryClass::SyncObject.into(),
            ))
            .chain(
                sync_class
                    .may_iter()
                    .map(|aname| Modify::Purged(aname.clone())),
            )
            .collect();

            let mods = ModifyList::new_list(modlist);

            self.qs_write
                .internal_modify(&f_all_sync, &mods)
                .inspect_err(|err| {
                    error!(
                        ?err,
                        "Failed to modify sync objects to grant authority to Kanidm"
                    );
                })?;

            filter!(f_or!([
                f_eq(Attribute::Uuid, PartialValue::Uuid(sync_uuid)),
                f_or(filter_or)
            ]))
        };

        self.qs_write.internal_delete(&delete_filter).map_err(|e| {
            error!(?e, "Failed to terminate sync account");
            e
        })
    }
}

pub struct ScimSyncUpdateEvent {
    pub ident: Identity,
}

impl IdmServerProxyWriteTransaction<'_> {
    #[instrument(level = "info", skip_all)]
    pub fn scim_sync_apply(
        &mut self,
        sse: &ScimSyncUpdateEvent,
        changes: &ScimSyncRequest,
        _ct: Duration,
    ) -> Result<(), OperationError> {
        let (sync_uuid, sync_authority_set, change_entries, sync_refresh) =
            self.scim_sync_apply_phase_1(sse, changes)?;

        // TODO: If the from_state is refresh and the to_state is active, then we need to
        // do delete all entries NOT present in the refresh set.
        // This accounts for the state of:
        //      active -> refresh -> active
        // which can occur when ldap asks us to do a refresh. Because of this entries may have
        // been removed, and will NOT be present in a delete_uuids phase. We can't just blanket
        // delete here as some entries may have been modified by users with authority over the
        // attributes.

        self.scim_sync_apply_phase_2(&change_entries, sync_uuid)?;

        // Remove dangling entries if this is a refresh operation.
        if sync_refresh {
            self.scim_sync_apply_phase_refresh_cleanup(&change_entries, sync_uuid)?;
        }

        // All stubs are now set-up. We can proceed to assert entry content.
        self.scim_sync_apply_phase_3(&change_entries, sync_uuid, &sync_authority_set)?;

        // Remove entries that now need deletion, We do this post assert in case an
        // entry was mistakenly ALSO in the assert set.
        self.scim_sync_apply_phase_4(&changes.retain, sync_uuid)?;

        // Final house keeping. Commit the new sync state.
        self.scim_sync_apply_phase_5(sync_uuid, &changes.to_state)
    }

    #[instrument(level = "debug", skip_all)]
    fn scim_sync_apply_phase_1<'b>(
        &mut self,
        sse: &'b ScimSyncUpdateEvent,
        changes: &'b ScimSyncRequest,
    ) -> Result<
        (
            Uuid,
            BTreeSet<Attribute>,
            BTreeMap<Uuid, &'b ScimEntry>,
            bool,
        ),
        OperationError,
    > {
        // Assert the token is valid.
        let sync_uuid = match &sse.ident.origin {
            IdentType::User(_) | IdentType::Internal => {
                warn!("Ident type is not synchronise");
                return Err(OperationError::AccessDenied);
            }
            IdentType::Synch(u) => {
                // Ok!
                *u
            }
        };

        match sse.ident.access_scope() {
            AccessScope::ReadOnly | AccessScope::ReadWrite => {
                warn!("Ident access scope is not synchronise");
                return Err(OperationError::AccessDenied);
            }
            AccessScope::Synchronise => {
                // As you were
            }
        };

        // Retrieve the related sync entry.
        let sync_entry = self
            .qs_write
            .internal_search_uuid(sync_uuid)
            .inspect_err(|err| {
                error!(
                    ?err,
                    "Failed to located sync entry related to {}", sync_uuid
                );
            })?;

        // Assert that the requested "from" state is consistent to this entry.
        // OperationError::InvalidSyncState

        match (
            &changes.from_state,
            sync_entry.get_ava_single_private_binary(Attribute::SyncCookie),
        ) {
            (ScimSyncState::Refresh, _) => {
                // valid
                info!("Refresh Sync");
            }
            (ScimSyncState::Active { cookie }, Some(sync_cookie)) => {
                // Check cookies.
                if cookie != sync_cookie {
                    // Invalid
                    error!(
                        "Invalid Sync State - Active, but agreement has divegent external cookie."
                    );
                    return Err(OperationError::InvalidSyncState);
                } else {
                    // Valid
                    info!("Active Sync with valid cookie");
                }
            }
            (ScimSyncState::Active { cookie: _ }, None) => {
                error!("Invalid Sync State - Sync Tool Reports Active, but agreement has Refresh Required. You can resync the agreement with `kanidm system sync force-refresh`");
                return Err(OperationError::InvalidSyncState);
            }
        };

        let sync_refresh = matches!(&changes.from_state, ScimSyncState::Refresh);

        // Get the sync authority set from the entry.
        let sync_authority_set: BTreeSet<Attribute> = sync_entry
            .get_ava_as_iutf8(Attribute::SyncYieldAuthority)
            .map(|set| set.iter().map(|s| Attribute::from(s.as_str())).collect())
            .unwrap_or_default();

        // Transform the changes into something that supports lookups.
        let change_entries: BTreeMap<Uuid, &ScimEntry> = changes
            .entries
            .iter()
            .map(|scim_entry| (scim_entry.id, scim_entry))
            .collect();

        Ok((sync_uuid, sync_authority_set, change_entries, sync_refresh))
    }

    #[instrument(level = "debug", skip_all)]
    pub(crate) fn scim_sync_apply_phase_2(
        &mut self,
        change_entries: &BTreeMap<Uuid, &ScimEntry>,
        sync_uuid: Uuid,
    ) -> Result<(), OperationError> {
        if change_entries.is_empty() {
            info!("No change_entries requested");
            return Ok(());
        }

        // First, search for all uuids present in the change set.
        // Note - we don't check the delete_uuids set here, that's done later. We use that
        // differently as we are somewhat more forgiving about reqs to delete uuids that are
        // already delete/tombstoned, or outside of the scope of this sync agreement.
        let filter_or = change_entries
            .keys()
            .copied()
            .map(|u| f_eq(Attribute::Uuid, PartialValue::Uuid(u)))
            .collect();

        // NOTE: We bypass recycled/ts here because we WANT to know if we are in that
        // state so we can AVOID updates to these entries!
        let existing_entries = self
            .qs_write
            .internal_search(filter_all!(f_or(filter_or)))
            .inspect_err(|err| {
                error!(?err, "Failed to determine existing entries set");
            })?;

        // Refuse to proceed if any entries are in the recycled or tombstone state, since subsequent
        // operations WOULD fail.
        //
        // I'm still a bit not sure what to do here though, because if we have uuid reuse from the
        // external system, that would be a pain, but I think we have to do this. This would be an
        // exceedingly rare situation though since 389-ds doesn't allow external uuid to be set, nor
        // does openldap. It would break both of their replication models for it to occur.
        //
        // Still we cover the possibility
        let mut fail = false;
        existing_entries.iter().for_each(|e| {
            if e.mask_recycled_ts().is_none() {
                error!("Unable to proceed: entry uuid {} ({}) is masked. You must re-map this entries uuid in the sync connector to proceed.", e.get_uuid(), e.get_display_id());
                fail = true;
            }
        });
        if fail {
            return Err(OperationError::InvalidEntryState);
        }
        // From that set of entries, partition to entries that exist and are
        // present, and entries that do not yet exist.
        //
        // We can't easily parititon here because we need to iterate over the
        // existing entry set to work out what we need, so what we do is copy
        // the change_entries set, then remove what we already have.
        let mut missing_scim = change_entries.clone();
        existing_entries.iter().for_each(|entry| {
            missing_scim.remove(&entry.get_uuid());
        });

        // For entries that do not exist, create stub entries. We don't create the external ID here
        // yet, because we need to ensure that it's unique.
        let create_stubs: Vec<EntryInitNew> = missing_scim
            .keys()
            .copied()
            .map(|u| {
                entry_init!(
                    (Attribute::Class, EntryClass::Object.to_value()),
                    (Attribute::Class, EntryClass::SyncObject.to_value()),
                    (Attribute::SyncParentUuid, Value::Refer(sync_uuid)),
                    (Attribute::Uuid, Value::Uuid(u))
                )
            })
            .collect();

        // We use internal create here to ensure that the values of these entries are all setup correctly.
        // We know that uuid won't conflict because it didn't exist in the previous search, so if we error
        // it has to be something bad.
        if !create_stubs.is_empty() {
            self.qs_write
                .internal_create(create_stubs)
                .inspect_err(|err| {
                    error!(?err, "Unable to create stub entries");
                })?;
        }

        // We have to search again now, this way we can do the internal mod process for
        // updating the external_id.
        //
        // For entries that do exist, mod their external_id
        //
        // Basically we just set this up as a batch modify and submit it.
        self.qs_write
            .internal_batch_modify(change_entries.iter().filter_map(|(u, scim_ent)| {
                // If the entry has an external id
                scim_ent.external_id.as_ref().map(|ext_id| {
                    // Add it to the mod request.
                    (
                        *u,
                        ModifyList::new_list(vec![
                            Modify::Assert(
                                Attribute::SyncParentUuid,
                                PartialValue::Refer(sync_uuid),
                            ),
                            Modify::Purged(Attribute::SyncExternalId),
                            Modify::Present(Attribute::SyncExternalId, Value::new_iutf8(ext_id)),
                        ]),
                    )
                })
            }))
            .inspect_err(|err| {
                error!(?err, "Unable to setup external ids from sync entries");
            })?;

        // Ready to go.

        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    pub(crate) fn scim_sync_apply_phase_refresh_cleanup(
        &mut self,
        change_entries: &BTreeMap<Uuid, &ScimEntry>,
        sync_uuid: Uuid,
    ) -> Result<(), OperationError> {
        // If this is a refresh, then the providing server is sending a full state of entries
        // and what state they should be in. This means that a situation can exist where on the
        // supplier you have:
        //
        //    Supplier           Kanidm
        //    Add X
        //    Sync X   ---------> X
        //    Delete X
        //    Refresh  --------->
        //
        // Since the delete uuid event wouldn't be sent, we need to ensure that Kanidm will clean
        // up entries that are *not* present in the change set here.
        //
        // To achieve this we do a delete where the condition is sync parent and not in the change
        // entry set.
        let filter_or = change_entries
            .keys()
            .copied()
            .map(|u| f_eq(Attribute::Uuid, PartialValue::Uuid(u)))
            .collect::<Vec<_>>();

        let delete_filter = if filter_or.is_empty() {
            filter!(f_and!([
                // Must be part of this sync agreement.
                f_eq(Attribute::SyncParentUuid, PartialValue::Refer(sync_uuid))
            ]))
        } else {
            filter!(f_and!([
                // Must be part of this sync agreement.
                f_eq(Attribute::SyncParentUuid, PartialValue::Refer(sync_uuid)),
                // Must not be an entry in the change set.
                f_andnot(f_or(filter_or))
            ]))
        };

        self.qs_write
            .internal_delete(&delete_filter)
            .or_else(|err| {
                // Skip if there is nothing to do
                if err == OperationError::NoMatchingEntries {
                    Ok(())
                } else {
                    Err(err)
                }
            })
            .map_err(|e| {
                error!(?e, "Failed to delete dangling uuids");
                e
            })
    }

    fn scim_attr_to_values(
        &mut self,
        scim_attr_name: &Attribute,
        scim_attr: &ScimValue,
    ) -> Result<Vec<Value>, OperationError> {
        let schema = self.qs_write.get_schema();

        let attr_schema = schema.get_attributes().get(scim_attr_name).ok_or_else(|| {
            OperationError::InvalidAttribute(format!(
                "No such attribute in schema - {scim_attr_name}"
            ))
        })?;

        match (attr_schema.syntax, attr_schema.multivalue, scim_attr) {
            (
                SyntaxType::Utf8StringIname,
                false,
                ScimValue::Simple(ScimAttr::String(value)),
            ) => Ok(vec![Value::new_iname(value)]),
            (
                SyntaxType::Utf8String,
                false,
                ScimValue::Simple(ScimAttr::String(value)),
            ) => Ok(vec![Value::new_utf8(value.clone())]),
            (
                SyntaxType::Utf8StringInsensitive,
                false,
                ScimValue::Simple(ScimAttr::String(value)),
            ) => Ok(vec![Value::new_iutf8(value)]),
            (
                SyntaxType::Uint32,
                false,
                ScimValue::Simple(ScimAttr::Integer(int_value)),
            ) => u32::try_from(*int_value).map_err(|_| {
                    error!("Invalid value - not within the bounds of a u32");
                    OperationError::InvalidAttribute(format!(
                        "Out of bounds unsigned integer - {scim_attr_name}"
                    ))
                })
                .map(|value| vec![Value::Uint32(value)]),
            (SyntaxType::ReferenceUuid, true, ScimValue::MultiComplex(values)) => {
                // In this case, because it's a reference uuid only, despite the multicomplex structure, it's a list of
                // "external_id" to external_ids. These *might* also be uuids. So we need to use sync_external_id_to_uuid
                // here to resolve things.
                //
                // This is why in phase 2 we "precreate" all objects to make sure they resolve.
                //
                // If an id does NOT resolve, we warn and SKIP since it's possible it may have been filtered.

                let mut vs = Vec::with_capacity(values.len());
                for complex in values.iter() {
                    let external_id = complex.get("external_id").ok_or_else(|| {
                        error!("Invalid scim complex attr - missing required key external_id");
                        OperationError::InvalidAttribute(format!(
                            "missing required key external_id - {scim_attr_name}"
                        ))
                    })?;

                    let value = match external_id {
                        ScimAttr::String(value) => Ok(value.as_str()),
                        _ => {
                            error!("Invalid external_id attribute - must be scim simple string");
                            Err(OperationError::InvalidAttribute(format!(
                                "external_id must be scim simple string - {scim_attr_name}"
                            )))
                        }
                    }?;

                    let maybe_uuid =
                        self.qs_write.sync_external_id_to_uuid(value).map_err(|e| {
                            error!(?e, "Unable to resolve external_id to uuid");
                            e
                        })?;

                    if let Some(uuid) = maybe_uuid {
                        vs.push(Value::Refer(uuid))
                    } else {
                        debug!("Could not convert external_id to reference - {}", value);
                    }
                }
                Ok(vs)
            }
            (SyntaxType::TotpSecret, true, ScimValue::MultiComplex(values)) => {
                // We have to break down each complex value into a totp.
                let mut vs = Vec::with_capacity(values.len());
                for complex in values.iter() {
                    let external_id = complex
                        .get("external_id")
                        .ok_or_else(|| {
                            error!("Invalid scim complex attr - missing required key external_id");
                            OperationError::InvalidAttribute(format!(
                                "missing required key external_id - {scim_attr_name}"
                            ))
                        })
                        .and_then(|external_id| match external_id {
                            ScimAttr::String(value) => Ok(value.clone()),
                            _ => {
                                error!(
                                    "Invalid external_id attribute - must be scim simple string"
                                );
                                Err(OperationError::InvalidAttribute(format!(
                                    "external_id must be scim simple string - {scim_attr_name}"
                                )))
                            }
                        })?;

                    let secret = complex
                        .get(SCIM_SECRET)
                        .ok_or_else(|| {
                            error!("Invalid SCIM complex attr - missing required key secret");
                            OperationError::InvalidAttribute(format!(
                                "missing required key secret - {scim_attr_name}"
                            ))
                        })
                        .and_then(|secret| match secret {
                            ScimAttr::String(value) => {
                                STANDARD.decode(value.as_str())
                                    .map_err(|_| {
                                        error!("Invalid secret attribute - must be base64 string");
                                        OperationError::InvalidAttribute(format!(
                                            "secret must be base64 string - {scim_attr_name}"
                                        ))
                                    })
                            }
                            _ => {
                                error!("Invalid secret attribute - must be scim simple string");
                                Err(OperationError::InvalidAttribute(format!(
                                    "secret must be scim simple string - {scim_attr_name}"
                                )))
                            }
                        })?;

                    let algo = complex.get(SCIM_ALGO)
                        .ok_or_else(|| {
                            error!("Invalid scim complex attr - missing required key algo");
                            OperationError::InvalidAttribute(format!(
                                "missing required key algo - {scim_attr_name}"
                            ))
                        })
                        .and_then(|algo_str| {
                            match algo_str {
                                ScimAttr::String(value) => {
                                    match value.as_str() {
                                        "sha1" => Ok(TotpAlgo::Sha1),
                                        "sha256" => Ok(TotpAlgo::Sha256),
                                        "sha512" => Ok(TotpAlgo::Sha512),
                                        _ => {
                                            error!("Invalid algo attribute - must be one of sha1, sha256 or sha512");
                                            Err(OperationError::InvalidAttribute(format!(
                                                "algo must be one of sha1, sha256 or sha512 - {scim_attr_name}"
                                            )))
                                        }
                                    }
                                }
                                _ => {
                                    error!("Invalid algo attribute - must be scim simple string");
                                    Err(OperationError::InvalidAttribute(format!(
                                        "algo must be scim simple string - {scim_attr_name}"
                                    )))
                                }
                            }
                        })?;

                    let step = complex.get(SCIM_STEP).ok_or_else(|| {
                        error!("Invalid scim complex attr - missing required key step");
                        OperationError::InvalidAttribute(format!(
                            "missing required key step - {scim_attr_name}"
                        ))
                    }).and_then(|step| {
                        match step {
                            ScimAttr::Integer(s) if *s >= 30 => Ok(*s as u64),
                            _ =>
                                Err(OperationError::InvalidAttribute(format!(
                                    "step must be a positive integer value equal to or greater than 30 - {scim_attr_name}"
                                ))),
                        }
                    })?;

                    let digits = complex
                        .get(SCIM_DIGITS)
                        .ok_or_else(|| {
                            error!("Invalid scim complex attr - missing required key digits");
                            OperationError::InvalidAttribute(format!(
                                "missing required key digits - {scim_attr_name}"
                            ))
                        })
                        .and_then(|digits| match digits {
                            ScimAttr::Integer(6) => Ok(TotpDigits::Six),
                            ScimAttr::Integer(8) => Ok(TotpDigits::Eight),
                            _ => {
                                error!("Invalid digits attribute - must be scim simple integer with the value 6 or 8");
                                Err(OperationError::InvalidAttribute(format!(
                                    "digits must be a positive integer value of 6 OR 8 - {scim_attr_name}"
                                )))
                            }
                        })?;

                    let totp = Totp::new(secret, step, algo, digits);
                    vs.push(Value::TotpSecret(external_id, totp))
                }
                Ok(vs)
            }
            (SyntaxType::EmailAddress, true, ScimValue::MultiComplex(values)) => {
                let mut vs = Vec::with_capacity(values.len());
                for complex in values.iter() {
                    let mail_addr = complex
                        .get("value")
                        .ok_or_else(|| {
                            error!("Invalid scim complex attr - missing required key value");
                            OperationError::InvalidAttribute(format!(
                                "missing required key value - {scim_attr_name}"
                            ))
                        })
                        .and_then(|external_id| match external_id {
                            ScimAttr::String(value) => Ok(value.clone()),
                            _ => {
                                error!("Invalid value attribute - must be scim simple string");
                                Err(OperationError::InvalidAttribute(format!(
                                    "value must be scim simple string - {scim_attr_name}"
                                )))
                            }
                        })?;

                    let primary = if let Some(primary) = complex.get("primary") {
                        match primary {
                            ScimAttr::Bool(value) => Ok(*value),
                            _ => {
                                error!("Invalid primary attribute - must be scim simple bool");
                                Err(OperationError::InvalidAttribute(format!(
                                    "primary must be scim simple bool - {scim_attr_name}"
                                )))
                            }
                        }?
                    } else {
                        false
                    };

                    vs.push(Value::EmailAddress(mail_addr, primary))
                }
                Ok(vs)
            }
            (SyntaxType::SshKey, true, ScimValue::MultiComplex(values)) => {
                let mut vs = Vec::with_capacity(values.len());
                for complex in values.iter() {
                    let label = complex
                        .get("label")
                        .ok_or_else(|| {
                            error!("Invalid scim complex attr - missing required key label");
                            OperationError::InvalidAttribute(format!(
                                "missing required key label - {scim_attr_name}"
                            ))
                        })
                        .and_then(|external_id| match external_id {
                            ScimAttr::String(value) => Ok(value.clone()),
                            _ => {
                                error!("Invalid value attribute - must be scim simple string");
                                Err(OperationError::InvalidAttribute(format!(
                                    "value must be scim simple string - {scim_attr_name}"
                                )))
                            }
                        })?;

                    let value = complex
                        .get("value")
                        .ok_or_else(|| {
                            error!("Invalid scim complex attr - missing required key value");
                            OperationError::InvalidAttribute(format!(
                                "missing required key value - {scim_attr_name}"
                            ))
                        })
                        .and_then(|external_id| match external_id {
                            ScimAttr::String(value) => SshPublicKey::from_string(value)
                                .map_err(|err| {
                                    error!(?err, "Invalid ssh key provided via scim");
                                    OperationError::SC0001IncomingSshPublicKey
                                }),
                            _ => {
                                error!("Invalid value attribute - must be scim simple string");
                                Err(OperationError::InvalidAttribute(format!(
                                    "value must be scim simple string - {scim_attr_name}"
                                )))
                            }
                        })?;

                    vs.push(Value::SshKey(label, value))
                }
                Ok(vs)
            }
            (
                SyntaxType::DateTime,
                false,
                ScimValue::Simple(ScimAttr::String(value)),
            ) => {
                Value::new_datetime_s(value)
                    .map(|v| vec![v])
                    .ok_or_else(|| {
                        error!("Invalid value attribute - must be scim simple string with rfc3339 formatted datetime");
                        OperationError::InvalidAttribute(format!(
                            "value must be scim simple string with rfc3339 formatted datetime - {scim_attr_name}"
                        ))
                    })
            }
            (syn, mv, sa) => {
                error!(?syn, ?mv, ?sa, "Unsupported scim attribute conversion. This may be a syntax error in your import, or a missing feature in Kanidm.");
                Err(OperationError::InvalidAttribute(format!(
                    "Unsupported attribute conversion - {scim_attr_name}"
                )))
            }
        }
    }

    fn scim_entry_to_mod(
        &mut self,
        scim_ent: &ScimEntry,
        sync_uuid: Uuid,
        sync_allow_class_set: &BTreeMap<String, SchemaClass>,
        sync_allow_attr_set: &BTreeSet<Attribute>,
        phantom_attr_set: &BTreeSet<Attribute>,
    ) -> Result<ModifyList<ModifyInvalid>, OperationError> {
        // What classes did they request for this entry to sync?
        let requested_classes = scim_ent.schemas.iter()
            .map(|schema| {
                schema.as_str().strip_prefix(SCIM_SCHEMA_SYNC_1)
                    .ok_or_else(|| {
                        error!(?schema, "Invalid requested schema - Not a kanidm sync schema.");
                        OperationError::InvalidEntryState
                    })
                    // Now look up if it's satisfiable.
                    .and_then(|cls_name| {
                        sync_allow_class_set.get_key_value(cls_name)
                        .ok_or_else(|| {
                            error!(?cls_name, "Invalid requested schema - Class does not exist in Kanidm or is not a sync_allowed class");
                            OperationError::InvalidEntryState
                        })
                    })
            })
            .collect::<Result<BTreeMap<&String, &SchemaClass>, _>>()?;

        // Get all the classes.
        debug!("Schemas valid - Proceeding with entry {}", scim_ent.id);

        #[allow(clippy::vec_init_then_push)]
        let mut mods = Vec::with_capacity(4);

        mods.push(Modify::Assert(
            Attribute::SyncParentUuid,
            PartialValue::Refer(sync_uuid),
        ));

        for req_class in requested_classes.keys() {
            mods.push(Modify::Present(
                Attribute::SyncClass,
                Value::new_iutf8(req_class),
            ));
            mods.push(Modify::Present(
                Attribute::Class,
                Value::new_iutf8(req_class),
            ));
        }

        // Clean up from removed classes. NEED THE OLD ENTRY FOR THIS.
        // Technically this is an EDGE case because in 99% of cases people aren't going to rug pull and REMOVE values on
        // ldap entries because of how it works.
        //
        // If we do decide to add this we use the sync_class attr to determine what was *previously* added to the object
        // rather than what we as kanidm added.
        //
        // We can then diff the sync_class from the set of req classes to work out what to remove.
        //
        // Cleaning up old attributes is weirder though. I'm not sure it's trivial or easy. Because we need to know if some attr X
        // is solely owned by that sync_class before we remove it, but it may not be. There could be two classes that allow it
        // and the other supporting class remains, so we shouldn't touch it. But then it has to be asked, where did it come from?
        // who owned it? Was it the sync side or kani? I think in general removal will be challenging.

        debug!(?requested_classes);

        // What attrs are owned by the set of requested classes?
        // We also need to account for phantom attrs somehow!
        //
        // - either we nominate phantom attrs on the classes they can import with
        //   or we need to always allow them?
        let sync_owned_attrs: BTreeSet<Attribute> = requested_classes
            .values()
            .flat_map(|cls| {
                cls.systemmay
                    .iter()
                    .chain(cls.may.iter())
                    .chain(cls.systemmust.iter())
                    .chain(cls.must.iter())
            })
            // Finally, establish if the attribute is syncable. Technically this could probe some attrs
            // multiple times due to how the loop is established, but in reality there are few attr overlaps.
            .filter(|a| sync_allow_attr_set.contains(*a))
            // Add in the set of phantom syncable attrs.
            .chain(phantom_attr_set.iter())
            .cloned()
            .collect();

        debug!(?sync_owned_attrs);

        for attr in sync_owned_attrs.iter() {
            if !phantom_attr_set.contains(attr) {
                // These are the attrs that are "real" and need to be cleaned out first.
                mods.push(Modify::Purged(attr.clone()));
            }
        }

        // For each attr in the scim entry, see if it's in the sync_owned set. If so, proceed.
        for (scim_attr_name, scim_attr) in scim_ent.attrs.iter() {
            let scim_attr_name = Attribute::from(scim_attr_name.as_str());

            if !sync_owned_attrs.contains(&scim_attr_name) {
                error!(
                    "Rejecting attribute {} for entry {} which is not sync owned",
                    scim_attr_name, scim_ent.id
                );
                return Err(OperationError::InvalidEntryState);
            }

            // Convert each scim_attr to a set of values.
            let values = self
                .scim_attr_to_values(&scim_attr_name, scim_attr)
                .inspect_err(|err| {
                    error!(
                        ?err,
                        "Failed to convert {} for entry {}", scim_attr_name, scim_ent.id
                    );
                })?;

            mods.extend(
                values
                    .into_iter()
                    .map(|val| Modify::Present(scim_attr_name.clone(), val)),
            );
        }

        trace!(?mods);

        Ok(ModifyList::new_list(mods))
    }

    #[instrument(level = "debug", skip_all)]
    pub(crate) fn scim_sync_apply_phase_3(
        &mut self,
        change_entries: &BTreeMap<Uuid, &ScimEntry>,
        sync_uuid: Uuid,
        sync_authority_set: &BTreeSet<Attribute>,
    ) -> Result<(), OperationError> {
        if change_entries.is_empty() {
            info!("No change_entries requested");
            return Ok(());
        }

        // Generally this is just assembling a large batch modify. Since we rely on external_id
        // to be present and valid, this is why we pre-apply that in phase 2.
        //
        // Another key point here is this is where we exclude changes to entries that our
        // domain has been granted authority over.
        //

        // The sync_allow_attr_set is what the sync connect *can* change. Authority is what the user
        // wants kani to control. As a result:
        //   sync_allow_attr = set of attrs from classes subtract attrs from authority.

        let schema = self.qs_write.get_schema();

        let class_snapshot = schema.get_classes();
        let attr_snapshot = schema.get_attributes();

        let sync_allow_class_set: BTreeMap<String, SchemaClass> = class_snapshot
            .values()
            .filter_map(|cls| {
                if cls.sync_allowed {
                    Some((cls.name.to_string(), cls.clone()))
                } else {
                    None
                }
            })
            .collect();

        let sync_allow_attr_set: BTreeSet<Attribute> = attr_snapshot
            .values()
            // Only add attrs to this if they are both sync allowed AND authority granted.
            .filter_map(|attr| {
                if attr.sync_allowed && !sync_authority_set.contains(&attr.name) {
                    Some(attr.name.clone())
                } else {
                    None
                }
            })
            .collect();

        let phantom_attr_set: BTreeSet<Attribute> = attr_snapshot
            .values()
            .filter_map(|attr| {
                if attr.phantom && attr.sync_allowed {
                    Some(attr.name.clone())
                } else {
                    None
                }
            })
            .collect();

        let asserts = change_entries
            .iter()
            .map(|(u, scim_ent)| {
                self.scim_entry_to_mod(
                    scim_ent,
                    sync_uuid,
                    &sync_allow_class_set,
                    &sync_allow_attr_set,
                    &phantom_attr_set,
                )
                .map(|e| (*u, e))
            })
            .collect::<Result<Vec<_>, _>>()?;

        // We can't just pass the above iter in here since it's fallible due to the
        // external resolve phase.

        self.qs_write
            .internal_batch_modify(asserts.into_iter())
            .inspect_err(|err| {
                error!(?err, "Unable to apply modifications to sync entries.");
            })
    }

    #[instrument(level = "debug", skip_all)]
    pub(crate) fn scim_sync_apply_phase_4(
        &mut self,
        retain: &ScimSyncRetentionMode,
        sync_uuid: Uuid,
    ) -> Result<(), OperationError> {
        let delete_filter = match retain {
            ScimSyncRetentionMode::Ignore => {
                info!("No retention mode requested");
                return Ok(());
            }
            ScimSyncRetentionMode::Retain(present_uuids) => {
                let filter_or = present_uuids
                    .iter()
                    .copied()
                    .map(|u| f_eq(Attribute::Uuid, PartialValue::Uuid(u)))
                    .collect::<Vec<_>>();

                if filter_or.is_empty() {
                    filter!(f_and!([
                        // F in chat for all these entries.
                        f_eq(Attribute::SyncParentUuid, PartialValue::Refer(sync_uuid))
                    ]))
                } else {
                    filter!(f_and!([
                        // Must be part of this sync agreement.
                        f_eq(Attribute::SyncParentUuid, PartialValue::Refer(sync_uuid)),
                        // Must not be an entry in the change set.
                        f_andnot(f_or(filter_or))
                    ]))
                }
            }
            ScimSyncRetentionMode::Delete(delete_uuids) => {
                if delete_uuids.is_empty() {
                    info!("No delete_uuids requested");
                    return Ok(());
                }

                // Search the set of delete_uuids that were requested.
                let filter_or = delete_uuids
                    .iter()
                    .copied()
                    .map(|u| f_eq(Attribute::Uuid, PartialValue::Uuid(u)))
                    .collect();

                // NOTE: We bypass recycled/ts here because we WANT to know if we are in that
                // state so we can AVOID updates to these entries!
                let delete_cands = self
                    .qs_write
                    .internal_search(filter_all!(f_or(filter_or)))
                    .inspect_err(|err| {
                        error!(?err, "Failed to determine existing entries set");
                    })?;

                let delete_filter = delete_cands
                    .into_iter()
                    .filter_map(|ent| {
                        if ent.mask_recycled_ts().is_none() {
                            debug!("Skipping already deleted entry {}", ent.get_uuid());
                            None
                        } else if ent.get_ava_single_refer(Attribute::SyncParentUuid)
                            != Some(sync_uuid)
                        {
                            warn!(
                                "Skipping entry that is not within sync control {}",
                                ent.get_uuid()
                            );
                            Some(Err(OperationError::AccessDenied))
                        } else {
                            Some(Ok(f_eq(
                                Attribute::Uuid,
                                PartialValue::Uuid(ent.get_uuid()),
                            )))
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                if delete_filter.is_empty() {
                    info!("No valid deletes requested");
                    return Ok(());
                }

                filter!(f_and(vec![
                    // Technically not needed, but it's better to add more safeties and this
                    // costs nothing to add.
                    f_eq(Attribute::SyncParentUuid, PartialValue::Refer(sync_uuid)),
                    f_or(delete_filter)
                ]))
            }
        };

        // Do the delete
        let res = self.qs_write.internal_delete(&delete_filter).map_err(|e| {
            error!(?e, "Failed to delete uuids");
            e
        });
        match res {
            Ok(()) => Ok(()),
            Err(OperationError::NoMatchingEntries) => {
                debug!("No deletes required");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    #[instrument(level = "debug", skip_all)]
    pub(crate) fn scim_sync_apply_phase_5(
        &mut self,
        sync_uuid: Uuid,
        to_state: &ScimSyncState,
    ) -> Result<(), OperationError> {
        // At this point everything is done. Now we do a final modify on the sync state entry
        // to reflect the new sync state.

        let modlist = match to_state {
            ScimSyncState::Active { cookie } => ModifyList::new_purge_and_set(
                Attribute::SyncCookie,
                Value::PrivateBinary(cookie.to_vec()),
            ),
            ScimSyncState::Refresh => ModifyList::new_purge(Attribute::SyncCookie),
        };

        self.qs_write
            .internal_modify_uuid(sync_uuid, &modlist)
            .inspect_err(|err| {
                error!(?err, "Failed to update sync entry state");
            })
    }
}

impl IdmServerProxyReadTransaction<'_> {
    pub fn scim_sync_get_state(
        &mut self,
        ident: &Identity,
    ) -> Result<ScimSyncState, OperationError> {
        // We must be *extra* careful in these functions since we do *internal* searches
        // which are *bypassing* normal access checks!

        // The ident *must* be a synchronise session.
        let sync_uuid = match &ident.origin {
            IdentType::User(_) | IdentType::Internal => {
                warn!("Ident type is not synchronise");
                return Err(OperationError::AccessDenied);
            }
            IdentType::Synch(u) => {
                // Ok!
                *u
            }
        };

        match ident.access_scope() {
            AccessScope::ReadOnly | AccessScope::ReadWrite => {
                warn!("Ident access scope is not synchronise");
                return Err(OperationError::AccessDenied);
            }
            AccessScope::Synchronise => {
                // As you were
            }
        };

        // Get the sync cookie of that session.
        let sync_entry = self.qs_read.internal_search_uuid(sync_uuid)?;

        Ok(
            match sync_entry.get_ava_single_private_binary(Attribute::SyncCookie) {
                Some(b) => ScimSyncState::Active { cookie: b.to_vec() },
                None => ScimSyncState::Refresh,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::idm::server::{IdmServerProxyWriteTransaction, IdmServerTransaction};
    use crate::prelude::*;
    use compact_jwt::traits::JwsVerifiable;
    use compact_jwt::{Jws, JwsCompact, JwsEs256Signer, JwsSigner};
    use kanidm_proto::internal::ApiTokenPurpose;
    use kanidm_proto::scim_v1::*;
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        GenerateScimSyncTokenEvent, ScimSyncFinaliseEvent, ScimSyncTerminateEvent, ScimSyncToken,
        ScimSyncUpdateEvent,
    };

    const TEST_CURRENT_TIME: u64 = 6000;

    fn create_scim_sync_account(
        idms_prox_write: &mut IdmServerProxyWriteTransaction<'_>,
        ct: Duration,
    ) -> (Uuid, JwsCompact) {
        let sync_uuid = Uuid::new_v4();

        let e1 = entry_init!(
            (Attribute::Class, EntryClass::Object.to_value()),
            (Attribute::Class, EntryClass::SyncAccount.to_value()),
            (Attribute::Name, Value::new_iname("test_scim_sync")),
            (Attribute::Uuid, Value::Uuid(sync_uuid)),
            (
                Attribute::Description,
                Value::new_utf8s("A test sync agreement")
            )
        );

        idms_prox_write
            .qs_write
            .internal_create(vec![e1])
            .expect("Failed to create sync account");

        let gte = GenerateScimSyncTokenEvent::new_internal(sync_uuid, "Sync Connector");

        let sync_token = idms_prox_write
            .scim_sync_generate_token(&gte, ct)
            .expect("failed to generate new scim sync token");

        (sync_uuid, sync_token)
    }

    #[idm_test]
    async fn test_idm_scim_sync_basic_function(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);

        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (sync_uuid, sync_token) = create_scim_sync_account(&mut idms_prox_write, ct);

        assert!(idms_prox_write.commit().is_ok());

        // Do a get_state to get the current "state cookie" if any.
        let mut idms_prox_read = idms.proxy_read().await.unwrap();

        let ident = idms_prox_read
            .validate_sync_client_auth_info_to_ident(sync_token.into(), ct)
            .expect("Failed to validate sync token");

        assert_eq!(Some(sync_uuid), ident.get_uuid());

        let sync_state = idms_prox_read
            .scim_sync_get_state(&ident)
            .expect("Failed to get current sync state");
        trace!(?sync_state);

        assert!(matches!(sync_state, ScimSyncState::Refresh));

        drop(idms_prox_read);
    }

    #[idm_test]
    async fn test_idm_scim_sync_token_security(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);

        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();

        let sync_uuid = Uuid::new_v4();

        let e1 = entry_init!(
            (Attribute::Class, EntryClass::Object.to_value()),
            (Attribute::Class, EntryClass::SyncAccount.to_value()),
            (Attribute::Name, Value::new_iname("test_scim_sync")),
            (Attribute::Uuid, Value::Uuid(sync_uuid)),
            (
                Attribute::Description,
                Value::new_utf8s("A test sync agreement")
            )
        );

        let ce = CreateEvent::new_internal(vec![e1]);
        let cr = idms_prox_write.qs_write.create(&ce);
        assert!(cr.is_ok());

        let gte = GenerateScimSyncTokenEvent::new_internal(sync_uuid, "Sync Connector");

        let sync_token = idms_prox_write
            .scim_sync_generate_token(&gte, ct)
            .expect("failed to generate new scim sync token");

        assert!(idms_prox_write.commit().is_ok());

        // -- Check the happy path.
        let mut idms_prox_read = idms.proxy_read().await.unwrap();
        let ident = idms_prox_read
            .validate_sync_client_auth_info_to_ident(sync_token.clone().into(), ct)
            .expect("Failed to validate sync token");
        assert_eq!(Some(sync_uuid), ident.get_uuid());
        drop(idms_prox_read);

        // -- Revoke the session

        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let me_inv_m = ModifyEvent::new_internal_invalid(
            filter!(f_eq(
                Attribute::Name,
                PartialValue::new_iname("test_scim_sync")
            )),
            ModifyList::new_list(vec![Modify::Purged(Attribute::SyncTokenSession)]),
        );
        assert!(idms_prox_write.qs_write.modify(&me_inv_m).is_ok());
        assert!(idms_prox_write.commit().is_ok());

        // Must fail
        let mut idms_prox_read = idms.proxy_read().await.unwrap();
        let fail =
            idms_prox_read.validate_sync_client_auth_info_to_ident(sync_token.clone().into(), ct);
        assert!(matches!(fail, Err(OperationError::NotAuthenticated)));
        drop(idms_prox_read);

        // -- New session, reset the JWS
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();

        let gte = GenerateScimSyncTokenEvent::new_internal(sync_uuid, "Sync Connector");
        let sync_token = idms_prox_write
            .scim_sync_generate_token(&gte, ct)
            .expect("failed to generate new scim sync token");

        let revoke_kid = sync_token.kid().expect("token does not contain a key id");

        idms_prox_write
            .qs_write
            .internal_modify_uuid(
                UUID_DOMAIN_INFO,
                &ModifyList::new_append(
                    Attribute::KeyActionRevoke,
                    Value::HexString(revoke_kid.to_string()),
                ),
            )
            .expect("Unable to revoke key");

        assert!(idms_prox_write.commit().is_ok());

        let mut idms_prox_read = idms.proxy_read().await.unwrap();
        let fail =
            idms_prox_read.validate_sync_client_auth_info_to_ident(sync_token.clone().into(), ct);
        assert!(matches!(fail, Err(OperationError::NotAuthenticated)));

        // -- Forge a session, use wrong types

        let sync_entry = idms_prox_read
            .qs_read
            .internal_search_uuid(sync_uuid)
            .expect("Unable to access sync entry");

        let sync_tokens = sync_entry
            .get_ava_as_apitoken_map(Attribute::SyncTokenSession)
            .cloned()
            .unwrap_or_default();

        // Steal these from the legit sesh.
        let (token_id, issued_at) = sync_tokens
            .iter()
            .next()
            .map(|(k, v)| (*k, v.issued_at))
            .expect("No sync tokens present");

        let purpose = ApiTokenPurpose::ReadWrite;

        let scim_sync_token = ScimSyncToken {
            token_id,
            issued_at,
            purpose,
        };

        let token = Jws::into_json(&scim_sync_token).expect("Unable to serialise forged token");

        let jws_key = JwsEs256Signer::generate_es256().expect("Unable to create signer");

        let forged_token = jws_key.sign(&token).expect("Unable to sign forged token");

        let fail = idms_prox_read.validate_sync_client_auth_info_to_ident(forged_token.into(), ct);
        assert!(matches!(fail, Err(OperationError::NotAuthenticated)));
    }

    fn test_scim_sync_apply_setup_ident(
        idms_prox_write: &mut IdmServerProxyWriteTransaction,
        ct: Duration,
    ) -> (Uuid, Identity) {
        let sync_uuid = Uuid::new_v4();

        let e1 = entry_init!(
            (Attribute::Class, EntryClass::Object.to_value()),
            (Attribute::Class, EntryClass::SyncAccount.to_value()),
            (Attribute::Name, Value::new_iname("test_scim_sync")),
            (Attribute::Uuid, Value::Uuid(sync_uuid)),
            (
                Attribute::Description,
                Value::new_utf8s("A test sync agreement")
            )
        );

        let ce = CreateEvent::new_internal(vec![e1]);
        let cr = idms_prox_write.qs_write.create(&ce);
        assert!(cr.is_ok());

        let gte = GenerateScimSyncTokenEvent::new_internal(sync_uuid, "Sync Connector");

        let sync_token = idms_prox_write
            .scim_sync_generate_token(&gte, ct)
            .expect("failed to generate new scim sync token");

        let ident = idms_prox_write
            .validate_sync_client_auth_info_to_ident(sync_token.into(), ct)
            .expect("Failed to process sync token to ident");

        (sync_uuid, ident)
    }

    #[idm_test]
    async fn test_idm_scim_sync_apply_phase_1_inconsistent(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            to_state: ScimSyncState::Refresh,
            entries: Vec::with_capacity(0),
            retain: ScimSyncRetentionMode::Ignore,
        };

        let res = idms_prox_write.scim_sync_apply_phase_1(&sse, &changes);

        assert!(matches!(res, Err(OperationError::InvalidSyncState)));

        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    async fn test_idm_scim_sync_apply_phase_2_basic(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let user_sync_uuid = uuid::uuid!("91b7aaf2-2445-46ce-8998-96d9f186cc69");

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            entries: vec![ScimEntry {
                schemas: vec![SCIM_SCHEMA_SYNC_PERSON.to_string()],
                id: user_sync_uuid,
                external_id: Some("dn=william,ou=people,dc=test".to_string()),
                meta: None,
                attrs: btreemap!((
                    Attribute::Name.to_string(),
                    ScimValue::Simple(ScimAttr::String("william".to_string()))
                ),),
            }],
            retain: ScimSyncRetentionMode::Ignore,
        };

        let (sync_uuid, _sync_authority_set, change_entries, _sync_refresh) = idms_prox_write
            .scim_sync_apply_phase_1(&sse, &changes)
            .expect("Failed to run phase 1");

        idms_prox_write
            .scim_sync_apply_phase_2(&change_entries, sync_uuid)
            .expect("Failed to run phase 2");

        let synced_entry = idms_prox_write
            .qs_write
            .internal_search_uuid(user_sync_uuid)
            .expect("Failed to access sync stub entry");

        assert!(
            synced_entry.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("dn=william,ou=people,dc=test")
        );
        assert_eq!(synced_entry.get_uuid(), user_sync_uuid);

        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    async fn test_idm_scim_sync_apply_phase_2_deny_on_tombstone(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);

        let user_sync_uuid = Uuid::new_v4();
        // Create a recycled entry
        assert!(idms_prox_write
            .qs_write
            .internal_create(vec![entry_init!(
                (Attribute::Class, EntryClass::Object.to_value()),
                (Attribute::Uuid, Value::Uuid(user_sync_uuid))
            )])
            .is_ok());

        assert!(idms_prox_write
            .qs_write
            .internal_delete_uuid(user_sync_uuid)
            .is_ok());

        // Now create a sync that conflicts with the tombstone uuid. This will be REJECTED.

        let sse = ScimSyncUpdateEvent { ident };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            entries: vec![ScimEntry {
                schemas: vec![SCIM_SCHEMA_SYNC_PERSON.to_string()],
                id: user_sync_uuid,
                external_id: Some("dn=william,ou=people,dc=test".to_string()),
                meta: None,
                attrs: btreemap!((
                    Attribute::Name.to_string(),
                    ScimValue::Simple(ScimAttr::String("william".to_string()))
                ),),
            }],
            retain: ScimSyncRetentionMode::Ignore,
        };

        let (sync_uuid, _sync_authority_set, change_entries, _sync_refresh) = idms_prox_write
            .scim_sync_apply_phase_1(&sse, &changes)
            .expect("Failed to run phase 1");

        let res = idms_prox_write.scim_sync_apply_phase_2(&change_entries, sync_uuid);

        assert!(matches!(res, Err(OperationError::InvalidEntryState)));

        assert!(idms_prox_write.commit().is_ok());
    }

    // Phase 3

    async fn apply_phase_3_test(
        idms: &IdmServer,
        entries: Vec<ScimEntry>,
    ) -> Result<(), OperationError> {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            entries,
            retain: ScimSyncRetentionMode::Ignore,
        };

        let (sync_uuid, sync_authority_set, change_entries, _sync_refresh) = idms_prox_write
            .scim_sync_apply_phase_1(&sse, &changes)
            .expect("Failed to run phase 1");

        assert!(idms_prox_write
            .scim_sync_apply_phase_2(&change_entries, sync_uuid)
            .is_ok());

        idms_prox_write
            .scim_sync_apply_phase_3(&change_entries, sync_uuid, &sync_authority_set)
            .and_then(|a| idms_prox_write.commit().map(|()| a))
    }

    #[idm_test]
    async fn test_idm_scim_sync_phase_3_basic(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let user_sync_uuid = Uuid::new_v4();

        assert!(apply_phase_3_test(
            idms,
            vec![ScimEntry {
                schemas: vec![SCIM_SCHEMA_SYNC_GROUP.to_string()],
                id: user_sync_uuid,
                external_id: Some("cn=testgroup,ou=people,dc=test".to_string()),
                meta: None,
                attrs: btreemap!((
                    Attribute::Name.to_string(),
                    ScimValue::Simple(ScimAttr::String("testgroup".to_string()))
                ),),
            }]
        )
        .await
        .is_ok());

        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();

        let ent = idms_prox_write
            .qs_write
            .internal_search_uuid(user_sync_uuid)
            .expect("Unable to access entry");

        assert_eq!(ent.get_ava_single_iname(Attribute::Name), Some("testgroup"));
        assert!(
            ent.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("cn=testgroup,ou=people,dc=test")
        );

        assert!(idms_prox_write.commit().is_ok());
    }

    // -- try to set uuid
    #[idm_test]
    async fn test_idm_scim_sync_phase_3_uuid_manipulation(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let user_sync_uuid = Uuid::new_v4();

        assert!(apply_phase_3_test(
            idms,
            vec![ScimEntry {
                schemas: vec![SCIM_SCHEMA_SYNC_GROUP.to_string()],
                id: user_sync_uuid,
                external_id: Some("cn=testgroup,ou=people,dc=test".to_string()),
                meta: None,
                attrs: btreemap!(
                    (
                        Attribute::Name.to_string(),
                        ScimValue::Simple(ScimAttr::String("testgroup".to_string()))
                    ),
                    (
                        Attribute::Uuid.to_string(),
                        ScimValue::Simple(ScimAttr::String(
                            "2c019619-f894-4a94-b356-05d371850e3d".to_string()
                        ))
                    )
                ),
            }]
        )
        .await
        .is_err());
    }

    // -- try to set sync_uuid / sync_object attrs
    #[idm_test]
    async fn test_idm_scim_sync_phase_3_sync_parent_uuid_manipulation(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let user_sync_uuid = Uuid::new_v4();

        assert!(apply_phase_3_test(
            idms,
            vec![ScimEntry {
                schemas: vec![SCIM_SCHEMA_SYNC_GROUP.to_string()],
                id: user_sync_uuid,
                external_id: Some("cn=testgroup,ou=people,dc=test".to_string()),
                meta: None,
                attrs: btreemap!(
                    (
                        Attribute::Name.to_string(),
                        ScimValue::Simple(ScimAttr::String("testgroup".to_string()))
                    ),
                    (
                        "sync_parent_uuid".to_string(),
                        ScimValue::Simple(ScimAttr::String(
                            "2c019619-f894-4a94-b356-05d371850e3d".to_string()
                        ))
                    )
                ),
            }]
        )
        .await
        .is_err());
    }

    // -- try to add class via class attr (not via scim schema)
    #[idm_test]
    async fn test_idm_scim_sync_phase_3_disallowed_class_forbidden(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let user_sync_uuid = Uuid::new_v4();

        assert!(apply_phase_3_test(
            idms,
            vec![ScimEntry {
                schemas: vec![SCIM_SCHEMA_SYNC_GROUP.to_string()],
                id: user_sync_uuid,
                external_id: Some("cn=testgroup,ou=people,dc=test".to_string()),
                meta: None,
                attrs: btreemap!(
                    (
                        Attribute::Name.to_string(),
                        ScimValue::Simple(ScimAttr::String("testgroup".to_string()))
                    ),
                    (
                        Attribute::Class.to_string(),
                        ScimValue::Simple(ScimAttr::String("posixgroup".to_string()))
                    )
                ),
            }]
        )
        .await
        .is_err());
    }

    // -- try to add class not in allowed class set (via scim schema)

    #[idm_test]
    async fn test_idm_scim_sync_phase_3_disallowed_class_system(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let user_sync_uuid = Uuid::new_v4();

        assert!(apply_phase_3_test(
            idms,
            vec![ScimEntry {
                schemas: vec![format!("{SCIM_SCHEMA_SYNC_1}system")],
                id: user_sync_uuid,
                external_id: Some("cn=testgroup,ou=people,dc=test".to_string()),
                meta: None,
                attrs: btreemap!((
                    Attribute::Name.to_string(),
                    ScimValue::Simple(ScimAttr::String("testgroup".to_string()))
                ),),
            }]
        )
        .await
        .is_err());
    }

    // Phase 4

    // Good delete - requires phase 5 due to need to do two syncs
    #[idm_test]
    async fn test_idm_scim_sync_phase_4_correct_delete(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let user_sync_uuid = Uuid::new_v4();
        // Create an entry via sync

        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent {
            ident: ident.clone(),
        };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            entries: vec![ScimEntry {
                schemas: vec![SCIM_SCHEMA_SYNC_GROUP.to_string()],
                id: user_sync_uuid,
                external_id: Some("cn=testgroup,ou=people,dc=test".to_string()),
                meta: None,
                attrs: btreemap!((
                    Attribute::Name.to_string(),
                    ScimValue::Simple(ScimAttr::String("testgroup".to_string()))
                ),),
            }],
            retain: ScimSyncRetentionMode::Ignore,
        };

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());
        assert!(idms_prox_write.commit().is_ok());

        // Now we can attempt the delete.
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let sse = ScimSyncUpdateEvent { ident };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            to_state: ScimSyncState::Active {
                cookie: vec![2, 3, 4, 5],
            },
            entries: vec![],
            retain: ScimSyncRetentionMode::Delete(vec![user_sync_uuid]),
        };

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        // Can't use internal_search_uuid since that applies a mask.
        assert!(idms_prox_write
            .qs_write
            .internal_search(filter_all!(f_eq(
                Attribute::Uuid,
                PartialValue::Uuid(user_sync_uuid)
            )))
            // Should be none as the entry was masked by being recycled.
            .map(|entries| {
                assert_eq!(entries.len(), 1);
                let ent = entries.first().unwrap();
                ent.mask_recycled_ts().is_none()
            })
            .unwrap_or(false));

        assert!(idms_prox_write.commit().is_ok());
    }

    // Delete that doesn't exist.
    #[idm_test]
    async fn test_idm_scim_sync_phase_4_nonexisting_delete(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            // Doesn't exist. If it does, then bless rng.
            entries: Vec::with_capacity(0),
            retain: ScimSyncRetentionMode::Delete(vec![Uuid::new_v4()]),
        };

        // Hard to know what was right here. IMO because it doesn't exist at all, we just ignore it
        // because the source sync is being overzealous, or it previously used to exist. Maybe
        // it was added and immediately removed. Either way, this is ok because we changed
        // nothing.
        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());
        assert!(idms_prox_write.commit().is_ok());
    }

    // Delete of something outside of agreement control - must fail.
    #[idm_test]
    async fn test_idm_scim_sync_phase_4_out_of_scope_delete(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();

        let user_sync_uuid = Uuid::new_v4();
        assert!(idms_prox_write
            .qs_write
            .internal_create(vec![entry_init!(
                (Attribute::Class, EntryClass::Object.to_value()),
                (Attribute::Uuid, Value::Uuid(user_sync_uuid))
            )])
            .is_ok());

        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            // Doesn't exist. If it does, then bless rng.
            entries: Vec::with_capacity(0),
            retain: ScimSyncRetentionMode::Delete(vec![user_sync_uuid]),
        };

        // Again, not sure what to do here. I think because this is clearly an overstep of the
        // rights of the delete_uuid request, this is an error here.
        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_err());
        // assert!(idms_prox_write.commit().is_ok());
    }

    // Delete already deleted entry.
    #[idm_test]
    async fn test_idm_scim_sync_phase_4_delete_already_deleted(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();

        let user_sync_uuid = Uuid::new_v4();
        assert!(idms_prox_write
            .qs_write
            .internal_create(vec![entry_init!(
                (Attribute::Class, EntryClass::Object.to_value()),
                (Attribute::Uuid, Value::Uuid(user_sync_uuid))
            )])
            .is_ok());

        assert!(idms_prox_write
            .qs_write
            .internal_delete_uuid(user_sync_uuid)
            .is_ok());

        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            // Doesn't exist. If it does, then bless rng.
            entries: Vec::with_capacity(0),
            retain: ScimSyncRetentionMode::Delete(vec![user_sync_uuid]),
        };

        // More subtely. There is clearly a theme here. In this case while the sync request
        // is trying to delete something out of scope and already deleted, since it already
        // is in a recycled state it doesn't matter, it's a no-op. We only care about when
        // the delete req applies to a live entry.
        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());
        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    async fn test_idm_scim_sync_phase_4_correct_retain(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        // Setup two entries.
        let sync_uuid_a = Uuid::new_v4();
        let sync_uuid_b = Uuid::new_v4();
        // Create an entry via sync

        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent {
            ident: ident.clone(),
        };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            entries: vec![
                ScimEntry {
                    schemas: vec![SCIM_SCHEMA_SYNC_GROUP.to_string()],
                    id: sync_uuid_a,
                    external_id: Some("cn=testgroup,ou=people,dc=test".to_string()),
                    meta: None,
                    attrs: btreemap!((
                        Attribute::Name.to_string(),
                        ScimValue::Simple(ScimAttr::String("testgroup".to_string()))
                    ),),
                },
                ScimEntry {
                    schemas: vec![SCIM_SCHEMA_SYNC_GROUP.to_string()],
                    id: sync_uuid_b,
                    external_id: Some("cn=anothergroup,ou=people,dc=test".to_string()),
                    meta: None,
                    attrs: btreemap!((
                        Attribute::Name.to_string(),
                        ScimValue::Simple(ScimAttr::String("anothergroup".to_string()))
                    ),),
                },
            ],
            retain: ScimSyncRetentionMode::Ignore,
        };

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());
        assert!(idms_prox_write.commit().is_ok());

        // Now retain only a single entry
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let sse = ScimSyncUpdateEvent { ident };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            to_state: ScimSyncState::Active {
                cookie: vec![2, 3, 4, 5],
            },
            entries: vec![],
            retain: ScimSyncRetentionMode::Retain(vec![sync_uuid_a]),
        };

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        // Can't use internal_search_uuid since that applies a mask.
        assert!(idms_prox_write
            .qs_write
            .internal_search(filter_all!(f_eq(
                Attribute::Uuid,
                PartialValue::Uuid(sync_uuid_b)
            )))
            // Should be none as the entry was masked by being recycled.
            .map(|entries| {
                assert_eq!(entries.len(), 1);
                let ent = entries.first().unwrap();
                ent.mask_recycled_ts().is_none()
            })
            .unwrap_or(false));

        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    async fn test_idm_scim_sync_phase_4_retain_none(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        // Setup two entries.
        let sync_uuid_a = Uuid::new_v4();
        let sync_uuid_b = Uuid::new_v4();

        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent {
            ident: ident.clone(),
        };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            entries: vec![
                ScimEntry {
                    schemas: vec![SCIM_SCHEMA_SYNC_GROUP.to_string()],
                    id: sync_uuid_a,
                    external_id: Some("cn=testgroup,ou=people,dc=test".to_string()),
                    meta: None,
                    attrs: btreemap!((
                        Attribute::Name.to_string(),
                        ScimValue::Simple(ScimAttr::String("testgroup".to_string()))
                    ),),
                },
                ScimEntry {
                    schemas: vec![SCIM_SCHEMA_SYNC_GROUP.to_string()],
                    id: sync_uuid_b,
                    external_id: Some("cn=anothergroup,ou=people,dc=test".to_string()),
                    meta: None,
                    attrs: btreemap!((
                        Attribute::Name.to_string(),
                        ScimValue::Simple(ScimAttr::String("anothergroup".to_string()))
                    ),),
                },
            ],
            retain: ScimSyncRetentionMode::Ignore,
        };

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());
        assert!(idms_prox_write.commit().is_ok());

        // Now retain no entries at all
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let sse = ScimSyncUpdateEvent { ident };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            to_state: ScimSyncState::Active {
                cookie: vec![2, 3, 4, 5],
            },
            entries: vec![],
            retain: ScimSyncRetentionMode::Retain(vec![]),
        };

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        // Can't use internal_search_uuid since that applies a mask.
        assert!(idms_prox_write
            .qs_write
            .internal_search(filter_all!(f_eq(
                Attribute::Uuid,
                PartialValue::Uuid(sync_uuid_a)
            )))
            // Should be none as the entry was masked by being recycled.
            .map(|entries| {
                assert_eq!(entries.len(), 1);
                let ent = entries.first().unwrap();
                ent.mask_recycled_ts().is_none()
            })
            .unwrap_or(false));

        // Can't use internal_search_uuid since that applies a mask.
        assert!(idms_prox_write
            .qs_write
            .internal_search(filter_all!(f_eq(
                Attribute::Uuid,
                PartialValue::Uuid(sync_uuid_b)
            )))
            // Should be none as the entry was masked by being recycled.
            .map(|entries| {
                assert_eq!(entries.len(), 1);
                let ent = entries.first().unwrap();
                ent.mask_recycled_ts().is_none()
            })
            .unwrap_or(false));

        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    async fn test_idm_scim_sync_phase_4_retain_no_deletes(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        // Setup two entries.
        let sync_uuid_a = Uuid::new_v4();

        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent {
            ident: ident.clone(),
        };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            entries: vec![ScimEntry {
                schemas: vec![SCIM_SCHEMA_SYNC_GROUP.to_string()],
                id: sync_uuid_a,
                external_id: Some("cn=testgroup,ou=people,dc=test".to_string()),
                meta: None,
                attrs: btreemap!((
                    Attribute::Name.to_string(),
                    ScimAttr::String("testgroup".to_string()).into()
                ),),
            }],
            retain: ScimSyncRetentionMode::Ignore,
        };

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());
        assert!(idms_prox_write.commit().is_ok());

        // Now retain no entries at all
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let sse = ScimSyncUpdateEvent { ident };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            to_state: ScimSyncState::Active {
                cookie: vec![2, 3, 4, 5],
            },
            entries: vec![],
            retain: ScimSyncRetentionMode::Retain(vec![sync_uuid_a]),
        };

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        // Entry still exists
        let ent = idms_prox_write
            .qs_write
            .internal_search_uuid(sync_uuid_a)
            .expect("Unable to access entry");

        assert_eq!(ent.get_ava_single_iname(Attribute::Name), Some("testgroup"));

        assert!(idms_prox_write.commit().is_ok());
    }

    // Phase 5
    #[idm_test]
    async fn test_idm_scim_sync_phase_5_from_refresh_to_active(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent {
            ident: ident.clone(),
        };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            entries: Vec::with_capacity(0),
            retain: ScimSyncRetentionMode::Ignore,
        };

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());
        assert!(idms_prox_write.commit().is_ok());

        // Advance the from -> to state.
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let sse = ScimSyncUpdateEvent { ident };

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            to_state: ScimSyncState::Active {
                cookie: vec![2, 3, 4, 5],
            },
            entries: vec![],
            retain: ScimSyncRetentionMode::Ignore,
        };

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());
        assert!(idms_prox_write.commit().is_ok());
    }

    // Test the client doing a sync refresh request (active -> refresh).

    // Real sample data test

    fn get_single_entry(
        name: &str,
        idms_prox_write: &mut IdmServerProxyWriteTransaction,
    ) -> Arc<EntrySealedCommitted> {
        idms_prox_write
            .qs_write
            .internal_search(filter!(f_eq(
                Attribute::Name,
                PartialValue::new_iname(name)
            )))
            .map_err(|_| ())
            .and_then(|mut entries| {
                if entries.len() != 1 {
                    error!("Incorrect number of results {:?}", entries);
                    Err(())
                } else {
                    entries.pop().ok_or(())
                }
            })
            .expect("Failed to access entry.")
    }

    #[idm_test]
    async fn test_idm_scim_sync_refresh_ipa_example_1(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_1).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        assert!(idms_prox_write.commit().is_ok());

        // Test properties of the imported entries.
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();

        let testgroup = get_single_entry("testgroup", &mut idms_prox_write);
        assert!(
            testgroup.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("cn=testgroup,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert!(testgroup
            .get_ava_single_uint32(Attribute::GidNumber)
            .is_none());

        let testposix = get_single_entry("testposix", &mut idms_prox_write);
        assert!(
            testposix.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("cn=testposix,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert_eq!(
            testposix.get_ava_single_uint32(Attribute::GidNumber),
            Some(1234567)
        );

        let testexternal = get_single_entry("testexternal", &mut idms_prox_write);
        assert!(
            testexternal.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("cn=testexternal,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert!(testexternal
            .get_ava_single_uint32(Attribute::GidNumber)
            .is_none());

        let testuser = get_single_entry("testuser", &mut idms_prox_write);
        assert!(
            testuser.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("uid=testuser,cn=users,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert_eq!(
            testuser.get_ava_single_uint32(Attribute::GidNumber),
            Some(12345)
        );
        assert_eq!(
            testuser.get_ava_single_utf8(Attribute::DisplayName),
            Some("Test User")
        );
        assert_eq!(
            testuser.get_ava_single_iutf8(Attribute::LoginShell),
            Some("/bin/sh")
        );

        let mut ssh_keyiter = testuser
            .get_ava_iter_sshpubkeys(Attribute::SshPublicKey)
            .expect("Failed to access ssh pubkeys");

        assert_eq!(ssh_keyiter.next(), Some("sk-ecdsa-sha2-nistp256@openssh.com AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20AAAAIbmlzdHAyNTYAAABBBENubZikrb8hu+HeVRdZ0pp/VAk2qv4JDbuJhvD0yNdWDL2e3cBbERiDeNPkWx58Q4rVnxkbV1fa8E2waRtT91wAAAAEc3NoOg== testuser@fidokey".to_string()));
        assert_eq!(ssh_keyiter.next(), None);

        // Check memberof works.
        let testgroup_mb = testgroup
            .get_ava_refer(Attribute::Member)
            .expect("No members!");
        assert!(testgroup_mb.contains(&testuser.get_uuid()));

        let testposix_mb = testposix
            .get_ava_refer(Attribute::Member)
            .expect("No members!");
        assert!(testposix_mb.contains(&testuser.get_uuid()));

        let testuser_mo = testuser
            .get_ava_refer(Attribute::MemberOf)
            .expect("No memberof!");
        assert!(testuser_mo.contains(&testposix.get_uuid()));
        assert!(testuser_mo.contains(&testgroup.get_uuid()));

        assert!(idms_prox_write.commit().is_ok());

        // Now apply updates.
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_2).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());
        assert!(idms_prox_write.commit().is_ok());

        // Test properties of the updated entries.
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();

        // Deleted
        assert!(idms_prox_write
            .qs_write
            .internal_search(filter!(f_eq(
                Attribute::Name,
                PartialValue::new_iname("testgroup")
            )))
            .unwrap()
            .is_empty());

        let testposix = get_single_entry("testposix", &mut idms_prox_write);
        info!("{:?}", testposix);
        assert!(
            testposix.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("cn=testposix,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert_eq!(
            testposix.get_ava_single_uint32(Attribute::GidNumber),
            Some(1234567)
        );

        let testexternal = get_single_entry("testexternal2", &mut idms_prox_write);
        info!("{:?}", testexternal);
        assert!(
            testexternal.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("cn=testexternal2,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert!(testexternal
            .get_ava_single_uint32(Attribute::GidNumber)
            .is_none());

        let testuser = get_single_entry("testuser", &mut idms_prox_write);

        // Check memberof works.
        let testexternal_mb = testexternal
            .get_ava_refer(Attribute::Member)
            .expect("No members!");
        assert!(testexternal_mb.contains(&testuser.get_uuid()));

        assert!(testposix.get_ava_refer(Attribute::Member).is_none());

        let testuser_mo = testuser
            .get_ava_refer(Attribute::MemberOf)
            .expect("No memberof!");
        assert!(testuser_mo.contains(&testexternal.get_uuid()));

        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    async fn test_idm_scim_sync_refresh_ipa_example_2(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_1).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        let from_state = changes.to_state.clone();

        // Indicate the next set of changes will be a refresh. Don't change content.
        // Strictly speaking this step isn't need.

        let changes = ScimSyncRequest::need_refresh(from_state);
        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        // Check entries still remain as expected.
        let testgroup = get_single_entry("testgroup", &mut idms_prox_write);
        assert!(
            testgroup.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("cn=testgroup,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert!(testgroup
            .get_ava_single_uint32(Attribute::GidNumber)
            .is_none());

        let testposix = get_single_entry("testposix", &mut idms_prox_write);
        assert!(
            testposix.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("cn=testposix,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert_eq!(
            testposix.get_ava_single_uint32(Attribute::GidNumber),
            Some(1234567)
        );

        let testexternal = get_single_entry("testexternal", &mut idms_prox_write);
        assert!(
            testexternal.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("cn=testexternal,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert!(testexternal
            .get_ava_single_uint32(Attribute::GidNumber)
            .is_none());

        let testuser = get_single_entry("testuser", &mut idms_prox_write);
        assert!(
            testuser.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("uid=testuser,cn=users,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert_eq!(
            testuser.get_ava_single_uint32(Attribute::GidNumber),
            Some(12345)
        );
        assert_eq!(
            testuser.get_ava_single_utf8(Attribute::DisplayName),
            Some("Test User")
        );
        assert_eq!(
            testuser.get_ava_single_iutf8(Attribute::LoginShell),
            Some("/bin/sh")
        );

        // Check memberof works.
        let testgroup_mb = testgroup
            .get_ava_refer(Attribute::Member)
            .expect("No members!");
        assert!(testgroup_mb.contains(&testuser.get_uuid()));

        let testposix_mb = testposix
            .get_ava_refer(Attribute::Member)
            .expect("No members!");
        assert!(testposix_mb.contains(&testuser.get_uuid()));

        let testuser_mo = testuser
            .get_ava_refer(Attribute::MemberOf)
            .expect("No memberof!");
        assert!(testuser_mo.contains(&testposix.get_uuid()));
        assert!(testuser_mo.contains(&testgroup.get_uuid()));

        // Now, the next change is the refresh.

        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_REFRESH_1).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        assert!(idms_prox_write
            .qs_write
            .internal_search(filter!(f_eq(
                Attribute::Name,
                PartialValue::new_iname("testposix")
            )))
            .unwrap()
            .is_empty());

        assert!(idms_prox_write
            .qs_write
            .internal_search(filter!(f_eq(
                Attribute::Name,
                PartialValue::new_iname("testexternal")
            )))
            .unwrap()
            .is_empty());

        let testgroup = get_single_entry("testgroup", &mut idms_prox_write);
        assert!(
            testgroup.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("cn=testgroup,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert!(testgroup
            .get_ava_single_uint32(Attribute::GidNumber)
            .is_none());

        let testuser = get_single_entry("testuser", &mut idms_prox_write);
        assert!(
            testuser.get_ava_single_iutf8(Attribute::SyncExternalId)
                == Some("uid=testuser,cn=users,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au")
        );
        assert_eq!(
            testuser.get_ava_single_uint32(Attribute::GidNumber),
            Some(12345)
        );
        assert_eq!(
            testuser.get_ava_single_utf8(Attribute::DisplayName),
            Some("Test User")
        );
        assert_eq!(
            testuser.get_ava_single_iutf8(Attribute::LoginShell),
            Some("/bin/sh")
        );

        // Check memberof works.
        let testgroup_mb = testgroup
            .get_ava_refer(Attribute::Member)
            .expect("No members!");
        assert!(testgroup_mb.contains(&testuser.get_uuid()));

        let testuser_mo = testuser
            .get_ava_refer(Attribute::MemberOf)
            .expect("No memberof!");
        assert!(testuser_mo.contains(&testgroup.get_uuid()));

        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    async fn test_idm_scim_sync_yield_authority(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_1).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        // Now we set the sync agreement to have description yielded.
        assert!(idms_prox_write
            .qs_write
            .internal_modify_uuid(
                sync_uuid,
                &ModifyList::new_purge_and_set(
                    Attribute::SyncYieldAuthority,
                    Value::new_iutf8(Attribute::LegalName.as_ref())
                )
            )
            .is_ok());

        let testuser_filter = filter!(f_eq(Attribute::Name, PartialValue::new_iname("testuser")));

        // We then can change our user.
        assert!(idms_prox_write
            .qs_write
            .internal_modify(
                &testuser_filter,
                &ModifyList::new_purge_and_set(
                    Attribute::LegalName,
                    Value::Utf8("Test Userington the First".to_string())
                )
            )
            .is_ok());

        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_REFRESH_1).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        // Finally, now the gidnumber should not have changed.
        let testuser = idms_prox_write
            .qs_write
            .internal_search(testuser_filter)
            .map(|mut results| results.pop().expect("Empty result set"))
            .expect("Failed to access testuser");

        assert!(
            testuser.get_ava_single_utf8(Attribute::LegalName) == Some("Test Userington the First")
        );

        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    async fn test_idm_scim_sync_finalise_1(idms: &IdmServer, _idms_delayed: &mut IdmServerDelayed) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_1).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        assert!(idms_prox_write.commit().is_ok());

        // Finalise the sync account.
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();

        let ident = idms_prox_write
            .qs_write
            .internal_search_uuid(UUID_ADMIN)
            .map(Identity::from_impersonate_entry_readwrite)
            .expect("Failed to get admin");

        let sfe = ScimSyncFinaliseEvent {
            ident,
            target: sync_uuid,
        };

        idms_prox_write
            .scim_sync_finalise(&sfe)
            .expect("Failed to finalise sync account");

        // Check that the entries still exists but now have no sync_object attached.
        let testgroup = get_single_entry("testgroup", &mut idms_prox_write);
        assert!(!testgroup.attribute_equality(Attribute::Class, &EntryClass::SyncObject.into()));

        let testposix = get_single_entry("testposix", &mut idms_prox_write);
        assert!(!testposix.attribute_equality(Attribute::Class, &EntryClass::SyncObject.into()));

        let testexternal = get_single_entry("testexternal", &mut idms_prox_write);
        assert!(!testexternal.attribute_equality(Attribute::Class, &EntryClass::SyncObject.into()));

        let testuser = get_single_entry("testuser", &mut idms_prox_write);
        assert!(!testuser.attribute_equality(Attribute::Class, &EntryClass::SyncObject.into()));

        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    async fn test_idm_scim_sync_finalise_2(idms: &IdmServer, _idms_delayed: &mut IdmServerDelayed) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_1).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        // The difference in this test is that the refresh deletes some entries
        // so the recycle bin case needs to be handled.
        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_REFRESH_1).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        assert!(idms_prox_write.commit().is_ok());

        // Finalise the sync account.
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();

        let ident = idms_prox_write
            .qs_write
            .internal_search_uuid(UUID_ADMIN)
            .map(Identity::from_impersonate_entry_readwrite)
            .expect("Failed to get admin");

        let sfe = ScimSyncFinaliseEvent {
            ident,
            target: sync_uuid,
        };

        idms_prox_write
            .scim_sync_finalise(&sfe)
            .expect("Failed to finalise sync account");

        // Check that the entries still exists but now have no sync_object attached.
        let testgroup = get_single_entry("testgroup", &mut idms_prox_write);
        assert!(!testgroup.attribute_equality(Attribute::Class, &EntryClass::SyncObject.into()));

        let testuser = get_single_entry("testuser", &mut idms_prox_write);
        assert!(!testuser.attribute_equality(Attribute::Class, &EntryClass::SyncObject.into()));

        for iname in ["testposix", "testexternal"] {
            trace!(%iname);
            assert!(idms_prox_write
                .qs_write
                .internal_search(filter!(f_eq(
                    Attribute::Name,
                    PartialValue::new_iname(iname)
                )))
                .unwrap()
                .is_empty());
        }

        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    async fn test_idm_scim_sync_terminate_1(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_1).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        assert!(idms_prox_write.commit().is_ok());

        // Terminate the sync account
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();

        let ident = idms_prox_write
            .qs_write
            .internal_search_uuid(UUID_ADMIN)
            .map(Identity::from_impersonate_entry_readwrite)
            .expect("Failed to get admin");

        let sfe = ScimSyncTerminateEvent {
            ident,
            target: sync_uuid,
        };

        idms_prox_write
            .scim_sync_terminate(&sfe)
            .expect("Failed to terminate sync account");

        // Check that the entries no longer exist
        for iname in ["testgroup", "testposix", "testexternal", "testuser"] {
            trace!(%iname);
            assert!(idms_prox_write
                .qs_write
                .internal_search(filter!(f_eq(
                    Attribute::Name,
                    PartialValue::new_iname(iname)
                )))
                .unwrap()
                .is_empty());
        }

        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    async fn test_idm_scim_sync_terminate_2(
        idms: &IdmServer,
        _idms_delayed: &mut IdmServerDelayed,
    ) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_1).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        // The difference in this test is that the refresh deletes some entries
        // so the recycle bin case needs to be handled.
        let changes =
            serde_json::from_str(TEST_SYNC_SCIM_IPA_REFRESH_1).expect("failed to parse scim sync");

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());

        assert!(idms_prox_write.commit().is_ok());

        // Terminate the sync account
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();

        let ident = idms_prox_write
            .qs_write
            .internal_search_uuid(UUID_ADMIN)
            .map(Identity::from_impersonate_entry_readwrite)
            .expect("Failed to get admin");

        let sfe = ScimSyncTerminateEvent {
            ident,
            target: sync_uuid,
        };

        idms_prox_write
            .scim_sync_terminate(&sfe)
            .expect("Failed to terminate sync account");

        // Check that the entries no longer exist
        for iname in ["testgroup", "testposix", "testexternal", "testuser"] {
            trace!(%iname);
            assert!(idms_prox_write
                .qs_write
                .internal_search(filter!(f_eq(
                    Attribute::Name,
                    PartialValue::new_iname(iname)
                )))
                .unwrap()
                .is_empty());
        }

        assert!(idms_prox_write.commit().is_ok());
    }

    #[idm_test]
    /// Assert that a SCIM JSON proto entry correctly serialises and deserialises
    /// and can be applied as a changeset. This serialisation is performed during
    /// the ScimEntry::try_from step.
    async fn test_idm_scim_sync_json_proto(idms: &IdmServer, _idms_delayed: &mut IdmServerDelayed) {
        let ct = Duration::from_secs(TEST_CURRENT_TIME);
        let mut idms_prox_write = idms.proxy_write(ct).await.unwrap();
        let (_sync_uuid, ident) = test_scim_sync_apply_setup_ident(&mut idms_prox_write, ct);
        let sse = ScimSyncUpdateEvent { ident };

        // Minimum Viable Person
        let person_1 = ScimSyncPerson::builder(
            Uuid::new_v4(),
            "cn=testperson_1".to_string(),
            "testperson_1".to_string(),
            "Test Person One".to_string(),
        )
        .build()
        .try_into()
        .unwrap();

        // Minimum Viable Group
        let group_1 = ScimSyncGroup::builder(
            Uuid::new_v4(),
            "cn=testgroup_1".to_string(),
            "testgroup_1".to_string(),
        )
        .build()
        .try_into()
        .unwrap();

        let user_sshkey = "sk-ecdsa-sha2-nistp256@openssh.com AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20AAAAIbmlzdHAyNTYAAABBBENubZikrb8hu+HeVRdZ0pp/VAk2qv4JDbuJhvD0yNdWDL2e3cBbERiDeNPkWx58Q4rVnxkbV1fa8E2waRtT91wAAAAEc3NoOg== testuser@fidokey";

        // All Attribute Person
        let person_2 = ScimSyncPerson::builder(
            Uuid::new_v4(),
            "cn=testperson_2".to_string(),
            "testperson_2".to_string(),
            "Test Person Two".to_string(),
        )
        .set_password_import(Some("ipaNTHash: iEb36u6PsRetBr3YMLdYbA".to_string()))
        .set_unix_password_import(Some("ipaNTHash: iEb36u6PsRetBr3YMLdYbA".to_string()))
        .set_totp_import(vec![ScimTotp {
            external_id: "Totp".to_string(),
            secret: "QICWZTON72IBS5MXWNURKAONC3JNOOOFMLKNRTIPXBYQ4BLRSEBM7KF5".to_string(),
            algo: "sha256".to_string(),
            step: 60,
            digits: 8,
        }])
        .set_mail(vec![MultiValueAttr {
            primary: Some(true),
            value: "testuser@example.com".to_string(),
            ..Default::default()
        }])
        .set_ssh_publickey(vec![ScimSshPubKey {
            label: "Key McKeyface".to_string(),
            value: user_sshkey.to_string(),
        }])
        .set_login_shell(Some("/bin/zsh".to_string()))
        .set_account_valid_from(Some("2023-11-28T04:57:55Z".to_string()))
        .set_account_expire(Some("2023-11-28T04:57:55Z".to_string()))
        .set_gidnumber(Some(12346))
        .build()
        .try_into()
        .unwrap();

        // All Attribute Group
        let group_2 = ScimSyncGroup::builder(
            Uuid::new_v4(),
            "cn=testgroup_2".to_string(),
            "testgroup_2".to_string(),
        )
        .set_description(Some("description".to_string()))
        .set_gidnumber(Some(12345))
        .set_members(vec!["cn=testperson_1".to_string(), "cn=testperson_2".to_string()].into_iter())
        .build()
        .try_into()
        .unwrap();

        let entries = vec![person_1, group_1, person_2, group_2];

        let changes = ScimSyncRequest {
            from_state: ScimSyncState::Refresh,
            to_state: ScimSyncState::Active {
                cookie: vec![1, 2, 3, 4],
            },
            entries,
            retain: ScimSyncRetentionMode::Ignore,
        };

        assert!(idms_prox_write.scim_sync_apply(&sse, &changes, ct).is_ok());
        assert!(idms_prox_write.commit().is_ok());
    }

    const TEST_SYNC_SCIM_IPA_1: &str = r#"
{
  "from_state": "Refresh",
  "to_state": {
    "Active": {
      "cookie": "aXBhLXN5bmNyZXBsLWthbmkuZGV2LmJsYWNraGF0cy5uZXQuYXU6Mzg5I2NuPWRpcmVjdG9yeSBtYW5hZ2VyOmRjPWRldixkYz1ibGFja2hhdHMsZGM9bmV0LGRjPWF1Oih8KCYob2JqZWN0Q2xhc3M9cGVyc29uKShvYmplY3RDbGFzcz1pcGFudHVzZXJhdHRycykob2JqZWN0Q2xhc3M9cG9zaXhhY2NvdW50KSkoJihvYmplY3RDbGFzcz1ncm91cG9mbmFtZXMpKG9iamVjdENsYXNzPWlwYXVzZXJncm91cCkoIShvYmplY3RDbGFzcz1tZXBtYW5hZ2VkZW50cnkpKSghKGNuPWFkbWlucykpKCEoY249aXBhdXNlcnMpKSkoJihvYmplY3RDbGFzcz1pcGF0b2tlbikob2JqZWN0Q2xhc3M9aXBhdG9rZW50b3RwKSkpIzEzNQ"
    }
  },
  "entries": [
    {
      "schemas": [
        "urn:ietf:params:scim:schemas:kanidm:sync:1:person",
        "urn:ietf:params:scim:schemas:kanidm:sync:1:account",
        "urn:ietf:params:scim:schemas:kanidm:sync:1:posixaccount"
      ],
      "id": "babb8302-43a1-11ed-a50d-919b4b1a5ec0",
      "externalId": "uid=testuser,cn=users,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au",
      "displayname": "Test User",
      "gidnumber": 12345,
      "loginshell": "/bin/sh",
      "name": "testuser",
      "mail": [
        {
          "value": "testuser@dev.blackhats.net.au"
        }
      ],
      "ssh_publickey": [
        {
          "label": "ssh-key",
          "value": "sk-ecdsa-sha2-nistp256@openssh.com AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20AAAAIbmlzdHAyNTYAAABBBENubZikrb8hu+HeVRdZ0pp/VAk2qv4JDbuJhvD0yNdWDL2e3cBbERiDeNPkWx58Q4rVnxkbV1fa8E2waRtT91wAAAAEc3NoOg== testuser@fidokey"
        }
      ],
      "unix_password_import": "ipaNTHash: iEb36u6PsRetBr3YMLdYbA",
      "password_import": "ipaNTHash: iEb36u6PsRetBr3YMLdYbA"
    },
    {
      "schemas": [
        "urn:ietf:params:scim:schemas:kanidm:sync:1:group"
      ],
      "id": "d547c581-5f26-11ed-a50d-919b4b1a5ec0",
      "externalId": "cn=testgroup,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au",
      "description": "Test group",
      "member": [
        {
          "external_id": "uid=testuser,cn=users,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au"
        }
      ],
      "name": "testgroup"
    },
    {
      "schemas": [
        "urn:ietf:params:scim:schemas:kanidm:sync:1:group"
      ],
      "id": "d547c583-5f26-11ed-a50d-919b4b1a5ec0",
      "externalId": "cn=testexternal,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au",
      "name": "testexternal"
    },
    {
      "schemas": [
        "urn:ietf:params:scim:schemas:kanidm:sync:1:group",
        "urn:ietf:params:scim:schemas:kanidm:sync:1:posixgroup"
      ],
      "id": "f90b0b81-5f26-11ed-a50d-919b4b1a5ec0",
      "externalId": "cn=testposix,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au",
      "gidnumber": 1234567,
      "member": [
        {
          "external_id": "uid=testuser,cn=users,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au"
        }
      ],
      "name": "testposix"
    }
  ],
  "retain": "Ignore"
}
    "#;

    const TEST_SYNC_SCIM_IPA_2: &str = r#"
{
  "from_state": {
    "Active": {
      "cookie": "aXBhLXN5bmNyZXBsLWthbmkuZGV2LmJsYWNraGF0cy5uZXQuYXU6Mzg5I2NuPWRpcmVjdG9yeSBtYW5hZ2VyOmRjPWRldixkYz1ibGFja2hhdHMsZGM9bmV0LGRjPWF1Oih8KCYob2JqZWN0Q2xhc3M9cGVyc29uKShvYmplY3RDbGFzcz1pcGFudHVzZXJhdHRycykob2JqZWN0Q2xhc3M9cG9zaXhhY2NvdW50KSkoJihvYmplY3RDbGFzcz1ncm91cG9mbmFtZXMpKG9iamVjdENsYXNzPWlwYXVzZXJncm91cCkoIShvYmplY3RDbGFzcz1tZXBtYW5hZ2VkZW50cnkpKSghKGNuPWFkbWlucykpKCEoY249aXBhdXNlcnMpKSkoJihvYmplY3RDbGFzcz1pcGF0b2tlbikob2JqZWN0Q2xhc3M9aXBhdG9rZW50b3RwKSkpIzEzNQ"
    }
  },
  "to_state": {
    "Active": {
      "cookie": "aXBhLXN5bmNyZXBsLWthbmkuZGV2LmJsYWNraGF0cy5uZXQuYXU6Mzg5I2NuPWRpcmVjdG9yeSBtYW5hZ2VyOmRjPWRldixkYz1ibGFja2hhdHMsZGM9bmV0LGRjPWF1Oih8KCYob2JqZWN0Q2xhc3M9cGVyc29uKShvYmplY3RDbGFzcz1pcGFudHVzZXJhdHRycykob2JqZWN0Q2xhc3M9cG9zaXhhY2NvdW50KSkoJihvYmplY3RDbGFzcz1ncm91cG9mbmFtZXMpKG9iamVjdENsYXNzPWlwYXVzZXJncm91cCkoIShvYmplY3RDbGFzcz1tZXBtYW5hZ2VkZW50cnkpKSghKGNuPWFkbWlucykpKCEoY249aXBhdXNlcnMpKSkoJihvYmplY3RDbGFzcz1pcGF0b2tlbikob2JqZWN0Q2xhc3M9aXBhdG9rZW50b3RwKSkpIzE0MA"
    }
  },
  "entries": [
    {
      "schemas": [
        "urn:ietf:params:scim:schemas:kanidm:sync:1:group"
      ],
      "id": "d547c583-5f26-11ed-a50d-919b4b1a5ec0",
      "externalId": "cn=testexternal2,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au",
      "member": [
        {
          "external_id": "uid=testuser,cn=users,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au"
        }
      ],
      "name": "testexternal2"
    },
    {
      "schemas": [
        "urn:ietf:params:scim:schemas:kanidm:sync:1:group",
        "urn:ietf:params:scim:schemas:kanidm:sync:1:posixgroup"
      ],
      "id": "f90b0b81-5f26-11ed-a50d-919b4b1a5ec0",
      "externalId": "cn=testposix,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au",
      "gidnumber": 1234567,
      "name": "testposix"
    }
  ],
  "retain": {
    "Delete": [
      "d547c581-5f26-11ed-a50d-919b4b1a5ec0"
    ]
  }
}
    "#;

    const TEST_SYNC_SCIM_IPA_REFRESH_1: &str = r#"
{
  "from_state": "Refresh",
  "to_state": {
    "Active": {
      "cookie": "aXBhLXN5bmNyZXBsLWthbmkuZGV2LmJsYWNraGF0cy5uZXQuYXU6Mzg5I2NuPWRpcmVjdG9yeSBtYW5hZ2VyOmRjPWRldixkYz1ibGFja2hhdHMsZGM9bmV0LGRjPWF1Oih8KCYob2JqZWN0Q2xhc3M9cGVyc29uKShvYmplY3RDbGFzcz1pcGFudHVzZXJhdHRycykob2JqZWN0Q2xhc3M9cG9zaXhhY2NvdW50KSkoJihvYmplY3RDbGFzcz1ncm91cG9mbmFtZXMpKG9iamVjdENsYXNzPWlwYXVzZXJncm91cCkoIShvYmplY3RDbGFzcz1tZXBtYW5hZ2VkZW50cnkpKSghKGNuPWFkbWlucykpKCEoY249aXBhdXNlcnMpKSkoJihvYmplY3RDbGFzcz1pcGF0b2tlbikob2JqZWN0Q2xhc3M9aXBhdG9rZW50b3RwKSkpIzEzNQ"
    }
  },
  "entries": [
    {
      "schemas": [
        "urn:ietf:params:scim:schemas:kanidm:sync:1:person",
        "urn:ietf:params:scim:schemas:kanidm:sync:1:account",
        "urn:ietf:params:scim:schemas:kanidm:sync:1:posixaccount"
      ],
      "id": "babb8302-43a1-11ed-a50d-919b4b1a5ec0",
      "externalId": "uid=testuser,cn=users,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au",
      "displayname": "Test User",
      "gidnumber": 12345,
      "loginshell": "/bin/sh",
      "name": "testuser",
      "password_import": "ipaNTHash: iEb36u6PsRetBr3YMLdYbA",
      "unix_password_import": "ipaNTHash: iEb36u6PsRetBr3YMLdYbA",
      "account_valid_from": "2021-11-28T04:57:55Z",
      "account_expire": "2023-11-28T04:57:55Z"
    },
    {
      "schemas": [
        "urn:ietf:params:scim:schemas:kanidm:sync:1:group"
      ],
      "id": "d547c581-5f26-11ed-a50d-919b4b1a5ec0",
      "externalId": "cn=testgroup,cn=groups,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au",
      "description": "Test group",
      "member": [
        {
          "external_id": "uid=testuser,cn=users,cn=accounts,dc=dev,dc=blackhats,dc=net,dc=au"
        }
      ],
      "name": "testgroup"
    }
  ],
  "retain": "Ignore"
}
    "#;
}
