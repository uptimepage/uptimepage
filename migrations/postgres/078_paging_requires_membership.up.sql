-- Someone paged in an org has to be a member of it. Rotation slots, override
-- windows, paging channels and escalation levels naming a person hang off
-- that person's membership, so however the membership goes (removed by an
-- owner, the account deleted) they go with it, and a write racing the removal
-- cannot bring one back.

ALTER TABLE on_call_participants
    ADD CONSTRAINT fk_on_call_participants_membership
    FOREIGN KEY (user_id, org_id) REFERENCES memberships (user_id, org_id) ON DELETE CASCADE;
ALTER TABLE on_call_overrides
    ADD CONSTRAINT fk_on_call_overrides_membership
    FOREIGN KEY (user_id, org_id) REFERENCES memberships (user_id, org_id) ON DELETE CASCADE;
ALTER TABLE user_contact_channels
    ADD CONSTRAINT fk_user_contact_channels_membership
    FOREIGN KEY (user_id, org_id) REFERENCES memberships (user_id, org_id) ON DELETE CASCADE;
-- A channel or schedule rung has no user_id; a NULL column leaves the key unchecked.
ALTER TABLE escalation_targets
    ADD CONSTRAINT fk_escalation_targets_membership
    FOREIGN KEY (user_id, org_id) REFERENCES memberships (user_id, org_id) ON DELETE CASCADE;
