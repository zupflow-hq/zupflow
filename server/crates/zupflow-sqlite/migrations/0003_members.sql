-- Users, the identities they sign in with, and their memberships.
-- `users` and `user_identities` are global: a person belongs to many
-- organizations. `members` is organization-scoped.

CREATE TABLE users (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    -- Lowercased; one user per address.
    email TEXT NOT NULL UNIQUE CHECK (length(email) BETWEEN 3 AND 320 AND email = lower(email)),
    avatar_url TEXT CHECK (avatar_url IS NULL OR length(avatar_url) BETWEEN 1 AND 2000),
    created_at INTEGER NOT NULL
);

-- Who vouches for a user: an identity adapter's `(provider, subject)`.
CREATE TABLE user_identities (
    provider TEXT NOT NULL CHECK (length(provider) BETWEEN 1 AND 300),
    subject TEXT NOT NULL CHECK (length(subject) BETWEEN 1 AND 300),
    user_id UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (provider, subject)
);

CREATE TABLE members (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'developer', 'viewer')),
    status TEXT NOT NULL CHECK (status IN ('active', 'invited')),
    invited_at INTEGER NOT NULL,
    joined_at INTEGER,
    last_active_at INTEGER,
    CHECK ((status = 'active') = (joined_at IS NOT NULL)),
    UNIQUE (organization_id, id),
    -- One membership or invitation per person per organization.
    UNIQUE (organization_id, user_id)
);

-- `/v1/me`: a user's memberships across organizations.
CREATE INDEX members_by_user ON members (user_id);
