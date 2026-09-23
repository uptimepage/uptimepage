-- A declared incident with no monitor reaches pages only through this list;
-- a monitor's incident keeps reaching them through the page's components.
CREATE TABLE incident_status_pages (
    org_id          UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    incident_id     UUID NOT NULL REFERENCES incidents(id) ON DELETE CASCADE,
    status_page_id  UUID NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    PRIMARY KEY (incident_id, status_page_id)
);

CREATE INDEX idx_incident_status_pages_page
    ON incident_status_pages (status_page_id, incident_id);
