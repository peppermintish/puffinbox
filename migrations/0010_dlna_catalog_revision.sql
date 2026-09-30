-- DLNA ContentDirectory clients use SystemUpdateID to decide whether cached
-- catalog pages need to be browsed again. Keep one transactional revision
-- counter and increment it once per catalog or visibility-affecting SQL
-- statement, rather than once per row during a bounded scanner batch.
CREATE TABLE dlna_catalog_revision (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton = TRUE),
    revision BIGINT NOT NULL DEFAULT 1
        CHECK (revision >= 0 AND revision <= 4294967295)
);

INSERT INTO dlna_catalog_revision(singleton, revision)
VALUES (TRUE, 1);

CREATE FUNCTION puffinbox_bump_dlna_catalog_revision()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    UPDATE dlna_catalog_revision
    SET revision = (revision + 1) % 4294967296
    WHERE singleton = TRUE;
    RETURN NULL;
END;
$$;

CREATE TRIGGER dlna_catalog_revision_items
AFTER INSERT OR UPDATE OR DELETE ON items
FOR EACH STATEMENT
EXECUTE FUNCTION puffinbox_bump_dlna_catalog_revision();

CREATE TRIGGER dlna_catalog_revision_libraries
AFTER INSERT OR UPDATE OR DELETE ON libraries
FOR EACH STATEMENT
EXECUTE FUNCTION puffinbox_bump_dlna_catalog_revision();

CREATE TRIGGER dlna_catalog_revision_users
AFTER INSERT OR UPDATE OR DELETE ON users
FOR EACH STATEMENT
EXECUTE FUNCTION puffinbox_bump_dlna_catalog_revision();

CREATE TRIGGER dlna_catalog_revision_user_library_access
AFTER INSERT OR UPDATE OR DELETE ON user_library_access
FOR EACH STATEMENT
EXECUTE FUNCTION puffinbox_bump_dlna_catalog_revision();

CREATE TRIGGER dlna_catalog_revision_item_metadata
AFTER INSERT OR UPDATE OR DELETE ON item_metadata
FOR EACH STATEMENT
EXECUTE FUNCTION puffinbox_bump_dlna_catalog_revision();

CREATE TRIGGER dlna_catalog_revision_live_tv_channels
AFTER INSERT OR UPDATE OR DELETE ON live_tv_channels
FOR EACH STATEMENT
EXECUTE FUNCTION puffinbox_bump_dlna_catalog_revision();
