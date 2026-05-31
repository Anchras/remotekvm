CREATE TABLE organization_invites (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    email TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'member' CHECK (role IN ('owner', 'admin', 'member')),
    invited_by UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    accepted_at TIMESTAMPTZ,
    UNIQUE(organization_id, email)
);

ALTER TABLE organization_members
    ADD CONSTRAINT organization_members_role_check CHECK (role IN ('owner', 'admin', 'member'));

CREATE INDEX idx_org_invites_org_id ON organization_invites(organization_id);
CREATE INDEX idx_org_invites_email ON organization_invites(email);
