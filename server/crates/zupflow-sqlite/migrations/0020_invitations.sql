-- Who granted each invitation: its sender, or whoever last changed its role.
-- Global: a user, who may leave every organization. Null for an
-- organization's creator, for invitations sent before this column, and once
-- that user is gone; an invitation with no grantor grants nothing.
ALTER TABLE members ADD COLUMN invited_by_user_id UUID REFERENCES users (id) ON DELETE SET NULL;

-- Withdrawing what a removed or demoted member granted, and the cascade
-- when a user is deleted, find their invitations here.
CREATE INDEX members_by_grantor ON members (invited_by_user_id, organization_id);
